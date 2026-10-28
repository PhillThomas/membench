//! Scalar reference kernels.
//!
//! These are the correctness reference every SIMD path is checked against
//! (PLAN.md: "each ISA path matches the scalar reference on a checksum test")
//! and the `--isa scalar` fallback.
//!
//! On x86-64 the loops are `asm!`, so "scalar" really means scalar: a `while`
//! loop over a slice in safe Rust gets vectorised by LLVM and stops being a
//! reference. On other architectures the fallback (`portable`) uses
//! `black_box` on every loaded word, which blocks vectorisation of the
//! reduction; it is still the checksum reference, and the JSON says which path
//! ran.
//!
//! The pointer chase (`chase`) and the clock chain (`add_chain`) live here too:
//! they are deliberately non-vectorised and architecture-specific.

use crate::result::Isa;

/// Read: XOR-fold 8-byte words into four independent chains.
///
/// # Safety
/// `src..src.add(len)` must be readable.
#[inline]
pub unsafe fn read(src: *const u8, len: usize, reps: usize) -> u64 {
    let reps = reps.max(1);
    #[cfg(target_arch = "x86_64")]
    let out = {
        let mut out: u64;
        // SAFETY: every load is inside `src..src + (len & !7)`: the 32-byte
        // loop is bounded by `src + (len & !31)` and the 8-byte tail only runs
        // while a whole word remains below the masked end. Only declared
        // registers are written.
        unsafe {
            core::arch::asm!(
                "xor    eax, eax",
                "xor    r13d, r13d",
                "xor    r14d, r14d",
                "xor    r15d, r15d",
                "mov    r11, rdx",
                "and    r11, -8",
                "add    r11, rdi",
                "mov    r8, r11",              // r8 = src + (len & !7)
                "and    rdx, -32",
                "lea    r9, [rdi + rdx]",      // r9 = src + (len & !31)
                "test   r12, r12",
                "jz     9f",
                "4:",
                "mov    r10, rdi",
                "2:",
                "cmp    r10, r9",
                "jae    3f",
                "xor    rax, [r10]",
                "xor    r13, [r10 + 8]",
                "xor    r14, [r10 + 16]",
                "xor    r15, [r10 + 24]",
                "add    r10, 32",
                "cmp    r10, r9",
                "jb     2b",
                "3:",
                "lea    r11, [r10 + 8]",
                "cmp    r11, r8",
                "ja     6f",
                "xor    rax, [r10]",
                "mov    r10, r11",
                "jmp    3b",
                "6:",
                "dec    r12",
                "jnz    4b",
                "9:",
                "xor    rax, r13",
                "xor    rax, r14",
                "xor    rax, r15",
                in("rdi") src,
                in("rdx") len,
                inlateout("r12") reps => _,
                lateout("rax") out,
                lateout("rcx") _,
                lateout("rsi") _,
                lateout("r8") _,
                lateout("r9") _,
                lateout("r10") _,
                lateout("r11") _,
                lateout("r13") _,
                lateout("r14") _,
                lateout("r15") _,
                options(nostack),
            )
        }
        out
    };
    #[cfg(not(target_arch = "x86_64"))]
    let out = portable::read(src, len, reps);
    std::hint::black_box(out)
}

/// Write with normal stores.
///
/// # Safety
/// `dst..dst.add(len)` must be writable.
#[inline]
pub unsafe fn write(dst: *mut u8, len: usize, reps: usize, pattern: u64) -> u64 {
    let reps = reps.max(1);
    #[cfg(target_arch = "x86_64")]
    let out = {
        let mut out: u64;
        // SAFETY: stores bounded by `dst..dst + (len & !7)`.
        unsafe {
            core::arch::asm!(
                "mov    r11, rdx",
                "and    r11, -8",
                "add    r11, rdi",
                "mov    r8, r11",
                "and    rdx, -32",
                "lea    r9, [rdi + rdx]",
                "test   r12, r12",
                "jz     9f",
                "4:",
                "mov    r10, rdi",
                "2:",
                "cmp    r10, r9",
                "jae    3f",
                "mov    [r10], rsi",
                "mov    [r10 + 8], rsi",
                "mov    [r10 + 16], rsi",
                "mov    [r10 + 24], rsi",
                "add    r10, 32",
                "cmp    r10, r9",
                "jb     2b",
                "3:",
                "lea    r11, [r10 + 8]",
                "cmp    r11, r8",
                "ja     6f",
                "mov    [r10], rsi",
                "mov    r10, r11",
                "jmp    3b",
                "6:",
                "dec    r12",
                "jnz    4b",
                "9:",
                "mov    rax, rsi",
                in("rdi") dst,
                in("rsi") pattern,
                in("rdx") len,
                inlateout("r12") reps => _,
                lateout("rax") out,
                lateout("r8") _,
                lateout("r9") _,
                lateout("r10") _,
                lateout("r11") _,
                options(nostack),
            )
        }
        out
    };
    #[cfg(not(target_arch = "x86_64"))]
    let out = portable::write(dst, len, reps, pattern);
    std::hint::black_box(out)
}

/// Copy with normal stores.
///
/// # Safety
/// `src..src.add(len)` readable, `dst..dst.add(len)` writable.
#[inline]
pub unsafe fn copy(src: *const u8, dst: *mut u8, len: usize, reps: usize) -> u64 {
    let reps = reps.max(1);
    #[cfg(target_arch = "x86_64")]
    let out = {
        let mut out: u64;
        // SAFETY: source loads and destination stores advance in lock-step and
        // are bounded by `(len & !7)` bytes from their respective bases.
        unsafe {
            core::arch::asm!(
                "mov    r11, rdx",
                "and    r11, -8",
                "add    r11, rdi",
                "mov    r8, r11",
                "and    rdx, -32",
                "lea    r9, [rdi + rdx]",
                "test   r12, r12",
                "jz     9f",
                "4:",
                "mov    r10, rdi",
                "mov    r11, rsi",
                "2:",
                "cmp    r10, r9",
                "jae    3f",
                "mov    rcx, [r10]",
                "mov    [r11], rcx",
                "mov    rcx, [r10 + 8]",
                "mov    [r11 + 8], rcx",
                "mov    rcx, [r10 + 16]",
                "mov    [r11 + 16], rcx",
                "mov    rcx, [r10 + 24]",
                "mov    [r11 + 24], rcx",
                "add    r10, 32",
                "add    r11, 32",
                "cmp    r10, r9",
                "jb     2b",
                "3:",
                "lea    rcx, [r10 + 8]",
                "cmp    rcx, r8",
                "ja     6f",
                "mov    rdx, [r10]",
                "mov    [r11], rdx",
                "mov    r10, rcx",
                "add    r11, 8",
                "jmp    3b",
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
                options(nostack),
            )
        }
        out
    };
    #[cfg(not(target_arch = "x86_64"))]
    let out = portable::copy(src, dst, len, reps);
    std::hint::black_box(out)
}

/// Dependent-load pointer chase: `p = *p`, unrolled 16x so the loop overhead
/// disappears behind the load latency (PLAN.md "Measuring").
///
/// Performs `loads & !15` loads; callers must divide elapsed time by that
/// number, not by `loads`.
///
/// # Safety
/// `start` must be the head of a closed cycle of readable pointers, as built by
/// [`crate::latency::build_chain`].
#[inline]
pub unsafe fn chase(start: *const u64, loads: u64) -> u64 {
    #[cfg(target_arch = "x86_64")]
    let out = {
        let mut out: u64;
        // SAFETY: the chain contract guarantees every address loaded is one of
        // the nodes handed to us, each readable for 8 bytes.
        unsafe {
            core::arch::asm!(
                "mov    rax, {p}",
                "shr    {n}, 4",
                "jz     2f",
                "4:",
                "mov    rax, [rax]",
                "mov    rax, [rax]",
                "mov    rax, [rax]",
                "mov    rax, [rax]",
                "mov    rax, [rax]",
                "mov    rax, [rax]",
                "mov    rax, [rax]",
                "mov    rax, [rax]",
                "mov    rax, [rax]",
                "mov    rax, [rax]",
                "mov    rax, [rax]",
                "mov    rax, [rax]",
                "mov    rax, [rax]",
                "mov    rax, [rax]",
                "mov    rax, [rax]",
                "mov    rax, [rax]",
                "dec    {n}",
                "jnz    4b",
                "2:",
                p = in(reg) start,
                n = inlateout(reg) loads => _,
                lateout("rax") out,
                options(nostack, preserves_flags),
            )
        }
        out
    };
    #[cfg(target_arch = "aarch64")]
    let out = {
        let mut out: u64;
        // SAFETY: as for x86-64.
        unsafe {
            core::arch::asm!(
                "mov    x8, {p}",
                "lsr    {n}, {n}, #4",
                "cbz    {n}, 2f",
                "4:",
                "ldr    x8, [x8]",
                "ldr    x8, [x8]",
                "ldr    x8, [x8]",
                "ldr    x8, [x8]",
                "ldr    x8, [x8]",
                "ldr    x8, [x8]",
                "ldr    x8, [x8]",
                "ldr    x8, [x8]",
                "ldr    x8, [x8]",
                "ldr    x8, [x8]",
                "ldr    x8, [x8]",
                "ldr    x8, [x8]",
                "ldr    x8, [x8]",
                "ldr    x8, [x8]",
                "ldr    x8, [x8]",
                "ldr    x8, [x8]",
                "subs   {n}, {n}, #1",
                "b.ne   4b",
                "2:",
                p = in(reg) start,
                n = inout(reg) loads => _,
                lateout("x8") out,
                options(nostack),
            )
        }
        out
    };
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let out = portable::chase(start, loads);
    std::hint::black_box(out)
}

/// Dependent integer add chain: one add per cycle, so adds per second is the
/// core clock (PLAN.md "System detection", rule 5).
///
/// Runs `iterations & !15` adds.
///
/// # Safety
/// Pure register arithmetic.
#[inline]
pub unsafe fn add_chain(iterations: u64) -> u64 {
    let iterations = iterations & !15;
    #[cfg(target_arch = "x86_64")]
    let out = {
        let mut out: u64;
        // SAFETY: registers only.
        unsafe {
            core::arch::asm!(
                "xor    eax, eax",
                // Sixteen adds per pass, so the pass count is iterations/16 --
                // the same shift the aarch64 version does. Without it the chain
                // runs 16x the requested work and the clock reading is wrong.
                "shr    {n}, 4",
                "jz     2f",
                "4:",
                "add    rax, 1",
                "add    rax, 1",
                "add    rax, 1",
                "add    rax, 1",
                "add    rax, 1",
                "add    rax, 1",
                "add    rax, 1",
                "add    rax, 1",
                "add    rax, 1",
                "add    rax, 1",
                "add    rax, 1",
                "add    rax, 1",
                "add    rax, 1",
                "add    rax, 1",
                "add    rax, 1",
                "add    rax, 1",
                "dec    {n}",
                "jnz    4b",
                "2:",
                n = inlateout(reg) iterations => _,
                lateout("rax") out,
                options(nostack),
            )
        }
        out
    };
    #[cfg(target_arch = "aarch64")]
    let out = {
        let mut out: u64;
        // SAFETY: registers only.
        unsafe {
            core::arch::asm!(
                "mov    x8, xzr",
                "lsr    {n}, {n}, #4",
                "cbz    {n}, 2f",
                "4:",
                "add    x8, x8, #1",
                "add    x8, x8, #1",
                "add    x8, x8, #1",
                "add    x8, x8, #1",
                "add    x8, x8, #1",
                "add    x8, x8, #1",
                "add    x8, x8, #1",
                "add    x8, x8, #1",
                "add    x8, x8, #1",
                "add    x8, x8, #1",
                "add    x8, x8, #1",
                "add    x8, x8, #1",
                "add    x8, x8, #1",
                "add    x8, x8, #1",
                "add    x8, x8, #1",
                "add    x8, x8, #1",
                "subs   {n}, {n}, #1",
                "b.ne   4b",
                "2:",
                n = inout(reg) iterations => _,
                lateout("x8") out,
                options(nostack),
            )
        }
        out
    };
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let out = portable::add_chain(iterations);
    std::hint::black_box(out)
}

/// Portable fallback for architectures without an `asm!` path. Kept so the
/// crate builds and tests on e.g. riscv64 CI, and used as the scalar path on
/// aarch64 (see the module docs).
#[cfg(not(target_arch = "x86_64"))]
mod portable {
    use std::hint::black_box;

    /// # Safety
    /// Same region contracts as the `asm!` versions.
    pub unsafe fn read(src: *const u8, len: usize, reps: usize) -> u64 {
        let words = (len & !7) / 8;
        let mut acc = 0u64;
        for _ in 0..reps.max(1) {
            for i in 0..words {
                // SAFETY: `i < words`, so the read stays inside the region the
                // contract covers.
                let v = unsafe { std::ptr::read_unaligned(src.add(i * 8) as *const u64) };
                // black_box per word: stops LLVM turning the reduction back
                // into a vectorised loop, which would make "scalar" a lie.
                acc ^= black_box(v);
            }
        }
        acc
    }

    /// # Safety
    /// As above.
    pub unsafe fn write(dst: *mut u8, len: usize, reps: usize, pattern: u64) -> u64 {
        let words = (len & !7) / 8;
        for _ in 0..reps.max(1) {
            for i in 0..words {
                // SAFETY: in-bounds by the mask.
                unsafe { std::ptr::write(dst.add(i * 8) as *mut u64, black_box(pattern)) };
            }
        }
        black_box(pattern)
    }

    /// # Safety
    /// As above.
    pub unsafe fn copy(src: *const u8, dst: *mut u8, len: usize, reps: usize) -> u64 {
        let words = (len & !7) / 8;
        for _ in 0..reps.max(1) {
            for i in 0..words {
                // SAFETY: both regions are at least `len` bytes long.
                let v = unsafe { std::ptr::read_unaligned(src.add(i * 8) as *const u64) };
                unsafe { std::ptr::write(dst.add(i * 8) as *mut u64, v) };
            }
        }
        0
    }

    /// # Safety
    /// As above.
    pub unsafe fn chase(start: *const u64, loads: u64) -> u64 {
        let mut p: usize = start as usize;
        for _ in 0..(loads & !15) {
            // SAFETY: the chain contract.
            p = unsafe { std::ptr::read_unaligned(p as *const u64) } as usize;
        }
        p as u64
    }

    pub fn add_chain(iterations: u64) -> u64 {
        let mut acc = 0u64;
        for _ in 0..(iterations & !15) {
            acc = black_box(acc.wrapping_add(1));
        }
        acc
    }
}

/// Which ISA these kernels implement.
pub const ISA: Isa = Isa::Scalar;

/// True when the scalar path is real scalar assembly rather than an
/// LLVM-compiled loop (affects only the honesty of the `--isa scalar` label).
pub const IS_ASM: bool = cfg!(target_arch = "x86_64");

#[cfg(test)]
mod tests {
    use crate::alloc::{AllocOpts, Buffer};

    fn buf(len: usize, seed: u64) -> Buffer {
        let mut b = Buffer::new(len, AllocOpts::default()).unwrap();
        b.fill_words(seed);
        b
    }

    #[test]
    fn read_matches_a_rust_xor_of_the_same_words() {
        let b = buf(40 * 1024, 0xdead_beef);
        let expect = b.checksum_words();
        // SAFETY: buffer is live and readable for its whole length.
        let got = unsafe { super::read(b.ptr(), b.len(), 3) };
        assert_eq!(got, expect);
    }

    #[test]
    fn read_covers_a_non_multiple_of_the_block_size() {
        // 4097 rounds up to 8192 in Buffer, but a length like 8192+4096 exercises
        // the 8-byte tail path with an odd word count too.
        let b = buf(12 * 1024 + 8, 7);
        // SAFETY: live buffer.
        assert_eq!(
            unsafe { super::read(b.ptr(), b.len(), 1) },
            b.checksum_words()
        );
    }

    #[test]
    fn write_then_read_round_trips() {
        let mut b = buf(16 * 1024, 1);
        let pat = 0x0123_4567_89ab_cdef;
        // SAFETY: live buffer.
        unsafe { super::write(b.ptr_mut(), b.len(), 2, pat) };
        for i in 0..b.len() / 8 {
            // SAFETY: in-bounds.
            let w = unsafe { std::ptr::read_unaligned(b.ptr().add(i * 8) as *const u64) };
            assert_eq!(w, pat, "word {i}");
        }
        assert_eq!(b.checksum_words(), unsafe {
            super::read(b.ptr(), b.len(), 1)
        });
    }

    #[test]
    fn copy_moves_everything() {
        let src = buf(24 * 1024, 99);
        let mut dst = buf(24 * 1024, 0);
        // SAFETY: two live buffers of equal length.
        unsafe { super::copy(src.ptr(), dst.ptr_mut(), src.len(), 2) };
        assert_eq!(dst.checksum_words(), src.checksum_words());
    }

    #[test]
    fn add_chain_counts_exactly() {
        // SAFETY: registers only.
        assert_eq!(unsafe { super::add_chain(1_000_000) }, 1_000_000 & !15);
    }

    #[test]
    fn chase_walks_the_cycle() {
        // A three-node cycle in a three-line buffer.
        let b = buf(3 * 64, 0);
        let base = b.ptr() as usize;
        let nodes = [base, base + 64, base + 128];
        for (i, n) in nodes.iter().enumerate() {
            let next = nodes[(i + 1) % nodes.len()] as *mut u64;
            // SAFETY: writing the first word of each of the three lines.
            unsafe { std::ptr::write(*n as *mut u64, next as u64) };
        }
        // SAFETY: closed cycle of three readable nodes.
        let loads = 64;
        let after = unsafe { super::chase(base as *const u64, loads) };
        // The kernel performs `loads & !15` dependent loads; reproduce that
        // count in safe Rust and expect the same landing node.
        let mut expect = base;
        for _ in 0..(loads & !15) {
            expect = nodes[(nodes.iter().position(|n| *n == expect).unwrap() + 1) % 3];
        }
        assert_eq!(after, expect as u64);
    }
}
