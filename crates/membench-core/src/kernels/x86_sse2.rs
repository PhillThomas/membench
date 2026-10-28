//! SSE2 streaming kernels: 16-byte vectors, 8 accumulators.
//!
//! On x86-64 SSE2 is part of the baseline, so this is the guaranteed SIMD path
//! and the first one checked against the scalar reference. Same register
//! convention and checksum convention as [`crate::kernels::x86_avx2`].

use core::arch::asm;

/// # Safety
/// `src..src.add(len)` readable.
#[inline]
pub unsafe fn read(src: *const u8, len: usize, reps: usize) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: loads are bounded by `src..src + (len & !7)`; the 128-byte loop
    // is bounded by `src + (len & !127)`, the 16-byte tail only runs when a
    // whole vector fits below the masked end.
    unsafe {
        asm!(
            "pxor   xmm0, xmm0",
            "pxor   xmm1, xmm1",
            "pxor   xmm2, xmm2",
            "pxor   xmm3, xmm3",
            "pxor   xmm4, xmm4",
            "pxor   xmm5, xmm5",
            "pxor   xmm6, xmm6",
            "pxor   xmm7, xmm7",
            "xor    eax, eax",
            "mov    r11, rdx",
            "and    r11, -8",
            "add    r11, rdi",
            "mov    r8, r11",              // src + (len & !7)
            "and    rdx, -128",
            "lea    r9, [rdi + rdx]",      // src + (len & !127)
            "test   r12, r12",
            "jz     9f",
            "4:",
            "mov    r10, rdi",
            "2:",
            "cmp    r10, r9",
            "jae    3f",
            "movdqu xmm8, [r10]",
            "pxor   xmm0, xmm8",
            "movdqu xmm9, [r10 + 16]",
            "pxor   xmm1, xmm9",
            "movdqu xmm10, [r10 + 32]",
            "pxor   xmm2, xmm10",
            "movdqu xmm11, [r10 + 48]",
            "pxor   xmm3, xmm11",
            "movdqu xmm12, [r10 + 64]",
            "pxor   xmm4, xmm12",
            "movdqu xmm13, [r10 + 80]",
            "pxor   xmm5, xmm13",
            "movdqu xmm14, [r10 + 96]",
            "pxor   xmm6, xmm14",
            "movdqu xmm15, [r10 + 112]",
            "pxor   xmm7, xmm15",
            "add    r10, 128",
            "cmp    r10, r9",
            "jb     2b",
            // 16-byte tail: fold the two 8-byte words of each vector by hand,
            // SSE2 has no 64-bit integer xor.
            "3:",
            "lea    r11, [r10 + 16]",
            "cmp    r11, r8",
            "ja     5f",
            "mov    rcx, [r10]",
            "xor    rax, rcx",
            "mov    rdx, [r10 + 8]",
            "xor    rax, rdx",
            "mov    r10, r11",
            "jmp    3b",
            // 8-byte remainder
            "5:",
            "lea    r11, [r10 + 8]",
            "cmp    r11, r8",
            "ja     6f",
            "mov    rcx, [r10]",
            "xor    rax, rcx",
            "mov    r10, r11",
            "jmp    5b",
            "6:",
            "dec    r12",
            "jnz    4b",
            "9:",
            "pxor   xmm0, xmm1",
            "pxor   xmm0, xmm2",
            "pxor   xmm0, xmm3",
            "pxor   xmm0, xmm4",
            "pxor   xmm0, xmm5",
            "pxor   xmm0, xmm6",
            "pxor   xmm0, xmm7",
            "movq   r11, xmm0",
            "xor    rax, r11",
            "psrldq xmm0, 8",
            "movq   r11, xmm0",
            "xor    rax, r11",
            in("rdi") src,
            in("rsi") 0usize,
            in("rdx") len,
            inlateout("r12") reps => _,
            lateout("rax") out,
            lateout("rcx") _,
            lateout("r8") _,
            lateout("r9") _,
            lateout("r10") _,
            lateout("r11") _,
            out("xmm0") _,
            out("xmm1") _,
            out("xmm2") _,
            out("xmm3") _,
            out("xmm4") _,
            out("xmm5") _,
            out("xmm6") _,
            out("xmm7") _,
            out("xmm8") _,
            out("xmm9") _,
            out("xmm10") _,
            out("xmm11") _,
            out("xmm12") _,
            out("xmm13") _,
            out("xmm14") _,
            out("xmm15") _,
            options(nostack),
        );
    }
    out
}

/// # Safety
/// `dst..dst.add(len)` writable.
#[inline]
pub unsafe fn write(dst: *mut u8, len: usize, reps: usize, pattern: u64) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: stores bounded by `dst..dst + (len & !7)`.
    unsafe {
        asm!(
            "movq   xmm0, {pat}",
            "punpcklqdq xmm0, xmm0",
            "mov    rax, {pat}",
            "mov    r11, rdx",
            "and    r11, -8",
            "add    r11, rdi",
            "mov    r8, r11",
            "and    rdx, -128",
            "lea    r9, [rdi + rdx]",
            "test   r12, r12",
            "jz     9f",
            "4:",
            "mov    r10, rdi",
            "2:",
            "cmp    r10, r9",
            "jae    3f",
            "movdqa [r10], xmm0",
            "movdqa [r10 + 16], xmm0",
            "movdqa [r10 + 32], xmm0",
            "movdqa [r10 + 48], xmm0",
            "movdqa [r10 + 64], xmm0",
            "movdqa [r10 + 80], xmm0",
            "movdqa [r10 + 96], xmm0",
            "movdqa [r10 + 112], xmm0",
            "add    r10, 128",
            "cmp    r10, r9",
            "jb     2b",
            // 32-byte tail (still 16-byte aligned: base is 2 MiB aligned)
            "3:",
            "lea    r11, [r10 + 32]",
            "cmp    r11, r8",
            "ja     5f",
            "movdqa [r10], xmm0",
            "movdqa [r10 + 16], xmm0",
            "mov    r10, r11",
            "jmp    3b",
            // 16-byte tail
            "5:",
            "lea    r11, [r10 + 16]",
            "cmp    r11, r8",
            "ja     7f",
            "movdqa [r10], xmm0",
            "mov    r10, r11",
            "jmp    5b",
            // remainder: scalar store of the pattern
            "7:",
            "lea    r11, [r10 + 8]",
            "cmp    r11, r8",
            "ja     6f",
            "mov    [r10], rax",
            "mov    r10, r11",
            "jmp    7b",
            "6:",
            "dec    r12",
            "jnz    4b",
            "9:",
            pat = in(reg) pattern,
            in("rdi") dst,
            in("rdx") len,
            inlateout("r12") reps => _,
            lateout("rax") out,
            lateout("r8") _,
            lateout("r9") _,
            lateout("r10") _,
            lateout("r11") _,
            out("xmm0") _,
            options(nostack),
        );
    }
    out
}

/// Non-temporal SSE2 stores (`movntdq`) plus `sfence`.
///
/// # Safety
/// `dst..dst.add(len)` writable and 16-byte aligned.
#[inline]
pub unsafe fn write_nt(dst: *mut u8, len: usize, reps: usize, pattern: u64) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: as for [`write`]; `movntdq` needs 16-byte alignment, guaranteed by
    // the 2 MiB-aligned base and the 16-byte stride.
    unsafe {
        asm!(
            "movq   xmm0, {pat}",
            "punpcklqdq xmm0, xmm0",
            "mov    rax, {pat}",
            "mov    r11, rdx",
            "and    r11, -8",
            "add    r11, rdi",
            "mov    r8, r11",
            "and    rdx, -128",
            "lea    r9, [rdi + rdx]",
            "test   r12, r12",
            "jz     9f",
            "4:",
            "mov    r10, rdi",
            "2:",
            "cmp    r10, r9",
            "jae    3f",
            "movntdq [r10], xmm0",
            "movntdq [r10 + 16], xmm0",
            "movntdq [r10 + 32], xmm0",
            "movntdq [r10 + 48], xmm0",
            "movntdq [r10 + 64], xmm0",
            "movntdq [r10 + 80], xmm0",
            "movntdq [r10 + 96], xmm0",
            "movntdq [r10 + 112], xmm0",
            "add    r10, 128",
            "cmp    r10, r9",
            "jb     2b",
            "3:",
            "lea    r11, [r10 + 16]",
            "cmp    r11, r8",
            "ja     5f",
            "movntdq [r10], xmm0",
            "mov    r10, r11",
            "jmp    3b",
            // remainder below one vector: normal stores
            "5:",
            "lea    r11, [r10 + 8]",
            "cmp    r11, r8",
            "ja     6f",
            "mov    [r10], {pat}",
            "mov    r10, r11",
            "jmp    5b",
            "6:",
            "dec    r12",
            "jnz    4b",
            "9:",
            "sfence",
            pat = in(reg) pattern,
            in("rdi") dst,
            in("rdx") len,
            inlateout("r12") reps => _,
            lateout("rax") out,
            lateout("r8") _,
            lateout("r9") _,
            lateout("r10") _,
            lateout("r11") _,
            out("xmm0") _,
            options(nostack),
        );
    }
    out
}

/// # Safety
/// `src..src.add(len)` readable, `dst..dst.add(len)` writable.
#[inline]
pub unsafe fn copy(src: *const u8, dst: *mut u8, len: usize, reps: usize) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: source and destination advance in lock-step, bounded by
    // `(len & !7)` bytes from their bases.
    unsafe {
        asm!(
            "mov    r11, rdx",
            "and    r11, -8",
            "add    r11, rdi",
            "mov    r8, r11",
            "and    rdx, -64",
            "lea    r9, [rdi + rdx]",
            "test   r12, r12",
            "jz     9f",
            "4:",
            "mov    r10, rdi",
            "mov    r11, rsi",
            "2:",
            "cmp    r10, r9",
            "jae    3f",
            "movdqu xmm0, [r10]",
            "movdqu [r11], xmm0",
            "movdqu xmm1, [r10 + 16]",
            "movdqu [r11 + 16], xmm1",
            "movdqu xmm2, [r10 + 32]",
            "movdqu [r11 + 32], xmm2",
            "movdqu xmm3, [r10 + 48]",
            "movdqu [r11 + 48], xmm3",
            "add    r10, 64",
            "add    r11, 64",
            "cmp    r10, r9",
            "jb     2b",
            "3:",
            "lea    rcx, [r10 + 16]",
            "cmp    rcx, r8",
            "ja     5f",
            "movdqu xmm0, [r10]",
            "movdqu [r11], xmm0",
            "mov    r10, rcx",
            "add    r11, 16",
            "jmp    3b",
            "5:",
            "lea    rcx, [r10 + 8]",
            "cmp    rcx, r8",
            "ja     6f",
            "mov    rdx, [r10]",
            "mov    [r11], rdx",
            "mov    r10, rcx",
            "add    r11, 8",
            "jmp    5b",
            "6:",
            "dec    r12",
            "jnz    4b",
            "9:",
            "xor    eax, eax",
            in("rdi") src,
            in("rsi") dst,
            in("rdx") len,
            inlateout("r12") reps => _,
            lateout("rax") out,
            lateout("rcx") _,
            lateout("r8") _,
            lateout("r9") _,
            lateout("r10") _,
            lateout("r11") _,
            out("xmm0") _,
            out("xmm1") _,
            out("xmm2") _,
            out("xmm3") _,
            options(nostack),
        );
    }
    out
}

/// # Safety
/// `src..src.add(len)` readable, `dst..dst.add(len)` writable and 16-byte
/// aligned.
#[inline]
pub unsafe fn copy_nt(src: *const u8, dst: *mut u8, len: usize, reps: usize) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: as for [`copy`]; `movntdq` targets are 16-byte aligned.
    unsafe {
        asm!(
            "mov    r11, rdx",
            "and    r11, -8",
            "add    r11, rdi",
            "mov    r8, r11",
            "and    rdx, -64",
            "lea    r9, [rdi + rdx]",
            "test   r12, r12",
            "jz     9f",
            "4:",
            "mov    r10, rdi",
            "mov    r11, rsi",
            "2:",
            "cmp    r10, r9",
            "jae    3f",
            "movdqu xmm0, [r10]",
            "movntdq [r11], xmm0",
            "movdqu xmm1, [r10 + 16]",
            "movntdq [r11 + 16], xmm1",
            "movdqu xmm2, [r10 + 32]",
            "movntdq [r11 + 32], xmm2",
            "movdqu xmm3, [r10 + 48]",
            "movntdq [r11 + 48], xmm3",
            "add    r10, 64",
            "add    r11, 64",
            "cmp    r10, r9",
            "jb     2b",
            "3:",
            "lea    rcx, [r10 + 16]",
            "cmp    rcx, r8",
            "ja     5f",
            "movdqu xmm0, [r10]",
            "movntdq [r11], xmm0",
            "mov    r10, rcx",
            "add    r11, 16",
            "jmp    3b",
            "5:",
            "lea    rcx, [r10 + 8]",
            "cmp    rcx, r8",
            "ja     6f",
            "mov    rdx, [r10]",
            "mov    [r11], rdx",
            "mov    r10, rcx",
            "add    r11, 8",
            "jmp    5b",
            "6:",
            "dec    r12",
            "jnz    4b",
            "9:",
            "sfence",
            "xor    eax, eax",
            in("rdi") src,
            in("rsi") dst,
            in("rdx") len,
            inlateout("r12") reps => _,
            lateout("rax") out,
            lateout("rcx") _,
            lateout("r8") _,
            lateout("r9") _,
            lateout("r10") _,
            lateout("r11") _,
            out("xmm0") _,
            out("xmm1") _,
            out("xmm2") _,
            out("xmm3") _,
            options(nostack),
        );
    }
    out
}
