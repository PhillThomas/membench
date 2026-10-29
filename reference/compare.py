#!/usr/bin/env python3
"""compare.py -- membench vs the C reference, cell by cell, against the 10% gate.

    compare.py --reference reference/out --membench path/to/membench-run.json

The definition of done in the membench plan is "within 10% of an established
tool on the same hardware".  MLC and lmbench are not installable here, so the
second opinion is the C reference in this directory (mbstream / mblatency).
This script lines the two up and says, per grid cell, how far apart they are.

Exit codes:
    0  every comparable cell is inside the gate
    1  at least one cell is outside the gate, or a cell is missing
    2  usage error (no arguments prints usage and exits here)
    3  an input file is missing or unreadable -- reported, never a traceback
    4  an input file exists but is not JSON we understand

Both sides keep their own units and their own definitions (see README.md); this
script does not average, smooth or convert anything beyond choosing which
statistic to compare.
"""

from __future__ import annotations

import argparse
import json
import os
import sys

# Cell order: (level, op).  Latency comes from mblatency, the rest from mbstream.
CELLS = [
    ("l1d", "read"), ("l1d", "write"), ("l1d", "copy"), ("l1d", "latency"),
    ("l2", "read"), ("l2", "write"), ("l2", "copy"), ("l2", "latency"),
    ("l3", "read"), ("l3", "write"), ("l3", "copy"), ("l3", "latency"),
    ("memory", "read"), ("memory", "write"), ("memory", "copy"),
    ("memory", "latency"),
]

# membench's JSON uses "l1"/"memory"; the C tools use the CLI words.
LEVEL_ALIAS = {
    "l1": "l1d", "l1d": "l1d", "l1 cache": "l1d",
    "l2": "l2", "l2 cache": "l2",
    "l3": "l3", "l3 cache": "l3",
    "mem": "memory", "memory": "memory", "dram": "memory",
}

OP_ALIAS = {
    "read": "read",
    "write": "write",
    "write_nt": "write_nt",
    "copy": "copy",
    "copy_nt": "copy_nt",
    "latency": "latency",
}

# The reference has no non-temporal paths, so the memory row is compared against
# the ordinary-store cells; say so instead of comparing different things.
OP_FALLBACK = {
    "write_nt": "write",
    "copy_nt": "copy",
}


def die(code: int, msg: str) -> "None":
    print(f"compare.py: {msg}", file=sys.stderr)
    sys.exit(code)


def load_json(path: str, what: str) -> dict:
    if not os.path.exists(path):
        die(3, f"{what} does not exist: {path}\n"
               f"            build and run the benchmarks first (./run.sh), and pass the\n"
               f"            path membench wrote with --json")
    if os.path.isdir(path):
        die(3, f"{what} is a directory, not a file: {path}")
    try:
        with open(path, "r", encoding="utf-8") as fh:
            return json.load(fh)
    except OSError as exc:
        die(3, f"cannot read {what} ({path}): {exc}")
    except json.JSONDecodeError as exc:
        die(4, f"{what} is not valid JSON ({path}): {exc}")
    return {}  # unreachable


def norm_level(v) -> str:
    return LEVEL_ALIAS.get(str(v).strip().lower(), str(v).strip().lower())


def norm_op(v) -> str:
    return OP_ALIAS.get(str(v).strip().lower(), str(v).strip().lower())


def pick(cell: dict, metric: str) -> "float | None":
    """The statistic to compare.  Default 'median': the plan's headline is the
    median of trials, and 'best' is the raw extremum, which is exactly the
    number most sensitive to a noisy host.

    No silent mixing: 'median' reads `median` only (falling back to `value` if
    a writer omitted it), 'best' reads `best` then `value`.
    """
    keys = ("median", "value") if metric == "median" else ("best", "value")
    for key in keys:
        v = cell.get(key)
        if isinstance(v, (int, float)) and v > 0:
            return float(v)
    return None


def read_reference(directory: str, metric: str) -> dict:
    """Fold every JSON file in the reference output directory into one
    {(level, op): value} map, plus a map of the latency sweep points."""
    if not os.path.isdir(directory):
        die(3, f"reference directory does not exist: {directory}\n"
               f"            run ./run.sh first, or point --reference at the out/ it writes")

    cells: dict = {}
    provenance: dict = {}
    sweep: dict = {}
    clock: dict = {}
    files = sorted(f for f in os.listdir(directory) if f.endswith(".json"))
    if not files:
        die(3, f"no .json files in {directory} -- ./run.sh writes them there")

    for name in files:
        path = os.path.join(directory, name)
        doc = load_json(path, f"reference file {name}")
        tool = str(doc.get("tool", ""))

        if tool == "mbstream":
            if not doc.get("optimised_build", True):
                print(f"compare.py: NOTE: {name} came from an UNOPTIMISED build; its numbers "
                      f"are not comparable", file=sys.stderr)
            for cell in doc.get("cells", []):
                level = norm_level(cell.get("level", "?"))
                op = norm_op(cell.get("op", "?"))
                v = pick(cell, metric)
                if v is None:
                    continue
                cells[(level, op)] = v
                provenance[(level, op)] = (name, cell)
        elif tool == "mblatency":
            if not doc.get("optimised_build", True):
                print(f"compare.py: NOTE: {name} came from an UNOPTIMISED build", file=sys.stderr)
            if isinstance(doc.get("clock_mhz_at_start"), (int, float)):
                clock = {"mhz": float(doc["clock_mhz_at_start"]),
                         "cpu": doc.get("cpu"), "file": name}
            for point in doc.get("level_latency", []) or []:
                level = norm_level(point.get("level", "?"))
                v = point.get("ns")
                if isinstance(v, (int, float)) and v > 0:
                    cells[(level, "latency")] = float(v)
                    provenance[(level, "latency")] = (name, point)
            for point in doc.get("points", []) or []:
                size = point.get("size_bytes")
                v = point.get("ns")
                if isinstance(size, int) and isinstance(v, (int, float)) and v > 0:
                    sweep[size] = {"ns": float(v), "cycles": point.get("cycles")}
        else:
            print(f"compare.py: NOTE: {name} is not a reference result "
                  f"(no \"tool\" field); skipped", file=sys.stderr)

    if not cells:
        die(4, f"no usable cells in {directory}: the JSON does not look like mbstream/"
               f"mblatency output")
    return {"cells": cells, "provenance": provenance, "sweep": sweep, "dir": directory,
            "clock": clock}


def read_membench(path: str, metric: str) -> dict:
    doc = load_json(path, "membench result")
    cells: dict = {}
    provenance: dict = {}
    if not isinstance(doc, dict) or "cells" not in doc:
        die(4, f"{path} has no \"cells\" array, so it is not a membench run export")

    # Schema 2 (issue #1 fixes) made the SPD timings floats and renamed
    # cas_string to cycles; neither is read here, but a note that names a stale
    # version is noise that teaches everyone to ignore notes.
    if doc.get("schema_version") != 2:
        print(f"compare.py: NOTE: membench schema_version is "
              f"{doc.get('schema_version')!r}, this script expects 2", file=sys.stderr)

    for cell in doc.get("cells", []):
        if not isinstance(cell, dict):
            continue
        level = norm_level(cell.get("level", "?"))
        op = norm_op(cell.get("op", "?"))
        v = pick(cell, metric)
        if v is None:
            continue
        cells[(level, op)] = v
        provenance[(level, op)] = ("membench", cell)

    sweep = {}
    sw = doc.get("sweep") or {}
    for point in sw.get("points", []) or []:
        size = point.get("size_bytes")
        v = point.get("ns")
        if isinstance(size, int) and isinstance(v, (int, float)) and v > 0:
            sweep[size] = {"ns": float(v), "cycles": point.get("cycles")}

    clock = {}
    clk = doc.get("clock")
    if isinstance(clk, dict) and isinstance(clk.get("mhz"), (int, float)):
        clock = {"mhz": float(clk["mhz"]), "cpu": clk.get("cpu"), "field": "clock.mhz"}

    return {"cells": cells, "provenance": provenance, "sweep": sweep, "path": path,
            "clock": clock}


def main(argv) -> int:
    ap = argparse.ArgumentParser(
        prog="compare.py",
        description="Compare membench against the C reference benchmarks; "
                    "exit non-zero if any cell differs by more than the gate.",
        epilog="exits: 0 inside the gate, 1 a cell is over the gate or missing, "
               "2 usage, 3 missing/unreadable input, 4 unparseable input",
    )
    ap.add_argument("--reference", default="reference/out",
                    help="directory holding the reference JSON (default: reference/out)")
    ap.add_argument("--membench", default=None,
                    help="membench run JSON (its --json output)")
    ap.add_argument("--gate", type=float, default=10.0,
                    help="percentage difference that counts as a failure (default 10)")
    ap.add_argument("--metric", choices=("median", "best"), default="median",
                    help="which statistic to compare (default: median)")
    ap.add_argument("--sweep", action="store_true",
                    help="also print the latency sweep, size by size")

    if len(argv) < 2:
        # Explicitly required: no arguments prints usage and exits 2.  argparse
        # would print usage and exit 2 on a bad flag anyway.
        ap.print_help()
        print("\ncompare.py: both --reference and --membench are required", file=sys.stderr)
        return 2

    args = ap.parse_args(argv[1:])
    if not args.membench:
        ap.print_help()
        print("\ncompare.py: --membench is required", file=sys.stderr)
        return 2

    ref = read_reference(args.reference, args.metric)
    mb = read_membench(args.membench, args.metric)

    print(f"membench : {args.membench}")
    print(f"reference: {args.reference}  ({len(ref['cells'])} cells)")
    print(f"metric   : {args.metric}   gate: {args.gate:g}%")
    mb_clk, ref_clk = mb.get("clock") or {}, ref.get("clock") or {}
    if mb_clk or ref_clk:
        def fmt(c):
            if not c:
                return "not reported"
            cpu = "" if c.get("cpu") is None else f" (cpu {c['cpu']})"
            return f"{c['mhz']:.0f} MHz{cpu}"
        print(f"clock    : membench {fmt(mb_clk)}  |  reference {fmt(ref_clk)}"
              f"   [dependent add chain on both sides]")
        if mb_clk and ref_clk and abs(mb_clk["mhz"] - ref_clk["mhz"]) / mb_clk["mhz"] > 0.10:
            print("           NOTE: the two runs ran at clearly different core clocks. ns and")
            print("           GB/s scale with the clock, so these rows are comparing a boosted")
            print("           run against a throttled one; the sweep's cycles column is the")
            print("           clock-independent comparison. Re-run both on an idle host.")
    print()
    hdr = f"{'cell':<22}{'membench':>12}{'reference':>12}{'diff':>10}  status"
    print(hdr)
    print("-" * (len(hdr) + 22))

    failures = 0
    compared = 0
    for level, op in CELLS:
        label = f"{level:<8}{op:<9}"
        mb_v = mb["cells"].get((level, op))
        ref_v = ref["cells"].get((level, op))
        note = ""

        if mb_v is None and op in OP_FALLBACK:
            alt = OP_FALLBACK[op]
            mb_v = mb["cells"].get((level, alt))
            if mb_v is not None:
                note = f"(membench {alt})"

        unit = "ns" if op == "latency" else "GB/s"
        if mb_v is None or ref_v is None:
            missing = []
            if mb_v is None:
                missing.append("membench")
            if ref_v is None:
                missing.append("reference")
            print(f"{label:<22}{'-':>12}{'-':>12}{'-':>10}  MISSING ({' and '.join(missing)})")
            failures += 1
            continue

        diff = (ref_v - mb_v) / mb_v * 100.0
        compared += 1
        if abs(diff) > args.gate:
            status = f"OVER {args.gate:g}%"
            failures += 1
        else:
            status = "ok"
        print(f"{label:<22}{mb_v:>12.3f}{ref_v:>12.3f}{diff:>+9.1f}%  "
              f"{status:<10}{note}")

    # Non-temporal cells: the reference has no NT kernels at all, so there is
    # nothing to gate against.  Show the row anyway -- a write_nt that is wildly
    # off even against ordinary stores is worth seeing -- but never count it as
    # a gate failure, because that would be measuring the gap between two
    # different operations rather than a disagreement between two tools.
    nt_rows = [("memory", "write_nt"), ("memory", "copy_nt"),
               ("l3", "write_nt"), ("l3", "copy_nt")]
    shown = False
    for level, op in nt_rows:
        mb_v = mb["cells"].get((level, op))
        if mb_v is None:
            continue
        ref_op = OP_FALLBACK[op]
        ref_v = ref["cells"].get((level, ref_op))
        if not shown:
            print("\nnon-temporal cells (reference has no NT kernels; compared against the"
                  " ordinary-store\ncell and reported for information only, never as a gate "
                  "failure):")
            shown = True
        if ref_v is None:
            print(f"{level + ' ' + op:<22}{mb_v:>12.3f}{'-':>12}{'-':>10}  info: no "
                  f"reference {ref_op} cell")
            continue
        diff = (mb_v - ref_v) / ref_v * 100.0
        print(f"{level + ' ' + op:<22}{mb_v:>12.3f}{ref_v:>12.3f}{diff:>+9.1f}%  "
              f"info: vs reference {ref_op}")
    if shown:
        print()

    if args.sweep and ref["sweep"] and mb["sweep"]:
        print("\nlatency sweep (ns per dependent load; cycles when both sides report them)")
        print(f"{'size':>12}{'membench':>12}{'reference':>12}{'diff':>10}"
              f"{'mb cyc':>10}{'ref cyc':>10}{'diff':>10}")
        for size in sorted(set(ref["sweep"]) & set(mb["sweep"])):
            a, b = mb["sweep"][size], ref["sweep"][size]
            mb_str = f"{size / 1024:g}K" if size < 1024 * 1024 else f"{size / 1048576:g}M"
            ac, bc = a.get("cycles"), b.get("cycles")
            both = isinstance(ac, (int, float)) and isinstance(bc, (int, float)) and ac > 0
            cyc = (f"{ac:>10.2f}{bc:>10.2f}{(bc - ac) / ac * 100.0:>+9.1f}%" if both
                   else f"{'-':>10}{'-':>10}{'-':>10}")
            print(f"{mb_str:>12}{a['ns']:>12.3f}{b['ns']:>12.3f}"
                  f"{(b['ns'] - a['ns']) / a['ns'] * 100.0:>+9.1f}%{cyc}")

    print()
    if failures:
        print(f"{failures} cell(s) failed the {args.gate:g}% gate or were missing "
              f"({compared} compared).")
        print("A missing cell is a failure on purpose: an empty grid square must never")
        print("look like a passing one.")
        return 1
    print(f"all {compared} cells inside {args.gate:g}%")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
