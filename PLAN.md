# Rust Memory Benchmark for Omarchy — Build Plan

Oct 28, 2026

## Goal and scope

Build `membench`, a Rust terminal app that reproduces the AIDA64 "Cache & Memory Benchmark" on Linux: one results grid of Read, Write, Copy (GB/s) and Latency (ns) for Memory, L1, L2 and L3, plus the system info panel beside it.

In scope:

- Bandwidth: read, write and copy, multi-threaded, per cache level and for main memory.
- Latency: dependent-load pointer chase, single core, per cache level and for main memory, plus a full latency-vs-block-size sweep.
- System panel: CPU model, cores/threads, clocks, cache sizes, memory type, speed, channels and timings where readable.
- A TUI (default), a plain CLI table, and JSON output for scripting and comparing runs.

Definition of done: on a modern x86-64 desktop, results land within 10% of Intel MLC and lmbench for the same hardware, and run-to-run spread is under 3%.

## Target platform and non-goals

Omarchy is [Arch Linux with Hyprland and Quickshell](https://www.starryhope.com/linux/what-is-omarchy/) and a terminal-first workflow, so the UI is a ratatui TUI. Nothing in the benchmark core is Omarchy-specific: it should run on any x86-64 or aarch64 Linux.

Platform facts to build around:

- Rolling-release Arch: target the current stable Rust toolchain via `rustup`; package later with a PKGBUILD.
- Wayland/Hyprland desktop: the compositor and browser add background noise, so the tool warns the user to close heavy apps before a run.
- No root by default. Everything core must work as a normal user; root only unlocks extras (DIMM details, 1 GB huge pages, real-time priority).
- Use ANSI palette colours only in the TUI, so it follows whatever Omarchy theme the terminal is using.

Non-goals for v1: GPU/VRAM benchmarks, disk benchmarks, overclocking controls, Windows/macOS, a GUI, multi-socket NUMA tuning (detect and warn only).

## Architecture

One Cargo workspace: a library crate holding all measurement logic, and a thin binary for the TUI and CLI. The UI never touches timing code, so benchmarks can be tested headless.

```text
membench/
  Cargo.toml                 # workspace
  crates/
    membench-core/
      src/
        lib.rs
        sysinfo/             # cpu.rs, cache.rs, dimm.rs, governor.rs, topology.rs
        alloc.rs             # mmap + huge pages + prefault + mlock
        affinity.rs          # pin threads, pick cores
        timer.rs             # Instant wrapper, TSC calibration, cycles<->ns
        kernels/
          mod.rs             # runtime SIMD dispatch
          x86_avx512.rs
          x86_avx2.rs
          x86_sse2.rs
          aarch64_neon.rs
          scalar.rs          # reference + tests
        bandwidth.rs         # read/write/copy runner, threads, barrier
        latency.rs           # pointer chase, Sattolo, sweep
        stats.rs             # best, median, stddev, outlier rejection
        result.rs            # serde result types
    membench/
      src/
        main.rs              # clap entry
        tui/                 # ratatui app, results grid, sweep chart
        cli.rs               # plain table output
  packaging/
    PKGBUILD
    membench.desktop
```

| Crate | Use |
| --- | --- |
| `clap` (derive) | CLI flags and subcommands |
| `ratatui` + `crossterm` | TUI grid, progress, latency sweep chart |
| `serde`, `serde_json` | JSON results and run comparison |
| `libc` or `rustix` | `mmap`, `madvise`, `mlock`, `sched_setaffinity`, `setpriority` |
| `raw-cpuid` (x86 only) | CPU brand, features, cache descriptors |
| `anyhow` / `thiserror` | Errors in binary / library |

No `criterion`: it is built for micro-benchmarks of functions, not sustained memory streams, and its harness adds noise here. Use the latest stable versions of each crate.

Release profile: `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`. Do not ship with `-C target-cpu=native`; pick SIMD paths at runtime so one binary works across machines.

## System detection

Detection runs first and drives every benchmark parameter: buffer sizes come from the real cache sizes, never hard-coded values.

| What | Source | Root? |
| --- | --- | --- |
| CPU model, features (SSE2/AVX2/AVX-512) | `raw-cpuid`, `/proc/cpuinfo` fallback | No |
| Cores, threads, SMT siblings | `/sys/devices/system/cpu/cpuN/topology/` (`core_cpus_list`, `package_cpus_list`) | No |
| Cache levels per core | `/sys/devices/system/cpu/cpuN/cache/indexM/` (`level`, `type`, `size`, `shared_cpu_list`, `coherency_line_size`) | No |
| Hybrid P/E cores | `/sys/devices/cpu_core/cpus` and `/sys/devices/cpu_atom/cpus` (Intel); `cpu_capacity` (Arm) | No |
| L3 domains (e.g. AMD CCDs) | group CPUs by L3 `shared_cpu_list` | No |
| Clock, governor, EPP | `cpufreq/scaling_cur_freq`, `cpuinfo_max_freq`, `scaling_governor`, `energy_performance_preference` | No |
| Transparent huge pages | `/sys/kernel/mm/transparent_hugepage/enabled` | No |
| NUMA nodes | `/sys/devices/system/node/` | No |
| Total RAM | `/proc/meminfo` | No |
| DIMM type, size, rated and configured speed, part number, slot | SMBIOS type 17 from `/sys/firmware/dmi/tables/DMI` (parse directly, no `dmidecode` dependency) | Yes |
| SPD rated timings (CL-tRCD-tRP-tRAS) | `spd5118` (DDR5) or `ee1004` (DDR4) EEPROM under `/sys/bus/i2c/drivers/` | Usually |

Rules:

- Treat every source as optional. A missing file means "Unknown" in the panel, never a crash.
- Read cache info per core, not just from CPU 0: hybrid CPUs have different L2 sizes on P and E cores.
- Measure the effective core clock during the run with a dependent integer-add chain (one add per cycle), and use it to convert latency to cycles. Sysfs frequency is only a fallback.
- Warn if the governor is not `performance`, or EPP is `power`/`balance_power`, and say how to change it.
- Without root, show "Run with sudo for DIMM details" in the memory panel.
- Actual running timings (what AIDA shows) need memory-controller register access per vendor. Leave that as a stretch goal; v1 shows SPD-rated timings labelled as such.

## Bandwidth benchmark

Each bandwidth cell is the best of several timed trials of a SIMD streaming kernel run on every pinned thread at once, over a buffer sized to sit in exactly one level of the hierarchy.

### Buffer sizing (per thread)

| Level | Per-thread buffer | Threads |
| --- | --- | --- |
| L1 | 0.5 × L1d | one per physical core |
| L2 | 0.5 × L2 | one per physical core |
| L3 | between 2 × L2 and 0.75 × (L3 ÷ threads sharing it) | one per physical core; drop threads if the range is empty |
| Memory | max(1 GiB, 16 × total L3) ÷ threads | all logical CPUs |

SMT siblings share L1 and L2, so cache tests default to one thread per physical core. Make thread count a flag (`--threads`).

### Kernels

- **Read:** stream loads into 4 to 8 independent vector accumulators (XOR or add), unrolled 8×, so no dependency chain limits load throughput. Feed the final value to `std::hint::black_box`.
- **Write:** store a constant vector register. Cache levels use normal stores. Memory runs both non-temporal stores (`vmovntdq` + `sfence`) and normal stores; normal stores pay a read-for-ownership, so show both and label which is the headline.
- **Copy:** load then store. Cache levels use normal stores; memory uses non-temporal stores.
- **Optional:** `rep movsb` / `rep stosb` variants (fast on CPUs with ERMSB/FSRM); report them in the detail view.
- Copy convention: count bytes copied (source size) by default; state it in the UI and JSON, with a `--copy-counts-both` flag for STREAM-style read+write accounting.

Write the inner loops in `core::arch::asm!`, not plain Rust. LLVM recognises simple store and copy loops and replaces them with `memset`/`memcpy` calls, which silently measures libc instead. Intrinsics are acceptable only if `cargo asm` confirms the loop survived.

Dispatch at runtime with `is_x86_feature_detected!`: AVX-512 → AVX2 → SSE2 on x86-64, NEON on aarch64, scalar as the test reference. Add `--isa` to force a path, since AVX-512 is slower than AVX2 on some chips.

### Memory allocation

1. Each thread pins itself first, then allocates its own buffer (first-touch keeps pages on the local NUMA node).
2. `mmap` anonymous, 2 MiB aligned, then `madvise(MADV_HUGEPAGE)`.
3. Prefault by writing every page before timing; optional `mlock` (ignore failure from `RLIMIT_MEMLOCK`).

### Running and timing

1. Spawn threads, pin with `sched_setaffinity`, allocate, prefault.
2. Warm-up pass on every thread (brings clocks up, fills TLBs).
3. `std::sync::Barrier`, then each thread loops its kernel for about 200 ms and records start and end with `Instant`.
4. Bandwidth = total bytes ÷ (latest end − earliest start).
5. Repeat 7 trials; report the best (AIDA-style peak) as the headline, and keep median and standard deviation in the detail view and JSON.
6. Optionally raise priority (`setpriority` / `SCHED_FIFO`) when running as root.

## Latency benchmark

Latency is the average time of one load in a long chain of dependent loads (`p = *p`) through a randomly ordered cycle, so neither out-of-order execution nor the hardware prefetchers can hide it.

### Building the chain

1. Allocate the buffer as in the bandwidth test (huge pages, prefaulted).
2. Split it into nodes of one cache line each (`coherency_line_size`, normally 64 B). Each node stores the address of the next.
3. Order the nodes with Sattolo's algorithm, which produces one single cycle through every node. Seed a small xorshift PRNG with a fixed value so runs are reproducible.
4. Verify the cycle length equals the node count in debug builds.

### Measuring

- One thread, pinned to the fastest core (a P-core on hybrid chips, highest `cpuinfo_max_freq` otherwise). All other benchmark threads idle.
- Inner loop in `asm!`: `mov rax, [rax]` unrolled 16×, so loop overhead disappears behind the load latency.
- Run for about 100 ms per trial, 7 trials, report the best; ns per load = elapsed ÷ loads.
- Convert to cycles with the clock measured during the run, and show both.

| Level | Buffer size |
| --- | --- |
| L1 | 0.5 × L1d |
| L2 | 0.5 × L2 |
| L3 | 0.5 × the L3 slice the pinned core can reach |
| Memory | max(1 GiB, 16 × total L3) |

### TLB effects

With 4 KiB pages, a random walk over 1 GiB adds a page walk to most loads and overstates memory latency. Keep huge pages as the default and check they were granted by reading `AnonHugePages` for the region in `/proc/self/smaps`. If they were not, show a warning beside the Memory latency cell. Offer `--pages=4k|2m|1g` (1 GiB pages need `hugetlbfs` set up by root).

### Latency sweep

Also run the chase at block sizes from 4 KiB to 1 GiB (powers of two plus 1.5× steps) and plot ns against log block size in the TUI. The plateaus show each cache level and confirm the detected sizes.

Stretch goals: loaded latency (latency while other cores run the bandwidth kernel, like Intel MLC), cross-L3-domain latency, and a core-to-core latency matrix.

## UI and output

The default `membench` command opens a TUI that mirrors AIDA's window: the results grid on top, system info below, and cells filling in live as each test finishes.

```text
┌ membench ─────────────────────────────────────────────────────┐
│            Read        Write       Copy        Latency        │
│ Memory     78.4 GB/s   71.2 GB/s   69.9 GB/s   72.1 ns        │
│ L1 Cache   3.1 TB/s    1.6 TB/s    3.0 TB/s    0.8 ns         │
│ L2 Cache   1.4 TB/s    1.1 TB/s    1.3 TB/s    2.6 ns         │
│ L3 Cache   820 GB/s    760 GB/s    790 GB/s    10.4 ns        │
├───────────────────────────────────────────────────────────────┤
│ CPU        <model>  16C/32T  5.4 GHz measured                 │
│ Caches     L1d 48K  L2 1M  L3 32M x2                          │
│ Memory     DDR5-6000  2 x 16 GB  dual channel  CL30 (SPD)     │
│ Warnings   governor: powersave                                │
├───────────────────────────────────────────────────────────────┤
│ [s] start  [l] latency sweep  [d] details  [e] export  [q] quit│
└───────────────────────────────────────────────────────────────┘
```

The numbers above are layout placeholders, not targets.

TUI behaviour:

- Progress shown in the cell being measured; nothing else runs while a test is timed, including redraws. The UI thread sleeps on a channel until a result arrives.
- `d` opens per-cell detail: best, median, stddev, threads, buffer size, ISA path, page size.
- `l` runs the sweep and draws it with a ratatui `Chart` (log x-axis).
- `e` writes JSON to `~/.local/share/membench/` with a timestamp.

CLI:

| Command | Does |
| --- | --- |
| `membench` | Open the TUI |
| `membench run --no-tui` | Run all tests, print the grid as plain text |
| `membench run --only read,latency --level mem,l3` | Subset of tests |
| `membench run --json out.json` | Write results as JSON |
| `membench sweep` | Latency sweep to stdout (CSV) |
| `membench info` | System panel only |
| `membench compare a.json b.json` | Side-by-side with % change |

Shared flags: `--threads N`, `--isa avx512|avx2|sse2|neon|scalar`, `--pages 4k|2m|1g`, `--trials N`, `--duration-ms N`.

JSON carries a schema version, timestamp, kernel version, full system info, every trial value and the settings used, so two runs can always be compared.

## Milestones

Build in this order; each milestone ends with its checks passing and a commit. Get one correct number end to end (M3) before adding breadth.

- [ ] **M0 Scaffold.** Workspace, two crates, release profile, `clippy` and `rustfmt` clean, CI running `cargo test`.
- [ ] **M1 System info.** All sources in the detection table, `membench info` prints them. Check: cache sizes match `lscpu -C`; missing files give "Unknown", not a panic; works without root.
- [ ] **M2 Plumbing.** Huge-page allocator, prefault, thread pinning, barrier runner, timer, stats. Check: `AnonHugePages` shows the buffer is backed by 2 MiB pages; each thread's affinity is verified with `sched_getaffinity`.
- [ ] **M3 First number.** Memory read bandwidth, AVX2 and scalar, via `membench run --no-tui`. Check: within 10% of Intel MLC or STREAM on the same box; spread across 5 runs under 3%; `cargo asm` shows the vector loop intact.
- [ ] **M4 All bandwidth cells.** Read, write (normal + non-temporal), copy for L1, L2, L3, Memory; SSE2, AVX-512 and NEON paths; `--isa`. Check: L1 > L2 > L3 > Memory for every op; each ISA path matches the scalar reference on a checksum test.
- [ ] **M5 Latency.** Sattolo chain, four cells, cycles conversion, sweep command. Check: Memory latency within 10% of `lat_mem_rd` (lmbench) or MLC idle latency; sweep plateaus sit at the detected cache sizes.
- [ ] **M6 TUI.** Grid, system panel, live progress, detail view, sweep chart, keybindings. Check: TUI redraw does not change results (compare to `--no-tui` runs).
- [ ] **M7 Export and compare.** JSON schema, `--json`, `compare`. Check: round-trip test serialises and parses a full result.
- [ ] **M8 Packaging.** PKGBUILD (`cargo build --release --locked`), `.desktop` entry that opens the TUI in the default terminal, README with how to read the results and how to set the performance governor.
- [ ] **M9 Stretch.** Loaded latency, core-to-core matrix, actual running timings.

## Validation

A number is only trusted once it agrees with an independent tool and with the hardware's theoretical limit.

Reference tools (install from Arch repos, the AUR, or source):

| Tool | Compare against |
| --- | --- |
| Intel MLC | Memory bandwidth, idle and loaded latency |
| STREAM | Memory copy and triad bandwidth (mind its 2× copy accounting) |
| lmbench `lat_mem_rd` | Latency sweep shape and memory latency |
| tinymembench | Read/write/copy and latency with huge pages |
| `lscpu -C` | Detected cache sizes |

Sanity math (approximate, for catching gross errors):

    peak GB/s = channels × MT/s × 8 B ÷ 1000

- Dual-channel DDR5-6000: 2 × 6000 × 8 B = 96 GB/s peak. Measured read should land at roughly 70 to 90% of peak; above 100% means a bug (usually the buffer fits in cache or the loop was optimised away).
- Typical modern x86 latency, approximate: L1 about 4 to 5 cycles, L2 about 12 to 16, L3 about 40 to 60, desktop DDR5 roughly 60 to 100 ns.

Automated tests:

- Every SIMD kernel produces the same checksum as the scalar reference on random buffers.
- Sattolo output is a single cycle covering every node (property test over many sizes).
- Buffer-size calculator never returns a size outside its level's bounds for a table of real CPU topologies (fixtures copied from `/sys` trees).
- Stats: best, median and stddev on known inputs.

## Rules for the coding agent

Most memory benchmarks that give wrong numbers fail for one of these reasons; check each before reporting a milestone done.

1. **Dead code elimination.** Every kernel result goes through `black_box`, and inner loops live in `asm!`. Inspect the generated assembly for each new kernel.
2. **Benchmarking a debug build.** Timing code refuses to run (prints an error) when `cfg!(debug_assertions)` is set.
3. **Buffer in the wrong level.** Derive every size from detected topology and log it; never hard-code 32K/1M/32M.
4. **Unpinned or migrating threads.** Pin before allocating; re-check affinity at the start of each trial.
5. **Clock ramp and power saving.** Always warm up first; warn on non-performance governors; record the measured clock with each result.
6. **Hybrid and multi-CCD CPUs.** Latency runs on a P-core; L3 tests stay inside one L3 domain; report which cores were used.
7. **TLB misses posing as DRAM latency.** Huge pages by default, and the UI must say when they were not granted.
8. **Page faults during timing.** Prefault every buffer before the first timed pass.
9. **Noise from the desktop.** Run the UI thread asleep during measurements; show a "close heavy apps" hint before runs.
10. **Unsafe code.** Keep `unsafe` inside `kernels/`, `alloc.rs` and `affinity.rs`, each block with a `// SAFETY:` comment; the rest of the code base is safe Rust.
11. **Honest labels.** Every number shows its unit and convention (copy accounting, NT vs normal stores, page size). Unknown values say "Unknown", never a guess.

When unsure how AIDA64 defines a figure, pick the most standard definition, document it in the README, and make it switchable by flag rather than guessing silently.
