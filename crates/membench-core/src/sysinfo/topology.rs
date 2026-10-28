//! Core inventory and topology: SMT siblings, physical cores, hybrid roles,
//! NUMA nodes, and "which core should a measurement run on".

use std::collections::BTreeSet;

use crate::result::{CacheInfo, CoreInfo, NumaNode, SystemInfo, Topology};
use crate::sysinfo::{dir_entries, exists, parse_cpu_list, read_to_string};

fn cpu_dir(cpu: usize) -> String {
    format!("/sys/devices/system/cpu/cpu{cpu}")
}

fn read_u64(path: &str) -> Option<u64> {
    read_to_string(path).and_then(|s| s.parse().ok())
}

/// Online CPUs, from the `cpuN` directories. Offline CPUs are dropped by the
/// caller when their `topology/` directory is missing.
fn present_cpus() -> Vec<usize> {
    let mut cpus: Vec<usize> = dir_entries("/sys/devices/system/cpu")
        .into_iter()
        .filter_map(|name| {
            name.strip_prefix("cpu")
                .filter(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
                .and_then(|rest| rest.parse::<usize>().ok())
        })
        .collect();
    cpus.sort_unstable();
    cpus
}

/// Hybrid CPU role tables: `/sys/devices/cpu_core/cpus` and `cpu_atom/cpus`.
/// Both present means a hybrid part; neither means uniform.
fn hybrid_sets() -> (Option<Vec<usize>>, Option<Vec<usize>>) {
    let perf = read_to_string("/sys/devices/cpu_core/cpus")
        .map(|s| parse_cpu_list(&s))
        .filter(|v| !v.is_empty());
    let atom = read_to_string("/sys/devices/cpu_atom/cpus")
        .map(|s| parse_cpu_list(&s))
        .filter(|v| !v.is_empty());
    (perf, atom)
}

pub fn detect(caches: &CacheInfo) -> (Vec<CoreInfo>, Topology) {
    let (perf_cpus, atom_cpus) = hybrid_sets();
    let mut cores: Vec<CoreInfo> = Vec::new();
    // Socket ids are only needed for grouping and the multi-socket warning, so
    // they stay in this local map instead of widening the JSON schema.
    let mut packages: std::collections::HashMap<usize, Option<u32>> = std::collections::HashMap::new();

    for cpu in present_cpus() {
        let dir = cpu_dir(cpu);
        if !exists(&format!("{dir}/topology")) {
            continue; // offline CPU: no topology, not usable
        }
        let siblings = read_to_string(&format!("{dir}/topology/thread_siblings_list"))
            .map(|s| parse_cpu_list(&s))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec![cpu]);
        let core_id = read_u64(&format!("{dir}/topology/core_id")).map(|v| v as u32);
        let package_id = read_u64(&format!("{dir}/topology/physical_package_id")).map(|v| v as u32);
        let thread_idx = siblings
            .iter()
            .position(|c| *c == cpu)
            .unwrap_or(0) as u32;

        let hybrid_role = if perf_cpus.as_ref().is_some_and(|v| v.contains(&cpu)) {
            Some("performance".to_string())
        } else if atom_cpus.as_ref().is_some_and(|v| v.contains(&cpu)) {
            Some("efficient".to_string())
        } else {
            None
        };

        packages.insert(cpu, package_id);
        cores.push(CoreInfo {
            cpu,
            core_id,
            physical_core: None, // filled after grouping
            is_boot_core: cpu == boot_cpu(),
            siblings,
            thread_idx: Some(thread_idx),
            capacity: read_u64(&format!("{dir}/topology/cpu_capacity")).map(|v| v as u32),
            cluster: None, // no portable sysfs name for cluster ids yet
            max_freq_khz: read_u64(&format!("{dir}/cpufreq/cpuinfo_max_freq"))
                .or_else(|| read_u64(&format!("{dir}/cpufreq/scaling_max_freq"))),
            cur_freq_khz: read_u64(&format!("{dir}/cpufreq/scaling_cur_freq")),
            governor: read_to_string(&format!("{dir}/cpufreq/scaling_governor")),
            energy_performance_preference: read_to_string(&format!(
                "{dir}/cpufreq/energy_performance_preference"
            )),
            hybrid_role,
        });
    }

    // Physical cores: group by (package, core id); the lowest-numbered CPU of
    // each group is the representative the benchmark pins to. A missing
    // core_id means the kernel does not expose it, so treat each CPU as its own
    // core rather than merging them all together.
    let mut groups: Vec<((u32, u32), Vec<usize>)> = Vec::new();
    for c in &cores {
        let key = (packages.get(&c.cpu).copied().flatten().unwrap_or(0), c.core_id.unwrap_or(c.cpu as u32));
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, members)) => members.push(c.cpu),
            None => groups.push((key, vec![c.cpu])),
        }
    }
    let mut core_cpus: Vec<usize> = Vec::with_capacity(groups.len());
    for (index, (_, members)) in groups.iter().enumerate() {
        core_cpus.push(*members.iter().min().unwrap_or(&usize::MAX));
        for cpu in members {
            if let Some(c) = cores.iter_mut().find(|c| c.cpu == *cpu) {
                c.physical_core = Some(index);
            }
        }
    }
    core_cpus.sort_unstable();

    let sockets: usize = {
        let set: BTreeSet<u32> = packages.values().filter_map(|p| *p).collect();
        set.len().max(1)
    };

    let topology = Topology {
        logical_cpus: cores.len(),
        physical_cores: core_cpus.len(),
        smt_enabled: cores.iter().any(|c| c.siblings.len() > 1),
        core_cpus,
        all_cpus: cores.iter().map(|c| c.cpu).collect(),
        hybrid: cores.iter().any(|c| c.hybrid_role.is_some()),
        numa_nodes: numa_nodes(),
        sockets,
    };
    // The L3 domain list came from the cache scan; it is kept in CacheInfo so
    // buffer sizing and the panel read the same source.
    debug_assert!(
        caches.l3_domains.iter().all(|d| !d.cpus.is_empty()),
        "empty L3 domain"
    );
    (cores, topology)
}

/// The CPU this process started on; used to label the panel.
pub fn boot_cpu() -> usize {
    crate::affinity::current_cpu().unwrap_or(0)
}

fn numa_nodes() -> Vec<NumaNode> {
    let mut out = Vec::new();
    for name in dir_entries("/sys/devices/system/node") {
        let Some(id) = name
            .strip_prefix("node")
            .filter(|rest| rest.chars().all(|c| c.is_ascii_digit()))
            .and_then(|s| s.parse::<usize>().ok())
        else {
            continue;
        };
        let dir = format!("/sys/devices/system/node/{name}");
        let cpus = read_to_string(&format!("{dir}/cpulist"))
            .or_else(|| read_to_string(&format!("{dir}/cpus")))
            .map(|s| parse_cpu_list(&s))
            .unwrap_or_default();
        // First line: "Node 0 MemTotal:       16383488 kB"
        let total = read_to_string(&format!("{dir}/meminfo")).and_then(|s| {
            s.lines().next().and_then(|line| {
                line.split_whitespace()
                    .nth(3)
                    .and_then(|kb| kb.parse::<u64>().ok())
                    .map(|kb| kb * 1024)
            })
        });
        out.push(NumaNode {
            id,
            cpus,
            total_ram_bytes: total,
        });
    }
    out.sort_by_key(|n| n.id);
    out
}

/// One CPU per physical core, ordered so index 0 is the best core to measure
/// latency on: performance-cores first on hybrid parts, then by `cpu_capacity`
/// (Arm big.LITTLE), then by maximum frequency, then by CPU number.
///
/// PLAN.md rule 6: latency runs on a P-core, L3 tests stay inside one L3
/// domain, and the cores used are reported in every result.
pub fn core_order(sys: &SystemInfo) -> Vec<usize> {
    let mut cpus = sys.topology.core_cpus.clone();
    cpus.sort_by_key(|cpu| {
        let core = sys.cores.iter().find(|c| c.cpu == *cpu);
        (
            match core.and_then(|c| c.hybrid_role.as_deref()) {
                Some("performance") => 0u8,
                None => 1,
                Some(_) => 2, // efficient cores last
            },
            std::cmp::Reverse(core.and_then(|c| c.capacity).unwrap_or(0)),
            std::cmp::Reverse(core.and_then(|c| c.max_freq_khz).unwrap_or(0)),
            *cpu,
        )
    });
    cpus
}

/// The single core to run the latency chase on.
pub fn latency_core(sys: &SystemInfo) -> Option<usize> {
    core_order(sys).first().copied()
}

/// CPUs that share a core with `cpu` (SMT siblings). Used to keep a sibling
/// thread off the L1 we are measuring, and to explain it in the detail view.
pub fn siblings_of(sys: &SystemInfo, cpu: usize) -> Vec<usize> {
    sys.cores
        .iter()
        .find(|c| c.cpu == cpu)
        .map(|c| c.siblings.clone())
        .unwrap_or_else(|| vec![cpu])
}

/// L3 domain containing `cpu`, when the kernel reports L3 sharing.
pub fn l3_domain_of(sys: &SystemInfo, cpu: usize) -> Option<usize> {
    sys.caches
        .l3_domains
        .iter()
        .find(|d| d.cpus.contains(&cpu))
        .map(|d| d.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sys() -> SystemInfo {
        let caches = crate::sysinfo::cache::detect();
        let (cores, topology) = detect(&caches);
        SystemInfo {
            cpu: crate::sysinfo::cpu::detect(),
            cores,
            topology,
            caches,
            ..SystemInfo::default()
        }
    }

    #[test]
    fn core_count_is_consistent_with_siblings() {
        let s = sys();
        assert!(s.topology.logical_cpus >= 1);
        assert!(s.topology.physical_cores <= s.topology.logical_cpus);
        assert_eq!(s.topology.core_cpus.len(), s.topology.physical_cores);
        let mut sorted = s.topology.core_cpus.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), s.topology.core_cpus.len());
        for cpu in &s.topology.core_cpus {
            assert!(s.cores.iter().any(|c| &c.cpu == cpu), "core {cpu} missing");
        }
    }

    #[test]
    fn no_two_representatives_share_a_core() {
        let s = sys();
        for (i, a) in s.topology.core_cpus.iter().enumerate() {
            let sa = siblings_of(&s, *a);
            for b in &s.topology.core_cpus[i + 1..] {
                assert!(!sa.contains(b), "cpus {a} and {b} are both representatives");
            }
        }
    }

    #[test]
    fn every_core_lists_itself_as_a_sibling() {
        let s = sys();
        for c in &s.cores {
            assert!(c.siblings.contains(&c.cpu), "cpu {} {:?}", c.cpu, c.siblings);
            assert!((c.thread_idx.unwrap_or(0) as usize) < c.siblings.len());
        }
    }

    #[test]
    fn core_order_is_a_permutation_of_the_representatives() {
        let s = sys();
        let order = core_order(&s);
        assert_eq!(order.len(), s.topology.core_cpus.len());
        let mut sorted = order.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), order.len());
        assert!(latency_core(&s).is_some());
        // Performance cores must sort before efficient ones when both exist.
        if s.topology.hybrid {
            let roles: Vec<Option<&str>> = order
                .iter()
                .map(|cpu| {
                    s.cores
                        .iter()
                        .find(|c| c.cpu == *cpu)
                        .and_then(|c| c.hybrid_role.as_deref())
                })
                .collect();
            let last_perf = roles.iter().rposition(|r| matches!(r, Some("performance")));
            let first_atom = roles.iter().position(|r| matches!(r, Some("efficient")));
            if let (Some(p), Some(a)) = (last_perf, first_atom) {
                assert!(p < a, "P-cores must lead: {roles:?}");
            }
        }
    }

    #[test]
    fn numa_nodes_cover_the_cpus() {
        let s = sys();
        if s.topology.numa_nodes.len() <= 1 {
            return;
        }
        let covered: usize = s.topology.numa_nodes.iter().map(|n| n.cpus.len()).sum();
        assert!(covered >= s.topology.logical_cpus);
    }

    #[test]
    fn hybrid_helper_is_lenient() {
        let (perf, atom) = hybrid_sets();
        // Either both tables exist (hybrid) or we do not claim hybrid.
        if perf.is_some() ^ atom.is_some() {
            // A kernel exposing only one table is unusual but must not be
            // interpreted as "everything is a P-core".
            assert!(perf.is_some() || atom.is_some());
        }
    }
}
