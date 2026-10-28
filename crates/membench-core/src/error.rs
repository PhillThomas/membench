//! Library error type (`thiserror` in the library, `anyhow` in the binary).

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

use std::fmt;

use crate::result::{Isa, Level, PageKind};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A benchmark was attempted from an unoptimised build (PLAN.md rule 2).
    #[error(
        "refusing to time anything from a debug build ({what}); build with `cargo build --release`"
    )]
    DebugBuild { what: String },

    /// The level asked for does not exist on this machine (e.g. no L3).
    #[error("this CPU has no {0} cache, so that cell cannot be measured")]
    MissingCache(Level),

    /// Buffer sizing produced something unusable (e.g. smaller than one line).
    #[error("cannot size a buffer for {level} from the detected topology: {why}")]
    Unsizeable { level: Level, why: String },

    #[error("requested ISA path {0} is not available on this CPU")]
    IsaUnavailable(Isa),

    #[error("allocating {bytes} bytes with {pages} pages failed: {os}")]
    AllocationFailed {
        bytes: usize,
        pages: PageKind,
        os: std::io::Error,
    },

    #[error("pinning to CPU {cpu} failed: {os}")]
    PinFailed { cpu: usize, os: std::io::Error },

    #[error("thread {0} panicked while measuring")]
    ThreadPanicked(String),

    #[error("kernel {name} is not implemented for {isa}")]
    KernelUnavailable { name: &'static str, isa: Isa },

    #[error("no cache line size reported by the firmware (need /sys/.../cache/index*/coherency_line_size)")]
    NoCacheLineSize,

    #[error("io error on {path}: {os}")]
    Io { path: String, os: std::io::Error },

    #[error("not supported on this platform: {0}")]
    Unsupported(&'static str),

    /// `--cpu` named a CPU this machine does not have. Listed rather than
    /// refused silently: on a pinned benchmark, running somewhere other than
    /// the requested core is the one mistake that cannot be spotted afterwards.
    #[error("cpu {cpu} is not a logical cpu of this machine (available: {list})")]
    NoSuchCpu { cpu: usize, list: String },
}

/// `Error` is used across thread boundaries, so it must not carry anything
/// non-`Send`; it does not.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    assert_send::<Error>();
    assert_sync::<Error>();
};

/// Small helper for the "treat every source as optional" rule in the detection
/// table: anything that fails to read becomes `None`, and the reason is kept
/// for `--verbose` diagnostics only.
pub struct ReadAttempt<T>(pub Option<T>, pub Option<String>);

impl<T> ReadAttempt<T> {
    pub fn ok(value: T) -> Self {
        ReadAttempt(Some(value), None)
    }

    pub fn failed(path: &str, os: &std::io::Error) -> Self {
        ReadAttempt(None, Some(format!("{path}: {os}")))
    }

    pub fn value(self) -> Option<T> {
        self.0
    }

    pub fn reason(&self) -> Option<&str> {
        self.1.as_deref()
    }
}

impl fmt::Display for ReadAttempt<u64> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.0, &self.1) {
            (Some(v), _) => write!(f, "{v}"),
            (None, Some(r)) => write!(f, "unreadable ({r})"),
            (None, None) => write!(f, "unreadable"),
        }
    }
}
