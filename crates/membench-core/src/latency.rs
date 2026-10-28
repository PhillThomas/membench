//! Pointer-chase latency (PLAN.md "Latency", rule 5, rule 6).
//!
//! A dependent load chain is the only way to measure latency that cannot be
//! optimised away or hidden by the prefetcher: each load's address comes from the
//! previous load's value, so the hardware cannot overlap them.
//!
//! Two details decide whether the number is real:
//!
//! * **One pointer per cache line.** If a line holds eight pointers, a chase
//!   walks inside one line eight times and measures L1, whatever the buffer size.
//! * **A single closed cycle.** A shuffle that produces several small cycles
//!   lets the whole cycle live in L1 after a few thousand loads. Sattolo's
//!   algorithm always yields exactly one cycle, so the chain is as long as the
//!   buffer.
//!
//! Cycles per load are computed from the clock measured **in the same run on the
//! same core**, never from `cpufreq` (rule 5).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::alloc::{AllocOpts, Buffer};
use crate::kernels;
use crate::result::{
    CellResult, ClockMeasurement, Level, Op, PageKind, Settings, Sweep, SweepPoint, SystemInfo,
};
use crate::runner::Event;
use crate::stats::{Direction, Stats};
use crate::timer;
use crate::{Error, Result};

/// Target duration of one timed latency window.
pub const TRIAL_MS: u64 = 100;

pub struct LatencyJob<'a> {
    pub sys: &'a SystemInfo,
    pub settings: &'a Settings,
    pub cpu: usize,
    pub level: Level,
    pub size_bytes: usize,
    pub headline: bool,
    pub cancel: &'a AtomicBool,
    pub events: Option<&'a Sender<Event>>,
}

/// What a chase measured, before it becomes a result row.
struct Chase {
    stats: Stats,
    loads: u64,
    trials: usize,
    values: Vec<f64>,
    clock: ClockMeasurement,
    pages_actual: PageKind,
    huge_bytes: Option<u64>,
    page_note: Option<String>,
    nodes: usize,
}

pub fn run(job: &LatencyJob) -> Result<CellResult> {
    crate::ensure_release_build("latency measurement")?;
    let chase = chase(job)?;

    let cycles = Some(chase.stats.best / (1000.0 / chase.clock.mhz.max(0.001)));
    let mut notes = vec![format!(
        "{} nodes × {} B line = {} of chain, single cycle (Sattolo), one pointer per line",
        chase.nodes,
        job.sys.caches.line_bytes.unwrap_or(64),
        crate::sysinfo::human_bytes(job.size_bytes as u64)
    )];
    notes.push(format!(
        "clock {:.0} MHz measured on CPU {} during this cell ({}), not from cpufreq",
        chase.clock.mhz, chase.clock.cpu, chase.clock.method
    ));
    if let Some(note) = &chase.page_note {
        notes.push(format!("pages: {note}"));
    }
    if chase.pages_actual == PageKind::FourK && job.settings.pages != PageKind::FourK {
        notes.push(format!(
            "huge pages requested ({}) but 4K pages are in use, so the TLB is part of what this \
             cell measures",
            job.settings.pages.label()
        ));
    }
    if job.level.is_memory() {
        notes.push(
            "chain is far larger than the last-level cache: this is DRAM latency including the \
             page-walk for a 4K-mapped chain"
                .to_string(),
        );
    }

    Ok(CellResult {
        level: job.level,
        op: Op::Latency,
        headline: job.headline,
        value: chase.stats.best,
        unit: "ns".to_string(),
        best: chase.stats.best,
        median: chase.stats.median,
        mean: chase.stats.mean,
        stddev: chase.stats.stddev,
        spread_pct: chase.stats.spread_pct,
        rejected_trials: chase.stats.rejected,
        trials: chase.values.clone(),
        threads: 1,
        cpus: vec![job.cpu],
        buffer_bytes_per_thread: job.size_bytes,
        buffers_per_thread: 1,
        bytes_moved: (chase.loads * job.sys.caches.line_bytes.unwrap_or(64) as u64) as u128,
        duration_us: (chase.stats.best * chase.loads as f64 / 1000.0) as u128,
        isa: crate::result::Isa::Scalar,
        pages_requested: job.settings.pages,
        pages_actual: chase.pages_actual,
        huge_pages_bytes: chase.huge_bytes,
        notes,
        cycles,
        clock_mhz: Some(chase.clock.mhz),
        peak_share_pct: None,
    })
}

/// Build the chain, pin, and chase it `trials` times.
fn chase(job: &LatencyJob) -> Result<Chase> {
    let line = job.sys.caches.line_bytes.unwrap_or(64) as usize;
    if line < 8 || !line.is_power_of_two() {
        return Err(Error::NoCacheLineSize);
    }
    let size = job.size_bytes.max(line * 2);

    if let Err(e) = crate::affinity::pin_current(job.cpu) {
        return Err(Error::PinFailed {
            cpu: job.cpu,
            os: std::io::Error::other(e.to_string()),
        });
    }
    if job.settings.boost_priority {
        let _ = crate::affinity::boost_nice(-5);
    }

    let mut buf = Buffer::new(
        size,
        AllocOpts {
            pages: job.settings.pages,
            prefault: true,
            lock: job.settings.mlock,
        },
    )?;
    let (head, nodes) = build_chain(&mut buf, line, job.settings.seed)?;
    let page = buf.page_status();

    // Calibrate: one pass, timed only to size the window.
    let warm = Instant::now();
    let sink = unsafe { kernels::chase(head as *const u64, nodes as u64) };
    std::hint::black_box(sink);
    let warm_dur = warm.elapsed().max(Duration::from_micros(50));

    let target_ms = job.settings.duration_ms.clamp(5, TRIAL_MS);
    let passes = ((target_ms as u128 * 1_000_000) / warm_dur.as_nanos().max(1)).max(1) as u64;
    let loads = (nodes as u64).saturating_mul(passes);
    // The kernel performs `loads & !15` dependent loads.
    let effective = loads & !15;
    if effective == 0 {
        return Err(Error::Unsizeable {
            level: job.level,
            why: "chain shorter than one unrolled group".to_string(),
        });
    }

    let mut values = Vec::with_capacity(job.settings.trials.max(1));
    for trial in 0..job.settings.trials.max(1) {
        if job.cancel.load(Ordering::Relaxed) {
            break;
        }
        // Rule 4: still on the intended core?
        if let Ok(set) = crate::affinity::current_affinity() {
            if set.as_slice() != [job.cpu] {
                let _ = crate::affinity::pin_current(job.cpu);
            }
        }
        let start = Instant::now();
        let last = unsafe { kernels::chase(head as *const u64, loads) };
        let elapsed = start.elapsed();
        std::hint::black_box(last);
        let ns = elapsed.as_nanos() as f64 / effective as f64;
        values.push(ns);
        if let Some(tx) = job.events {
            let _ = tx.send(Event::Trial {
                level: job.level,
                op: Op::Latency,
                trial,
                trials: job.settings.trials.max(1),
                elapsed_us: elapsed.as_micros() as u64,
            });
        }
    }
    if values.is_empty() {
        return Err(Error::Unsupported("cancelled"));
    }

    let stats = Stats::new(&values, Direction::LowerIsBetter);
    // Same core, same thermal moment as the measurement.
    let clock = timer::measure_core_clock(Duration::from_millis(20))?;

    Ok(Chase {
        stats,
        loads: effective,
        trials: values.len(),
        values,
        clock,
        pages_actual: page.actual,
        huge_bytes: page.huge_bytes,
        page_note: page.note,
        nodes,
    })
}

/// Lay out a single closed cycle: `nodes` slots, one per cache line.
///
/// Returns the head pointer and the node count. The last link points back to the
/// head, so the chain never runs off the end of the buffer.
///
/// # Safety
/// The returned pointer addresses `buf`, which the caller must keep alive.
/// Address of the first node of a chain.
pub type ChainHead = usize;

pub fn build_chain(buf: &mut Buffer, line: usize, seed: u64) -> Result<(ChainHead, usize)> {
    let nodes = buf.len() / line;
    if nodes < 2 {
        return Err(Error::Unsizeable {
            level: Level::Memory,
            why: format!(
                "{} of buffer holds {} node(s) of {} B",
                crate::sysinfo::human_bytes(buf.len() as u64),
                nodes,
                line
            ),
        });
    }
    let base = buf.ptr() as usize;
    let mut order: Vec<usize> = (0..nodes).collect();
    sattolo(&mut order, seed);

    for k in 0..nodes {
        let from = base + order[k] * line;
        let to = base + order[(k + 1) % nodes] * line;
        // SAFETY: both addresses are inside the mapping and 8-byte aligned (the
        // base is 2 MiB aligned and `line` is a multiple of 8).
        unsafe {
            std::ptr::write_unaligned(from as *mut u64, to as u64);
        }
    }
    // SAFETY: the chain only ever visits addresses written above.
    Ok((base + order[0] * line, nodes))
}

/// Sattolo's algorithm: unlike Fisher–Yates this always produces **one** cycle,
/// which is the property the chase depends on.
pub fn sattolo(v: &mut [usize], seed: u64) {
    let mut rng = seed | 1;
    for i in (1..v.len()).rev() {
        rng = xorshift64star(&mut rng);
        let j = (rng % (i as u64)) as usize;
        v.swap(i, j);
    }
}

fn xorshift64star(state: &mut u64) -> u64 {
    let mut x = *state;
    if x == 0 {
        x = 0x2545_F491_4F6C_DD1D;
    }
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// Sizes for the latency-vs-block-size sweep: powers of two from 4 KiB to 1 GiB
/// with a 1.5× point between each pair, so the cache boundaries are bracketed by
/// more than one sample.
pub fn sweep_ladder(max_bytes: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut size = 4096usize;
    while size <= max_bytes {
        out.push(size);
        let next = size * 3 / 2;
        if next > size && next < size * 2 && next <= max_bytes {
            out.push(next);
        }
        size *= 2;
    }
    out.dedup();
    out
}

pub struct SweepJob<'a> {
    pub sys: &'a SystemInfo,
    pub settings: &'a Settings,
    pub cpu: usize,
    pub cancel: &'a AtomicBool,
    pub events: Option<&'a Sender<Event>>,
}

pub fn run_sweep(job: &SweepJob) -> Result<Sweep> {
    crate::ensure_release_build("the latency sweep")?;
    let mut settings = job.settings.clone();
    settings.trials = job.settings.sweep_trials.unwrap_or(3).max(1);
    settings.duration_ms = job.settings.sweep_duration_ms.unwrap_or(60).max(5);

    let max_bytes = sweep_max_bytes(job.sys);
    let mut points = Vec::new();
    let mut notes = vec![format!(
        "sweep ladder 4 KiB → {} on CPU {}, one pointer per cache line",
        crate::sysinfo::human_bytes(max_bytes as u64),
        job.cpu
    )];

    for size in sweep_ladder(max_bytes) {
        if job.cancel.load(Ordering::Relaxed) {
            notes.push("sweep stopped early".to_string());
            break;
        }
        if let Some(tx) = job.events {
            let _ = tx.send(Event::SweepStart { size_bytes: size });
        }
        let inner = LatencyJob {
            sys: job.sys,
            settings: &settings,
            cpu: job.cpu,
            level: Level::Memory,
            size_bytes: size,
            headline: false,
            cancel: job.cancel,
            events: None,
        };
        let c = match chase(&inner) {
            Ok(c) => c,
            Err(e) => {
                notes.push(format!(
                    "{} skipped: {e}",
                    crate::sysinfo::human_bytes(size as u64)
                ));
                continue;
            }
        };
        let clock = c.clock.mhz.max(0.001);
        let point = SweepPoint {
            size_bytes: size,
            nodes: c.nodes,
            ns: c.stats.best,
            cycles: Some(c.stats.best / (1000.0 / clock)),
            trials: c.trials,
            best_ns: c.stats.best,
            spread_pct: c.stats.spread_pct,
            pages_actual: c.pages_actual,
            huge_pages_bytes: c.huge_bytes,
        };
        if let Some(tx) = job.events {
            let _ = tx.send(Event::SweepPoint(point.clone()));
        }
        points.push(point);
    }

    if points.is_empty() {
        return Err(Error::Unsupported("the sweep produced no points"));
    }
    notes.push(
        "steps in this curve are cache boundaries: L1 → L2 → L3 → page-walk → DRAM".to_string(),
    );
    Ok(Sweep {
        points,
        notes,
        duration_ms: settings.duration_ms,
        trials: settings.trials,
    })
}

/// Never sweep past half the free RAM: a 1 GiB chain is fine on a 16 GiB box and
/// a disaster on a 1 GiB VM.
fn sweep_max_bytes(sys: &SystemInfo) -> usize {
    let ceiling = 1024 * 1024 * 1024usize;
    match crate::sysinfo::available_ram_bytes() {
        Some(avail) => ceiling.min((avail / 2) as usize).max(1024 * 1024),
        None => {
            let _ = sys;
            256 * 1024 * 1024
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sattolo_makes_exactly_one_cycle() {
        for seed in [1u64, 7, 999, u64::MAX] {
            for n in [2usize, 3, 17, 1024] {
                let mut v: Vec<usize> = (0..n).collect();
                sattolo(&mut v, seed);
                // v[i] is the node after i in this test's reading; follow it and
                // require a single cycle covering every node.
                let mut seen = vec![false; n];
                let mut at = 0usize;
                for _ in 0..n {
                    assert!(
                        !seen[at],
                        "revisited {at} after {} steps (seed {seed}, n {n})",
                        seen.iter().filter(|s| **s).count()
                    );
                    seen[at] = true;
                    at = v[at];
                }
                assert!(
                    seen.iter().all(|s| *s),
                    "not all nodes reachable, seed {seed} n {n}"
                );
                assert_eq!(at, 0, "cycle did not close, seed {seed} n {n}");
            }
        }
    }

    #[test]
    fn chain_links_back_to_the_head_and_visits_every_node() {
        let line = 64;
        let mut buf = Buffer::new(64 * 1024, AllocOpts::default()).unwrap();
        let (head, nodes) = build_chain(&mut buf, line, 42).unwrap();
        assert_eq!(nodes, 64 * 1024 / line);
        let base = buf.ptr() as usize;
        assert!((head - base) % line == 0, "head not line aligned");

        let mut seen = vec![false; nodes];
        let mut at = head;
        for step in 0..nodes {
            let idx = (at - base) / line;
            assert!(idx < nodes, "chase left the buffer at step {step}");
            assert!(!seen[idx], "cycle closed early at step {step}");
            seen[idx] = true;
            // SAFETY: idx is inside the buffer.
            at = unsafe { std::ptr::read_unaligned((base + idx * line) as *const u64) } as usize;
        }
        assert!(seen.iter().all(|s| *s));
        assert_eq!(at, head, "chain does not close on the head");
    }

    #[test]
    fn chain_rejects_buffers_below_two_nodes() {
        // `Buffer` rounds up to a page, so a node larger than the mapping is the
        // way to ask for fewer than two nodes.
        let mut buf = Buffer::new(64, AllocOpts::default()).unwrap();
        let len = buf.len();
        assert!(build_chain(&mut buf, len * 2, 1).is_err());
        assert!(build_chain(&mut buf, len, 1).is_err());
    }

    #[test]
    fn sweep_ladder_brackets_the_caches() {
        let l = sweep_ladder(1024 * 1024 * 1024);
        assert_eq!(l.first(), Some(&4096));
        assert!(l.len() > 15, "{l:?}");
        assert!(l.windows(2).all(|w| w[1] > w[0]), "not ascending: {l:?}");
        // Both sides of a 32K L1 and a 16M L3 must be sampled.
        let covers =
            |target: usize| l.iter().any(|s| *s <= target) && l.iter().any(|s| *s > target);
        assert!(covers(32 * 1024));
        assert!(covers(16 * 1024 * 1024));
        // Halved ceiling shortens the ladder but never empties it.
        assert!(!sweep_ladder(1024 * 1024).is_empty());
        // Below the first rung there is nothing to sample; `sweep_max_bytes`
        // clamps real runs to at least one rung, so an empty ladder here is the
        // documented answer rather than a silent single point.
        assert!(sweep_ladder(2048).is_empty(), "{:?}", sweep_ladder(2048));
        assert_eq!(sweep_ladder(4096), vec![4096]);
    }

    /// Only meaningful in a debug build: in a release build this job would
    /// really chase pointers, which is a benchmark, not a unit test. The
    /// release side of the same rule is checked cheaply in `runner`.
    #[cfg(debug_assertions)]
    #[test]
    fn latency_refuses_debug_builds() {
        let sys = Box::leak(Box::new(crate::sysinfo::gather()));
        let settings = Box::leak(Box::new(Settings::default()));
        let cancel = AtomicBool::new(false);
        let job = LatencyJob {
            sys,
            settings,
            cpu: 0,
            level: Level::L2,
            size_bytes: 256 * 1024,
            headline: true,
            cancel: &cancel,
            events: None,
        };
        let err = run(&job).unwrap_err();
        assert!(matches!(err, Error::DebugBuild { .. }), "{err:?}");
    }

    #[test]
    fn chase_kernel_follows_the_chain_in_debug() {
        // The kernel itself is ISA-level code; check it lands where the chain
        // says it should, using the portable path's contract.
        let line = 64;
        let mut buf = Buffer::new(16 * 1024, AllocOpts::default()).unwrap();
        let (head, nodes) = build_chain(&mut buf, line, 7).unwrap();
        let loads = (nodes as u64) & !15;
        // SAFETY: chain built over a live buffer.
        let last = unsafe { kernels::chase(head as *const u64, loads) } as usize;
        let base = buf.ptr() as usize;
        assert!((last - base) % line == 0, "landing {last:#x} is off a line");
        assert!(last >= base && last < base + buf.len());
    }
}
