//! CPU affinity and scheduling priority.
//!
//! PLAN.md rule 4: pin **before** allocating (first-touch then keeps pages on
//! the local NUMA node), and re-check affinity at the start of every trial.
//! The mask type below is layout-identical to glibc's `cpu_set_t`, so we do not
//! depend on the libc crate exposing the `CPU_SET` macros.

use std::io;

use crate::{Error, Result};

/// `CPU_SETSIZE` (1024) bits packed into `c_ulong` words, matching glibc's
/// `cpu_set_t` byte layout on 64-bit Linux.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Cpumask {
    words: [u64; 16],
}

const BITS_PER_WORD: usize = 64;

impl Cpumask {
    fn set(&mut self, cpu: usize) {
        let (word, bit) = (cpu / BITS_PER_WORD, cpu % BITS_PER_WORD);
        if word < self.words.len() {
            self.words[word] |= 1u64 << bit;
        }
    }

    /// The inverse of `set`, exercised by the round-trip test below.
    #[cfg(test)]
    fn is_set(&self, cpu: usize) -> bool {
        let (word, bit) = (cpu / BITS_PER_WORD, cpu % BITS_PER_WORD);
        word < self.words.len() && (self.words[word] >> bit) & 1 == 1
    }

    fn cpus(&self) -> Vec<usize> {
        let mut out = Vec::new();
        for (w, word) in self.words.iter().enumerate() {
            let mut bits = *word;
            while bits != 0 {
                let low = bits.trailing_zeros() as usize;
                out.push(w * BITS_PER_WORD + low);
                bits &= bits - 1;
            }
        }
        out
    }
}

/// Restrict the calling thread to one logical CPU and verify it took.
pub fn pin_current(cpu: usize) -> Result<()> {
    let mut mask = Cpumask::default();
    mask.set(cpu);
    // SAFETY: `mask` is a `Cpumask`, whose layout is glibc's `cpu_set_t`, and we
    // pass the matching `size_of` value. `pid == 0` targets the calling thread,
    // which lives as long as the call.
    let rc = unsafe {
        libc::sched_setaffinity(
            0,
            std::mem::size_of::<Cpumask>(),
            &mask as *const Cpumask as *const libc::cpu_set_t,
        )
    };
    if rc != 0 {
        return Err(Error::PinFailed {
            cpu,
            os: io::Error::last_os_error(),
        });
    }
    let now = current_affinity()?;
    if now.as_slice() != [cpu] {
        return Err(Error::PinFailed {
            cpu,
            os: io::Error::from_raw_os_error(libc::EINVAL),
        });
    }
    Ok(())
}

/// Logical CPUs the calling thread is currently allowed to run on.
pub fn current_affinity() -> Result<Vec<usize>> {
    let mut mask = Cpumask::default();
    // SAFETY: writing `size_of::<Cpumask>()` bytes (the layout glibc expects)
    // into a live, writable local.
    let rc = unsafe {
        libc::sched_getaffinity(
            0,
            std::mem::size_of::<Cpumask>(),
            &mut mask as *mut Cpumask as *mut libc::cpu_set_t,
        )
    };
    if rc != 0 {
        return Err(Error::Io {
            path: "sched_getaffinity".to_string(),
            os: io::Error::last_os_error(),
        });
    }
    Ok(mask.cpus())
}

/// The logical CPU the calling thread is running on right now, if the kernel
/// exposes it (it has since 2.6.11).
pub fn current_cpu() -> Option<usize> {
    // SAFETY: argument-free syscall returning a non-negative cpu number or -1.
    let cpu = unsafe { libc::sched_getcpu() };
    if cpu < 0 {
        None
    } else {
        Some(cpu as usize)
    }
}

/// Remove any affinity restriction from the calling thread.
///
/// The kernel intersects the requested mask with the CPUs that exist, so
/// setting all 1024 bits is the portable way to say "everywhere".
pub fn unpin_current() -> Result<()> {
    let mask = Cpumask { words: [u64::MAX; 16] };
    // SAFETY: as in `pin_current`; the kernel drops bits for CPUs that do not
    // exist, so the mask is never empty on a machine with at least one CPU.
    let rc = unsafe {
        libc::sched_setaffinity(
            0,
            std::mem::size_of::<Cpumask>(),
            &mask as *const Cpumask as *const libc::cpu_set_t,
        )
    };
    if rc != 0 {
        return Err(Error::PinFailed {
            cpu: usize::MAX,
            os: io::Error::last_os_error(),
        });
    }
    Ok(())
}

/// Does the calling thread run on exactly one CPU?
pub fn is_pinned() -> bool {
    current_affinity().map(|v| v.len() == 1).unwrap_or(false)
}

/// Best-effort priority boost. Returns a note describing what actually happened
/// so the detail view can say so out loud (PLAN.md rule 11).
///
/// Only `nice` is touched here. `SCHED_FIFO` is available through
/// [`try_realtime`] behind an explicit flag: an RT-priority spin loop can make
/// a desktop unresponsive, which is not a reasonable default.
pub fn boost_nice(delta: i32) -> Option<String> {
    // SAFETY: PRIO_PROCESS with pid 0 (this process) and a small nice delta.
    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, delta) };
    if rc != 0 {
        let e = io::Error::last_os_error();
        return Some(format!(
            "could not set nice {delta}: {e} (run as root for scheduler priority)"
        ));
    }
    None
}

/// Ask for `SCHED_FIFO` real-time scheduling (needs `CAP_SYS_NICE`).
pub fn try_realtime(priority: i32) -> std::result::Result<String, String> {
    let param = libc::sched_param {
        sched_priority: priority,
    };
    // SAFETY: `param` is a live, correctly typed sched_param; SCHED_FIFO with a
    // bounded priority is a valid policy/priority pair.
    let rc = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
    if rc != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(format!("SCHED_FIFO priority {priority}"))
}

/// `RLIMIT_MEMLOCK`, used to decide whether `mlock` of a big buffer can work.
pub fn memlock_limit_bytes() -> Option<u64> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: writing a getrlimit-sized struct into a live local.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut lim) };
    if rc != 0 {
        return None;
    }
    if lim.rlim_cur == libc::RLIM_INFINITY {
        Some(u64::MAX)
    } else {
        Some(lim.rlim_cur as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_round_trip() {
        let mut m = Cpumask::default();
        for c in [0usize, 1, 63, 64, 1023] {
            m.set(c);
        }
        assert_eq!(
            m.cpus(),
            vec![0, 1, 63, 64, 1023],
            "mask decode mismatch: {:?}",
            m.cpus()
        );
        assert!(m.is_set(64) && !m.is_set(65));
    }

    #[test]
    fn mask_size_matches_glibc_cpu_set_t() {
        assert_eq!(std::mem::size_of::<Cpumask>(), 128);
    }

    #[test]
    fn can_pin_and_read_back() {
        let before = current_affinity().expect("affinity");
        let target = before[0];
        pin_current(target).expect("pin");
        assert_eq!(current_affinity().expect("read back"), vec![target]);
        assert!(is_pinned());
        // Leave the thread as we found it: other tests share the process.
        unpin_current().expect("unpin");
        let after = current_affinity().expect("affinity after unpin");
        assert!(
            after.len() >= before.len(),
            "unpin narrowed affinity: {after:?} vs {before:?}"
        );
    }

    #[test]
    fn current_cpu_is_sane() {
        if let Some(cpu) = current_cpu() {
            assert!(current_affinity().unwrap().contains(&cpu));
        }
    }
}
