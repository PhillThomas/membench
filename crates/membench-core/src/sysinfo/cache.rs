//! Cache topology from `/sys/devices/system/cpu/cpu*/cache/index*`.
//!
//! Why not `lscpu -C` or CPUID leaf 4 directly: the benchmark needs the
//! **per-core** view, because buffer sizing depends on what one core can keep
//! resident, and because a hybrid CPU reports different L2 sizes for P and E
//! cores — an aggregate hides that. sysfs gives per-core entries plus
//! `shared_cpu_list`, which is what identifies an L3 domain (an AMD CCD, an
//! Intel LLC slice).

use std::collections::HashMap;

use crate::result::{CacheInfo, CacheKind, CacheLevel, L3Domain};
use crate::sysinfo::{dir_entries, parse_cpu_list, parse_size_kb, read_to_string};

/// One `indexN` directory.
fn read_index_dir(path: &str) -> Option<CacheLevel> {
    let level = read_to_string(&format!("{path}/level"))?
        .parse::<u8>()
        .ok()?;
    let kind = match read_to_string(&format!("{path}/type"))?
        .to_ascii_lowercase()
        .as_str()
    {
        "data" => CacheKind::Data,
        "instruction" => CacheKind::Instruction,
        "unified" => CacheKind::Unified,
        _ => return None,
    };
    Some(CacheLevel {
        level,
        kind,
        size_bytes: read_to_string(&format!("{path}/size")).and_then(|s| parse_size_kb(&s)),
        line_bytes: read_to_string(&format!("{path}/coherency_line_size"))
            .and_then(|s| s.parse().ok()),
        physical_line_bytes: read_to_string(&format!("{path}/physical_line_size"))
            .and_then(|s| s.parse().ok()),
        ways: read_to_string(&format!("{path}/ways_of_associativity")).and_then(|s| s.parse().ok()),
        sets: read_to_string(&format!("{path}/number_of_sets")).and_then(|s| s.parse().ok()),
        shared_cpus: read_to_string(&format!("{path}/shared_cpu_list"))
            .map(|s| parse_cpu_list(&s))
            .unwrap_or_default(),
        instances: None,
        total_bytes: None,
        sizes_vary_between_cores: false,
    })
}

fn list_cache_dirs(cpu: &str) -> Vec<String> {
    dir_entries(cpu)
        .into_iter()
        .filter(|name| name.starts_with("index") && name[5..].chars().all(|c| c.is_ascii_digit()))
        .map(|name| format!("{cpu}/{name}"))
        .collect()
}

/// All CPUs that have a cache directory.
fn cpus_with_cache() -> Vec<usize> {
    let mut cpus: Vec<usize> = dir_entries("/sys/devices/system/cpu")
        .into_iter()
        .filter_map(|name| {
            name.strip_prefix("cpu")
                .and_then(|rest| rest.parse::<usize>().ok())
        })
        .collect();
    cpus.sort_unstable();
    cpus
}

pub fn detect() -> CacheInfo {
    let cpus = cpus_with_cache();
    // (level, kind) -> per-cpu entries
    let mut seen: HashMap<(u8, u8), Vec<(usize, CacheLevel)>> = HashMap::new();

    for cpu in &cpus {
        let base = format!("/sys/devices/system/cpu/cpu{cpu}/cache");
        for dir in list_cache_dirs(&base) {
            if let Some(mut entry) = read_index_dir(&dir) {
                if entry.shared_cpus.is_empty() {
                    // Some kernels hide shared_cpu_list; assume the cache is
                    // private to this logical CPU rather than inventing a set.
                    entry.shared_cpus = vec![*cpu];
                }
                let key = (entry.level, kind_key(&entry.kind));
                seen.entry(key).or_default().push((*cpu, entry));
            }
        }
    }

    let mut levels: Vec<CacheLevel> = Vec::new();
    for ((level, _), entries) in &mut seen {
        let Some((_, first)) = entries.first() else {
            continue;
        };
        let mut merged = first.clone();
        merged.level = *level;

        let sizes: Vec<u64> = entries.iter().filter_map(|(_, e)| e.size_bytes).collect();
        merged.sizes_vary_between_cores = sizes
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            > 1;
        // The sizing code must use the *smallest* instance of a level, or a
        // buffer that fits an E-core's L2 would not fit a bigger one (hybrid).
        merged.size_bytes = if sizes.is_empty() {
            None
        } else if merged.sizes_vary_between_cores {
            sizes.iter().min().copied()
        } else {
            Some(sizes[0])
        };

        // Instances = distinct shared-cpu sets, which is how you count L1d
        // copies (one per core) or L3 slices (one per CCD).
        let mut domains: Vec<Vec<usize>> = Vec::new();
        for (_, e) in entries.iter() {
            if !domains.iter().any(|d| d == &e.shared_cpus) {
                domains.push(e.shared_cpus.clone());
            }
        }
        merged.instances = Some(domains.len());
        merged.total_bytes = merged
            .size_bytes
            .map(|sz| sz.saturating_mul(domains.len() as u64));
        // Prefer the widest reported line info from any instance.
        merged.line_bytes = entries.iter().filter_map(|(_, e)| e.line_bytes).max();
        merged.physical_line_bytes = entries
            .iter()
            .filter_map(|(_, e)| e.physical_line_bytes)
            .max();
        levels.push(merged);
    }

    levels.sort_by_key(|c| (c.level, kind_key(&c.kind)));

    let line_bytes = levels
        .iter()
        .find(|c| c.level == 1 && c.kind == CacheKind::Data)
        .and_then(|c| c.line_bytes)
        .or_else(|| {
            levels
                .iter()
                .find(|c| c.level == 2)
                .and_then(|c| c.line_bytes)
        })
        .or_else(|| levels.iter().find_map(|c| c.line_bytes));

    // L3 domains: one entry per distinct sharing set.
    let l3_domains: Vec<L3Domain> = levels
        .iter()
        .filter(|c| c.level == 3)
        .flat_map(|c| {
            let mut per_domain: Vec<Vec<usize>> = Vec::new();
            // Recompute domains from the merged entry's own data: merged only
            // keeps one set, so recover them from the raw entries.
            let key = (3u8, kind_key(&c.kind));
            if let Some(entries) = seen.get(&key) {
                for (_, e) in entries {
                    if !per_domain.iter().any(|d: &Vec<usize>| d == &e.shared_cpus) {
                        per_domain.push(e.shared_cpus.clone());
                    }
                }
            }
            per_domain
                .into_iter()
                .enumerate()
                .map(move |(id, cpus)| L3Domain {
                    id,
                    cpus,
                    size_bytes: c.size_bytes,
                })
        })
        .collect();

    CacheInfo {
        levels,
        line_bytes,
        l3_domains,
    }
}

/// Stable sort key for a cache kind.
fn kind_key(kind: &CacheKind) -> u8 {
    match kind {
        CacheKind::Instruction => 0,
        CacheKind::Data => 1,
        CacheKind::Unified => 2,
    }
}

/// Human summary of one level, used by the detail view.
pub fn describe(level: &CacheLevel) -> String {
    let size = level
        .size_bytes
        .map(crate::sysinfo::human_bytes)
        .unwrap_or_else(|| "Unknown".to_string());
    let line = level
        .line_bytes
        .map(|l| format!("{l} B line"))
        .unwrap_or_else(|| "line Unknown".to_string());
    let ways = level
        .ways
        .map(|w| format!("{w}-way"))
        .unwrap_or_else(|| "ways Unknown".to_string());
    let sets = level
        .sets
        .map(|s| s.to_string())
        .unwrap_or_else(|| "Unknown".to_string());
    let inst = level
        .instances
        .map(|i| i.to_string())
        .unwrap_or_else(|| "Unknown".to_string());
    format!(
        "{size}, {ways}, {sets} sets, {line}, {inst} instance(s), shared by {}",
        if level.shared_cpus.is_empty() {
            "Unknown".to_string()
        } else {
            crate::sysinfo::format_cpu_list(&level.shared_cpus)
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_returns_unknown_not_panic_when_sysfs_is_there() {
        let info = detect();
        // On Linux with the standard sysfs there is at least an L1. On an
        // exotic kernel the correct behaviour is an empty list, not a panic.
        if info.levels.is_empty() {
            assert!(info.line_bytes.is_none(), "no levels but a line size?");
            return;
        }
        assert!(info.levels.iter().any(|l| l.level == 1));
        // Instance counts must be consistent with the aggregate.
        for l in &info.levels {
            if let (Some(sz), Some(inst), Some(total)) = (l.size_bytes, l.instances, l.total_bytes)
            {
                assert_eq!(total, sz * inst as u64);
            }
        }
        // Line size, when present, is a sane power of two.
        if let Some(line) = info.line_bytes {
            assert!(
                line.is_power_of_two() && (32..=256).contains(&line),
                "{line}"
            );
        }
    }

    #[test]
    fn l3_domains_partition_the_cpus() {
        let info = detect();
        if info.l3_domains.is_empty() {
            return; // no L3 on this CPU (or no sysfs)
        }
        let mut all: Vec<usize> = info
            .l3_domains
            .iter()
            .flat_map(|d| d.cpus.iter().copied())
            .collect();
        let count = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(
            all.len(),
            count,
            "L3 domains overlap: {:?}",
            info.l3_domains
        );
        // Never a domain with zero cores.
        assert!(info.l3_domains.iter().all(|d| !d.cpus.is_empty()));
    }

    #[test]
    fn describe_handles_missing_data() {
        let l = CacheLevel {
            level: 2,
            kind: CacheKind::Unified,
            size_bytes: None,
            line_bytes: None,
            physical_line_bytes: None,
            ways: None,
            sets: None,
            shared_cpus: vec![],
            instances: None,
            total_bytes: None,
            sizes_vary_between_cores: false,
        };
        let s = describe(&l);
        assert!(s.contains("Unknown"), "{s}");
        assert!(!s.contains("null"), "{s}");
    }
}
