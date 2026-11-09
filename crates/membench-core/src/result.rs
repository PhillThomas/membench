//! Serialisable result / system-schema types.
//!
//! This module is the JSON contract: [`SCHEMA_VERSION`] plus every field that
//! two runs need in order to be comparable (settings, per-trial samples,
//! detected system, conventions). Fields that could not be determined are
//! `Option::None` and serialise as JSON `null` — never a guess.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::SCHEMA_VERSION;

/// Which level of the hierarchy a cell belongs to.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    L1,
    L2,
    L3,
    Memory,
}

impl Level {
    pub const ALL: [Level; 4] = [Level::L1, Level::L2, Level::L3, Level::Memory];

    /// Row label in the results grid.
    pub fn grid_label(self) -> &'static str {
        match self {
            Level::L1 => "L1 Cache",
            Level::L2 => "L2 Cache",
            Level::L3 => "L3 Cache",
            Level::Memory => "Memory",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Level::L1 => "L1",
            Level::L2 => "L2",
            Level::L3 => "L3",
            Level::Memory => "Mem",
        }
    }

    pub fn is_memory(self) -> bool {
        matches!(self, Level::Memory)
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.short())
    }
}

impl FromStr for Level {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "l1" | "l1d" | "l1-cache" => Ok(Level::L1),
            "l2" => Ok(Level::L2),
            "l3" => Ok(Level::L3),
            "mem" | "memory" | "dram" => Ok(Level::Memory),
            other => Err(format!("unknown level {other:?} (use l1, l2, l3 or mem)")),
        }
    }
}

/// Which measurement a cell holds.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Read,
    /// Normal (read-for-ownership) stores.
    Write,
    /// Non-temporal stores (`vmovntdq` + `sfence`).
    WriteNt,
    /// Load + normal store.
    Copy,
    /// Load + non-temporal store.
    CopyNt,
    Latency,
    /// `rep movsb` variant, detail view only (fast on ERMSB/FSRM CPUs).
    RepMovsb,
    /// `rep stosb` variant, detail view only.
    RepStosb,
}

/// The four columns of the AIDA-style grid.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Column {
    Read,
    Write,
    Copy,
    Latency,
}

impl Op {
    pub const ALL: [Op; 6] = [
        Op::Read,
        Op::Write,
        Op::WriteNt,
        Op::Copy,
        Op::CopyNt,
        Op::Latency,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Op::Read => "Read",
            Op::Write => "Write",
            Op::WriteNt => "Write (NT)",
            Op::Copy => "Copy",
            Op::CopyNt => "Copy (NT)",
            Op::Latency => "Latency",
            Op::RepMovsb => "Copy (rep movsb)",
            Op::RepStosb => "Write (rep stosb)",
        }
    }

    pub fn unit(self) -> &'static str {
        match self {
            Op::Latency => "ns",
            _ => "GB/s",
        }
    }

    pub fn is_latency(self) -> bool {
        matches!(self, Op::Latency)
    }

    /// Grid column this op belongs to.
    pub fn column(self) -> Column {
        match self {
            Op::Read => Column::Read,
            Op::Write | Op::WriteNt | Op::RepStosb => Column::Write,
            Op::Copy | Op::CopyNt | Op::RepMovsb => Column::Copy,
            Op::Latency => Column::Latency,
        }
    }

    /// User-facing names accepted by `--only`.
    pub fn parse_list(s: &str) -> Result<Vec<Op>, String> {
        let mut out = Vec::new();
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match part.to_ascii_lowercase().as_str() {
                "read" => out.push(Op::Read),
                "write" => {
                    out.push(Op::Write);
                    out.push(Op::WriteNt);
                }
                "write-nt" | "writent" => out.push(Op::WriteNt),
                "write-normal" | "writenormal" => out.push(Op::Write),
                "copy" => {
                    out.push(Op::Copy);
                    out.push(Op::CopyNt);
                }
                "copy-nt" | "copynt" => out.push(Op::CopyNt),
                "latency" | "lat" => out.push(Op::Latency),
                other => {
                    return Err(format!(
                        "unknown test {other:?} (use read, write, copy or latency)"
                    ))
                }
            }
        }
        if out.is_empty() {
            return Err("no tests selected".to_string());
        }
        Ok(out)
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl fmt::Display for Column {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Column::Read => "Read",
            Column::Write => "Write",
            Column::Copy => "Copy",
            Column::Latency => "Latency",
        })
    }
}

/// Which SIMD path executed a kernel.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Isa {
    Avx512,
    Avx2,
    Sse2,
    Neon,
    Scalar,
}

impl Isa {
    /// Panel label.
    pub fn label(self) -> &'static str {
        match self {
            Isa::Avx512 => "AVX-512",
            Isa::Avx2 => "AVX2",
            Isa::Sse2 => "SSE2",
            Isa::Neon => "NEON",
            Isa::Scalar => "scalar",
        }
    }

    pub const ALL: [Isa; 5] = [Isa::Avx512, Isa::Avx2, Isa::Sse2, Isa::Neon, Isa::Scalar];
}

impl fmt::Display for Isa {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Isa::Avx512 => "avx512",
            Isa::Avx2 => "avx2",
            Isa::Sse2 => "sse2",
            Isa::Neon => "neon",
            Isa::Scalar => "scalar",
        })
    }
}

impl FromStr for Isa {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "avx512" | "avx-512" | "avx512f" => Ok(Isa::Avx512),
            "avx2" => Ok(Isa::Avx2),
            "sse2" => Ok(Isa::Sse2),
            "neon" => Ok(Isa::Neon),
            "scalar" => Ok(Isa::Scalar),
            other => Err(format!(
                "unknown ISA {other:?} (use avx512, avx2, sse2, neon or scalar)"
            )),
        }
    }
}

/// Page size the benchmark asked for.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[serde(rename_all = "lowercase")]
pub enum PageKind {
    FourK,
    #[serde(rename = "2m")]
    TwoM,
    #[serde(rename = "1g")]
    OneG,
}

impl PageKind {
    /// Panel label: "4K", "2M", "1G".
    pub fn label(self) -> &'static str {
        match self {
            PageKind::FourK => "4K",
            PageKind::TwoM => "2M",
            PageKind::OneG => "1G",
        }
    }

    pub fn bytes(self) -> usize {
        match self {
            PageKind::FourK => 4 * 1024,
            PageKind::TwoM => 2 * 1024 * 1024,
            PageKind::OneG => 1024 * 1024 * 1024,
        }
    }
}

impl fmt::Display for PageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PageKind::FourK => "4k",
            PageKind::TwoM => "2m",
            PageKind::OneG => "1g",
        })
    }
}

impl FromStr for PageKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s
            .to_ascii_lowercase()
            .replace("page", "")
            .trim()
            .to_string()
            .as_str()
        {
            "4k" | "4" => Ok(PageKind::FourK),
            "2m" | "2" => Ok(PageKind::TwoM),
            "1g" | "1" => Ok(PageKind::OneG),
            other => Err(format!("unknown page size {other:?} (use 4k, 2m or 1g)")),
        }
    }
}

/// Which store the "Write" headline column shows for the Memory row.
#[derive(
    Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum WriteHeadline {
    /// Non-temporal stores: the achievable peak write, no read-for-ownership.
    #[default]
    Nt,
    /// Normal stores: what a plain `memcpy`-style writer sees.
    Normal,
}

impl fmt::Display for WriteHeadline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            WriteHeadline::Nt => "nt",
            WriteHeadline::Normal => "normal",
        })
    }
}

impl FromStr for WriteHeadline {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "nt" | "non-temporal" | "nontemporal" => Ok(WriteHeadline::Nt),
            "normal" | "regular" => Ok(WriteHeadline::Normal),
            other => Err(format!(
                "unknown write headline {other:?} (use nt or normal)"
            )),
        }
    }
}

/// Everything that changes a number, so two JSON runs can be diffed honestly.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Settings {
    /// `None` = "one thread per physical core" (cache levels) / all logical
    /// CPUs (memory).
    pub threads: Option<usize>,
    /// `None` = best path the CPU supports.
    pub isa: Option<Isa>,
    pub pages: PageKind,
    pub trials: usize,
    pub duration_ms: u64,
    /// `None` = the runner's own sweep ladder.
    pub sweep_duration_ms: Option<u64>,
    pub sweep_trials: Option<usize>,
    pub copy_counts_both: bool,
    pub write_headline: WriteHeadline,
    /// Latency sweep requested in this run?
    pub with_sweep: bool,
    /// Fixed seed for the pointer-chase shuffle (reproducible runs).
    pub seed: u64,
    /// Try to `mlock()` buffers (best effort).
    pub mlock: bool,
    /// Raise scheduling priority when possible (root only).
    pub boost_priority: bool,
    /// CPU the single-threaded cells and the sweep run on (`--cpu`). `None`
    /// takes the first core in the detected order. On a shared machine that
    /// default can land next to a busy SMT sibling, and no amount of care in
    /// the kernels fixes that, so the choice has to be reachable from outside.
    pub latency_cpu: Option<usize>,
    /// Drop the fields that identify this machine to a person rather than
    /// describing the hardware the numbers came from (`--redact`): the
    /// hostname, and the serial numbers burned into the memory modules. A
    /// setting rather than a post-processing step, because it has to be
    /// recorded: a field that was removed and a field the tool could never
    /// read must not look identical to whoever compares two runs later.
    pub redact: bool,
    /// Levels and ops selected for this run.
    pub levels: Vec<Level>,
    pub ops: Vec<Op>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            threads: None,
            isa: None,
            pages: PageKind::TwoM,
            trials: 7,
            duration_ms: 200,
            sweep_duration_ms: Some(60),
            sweep_trials: Some(3),
            copy_counts_both: false,
            write_headline: WriteHeadline::default(),
            with_sweep: false,
            seed: 0x5eed_1234_9abc_def0,
            mlock: true,
            boost_priority: true,
            latency_cpu: None,
            redact: false,
            levels: Level::ALL.to_vec(),
            ops: vec![
                Op::Read,
                Op::Write,
                Op::WriteNt,
                Op::Copy,
                Op::CopyNt,
                Op::Latency,
            ],
        }
    }
}

impl Settings {
    /// Ops that actually run for `level`.
    ///
    /// Cache levels have no non-temporal variant (NT stores bypass the cache
    /// being measured), so only normal stores run there. Main memory runs both
    /// the normal-store and the non-temporal-store variant; the grid shows the
    /// one named by [`Settings::write_headline`] and the detail view shows both.
    pub fn ops_for(&self, level: Level) -> Vec<Op> {
        self.ops
            .iter()
            .copied()
            .filter(|op| match op {
                Op::WriteNt | Op::CopyNt => level.is_memory(),
                _ => true,
            })
            .collect()
    }

    /// Which op the grid column shows for `level`.
    pub fn headline_op(&self, level: Level, column: Column) -> Option<Op> {
        let candidates: &[Op] = match column {
            Column::Read => &[Op::Read],
            Column::Write => {
                if level.is_memory() {
                    match self.write_headline {
                        WriteHeadline::Nt => &[Op::WriteNt, Op::Write],
                        WriteHeadline::Normal => &[Op::Write, Op::WriteNt],
                    }
                } else {
                    &[Op::Write]
                }
            }
            Column::Copy => {
                if level.is_memory() {
                    &[Op::CopyNt, Op::Copy]
                } else {
                    &[Op::Copy]
                }
            }
            Column::Latency => &[Op::Latency],
        };
        candidates.iter().copied().find(|op| self.ops.contains(op))
    }
}

/// A warning that may invalidate or skew a number (PLAN.md rule 11: say it out
/// loud instead of silently reporting a suspicious figure).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Warning {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Warning {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Warning {
            code: code.to_string(),
            message: message.into(),
            hint: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

// ---------------------------------------------------------------------------------------------
// System information
// ---------------------------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct CpuInfo {
    pub vendor: Option<String>,
    pub model: Option<String>,
    /// "AMD" / "Intel" / "ARM" … derived from the vendor string.
    pub vendor_flavour: Option<String>,
    pub architecture: String,
    /// Feature flags relevant to the kernel dispatch (SSE2, AVX2, AVX-512F…).
    pub features: Vec<String>,
    /// ISA paths this CPU can execute, best first.
    pub isa_paths: Vec<Isa>,
    /// `raw-cpuid` reported cache topology (informational only; the sysfs
    /// tree is what drives buffer sizing because it is per-core).
    pub cpuid_leaf_max: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct CoreInfo {
    /// Logical CPU number.
    pub cpu: usize,
    pub core_id: Option<u32>,
    pub physical_core: Option<usize>,
    pub is_boot_core: bool,
    /// SMT siblings including itself.
    pub siblings: Vec<usize>,
    pub thread_idx: Option<u32>,
    /// `cpu_capacity` (Arm big.LITTLE / Intel hybrid), 1024 = max.
    pub capacity: Option<u32>,
    pub cluster: Option<String>,
    pub max_freq_khz: Option<u64>,
    pub cur_freq_khz: Option<u64>,
    pub governor: Option<String>,
    pub energy_performance_preference: Option<String>,
    /// Intel hybrid role: "performance" / "efficient".
    pub hybrid_role: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct Topology {
    pub logical_cpus: usize,
    pub physical_cores: usize,
    pub smt_enabled: bool,
    /// One id per physical core, first sibling chosen as representative.
    pub core_cpus: Vec<usize>,
    pub all_cpus: Vec<usize>,
    pub hybrid: bool,
    pub numa_nodes: Vec<NumaNode>,
    pub sockets: usize,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct NumaNode {
    pub id: usize,
    pub cpus: Vec<usize>,
    pub total_ram_bytes: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum CacheKind {
    Data,
    Instruction,
    Unified,
}

impl fmt::Display for CacheKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CacheKind::Data => "Data",
            CacheKind::Instruction => "Instruction",
            CacheKind::Unified => "Unified",
        })
    }
}

/// One cache level as seen by one core.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct CacheLevel {
    pub level: u8,
    pub kind: CacheKind,
    pub size_bytes: Option<u64>,
    pub line_bytes: Option<u32>,
    pub physical_line_bytes: Option<u32>,
    pub ways: Option<u32>,
    pub sets: Option<u32>,
    /// CPUs that share this instance (from `shared_cpu_list`).
    pub shared_cpus: Vec<usize>,
    /// Number of distinct instances of this level (unique `shared_cpu_list`s).
    pub instances: Option<usize>,
    /// Aggregate size across instances.
    pub total_bytes: Option<u64>,
    /// `size` per core differs between P and E cores on hybrid parts.
    pub sizes_vary_between_cores: bool,
}

/// All cache levels, one entry per (level, kind) as reported for the boot core.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct CacheInfo {
    pub levels: Vec<CacheLevel>,
    /// Largest data cache at `level` for a given core (what buffers are sized from).
    pub line_bytes: Option<u32>,
    pub l3_domains: Vec<L3Domain>,
}

impl CacheInfo {
    pub fn level(&self, level: u8, kind: CacheKind) -> Option<&CacheLevel> {
        self.levels
            .iter()
            .find(|c| c.level == level && (kind == CacheKind::Unified || c.kind == kind))
    }

    /// Largest data-bearing cache at a level (Data or Unified).
    pub fn data_level(&self, level: u8) -> Option<&CacheLevel> {
        self.levels
            .iter()
            .filter(|c| c.level == level && c.kind != CacheKind::Instruction)
            .max_by_key(|c| c.size_bytes.unwrap_or(0))
    }

    pub fn data_cache_bytes(&self, level: Level) -> Option<u64> {
        match level {
            Level::L1 => self.data_level(1).and_then(|c| c.size_bytes),
            Level::L2 => self.data_level(2).and_then(|c| c.size_bytes),
            Level::L3 => self.data_level(3).and_then(|c| c.size_bytes),
            Level::Memory => None,
        }
    }
}

/// A group of cores sharing one L3 (an AMD CCD, or one Intel LLC slice).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct L3Domain {
    pub id: usize,
    pub cpus: Vec<usize>,
    pub size_bytes: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct Dimm {
    pub slot: Option<String>,
    pub bank: Option<String>,
    pub size_bytes: Option<u64>,
    pub form_factor: Option<String>,
    pub device_type: Option<String>,
    pub max_speed_mts: Option<u32>,
    pub configured_speed_mts: Option<u32>,
    pub part_number: Option<String>,
    pub manufacturer: Option<String>,
    pub serial: Option<String>,
    pub data_width_bits: Option<u32>,
    pub total_width_bits: Option<u32>,
    /// Check bits beyond the data path: total width minus data width, when the
    /// firmware reported both. Eight on a 72-bit ECC module. `None` means the
    /// widths were not reported, which is not the same answer as zero.
    pub ecc_bits: Option<u32>,
    pub ranks: Option<u32>,
    /// Where this row came from, so the panel can be honest about it.
    pub source: String,
}

/// SPD timings as read from the EEPROM, always labelled as rated rather than
/// actual (running timings need memory-controller register access per vendor).
// The field names are the JEDEC timing mnemonics, not Rust identifiers in
// disguise: `tAA_ns` is what the SPD layout calls it and the JSON is public.
// They are floats because the layout resolves finer than a nanosecond: the
// reporter's own module publishes tAAmin = 13.75 ns, and an integer field
// would have printed that as 13 and called it a spec figure.
#[allow(non_snake_case)]
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct SpdTimings {
    /// AA-RCD-RP-RAS in cycles at tCKAVGmin, JEDEC-rounded, if derivable. Not
    /// a CAS *rating* — the label used to imply one: which CAS a module runs
    /// at is the controller's pick from the supported set, and the note
    /// carries that set.
    pub cycles: Option<String>,
    pub tAA_ns: Option<f32>,
    pub tRCD_ns: Option<f32>,
    pub tRP_ns: Option<f32>,
    pub tRAS_ns: Option<f32>,
    /// Rated speed from tCKAVGmin in MT/s: the number on the module's label.
    pub max_mts: Option<u32>,
    /// Module part number as written in the SPD vendor block (bytes 329..348
    /// for DDR4). Additive in schema 2: an older document simply lacks it.
    pub part_number: Option<String>,
    pub generation: Option<String>,
    pub device: String,
    pub note: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct MemoryInfo {
    pub total_bytes: Option<u64>,
    pub mem_type: Option<String>,
    /// Where `mem_type` came from, for the honest-label rule.
    pub mem_type_source: Option<String>,
    pub speed_mts: Option<u32>,
    pub configured_speed_mts: Option<u32>,
    pub channels: Option<u32>,
    pub channels_note: Option<String>,
    pub modules: Vec<Dimm>,
    pub populated_modules: Option<usize>,
    /// What the physical memory array says about error correction, from
    /// SMBIOS type 16. A claim about the board, not about any module: an array
    /// can say `None` while its modules carry check bits, and printing one
    /// merged verdict would hide the only interesting case.
    pub error_correction: Option<String>,
    pub spd: Vec<SpdTimings>,
    pub transparent_hugepages: Option<String>,
    pub numa_node_count: usize,
    /// Human note about DIMM access (e.g. "run with sudo for DIMM details").
    pub access_note: Option<String>,
    /// Swap / ballooning etc. that would corrupt memory-bandwidth numbers.
    pub overcommit_or_ballooned: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ClockMeasurement {
    /// Effective core clock in Hz, measured with a dependent integer-add chain.
    pub hz: f64,
    pub mhz: f64,
    pub cpu: usize,
    pub method: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct OsInfo {
    pub kernel_release: Option<String>,
    pub hostname: Option<String>,
    pub page_size_bytes: usize,
    pub membench_version: String,
    pub running_as_root: bool,
    pub mlock_limit_bytes: Option<u64>,
}

/// Everything the system panel and the JSON document report.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct SystemInfo {
    pub cpu: CpuInfo,
    pub cores: Vec<CoreInfo>,
    pub topology: Topology,
    pub caches: CacheInfo,
    pub memory: MemoryInfo,
    pub os: OsInfo,
    pub warnings: Vec<Warning>,
}

impl SystemInfo {
    pub fn cache_line_bytes(&self) -> Option<u32> {
        self.caches.line_bytes
    }

    pub fn governor_warnings(&self) -> Vec<&Warning> {
        self.warnings
            .iter()
            .filter(|w| matches!(w.code.as_str(), "governor" | "epp" | "freq-drop"))
            .collect()
    }

    /// Remove what identifies this machine to a person, and keep everything
    /// that explains the numbers. Called once, straight after detection, by
    /// [`crate::sysinfo::gather_for`] when `--redact` is set, so that no
    /// writer has to remember.
    ///
    /// The warning matters as much as the removal. `None` already means "the
    /// firmware did not tell us", and a reader who cannot tell that apart from
    /// "the user withheld it" has been told something false about the
    /// machine. So a redacted run says so in the same list that carries every
    /// other caveat.
    pub fn redact(&mut self) {
        let had_serials = self.memory.modules.iter().any(|d| d.serial.is_some());
        self.os.hostname = None;
        for d in &mut self.memory.modules {
            d.serial = None;
        }
        let hint = if had_serials {
            "every other field is as detected; run without --redact where the file is not being shared"
        } else {
            "this machine reports no serial numbers anyway; every other field is as detected"
        };
        self.warnings.push(
            Warning::new(
                "redaction",
                "hostname and DIMM serial numbers were withheld on request",
            )
            .with_hint(hint),
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------------------------

/// One measured cell: a (level, op) pair with its samples and full metadata.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct CellResult {
    pub level: Level,
    pub op: Op,
    /// Shown in the grid (vs. detail-view only).
    pub headline: bool,
    /// Headline figure: best trial (AIDA-style peak).
    pub value: f64,
    pub unit: String,
    pub best: f64,
    pub median: f64,
    pub mean: f64,
    pub stddev: f64,
    /// (max-min)/median across retained trials: run-to-run spread.
    pub spread_pct: f64,
    pub rejected_trials: usize,
    /// Every trial's value in `unit`, in run order.
    pub trials: Vec<f64>,
    pub threads: usize,
    pub cpus: Vec<usize>,
    pub buffer_bytes_per_thread: usize,
    /// Buffers per thread (copy uses two).
    pub buffers_per_thread: u8,
    /// Bytes moved by the *winning* trial, summed over threads.
    pub bytes_moved: u128,
    /// Wall-clock duration of the winning trial.
    pub duration_us: u128,
    pub isa: Isa,
    pub pages_requested: PageKind,
    /// Page size actually verified for the buffers (from `/proc/self/smaps`).
    pub pages_actual: PageKind,
    pub huge_pages_bytes: Option<u64>,
    pub notes: Vec<String>,
    /// Latency only: nanoseconds converted to cycles with `clock`.
    pub cycles: Option<f64>,
    /// Effective clock during this cell, MHz.
    pub clock_mhz: Option<f64>,
    /// Peak bandwidth as a share of the theoretical memory peak, when known.
    pub peak_share_pct: Option<f64>,
}

/// An empty cell. Written by hand rather than derived so that the zero of the
/// enum types stays a deliberate choice: `Level::L1`/`Op::Read` are placeholders
/// here, and `Isa::Scalar`/`PageKind::FourK` are the values that are always
/// safe to print before a real measurement has filled the struct in.
impl Default for CellResult {
    fn default() -> Self {
        CellResult {
            level: Level::L1,
            op: Op::Read,
            headline: false,
            value: 0.0,
            unit: String::new(),
            best: 0.0,
            median: 0.0,
            mean: 0.0,
            stddev: 0.0,
            spread_pct: 0.0,
            rejected_trials: 0,
            trials: Vec::new(),
            threads: 0,
            cpus: Vec::new(),
            buffer_bytes_per_thread: 0,
            buffers_per_thread: 1,
            bytes_moved: 0,
            duration_us: 0,
            isa: Isa::Scalar,
            pages_requested: PageKind::FourK,
            pages_actual: PageKind::FourK,
            huge_pages_bytes: None,
            notes: Vec::new(),
            cycles: None,
            clock_mhz: None,
            peak_share_pct: None,
        }
    }
}

impl CellResult {
    pub fn key(&self) -> (Level, Op) {
        (self.level, self.op)
    }

    /// A cell holding a single figure. The runners fill in every statistic;
    /// this exists for placeholder rows and for tests that only care about the
    /// grid position.
    pub fn new(level: Level, op: Op, value: f64, unit: impl Into<String>) -> Self {
        CellResult {
            level,
            op,
            headline: false,
            value,
            unit: unit.into(),
            best: value,
            median: value,
            mean: value,
            ..CellResult::default()
        }
    }
}

/// One point of the latency-vs-block-size sweep.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct SweepPoint {
    pub size_bytes: usize,
    pub nodes: usize,
    pub ns: f64,
    pub cycles: Option<f64>,
    pub trials: usize,
    pub best_ns: f64,
    pub spread_pct: f64,
    pub pages_actual: PageKind,
    pub huge_pages_bytes: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Sweep {
    pub points: Vec<SweepPoint>,
    pub notes: Vec<String>,
    pub duration_ms: u64,
    pub trials: usize,
}

/// The whole document written by `--json` / `e`xport.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct RunResults {
    pub schema_version: u32,
    pub generator: String,
    /// Local time with offset, RFC 3339.
    pub timestamp: String,
    pub system: SystemInfo,
    pub settings: Settings,
    pub warnings: Vec<Warning>,
    pub clock: Option<ClockMeasurement>,
    pub cells: Vec<CellResult>,
    pub sweep: Option<Sweep>,
}

impl RunResults {
    pub fn new(system: SystemInfo, settings: Settings) -> Self {
        RunResults {
            schema_version: SCHEMA_VERSION,
            generator: format!("membench {}", crate::time::version()),
            timestamp: crate::time::now_rfc3339(),
            system,
            settings,
            warnings: Vec::new(),
            clock: None,
            cells: Vec::new(),
            sweep: None,
        }
    }

    pub fn cell(&self, level: Level, op: Op) -> Option<&CellResult> {
        self.cells.iter().find(|c| c.level == level && c.op == op)
    }

    /// The cell to show for a grid column: the headline one if present,
    /// otherwise any cell for that (level, column).
    pub fn grid_cell(&self, level: Level, column: Column) -> Option<&CellResult> {
        let mut fallback = None;
        for c in &self.cells {
            if c.level != level || c.op.column() != column {
                continue;
            }
            if c.headline {
                return Some(c);
            }
            fallback.get_or_insert(c);
        }
        fallback
    }

    /// Cell order for the CLI/TUI grid: Memory first, then L1/L2/L3 (AIDA order).
    pub fn grid_row_levels(&self) -> Vec<Level> {
        let order = [Level::Memory, Level::L1, Level::L2, Level::L3];
        let present: Vec<Level> = order
            .into_iter()
            .filter(|l| self.cells.iter().any(|c| c.level == *l))
            .collect();
        if present.is_empty() {
            self.settings.levels.to_vec()
        } else {
            present
        }
    }

    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> SystemInfo {
        SystemInfo {
            os: OsInfo {
                hostname: Some("3950xrig".to_string()),
                kernel_release: Some("6.12.115".to_string()),
                ..Default::default()
            },
            memory: MemoryInfo {
                modules: vec![Dimm {
                    slot: Some("DIMM 0".to_string()),
                    serial: Some("89A2C1F4".to_string()),
                    part_number: Some("F4-3600C18-16GTZR".to_string()),
                    manufacturer: Some("G Skill".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn redaction_removes_identity_and_says_which_it_was() {
        let mut sys = machine();
        sys.redact();
        assert!(sys.os.hostname.is_none());
        assert!(
            sys.memory.modules[0].serial.is_none(),
            "the serial is the one field that singles out this box"
        );
        // Part number and manufacturer stay: they describe the hardware, and
        // dropping them would cost the reader a real comparison.
        assert!(sys.memory.modules[0].part_number.is_some());
        assert!(sys.memory.modules[0].manufacturer.is_some());
        assert_eq!(sys.os.kernel_release.as_deref(), Some("6.12.115"));
        let red = sys
            .warnings
            .iter()
            .filter(|w| w.code == "redaction")
            .collect::<Vec<_>>();
        assert_eq!(red.len(), 1, "a redacted run must record that it was");
        assert!(red[0].hint.is_some(), "and say what is still real");
    }

    #[test]
    fn redaction_distinguishes_withheld_from_absent() {
        let mut bare = machine();
        bare.memory.modules[0].serial = None;
        bare.redact();
        let w = bare
            .warnings
            .iter()
            .find(|w| w.code == "redaction")
            .expect("warning");
        assert!(
            w.hint.as_deref().unwrap_or_default().contains("anyway"),
            "the hint must differ when there was nothing to withhold: {:?}",
            w.hint
        );
    }

    #[test]
    fn redact_defaults_off_and_is_recorded_in_settings() {
        assert!(!Settings::default().redact);
        let s = Settings {
            redact: true,
            ..Settings::default()
        };
        assert!(serde_json::to_string(&s)
            .unwrap()
            .contains("\"redact\":true"));
    }
}
