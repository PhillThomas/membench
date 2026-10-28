#!/usr/bin/env bash
#
# Cross-check membench against the independent C benchmarks in reference/.
#
# Built-in exit-code discipline: this reports and does not gate, unless asked
# to. A 10% per-cell agreement between two microbenchmarks requires an idle
# machine, and this project's development host is a container on a busy TrueNAS
# server where the same binary disagrees with itself by 30% at L1 between
# runs. A CI job that failed on that would be a CI job that teaches people to
# ignore CI. Run it by hand on a quiet box, with GATE set, when you want the
# verdict to mean something.

set -uo pipefail # not -e: each step's exit status is a result, not a crash

cd "$(dirname "$0")/.."
BIN=${BIN:-target/release/membench}

if [[ ! -x "$BIN" ]]; then
    echo "no $BIN — run: cargo build --release" >&2
    exit 2
fi
if [[ ! -d reference ]]; then
    echo "no reference/ directory" >&2
    exit 2
fi

echo "== building the reference suite"
if ! make -s -C reference >/tmp/membench-reference-build.log 2>&1; then
    echo "reference build failed; see /tmp/membench-reference-build.log" >&2
    exit 2
fi

echo "== running the reference suite (a few minutes)"
if ! (cd reference && ./run.sh >/tmp/membench-reference-run.log 2>&1); then
    echo "reference run failed; see /tmp/membench-reference-run.log" >&2
    exit 3
fi
grep -m1 "load average" /tmp/membench-reference-run.log || true

echo "== running membench"
out=$(mktemp /tmp/membench-compare-XXXXXX.json)
if ! "$BIN" run --no-tui --json "$out" >/dev/null 2>&1; then
    echo "membench run failed" >&2
    exit 3
fi

echo
echo "== compare, medians"
(cd reference && python3 compare.py --reference out --membench "$out" --gate "${GATE:-10}")
median=$?
echo
echo "== compare, best trial (membench's headline is the best trial; the median"
echo "   is the honest central value, so both are printed)"
(cd reference && python3 compare.py --reference out --membench "$out" --gate "${GATE:-10}" \
    --metric best)
best=$?

echo
echo "membench JSON kept at $out"
echo "reference JSON in reference/out/, its raw output in /tmp/membench-reference-run.log"

if [[ -n "${GATE:-}" ]]; then
    # Asked for a verdict, so give one: both statistics have to pass.
    if ((median != 0)); then exit "$median"; fi
    exit "$best"
fi

echo
echo "report only — set GATE=10 (or another percentage) to exit non-zero on a"
echo "cell over the gate. See the README's 'The gate, run twice' for why that"
echo "gate is not measurable on a loaded host."
exit 0
