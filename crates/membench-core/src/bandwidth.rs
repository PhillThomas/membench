//! Multi-threaded bandwidth runner.
//!
//! The order of operations is the whole design (PLAN.md rules 4, 6, 8, 9):
//!
//! 1. spawn one worker per CPU in the plan,
//! 2. **pin first**, so the allocator's first touch lands on the NUMA node of
//!    the CPU that will do the work,
//! 3. allocate and prefault the buffers on that worker,
//! 4. run one untimed pass and time it, to derive a repetition count that fills
//!    the target window (min across threads, so every thread runs the same
//!    amount of work),
//! 5. run the timed trials behind a barrier, re-checking affinity before each
//!    window — a scheduler migration in the middle of a 200 ms window is exactly
//!    the silent error this project exists to avoid,
//! 6. aggregate a trial as *earliest start → latest end*, because a bandwidth
//!    figure that ignores the straggler is a fiction.
//!
//! The calling (UI) thread only ever waits on a channel: while measuring it is
//! asleep, which keeps its own work off the memory hierarchy (rule 9).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use crate::alloc::{AllocOpts, Buffer, PageStatus};
use crate::kernels::Kernels;
use crate::result::{CellResult, ClockMeasurement, Level, Op, PageKind, Settings, SystemInfo};
use crate::runner::Event;
use crate::sizes::CellPlan;
use crate::stats::{Direction, Stats};
use crate::timer;
use crate::{Error, Result};

/// Bytes per second → GB/s, SI (10⁹ B/s) as the plan's table requires.
pub const GBPS: f64 = 1_000_000_000.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Work {
    Read,
    Write,
    WriteNt,
    Copy,
    CopyNt,
}

impl Work {
    pub fn from_op(op: Op) -> Option<Work> {
        match op {
            Op::Read => Some(Work::Read),
            Op::Write => Some(Work::Write),
            Op::WriteNt => Some(Work::WriteNt),
            Op::Copy => Some(Work::Copy),
            Op::CopyNt => Some(Work::CopyNt),
            _ => None,
        }
    }

    pub fn needs_two_buffers(self) -> bool {
        matches!(self, Work::Copy | Work::CopyNt)
    }

    /// Count the destination as well as the source? (PLAN.md rule 12: no by
    /// default, `--copy-counts-both` to change it.)
    pub fn bytes_multiplier(self, copy_counts_both: bool) -> u64 {
        match self {
            Work::Copy | Work::CopyNt => {
                if copy_counts_both {
                    2
                } else {
                    1
                }
            }
            _ => 1,
        }
    }
}

pub struct Job<'a> {
    pub sys: &'a SystemInfo,
    pub settings: &'a Settings,
    pub plan: &'a CellPlan,
    pub kernels: Kernels,
    pub work: Work,
    pub headline: bool,
    pub cancel: Arc<AtomicBool>,
    pub events: Option<Sender<Event>>,
}

/// One thread's timing for one trial.
#[derive(Clone, Copy)]
struct Sample {
    start: Instant,
    end: Instant,
    bytes: u64,
    migrated: bool,
}

struct Outcome {
    cpu: usize,
    samples: Vec<Sample>,
    page: PageStatus,
    locked: bool,
    clock: Option<ClockMeasurement>,
    error: Option<String>,
}

pub fn run(job: &Job) -> Result<CellResult> {
    crate::ensure_release_build("bandwidth measurement")?;

    let plan = job.plan;
    if plan.threads == 0 || plan.bytes_per_thread == 0 {
        return Err(Error::Unsizeable {
            level: plan.level,
            why: "zero-sized plan".to_string(),
        });
    }
    if job.cancel.load(Ordering::Relaxed) {
        return Err(Error::Unsupported("cancelled"));
    }

    let barrier = Arc::new(Barrier::new(plan.threads));
    let reps = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(0));
    let cancel = job.cancel.clone();
    let events = job.events.clone();

    let mut handles = Vec::with_capacity(plan.threads);
    for (index, cpu) in plan.cpus.iter().enumerate() {
        let barrier = Arc::clone(&barrier);
        let reps = Arc::clone(&reps);
        let failures = Arc::clone(&failures);
        let cancel = Arc::clone(&cancel);
        let events = events.clone();
        let kernels = job.kernels;
        let settings = job.settings.clone();
        let bytes_per_thread = plan.bytes_per_thread;
        let work = job.work;
        let level = plan.level;
        let cpu = *cpu;
        let index = index;

        handles.push(thread::Builder::new().name(format!("membench-{cpu}")).spawn(move || {
            let outcome = worker(
                cpu,
                index,
                bytes_per_thread,
                work,
                &settings,
                kernels,
                &barrier,
                &reps,
                &cancel,
                events.as_ref(),
                level,
            );
            if outcome.error.is_some() {
                failures.fetch_add(1, Ordering::SeqCst);
            }
            outcome
        }));
    }

    let mut outcomes = Vec::with_capacity(plan.threads);
    let mut first_error: Option<String> = None;
    for h in handles {
        let joined = match h {
            Ok(handle) => match handle.join() {
                Ok(outcome) => outcome,
                Err(_) => {
                    first_error.get_or_insert_with(|| "a worker thread panicked".to_string());
                    continue;
                }
            },
            Err(e) => {
                first_error.get_or_insert_with(|| format!("could not start worker: {e}"));
                continue;
            }
        };
        if let Some(err) = joined.error.clone() {
            first_error.get_or_insert(err);
        }
        outcomes.push(joined);
    }

    if let Some(err) = first_error {
        if outcomes.iter().all(|o| o.samples.is_empty()) {
            return Err(Error::ThreadPanicked(err));
        }
    }
    if outcomes.is_empty() {
        return Err(Error::ThreadPanicked("no workers ran".to_string()));
    }

    Ok(aggregate(job, &outcomes))
}

/// The per-thread body: pin → allocate → prefault → warm up → trials.
fn worker(
    cpu: usize,
    index: usize,
    bytes_per_thread: usize,
    work: Work,
    settings: &Settings,
    kernels: Kernels,
    barrier: &Barrier,
    reps_shared: &AtomicUsize,
    cancel: &AtomicBool,
    events: Option<&Sender<Event>>,
    level: Level,
) -> Outcome {
    let fail = |msg: String| Outcome {
        cpu,
        samples: Vec::new(),
        page: PageStatus {
            requested: settings.pages,
            actual: PageKind::FourK,
            huge_bytes: None,
            note: Some("buffer was never allocated".to_string()),
        },
        locked: false,
        clock: None,
        error: Some(msg),
    };

    // Rule 4: pin before allocating.
    if let Err(e) = crate::affinity::pin_current(cpu) {
        return fail(format!("could not pin to CPU {cpu}: {e}"));
    }
    if settings.boost_priority {
        // Best effort: a non-root process cannot always do this.
        let _ = crate::affinity::boost_nice(-5);
    }

    let opts = AllocOpts {
        pages: settings.pages,
        prefault: true,
        lock: settings.mlock,
    };
    let mut primary = match Buffer::new(bytes_per_thread, opts) {
        Ok(b) => b,
        Err(e) => return fail(format!("CPU {cpu}: {e}")),
    };
    let mut secondary = if work.needs_two_buffers() {
        match Buffer::new(bytes_per_thread, opts) {
            Ok(b) => Some(b),
            Err(e) => return fail(format!("CPU {cpu}: {e}")),
        }
    } else {
        None
    };

    // Rule 8: every page touched before timing. `fill_words` writes one word
    // per 8 bytes; `Buffer::new` already faulted one word per page, and the
    // whole buffer is written by the warm-up pass below.
    primary.fill_words(0x9E37_79B9_7F4A_7C15 ^ (cpu as u64));
    if let Some(b) = secondary.as_mut() {
        b.fill_words(0xC2B2_AE3D_27D4_EB4F ^ (cpu as u64));
    }

    let src = primary.ptr();
    let dst = secondary
        .as_mut()
        .map(|b| b.ptr_mut())
        .unwrap_or_else(|| primary.ptr_mut());
    let len = primary.len();

    // Untimed warm-up, timed only to size the real windows.
    let warm_start = Instant::now();
    let sink = unsafe { kernels_run(kernels, work, src, dst, len, 1) };
    std::hint::black_box(sink);
    let warm = warm_start.elapsed().max(Duration::from_micros(1));

    // Every thread computes its own rep count; the smallest one wins so the
    // threads stay in step (a thread that finishes early idles in L3).
    let target = Duration::from_millis(settings.duration_ms.max(1));
    let mine = (target.as_nanos() / warm.as_nanos().max(1) as u128).max(1) as usize;
    barrier.wait();
    if index == 0 {
        reps_shared.store(mine, Ordering::SeqCst);
    }
    barrier.wait();
    let reps = reps_shared.load(Ordering::SeqCst).max(1);

    let mut samples = Vec::with_capacity(settings.trials.max(1));
    for trial in 0..settings.trials.max(1) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        barrier.wait();

        // Rule 4 again, inside the loop: a migration between trials invalidates
        // the comparison between trials.
        let migrated = match crate::affinity::current_affinity() {
            Ok(set) => set.as_slice() != [cpu],
            Err(_) => false,
        };

        let start = Instant::now();
        let acc = unsafe { kernels_run(kernels, work, src, dst, len, reps) };
        let end = Instant::now();
        std::hint::black_box(acc);

        let bytes = (len as u64)
            .saturating_mul(reps as u64)
            .saturating_mul(work.bytes_multiplier(settings.copy_counts_both));
        samples.push(Sample {
            start,
            end,
            bytes,
            migrated,
        });

        if let Some(tx) = events {
            let _ = tx.send(Event::Trial {
                level,
                op: work.op(),
                trial,
                trials: settings.trials.max(1),
                elapsed_us: end.duration_since(start).as_micros() as u64,
            });
        }
    }

    let page = primary.page_status();
    let locked = primary.is_locked() && secondary.as_ref().map(|b| b.is_locked()).unwrap_or(true);

    // The clock of the core that did the work, measured after the trials so it
    // sees the same thermal state.
    let clock = timer::measure_core_clock(Duration::from_millis(20)).ok();

    Outcome {
        cpu,
        samples,
        page,
        locked,
        clock,
        error: None,
    }
}

/// SAFETY: the caller guarantees the regions are live for `len` bytes and that
/// `dst` is writable; buffers never overlap (separate mappings).
unsafe fn kernels_run(
    kernels: Kernels,
    work: Work,
    src: *mut u8,
    dst: *mut u8,
    len: usize,
    reps: usize,
) -> u64 {
    match work {
        Work::Read => kernels.read(src as *const u8, len, reps),
        Work::Write => kernels.write(dst, len, reps, 0x0123_4567_89AB_CDEF),
        Work::WriteNt => kernels.write_nt(dst, len, reps, 0x0123_4567_89AB_CDEF),
        Work::Copy => kernels.copy(src as *const u8, dst, len, reps),
        Work::CopyNt => kernels.copy_nt(src as *const u8, dst, len, reps),
    }
}

impl Work {
    pub fn op(self) -> Op {
        match self {
            Work::Read => Op::Read,
            Work::Write => Op::Write,
            Work::WriteNt => Op::WriteNt,
            Work::Copy => Op::Copy,
            Work::CopyNt => Op::CopyNt,
        }
    }
}

/// Fold the per-thread samples into one number per trial.
fn aggregate(job: &Job, outcomes: &[Outcome]) -> CellResult {
    let plan = job.plan;
    let trials = outcomes
        .iter()
        .map(|o| o.samples.len())
        .min()
        .unwrap_or(0);

    let mut values: Vec<f64> = Vec::with_capacity(trials);
    let mut best_bytes: u128 = 0;
    let mut best_duration_us: u128 = 0;
    let mut best_value = f64::MIN;
    let mut migrations = 0usize;

    for t in 0..trials {
        let mut start = outcomes[0].samples[t].start;
        let mut end = outcomes[0].samples[t].end;
        let mut bytes: u128 = 0;
        for o in outcomes {
            let s = o.samples[t];
            if s.start < start {
                start = s.start;
            }
            if s.end > end {
                end = s.end;
            }
            bytes += s.bytes as u128;
            if s.migrated {
                migrations += 1;
            }
        }
        let secs = (end - start).as_secs_f64().max(1e-9);
        let gbps = bytes as f64 / secs / GBPS;
        values.push(gbps);
        if gbps > best_value {
            best_value = gbps;
            best_bytes = bytes;
            best_duration_us = ((end - start).as_micros() as u128).max(1);
        }
    }

    let stats = Stats::new(&values, Direction::HigherIsBetter);
    let page = outcomes[0].page.clone();
    let clock_mhz = outcomes
        .iter()
        .filter_map(|o| o.clock.as_ref())
        .map(|c| c.mhz)
        .fold(None::<f64>, |acc, m| Some(acc.unwrap_or(m).max(m)));

    let mut notes: Vec<String> = Vec::new();
    notes.push(format!(
        "buffer: {} per thread × {} thread(s), {} in total",
        crate::sysinfo::human_bytes(plan.bytes_per_thread as u64),
        plan.threads,
        crate::sysinfo::human_bytes(plan.bytes_per_thread.saturating_mul(plan.threads) as u64)
    ));
    notes.push(format!("sizing: {}", plan.derivation));
    if let Some(n) = &plan.note {
        notes.push(format!("sizing note: {n}"));
    }
    if job.work.needs_two_buffers() {
        notes.push(format!(
            "copy counted as {} bytes per iteration ({})",
            if job.settings.copy_counts_both {
                "source + destination"
            } else {
                "source only"
            },
            if job.settings.copy_counts_both {
                "--copy-counts-both"
            } else {
                "default; use --copy-counts-both for AIDA-style counting"
            }
        ));
    }
    if matches!(job.work, Work::WriteNt | Work::CopyNt) && job.kernels.nt_is_normal {
        notes.push(format!(
            "no non-temporal store on the {} path: this cell used ordinary stores",
            job.kernels.isa.label()
        ));
    }
    if page.actual != page.requested {
        notes.push(format!(
            "huge pages requested ({}) but {} are in use{}",
            page.requested.label(),
            page.actual.label(),
            page.note
                .as_ref()
                .map(|n| format!(" — {n}"))
                .unwrap_or_default()
        ));
    } else if let Some(n) = &page.note {
        notes.push(format!("pages: {n}"));
    }
    if !outcomes.iter().all(|o| o.locked) {
        notes.push(
            "buffers could not be mlock'd (check RLIMIT_MEMLOCK): page reclaim can perturb this \
             cell"
                .to_string(),
        );
    }
    if migrations > 0 {
        notes.push(format!(
            "{migrations} affinity check(s) failed: the scheduler moved a worker, so the trial \
             spread is not trustworthy"
        ));
    }
    if let Some(c) = clock_mhz {
        notes.push(format!(
            "effective clock during the cell: {:.0} MHz (measured, not from cpufreq)",
            c
        ));
    }
    if plan.level == Level::Memory {
        notes.push(
            "memory cells read/write DRAM: close heavy applications, especially anything with a \
             large working set"
                .to_string(),
        );
    }
    if outcomes.len() < plan.threads {
        notes.push(format!(
            "only {} of {} workers reported results",
            outcomes.len(),
            plan.threads
        ));
    }

    CellResult {
        level: plan.level,
        op: job.work.op(),
        headline: job.headline,
        value: stats.best,
        unit: "GB/s".to_string(),
        best: stats.best,
        median: stats.median,
        mean: stats.mean,
        stddev: stats.stddev,
        spread_pct: stats.spread_pct,
        rejected_trials: stats.rejected,
        trials: values,
        threads: outcomes.len(),
        cpus: outcomes.iter().map(|o| o.cpu).collect(),
        buffer_bytes_per_thread: plan.bytes_per_thread,
        buffers_per_thread: if job.work.needs_two_buffers() { 2 } else { 1 },
        bytes_moved: best_bytes,
        duration_us: best_duration_us,
        isa: job.kernels.isa,
        pages_requested: job.settings.pages,
        pages_actual: page.actual,
        huge_pages_bytes: page.huge_bytes,
        notes,
        cycles: None,
        clock_mhz,
        peak_share_pct: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernels;
    use crate::sizes;
    use crate::sysinfo;

    fn job(work: Work, level: Level, bytes: usize, threads: usize) -> (Job<'static>, SystemInfoOwned)
    {
        // Owned system info: `Job` borrows it, so leak it for the test.
        let sys: &'static SystemInfo = Box::leak(Box::new(sysinfo::gather()));
        let settings: &'static Settings = Box::leak(
            Box::new(Settings {
                threads: Some(threads),
                trials: 2,
                duration_ms: 20,
                mlock: false,
                ..Default::default()
            }),
        );
        let mut plan = sizes::CellPlan {
            level,
            threads,
            cpus: sys.topology.core_cpus.iter().take(threads).copied().collect(),
            bytes_per_thread: bytes,
            buffers_per_thread: 1,
            total_bytes: bytes * threads,
            cache_bytes: None,
            derivation: "test plan".to_string(),
            note: None,
        };
        plan.cpus = if work.needs_two_buffers() {
            plan.cpus.clone()
        } else {
            plan.cpus.clone()
        };
        let plan: &'static CellPlan = Box::leak(Box::new(plan));
        let job = Job {
            sys,
            settings,
            plan,
            kernels: kernels::Kernels::resolve(None).unwrap(),
            work,
            headline: true,
            cancel: Arc::new(AtomicBool::new(false)),
            events: None,
        };
        (job, SystemInfoOwned)
    }

    /// Marker so the return type above stays honest.
    pub struct SystemInfoOwned;

    #[test]
    #[cfg_attr(not(debug_assertions), ignore = "runs the timed path in release")]
    fn bandwidth_runner_produces_a_number_in_debug_without_timing() {
        // The runner refuses to time in a debug build (rule 2), which is the
        // thing this test pins down.
        let (job, _) = job(Work::Read, Level::L2, 256 * 1024, 1);
        let err = run(&job).unwrap_err();
        assert!(matches!(err, Error::DebugBuild { .. }), "{err:?}");
    }

    #[test]
    fn work_mapping_is_complete() {
        for op in [Op::Read, Op::Write, Op::WriteNt, Op::Copy, Op::CopyNt] {
            let w = Work::from_op(op).expect("mapped");
            assert_eq!(w.op(), op);
        }
        assert_eq!(Work::from_op(Op::Latency), None);
        assert!(Work::Copy.needs_two_buffers());
        assert!(!Work::Read.needs_two_buffers());
        assert_eq!(Work::Copy.bytes_multiplier(false), 1);
        assert_eq!(Work::Copy.bytes_multiplier(true), 2);
        assert_eq!(Work::Read.bytes_multiplier(true), 1);
    }

    #[test]
    fn aggregate_is_deterministic_on_synthetic_samples() {
        // Two threads, two trials, one deliberately slow straggler: the trial
        // window must be earliest-start → latest-end.
        let base = Instant::now();
        let mk = |o: u64, dur: u64| Sample {
            start: base + Duration::from_millis(o),
            end: base + Duration::from_millis(o + dur),
            bytes: 1000,
            migrated: false,
        };
        let a = Outcome {
            cpu: 0,
            samples: vec![mk(0, 100), mk(0, 100)],
            page: PageStatus {
                requested: PageKind::TwoM,
                actual: PageKind::FourK,
                huge_bytes: None,
                note: Some("test".to_string()),
            },
            locked: true,
            clock: None,
            error: None,
        };
        let mut b = a.clone_for_cpu(1);
        b.samples = vec![mk(50, 100), mk(0, 40)];
        let sys: &SystemInfo = Box::leak(Box::new(sysinfo::gather()));
        let settings: &Settings = Box::leak(Box::new(Settings {
            mlock: true,
            ..Default::default()
        }));
        let plan: &CellPlan = Box::leak(Box::new(CellPlan {
            level: Level::L2,
            threads: 2,
            cpus: vec![0, 1],
            bytes_per_thread: 1000,
            buffers_per_thread: 1,
            total_bytes: 2000,
            cache_bytes: None,
            derivation: "unit test".to_string(),
            note: None,
        }));
        let job = Job {
            sys,
            settings,
            plan,
            kernels: kernels::Kernels::resolve(None).unwrap(),
            work: Work::Read,
            headline: true,
            cancel: Arc::new(AtomicBool::new(false)),
            events: None,
        };
        let r = aggregate(&job, &[a, b]);
        assert_eq!(r.trials.len(), 2);
        assert_eq!(r.threads, 2);
        // The trial window is earliest start to latest end, 0..150 ms, so the
        // 2000 bytes moved across both threads are divided by 150 ms -- the
        // aggregate deliberately refuses to pretend two threads did the work of
        // one core.
        let expect = 2000.0 / 0.15 / GBPS;
        assert!(
            (r.trials[0] - expect).abs() < expect * 0.05,
            "{} vs {expect}",
            r.trials[0]
        );
        assert!(r.value >= r.median);
        assert!(r.notes.iter().any(|n| n.contains("unit test")));
        assert!(r
            .notes
            .iter()
            .any(|n| n.contains("huge pages requested") || n.contains("pages:")));
    }

    trait CloneForCpu {
        fn clone_for_cpu(&self, cpu: usize) -> Outcome;
    }
    impl CloneForCpu for Outcome {
        fn clone_for_cpu(&self, cpu: usize) -> Outcome {
            Outcome {
                cpu,
                samples: self.samples.clone(),
                page: self.page.clone(),
                locked: self.locked,
                clock: self.clock.clone(),
                error: None,
            }
        }
    }
}
