#!/bin/sh
# run.sh - build the reference benchmarks and run the whole grid.
#
# What it does, in order:
#   1. make (gcc -O3 -std=gnu11 -pthread -march=native, -Wall -Wextra clean)
#   2. mbstream for l1d, l2, l3 and mem.  The cache levels get one thread per
#      PHYSICAL core (two SMT siblings inside one L1d measure something that is
#      not L1d); memory gets every LOGICAL CPU, because DRAM bandwidth scales
#      with channels and outstanding requests, not with cores.
#   3. mblatency over the sizes membench's own latency cells use, plus a sweep.
#   4. one JSON per program into out/, then a summary table.
#
# Unprivileged: no mlock, no sysctl, no root.  Transparent huge pages are
# requested and then verified; if the kernel refuses them the programs say so
# in the output and the run carries on, because a refused THP changes the
# number and hiding that would be worse than reporting it.
#
# Environment:
#   TRIALS, DURATION      bandwidth trials and window (ms)   [5, 200]
#   LAT_TRIALS, LAT_MS    latency trials and window (ms)     [5, 100]
#   LAT_SIZES             size list for the sweep            [auto,4K,...,1G]
#   MBSTREAM_EXTRA        extra flags for mbstream (e.g. --isa scalar)
#   MBLATENCY_EXTRA       extra flags for mblatency (e.g. --cpu 2)
#   NATIVE                passed through to make; `NATIVE= ./run.sh` drops
#                         -march=native entirely

set -eu

cd "$(dirname "$0")"

TRIALS=${TRIALS:-5}
DURATION=${DURATION:-200}
LAT_TRIALS=${LAT_TRIALS:-5}
LAT_MS=${LAT_MS:-100}
LAT_SIZES=${LAT_SIZES:-auto,4K,32K,256K,1M,16M,256M}
MBSTREAM_EXTRA=${MBSTREAM_EXTRA:-}
MBLATENCY_EXTRA=${MBLATENCY_EXTRA:-}
OUT=out

echo "== building =="
# ${VAR-default} rather than ${VAR:-default}: `NATIVE= ./run.sh` must really
# drop the flag, not fall back to it.
make "NATIVE=${NATIVE--march=native}" all

mkdir -p "$OUT"

# --- topology, straight from sysfs ----------------------------------------
# A CPU is its physical core's representative when it is the first id listed
# among its own SMT siblings.
CORES=""
ALL=""
for d in /sys/devices/system/cpu/cpu[0-9]*; do
    [ -r "$d/topology/thread_siblings_list" ] || continue
    cpu=$(basename "$d" | tr -d 'cpu')
    ALL="$ALL $cpu"
    first=$(cut -d, -f1 "$d/topology/thread_siblings_list" 2>/dev/null | tr -d ' \n' || echo "")
    first=$(echo "$first" | cut -d- -f1)
    [ "$first" = "$cpu" ] && CORES="$CORES $cpu"
done

NALL=$(echo "$ALL" | wc -w)
NCORE=$(echo "$CORES" | wc -w)
if [ "$NALL" -eq 0 ]; then
    echo "run.sh: no CPUs under /sys/devices/system/cpu -- refusing to guess" >&2
    exit 3
fi
[ "$NCORE" -eq 0 ] && NCORE=$NALL

echo
echo "physical core cpus :$(echo "$CORES" | tr ' ' '\n' | sed '/^$/d' | tr '\n' ' ')"
echo "logical cpu cpus   :$(echo "$ALL" | tr ' ' '\n' | sed '/^$/d' | sort -n | tr '\n' ' ')"
echo "cache levels       :$([ -r /sys/devices/system/cpu/cpu0/cache/index0/size ] && \
    for i in /sys/devices/system/cpu/cpu0/cache/index*; do \
        printf ' %s:%s(%s)' "$(cat $i/level)" "$(cat $i/size)" "$(cat $i/type)"; done)"
if [ -r /sys/kernel/mm/transparent_hugepage/enabled ]; then
    echo "thp setting        : $(cat /sys/kernel/mm/transparent_hugepage/enabled)"
    case "$(cat /sys/kernel/mm/transparent_hugepage/enabled)" in
        *"[never]"*) echo "                     (THP is off: the programs will report 0 huge pages granted)" ;;
    esac
else
    echo "thp setting        : unknown (sysfs not readable)"
fi
echo "load average       : $(cut -d' ' -f1-3 /proc/loadavg)  (a loaded host moves these numbers)"
echo

# --- bandwidth grid --------------------------------------------------------
for level in l1d l2 l3 mem; do
    if [ "$level" = mem ]; then
        threads=$NALL
        which="every logical CPU"
    else
        threads=$NCORE
        which="one thread per physical core"
    fi
    echo "=================================================="
    echo "== mbstream --level $level  ($threads threads, $which)"
    echo "=================================================="
    # shellcheck disable=SC2086
    ./bin/mbstream --level "$level" --threads "$threads" \
        --trials "$TRIALS" --duration "$DURATION" $MBSTREAM_EXTRA \
        --json "$OUT/mbstream-$level.json"
    echo
done

# --- latency ---------------------------------------------------------------
echo "=================================================="
echo "== mblatency --sizes $LAT_SIZES"
echo "=================================================="
# shellcheck disable=SC2086
./bin/mblatency --sizes "$LAT_SIZES" --trials "$LAT_TRIALS" --duration "$LAT_MS" \
    $MBLATENCY_EXTRA --json "$OUT/mblatency.json"

# --- summary ---------------------------------------------------------------
echo
echo "=================================================="
echo "== summary (GB/s, SI; median of trials)"
echo "=================================================="
python3 - "$OUT" <<'PY'
import json, os, sys

out = sys.argv[1]
# file tags are the CLI words: mbstream-mem.json, not mbstream-memory.json
levels = [("l1d", "L1d"), ("l2", "L2"), ("l3", "L3"), ("mem", "Memory")]
rows = {}
huge = {}
clocks = {}
for tag, _ in levels:
    path = os.path.join(out, f"mbstream-{tag}.json")
    if not os.path.exists(path):
        continue
    d = json.load(open(path))
    rows[tag] = {c["op"]: c for c in d["cells"]}
    huge[tag] = [(c["op"], c.get("huge_pages_bytes"), c.get("huge_mapped_bytes"))
                 for c in d["cells"]]
    clocks[tag] = [(c.get("clock_mhz_min"), c.get("clock_mhz_max")) for c in d["cells"]]

print(f"{'':<10}{'Read':>10}{'Write':>10}{'Copy':>10}{'clock MHz':>16}")
for tag, label in levels:
    r = rows.get(tag)
    if not r:
        print(f"{label:<10}{'-':>10}{'-':>10}{'-':>10}")
        continue
    cl = [c for c in clocks[tag] if c[0]]
    clk = f"{min(c[0] for c in cl):.0f}-{max(c[1] for c in cl):.0f}" if cl else "-"
    print(f"{label:<10}" + "".join(
        f"{r.get(op, {}).get('median', 0.0):>10.1f}" for op in ("read", "write", "copy"))
        + f"{clk:>16}")

# The aggregate is earliest-start to latest-end over all threads, so on a busy
# or power-limited host L1d and L2 can land within a few percent of each other
# even though each core is still 1.8x faster out of L1.  Print the per-thread
# medians too: that is where the cache hierarchy is visible.
print("\nper-thread median GB/s (slowest..fastest worker in the cell)")
print(f"{'':<10}{'Read':>18}{'Write':>18}{'Copy':>18}")
for tag, label in levels:
    r = rows.get(tag)
    if not r:
        continue
    cells_ = []
    for op in ("read", "write", "copy"):
        c = r.get(op)
        cells_.append("-" if not c
                      else f"{c.get('thread_gbps_min', 0.0):.0f}-{c.get('thread_gbps_max', 0.0):.0f}")
    print(f"{label:<10}" + "".join(f"{x:>18}" for x in cells_))
print("  (the aggregate above is one window for the whole group; these are the")
print("   threads inside it, so a package power limit or another tenant on this")
print("   host shows up here first)")

print("\nlatency cells (ns per dependent load, from the sizes membench uses):")
mpath = os.path.join(out, "mblatency.json")
if os.path.exists(mpath):
    d = json.load(open(mpath))
    for p in d.get("level_latency", []):
        print(f"  {p['level']:<8} {p['size_bytes']/1024:>9.0f} KiB   {p['ns']:>7.2f} ns"
              f"   {p['cycles']:>7.1f} cycles @ {p['clock_mhz']:.0f} MHz")
    pts = [p for p in d.get("points", []) if not p.get("size_bytes", 0) == 0]
    print("\nsweep:")
    for p in sorted(pts, key=lambda x: x["size_bytes"]):
        sz = p["size_bytes"]
        lab = f"{sz/1024:.0f}K" if sz < 1048576 else f"{sz/1048576:.1f}M"
        print(f"  {lab:>8} {p['ns']:>8.2f} ns  ({p.get('label') or 'sweep'})")

if any(huge.values()):
    print("\nhuge pages actually granted (verified in /proc/self/smaps):")
    for tag, label in levels:
        for op, got, mapped in huge.get(tag, []):
            if got is None:
                state = "unknown (no AnonHugePages line)"
            elif got == 0:
                state = "0 granted - kernel refused, 4 KiB pages used"
            else:
                state = f"{got} granted"
            print(f"  {label:<8} {op:<6} {state} of {mapped} bytes")
PY

echo
echo "json written to $OUT/:"
ls -1 "$OUT"
echo
echo "compare with:  python3 compare.py --reference $OUT --membench path/to/membench-run.json"
