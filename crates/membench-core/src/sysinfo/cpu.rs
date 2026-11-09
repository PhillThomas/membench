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
        (vendor, model)
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    {
        // Arm / others. AArch64 Linux prints no model name at all: an ordinary
        // server cpuinfo has `CPU implementer`, `CPU part`, `CPU variant` and
        // `CPU revision` and nothing that names the core, so the registers have
        // to be decoded rather than read. The board strings some kernels do
        // print (`Hardware`, `Model`) still win when present: they are the
        // machine's own answer.
        let midr = midr();
        let model = cpuinfo_field("model name")
            .or_else(|| cpuinfo_field("Hardware"))
            .or_else(|| cpuinfo_field("Model"))
            .or_else(|| midr.as_ref().map(Midr::model));
        let vendor = cpuinfo_field("vendor_id")
            .or_else(|| midr.as_ref().and_then(|m| m.vendor().map(str::to_string)))
            .or_else(|| {
                midr.as_ref()
                    .map(|m| format!("implementer {:#04x}", m.implementer))
            });
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
    let hay = format!("{} {}", vendor.unwrap_or(""), model.unwrap_or("")).to_ascii_uppercase();
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
            assert!(
                cpu.features.contains(&"SSE2".to_string()),
                "{:?}",
                cpu.features
            );
        }
    }

    #[test]
    fn vendor_flavour_classifies_known_strings() {
        assert_eq!(
            flavour(Some("GenuineIntel"), None).as_deref(),
            Some("Intel")
        );
        assert_eq!(flavour(Some("AuthenticAMD"), None).as_deref(), Some("AMD"));
        assert_eq!(
            flavour(None, Some("AMD Ryzen 5 PRO 5650G with Radeon Graphics")).as_deref(),
            Some("AMD")
        );
        assert_eq!(flavour(Some("something else"), None), None);
    }
}

/// The MIDR registers, decoded.
///
/// `midr_el1` is exported by the arm64 kernel at
/// `/sys/devices/system/cpu/cpuN/regs/identification/`, which ACPI guests have
/// too, so this works on a cloud VM with no device tree. Fields: implementer
/// [31:24], variant [23:20], architecture [19:16], part [15:4], revision [3:0].
///
/// The core names are the public ones from the ARM Trustzone/TRM documents.
/// Identifiers and names were cross-checked against util-linux's
/// `sys-utils/lscpu-arm.c` (GPL-2.0-or-later) so that `membench` and `lscpu`
/// agree on the same machine; nothing else was taken from it, and a part that
/// is not listed here prints as its raw registers. That is deliberate: an
/// unrecognised core costs a less readable string, never a wrong one, which is
/// the same rule the rest of the detection code follows.
#[cfg(any(test, not(any(target_arch = "x86", target_arch = "x86_64"))))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Midr {
    implementer: u8,
    variant: u8,
    part: u16,
    revision: u8,
}

#[cfg(any(test, not(any(target_arch = "x86", target_arch = "x86_64"))))]
const IMPLEMENTERS: &[(u8, &str)] = &[
    (0x41, "ARM"),
    (0x42, "Broadcom"),
    (0x43, "Cavium"),
    (0x46, "Fujitsu"),
    (0x48, "HiSilicon"),
    (0x4e, "NVIDIA"),
    (0x50, "APM"),
    (0x51, "Qualcomm"),
    (0x53, "Samsung"),
    (0x56, "Marvell"),
    (0x61, "Apple"),
    (0x6d, "Microsoft"),
    (0x70, "Phytium"),
    (0xc0, "Ampere"),
];

#[cfg(any(test, not(any(target_arch = "x86", target_arch = "x86_64"))))]
const CORES: &[(u8, u16, &str)] = &[
    (0x41, 0xd01, "Cortex-A32"),
    (0x41, 0xd02, "Cortex-A34"),
    (0x41, 0xd03, "Cortex-A53"),
    (0x41, 0xd04, "Cortex-A35"),
    (0x41, 0xd05, "Cortex-A55"),
    (0x41, 0xd06, "Cortex-A65"),
    (0x41, 0xd07, "Cortex-A57"),
    (0x41, 0xd08, "Cortex-A72"),
    (0x41, 0xd09, "Cortex-A73"),
    (0x41, 0xd0a, "Cortex-A75"),
    (0x41, 0xd0b, "Cortex-A76"),
    (0x41, 0xd0c, "Neoverse-N1"),
    (0x41, 0xd0d, "Cortex-A77"),
    (0x41, 0xd0e, "Cortex-A76AE"),
    (0x41, 0xd40, "Neoverse-V1"),
    (0x41, 0xd41, "Cortex-A78"),
    (0x41, 0xd42, "Cortex-A78AE"),
    (0x41, 0xd44, "Cortex-X1"),
    (0x41, 0xd46, "Cortex-A510"),
    (0x41, 0xd47, "Cortex-A710"),
    (0x41, 0xd48, "Cortex-X2"),
    (0x41, 0xd49, "Neoverse-N2"),
    (0x41, 0xd4a, "Neoverse-E1"),
    (0x41, 0xd4b, "Cortex-A78C"),
    (0x41, 0xd4c, "Cortex-X1C"),
    (0x41, 0xd4d, "Cortex-A715"),
    (0x41, 0xd4e, "Cortex-X3"),
    (0x41, 0xd4f, "Neoverse-V2"),
    (0x41, 0xd80, "Cortex-A520"),
    (0x41, 0xd81, "Cortex-A720"),
    (0x41, 0xd82, "Cortex-X4"),
    (0x41, 0xd83, "Neoverse-V3AE"),
    (0x41, 0xd84, "Neoverse-V3"),
    (0x41, 0xd85, "Cortex-X925"),
    (0x41, 0xd87, "Cortex-A725"),
    (0x41, 0xd8e, "Neoverse-N3"),
    (0x42, 0x100, "Brahma-B53"),
    (0x42, 0x516, "ThunderX2"),
    (0x43, 0x0a0, "ThunderX"),
    (0x43, 0x0af, "ThunderX2-99xx"),
    (0x43, 0x0b0, "OcteonTX2"),
    (0x43, 0x0b8, "ThunderX3-T110"),
    (0x46, 0x001, "A64FX"),
    (0x48, 0xd01, "Kunpeng-920"),
    (0x4e, 0x010, "Olympus"),
    (0x50, 0x000, "X-Gene"),
    (0x51, 0x001, "Oryon"),
    (0x51, 0xc00, "Falkor"),
    (0x51, 0xc01, "Saphira"),
    (0x6d, 0xd49, "Azure-Cobalt-100"),
    (0x70, 0x660, "FTC660"),
    (0xc0, 0xac3, "Ampere-1"),
    (0xc0, 0xac4, "Ampere-1a"),
];

#[cfg(any(test, not(any(target_arch = "x86", target_arch = "x86_64"))))]
fn hex_u64(s: &str) -> Option<u64> {
    let s = s.trim();
    let digits = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u64::from_str_radix(digits, 16).ok()
}

#[cfg(any(test, not(any(target_arch = "x86", target_arch = "x86_64"))))]
impl Midr {
    fn from_u64(v: u64) -> Self {
        Midr {
            implementer: (v >> 24) as u8,
            variant: ((v >> 20) & 0xf) as u8,
            part: ((v >> 4) & 0xfff) as u16,
            revision: (v & 0xf) as u8,
        }
    }

    /// Stepping, in the form the ARM documentation and `lscpu` both use.
    fn stepping(&self) -> String {
        format!("r{}p{}", self.variant, self.revision)
    }

    fn name(&self) -> Option<&'static str> {
        CORES
            .iter()
            .find(|(i, p, _)| *i == self.implementer && *p == self.part)
            .map(|(_, _, n)| *n)
    }

    fn vendor(&self) -> Option<&'static str> {
        IMPLEMENTERS
            .iter()
            .find(|(i, _)| *i == self.implementer)
            .map(|(_, n)| *n)
    }

    /// The core, named if we know it and described by its registers if we do
    /// not. The revision is always carried: two steppings of one core can
    /// differ in the errata that matter to a memory benchmark.
    fn model(&self) -> String {
        match self.name() {
            Some(name) => format!("{name} ({})", self.stepping()),
            None => format!(
                "AArch64 core (implementer {:#04x}, part {:#05x}, {})",
                self.implementer,
                self.part,
                self.stepping()
            ),
        }
    }
}

/// The identification registers of cpu0, from sysfs when the kernel exports
/// them and from `/proc/cpuinfo` otherwise. The two carry the same numbers:
/// `CPU variant` is hex, `CPU revision` decimal.
///
/// Not compiled into the x86 build at all, unlike the decoding above which is
/// tested on every architecture: this is the reader, it has no caller there,
/// and a `#[cfg]` on the definition is a smaller claim than an `#[allow]`.
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn midr() -> Option<Midr> {
    if let Some(raw) = read_to_string("/sys/devices/system/cpu/cpu0/regs/identification/midr_el1") {
        if let Some(v) = hex_u64(&raw) {
            return Some(Midr::from_u64(v));
        }
    }
    midr_from_cpuinfo(
        &cpuinfo_field("CPU implementer")?,
        &cpuinfo_field("CPU part")?,
        &cpuinfo_field("CPU variant").unwrap_or_default(),
        &cpuinfo_field("CPU revision").unwrap_or_default(),
    )
}

/// The same registers as the kernel prints them in `/proc/cpuinfo`, which is
/// where an arm64 guest without the sysfs identification directory has to be
/// read from. The formats differ per field: implementer, part and variant are
/// hex, revision is decimal.
#[cfg(any(test, not(any(target_arch = "x86", target_arch = "x86_64"))))]
fn midr_from_cpuinfo(implementer: &str, part: &str, variant: &str, revision: &str) -> Option<Midr> {
    Some(Midr {
        implementer: hex_u64(implementer)? as u8,
        part: hex_u64(part)? as u16,
        variant: hex_u64(variant).unwrap_or(0) as u8,
        revision: revision.trim().parse().unwrap_or(0),
    })
}

#[cfg(test)]
mod midr_tests {
    use super::*;

    #[test]
    fn midr_fields_split_as_the_arm_manual_says() {
        // The value from an Oracle A1.Flex instance, which is also what its
        // /proc/cpuinfo prints: implementer 0x41, variant 3, part 0xd0c,
        // revision 1.
        let m = Midr::from_u64(0x0000_0000_413f_d0c1);
        assert_eq!(
            (m.implementer, m.variant, m.part, m.revision),
            (0x41, 3, 0xd0c, 1)
        );
        assert_eq!(m.model(), "Neoverse-N1 (r3p1)");
        assert_eq!(m.vendor(), Some("ARM"));
    }

    #[test]
    fn an_unknown_part_is_described_not_guessed() {
        // Built the way the register is built, rather than by hand-shifting a
        // literal and hoping the nibbles landed where I meant.
        let m = Midr::from_u64((0x41u64 << 24) | (0xf << 16) | (0xabc << 4));
        assert_eq!(m.name(), None);
        assert_eq!(
            m.model(),
            "AArch64 core (implementer 0x41, part 0xabc, r0p0)"
        );
        // Still attributed to a vendor we do know.
        assert_eq!(m.vendor(), Some("ARM"));
    }

    #[test]
    fn implementers_are_decoded_and_part_numbers_collide_across_them() {
        // HiSilicon and ARM both use 0xd01; the pair, not the part, decides.
        let hi = Midr {
            implementer: 0x48,
            variant: 0,
            part: 0xd01,
            revision: 0,
        };
        let arm = Midr {
            implementer: 0x41,
            variant: 0,
            part: 0xd01,
            revision: 0,
        };
        assert_eq!(hi.name(), Some("Kunpeng-920"));
        assert_eq!(arm.name(), Some("Cortex-A32"));
        assert_eq!(hi.vendor(), Some("HiSilicon"));
    }

    #[test]
    fn cpuinfo_fields_rebuild_the_same_identity_as_sysfs() {
        // Verbatim values from the same Oracle A1.Flex instance whose midr_el1
        // is the test above: the two sources must agree, because on a box with
        // both, whichever is read first decides what the report says.
        let m = midr_from_cpuinfo("0x41", "0xd0c", "0x3", "1").unwrap();
        assert_eq!(m, Midr::from_u64(0x0000_0000_413f_d0c1));
        assert_eq!(m.model(), "Neoverse-N1 (r3p1)");
        // A missing variant or revision is not fatal: it is reported as r0p0.
        assert_eq!(
            midr_from_cpuinfo("0x41", "0xd0c", "", "")
                .unwrap()
                .stepping(),
            "r0p0"
        );
        assert!(midr_from_cpuinfo("Arm Holdings", "0xd0c", "0x3", "1").is_none());
    }

    #[test]
    fn hex_inputs_tolerate_the_kernels_variants() {
        assert_eq!(hex_u64("0x00000000413fd0c1\n"), Some(0x413fd0c1));
        assert_eq!(hex_u64("0X41"), Some(0x41));
        assert_eq!(hex_u64("not hex"), None);
    }
}
