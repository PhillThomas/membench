//! AVX2 streaming kernels: 32-byte vectors, 8 accumulators, 8x unrolled.
//!
//! Inner loops live entirely in `asm!` (PLAN.md rule 1): LLVM recognises simple
//! store and copy loops written in safe Rust and replaces them with
//! `memset`/`memcpy` calls, which would silently measure libc instead of the
//! memory system.
//!
//! Register convention used by every block (the copy kernels need callee-saved
//! `r12`, so Rust saves and restores it around the block):
//!
//! | reg | role |
//! | --- | --- |
//! | `rdi` | source / destination base (never modified) |
//! | `rsi` | second base (copy destination) or length scratch |
//! | `rdx` | `len` (consumed into the two end pointers) / scratch |
//! | `rcx` | scratch |
//! | `r8`  | effective region end = base + (`len` & !7) |
//! | `r9`  | end of the 8x-unrolled region |
//! | `r10` | walk pointer, source side |
//! | `r11` | walk pointer, destination side / scratch |
//! | `rax` | scalar accumulator, then the result |
//! | `r12` | `reps` countdown |
//!
//! Checksum convention: the XOR of every 8-byte word of the covered region.
//! Fewer-than-8 trailing bytes are never read or written, in this or any other
//! path, which is what makes all ISA paths' checksums directly comparable.

use core::arch::asm;

/// Read kernel: XOR-fold the buffer into 8 independent vector accumulators,
/// `reps` times, so no dependency chain limits load throughput (PLAN.md
/// "Kernels"). The final value goes to `std::hint::black_box` in the runner.
///
/// # Safety
/// `src..src.add(len)` must be readable and `reps` must be a sane count (the
/// wrapper clamps it to at least 1).
#[inline]
#[target_feature(enable = "avx2")]
pub unsafe fn read(src: *const u8, len: usize, reps: usize) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: every load address is in `src..src + (len & !7)`: the unrolled
    // loop is bounded by `src + (len & !255)` (a multiple of 256, hence of 8),
    // the 32- and 8-byte tails only run when the whole block fits below the
    // masked end. Only declared registers are written; no stack, no flags the
    // caller owns.
    unsafe {
        asm!(
            "vpxor  xmm0, xmm0, xmm0",
            "vpxor  xmm1, xmm1, xmm1",
            "vpxor  xmm2, xmm2, xmm2",
            "vpxor  xmm3, xmm3, xmm3",
            "vpxor  xmm4, xmm4, xmm4",
            "vpxor  xmm5, xmm5, xmm5",
            "vpxor  xmm6, xmm6, xmm6",
            "vpxor  xmm7, xmm7, xmm7",
            "xor    eax, eax",
            "mov    r11, rdx",
            "and    r11, -8",
            "add    r11, rdi",
            "mov    r8, r11",              // r8 = src + (len & !7)
            "and    rdx, -256",
            "lea    r9, [rdi + rdx]",      // r9 = src + (len & !255)
            "test   r12, r12",
            "jz     9f",
            // --- one pass over the buffer -------------------------------
            "4:",
            "mov    r10, rdi",
            // --- 256-byte blocks, 8 independent chains ------------------
            "2:",
            "cmp    r10, r9",
            "jae    3f",
            "vpxor  ymm0, ymm0, [r10]",
            "vpxor  ymm1, ymm1, [r10 + 32]",
            "vpxor  ymm2, ymm2, [r10 + 64]",
            "vpxor  ymm3, ymm3, [r10 + 96]",
            "vpxor  ymm4, ymm4, [r10 + 128]",
            "vpxor  ymm5, ymm5, [r10 + 160]",
            "vpxor  ymm6, ymm6, [r10 + 192]",
            "vpxor  ymm7, ymm7, [r10 + 224]",
            "add    r10, 256",
            "cmp    r10, r9",
            "jb     2b",
            // --- 32-byte tail ------------------------------------------
            "3:",
            "lea    r11, [r10 + 32]",
            "cmp    r11, r8",
            "ja     5f",
            "vpxor  ymm0, ymm0, [r10]",
            "mov    r10, r11",
            "jmp    3b",
            // --- 8-byte tail -------------------------------------------
            "5:",
            "lea    r11, [r10 + 8]",
            "cmp    r11, r8",
            "ja     6f",
            "mov    rcx, [r10]",
            "xor    rax, rcx",
            "mov    r10, r11",
            "jmp    5b",
            // --- end of pass -------------------------------------------
            "6:",
            "dec    r12",
            "jnz    4b",
            // fold the eight accumulators into one 64-bit value
            "9:",
            "vpxor  ymm0, ymm0, ymm1",
            "vpxor  ymm0, ymm0, ymm2",
            "vpxor  ymm0, ymm0, ymm3",
            "vpxor  ymm0, ymm0, ymm4",
            "vpxor  ymm0, ymm0, ymm5",
            "vpxor  ymm0, ymm0, ymm6",
            "vpxor  ymm0, ymm0, ymm7",
            "vextracti128 xmm1, ymm0, 1",
            "vpxor  xmm0, xmm0, xmm1",
            "vpextrq r11, xmm0, 0",
            "xor    rax, r11",
            "vpextrq r11, xmm0, 1",
            "xor    rax, r11",
            "vzeroupper",
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
            // `nomem` is deliberately *not* set: the loads must stay inside the
            // timed region and must not be folded away.
            options(nostack),
        );
    }
    out
}

/// Write kernel with normal stores: fill the buffer with `pattern`.
///
/// Normal stores pay a read-for-ownership; for cache levels that is what the
/// "Write" cell means, and for main memory the non-temporal variant
/// ([`write_nt`]) is measured alongside it and the convention is printed.
///
/// # Safety
/// `dst..dst.add(len)` must be writable.
#[inline]
#[target_feature(enable = "avx2")]
pub unsafe fn write(dst: *mut u8, len: usize, reps: usize, pattern: u64) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: stores are confined to `dst..dst + (len & !7)` by the same
    // bounding argument as in [`read`].
    unsafe {
        asm!(
            "vmovq    xmm0, {pat}",
            "vpbroadcastq ymm0, xmm0",
            "mov    rax, {pat}",
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
            "2:",
            "cmp    r10, r9",
            "jae    3f",
            "vmovdqa [r10], ymm0",
            "vmovdqa [r10 + 32], ymm0",
            "vmovdqa [r10 + 64], ymm0",
            "vmovdqa [r10 + 96], ymm0",
            "vmovdqa [r10 + 128], ymm0",
            "vmovdqa [r10 + 160], ymm0",
            "vmovdqa [r10 + 192], ymm0",
            "vmovdqa [r10 + 224], ymm0",
            "add    r10, 256",
            "cmp    r10, r9",
            "jb     2b",
            "3:",
            "lea    r11, [r10 + 32]",
            "cmp    r11, r8",
            "ja     5f",
            "vmovdqa [r10], ymm0",
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
            "vzeroupper",
            pat = in(reg) pattern,
            in("rdi") dst,
            in("rsi") 0usize,
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

/// Write kernel with non-temporal stores (`vmovntdq`) and one trailing
/// `sfence`, so the timed window cannot close while stores are still in
/// flight.
///
/// # Safety
/// `dst..dst.add(len)` must be writable and 32-byte aligned. All membench
/// buffers are 2 MiB aligned and every block offset is a multiple of 32, so
/// every address written here is aligned.
#[inline]
#[target_feature(enable = "avx2")]
pub unsafe fn write_nt(dst: *mut u8, len: usize, reps: usize, pattern: u64) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: as for [`write`]; the alignment requirement of `vmovntdq` is met
    // by the base pointer (2 MiB aligned) and the 32-byte block stride. A
    // remainder below 32 bytes falls through to the scalar (normal-store) tail.
    unsafe {
        asm!(
            "vmovq    xmm0, {pat}",
            "vpbroadcastq ymm0, xmm0",
            "mov    rax, {pat}",
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
            "2:",
            "cmp    r10, r9",
            "jae    3f",
            "vmovntdq [r10], ymm0",
            "vmovntdq [r10 + 32], ymm0",
            "vmovntdq [r10 + 64], ymm0",
            "vmovntdq [r10 + 96], ymm0",
            "vmovntdq [r10 + 128], ymm0",
            "vmovntdq [r10 + 160], ymm0",
            "vmovntdq [r10 + 192], ymm0",
            "vmovntdq [r10 + 224], ymm0",
            "add    r10, 256",
            "cmp    r10, r9",
            "jb     2b",
            "3:",
            "lea    r11, [r10 + 32]",
            "cmp    r11, r8",
            "ja     5f",
            "vmovntdq [r10], ymm0",
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
            in("rsi") 0usize,
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

/// Copy kernel, normal stores: `vmovdqa` load + store, 4 pairs per 128-byte
/// block.
///
/// The return value is a scheduling sink, not a checksum: a copy moves bytes
/// rather than reducing them, so the tests verify a copy by reading the
/// destination back with [`read`] and comparing against the source checksum.
///
/// # Safety
/// `src..src.add(len)` readable, `dst..dst.add(len)` writable.
#[inline]
#[target_feature(enable = "avx2")]
pub unsafe fn copy(src: *const u8, dst: *mut u8, len: usize, reps: usize) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: source loads are bounded by `src + (len & !127)` for the unrolled
    // loop and by the masked end `src + (len & !7)` for the tails; the
    // destination advances in lock-step, so it stays inside `dst..dst+len`.
    unsafe {
        asm!(
            "xor    eax, eax",
            "mov    r11, rdx",
            "and    r11, -8",
            "add    r11, rdi",
            "mov    r8, r11",              // src effective end
            "and    rdx, -128",
            "lea    r9, [rdi + rdx]",      // src unrolled end
            "test   r12, r12",
            "jz     9f",
            "4:",
            "mov    r10, rdi",             // src walk
            "mov    r11, rsi",             // dst walk
            "2:",
            "cmp    r10, r9",
            "jae    3f",
            "vmovdqa ymm0, [r10]",
            "vmovdqa [r11], ymm0",
            "vmovdqa ymm1, [r10 + 32]",
            "vmovdqa [r11 + 32], ymm1",
            "vmovdqa ymm2, [r10 + 64]",
            "vmovdqa [r11 + 64], ymm2",
            "vmovdqa ymm3, [r10 + 96]",
            "vmovdqa [r11 + 96], ymm3",
            "add    r10, 128",
            "add    r11, 128",
            "cmp    r10, r9",
            "jb     2b",
            "3:",
            "lea    rcx, [r10 + 32]",
            "cmp    rcx, r8",
            "ja     5f",
            "vmovdqa ymm0, [r10]",
            "vmovdqa [r11], ymm0",
            "mov    r10, rcx",
            "add    r11, 32",
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
            "vpextrq rax, xmm0, 0",
            "vzeroupper",
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

/// Copy kernel with non-temporal stores. This is what the Memory "Copy" column
/// reports: copying DRAM-sized buffers with normal stores would pollute the
/// caches and add read-for-ownership traffic.
///
/// # Safety
/// `src..src.add(len)` readable, `dst..dst.add(len)` writable and 32-byte
/// aligned.
#[inline]
#[target_feature(enable = "avx2")]
pub unsafe fn copy_nt(src: *const u8, dst: *mut u8, len: usize, reps: usize) -> u64 {
    let reps = reps.max(1);
    let mut out: u64;
    // SAFETY: as for [`copy`]; `dst` is 2 MiB aligned so every `vmovntdq`
    // target is 32-byte aligned.
    unsafe {
        asm!(
            "xor    eax, eax",
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
            "mov    r11, rsi",
            "2:",
            "cmp    r10, r9",
            "jae    3f",
            "vmovdqa ymm0, [r10]",
            "vmovntdq [r11], ymm0",
            "vmovdqa ymm1, [r10 + 32]",
            "vmovntdq [r11 + 32], ymm1",
            "vmovdqa ymm2, [r10 + 64]",
            "vmovntdq [r11 + 64], ymm2",
            "vmovdqa ymm3, [r10 + 96]",
            "vmovntdq [r11 + 96], ymm3",
            "add    r10, 128",
            "add    r11, 128",
            "cmp    r10, r9",
            "jb     2b",
            "3:",
            "lea    rcx, [r10 + 32]",
            "cmp    rcx, r8",
            "ja     5f",
            "vmovdqa ymm0, [r10]",
            "vmovntdq [r11], ymm0",
            "mov    r10, rcx",
            "add    r11, 32",
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
            "vpextrq rax, xmm0, 0",
            "vzeroupper",
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
