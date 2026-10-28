# Reference benchmarks

A second opinion on `membench`'s numbers, written in plain C so that it cannot
inherit the mistakes of the Rust kernels it is meant to check.

The plan's definition of done is *"within 10% of an established tool on the
same hardware"*. Intel MLC and lmbench are not installable on this host, so the
second opinion had to be built. These two programs are that: independently
written, no shared code with `crates/membench-core`, no inline assembly, and
explicit about what each number does and does not include.

```
mbstream.c   read / write / copy bandwidth per cache level
mblatency.c  dependent pointer-chase latency, size sweep + per-level cells
Makefile     builds both into bin/
run.sh       build + whole grid + summary, JSON into out/
compare.py   membench vs reference, per cell, against the 10% gate
```

## Build

```sh
make                      # gcc -O3 -std=gnu11 -pthread -march=native -Wall -Wextra
make NATIVE=              # drop -march=native (portable code generation)
make NATIVE=-march=x86-64-v2
make OPT=-O2
make scalar               # rebuild with -DMB_FORCE_SCALAR=1 (see "scalar" below)
make clean                # remove bin/ and out/
```

Both compile warning-free with `-Wall -Wextra` (and with `-Werror`). Nothing is
linked beyond glibc.

## Run

```sh
./run.sh                  # build, 4 bandwidth levels, latency sweep, summary
TRIALS=7 DURATION=300 ./run.sh
LAT_SIZES=auto ./run.sh                          # the four level cells only
MBSTREAM_EXTRA=--isa\ scalar ./run.sh            # portable source loops
python3 compare.py --reference out --membench /tmp/membench-run.json
```

`run.sh` needs no privileges: no `mlock`, no `sysctl`, no root. Cache levels get
one thread per **physical** core (two SMT siblings sharing one L1d measure
something that is not L1d); the memory level gets every **logical** CPU, because
DRAM bandwidth scales with channels and outstanding requests rather than with
cores. Both counts come from
`/sys/devices/system/cpu/cpu*/topology/thread_siblings_list`, and the CPU list is
printed so a run can be reproduced.

## What mbstream measures

* **Levels.** `--level l1d|l2|l3|mem`. Buffer sizing follows the plan: 0.5 × L1d
  and 0.5 × L2 per thread, 0.75 × L3 divided over the cores in the L3 domain with
  a floor of 2 × L2 (threads are dropped before the floor is inflated), and
  `max(1 GiB, 16 × total L3)` divided over logical CPUs for memory, clamped to
  half of `MemAvailable` and rounded down to a page. Cache geometry is read from
  sysfs, never hardcoded.
* **Accounting.** SI units throughout: 1 GB/s = 10⁹ B/s. **Copy counts source
  bytes only** (the AIDA convention); `--copy-counts-both` switches to
  source + destination and says so on every line of output.
* **Threads.** Pinned with `pthread_setaffinity_np` *before* allocating, so the
  buffer is faulted by the core that will own it; affinity is re-verified at the
  start of every timed trial, because a trial measured on the wrong core is not
  worth keeping.
* **Buffers.** `mmap` private anonymous, over-mapped and trimmed to a 2 MiB
  aligned window, `MADV_HUGEPAGE` requested, then one word per 4 KiB prefaulted
  before anything is timed. Huge pages are then **verified** through
  `AnonHugePages` in `/proc/self/smaps` and reported as granted/refused — in this
  container the kernel refuses them and the output says
  `huge pages: 0 of N bytes granted`.
* **Trials.** One untimed warm-up pass sizes the repetition count (the minimum
  across threads, so every thread does identical work), then N timed trials; the
  **median** is the headline and the best trial is also reported. A trial's rate
  is total bytes over earliest-start → latest-end, which is the strictest honest
  definition of "this group moved this much data in this much wall-clock time".
* **Clock.** Each cell also reports the core clock measured with a dependent
  integer add chain at the end of the cell. It is never read from cpufreq: the
  governor's opinion and the core's actual frequency differ a lot on a boosting,
  shared host, and cycles-per-op claims need the real one. Two samples are taken
  and the **faster** one kept: preemption and SMT siblings can only make the
  chain look slower than the core was, never faster, so the larger sample is the
  closer estimate and averaging the contaminated one in would under-report.

## What mblatency measures

A dependent pointer chase: each load produces the address of the next one, so
there is nothing for the hardware to overlap and the number is latency, not
bandwidth.

* **One pointer per cache line.** A line holding eight pointers lets the chase
  walk inside one line eight times and report "L1" whatever the buffer size.
* **One closed cycle** via Sattolo's algorithm, not Fisher-Yates — Fisher-Yates
  yields many short cycles and after a few thousand loads the chase is just
  bouncing around inside L1.
* **`--sizes auto`** expands to the buffer sizes membench's own per-level latency
  cells use (0.5 × L1d, 0.5 × L2, the L3 share, the memory share), so latency
  lines up cell-for-cell instead of merely having the same shape. Mixing is
  allowed: `--sizes auto,4K,32K,256K,1M,16M,256M` gives the level cells plus a
  sweep, with duplicates dropped.
* **`ns/load`** is the median of trials; **cycles/load** comes from the
  add-chain clock measured on the same core right afterwards. Zen4 territory:
  ~4 cycles L1, ~12 L2, ~50 L3, and low hundreds for DRAM with 4 KiB pages
  (page-walk cost included, see limitations).
* Pinned to a single CPU (`--cpu N`), pinned before allocating, and verified
  after: an unpinned chase measures the scheduler.

## Limitations, honestly

* **These are not AIDA64's numbers.** They are a second measurement of the same
  physical quantity, on the same host, by different code. Agreement is evidence;
  it is not calibration against AIDA64.
* **`--isa scalar` does not guarantee scalar instructions.** Nothing here uses
  inline assembly (deliberately: the point is portable, auditable C), so gcc is
  free to vectorise the portable loop. `--isa scalar` selects the portable *
  source*, and the output labels it as such. To actually force scalar code:
  `make EXTRA_CFLAGS=-fno-tree-vectorize`. The default kernels, by contrast,
  deliberately keep two independent streams per buffer half — with a single
  accumulator chain the measurement is limited by the adder, not the cache, which
  is how the first version of this file under-reported L1d read by ~1.9×.
* **No non-temporal paths.** There is no `write_nt` / `copy_nt` kernel here, so
  there is nothing to gate them against. compare.py prints membench's NT cells
  anyway, side by side with the reference's ordinary-store cell, under a heading
  that says *information only, never a gate failure* — comparing a streaming
  store against a write-back store measures the difference between two
  operations, not a disagreement between two tools. The NT implementation is
  therefore **not** verified by this reference; it is only watched.
* **Huge pages are usually refused here.** `MADV_HUGEPAGE` is requested at every
  level and the grant is checked. In this container the answer is zero, so the
  memory-level numbers include 4 KiB page-walk traffic and will sit below a bare
  metal run with THP working. Both programs print this rather than burying it,
  and they distinguish "requested and refused" from "not asked for"
  (`mblatency --no-huge`).
* **The L3 copy cell is part memory traffic.** Copy touches two buffers, so its
  footprint is twice the plan's per-thread share: 6 × 2 MiB × 2 = 24 MiB against
  a 16 MiB L3. That doubling is membench's own convention
  (`runner.rs::plan_for`), so it is reproduced here — and labelled, per the rule
  that a buffer bigger than its target level must say so.
* **A loaded host shows up in the numbers.** The aggregate window ends when the
  *last* thread finishes, so one busy core sets the pace of a whole cell. Every
  cell therefore prints the per-thread minimum/maximum and the measured clock
  range; on this box those lines regularly show 1.7 GHz on one core and 4.2 GHz
  on its neighbours, which explains a low aggregate better than a footnote would.
  `run.sh` prints the load average up front for the same reason.
* **Aggregate L1d read on six cores is power-limited, not cache-limited.** Single
  thread the read kernel moves ~250 GB/s per core at 4.3 GHz; six cores together
  reach ~900 GB/s while the clock drops to 1.7–4.2 GHz across cores, which puts
  the L1d cell only ~1.2× above the L2 cell instead of ~2×. That is the machine,
  not the kernel: the same code on quiet cores gives 233 vs 131 GB/s per core.
  Compare per-thread numbers if you want the per-core hierarchy.
* **The measured clock is an effective rate.** On a core whose SMT sibling is
  busy, the add chain shares the core and reads low. It is labelled as measured
  on the workers while the other workers still ran.
* **Latency trials are capped at 100 ms each** (`TRIAL_MS_CAP`): a full pass of a
  1 GiB chain is already slow, and the quantity of interest is nanoseconds per
  load.
* **`compare.py` does not convert or normalise anything** beyond choosing
  median-vs-best. It will never turn a `GB/s` cell into a `ns` cell.

## The 10% gate

```sh
python3 compare.py --reference out --membench path/to/membench-run.json
python3 compare.py --reference out --membench run.json --metric best --gate 15
python3 compare.py --reference out --membench run.json --sweep
```

`compare.py` folds every JSON in the reference directory into a set of
`(level, op)` cells, folds membench's `--json` export into the same shape, and
prints one row per grid cell with the percentage difference and a status. Exit
codes: `0` everything inside the gate, `1` at least one cell outside it **or
missing**, `2` usage error, `3` an input file is missing or unreadable (a message
naming the file, never a traceback), `4` an input file that is not the JSON shape
it expects.

Because ns and GB/s both scale with the core clock, compare.py prints the clock
each side measured (`clock.mhz` from membench, `clock_mhz_at_start` from
mblatency) and says out loud when the two runs were not at the same clock — a
boosted run compared against a throttled one can fail the gate on every row for
reasons that have nothing to do with either program. The latency sweep gains a
cycles column for exactly that case: cycles per load are the clock-independent
comparison, and both sides compute them from an add-chain measurement rather
than from cpufreq.

Two deliberate sharp edges:

* **A missing cell is a failure.** An empty grid square must never look like a
  passing one, so `MISSING (reference)` exits non-zero.
* **`median` is the default comparison**, because the plan's headline is the
  median of trials. `--metric best` compares extrema, which is the noisiest
  possible choice on a shared host — use it to compare AIDA-style peaks only when
  the machine is otherwise idle.

## Conventions this code deliberately copies from the Rust side

| convention | where it comes from |
| --- | --- |
| SI units, 1 GB/s = 10⁹ B/s | `bandwidth.rs::GBPS` |
| copy counts source bytes | plan rule, AIDA behaviour |
| level fractions 0.5 / 0.5 / 0.75-with-2×L2-floor / max(1 GiB, 16×L3) | `sizes.rs` |
| pin before allocating, re-check affinity per trial | `bandwidth.rs` |
| prefault every page before timing | `alloc.rs` |
| request 2 MiB pages, verify with `smaps`, say so when refused | `alloc.rs` |
| median of trials, `spread_pct = (max-min)/median`, best = raw extremum | `stats.rs` |
| clock from a dependent add chain, never cpufreq | `timer.rs` |
| one pointer per line, single cycle, 100 ms trial cap | `latency.rs` |
| refuse to print a number the program cannot justify | plan's honesty rules |

Anything the two implementations disagree about on purpose is written down
above; anything they disagree about by accident is what `compare.py` is for.
