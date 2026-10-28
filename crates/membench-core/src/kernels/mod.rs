//! Runtime SIMD dispatch.
//!
//! PLAN.md: pick the kernel set at runtime (`is_x86_feature_detected!`), never
//! with `-C target-cpu=native`, so one binary runs on every machine; `--isa`
//! forces a path because AVX-512 is slower than AVX2 on some chips.
//!
//! The resolved [`Kernels`] is a small struct of function pointers built once,
//! before any timing, so the dispatch branch is not inside a timed region.

use crate::result::Isa;
use crate::{Error, Result};

pub mod scalar;

#[cfg(target_arch = "x86_64")]
pub mod x86_avx2;
#[cfg(target_arch = "x86_64")]
pub mod x86_avx512;
#[cfg(target_arch = "x86_64")]
pub mod x86_common;
#[cfg(target_arch = "x86_64")]
pub mod x86_sse2;

#[cfg(target_arch = "aarch64")]
pub mod aarch64_neon;

pub use scalar::{add_chain, chase};

#[cfg(target_arch = "x86_64")]
pub use x86_common::{rdtsc, rep_movsb, rep_stosb, tsc_available};

pub type ReadFn = unsafe fn(*const u8, usize, usize) -> u64;
pub type WriteFn = unsafe fn(*mut u8, usize, usize, u64) -> u64;
pub type CopyFn = unsafe fn(*const u8, *mut u8, usize, usize) -> u64;

/// One coherent set of kernels plus the label that goes with it.
#[derive(Clone, Copy, Debug)]
pub struct Kernels {
    pub isa: Isa,
    pub read: ReadFn,
    pub write: WriteFn,
    pub write_nt: WriteFn,
    pub copy: CopyFn,
    pub copy_nt: CopyFn,
    /// True when the "non-temporal" entry points are the ordinary-store kernels
    /// (there is no non-temporal scalar store). The runner prints this so the
    /// Write cell is never mislabelled (PLAN.md rule 11).
    pub nt_is_normal: bool,
}

/// Can this CPU execute `isa`?
pub fn detect(isa: Isa) -> bool {
    match isa {
        Isa::Scalar => true,
        #[cfg(target_arch = "x86_64")]
        Isa::Sse2 => true, // part of the x86-64 baseline
        #[cfg(target_arch = "x86")]
        Isa::Sse2 => std::arch::is_x86_feature_detected!("sse2"),
        #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
        Isa::Sse2 => false,
        #[cfg(target_arch = "x86_64")]
        Isa::Avx2 => std::arch::is_x86_feature_detected!("avx2"),
        #[cfg(not(target_arch = "x86_64"))]
        Isa::Avx2 => false,
        #[cfg(target_arch = "x86_64")]
        Isa::Avx512 => std::arch::is_x86_feature_detected!("avx512f"),
        #[cfg(not(target_arch = "x86_64"))]
        Isa::Avx512 => false,
        #[cfg(target_arch = "aarch64")]
        Isa::Neon => true, // NEON is mandatory on AArch64
        #[cfg(not(target_arch = "aarch64"))]
        Isa::Neon => false,
    }
}

/// Executable ISA paths on this CPU, best first.
pub fn available() -> Vec<Isa> {
    Isa::ALL.into_iter().filter(|isa| detect(*isa)).collect()
}

/// The path the runner picks when `--isa` was not given.
pub fn best() -> Isa {
    available().first().copied().unwrap_or(Isa::Scalar)
}

impl Kernels {
    /// Resolve a kernel set. `None` means "fastest this CPU supports".
    ///
    /// An explicitly requested path that the CPU does not implement is an error,
    /// never a silent downgrade (rule 11: honest labels).
    pub fn resolve(requested: Option<Isa>) -> Result<Kernels> {
        let isa = match requested {
            Some(isa) => {
                if !detect(isa) {
                    return Err(Error::IsaUnavailable(isa));
                }
                isa
            }
            None => best(),
        };
        Self::for_isa(isa)
    }

    pub fn for_isa(isa: Isa) -> Result<Kernels> {
        let kernels = match isa {
            Isa::Scalar => Kernels {
                isa,
                read: scalar::read,
                write: scalar::write,
                write_nt: scalar::write,
                copy: scalar::copy,
                copy_nt: scalar::copy,
                nt_is_normal: true,
            },
            #[cfg(target_arch = "x86_64")]
            Isa::Sse2 => Kernels {
                isa,
                read: x86_sse2::read,
                write: x86_sse2::write,
                write_nt: x86_sse2::write_nt,
                copy: x86_sse2::copy,
                copy_nt: x86_sse2::copy_nt,
                nt_is_normal: false,
            },
            #[cfg(target_arch = "x86_64")]
            Isa::Avx2 => Kernels {
                isa,
                read: x86_avx2::read,
                write: x86_avx2::write,
                write_nt: x86_avx2::write_nt,
                copy: x86_avx2::copy,
                copy_nt: x86_avx2::copy_nt,
                nt_is_normal: false,
            },
            #[cfg(target_arch = "x86_64")]
            Isa::Avx512 => Kernels {
                isa,
                read: x86_avx512::read,
                write: x86_avx512::write,
                write_nt: x86_avx512::write_nt,
                copy: x86_avx512::copy,
                copy_nt: x86_avx512::copy_nt,
                nt_is_normal: false,
            },
            #[cfg(target_arch = "aarch64")]
            Isa::Neon => Kernels {
                isa,
                read: aarch64_neon::read,
                write: aarch64_neon::write,
                write_nt: aarch64_neon::write_nt,
                copy: aarch64_neon::copy,
                copy_nt: aarch64_neon::copy_nt,
                nt_is_normal: false,
            },
            #[cfg(not(target_arch = "x86_64"))]
            Isa::Sse2 | Isa::Avx2 | Isa::Avx512 => return Err(Error::IsaUnavailable(isa)),
            #[cfg(not(target_arch = "aarch64"))]
            Isa::Neon => return Err(Error::IsaUnavailable(isa)),
        };
        debug_assert_eq!(kernels.isa, isa);
        Ok(kernels)
    }

    /// # Safety
    /// Contracts of the individual kernels (live, correctly sized regions).
    #[inline]
    pub unsafe fn read(&self, src: *const u8, len: usize, reps: usize) -> u64 {
        (self.read)(src, len, reps)
    }

    /// # Safety
    /// As above.
    #[inline]
    pub unsafe fn write(&self, dst: *mut u8, len: usize, reps: usize, pattern: u64) -> u64 {
        (self.write)(dst, len, reps, pattern)
    }

    /// # Safety
    /// As above.
    #[inline]
    pub unsafe fn write_nt(&self, dst: *mut u8, len: usize, reps: usize, pattern: u64) -> u64 {
        (self.write_nt)(dst, len, reps, pattern)
    }

    /// # Safety
    /// As above.
    #[inline]
    pub unsafe fn copy(&self, src: *const u8, dst: *mut u8, len: usize, reps: usize) -> u64 {
        (self.copy)(src, dst, len, reps)
    }

    /// # Safety
    /// As above.
    #[inline]
    pub unsafe fn copy_nt(&self, src: *const u8, dst: *mut u8, len: usize, reps: usize) -> u64 {
        (self.copy_nt)(src, dst, len, reps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc::{AllocOpts, Buffer};

    #[test]
    fn at_least_scalar_and_one_simd_path_exist() {
        let avail = available();
        assert!(avail.contains(&Isa::Scalar));
        assert!(!avail.is_empty());
        assert_eq!(available().first().copied(), Some(best()));
        // Ordering: every listed path must be detectable.
        for isa in &avail {
            assert!(detect(*isa), "{isa} listed but not detected");
        }
    }

    #[test]
    fn unavailable_isa_is_an_error_not_a_downgrade() {
        let impossible = Isa::ALL
            .into_iter()
            .find(|isa| !detect(*isa))
            .unwrap_or(Isa::Scalar);
        if detect(impossible) {
            return; // every path is available on this CPU
        }
        assert!(matches!(
            Kernels::resolve(Some(impossible)),
            Err(Error::IsaUnavailable(_))
        ));
    }

    /// PLAN.md validation: "Every SIMD kernel produces the same checksum as the
    /// scalar reference on random buffers."
    #[test]
    fn every_isa_path_matches_the_scalar_checksum() {
        for len in [4096usize, 12 * 1024, 64 * 1024 + 8, 200 * 1024] {
            let mut src = Buffer::new(len, AllocOpts::default()).expect("map");
            src.fill_words(0x1234_5678 + len as u64);
            let expect = src.checksum_words();
            for isa in available() {
                let k = Kernels::for_isa(isa).expect("kernels");
                // One pass: the accumulator is an XOR, so an even number of
                // passes cancels itself out and would prove nothing.
                // SAFETY: live buffer, readable for its whole length.
                let got = unsafe { k.read(src.ptr(), src.len(), 1) };
                assert_eq!(
                    got, expect,
                    "read checksum differs for {isa} at {len} bytes"
                );
            }
        }
    }

    /// Write + copy paths must leave the same bytes behind in every ISA path.
    #[test]
    fn every_isa_path_writes_and_copies_identically() {
        let len = 48 * 1024;
        let pattern = 0xabcd_ef01_2345_6789u64;
        let mut src = Buffer::new(len, AllocOpts::default()).expect("map");
        src.fill_words(7);
        for isa in available() {
            let k = Kernels::for_isa(isa).expect("kernels");
            let mut a = Buffer::new(len, AllocOpts::default()).expect("map");
            let mut b = Buffer::new(len, AllocOpts::default()).expect("map");
            // SAFETY: live buffers of equal length, no overlap.
            unsafe {
                k.write(a.ptr_mut(), a.len(), 3, pattern);
                k.write_nt(b.ptr_mut(), b.len(), 3, pattern);
                assert_eq!(
                    a.checksum_words(),
                    b.checksum_words(),
                    "write vs write-nt differ for {isa}"
                );
                // The NT variant must have written the same constant.
                k.copy(src.ptr(), a.ptr_mut(), len, 2);
                assert_eq!(a.checksum_words(), src.checksum_words(), "copy for {isa}");
                k.copy_nt(src.ptr(), b.ptr_mut(), len, 2);
                assert_eq!(
                    b.checksum_words(),
                    src.checksum_words(),
                    "copy-nt for {isa}"
                );
            }
        }
    }

    /// A write followed by a read of every word, per ISA: catches a kernel that
    /// skips a tail (the classic way to "win" a bandwidth benchmark).
    #[test]
    fn every_write_kernel_fills_every_word() {
        let pattern = 0x5a5a_5a5a_1234_0001u64;
        for len in [4096usize, 4096 + 8, 8192 + 32] {
            for isa in available() {
                let k = Kernels::for_isa(isa).expect("kernels");
                let mut b = Buffer::new(len, AllocOpts::default()).expect("map");
                // SAFETY: live buffer.
                unsafe {
                    k.write(b.ptr_mut(), b.len(), 2, pattern);
                }
                let words = b.len() / 8;
                for i in 0..words {
                    // SAFETY: in-bounds by construction.
                    let w = unsafe { std::ptr::read_unaligned(b.ptr().add(i * 8) as *const u64) };
                    assert_eq!(w, pattern, "{isa} left word {i} of {} untouched", b.len());
                }
            }
        }
    }
}
