#!/usr/bin/env bash
# PLAN.md rule 1: the measured loops have to be the loops in the source, not a
# call into libc. This compiles the library the way the release build does and
# fails if any kernel function references memcpy, memset or memmove — which is
# what happens when an ordinary Rust loop gets optimised into a libc call.
#
# Why --emit=asm and not objdump: the release profile strips symbols, so an
# objdump-based scan sees a handful of symbols and proves nothing.
#
#   scripts/check-kernels-asm.sh [-v]
set -euo pipefail

cd "$(dirname "$0")/.."

VERBOSE=""
if [[ "${1:-}" == "-v" ]]; then VERBOSE=1; fi

export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || { echo "cargo not found (source ~/.cargo/env)" >&2; exit 2; }

OUT=target/asm-check
mkdir -p "$OUT"

# The library only, with the real release settings, down to assembly.
cargo rustc -q -p membench-core --release --lib --target-dir "$OUT" -- --emit=asm 2>/dev/null

ASM=$(ls -t "$OUT"/release/deps/membench_core-*.s 2>/dev/null | head -1)
if [[ -z "$ASM" ]]; then
    echo "no assembly produced (looked in $OUT/release/deps)" >&2
    exit 2
fi

# Attribute every copy/fill to the function it appears in. Two shapes count: a
# call into libc, and `rep movs`/`rep stos`, which is what LLVM emits when it
# inlines a copy itself. The inline asm lives in `kernels::`, so a hit inside
# one of those labels means the measurement is measuring something else.
#
# The label count is checked first: a scan over a file that contains no kernel
# labels would pass, and a pass like that is worse than no check at all.
if VERBOSE="$VERBOSE" awk '
    /^[A-Za-z_$][A-Za-z0-9_$]*:/ {
        current = $1
        sub(/:$/, "", current)
        if (current ~ /kernels/) kernel_labels++
        next
    }
    /(call|jmp)[ \t]+_?_(memcpy|memset|memmove|bcopy)/ || /[ \t]rep[ \t]+(movs|stos)/ {
        if (!(current in seen)) { seen[current] = $0 }
        if (current ~ /kernels/) kernel_hits++
        else other_hits++
    }
    END {
        printf "kernel functions in the assembly: %d\n", kernel_labels + 0
        printf "kernel functions that copy or fill outside our asm: %d\n", kernel_hits + 0
        printf "other functions that do: %d (formatting and collection code, not measurement)\n", other_hits + 0
        if (ENVIRON["VERBOSE"]) { for (f in seen) printf "  %s\n    %s\n", f, seen[f] }
        if ((kernel_labels + 0) < 20) {
            print "only a handful of kernel labels — this scan cannot prove anything"
            exit 2
        }
        exit (kernel_hits > 0) ? 1 : 0
    }
' "$ASM"; then
    echo "ok: $(basename "$ASM") — the measured loops are the loops in the source"
else
    cat >&2 <<'EOF'
FAILED: a kernel function copies or fills outside the inline asm.

Two ways to get here: a loop in kernels/ was written in ordinary Rust and the
optimiser turned it into a libc call or a `rep movs` (so you are timing glibc
or the string unit rather than your own loop), or a safe wrapper was added
around a hot loop. Put the loop back into an `asm!` block.

Run with -v to see the offending labels.
EOF
    exit 1
fi
