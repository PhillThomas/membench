//! Run orchestration: which cells, in which order, with what reported back.
//!
//! The runner is deliberately dumb: it walks the level/op matrix, hands each
//! cell to `bandwidth` or `latency`, and streams [`Event`]s to whoever is
//! listening. The UI (TUI or CLI) is a *consumer* of those events and never
//! holds the CPU while a measurement is in flight (PLAN.md rule 9), which is why
//! the runners do their work on their own threads and communicate over a
//! channel.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::bandwidth::{self, Job as BandwidthJob, Work};
use crate::kernels::Kernels;
use crate::latency::{self, LatencyJob, SweepJob};
use crate::result::{
    CellResult, ClockMeasurement, Column, Level, Op, PageKind, RunResults, Settings, SweepPoint,
    SystemInfo, Warning,
};
use crate::sizes::{self, CellPlan};
use crate::sysinfo;
use crate::{Error, Result};

/// Everything the UI needs to draw, in arrival order.
#[derive(Clone, Debug)]
pub enum Event {
    CellStart {
        level: Level,
        op: Op,
        threads: usize,
        bytes_per_thread: usize,
    },
    Trial {
        level: Level,
        op: Op,
        trial: usize,
        trials: usize,
        elapsed_us: u64,
    },
    CellDone(CellResult),
    SweepStart {
        size_bytes: usize,
    },
    SweepPoint(SweepPoint),
    /// Boxed: `RunResults` carries the whole system description, so leaving it
    /// inline would make every event in the channel that large.
    Finished(Box<RunResults>),
    Failed(String),
}

pub struct Runner {
    pub sys: SystemInfo,
    pub settings: Settings,
    pub kernels: Kernels,
    /// CPU the single-threaded cells (and the sweep) run on.
    pub latency_cpu: usize,
}

impl Runner {
    /// Detect the machine and resolve the kernel set. Cheap, no measurement.
    pub fn prepare(settings: Settings) -> Result<Runner> {
        let sys = sysinfo::gather();
        let kernels = Kernels::resolve(settings.isa)?;
        // An explicit `--cpu` is checked rather than trusted: pinning silently
        // fails on a cpu that does not exist, and the run then measures a
        // different core from the one the report names.
        let latency_cpu = match settings.latency_cpu {
            Some(cpu) => {
                if !sys.topology.all_cpus.contains(&cpu) {
                    return Err(Error::NoSuchCpu {
                        cpu,
                        list: sys
                            .topology
                            .all_cpus
                            .iter()
                            .map(usize::to_string)
                            .collect::<Vec<_>>()
                            .join(", "),
                    });
                }
                cpu
            }
            None => sysinfo::topology::latency_core(&sys).unwrap_or(0),
        };
        Ok(Runner {
            sys,
            settings,
            kernels,
            latency_cpu,
        })
    }

    /// Warnings that only make sense once a run has been configured.
    pub fn preflight_warnings(&self) -> Vec<Warning> {
        let mut out = Vec::new();
        if let Some(requested) = self.settings.isa {
            if requested != self.kernels.isa {
                out.push(Warning::new(
                    "isa-forced",
                    format!("--isa {requested} overrides the default path",),
                ));
            }
        }
        if self.kernels.isa == crate::result::Isa::Scalar {
            out.push(Warning::new(
                "isa-scalar",
                "running the scalar kernel path: bandwidth numbers will be far below what this \
                 CPU can do",
            )
            .with_hint("check --isa and the CPU feature list"));
        }
        if self.settings.pages != PageKind::FourK
            && sysinfo::transparent_hugepages_setting().as_deref() == Some("never")
        {
            out.push(
                Warning::new(
                    "thp-never",
                    "transparent huge pages are set to `never`, so 2 MiB requests cannot be met",
                )
                .with_hint(
                    "echo madvise | sudo tee /sys/kernel/mm/transparent_hugepage/enabled, or \
                     reserve huge pages with vm.nr_hugepages",
                ),
            );
        }
        // The machine-level findings gathered at startup — governor, energy
        // bias, NUMA — belong here as well as in a finished run's document, so
        // `--dry-run` can warn before anything is measured.
        for warning in &self.sys.warnings {
            if !out.iter().any(|w| w.code == warning.code) {
                out.push(warning.clone());
            }
        }
        if self.settings.threads.unwrap_or(0) > self.sys.topology.logical_cpus {
            out.push(Warning::new(
                "threads",
                format!(
                    "{} threads requested but only {} logical CPUs exist; the runner will reuse \
                     CPUs and the numbers will be meaningless",
                    self.settings.threads.unwrap_or(0),
                    self.sys.topology.logical_cpus
                ),
            ));
        }
        out
    }

    /// Run every configured cell, then the sweep if asked for.
    pub fn run(
        &self,
        cancel: &Arc<AtomicBool>,
        events: Option<Sender<Event>>,
    ) -> Result<RunResults> {
        crate::ensure_release_build("a benchmark run")?;
        let mut results = RunResults::new(self.sys.clone(), self.settings.clone());
        results.warnings = self.preflight_warnings();
        for w in &self.sys.warnings {
            if !results
                .warnings
                .iter()
                .any(|existing| existing.code == w.code)
            {
                results.warnings.push(w.clone());
            }
        }

        // Effective clock for the run as a whole, on the core that runs the
        // single-threaded cells (rule 5: measured, never read from cpufreq).
        results.clock = measure_clock_on(self.latency_cpu, Duration::from_millis(30));
        // A clock outside the physical envelope is the machine's timer talking,
        // and every `cycles` figure in this document is divided by it. Say so
        // rather than let a synthesised 20.45 GHz scale the whole column quietly.
        if let Some(c) = &results.clock {
            if !crate::timer::clock_is_plausible(c.hz) {
                results.warnings.push(
                    Warning::new(
                        "clock",
                        format!(
                            "the core clock measured {:.0} MHz, which is not a frequency a core \
                             runs at: the clocksource here is synthesised, so the cycles column \
                             is scaled by the same error",
                            c.mhz
                        ),
                    )
                    .with_hint(
                        "read the ns column, or run on bare metal for cycles that mean something",
                    ),
                );
            }
        }

        for &level in &self.settings.levels.clone() {
            let plan = match sizes::for_level(&self.sys, &self.settings, level) {
                Ok(p) => p,
                Err(e) => {
                    results.warnings.push(
                        Warning::new("sizing", format!("{}: {e}", level.grid_label()))
                            .with_hint("the cell is missing from the table rather than estimated"),
                    );
                    continue;
                }
            };

            for op in self.settings.ops_for(level) {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Unsupported("cancelled"));
                }
                let headline = is_headline(&self.settings, level, op);
                let cell = if op == Op::Latency {
                    let size = plan.bytes_per_thread.max(8 * 1024);
                    if let Some(tx) = &events {
                        let _ = tx.send(Event::CellStart {
                            level,
                            op,
                            threads: 1,
                            bytes_per_thread: size,
                        });
                    }
                    let job = LatencyJob {
                        sys: &self.sys,
                        settings: &self.settings,
                        cpu: self.latency_cpu,
                        level,
                        size_bytes: size,
                        headline,
                        cancel,
                        events: events.as_ref(),
                    };
                    latency::run(&job)
                } else {
                    let Some(work) = Work::from_op(op) else {
                        continue;
                    };
                    let plan = plan_for(work, &plan);
                    if let Some(tx) = &events {
                        let _ = tx.send(Event::CellStart {
                            level,
                            op,
                            threads: plan.threads,
                            bytes_per_thread: plan.bytes_per_thread,
                        });
                    }
                    let job = BandwidthJob {
                        sys: &self.sys,
                        settings: &self.settings,
                        plan: &plan,
                        kernels: self.kernels,
                        work,
                        headline,
                        cancel: Arc::clone(cancel),
                        events: events.clone(),
                    };
                    bandwidth::run(&job)
                };

                match cell {
                    Ok(cell) => {
                        if let Some(tx) = &events {
                            let _ = tx.send(Event::CellDone(cell.clone()));
                        }
                        results.cells.push(cell);
                    }
                    Err(e) => {
                        if matches!(e, Error::Unsupported(_)) {
                            return Err(e);
                        }
                        results.warnings.push(Warning::new(
                            "cell-failed",
                            format!("{} {}: {e}", level.grid_label(), op.label()),
                        ));
                    }
                }
            }
        }

        // Rule 7: the UI has to say when huge pages were not granted.
        if let Some(cell) = results
            .cells
            .iter()
            .find(|c| c.pages_actual == PageKind::FourK && c.pages_requested != PageKind::FourK)
        {
            let requested = cell.pages_requested;
            results.warnings.push(
                Warning::new(
                    "huge-pages",
                    format!(
                        "{} pages were requested but {} pages are in use",
                        requested.label(),
                        cell.pages_actual.label()
                    ),
                )
                .with_hint("memory cells will read a little low and latency a little high"),
            );
        }

        if self.settings.with_sweep {
            if let Some(tx) = &events {
                let _ = tx.send(Event::CellStart {
                    level: Level::Memory,
                    op: Op::Latency,
                    threads: 1,
                    bytes_per_thread: 0,
                });
            }
            let job = SweepJob {
                sys: &self.sys,
                settings: &self.settings,
                cpu: self.latency_cpu,
                cancel,
                events: events.as_ref(),
            };
            match latency::run_sweep(&job) {
                Ok(sweep) => {
                    results.sweep = Some(sweep);
                }
                Err(e) if matches!(e, Error::Unsupported(_)) => return Err(e),
                Err(e) => results.warnings.push(Warning::new(
                    "sweep",
                    format!("the latency sweep failed: {e}"),
                )),
            }
        }

        // A sweep-only invocation legitimately has no grid cells; refusing one
        // would throw away a complete ladder because a different table was
        // empty.
        if results.cells.is_empty() && results.sweep.is_none() {
            return Err(Error::Unsupported(
                "nothing was measured: no grid cells and no sweep",
            ));
        }
        if let Some(tx) = &events {
            let _ = tx.send(Event::Finished(Box::new(results.clone())));
        }
        Ok(results)
    }
}

/// The grid shows one cell per column; the others stay in the detail view.
fn is_headline(settings: &Settings, level: Level, op: Op) -> bool {
    settings.headline_op(level, op.column()) == Some(op)
}

/// Copy needs two buffers, so the plan's per-thread byte count is what each of
/// them is sized to; the plan already accounts for that.
fn plan_for(work: Work, plan: &CellPlan) -> CellPlan {
    let mut p = plan.clone();
    p.buffers_per_thread = if work.needs_two_buffers() { 2 } else { 1 };
    p.total_bytes = p.bytes_per_thread * p.threads * p.buffers_per_thread;
    p
}

/// Measure the clock on a specific core without pinning the caller (which is
/// usually the UI thread).
pub fn measure_clock_on(cpu: usize, target: Duration) -> Option<ClockMeasurement> {
    let handle = thread::Builder::new()
        .name("membench-clock".to_string())
        .spawn(move || {
            crate::affinity::pin_current(cpu).ok()?;
            crate::timer::measure_core_clock(target).ok()
        })
        .ok()?;
    handle.join().ok().flatten()
}

/// Convenience for the CLI: gather, run, return the document.
pub fn run_once(settings: Settings, cancel: &Arc<AtomicBool>) -> Result<RunResults> {
    let runner = Runner::prepare(settings)?;
    runner.run(cancel, None)
}

/// Grid-shaped helper for both front ends: the value to print for a row.
pub fn grid_value(results: &RunResults, level: Level, column: Column) -> Option<String> {
    results
        .grid_cell(level, column)
        .map(|c| format!("{:.1}", c.value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_resolves_kernels_and_a_latency_core() {
        let r = Runner::prepare(Settings::default()).unwrap();
        assert!(Kernels::resolve(Some(r.kernels.isa)).is_ok());
        assert!(
            r.sys.topology.core_cpus.contains(&r.latency_cpu)
                || r.sys.topology.core_cpus.is_empty()
        );
        assert!(!r
            .preflight_warnings()
            .iter()
            .any(|w| w.code == "cell-failed"));
    }

    #[test]
    fn forced_scalar_is_reported() {
        let s = Settings {
            isa: Some(crate::result::Isa::Scalar),
            ..Default::default()
        };
        let r = Runner::prepare(s).unwrap();
        let w = r.preflight_warnings();
        // Scalar is either the default (no "isa-forced" noise) or explicitly
        // chosen (then it must be explained).
        assert!(w.iter().any(|w| w.code == "isa-scalar") || r.sys.cpu.isa_paths.len() == 1);
    }

    #[test]
    fn headline_selection_matches_the_settings() {
        let s = Settings::default();
        assert!(is_headline(&s, Level::L1, Op::Read));
        assert!(!is_headline(&s, Level::L1, Op::WriteNt));
        assert!(is_headline(&s, Level::L1, Op::Write));
        // Memory write defaults to the non-temporal cell.
        assert!(is_headline(&s, Level::Memory, Op::WriteNt));
        assert!(!is_headline(&s, Level::Memory, Op::Write));
        assert!(is_headline(&s, Level::Memory, Op::CopyNt));
        // ...and --write-hint normal flips it.
        let s2 = Settings {
            write_headline: crate::result::WriteHeadline::Normal,
            ..Default::default()
        };
        assert!(is_headline(&s2, Level::Memory, Op::Write));
        assert!(!is_headline(&s2, Level::Memory, Op::WriteNt));
        // Latency is its own column with its own single measurement, so it
        // is trivially that column's headline.
        assert!(is_headline(&s, Level::Memory, Op::Latency));
    }

    #[test]
    fn copy_plans_two_buffers() {
        let p = CellPlan {
            level: Level::L2,
            threads: 2,
            cpus: vec![0, 1],
            bytes_per_thread: 4096,
            buffers_per_thread: 1,
            total_bytes: 8192,
            cache_bytes: None,
            derivation: "test".to_string(),
            note: None,
        };
        let copy = plan_for(Work::Copy, &p);
        assert_eq!(copy.buffers_per_thread, 2);
        assert_eq!(copy.total_bytes, 16384);
        assert_eq!(plan_for(Work::Read, &p).total_bytes, 8192);
    }

    #[test]
    fn clock_measurement_runs_on_the_requested_core() {
        // Debug builds refuse to *time benchmarks* but the clock helper is
        // allowed, so this is a real measurement.
        let cpu = 0;
        if let Some(c) = measure_clock_on(cpu, Duration::from_millis(5)) {
            // Pinning, provenance and a positive rate are the code's business.
            // Whether the rate is physically possible is the machine's: a CI VM
            // measured 20.45 GHz for a 2.05 GHz core and failed this line, which
            // is an assertion about the host wearing a test's clothes. The
            // warning that replaces it lives at the point of use, where a reader
            // of the document will actually see it.
            assert!(c.mhz > 0.0, "{c:?}");
            assert_eq!(c.cpu, cpu);
            assert!(!c.method.is_empty());
            if !crate::timer::clock_is_plausible(c.hz) {
                eprintln!(
                    "note: {:.0} MHz is not a physical core clock here; the runner warned \
                           in the run output, which is where it belongs",
                    c.mhz
                );
            }
        }
    }

    #[test]
    fn grid_value_formats_or_returns_none() {
        let sys = sysinfo::gather();
        let mut r = RunResults::new(sys, Settings::default());
        assert!(grid_value(&r, Level::L1, Column::Read).is_none());
        r.cells.push(CellResult {
            level: Level::L1,
            op: Op::Read,
            headline: true,
            value: 412.34,
            unit: "GB/s".to_string(),
            best: 412.34,
            median: 400.0,
            mean: 400.0,
            stddev: 1.0,
            spread_pct: 2.0,
            rejected_trials: 0,
            trials: vec![412.34],
            threads: 1,
            cpus: vec![0],
            buffer_bytes_per_thread: 4096,
            buffers_per_thread: 1,
            bytes_moved: 4096,
            duration_us: 1,
            isa: crate::result::Isa::Scalar,
            pages_requested: PageKind::FourK,
            pages_actual: PageKind::FourK,
            huge_pages_bytes: None,
            notes: vec![],
            cycles: None,
            clock_mhz: None,
            peak_share_pct: None,
        });
        assert_eq!(
            grid_value(&r, Level::L1, Column::Read).as_deref(),
            Some("412.3")
        );
    }

    /// Rule 2, debug half: an unoptimised build must not produce numbers.
    /// Gated, because the release half would run the whole benchmark here.
    #[cfg(debug_assertions)]
    #[test]
    fn run_refuses_debug_builds() {
        let cancel = Arc::new(AtomicBool::new(false));
        match run_once(Settings::default(), &cancel) {
            Err(Error::DebugBuild { .. }) => {}
            // Detection-only failures (no cache info in a container) are also
            // acceptable, but never a silent success.
            Err(_) => {}
            Ok(_) => panic!("a debug build must not produce numbers"),
        }
    }

    /// Rule 2, release half: the guard must not refuse an optimised build.
    #[cfg(not(debug_assertions))]
    #[test]
    fn release_build_is_allowed_to_time() {
        assert!(crate::ensure_release_build("a benchmark run").is_ok());
    }
}
