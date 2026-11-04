//! AArch64 NEON streaming kernels: 16-byte vectors, 8 accumulators.
//!
//! NEON is mandatory on AArch64, so no feature detection is needed. Same
//! checksum convention as the x86 paths: XOR of every 8-byte word, so the
//! cross-ISA checksum tests compare apples with apples.
//!
//! This path is compiled for aarch64 only. It cannot be measured on the x86-64
//! development box at all, so it is checked two ways instead: `cargo check
//! --target aarch64-unknown-linux-gnu` runs in CI (a previous version of this
//! file did not compile, and nothing noticed because nothing built it), and the
//! kernels are compared against the scalar reference by `membench self-test` on
//! real aarch64 hardware.

use core::arch::asm;

/// # Safety
/// `src..src.add(len)` readable.
#[inline]
pub unsafe fn read(src: *const u8, len: usize, reps: usize) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: `x9` is `src + (len & !7)`, the effective end; the unrolled loop
    // stops at `src + (len & !127)` and both tails only run while the whole
    // block fits below `x9`.
    unsafe {
        asm!(
            "movi   v0.16b, #0",
            "movi   v1.16b, #0",
            "movi   v2.16b, #0",
            "movi   v3.16b, #0",
            "movi   v4.16b, #0",
            "movi   v5.16b, #0",
            "movi   v6.16b, #0",
            "movi   v7.16b, #0",
            "bic    x9, x1, #7",
            "add    x9, x9, x0",
            "bic    x10, x1, #127",
            "add    x10, x10, x0",
            "cbz    x2, 9f",
            // one pass over the buffer
            "4:",
            "mov    x3, x0",
            // 128-byte blocks, 8 independent chains
            "2:",
            "cmp    x3, x10",
            "b.cs   3f",
            "ldr    q8, [x3]",
            "eor    v0.16b, v0.16b, v8.16b",
            "ldr    q9, [x3, #16]",
            "eor    v1.16b, v1.16b, v9.16b",
            "ldr    q10, [x3, #32]",
            "eor    v2.16b, v2.16b, v10.16b",
            "ldr    q11, [x3, #48]",
            "eor    v3.16b, v3.16b, v11.16b",
            "ldr    q12, [x3, #64]",
            "eor    v4.16b, v4.16b, v12.16b",
            "ldr    q13, [x3, #80]",
            "eor    v5.16b, v5.16b, v13.16b",
            "ldr    q14, [x3, #96]",
            "eor    v6.16b, v6.16b, v14.16b",
            "ldr    q15, [x3, #112]",
            "eor    v7.16b, v7.16b, v15.16b",
            "add    x3, x3, #128",
            "cmp    x3, x10",
            "b.cc   2b",
            // 16-byte tail
            "3:",
            "add    x4, x3, #16",
            "cmp    x4, x9",
            "b.hi   5f",
            "ldr    q8, [x3]",
            "eor    v0.16b, v0.16b, v8.16b",
            "mov    x3, x4",
            "b      3b",
            // 8-byte remainder
            "5:",
            "add    x4, x3, #8",
            "cmp    x4, x9",
            "b.hi   6f",
            "ldr    x6, [x3]",
            "mov    v8.d[0], x6",
            "eor    v0.16b, v0.16b, v8.16b",
            "mov    x3, x4",
            "b      5b",
            "6:",
            "subs   x2, x2, #1",
            "b.ne   4b",
            // fold
            "9:",
            "eor    v0.16b, v0.16b, v1.16b",
            "eor    v0.16b, v0.16b, v2.16b",
            "eor    v0.16b, v0.16b, v3.16b",
            "eor    v0.16b, v0.16b, v4.16b",
            "eor    v0.16b, v0.16b, v5.16b",
            "eor    v0.16b, v0.16b, v6.16b",
            "eor    v0.16b, v0.16b, v7.16b",
            "umov   x0, v0.d[0]",
            "umov   x11, v0.d[1]",
            "eor    x0, x0, x11",
            inlateout("x0") src => out,
            in("x1") len,
            in("x2") reps,
            lateout("x3") _,
            lateout("x4") _,
            lateout("x6") _,
            lateout("x9") _,
            lateout("x10") _,
            lateout("x11") _,
            out("v0") _,
            out("v1") _,
            out("v2") _,
            out("v3") _,
            out("v4") _,
            out("v5") _,
            out("v6") _,
            out("v7") _,
            out("v8") _,
            out("v9") _,
            out("v10") _,
            out("v11") _,
            out("v12") _,
            out("v13") _,
            out("v14") _,
            out("v15") _,
            options(nostack),
        )
    }
    out
}

/// # Safety
/// `dst..dst.add(len)` writable.
#[inline]
pub unsafe fn write(dst: *mut u8, len: usize, reps: usize, pattern: u64) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: stores bounded by `dst + (len & !7)`.
    unsafe {
        asm!(
            "dup    v0.2d, x4",
            "bic    x9, x1, #7",
            "add    x9, x9, x0",
            "bic    x10, x1, #63",
            "add    x10, x10, x0",
            "cbz    x2, 9f",
            "4:",
            "mov    x3, x0",
            "2:",
            "cmp    x3, x10",
            "b.cs   3f",
            "str    q0, [x3]",
            "str    q0, [x3, #16]",
            "str    q0, [x3, #32]",
            "str    q0, [x3, #48]",
            "add    x3, x3, #64",
            "cmp    x3, x10",
            "b.cc   2b",
            "3:",
            "add    x5, x3, #16",
            "cmp    x5, x9",
            "b.hi   5f",
            "str    q0, [x3]",
            "mov    x3, x5",
            "b      3b",
            "5:",
            "add    x5, x3, #8",
            "cmp    x5, x9",
            "b.hi   6f",
            "str    d0, [x3]",
            "mov    x3, x5",
            "b      5b",
            "6:",
            "subs   x2, x2, #1",
            "b.ne   4b",
            "9:",
            "mov    x8, x4",
            in("x0") dst,
            in("x1") len,
            in("x2") reps,
            in("x4") pattern,
            lateout("x8") out,
            lateout("x3") _,
            lateout("x5") _,
            lateout("x9") _,
            lateout("x10") _,
            out("v0") _,
            options(nostack),
        )
    }
    out
}

/// Non-temporal writes: `stnp` (store non-temporal pair) with a `dsb ishst`
/// before the timed window closes.
///
/// # Safety
/// `dst..dst.add(len)` writable.
#[inline]
pub unsafe fn write_nt(dst: *mut u8, len: usize, reps: usize, pattern: u64) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: as for [`write`]; `stnp` needs 8-byte alignment, satisfied because
    // the base is 2 MiB aligned and every stride is a multiple of 8.
    unsafe {
        asm!(
            "dup    v0.2d, x4",
            "bic    x9, x1, #7",
            "add    x9, x9, x0",
            "bic    x10, x1, #63",
            "add    x10, x10, x0",
            "cbz    x2, 9f",
            "4:",
            "mov    x3, x0",
            "2:",
            "cmp    x3, x10",
            "b.cs   3f",
            "stnp   q0, q0, [x3]",
            "stnp   q0, q0, [x3, #32]",
            "add    x3, x3, #64",
            "cmp    x3, x10",
            "b.cc   2b",
            "3:",
            "add    x5, x3, #32",
            "cmp    x5, x9",
            "b.hi   5f",
            "stnp   q0, q0, [x3]",
            "mov    x3, x5",
            "b      3b",
            "5:",
            "add    x5, x3, #8",
            "cmp    x5, x9",
            "b.hi   6f",
            "str    d0, [x3]",
            "mov    x3, x5",
            "b      5b",
            "6:",
            "subs   x2, x2, #1",
            "b.ne   4b",
            "9:",
            "dsb    ishst",
            "mov    x8, x4",
            in("x0") dst,
            in("x1") len,
            in("x2") reps,
            in("x4") pattern,
            lateout("x8") out,
            lateout("x3") _,
            lateout("x5") _,
            lateout("x9") _,
            lateout("x10") _,
            out("v0") _,
            options(nostack),
        )
    }
    out
}

/// # Safety
/// `src..src.add(len)` readable, `dst..dst.add(len)` writable.
#[inline]
pub unsafe fn copy(src: *const u8, dst: *mut u8, len: usize, reps: usize) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: `x9` is the effective source end; both sides advance in
    // lock-step, so the destination stays inside `dst..dst+len`.
    unsafe {
        asm!(
            "bic    x9, x2, #7",
            "add    x9, x9, x0",
            "bic    x10, x2, #63",
            "add    x10, x10, x0",
            "cbz    x3, 9f",
            "4:",
            "mov    x4, x0",
            "mov    x5, x1",
            "2:",
            "cmp    x4, x10",
            "b.cs   3f",
            "ldr    q0, [x4]",
            "str    q0, [x5]",
            "ldr    q1, [x4, #16]",
            "str    q1, [x5, #16]",
            "ldr    q2, [x4, #32]",
            "str    q2, [x5, #32]",
            "ldr    q3, [x4, #48]",
            "str    q3, [x5, #48]",
            "add    x4, x4, #64",
            "add    x5, x5, #64",
            "cmp    x4, x10",
            "b.cc   2b",
            "3:",
            "add    x6, x4, #16",
            "cmp    x6, x9",
            "b.hi   5f",
            "ldr    q0, [x4]",
            "str    q0, [x5]",
            "mov    x4, x6",
            "add    x5, x5, #16",
            "b      3b",
            "5:",
            "add    x6, x4, #8",
            "cmp    x6, x9",
            "b.hi   6f",
            "ldr    x7, [x4]",
            "str    x7, [x5]",
            "mov    x4, x6",
            "add    x5, x5, #8",
            "b      5b",
            "6:",
            "subs   x3, x3, #1",
            "b.ne   4b",
            "9:",
            "mov    x8, xzr",
            in("x0") src,
            in("x1") dst,
            in("x2") len,
            in("x3") reps,
            lateout("x8") out,
            lateout("x4") _,
            lateout("x5") _,
            lateout("x6") _,
            lateout("x7") _,
            lateout("x9") _,
            lateout("x10") _,
            out("v0") _,
            out("v1") _,
            out("v2") _,
            out("v3") _,
            options(nostack),
        )
    }
    out
}

/// # Safety
/// `src..src.add(len)` readable, `dst..dst.add(len)` writable and 8-byte
/// aligned.
#[inline]
pub unsafe fn copy_nt(src: *const u8, dst: *mut u8, len: usize, reps: usize) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: as for [`copy`]; `ldp`/`stnp` need 8-byte alignment, provided by
    // the 2 MiB-aligned bases and the 8-byte minimum stride.
    unsafe {
        asm!(
            "bic    x9, x2, #7",
            "add    x9, x9, x0",
            "bic    x10, x2, #63",
            "add    x10, x10, x0",
            "cbz    x3, 9f",
            "4:",
            "mov    x4, x0",
            "mov    x5, x1",
            "2:",
            "cmp    x4, x10",
            "b.cs   3f",
            "ldp    q0, q1, [x4]",
            "stnp   q0, q1, [x5]",
            "ldp    q2, q3, [x4, #32]",
            "stnp   q2, q3, [x5, #32]",
            "add    x4, x4, #64",
            "add    x5, x5, #64",
            "cmp    x4, x10",
            "b.cc   2b",
            "3:",
            "add    x6, x4, #16",
            "cmp    x6, x9",
            "b.hi   5f",
            "ldr    q0, [x4]",
            "str    q0, [x5]",
            "mov    x4, x6",
            "add    x5, x5, #16",
            "b      3b",
            "5:",
            "add    x6, x4, #8",
            "cmp    x6, x9",
            "b.hi   6f",
            "ldr    x7, [x4]",
            "str    x7, [x5]",
            "mov    x4, x6",
            "add    x5, x5, #8",
            "b      5b",
            "6:",
            "subs   x3, x3, #1",
            "b.ne   4b",
            "9:",
            "dsb    ishst",
            "mov    x8, xzr",
            in("x0") src,
            in("x1") dst,
            in("x2") len,
            in("x3") reps,
            lateout("x8") out,
            lateout("x4") _,
            lateout("x5") _,
            lateout("x6") _,
            lateout("x7") _,
            lateout("x9") _,
            lateout("x10") _,
            out("v0") _,
            out("v1") _,
            out("v2") _,
            out("v3") _,
            options(nostack),
        )
    }
    out
}
