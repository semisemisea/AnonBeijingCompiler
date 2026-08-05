#!/usr/bin/env bash
# Generate the SysY runtime assembly templates embedded into every
# compiler output (.s).
#
# Usage: scripts/gen_runtime_templates.sh [--check]
# Output: sysylib/runtime_riscv64.s, sysylib/runtime_aarch64.s (committed)

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="${IMAGE:-soyo-test-tools}"

MODE="${1:-generate}"
case "$MODE" in
    generate) ;;
    --check) ;;
    *)
        echo "usage: $0 [--check]" >&2
        exit 2
        ;;
esac

if [[ "$MODE" == "--check" ]]; then
    TMP_DIR="$(mktemp -d)"
    trap 'rm -rf "$TMP_DIR"' EXIT
    docker run --rm -u "$(id -u):$(id -g)" \
        -v "$ROOT/sysylib:/work/sysylib:ro" \
        -v "$TMP_DIR:/work/output" \
        -w /work/sysylib \
        --entrypoint /bin/sh \
        "$IMAGE" -c '
set -e
riscv64-linux-gnu-gcc -O2 -S -march=rv64gc \
    -fno-asynchronous-unwind-tables -fno-dwarf2-cfi-asm \
    -fno-builtin -fno-tree-loop-distribute-patterns \
    sylib.c -o /work/output/runtime_riscv64.s
aarch64-linux-gnu-gcc -O2 -S -march=armv8-a \
    -fno-asynchronous-unwind-tables -fno-dwarf2-cfi-asm \
    -fno-builtin -fno-tree-loop-distribute-patterns \
    sylib.c -o /work/output/runtime_aarch64.s
'
    cmp "$TMP_DIR/runtime_riscv64.s" "$ROOT/sysylib/runtime_riscv64.s"
    cmp "$TMP_DIR/runtime_aarch64.s" "$ROOT/sysylib/runtime_aarch64.s"
    echo "runtime templates are up to date"
else
    docker run --rm -u "$(id -u):$(id -g)" \
        -v "$ROOT/sysylib:/work/sysylib" \
        -w /work/sysylib \
        --entrypoint /bin/sh \
        "$IMAGE" -c '
set -e
riscv64-linux-gnu-gcc -O2 -S -march=rv64gc \
    -fno-asynchronous-unwind-tables -fno-dwarf2-cfi-asm \
    -fno-builtin -fno-tree-loop-distribute-patterns \
    sylib.c -o runtime_riscv64.s
aarch64-linux-gnu-gcc -O2 -S -march=armv8-a \
    -fno-asynchronous-unwind-tables -fno-dwarf2-cfi-asm \
    -fno-builtin -fno-tree-loop-distribute-patterns \
    sylib.c -o runtime_aarch64.s
'

    echo "generated:"
    ls -la "$ROOT/sysylib/runtime_riscv64.s" "$ROOT/sysylib/runtime_aarch64.s"
fi
