//! # membench-core
//!
//! All measurement logic for `membench`, an AIDA64 "Cache & Memory Benchmark"
//! work-alike for Linux. The UI (TUI/CLI) lives in the `membench` binary crate
//! and never touches timing code, so every measurement can be driven headless
//! and from tests.
//!
//! ## Conventions (see also the README; these are printed in the JSON too)
//!
//! * Bandwidth is reported in **GB/s = 10<sup>9</sup> bytes per second** (SI,
//!   not GiB/s).
//! * Copy counts **source bytes only** by default (AIDA convention);
//!   `Settings::copy_counts_both` switches to STREAM-style read+write
//!   accounting.
//! * Latency is **nanoseconds per dependent load** measured by a pointer chase
//!   on an idle core; the cycle figure is derived from the core clock measured
//!   during the same run, not from `cpufreq`.
//! * Anything that cannot be read is reported as `None`/`Unknown` — never
//!   guessed. `Option` fields in [`result`] serialise as JSON `null`.
//!
//! ## Safety
//!
//! `unsafe` is confined to [`kernels`], [`alloc`] and [`affinity`], each block
//! carrying a `// SAFETY:` comment. Everything above that layer is safe Rust.

// Numeric casts are kept in their explicit form even when they are a no-op on
// x86-64: `libc::c_long`, `size_t` and the `suseconds` fields change width
// between 32-bit ARM, 64-bit x86 and musl, and the kernels crate is meant to
// build on all of them. Removing the cast to satisfy the lint would move the
// width question from the source into the target.
#![allow(clippy::unnecessary_cast)]

pub mod affinity;
pub mod alloc;
pub mod bandwidth;
pub mod error;
pub mod kernels;
pub mod latency;
pub mod result;
pub mod runner;
pub mod sizes;
pub mod stats;
pub mod sysinfo;
pub mod time;
pub mod timer;

pub use error::{Error, Result};
pub use result::{CellResult, Isa, Level, Op, PageKind, RunResults, Settings, SystemInfo};
pub use runner::{run_once, Event, Runner};

/// The version of the JSON document schema (PLAN.md: "JSON carries a schema
/// version … so two runs can always be compared").
/// Bumped to 2 with the SPD fixes: the timing fields became sub-nanosecond
/// floats and `cas_string` — which never held a CAS rating — became `cycles`.
/// A v1 document is refused by name rather than silently misread.
pub const SCHEMA_VERSION: u32 = 2;

/// Refuse to time anything from an unoptimised build (PLAN.md rule 2: a debug
/// build inlines nothing and the numbers are meaningless).
///
/// Kernel functions themselves stay usable in debug builds so that the
/// checksum tests can compare SIMD paths against the scalar reference.
pub fn ensure_release_build(what: &str) -> Result<()> {
    if cfg!(debug_assertions) {
        return Err(Error::DebugBuild {
            what: what.to_string(),
        });
    }
    Ok(())
}
