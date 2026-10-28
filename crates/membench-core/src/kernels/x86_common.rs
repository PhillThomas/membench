//! x86-only extras: `rep movsb` / `rep stosb` variants and the TSC.
//!
//! The `rep string` variants are the "optional" kernels from PLAN.md: fast on
//! CPUs with ERMSB/FSRM, and reported in the detail view rather than the grid,
//! because their numbers are not comparable with the SIMD kernels (the store
//! buffer and the microcode loop, not the load/store ports, are the limit).

use core::arch::asm;

/// `rep movsb` copy of one buffer, `reps` times.
///
/// # Safety
/// `src..src.add(len)` readable, `dst..dst.add(len)` writable, no overlap.
#[inline]
pub unsafe fn rep_movsb(src: *const u8, dst: *mut u8, len: usize, reps: usize) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: `rep movsb` copies exactly `len` bytes between two non-overlapping
    // live mappings; the Direction Flag is clear per the Rust ABI, so no `cld`
    // is needed. Registers the instruction walks over (`rdi`, `rsi`, `rcx`) are
    // reloaded from the saved bases on every repetition.
    unsafe {
        asm!(
            "test   r12, r12",
            "jz     9f",
            "4:",
            "mov    rdi, r9",
            "mov    rsi, r10",
            "mov    rcx, r8",
            "rep movsb",
            "dec    r12",
            "jnz    4b",
            "9:",
            "mov    rax, r8",
            in("r8") len,
            in("r9") dst,
            in("r10") src,
            inlateout("r12") reps => _,
            lateout("rax") out,
            lateout("rcx") _,
            lateout("rdi") _,
            lateout("rsi") _,
            options(nostack),
        );
    }
    out
}

/// `rep stosb` fill of one buffer, `reps` times.
///
/// # Safety
/// `dst..dst.add(len)` writable.
#[inline]
pub unsafe fn rep_stosb(dst: *mut u8, len: usize, reps: usize, fill: u8) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: `rep stosb` writes exactly `len` bytes into a live mapping; `rep
    // stosb` fills with AL, so the byte is masked from the operand register.
    unsafe {
        asm!(
            "movzx  eax, dl",
            "test   r12, r12",
            "jz     9f",
            "4:",
            "mov    rdi, r9",
            "mov    rcx, r8",
            "rep stosb",
            "dec    r12",
            "jnz    4b",
            "9:",
            "mov    rax, r8",
            in("r8") len,
            in("r9") dst,
            in("rdx") fill as u64,
            inlateout("r12") reps => _,
            lateout("rax") out,
            lateout("rcx") _,
            lateout("rdi") _,
            options(nostack),
        );
    }
    out
}

/// `rdtsc`, serialised by a preceding `lfence` so it cannot move earlier than
/// the surrounding code.
///
/// # Safety
/// Requires the TSC feature; check [`tsc_available`] first.
#[inline]
pub unsafe fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: `rdtsc` only reads the time stamp counter; `lfence` serialises it.
    // Flags are clobbered, memory is not touched.
    unsafe {
        asm!("lfence", "rdtsc", lateout("eax") lo, lateout("edx") hi, options(nostack));
    }
    ((hi as u64) << 32) | lo as u64
}

/// Is the TSC usable for timing? CPUID.1:EDX[4] is the architectural answer;
/// some hypervisors clear it or leave the counter stopped.
pub fn tsc_available() -> bool {
    raw_cpuid::CpuId::new()
        .get_feature_info()
        .map(|f| f.has_tsc())
        .unwrap_or(false)
}
