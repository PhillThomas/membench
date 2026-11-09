//! System detection: everything in PLAN.md's detection table.
//!
//! Two rules from the plan govern this module:
//!
//! * Every source is optional. A missing or unreadable file means `None`, which
//!   the UI prints as "Unknown" — never a crash and never a guess.
//! * Detection drives every benchmark parameter (buffer sizes come from the
//!   real cache sizes), so the per-core view is what matters: hybrid CPUs
//!   report different L2 sizes for P and E cores, and `lscpu`-style aggregates
//!   hide that.

pub mod cache;
pub mod cpu;
pub mod dimm;
pub mod governor;
pub mod interference;
pub mod topology;

use std::fs;
use std::fs::File;
use std::io::Read;

use crate::result::{CacheInfo, SystemInfo, Warning};

/// Read a sysfs/procfs file into a String, trimming the trailing newline.
/// `None` for anything that fails, which is the normal case for optional
/// sources.
pub fn read_to_string(path: &str) -> Option<String> {
    let mut s = String::new();
    match File::open(path).and_then(|mut f| f.read_to_string(&mut s)) {
        Ok(_) => {
            let t = s.trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        }
        Err(_) => None,
    }
}

/// Read a binary file (SMBIOS tables, SPD EEPROMs).
pub fn read_bytes(path: &str, cap: usize) -> Option<Vec<u8>> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut v = Vec::new();
    f.by_ref().take(cap as u64).read_to_end(&mut v).ok()?;
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// Does `path` exist (as anything)? Used for the "driver bound?" probes.
pub fn exists(path: &str) -> bool {
    fs::metadata(path).is_ok()
}

/// List the names inside a directory, sorted; empty on failure.
pub fn dir_entries(path: &str) -> Vec<String> {
    let mut v: Vec<String> = match fs::read_dir(path) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => return Vec::new(),
    };
    v.sort();
    v
}

/// Parse the sizes sysfs uses: "32K", "512K", "16M", "1024", "32768".
pub fn parse_size_kb(s: &str) -> Option<u64> {
    let s = s.trim();
    let (digits, mul) = match s.chars().last()? {
        'K' | 'k' => (&s[..s.len() - 1], 1024u64),
        'M' | 'm' => (&s[..s.len() - 1], 1024u64 * 1024),
        'G' | 'g' => (&s[..s.len() - 1], 1024u64 * 1024 * 1024),
        _ => (s, 1u64),
    };
    digits.trim().parse::<u64>().ok().map(|v| v * mul)
}

/// Parse a CPU list such as `0,2,4-7` (the format used by every `*_cpus_list`
/// sysfs attribute).
pub fn parse_cpu_list(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) {
                for c in a..=b {
                    out.push(c);
                }
            }
        } else if let Ok(v) = part.parse::<usize>() {
            out.push(v);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// `cpu_list` rendered back into `0-3,8-11` for the UI.
pub fn format_cpu_list(cpus: &[usize]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < cpus.len() {
        let start = cpus[i];
        let mut end = start;
        while i + 1 < cpus.len() && cpus[i + 1] == end + 1 {
            i += 1;
            end = cpus[i];
        }
        if !out.is_empty() {
            out.push(',');
        }
        if start == end {
            out.push_str(&start.to_string());
        } else {
            out.push_str(&format!("{start}-{end}"));
        }
        i += 1;
    }
    out
}

/// Is `flag` in the CPU feature list the kernel printed? (`flags:` on x86,
/// `Features:` on Arm.) Used for the bits `is_x86_feature_detected!` does not
/// cover, like ERMS and FSRM.
pub fn sysfs_flag_present(flag: &str) -> bool {
    let Some(raw) = read_to_string("/proc/cpuinfo") else {
        return false;
    };
    for line in raw.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if !matches!(key.trim(), "flags" | "Features" | "features") {
            continue;
        }
        if value.split_whitespace().any(|f| f == flag) {
            return true;
        }
    }
    false
}

/// `/sys/kernel/mm/transparent_hugepage/enabled` → "always" | "madvise" |
/// "never" (the bracketed choice), or `None`.
pub fn transparent_hugepages_setting() -> Option<String> {
    let raw = read_to_string("/sys/kernel/mm/transparent_hugepage/enabled")?;
    if let Some(start) = raw.find('[') {
        if let Some(end) = raw[start..].find(']') {
            return Some(raw[start + 1..start + end].to_string());
        }
    }
    raw.split_whitespace().next().map(str::to_string)
}

/// `MemTotal` in bytes.
pub fn total_ram_bytes() -> Option<u64> {
    let raw = read_to_string("/proc/meminfo")?;
    for line in raw.lines() {
        let mut it = line.split_whitespace();
        if it.next()? == "MemTotal:" {
            let kb: u64 = it.next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

/// `MemAvailable`, used only to warn when a memory-sized buffer would swap.
pub fn available_ram_bytes() -> Option<u64> {
    let raw = read_to_string("/proc/meminfo")?;
    for line in raw.lines() {
        let mut it = line.split_whitespace();
        if it.next()? == "MemAvailable:" {
            let kb: u64 = it.next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

/// Is a balloon driver inflating/deflating this VM's memory? (Ballooning
/// mid-run destroys memory-bandwidth numbers, and a VM has no real DRAM to
/// measure anyway.)
fn ballooned() -> bool {
    exists("/sys/devices/system/memory_balloon")
        || dir_entries("/sys/devices/virtual/misc")
            .iter()
            .any(|d| d.contains("balloon"))
}

pub fn kernel_release() -> Option<String> {
    read_to_string("/proc/sys/kernel/osrelease")
}

pub fn hostname() -> Option<String> {
    read_to_string("/proc/sys/kernel/hostname")
}

/// Collect the whole detection table.
pub fn gather() -> SystemInfo {
    let cpu = cpu::detect();
    let caches = cache::detect();
    let (cores, topology) = topology::detect(&caches);
    let mut memory = dimm::detect(&cpu);
    // The panel prints this field, so it has to come from the detection that
    // exists. It was declared and printed but never assigned, which made a
    // single-node machine report "0 node(s)" — a number no Linux box has, and
    // the one place where the panel contradicted the NUMA warning below it.
    memory.numa_node_count = topology.numa_nodes.len();
    let mut warnings = governor::warnings(&cores, &topology);

    if let Some(total) = total_ram_bytes() {
        if ballooned() {
            let _ = total;
            warnings.push(
                Warning::new(
                    "balloon",
                    "a memory balloon device is present: this is a VM, so DRAM bandwidth and \
                     latency describe the hypervisor's host, not this guest",
                )
                .with_hint("run on bare metal for figures that mean anything"),
            );
        }
    }
    if topology.numa_nodes.len() > 1 {
        warnings.push(
            Warning::new(
                "numa",
                format!(
                    "{} NUMA nodes: v1 measures the first-touch local node only and does not \
                     tune per node",
                    topology.numa_nodes.len()
                ),
            )
            .with_hint("compare nodes by pinning with taskset for now"),
        );
    }
    if topology.sockets > 1 {
        warnings.push(Warning::new(
            "sockets",
            format!(
                "{} sockets detected: multi-socket tuning is out of scope for v1",
                topology.sockets
            ),
        ));
    }

    let os = crate::result::OsInfo {
        kernel_release: kernel_release(),
        hostname: hostname(),
        page_size_bytes: page_size(),
        membench_version: crate::time::version().to_string(),
        running_as_root: is_root(),
        mlock_limit_bytes: crate::affinity::memlock_limit_bytes(),
    };

    SystemInfo {
        cpu,
        cores,
        topology,
        caches,
        memory,
        os,
        warnings,
    }
}

/// Detect the machine the way these settings ask for it.
///
/// Everything that reports a machine should come through here rather than
/// calling [`gather`] directly, so that a setting like `--redact` cannot be
/// forgotten by the next output path someone adds. `gather` stays public
/// because detection itself is useful without settings (the self-test, and
/// tests that only care about parsing).
pub fn gather_for(settings: &crate::result::Settings) -> crate::result::SystemInfo {
    let mut sys = gather();
    if settings.redact {
        sys.redact();
    }
    sys
}

pub fn page_size() -> usize {
    // SAFETY: sysconf with a constant name is argument-free in practice.
    let sz = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if sz > 0 {
        sz as usize
    } else {
        4096
    }
}

pub fn is_root() -> bool {
    // SAFETY: argument-free syscall.
    unsafe { libc::geteuid() == 0 }
}

/// Cache size summary line for the system panel, e.g.
/// `L1d 48K  L2 1M  L3 32M x2`.
pub fn cache_summary(caches: &CacheInfo) -> String {
    let mut parts = Vec::new();
    for (level, name) in [(1u8, "L1d"), (2, "L2"), (3, "L3")] {
        if let Some(c) = caches.data_level(level) {
            let size = c
                .size_bytes
                .map(human_bytes)
                .unwrap_or_else(|| "Unknown".to_string());
            let instances = match (level, c.instances) {
                (3, Some(n)) if n > 1 => format!(" x{n}"),
                (_, Some(n)) if n > 1 && level < 3 => String::new(),
                _ => String::new(),
            };
            parts.push(format!("{name} {size}{instances}"));
        }
    }
    if parts.is_empty() {
        "Unknown".to_string()
    } else {
        parts.join("  ")
    }
}

/// 16777216 → "16M", 49152 → "48K", 1536 → "1.5K", 512 → "512B".
///
/// The largest unit that fits always wins; exactness only decides whether the
/// decimal point is printed. It used to be the other way round: the code asked
/// "exact multiple of G? of M? of K?" and took the first yes, so a number that
/// was exact at K — which is every memory total, since the kernel reports them
/// in kilobytes — came out in K. A 32 GiB machine with 31.25 GiB usable printed
/// "32765132K" while this project's own 16 GiB development box, being an exact
/// multiple of a gigabyte, kept printing "16G". The bug could only hide on the
/// machines within reach, which is the usual shape of these.
pub fn human_bytes(bytes: u64) -> String {
    // Tebibytes included because 2 TB is no longer exotic and "2048G" is the
    // sort of line a reader misreads as a bug in something else. Bytes get no
    // exact-match path: every number is a multiple of one, so it would swallow
    // the rounded cases.
    const UNITS: [(&str, u64); 4] = [
        ("T", 1024 * 1024 * 1024 * 1024),
        ("G", 1024 * 1024 * 1024),
        ("M", 1024 * 1024),
        ("K", 1024),
    ];
    for (suffix, size) in UNITS {
        if bytes >= size {
            return if bytes % size == 0 {
                format!("{}{}", bytes / size, suffix)
            } else {
                format!("{:.1}{suffix}", bytes as f64 / size as f64)
            };
        }
    }
    format!("{bytes}B")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_lists_round_trip() {
        assert_eq!(parse_cpu_list("0,2,4-6"), vec![0, 2, 4, 5, 6]);
        assert_eq!(parse_cpu_list("  3 "), vec![3]);
        assert_eq!(parse_cpu_list("1-1"), vec![1]);
        assert_eq!(parse_cpu_list("nonsense"), Vec::<usize>::new());
        assert_eq!(format_cpu_list(&[0, 1, 2, 5, 7, 8, 9]), "0-2,5,7-9");
        assert_eq!(format_cpu_list(&[3]), "3");
    }

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size_kb("32K"), Some(32 * 1024));
        assert_eq!(parse_size_kb("512K"), Some(512 * 1024));
        assert_eq!(parse_size_kb("16M"), Some(16 * 1024 * 1024));
        assert_eq!(parse_size_kb("4096"), Some(4096));
        assert_eq!(parse_size_kb(""), None);
        assert_eq!(parse_size_kb("abc"), None);
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_bytes(16 * 1024 * 1024), "16M");
        assert_eq!(human_bytes(48 * 1024), "48K");
        assert_eq!(human_bytes(1024), "1K");
        assert_eq!(human_bytes(1536), "1.5K");
        // The reporter's 32 GiB machine, whose /proc/meminfo says 32765132 kB:
        // an exact multiple of 1024 and of nothing larger, so the old
        // exactness-first ordering printed a 32 GiB box as "32765132K".
        assert_eq!(human_bytes(32_765_132 * 1024), "31.2G");
        // And a machine large enough to have needed a unit the list lacked.
        assert_eq!(human_bytes(2 * 1024 * 1024 * 1024 * 1024), "2T");
    }

    #[test]
    fn gather_never_panics_and_fills_the_basics() {
        let sys = gather();
        // The one thing that must always be true on Linux: we have CPUs.
        assert!(sys.topology.logical_cpus >= 1, "{:?}", sys.topology);
        assert!(!sys.cores.is_empty());
        assert!(!sys.os.membench_version.is_empty());
        // PLAN.md: missing sources give Unknown, never a panic — so gather()
        // returning at all is the assertion. Spot-check the panel builder too.
        let _ = cache_summary(&sys.caches);
        // ...but a field the panel *prints* must be filled by the detection that
        // exists. `numa_node_count` was declared, printed, and never assigned, so
        // a one-node machine printed "0 node(s)" — a state Linux does not have —
        // while the warning path underneath it counted nodes correctly.
        // Asserted as a wiring, not as ">= 1", because a sandbox without /sys is
        // allowed to detect nothing at all.
        assert_eq!(
            sys.memory.numa_node_count,
            sys.topology.numa_nodes.len(),
            "numa_node_count is not wired to the topology it prints"
        );
    }

    #[test]
    fn thp_setting_is_a_known_word() {
        if let Some(s) = transparent_hugepages_setting() {
            assert!(
                matches!(s.as_str(), "always" | "madvise" | "never"),
                "unexpected THP setting {s:?}"
            );
        }
    }
}
