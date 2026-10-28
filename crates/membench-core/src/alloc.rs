//! Buffers for the measurements: `mmap`, 2 MiB alignment, huge pages,
//! prefault and optional `mlock`.
//!
//! PLAN.md "Memory allocation": pin first (done by the runner, so first-touch
//! keeps pages on the local NUMA node), `mmap` anonymous and 2 MiB aligned,
//! `madvise(MADV_HUGEPAGE)`, then prefault by writing every page before any
//! timing starts (rule 8: no page faults inside a timed region).
//!
//! Huge pages are not cosmetic: a random pointer chase over 1 GiB with 4 KiB
//! pages takes a page walk on most loads and reports DRAM latency that is
//! tens of nanoseconds too high (rule 7), so [`Buffer::page_status`] verifies
//! what the kernel actually handed us by reading `/proc/self/smaps`.

use std::fs::File;
use std::io::{self, BufRead};
use std::ptr;

use crate::result::PageKind;
use crate::{Error, Result};

const PAGE_4K: usize = 4096;
const TWO_MIB: usize = 2 * 1024 * 1024;
/// One `u64` written per 4 KiB is enough to fault in a 4 KiB page, and one
/// 4 KiB touch inside a 2 MiB region is enough for the kernel to install the
/// whole huge page — so this fills the buffer without writing it twice over.
const PREFULT_STRIDE: usize = PAGE_4K;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllocOpts {
    pub pages: PageKind,
    pub prefault: bool,
    pub lock: bool,
}

impl Default for AllocOpts {
    fn default() -> Self {
        AllocOpts {
            pages: PageKind::TwoM,
            prefault: true,
            lock: false,
        }
    }
}

/// Page size actually delivered, as verified from `/proc/self/smaps`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageStatus {
    pub requested: PageKind,
    /// `TwoM`/`OneG` only when essentially the whole region is huge-backed.
    pub actual: PageKind,
    /// `AnonHugePages` (or `HugetlbPages`) covering this mapping, in bytes.
    pub huge_bytes: Option<u64>,
    pub note: Option<String>,
}

/// An anonymous mapping, aligned so huge pages and non-temporal stores are
/// happy, fully faulted in.
pub struct Buffer {
    ptr: *mut u8,
    len: usize,
    opts: AllocOpts,
    locked: bool,
    madvise_note: Option<String>,
    /// Set once `MADV_COLLAPSE` has been tried for this mapping, so a status
    /// read never repeats a syscall the kernel already refused.
    collapse_tried: std::cell::Cell<bool>,
}

impl Buffer {
    /// Map `len` bytes (rounded up to a 4 KiB multiple) and prepare them.
    pub fn new(len: usize, opts: AllocOpts) -> Result<Buffer> {
        let len = (len + PAGE_4K - 1) & !(PAGE_4K - 1);
        if len == 0 {
            return Err(Error::Unsizeable {
                level: crate::result::Level::Memory,
                why: "zero-length buffer".to_string(),
            });
        }

        let align = match opts.pages {
            PageKind::FourK | PageKind::TwoM => TWO_MIB,
            PageKind::OneG => 1024 * TWO_MIB,
        };

        let mut flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE;
        if opts.pages == PageKind::OneG {
            // log2(1 GiB) << MAP_HUGE_SHIFT, as hugetlbfs expects.
            flags |= libc::MAP_HUGETLB | ((30 as libc::c_int) << libc::MAP_HUGE_SHIFT);
        }

        // Over-map by one alignment unit so an aligned interior can be kept.
        let raw_len = len + align;
        // SAFETY: anonymous private mapping; no fd, no file offset, and `raw_len`
        // is a positive multiple of the page size.
        let base = unsafe {
            libc::mmap(
                ptr::null_mut(),
                raw_len,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            let os = io::Error::last_os_error();
            if opts.pages == PageKind::OneG {
                return Err(Error::AllocationFailed {
                    bytes: len,
                    pages: opts.pages,
                    os: io::Error::other(format!(
                        "{os} — 1 GiB hugetlbfs pages must be reserved first, e.g. \
                         `sudo sysctl vm.nr_hugepages=4` (they are not allocated on demand)"
                    )),
                });
            }
            return Err(Error::AllocationFailed {
                bytes: len,
                pages: opts.pages,
                os,
            });
        }

        let base_addr = base as usize;
        let aligned = (base_addr + align - 1) & !(align - 1);
        // Release the slack in front of and behind the aligned window so the
        // process does not hold on to needless address space.
        let head = aligned - base_addr;
        if head > 0 {
            // SAFETY: `base..base+head` is a live mapping of exactly that size.
            unsafe { libc::munmap(base, head) };
        }
        let tail = raw_len - head - len;
        if tail > 0 {
            // SAFETY: `aligned+len..aligned+len+tail` is the untouched remainder
            // of the original mapping (head already released).
            unsafe { libc::munmap((aligned + len) as *mut libc::c_void, tail) };
        }

        let ptr = aligned as *mut u8;
        let mut madvise_note = None;

        // Ask for (or explicitly refuse) transparent huge pages.
        let advice = match opts.pages {
            PageKind::TwoM => Some(libc::MADV_HUGEPAGE),
            PageKind::FourK => Some(libc::MADV_NOHUGEPAGE),
            PageKind::OneG => None,
        };
        if let Some(advice) = advice {
            // SAFETY: `ptr..ptr+len` is a live mapping; the advice constant is
            // one of the madvise values for anonymous memory.
            let rc = unsafe { libc::madvise(ptr as *mut libc::c_void, len, advice) };
            if rc != 0 {
                madvise_note = Some(format!(
                    "madvise({}) failed: {}",
                    if advice == libc::MADV_HUGEPAGE {
                        "MADV_HUGEPAGE"
                    } else {
                        "MADV_NOHUGEPAGE"
                    },
                    io::Error::last_os_error()
                ));
            }
        }

        if opts.prefault {
            // Rule 8: every page has to be resident before the first timed
            // pass, or the first trial pays the faults. `prefault` does its own
            // unchecked writes behind a SAFETY comment of its own.
            prefault(ptr, len);
        }

        let mut locked = false;
        if opts.lock {
            // Failure is expected without CAP_IPC_LOCK; the runner reports the
            // mlock limit instead of treating it as fatal.
            // SAFETY: live mapping, `len` bytes.
            let rc = unsafe { libc::mlock(ptr as *mut libc::c_void, len) };
            locked = rc == 0;
        }

        Ok(Buffer {
            ptr,
            len,
            opts,
            locked,
            madvise_note,
            collapse_tried: std::cell::Cell::new(false),
        })
    }

    pub fn ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// Mutable view of the mapping (mirrors `&mut self`, so no aliasing).
    pub fn ptr_mut(&mut self) -> *mut u8 {
        self.ptr
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn opts(&self) -> AllocOpts {
        self.opts
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Write `seed`-derived words across the buffer (used by the kernel
    /// checksum tests; `prefault` deliberately writes a constant).
    pub fn fill_words(&mut self, seed: u64) {
        let mut x = if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        };
        let mut cursor = self.ptr as *mut u64;
        // SAFETY: one past the last byte of a mapping of at least 8 bytes.
        let end = unsafe { self.ptr.add(self.len) } as *mut u64;
        while cursor < end {
            // xorshift64*: deterministic without pulling in a PRNG crate.
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            let value = x.wrapping_mul(0x2545_f491_4f6c_dd1d);
            // SAFETY: loop invariant `cursor < end`, 8-byte stride, and the
            // mapping is `len` bytes with `len % 8 == 0`.
            unsafe { ptr::write(cursor, value) };
            // SAFETY: advancing within the same object.
            cursor = unsafe { cursor.add(1) };
        }
    }

    /// XOR of every 8-byte word — the cross-ISA checksum (PLAN.md: "every SIMD
    /// kernel produces the same checksum as the scalar reference").
    pub fn checksum_words(&self) -> u64 {
        let mut acc = 0u64;
        let mut cursor = self.ptr as *const u64;
        let end = unsafe { self.ptr.add(self.len) } as *const u64;
        while cursor < end {
            // SAFETY: `cursor < end` with an 8-byte stride.
            let w = unsafe { ptr::read_unaligned(cursor) };
            acc ^= w;
            cursor = unsafe { cursor.add(1) };
        }
        acc
    }

    /// What page size did the kernel actually give us?
    pub fn page_status(&self) -> PageStatus {
        let want_huge = self.opts.pages != PageKind::FourK;
        let region = read_smaps_region(self.ptr as usize);
        let (mut huge_bytes, field) = match region {
            Some(r) => {
                if self.opts.pages == PageKind::OneG {
                    (Some(r.hugetlb_bytes), "HugetlbPages")
                } else {
                    (Some(r.anon_huge_bytes), "AnonHugePages")
                }
            }
            None => (None, "AnonHugePages"),
        };

        let Some(read_bytes) = huge_bytes else {
            return PageStatus {
                requested: self.opts.pages,
                actual: PageKind::FourK,
                huge_bytes: None,
                note: Some("could not read /proc/self/smaps for this mapping".to_string()),
            };
        };

        // With `defrag=madvise` or a fragmented buddy allocator the first touch
        // can hand back 4 KiB pages even though we asked for huge ones. Linux
        // 6.1+ offers MADV_COLLAPSE, a synchronous collapse of an existing
        // range: it is the last thing we try, we try it once per buffer, and
        // when it fails we say so rather than reporting pages we do not have
        // (PLAN.md rule 7: verify the huge pages, and say when they are not
        // granted).
        // A mapping smaller than one huge page cannot be huge-backed, and the
        // arithmetic below would happily say otherwise: `expected` rounds down
        // to zero and `huge_bytes >= 0` looks like a grant. The L1 and L2 cells
        // run on buffers of a few KiB, so this case is the common case, not an
        // edge one.
        let page_bytes = self.opts.pages.bytes();
        let too_small = want_huge && self.len < page_bytes;

        let mut collapse_note: Option<String> = None;
        if want_huge
            && self.opts.pages == PageKind::TwoM
            && !too_small
            && read_bytes == 0
            && !self.collapse_tried.get()
        {
            self.collapse_tried.set(true);
            match self.collapse() {
                Ok(()) => {
                    if let Some(r) = read_smaps_region(self.ptr as usize) {
                        huge_bytes = Some(r.anon_huge_bytes);
                    }
                }
                Err(e) => collapse_note = Some(format!("MADV_COLLAPSE failed: {e}")),
            }
        }
        let huge_bytes = huge_bytes.unwrap_or(read_bytes);

        let expected = (self.len / page_bytes) * page_bytes;
        let granted = if want_huge {
            !too_small && huge_bytes as usize >= expected.saturating_sub(page_bytes)
        } else {
            huge_bytes == 0
        };

        let actual = if granted {
            self.opts.pages
        } else {
            PageKind::FourK
        };

        let note = if too_small {
            Some(format!(
                "a {} byte mapping is smaller than one {} page, so it is backed by 4 KiB pages \
                 and there is nothing to verify",
                self.len,
                self.opts.pages.label()
            ))
        } else if want_huge && !granted {
            Some(format!(
                "requested {pages} pages but {field} shows {huge_bytes} bytes of huge-backed \
                 memory for a {len} byte mapping (transparent hugepages setting: {}; \
                 a random chase over this buffer will include page walks){extra}",
                crate::sysinfo::transparent_hugepages_setting().unwrap_or_else(|| "?".into()),
                pages = self.opts.pages,
                len = self.len,
                extra = collapse_note
                    .as_deref()
                    .map(|n| format!("; {n}"))
                    .unwrap_or_default(),
            ))
        } else {
            self.madvise_note.clone()
        };

        PageStatus {
            requested: self.opts.pages,
            actual,
            huge_bytes: Some(huge_bytes),
            note,
        }
    }

    /// Ask the kernel to collapse this mapping into huge pages right now.
    fn collapse(&self) -> std::io::Result<()> {
        if MADV_COLLAPSE < 0 {
            return Err(std::io::Error::other("MADV_COLLAPSE is not available here"));
        }
        // SAFETY: `ptr..ptr+len` is this live mapping. The advice asks the kernel
        // to merge pages it already owns for us; it cannot fabricate memory or
        // reach outside the range.
        let rc = unsafe { libc::madvise(self.ptr as *mut libc::c_void, self.len, MADV_COLLAPSE) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        if self.locked {
            // SAFETY: live mapping; munlock of an unmapped range is not
            // reachable here because drop runs exactly once.
            unsafe { libc::munlock(self.ptr as *mut libc::c_void, self.len) };
        }
        // SAFETY: `ptr..ptr+len` is the mapping created by `new`; Drop runs
        // exactly once and nothing else can unmap it.
        let rc = unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
        debug_assert!(rc == 0, "munmap of a live buffer failed");
    }
}

// SAFETY: a Buffer is a plain mapping with no thread affinity; the runner
// hands each buffer to exactly one thread and never shares it. Rust's rule
// against shared mutable aliasing is enforced by the type system (`&mut self`
// methods), and the mapping itself is process-wide.
unsafe impl Send for Buffer {}
unsafe impl Sync for Buffer {}

/// `MADV_COLLAPSE`, spelled out because older `libc` releases do not name it.
#[cfg(target_os = "linux")]
const MADV_COLLAPSE: libc::c_int = 23;
#[cfg(not(target_os = "linux"))]
const MADV_COLLAPSE: libc::c_int = -1;

/// Write one `u64` per page so no load inside a timed region can fault.
///
/// Deliberately a plain (non-vectorised) loop: the kernel's fault handler, not
/// store throughput, is the bottleneck, and the loop runs outside every timed
/// window.
fn prefault(ptr: *mut u8, len: usize) {
    let mut offset = 0usize;
    while offset < len {
        // SAFETY: `offset < len`, and the write is 8 bytes inside a mapping
        // that is at least `len` long with `len % 4096 == 0`.
        unsafe { ptr::write(ptr.add(offset) as *mut u64, 0) };
        offset += PREFULT_STRIDE;
    }
    // Also touch the last word so a buffer whose length is not a multiple of
    // the stride is covered too.
    // SAFETY: `len >= 8` because `len` is a positive multiple of 4096.
    unsafe { ptr::write(ptr.add(len - 8) as *mut u64, 0) };
}

struct SmapsRegion {
    anon_huge_bytes: u64,
    hugetlb_bytes: u64,
}

/// Find the VMA containing `addr` in `/proc/self/smaps` and read the two
/// huge-page counters out of it.
fn read_smaps_region(addr: usize) -> Option<SmapsRegion> {
    let file = File::open("/proc/self/smaps").ok()?;
    let mut reader = io::BufReader::new(file);
    let mut line = String::new();
    let mut inside = false;
    let mut anon = 0u64;
    let mut huge = 0u64;
    let mut seen_any = false;

    loop {
        line.clear();
        let n = reader.read_line(&mut line).ok()?;
        if n == 0 {
            break;
        }
        if let Some((start, end)) = parse_smaps_range(&line) {
            if inside {
                break; // moved past the region of interest
            }
            inside = start <= addr && addr < end;
            seen_any |= inside;
            anon = 0;
            huge = 0;
            continue;
        }
        if !inside {
            continue;
        }
        if let Some(v) = parse_kb_field(&line, "AnonHugePages:") {
            anon = v;
        } else if let Some(v) = parse_kb_field(&line, "HugetlbPages:") {
            huge = v;
        }
    }

    if inside || seen_any {
        Some(SmapsRegion {
            anon_huge_bytes: anon,
            hugetlb_bytes: huge,
        })
    } else {
        None
    }
}

/// `7f8a1c000000-7f8a1c200000 rw-p 00000000 00:12 34567 /path` → (start, end)
fn parse_smaps_range(line: &str) -> Option<(usize, usize)> {
    let first = line.split_whitespace().next()?;
    let (a, b) = first.split_once('-')?;
    let start = usize::from_str_radix(a, 16).ok()?;
    let end = usize::from_str_radix(b, 16).ok()?;
    Some((start, end))
}

/// `AnonHugePages:    32768 kB` → bytes
fn parse_kb_field(line: &str, key: &str) -> Option<u64> {
    let rest = line.strip_prefix(key)?;
    let value: u64 = rest
        .split_whitespace()
        .next()?
        .parse()
        .map_err(|_| ())
        .ok()?;
    Some(value * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_and_prefaults() {
        let b = Buffer::new(128 * 1024, AllocOpts::default()).expect("map");
        assert_eq!(b.len() % PAGE_4K, 0);
        assert_eq!(b.ptr() as usize % TWO_MIB, 0, "2 MiB alignment");
        let s = b.page_status();
        assert_eq!(s.requested, PageKind::TwoM);
        assert!(
            matches!(s.actual, PageKind::TwoM | PageKind::FourK),
            "{s:?}"
        );
    }

    #[test]
    fn four_k_request_refuses_huge_pages() {
        let b = Buffer::new(
            8 * 1024 * 1024,
            AllocOpts {
                pages: PageKind::FourK,
                ..Default::default()
            },
        )
        .expect("map");
        let s = b.page_status();
        assert_eq!(s.actual, PageKind::FourK, "{s:?}");
    }

    #[test]
    fn checksum_matches_a_reference_xor() {
        let mut b = Buffer::new(64 * 1024, AllocOpts::default()).expect("map");
        b.fill_words(7);
        // Independent recomputation straight out of the mapping.
        let mut expect = 0u64;
        for i in 0..b.len() / 8 {
            // SAFETY: in-bounds by construction.
            let w = unsafe { ptr::read_unaligned(b.ptr().add(i * 8) as *const u64) };
            expect ^= w;
        }
        assert_eq!(b.checksum_words(), expect);
    }

    #[test]
    fn smaps_parser_handles_a_real_line() {
        assert_eq!(
            parse_smaps_range("7f8a1c000000-7f8a1c200000 rw-p 00000000 00:12 345 /x"),
            Some((0x7f8a1c000000, 0x7f8a1c200000))
        );
        assert_eq!(
            parse_kb_field("AnonHugePages:    2048 kB", "AnonHugePages:"),
            Some(2 << 20)
        );
        assert_eq!(parse_smaps_range("Rss: 12 kB"), None);
    }

    /// PLAN.md M2 check, written as a contract rather than a wish: whatever the
    /// kernel does, `page_status` must tell the truth about it, because the UI
    /// prints this and a silent fallback would quietly change what the
    /// latency numbers mean.
    #[test]
    fn huge_page_status_reports_what_the_kernel_gave() {
        let b = Buffer::new(32 * 1024 * 1024, AllocOpts::default()).expect("map");
        let s = b.page_status();
        assert_eq!(s.requested, PageKind::TwoM);
        match s.actual {
            PageKind::TwoM => {
                assert!(
                    s.huge_bytes.unwrap_or(0) > 0,
                    "claims 2 MiB pages with none: {s:?}"
                )
            }
            PageKind::FourK => {
                assert!(s.note.is_some(), "no explanation for the fallback: {s:?}");
                assert_eq!(
                    s.huge_bytes.unwrap_or(0),
                    0,
                    "fallback with huge bytes: {s:?}"
                );
            }
            PageKind::OneG => panic!("asked for 2 MiB, reported 1 GiB: {s:?}"),
        }
        // A 4 KiB request must never claim huge pages.
        let plain = Buffer::new(
            32 * 1024 * 1024,
            AllocOpts {
                pages: PageKind::FourK,
                ..AllocOpts::default()
            },
        )
        .expect("map");
        assert_eq!(plain.page_status().actual, PageKind::FourK);
    }

    #[test]
    fn a_mapping_smaller_than_a_huge_page_is_not_reported_as_huge() {
        // The L1 and L2 cells allocate tens of KiB while `--pages 2m` is still
        // in effect. Calling that "2m verified" is the exact lie rule 7 exists
        // to prevent, and the arithmetic in `page_status` used to do it: the
        // expected huge-backed byte count rounds down to zero, and zero bytes
        // of huge memory satisfies "at least zero".
        let mut b = Buffer::new(
            64 * 1024,
            AllocOpts {
                pages: PageKind::TwoM,
                ..Default::default()
            },
        )
        .expect("map 64 KiB");
        b.fill_words(7);
        let s = b.page_status();
        assert_eq!(s.requested, PageKind::TwoM);
        assert_eq!(s.actual, PageKind::FourK, "{s:?}");
        assert_eq!(s.huge_bytes.unwrap_or(0), 0, "{s:?}");
        let note = s.note.clone().unwrap_or_default();
        assert!(
            note.contains("smaller than one") || note.contains("4 KiB"),
            "the note does not explain the size: {note}"
        );
    }
}
