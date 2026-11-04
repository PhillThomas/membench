#!/usr/bin/env bash
# Everything a machine can check without a human looking at numbers. Run it
# before pushing; `.github/workflows/ci.yml` is a thin wrapper around this file
# so CI and your laptop agree about what "passes" means.
#
#   scripts/ci.sh [--quick]     skip the real measurement steps
set -euo pipefail

cd "$(dirname "$0")/.."
QUICK=""
[[ "${1:-}" == "--quick" ]] && QUICK=1

export PATH="$HOME/.cargo/bin:$PATH"
export RUST_BACKTRACE=1

step() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[31mFAILED: %s\033[0m\n' "$*" >&2; exit 1; }

step "toolchain"
rustc --version
cargo --version
# Name a missing tool as the environment problem it is. On the DDR5 test box
# python3 was not installed, and everything the run said about the contract
# check was "the JSON document is not what the contract says": a red light
# blaming the code for a missing package, and an hour spent reading JSON that
# was never the problem. `--quick` stops before these steps, so it must not
# demand python3 it will never use.
if [[ -z "$QUICK" ]] && ! command -v python3 >/dev/null 2>&1; then
    fail "python3 is not installed, and the JSON contract and TUI steps need it"
fi

step "format"
cargo fmt --all --check || fail "cargo fmt --all would change files"

step "build (debug)"
cargo build --workspace --all-targets

step "clippy (-D warnings)"
# The lint allowances live at the crate roots with a reason next to them, so
# anything left over is a real finding.
cargo clippy --workspace --all-targets -- -D warnings

step "tests (debug)"
cargo test --workspace -- --test-threads=1

step "build (release)"
cargo build --release --workspace

step "tests (release)"
cargo test --release --workspace -- --test-threads=1

step "kernels are still our own assembly"
./scripts/check-kernels-asm.sh

step "cross-check: the aarch64 build compiles"
# The NEON kernels cannot be executed on an x86-64 box, so a compiler is the
# only cheap thing standing between them and rot. That bar was not met: four
# kernels ended in `mov {out}, ...` against an operand list that never declared
# `out`, so membench-core did not build for aarch64 at all — and nothing noticed
# for as long as the project existed, because nothing built it. The README
# promises "any x86-64 or aarch64 Linux", so the promise gets a check.
#
# A missing target is a skip locally and a failure in CI (REQUIRE_AARCH64), so
# that a broken toolchain cannot read as a pass here either.
if command -v rustup >/dev/null 2>&1 \
    && rustup target list --installed 2>/dev/null | grep -qx 'aarch64-unknown-linux-gnu'; then
    cargo check --workspace --all-targets --target aarch64-unknown-linux-gnu \
        || fail "the aarch64 build is broken, and the README promises aarch64 Linux"
elif [[ -n "${REQUIRE_AARCH64:-}" ]]; then
    fail "REQUIRE_AARCH64 is set but the aarch64 target is not installed (rustup target add)"
else
    printf 'rustup target aarch64-unknown-linux-gnu not installed here: skipping.\n'
    printf 'CI installs it, so this skip is local-only.\n'
fi

if [[ -n "$QUICK" ]]; then
    printf '\n--quick: stopping before anything that measures the machine\n'
    exit 0
fi

BIN=target/release/membench

step "self-test"
$BIN self-test || fail "self-test reports a broken machine or a broken build"

step "a real run, written to JSON"
JSON=$(mktemp --suffix=.json)
trap 'rm -f "$JSON"' EXIT
$BIN run --no-tui --trials 2 --duration-ms 50 --level l1,l2 --only read,latency \
    --json "$JSON" 2>/dev/null >/dev/null
# The expected version comes from the crate, not from this script, so the
# check cannot quietly fall behind a deliberate bump.
SCHEMA=$(sed -n 's/^pub const SCHEMA_VERSION: u32 = \(.*\);$/\1/p' \
    crates/membench-core/src/lib.rs)
# The same failure mode one step down: an empty SCHEMA arrives in python as
# int("") and comes out of the except clause as a contract violation.
[[ -n "$SCHEMA" ]] || fail "SCHEMA_VERSION no longer parses out of lib.rs — update this script"
python3 - "$JSON" "$SCHEMA" <<'PY' || fail "the JSON document is not what the contract says"
import json, math, sys
doc = json.load(open(sys.argv[1]))
assert doc["schema_version"] == int(sys.argv[2]), (doc["schema_version"], sys.argv[2])
assert doc["cells"], "no cells were measured"
for c in doc["cells"]:
    assert math.isfinite(c["value"]) and c["value"] > 0, c
    assert c["unit"] in ("GB/s", "ns"), c
    assert c["trials"], c
# Latency belongs in the document even when only two levels were asked for.
assert any(c["unit"] == "ns" for c in doc["cells"]), "no latency cell"
assert any(c["unit"] == "GB/s" for c in doc["cells"]), "no bandwidth cell"
print(f"  {len(doc['cells'])} cells, schema {doc['schema_version']}, generator {doc['generator']}")
PY

step "compare a run against itself"
JSON2=$(mktemp --suffix=.json)
trap 'rm -f "$JSON" "$JSON2"' EXIT
$BIN run --no-tui --trials 2 --duration-ms 50 --level l1 --only read --json "$JSON2" 2>/dev/null >/dev/null
$BIN compare "$JSON" "$JSON2" >/dev/null

step "sweep CSV"
csv=$($BIN sweep --duration-ms 20 --trials 1 2>/dev/null | head -1)
[[ "$csv" == size_bytes,* ]] || fail "sweep CSV header missing (got: $csv)"
echo "  $csv"

step "debug build refuses to time"
# Capture first, then match. Piping straight into grep under `pipefail` makes
# the pipeline take the benchmark's deliberate exit 1, and the check fails for
# succeeding — which is how this line first misled me.
dbg=$(target/debug/membench run --no-tui --level l1 --only read 2>&1 || true)
if printf '%s' "$dbg" | grep -qi "debug"; then
    echo "  refused, as it must"
else
    fail "a debug build produced numbers"
fi

step "TUI"
python3 scripts/tui-smoke.py "$BIN" | tail -1

if [[ -n "$QUICK" ]]; then
    step "external comparison"
    echo "  --quick: skipped (run scripts/compare-reference.sh on an idle machine)"
elif [[ -x reference/bin/mbstream && -x reference/bin/mblatency ]]; then
    step "external comparison against the C reference (report only)"
    # Report, do not gate. Two microbenchmarks agreeing within 10% per cell is
    # a statement about an idle machine, and a CI runner is not one: on this
    # project's host the same binary moves 30% at L1 between runs, so a hard
    # gate here would go red for reasons unrelated to the code and train
    # everyone to ignore the red. The table prints so a human reads it.
    BIN="$BIN" scripts/compare-reference.sh | tail -30
    echo "  report only — see the README's \"The gate, run twice\""
else
    step "external comparison"
    echo "  reference/bin/ is empty; scripts/compare-reference.sh builds it on demand"
fi

printf '\n\033[32mall machine-checkable gates passed\033[0m\n'
