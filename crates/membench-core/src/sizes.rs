//! Buffer sizing (PLAN.md "Buffer sizing", rule 3, rule 6).
//!
//! The whole point of this module is that a "cache" number is only a cache
//! number if the buffer really stayed in that cache. Two rules drive it:
//!
//! * **Half a cache, not all of it.** A buffer that exactly fills an L2 spends
//!   half its time on replacement traffic, so each thread gets half the level it
//!   is meant to measure (`0.5 × size` for L1d and L2).
//! * **Per thread, not per process.** With SMT, two threads share one L1d, so the
//!   budget is split by the threads that actually share the level: L1/L2 by the
//!   threads on that core (we avoid that by using one thread per physical core),
//!   L3 by every core in the L3 domain.
//!
//! Every plan carries the arithmetic that produced it (`derivation`) plus a
//! note whenever the buffer is bigger than the level it targets, so a cell can
//! never be read as something it is not.

use crate::result::{CacheInfo, Level, Settings, SystemInfo};
use crate::{Error, Result};

/// Smallest usable bandwidth buffer per thread. Below this the timed region is
/// loop overhead, not memory.
pub const MIN_BANDWIDTH_BYTES: usize = 1024;

/// Latency chains need one pointer per cache line; this is the floor for the
/// whole chain (PLAN.md: at least one page so the TLB is at least exercised).
pub const MIN_LATENCY_BYTES: usize = 4096;

/// The memory-sized target, before it is split between threads.
pub const MIN_MEMORY_TOTAL_BYTES: usize = 1024 * 1024 * 1024;

/// Multiple of the page size every buffer is rounded down to.
pub const ROUND_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellPlan {
    pub level: Level,
    pub threads: usize,
    pub cpus: Vec<usize>,
    pub bytes_per_thread: usize,
    /// 2 for copy operations (source and destination), 1 otherwise.
    pub buffers_per_thread: usize,
    pub total_bytes: usize,
    /// The cache size this level was derived from, when known.
    pub cache_bytes: Option<u64>,
    /// Human arithmetic, printed in the detail view: "0.5 × 32K L1d = 16K".
    pub derivation: String,
    /// Set whenever the buffer does not fit the level it is meant to measure.
    pub note: Option<String>,
}

impl CellPlan {
    pub fn bytes_per_trial(&self) -> usize {
        self.bytes_per_thread.saturating_mul(self.threads)
    }
}

/// Size every requested level.
pub fn plan(sys: &SystemInfo, settings: &Settings) -> Result<Vec<CellPlan>> {
    let mut out = Vec::new();
    for level in Level::ALL {
        if !settings.levels.is_empty() && !settings.levels.contains(&level) {
            continue;
        }
        out.push(for_level(sys, settings, level)?);
    }
    Ok(out)
}

pub fn for_level(sys: &SystemInfo, settings: &Settings, level: Level) -> Result<CellPlan> {
    let page = sys.os.page_size_bytes.max(1);
    // `None` means "as many as this level can use": one thread per physical core
    // for the cache levels, every logical CPU for DRAM.
    let requested_threads = settings.threads.unwrap_or_else(|| match level {
        Level::Memory => sys.topology.all_cpus.len().max(1),
        _ => core_cpus(sys).len().max(1),
    });
    let requested_threads = requested_threads.max(1);

    let (threads, cpus, cache_bytes, bytes, derivation) = match level {
        Level::L1 => l1(sys, requested_threads)?,
        Level::L2 => l2(sys, requested_threads)?,
        Level::L3 => l3(sys, requested_threads)?,
        Level::Memory => memory(sys, settings, requested_threads)?,
    };

    if threads == 0 {
        return Err(Error::Unsizeable {
            level,
            why: "no usable CPUs".to_string(),
        });
    }

    let bytes = clamp_bytes(bytes, MIN_BANDWIDTH_BYTES, page);
    let note = oversized_note(sys, level, bytes, threads, cache_bytes);

    Ok(CellPlan {
        level,
        threads,
        cpus,
        bytes_per_thread: bytes,
        buffers_per_thread: 1,
        total_bytes: bytes.saturating_mul(threads),
        cache_bytes,
        derivation,
        note,
    })
}

/// What one level planner decides: bytes per thread, the CPUs to run on, the
/// cache size it was sized against (`None` for the memory row), the thread
/// count, and the note that tells the user how the size was arrived at.
type Planned = (usize, Vec<usize>, Option<u64>, usize, String);

fn l1(sys: &SystemInfo, requested: usize) -> Result<Planned> {
    let l1d = data_cache(sys, 1).ok_or(Error::MissingCache(Level::L1))?;
    let cores = core_cpus(sys);
    if cores.is_empty() {
        return Err(Error::Unsizeable {
            level: Level::L1,
            why: "no CPU list".to_string(),
        });
    }
    // One thread per physical core: two SMT siblings in one 32 KiB L1d both
    // measure something else.
    let threads = requested.min(cores.len());
    let cpus = cores[..threads].to_vec();
    let bytes = half(l1d) as usize;
    let derivation = format!(
        "0.5 × {} L1d per core = {} per thread, {threads} thread(s) on {}/{} physical cores",
        crate::sysinfo::human_bytes(l1d),
        crate::sysinfo::human_bytes(bytes as u64),
        threads,
        cores.len()
    );
    Ok((threads, cpus, Some(l1d), bytes, derivation))
}

fn l2(sys: &SystemInfo, requested: usize) -> Result<Planned> {
    let l2 = data_cache(sys, 2).ok_or(Error::MissingCache(Level::L2))?;
    let cores = core_cpus(sys);
    if cores.is_empty() {
        return Err(Error::Unsizeable {
            level: Level::L2,
            why: "no CPU list".to_string(),
        });
    }
    let threads = requested.min(cores.len());
    let cpus = cores[..threads].to_vec();
    let bytes = half(l2) as usize;
    let derivation = format!(
        "0.5 × {} L2 = {} per thread, {threads} thread(s)",
        crate::sysinfo::human_bytes(l2),
        crate::sysinfo::human_bytes(bytes as u64)
    );
    Ok((threads, cpus, Some(l2), bytes, derivation))
}

/// L3 needs a range, not a fraction: big enough to spill out of every L2 on the
/// way through, small enough to stay inside the share of L3 this thread gets.
fn l3(sys: &SystemInfo, requested: usize) -> Result<Planned> {
    let l3_bytes = data_cache(sys, 3).ok_or(Error::MissingCache(Level::L3))?;
    let l2_bytes = data_cache(sys, 2).unwrap_or(l3_bytes / 8);
    let domains = &sys.caches.l3_domains;

    // Work inside the smallest L3 domain so the arithmetic holds on a chip with
    // unequal CCs, and stay inside one domain so the number is not cross-domain.
    let domain = domains
        .iter()
        .min_by_key(|d| (d.size_bytes.unwrap_or(l3_bytes), d.cpus.len()))
        .cloned()
        .unwrap_or_else(|| crate::result::L3Domain {
            id: 0,
            cpus: core_cpus(sys),
            size_bytes: Some(l3_bytes),
        });
    let domain_size = domain.size_bytes.unwrap_or(l3_bytes);
    let cores_in_domain: Vec<usize> = {
        let cores = core_cpus(sys);
        let mut v: Vec<usize> = cores
            .iter()
            .filter(|cpu| domain.cpus.contains(cpu))
            .copied()
            .collect();
        if v.is_empty() {
            v = cores;
        }
        v
    };

    let mut threads = requested.min(cores_in_domain.len()).max(1);
    // Declared outside the loop and assigned on its first pass: the amount of
    // the domain each thread gets to itself.
    let mut share: usize;
    loop {
        share = ((domain_size as usize) * 3 / 4).max(1) / threads;
        if (share as u64) * 2 >= l2_bytes * 2 || threads == 1 {
            break;
        }
        threads -= 1;
    }

    let lower = (l2_bytes as usize).max(MIN_BANDWIDTH_BYTES) * 2;
    let mut bytes = share.max(lower);
    let mut note_reason = None;
    if bytes > domain_size as usize {
        note_reason = Some(format!(
            "even one thread cannot get a share of L3 that fits; using {} which is larger than \
             the {} domain",
            crate::sysinfo::human_bytes(bytes as u64),
            crate::sysinfo::human_bytes(domain_size)
        ));
        bytes = domain_size as usize;
    }

    let cpus = cores_in_domain[..threads.min(cores_in_domain.len())].to_vec();
    let derivation = format!(
        "0.75 × {} L3 ÷ {threads} core(s) in L3 domain {} = {} per thread (floor 2 × {} L2 = {})",
        crate::sysinfo::human_bytes(domain_size),
        domain.id,
        crate::sysinfo::human_bytes(bytes as u64),
        crate::sysinfo::human_bytes(l2_bytes),
        crate::sysinfo::human_bytes(lower as u64)
    );
    if let Some(why) = note_reason {
        return Ok((
            threads.max(1),
            cpus,
            Some(l3_bytes),
            bytes,
            format!("{derivation}; {why}"),
        ));
    }
    Ok((threads.max(1), cpus, Some(l3_bytes), bytes, derivation))
}

fn memory(sys: &SystemInfo, settings: &Settings, requested: usize) -> Result<Planned> {
    let total_l3 = sys
        .caches
        .data_level(3)
        .and_then(|c| c.total_bytes)
        .unwrap_or(0);
    let mut target = (MIN_MEMORY_TOTAL_BYTES as u64).max(16 * (total_l3 + 1));

    // Never ask for more than half of what the machine has free; a run that
    // starts swapping measures the swap device.
    let available = crate::sysinfo::available_ram_bytes().unwrap_or(u64::MAX / 4);
    let cap = (available / 2).max(MIN_BANDWIDTH_BYTES as u64 * 2);
    let mut clamped = false;
    if target > cap {
        target = cap;
        clamped = true;
    }

    // Memory bandwidth scales with memory channels, not cores: use every logical
    // CPU, which is what STREAM does.
    let cpus = sys.topology.all_cpus.clone();
    let threads = requested.min(cpus.len().max(1));
    let bytes = (target as usize / threads.max(1)).max(MIN_BANDWIDTH_BYTES);

    let mut derivation = format!(
        "max(1 GiB, 16 × {} total L3) ÷ {threads} logical CPU(s) = {} per thread",
        if total_l3 == 0 {
            "unknown".to_string()
        } else {
            crate::sysinfo::human_bytes(total_l3)
        },
        crate::sysinfo::human_bytes(bytes as u64)
    );
    if clamped {
        derivation.push_str(&format!(
            "; clamped to half of the {} RAM available to avoid swapping",
            crate::sysinfo::human_bytes(available)
        ));
    }
    if settings.copy_counts_both {
        derivation.push_str(" (copy counts both buffers)");
    }
    Ok((threads, cpus, Some(total_l3), bytes, derivation))
}

/// The data cache for a level: L1d specifically, then unified, then anything.
fn data_cache(sys: &SystemInfo, level: u8) -> Option<u64> {
    let c: &CacheInfo = &sys.caches;
    c.data_level(level).and_then(|c| c.size_bytes).or_else(|| {
        c.levels
            .iter()
            .find(|c| c.level == level)
            .and_then(|c| c.size_bytes)
    })
}

/// Physical-core representatives, fastest first, so thread 0 lands on a P-core.
fn core_cpus(sys: &SystemInfo) -> Vec<usize> {
    crate::sysinfo::topology::core_order(sys)
}

fn half(bytes: u64) -> u64 {
    (bytes / 2).max(MIN_BANDWIDTH_BYTES as u64)
}

/// Round down to the page size, floor at `min`.
fn clamp_bytes(bytes: usize, min: usize, page: usize) -> usize {
    let page = page.max(1);
    let rounded = bytes / page * page;
    rounded.max(min)
}

/// PLAN.md rule 3: a per-cell note whenever the buffer exceeds a cache.
fn oversized_note(
    sys: &SystemInfo,
    level: Level,
    bytes_per_thread: usize,
    threads: usize,
    _cache_bytes: Option<u64>,
) -> Option<String> {
    let per_thread_total = bytes_per_thread;
    let line = sys.caches.line_bytes.unwrap_or(64) as usize;
    let _ = line;
    match level {
        Level::L1 => {
            let l1d = data_cache(sys, 1)?;
            if (per_thread_total as u64) > l1d {
                Some(format!(
                    "buffer exceeds L1d ({} > {}), so this cell is L2-bound",
                    crate::sysinfo::human_bytes(per_thread_total as u64),
                    crate::sysinfo::human_bytes(l1d)
                ))
            } else {
                None
            }
        }
        Level::L2 => {
            let l2 = data_cache(sys, 2)?;
            if (per_thread_total as u64) > l2 {
                Some(format!(
                    "buffer exceeds L2 ({} > {}), so this cell is L3-bound",
                    crate::sysinfo::human_bytes(per_thread_total as u64),
                    crate::sysinfo::human_bytes(l2)
                ))
            } else {
                let l1d = data_cache(sys, 1).unwrap_or(0);
                if (per_thread_total as u64) > l1d {
                    Some(format!(
                        "buffer is {}× L1d, so it is L2-resident as intended",
                        ratio(per_thread_total as u64, l1d)
                    ))
                } else {
                    None
                }
            }
        }
        Level::L3 => {
            let l3 = data_cache(sys, 3)?;
            let aggregate = (per_thread_total as u64) * threads as u64;
            if aggregate > l3 {
                Some(format!(
                    "the {threads} buffers together are {} > {} L3, so part of this cell is \
                     memory traffic",
                    crate::sysinfo::human_bytes(aggregate),
                    crate::sysinfo::human_bytes(l3)
                ))
            } else {
                let l2 = data_cache(sys, 2).unwrap_or(0);
                if l2 > 0 && (per_thread_total as u64) < 2 * l2 {
                    Some(format!(
                        "buffer is under 2 × L2 ({}), so some lines may still be caught by L2",
                        crate::sysinfo::human_bytes(per_thread_total as u64)
                    ))
                } else {
                    None
                }
            }
        }
        Level::Memory => {
            let l3 = data_cache(sys, 3).unwrap_or(0);
            let aggregate = (per_thread_total as u64) * threads as u64;
            if l3 > 0 && aggregate < 4 * l3 {
                Some(format!(
                    "working set {} is under 4 × total L3 ({}), so cache residency may inflate \
                     this number",
                    crate::sysinfo::human_bytes(aggregate),
                    crate::sysinfo::human_bytes(4 * l3)
                ))
            } else {
                None
            }
        }
    }
}

fn ratio(a: u64, b: u64) -> String {
    if b == 0 {
        "Unknown".to_string()
    } else {
        format!("{:.1}", a as f64 / b as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::*;

    /// A synthetic 8-core / 16-thread machine with Intel-style hybrid-free
    /// caches, so the sizing table is a fixed assertion instead of whatever the
    /// build host happens to be.
    pub fn fixture(
        l1d: u64,
        l2: u64,
        l3: u64,
        cores: usize,
        threads_per_core: usize,
    ) -> SystemInfo {
        let logical = cores * threads_per_core;
        let cores_list: Vec<usize> = (0..cores).collect();
        let all: Vec<usize> = (0..logical).collect();
        let cache = |level: u8, kind: CacheKind, size: u64, instances: usize| CacheLevel {
            level,
            kind,
            size_bytes: Some(size),
            line_bytes: Some(64),
            physical_line_bytes: Some(64),
            ways: Some(8),
            sets: Some(64),
            shared_cpus: vec![0],
            instances: Some(instances),
            total_bytes: Some(size * instances as u64),
            sizes_vary_between_cores: false,
        };
        SystemInfo {
            cpu: CpuInfo::default(),
            cores: (0..logical)
                .map(|cpu| CoreInfo {
                    cpu,
                    core_id: Some(cpu as u32 / threads_per_core as u32),
                    physical_core: Some(cpu / threads_per_core),
                    is_boot_core: cpu == 0,
                    siblings: (0..threads_per_core)
                        .map(|t| (cpu / threads_per_core) * threads_per_core + t)
                        .collect(),
                    thread_idx: Some((cpu % threads_per_core) as u32),
                    capacity: None,
                    cluster: None,
                    max_freq_khz: Some(4_000_000),
                    cur_freq_khz: None,
                    governor: Some("performance".to_string()),
                    energy_performance_preference: None,
                    hybrid_role: None,
                })
                .collect(),
            topology: Topology {
                logical_cpus: logical,
                physical_cores: cores,
                smt_enabled: threads_per_core > 1,
                core_cpus: cores_list,
                all_cpus: all,
                hybrid: false,
                numa_nodes: vec![NumaNode {
                    id: 0,
                    cpus: (0..logical).collect(),
                    total_ram_bytes: Some(16 * 1024 * 1024 * 1024),
                }],
                sockets: 1,
            },
            caches: CacheInfo {
                levels: vec![
                    cache(1, CacheKind::Data, l1d, cores),
                    cache(1, CacheKind::Instruction, l1d, cores),
                    cache(2, CacheKind::Unified, l2, cores),
                    cache(3, CacheKind::Unified, l3, 1),
                ],
                line_bytes: Some(64),
                l3_domains: vec![L3Domain {
                    id: 0,
                    cpus: (0..logical).collect(),
                    size_bytes: Some(l3),
                }],
            },
            memory: MemoryInfo::default(),
            os: OsInfo {
                kernel_release: Some("test".to_string()),
                hostname: Some("test".to_string()),
                page_size_bytes: 4096,
                membench_version: "test".to_string(),
                running_as_root: false,
                mlock_limit_bytes: None,
            },
            warnings: vec![],
        }
    }

    fn settings(threads: usize) -> Settings {
        Settings {
            threads: Some(threads),
            ..Settings::default()
        }
    }

    #[test]
    fn sizing_table_on_the_reference_fixture() {
        // 8 cores, SMT2, 32K L1d, 512K L2, 16M L3: the Ryzen-5650G-shaped case.
        let sys = fixture(32 * 1024, 512 * 1024, 16 * 1024 * 1024, 8, 2);
        let s = settings(8);
        let l1 = for_level(&sys, &s, Level::L1).unwrap();
        assert_eq!(l1.bytes_per_thread, 16 * 1024, "L1 = half of 32K");
        assert_eq!(l1.threads, 8, "one thread per physical core");
        assert_eq!(l1.cpus, vec![0, 1, 2, 3, 4, 5, 6, 7]);

        let l2 = for_level(&sys, &s, Level::L2).unwrap();
        assert_eq!(l2.bytes_per_thread, 256 * 1024, "L2 = half of 512K");

        let l3 = for_level(&sys, &s, Level::L3).unwrap();
        // 0.75 × 16M / 8 = 1.5M, and the floor is 2 × 512K = 1M, so 1.5M wins.
        assert_eq!(l3.bytes_per_thread, 1536 * 1024);
        assert!(l3.derivation.contains("L3 domain 0"), "{}", l3.derivation);

        // Only an unpinned request spreads over every logical CPU; an explicit
        // --threads n is obeyed literally at every level.
        let unpinned = Settings {
            threads: None,
            ..s.clone()
        };
        let mem = for_level(&sys, &unpinned, Level::Memory).unwrap();
        assert_eq!(mem.threads, 16, "memory uses every logical CPU");
        // 16 × 16M = 256M < 1 GiB, so the 1 GiB floor governs: 1 GiB / 16.
        assert_eq!(mem.bytes_per_thread, 64 * 1024 * 1024);

        for plan in [l1, l2, l3, mem] {
            assert_eq!(
                plan.bytes_per_thread % ROUND_BYTES,
                0,
                "{:?} not page aligned",
                plan.level
            );
            assert!(plan.bytes_per_thread >= MIN_BANDWIDTH_BYTES);
            assert!(!plan.derivation.is_empty());
        }
    }

    #[test]
    fn memory_cell_grows_with_big_l3() {
        // 256M of L3: 16 × 256M = 4 GiB beats the 1 GiB floor.
        let sys = fixture(48 * 1024, 2 * 1024 * 1024, 256 * 1024 * 1024, 16, 2);
        let mem = for_level(&sys, &settings(16), Level::Memory).unwrap();
        assert!(mem.bytes_per_thread * mem.threads >= 4 * 1024 * 1024 * 1024);
        assert!(mem.derivation.contains("total L3"), "{}", mem.derivation);
    }

    #[test]
    fn tiny_l3_domain_forces_fewer_threads_instead_of_an_impossible_buffer() {
        // L2 2M with only 4M of L3 across 8 cores: 0.75×4M/8 = 384K, below the
        // 4M floor (2 × L2), so threads must drop.
        let sys = fixture(64 * 1024, 2 * 1024 * 1024, 4 * 1024 * 1024, 8, 1);
        let l3 = for_level(&sys, &settings(8), Level::L3).unwrap();
        assert!(l3.threads < 8, "threads must shrink: {}", l3.threads);
        assert!(
            l3.bytes_per_thread as u64 >= 2 * (2 * 1024 * 1024) || l3.threads == 1,
            "{}",
            l3.bytes_per_thread
        );
        // The derivation always explains what happened.
        assert!(l3.derivation.contains("floor"), "{}", l3.derivation);
    }

    #[test]
    fn missing_cache_levels_are_typed_errors() {
        let mut sys = fixture(32 * 1024, 512 * 1024, 16 * 1024 * 1024, 4, 1);
        sys.caches.levels.retain(|c| c.level != 2);
        for c in sys.caches.levels.iter_mut() {
            if c.level == 3 {
                c.size_bytes = None;
            }
        }
        sys.caches.l3_domains.clear();
        let e = for_level(&sys, &settings(1), Level::L2).unwrap_err();
        assert!(matches!(e, Error::MissingCache(Level::L2)), "{e:?}");
        // L3 now has no size at all.
        let e = for_level(&sys, &settings(1), Level::L3).unwrap_err();
        assert!(matches!(e, Error::MissingCache(Level::L3)), "{e:?}");
        // L1 still works and says so.
        assert!(for_level(&sys, &settings(1), Level::L1).is_ok());
    }

    #[test]
    fn notes_appear_when_the_buffer_outgrows_the_level() {
        let mut sys = fixture(32 * 1024, 512 * 1024, 16 * 1024 * 1024, 4, 1);
        // Shrink L1 to 4K so the half-of-L2 buffer is far bigger than L1.
        for c in sys.caches.levels.iter_mut() {
            if c.level == 1 {
                c.size_bytes = Some(4096);
            }
        }
        let n = oversized_note(&sys, Level::L1, 256 * 1024, 1, Some(4096));
        assert!(n.is_some());
        assert!(n.unwrap().contains("L2-bound"));

        // L3 aggregate spilling over the L3 must be flagged.
        let n = oversized_note(&sys, Level::L3, 8 * 1024 * 1024, 4, Some(16 * 1024 * 1024));
        assert!(n.unwrap().contains("memory traffic"));
    }

    #[test]
    fn clamp_never_goes_below_the_floor() {
        assert_eq!(
            clamp_bytes(0, MIN_BANDWIDTH_BYTES, 4096),
            MIN_BANDWIDTH_BYTES
        );
        assert_eq!(clamp_bytes(9000, MIN_BANDWIDTH_BYTES, 4096), 8192);
        // Rounding down to a 2 MiB page can reach zero; the floor wins, because
        // a cell measured on nothing at all is worse than one on a small buffer.
        assert_eq!(
            clamp_bytes(1_000_000, MIN_BANDWIDTH_BYTES, 2 * 1024 * 1024),
            MIN_BANDWIDTH_BYTES
        );
    }

    #[test]
    fn plan_respects_the_level_filter_and_covers_the_grid() {
        let sys = fixture(32 * 1024, 512 * 1024, 16 * 1024 * 1024, 8, 2);
        let all = plan(&sys, &settings(4)).unwrap();
        assert_eq!(all.len(), 4);
        let only = plan(
            &sys,
            &Settings {
                levels: vec![Level::Memory],
                ..settings(4)
            },
        )
        .unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].level, Level::Memory);
    }
}
