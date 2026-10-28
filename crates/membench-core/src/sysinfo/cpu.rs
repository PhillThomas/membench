//! CPU identity and the feature list that drives kernel dispatch.

use crate::kernels;
use crate::result::{CpuInfo, Isa};
use crate::sysinfo::{read_to_string, sysfs_flag_present};

/// CPU model, vendor, executable ISA paths, relevant feature flags.
pub fn detect() -> CpuInfo {
    let (vendor, model) = identity();
    let features = feature_list();
    CpuInfo {
        vendor: vendor.clone(),
        model: model.clone(),
        vendor_flavour: flavour(vendor.as_deref(), model.as_deref()),
        architecture: std::env::consts::ARCH.to_string(),
        features,
        isa_paths: kernels::available(),
        cpuid_leaf_max: cpuid_leaf_max(),
    }
}

/// (vendor, model). `raw-cpuid` first (it survives a masked `/proc/cpuinfo`),
/// `/proc/cpuinfo` as the fallback (PLAN.md detection table).
fn identity() -> (Option<String>, Option<String>) {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        let cpuid = raw_cpuid::CpuId::new();
        let vendor = cpuid
            .get_vendor_info()
            .map(|v| v.as_str().trim_end_matches('\0').trim().to_string())
            .filter(|s| !s.is_empty());
        let model = cpuid
            .get_processor_brand_string()
            .map(|b| b.as_str().trim().trim_end_matches('\0').to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| cpuinfo_field("model name"));
        return (vendor, model);
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    {
        // Arm / others: /proc/cpuinfo carries the model ("Hardware", "model
        // name" or "CPU part" depending on the kernel).
        let model = cpuinfo_field("model name")
            .or_else(|| cpuinfo_field("Hardware"))
            .or_else(|| cpuinfo_field("Model"));
        let vendor = cpuinfo_field("vendor_id").or_else(|| cpuinfo_field("CPU implementer"));
        (vendor, model)
    }
}

/// The highest CPUID leaf the CPU answers, straight from the kernel's own
/// reading of it. Only useful as a sanity check in the panel.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn cpuid_leaf_max() -> Option<u32> {
    cpuinfo_field("cpuid level")?.parse().ok()
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn cpuid_leaf_max() -> Option<u32> {
    None
}

fn cpuinfo_field(key: &str) -> Option<String> {
    let raw = read_to_string("/proc/cpuinfo")?;
    for line in raw.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim() == key {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// Feature flags worth showing: the ones that select a kernel, plus the string
/// operations that decide whether the `rep movsb` detail cells are interesting.
fn feature_list() -> Vec<String> {
    let mut out = Vec::new();
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        // Written out on purpose: `is_x86_feature_detected!` matches on a
        // literal token, so passing the name through a macro does not work.
        if std::arch::is_x86_feature_detected!("sse2") {
            out.push("SSE2".to_string());
        }
        if std::arch::is_x86_feature_detected!("sse4.2") {
            out.push("SSE4.2".to_string());
        }
        if std::arch::is_x86_feature_detected!("avx") {
            out.push("AVX".to_string());
        }
        if std::arch::is_x86_feature_detected!("fma") {
            out.push("FMA".to_string());
        }
        if std::arch::is_x86_feature_detected!("avx2") {
            out.push("AVX2".to_string());
        }
        if std::arch::is_x86_feature_detected!("avx512f") {
            out.push("AVX512F".to_string());
        }
        if std::arch::is_x86_feature_detected!("avx512bw") {
            out.push("AVX512BW".to_string());
        }
        if std::arch::is_x86_feature_detected!("bmi2") {
            out.push("BMI2".to_string());
        }
        if crate::kernels::tsc_available() {
            out.push("TSC".to_string());
        }
        // Not in is_x86_feature_detected: read them from the kernel's flag list.
        if sysfs_flag_present("erms") {
            out.push("ERMS".to_string());
        }
        if sysfs_flag_present("fsrm") {
            out.push("FSRM".to_string());
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        out.push("NEON".to_string());
        for (name, flag) in [
            ("SVE", "sve"),
            ("SVE2", "sve2"),
            ("PAuth", "paca"),
            ("LSE atomics", "atomics"),
        ] {
            if sysfs_flag_present(flag) {
                out.push(name.to_string());
            }
        }
    }
    out
}

fn flavour(vendor: Option<&str>, model: Option<&str>) -> Option<String> {
    let hay = format!(
        "{} {}",
        vendor.unwrap_or(""),
        model.unwrap_or("")
    )
    .to_ascii_uppercase();
    for (needle, name) in [
        ("AMD", "AMD"),
        ("INTEL", "Intel"),
        ("HYGON", "Hygon"),
        ("VIA", "VIA"),
        ("ARM", "Arm"),
        ("CAVIUM", "Arm"),
        ("NVIDIA", "Arm"),
        ("APPLE", "Apple"),
        ("RISCV", "RISC-V"),
    ] {
        if hay.contains(needle) {
            return Some(name.to_string());
        }
    }
    None
}

/// Which ISA the panel should name as "in use" for a fresh run.
pub fn selected_isa() -> Isa {
    kernels::best()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_features_are_sane() {
        let cpu = detect();
        assert_eq!(cpu.architecture, std::env::consts::ARCH);
        assert!(cpu.isa_paths.contains(&Isa::Scalar));
        assert!(cpu.isa_paths.iter().all(|isa| kernels::detect(*isa)));
        // Every advertised feature must be one we probed, not a stray string.
        for f in &cpu.features {
            assert!(!f.is_empty());
        }
        #[cfg(target_arch = "x86_64")]
        {
            // x86-64 implies SSE2 by definition.
            assert!(cpu.features.contains(&"SSE2".to_string()), "{:?}", cpu.features);
        }
    }

    #[test]
    fn vendor_flavour_classifies_known_strings() {
        assert_eq!(
            flavour(Some("GenuineIntel"), None).as_deref(),
            Some("Intel")
        );
        assert_eq!(
            flavour(Some("AuthenticAMD"), None).as_deref(),
            Some("AMD")
        );
        assert_eq!(
            flavour(None, Some("AMD Ryzen 5 PRO 5650G with Radeon Graphics")).as_deref(),
            Some("AMD")
        );
        assert_eq!(flavour(Some("something else"), None), None);
    }
}
