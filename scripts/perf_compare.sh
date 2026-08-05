#!/usr/bin/env bash
# Per-milestone static-code-size comparison for the perf corpus.
#
# Compiles the given perf cases with the current compiler at -O2 (AArch64),
# counts the instructions in each emitted .s, and prints a table against the
# stored reference artifacts in results/perf/ ( *_orig.s / *_sched.s /
# *_clang.s when present). Optionally runs gem5 for dynamic sim_insts.
#
# Usage:
#   scripts/perf_compare.sh [--gem5] [case ...]
#
# Output is written to results/perf_compare/{milestone}/table.tsv plus a
# human-readable summary on stdout. The static count is a *model-level*
# regression check only (see TODO.md §1.3); gem5 sim_insts are the
# hardware-modeled dynamic count, not an XCZU15EG measurement.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
COMPILER="${SOYO_COMPILER:-$ROOT/target/debug/compiler}"
TARGET=aarch64
OPT=-O2
RESULTS="$ROOT/results/perf_compare"
GEM5="${GEM5:-no}"
args=()
for arg in "$@"; do
    case "$arg" in
        --gem5) GEM5=yes ;;
        *) args+=("$arg") ;;
    esac
done
set -- "${args[@]+"${args[@]}"}"

# Millestone label (git short sha + branch) recorded for traceability.
MILESTONE="${SOYO_MILESTONE:-$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)}"
OUT_DIR="$RESULTS/$MILESTONE"
mkdir -p "$OUT_DIR"

cases=("$@")
if [ ${#cases[@]} -eq 0 ]; then
    cases=(huffman-01 conv2d-1 sl1 01_mm1)
fi

# Count real instructions in an emitted .s: lines that are not directives,
# labels, or empty.
count_insts() {
    local file="$1"
    local marker="${2:-}"
    local active=1
    if [[ -n "$marker" ]]; then
        active=0
    fi
    awk -v marker="$marker" -v active="$active" '
        marker != "" && index($0, marker) { active = 1; next }
        marker != "" && !active { next }
        /^[[:space:]]*$/ { next }
        /^[[:space:]]*\.[a-z]/ { next }
        /^[[:space:]]*\.L[a-z0-9_.]*:/ { next }
        /^[A-Za-z_.][A-Za-z0-9_.]*:/ { next }
        { n++ }
        END { print n+0 }
    ' "$file"
}

run_gem5_sim_insts() {
    # Reuse the latest harness-produced gem5 stats for this case when
    # present (fresh runs go through `make gem5`, which builds ELFs with
    # the container toolchain). Look in results/ and results/perf/.
    local case="$1"
    local stats=""
    for candidate in "$ROOT/results/$case.gem5-stats/stats.txt" "$ROOT/results/perf/$case.gem5-stats/stats.txt"; do
        if [ -s "$candidate" ]; then
            stats="$candidate"
            break
        fi
    done
    if [ -n "$stats" ]; then
        awk '/^simInsts[[:space:]]+[0-9]/ { print $2; exit }' "$stats" || true
    fi
}

printf '%-24s %8s %8s %8s %8s %10s\n' case current orig sched clang 'gem5 sim' > "$OUT_DIR/table.tsv"
printf '%-24s %8s %8s %8s %8s %10s\n' case current orig sched clang 'gem5 sim'

for case in "${cases[@]}"; do
    src="$ROOT/tests/perf/$case.sy"
    if [ ! -f "$src" ]; then
        echo "skip $case (no tests/perf/$case.sy)" >&2
        continue
    fi
    asm="$OUT_DIR/$case.s"
    "$COMPILER" -S "$OPT" --target "$TARGET" -o "$asm" "$src" >/dev/null 2>&1
    current=$(count_insts "$asm" '# SOYO_PROGRAM_ASM_BEGIN')
    sim="-"
    if [ "$GEM5" = "yes" ]; then
        sim=$(run_gem5_sim_insts "$case")
        sim=${sim:-"-"}
    fi
    orig="-"
    sched="-"
    clang="-"
    [ -f "$ROOT/results/perf/${case}_orig.s" ] && orig=$(count_insts "$ROOT/results/perf/${case}_orig.s")
    [ -f "$ROOT/results/perf/${case}_sched.s" ] && sched=$(count_insts "$ROOT/results/perf/${case}_sched.s")
    [ -f "$ROOT/results/perf/${case}_clang.s" ] && clang=$(count_insts "$ROOT/results/perf/${case}_clang.s")
    printf '%-24s %8s %8s %8s %8s %10s\n' "$case" "$current" "$orig" "$sched" "$clang" "$sim" >> "$OUT_DIR/table.tsv"
    printf '%-24s %8s %8s %8s %8s %10s\n' "$case" "$current" "$orig" "$sched" "$clang" "$sim"
done
printf '\nstatic .s instruction counts (model-level only, see TODO.md §1.3)\n'
printf 'table: %s\n' "$OUT_DIR/table.tsv"
