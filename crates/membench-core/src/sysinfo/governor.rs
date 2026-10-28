//! Scaling-governor and energy-preference warnings (PLAN.md rule 4: "warn on
//! `powersave` / `conservative` / EPP-power").
//!
//! The nuance the warning has to carry: with the `intel_pstate` and `amd-pstate`
//! drivers, `powersave` is the *only* governor and it still boosts, so the
//! message must not claim the machine is throttled — only that the policy is not
//! the one a benchmark wants, and that a frequency change **during** a run is
//! what ruins the numbers.

use crate::result::{CoreInfo, Topology, Warning};

/// Governors that never change frequency under load.
const SAFE_GOVERNORS: [&str; 2] = ["performance", "none"];

/// EPP / energy-bias values that ask the CPU to save power.
const POWER_EPP: [&str; 3] = ["power", "balance_power", "low_power"];

pub fn warnings(cores: &[CoreInfo], _topology: &Topology) -> Vec<Warning> {
    let mut out = Vec::new();

    let governors: Vec<(&str, Vec<usize>)> = group_by(cores, |c| c.governor.as_deref());
    if governors.is_empty() {
        // No cpufreq interface at all: some ARM boards, some VMs, containers
        // with /sys mounted read-only. Not a governor risk -- there is nothing
        // to scale -- but the run has no frequency telemetry either, so the
        // measured clock is the only clock figure in the report. Say it once.
        out.push(
            Warning::new(
                "clock",
                "no cpufreq interface: the panel cannot show a governor or a requested frequency",
            )
            .with_hint(
                "membench times a dependent add chain to measure the effective clock instead \
                 of trusting a sysfs number",
            ),
        );
        return out;
    }

    let risky: Vec<(&str, Vec<usize>)> = governors
        .iter()
        .filter(|(g, _)| !SAFE_GOVERNORS.contains(g))
        .map(|(g, cpus)| (*g, cpus.clone()))
        .collect();

    if !risky.is_empty() {
        let list = risky
            .iter()
            .map(|(g, cpus)| format!("{g} [{}]", crate::sysinfo::format_cpu_list(cpus)))
            .collect::<Vec<_>>()
            .join(", ");
        let mut w = Warning::new(
            "governor",
            format!(
                "CPU frequency scaling is active ({list}); a frequency change mid-run moves \
                     the numbers more than any code change would"
            ),
        );
        if risky.iter().any(|(g, _)| *g == "powersave") {
            w = w.with_hint(
                "with intel_pstate/amd-pstate 'powersave' still boosts, so this may be harmless; \
                 for steady numbers: cpupower frequency-set -g performance",
            );
        } else {
            w = w.with_hint("for steady numbers: cpupower frequency-set -g performance");
        }
        out.push(w);
    }

    let epp: Vec<(&'static str, Vec<usize>)> = {
        let mut acc: Vec<(&'static str, Vec<usize>)> = Vec::new();
        for c in cores {
            let Some(raw) = c.energy_performance_preference.as_deref() else {
                continue;
            };
            // Lower-cased values are leaked for the lifetime of the process:
            // a handful of strings per run, and it keeps the grouping simple.
            let key: &'static str = Box::leak(raw.to_ascii_lowercase().into_boxed_str());
            match acc.iter_mut().find(|(name, _)| *name == key) {
                Some((_, cpus)) => cpus.push(c.cpu),
                None => acc.push((key, vec![c.cpu])),
            }
        }
        acc.sort_by_key(|(name, _)| *name);
        acc
    };
    let power_epp: Vec<(&str, Vec<usize>)> = epp
        .iter()
        .filter(|(v, _)| POWER_EPP.contains(v))
        .map(|(v, cpus)| (*v, cpus.clone()))
        .collect();
    if !power_epp.is_empty() {
        let list = power_epp
            .iter()
            .map(|(v, cpus)| format!("{v} [{}]", crate::sysinfo::format_cpu_list(cpus)))
            .collect::<Vec<_>>()
            .join(", ");
        out.push(
            Warning::new(
                "epp",
                format!("energy_performance_preference asks for power saving ({list}), which caps \
                         boost and skews single-core latency"),
            )
            .with_hint("echo performance | sudo tee /sys/devices/system/cpu/cpu*/cpufreq/energy_performance_preference"),
        );
    }

    // No cpufreq directory anywhere, but CPUs exist: the clock cannot be
    // cross-checked against sysfs, so say which measurement we are using.
    if cores.iter().all(|c| c.max_freq_khz.is_none()) {
        out.push(Warning::new(
            "clock",
            "no cpufreq information exposed: reported frequencies come from the measured \
             add-chain clock only",
        ));
    }

    out
}

/// Group CPU numbers by a string-valued attribute, sorted by group name.
fn group_by<F>(cores: &[CoreInfo], mut key: F) -> Vec<(&str, Vec<usize>)>
where
    F: FnMut(&CoreInfo) -> Option<&str>,
{
    let mut out: Vec<(&str, Vec<usize>)> = Vec::new();
    for c in cores {
        let Some(k) = key(c) else { continue };
        match out.iter_mut().find(|(name, _)| *name == k) {
            Some((_, cpus)) => cpus.push(c.cpu),
            None => out.push((k, vec![c.cpu])),
        }
    }
    out.sort_by_key(|(name, _)| *name);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core(cpu: usize, governor: Option<&str>, epp: Option<&str>) -> CoreInfo {
        CoreInfo {
            cpu,
            governor: governor.map(str::to_string),
            energy_performance_preference: epp.map(str::to_string),
            ..Default::default()
        }
    }

    fn topo() -> Topology {
        Topology::default()
    }

    #[test]
    fn powersave_governor_warns() {
        let cores = vec![
            core(0, Some("powersave"), None),
            core(1, Some("powersave"), None),
        ];
        let w = warnings(&cores, &topo());
        assert!(w.iter().any(|w| w.code == "governor"), "{w:?}");
        // The hint must not overclaim: powersave is named, and the nuance too.
        let g = w.iter().find(|w| w.code == "governor").unwrap();
        assert!(g.message.contains("powersave"));
        assert!(g.hint.as_deref().unwrap_or_default().contains("boost"));
    }

    #[test]
    fn performance_governor_is_silent() {
        let cores = vec![core(0, Some("performance"), Some("performance"))];
        let w = warnings(&cores, &topo());
        assert!(!w.iter().any(|w| w.code == "governor"), "{w:?}");
        assert!(!w.iter().any(|w| w.code == "epp"), "{w:?}");
    }

    #[test]
    fn conservative_and_epp_power_both_warn() {
        let cores = vec![core(0, Some("conservative"), Some("power"))];
        let w = warnings(&cores, &topo());
        assert!(w.iter().any(|w| w.code == "governor"));
        assert!(w.iter().any(|w| w.code == "epp"));
    }

    #[test]
    fn missing_cpufreq_is_reported_once_not_as_governor_risk() {
        let cores = vec![core(0, None, None), core(1, None, None)];
        let w = warnings(&cores, &topo());
        assert!(!w.iter().any(|w| w.code == "governor"), "{w:?}");
        assert!(w.iter().any(|w| w.code == "clock"), "{w:?}");
    }

    #[test]
    fn hybrid_mixed_governors_are_both_named() {
        let cores = vec![
            core(0, Some("performance"), None),
            core(4, Some("powersave"), None),
        ];
        let w = warnings(&cores, &topo());
        let g = w.iter().find(|w| w.code == "governor").expect("warn");
        assert!(g.message.contains("powersave"), "{}", g.message);
        assert!(!g.message.contains("performance ["), "{}", g.message);
    }

    #[test]
    fn grouping_sorts_and_collects_cpus() {
        let cores = vec![
            core(3, Some("powersave"), None),
            core(1, Some("performance"), None),
            core(0, Some("powersave"), None),
        ];
        let g = group_by(&cores, |c| c.governor.as_deref());
        assert_eq!(g[0].0, "performance");
        assert_eq!(g[1], ("powersave", vec![3, 0]));
    }
}
