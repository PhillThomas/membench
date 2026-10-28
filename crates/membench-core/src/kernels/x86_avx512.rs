//! AVX-512 streaming kernels: 64-byte vectors, 8 accumulators.
//!
//! Only AVX512F is required (all the instructions here are `vpxorq`,
//! `vmovdqu*`/`vmovntdq` and `vpbroadcastq` on zmm). Some chips downclock under
//! AVX-512, which is why `--isa` exists (PLAN.md: "AVX-512 is slower than AVX2
//! on some chips").
//!
//! Same register and checksum conventions as [`crate::kernels::x86_avx2`].

use core::arch::asm;

/// # Safety
/// `src..src.add(len)` readable.
#[inline]
#[target_feature(enable = "avx512f")]
pub unsafe fn read(src: *const u8, len: usize, reps: usize) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: loads bounded by `src..src + (len & !7)`; the unrolled loop is
    // bounded by `src + (len & !511)`.
    unsafe {
        asm!(
            "vpxorq xmm0, xmm0, xmm0",
            "vpxorq xmm1, xmm1, xmm1",
            "vpxorq xmm2, xmm2, xmm2",
            "vpxorq xmm3, xmm3, xmm3",
            "vpxorq xmm4, xmm4, xmm4",
            "vpxorq xmm5, xmm5, xmm5",
            "vpxorq xmm6, xmm6, xmm6",
            "vpxorq xmm7, xmm7, xmm7",
            "xor    eax, eax",
            "mov    r11, rdx",
            "and    r11, -8",
            "add    r11, rdi",
            "mov    r8, r11",              // src + (len & !7)
            "and    rdx, -512",
            "lea    r9, [rdi + rdx]",      // src + (len & !511)
            "test   r12, r12",
            "jz     9f",
            "4:",
            "mov    r10, rdi",
            "2:",
            "cmp    r10, r9",
            "jae    3f",
            "vpxorq zmm0, zmm0, [r10]",
            "vpxorq zmm1, zmm1, [r10 + 64]",
            "vpxorq zmm2, zmm2, [r10 + 128]",
            "vpxorq zmm3, zmm3, [r10 + 192]",
            "vpxorq zmm4, zmm4, [r10 + 256]",
            "vpxorq zmm5, zmm5, [r10 + 320]",
            "vpxorq zmm6, zmm6, [r10 + 384]",
            "vpxorq zmm7, zmm7, [r10 + 448]",
            "add    r10, 512",
            "cmp    r10, r9",
            "jb     2b",
            // 64-byte tail
            "3:",
            "lea    r11, [r10 + 64]",
            "cmp    r11, r8",
            "ja     5f",
            "vpxorq zmm0, zmm0, [r10]",
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
            "vpxorq zmm0, zmm0, zmm1",
            "vpxorq zmm0, zmm0, zmm2",
            "vpxorq zmm0, zmm0, zmm3",
            "vpxorq zmm0, zmm0, zmm4",
            "vpxorq zmm0, zmm0, zmm5",
            "vpxorq zmm0, zmm0, zmm6",
            "vpxorq zmm0, zmm0, zmm7",
            "vextracti64x4 ymm1, zmm0, 1",
            "vpxor  ymm0, ymm0, ymm1",
            "vextracti128 xmm1, ymm0, 1",
            "vpxor  xmm0, xmm0, xmm1",
            "vpextrq r11, xmm0, 0",
            "xor    rax, r11",
            "vpextrq r11, xmm0, 1",
            "xor    rax, r11",
            "vzeroupper",
            in("rdi") src,
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
            options(nostack),
        );
    }
    out
}

/// # Safety
/// `dst..dst.add(len)` writable.
#[inline]
#[target_feature(enable = "avx512f")]
pub unsafe fn write(dst: *mut u8, len: usize, reps: usize, pattern: u64) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: stores bounded by `dst..dst + (len & !7)`.
    unsafe {
        asm!(
            "vmovq    xmm1, {pat}",
            "vpbroadcastq zmm0, xmm1",
            "mov    rax, {pat}",
            "mov    r11, rdx",
            "and    r11, -8",
            "add    r11, rdi",
            "mov    r8, r11",
            "and    rdx, -512",
            "lea    r9, [rdi + rdx]",
            "test   r12, r12",
            "jz     9f",
            "4:",
            "mov    r10, rdi",
            "2:",
            "cmp    r10, r9",
            "jae    3f",
            "vmovdqa64 [r10], zmm0",
            "vmovdqa64 [r10 + 64], zmm0",
            "vmovdqa64 [r10 + 128], zmm0",
            "vmovdqa64 [r10 + 192], zmm0",
            "vmovdqa64 [r10 + 256], zmm0",
            "vmovdqa64 [r10 + 320], zmm0",
            "vmovdqa64 [r10 + 384], zmm0",
            "vmovdqa64 [r10 + 448], zmm0",
            "add    r10, 512",
            "cmp    r10, r9",
            "jb     2b",
            // 64-byte tail (aligned: base is 2 MiB aligned, stride 64)
            "3:",
            "lea    r11, [r10 + 64]",
            "cmp    r11, r8",
            "ja     5f",
            "vmovdqa64 [r10], zmm0",
            "mov    r10, r11",
            "jmp    3b",
            // remainder: scalar stores
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
            "vzeroupper",
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
            out("xmm1") _,
            options(nostack),
        );
    }
    out
}

/// # Safety
/// `dst..dst.add(len)` writable and 64-byte aligned.
#[inline]
#[target_feature(enable = "avx512f")]
pub unsafe fn write_nt(dst: *mut u8, len: usize, reps: usize, pattern: u64) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: as for [`write`]; `vmovntdq` on zmm requires 64-byte alignment,
    // provided by the 2 MiB-aligned base and the 64-byte stride.
    unsafe {
        asm!(
            "vmovq    xmm1, {pat}",
            "vpbroadcastq zmm0, xmm1",
            "mov    rax, {pat}",
            "mov    r11, rdx",
            "and    r11, -8",
            "add    r11, rdi",
            "mov    r8, r11",
            "and    rdx, -512",
            "lea    r9, [rdi + rdx]",
            "test   r12, r12",
            "jz     9f",
            "4:",
            "mov    r10, rdi",
            "2:",
            "cmp    r10, r9",
            "jae    3f",
            "vmovntdq [r10], zmm0",
            "vmovntdq [r10 + 64], zmm0",
            "vmovntdq [r10 + 128], zmm0",
            "vmovntdq [r10 + 192], zmm0",
            "vmovntdq [r10 + 256], zmm0",
            "vmovntdq [r10 + 320], zmm0",
            "vmovntdq [r10 + 384], zmm0",
            "vmovntdq [r10 + 448], zmm0",
            "add    r10, 512",
            "cmp    r10, r9",
            "jb     2b",
            "3:",
            "lea    r11, [r10 + 64]",
            "cmp    r11, r8",
            "ja     5f",
            "vmovntdq [r10], zmm0",
            "mov    r10, r11",
            "jmp    3b",
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
            "vzeroupper",
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
            out("xmm1") _,
            options(nostack),
        );
    }
    out
}

/// # Safety
/// `src..src.add(len)` readable, `dst..dst.add(len)` writable.
#[inline]
#[target_feature(enable = "avx512f")]
pub unsafe fn copy(src: *const u8, dst: *mut u8, len: usize, reps: usize) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: both sides advance in lock-step, bounded by `(len & !7)`.
    unsafe {
        asm!(
            "mov    r11, rdx",
            "and    r11, -8",
            "add    r11, rdi",
            "mov    r8, r11",
            "and    rdx, -256",
            "lea    r9, [rdi + rdx]",
            "test   r12, r12",
            "jz     9f",
            "4:",
            "mov    r10, rdi",
            "mov    r11, rsi",
            "2:",
            "cmp    r10, r9",
            "jae    3f",
            "vmovdqu64 zmm0, [r10]",
            "vmovdqu64 [r11], zmm0",
            "vmovdqu64 zmm1, [r10 + 64]",
            "vmovdqu64 [r11 + 64], zmm1",
            "vmovdqu64 zmm2, [r10 + 128]",
            "vmovdqu64 [r11 + 128], zmm2",
            "vmovdqu64 zmm3, [r10 + 192]",
            "vmovdqu64 [r11 + 192], zmm3",
            "add    r10, 256",
            "add    r11, 256",
            "cmp    r10, r9",
            "jb     2b",
            "3:",
            "lea    rcx, [r10 + 64]",
            "cmp    rcx, r8",
            "ja     5f",
            "vmovdqu64 zmm0, [r10]",
            "vmovdqu64 [r11], zmm0",
            "mov    r10, rcx",
            "add    r11, 64",
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
            "vzeroupper",
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
/// `src..src.add(len)` readable, `dst..dst.add(len)` writable and 64-byte
/// aligned.
#[inline]
#[target_feature(enable = "avx512f")]
pub unsafe fn copy_nt(src: *const u8, dst: *mut u8, len: usize, reps: usize) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: as for [`copy`]; NT stores target 64-byte aligned addresses.
    unsafe {
        asm!(
            "mov    r11, rdx",
            "and    r11, -8",
            "add    r11, rdi",
            "mov    r8, r11",
            "and    rdx, -256",
            "lea    r9, [rdi + rdx]",
            "test   r12, r12",
            "jz     9f",
            "4:",
            "mov    r10, rdi",
            "mov    r11, rsi",
            "2:",
            "cmp    r10, r9",
            "jae    3f",
            "vmovdqu64 zmm0, [r10]",
            "vmovntdq [r11], zmm0",
            "vmovdqu64 zmm1, [r10 + 64]",
            "vmovntdq [r11 + 64], zmm1",
            "vmovdqu64 zmm2, [r10 + 128]",
            "vmovntdq [r11 + 128], zmm2",
            "vmovdqu64 zmm3, [r10 + 192]",
            "vmovntdq [r11 + 192], zmm3",
            "add    r10, 256",
            "add    r11, 256",
            "cmp    r10, r9",
            "jb     2b",
            "3:",
            "lea    rcx, [r10 + 64]",
            "cmp    rcx, r8",
            "ja     5f",
            "vmovdqu64 zmm0, [r10]",
            "vmovntdq [r11], zmm0",
            "mov    r10, rcx",
            "add    r11, 64",
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
            "vzeroupper",
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
