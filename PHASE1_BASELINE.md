# Phase 1 Register Allocation Baseline

This records the pre-Ion allocator baseline required for the Phase 1 exit.
Re-run it after changing register allocation, frame layout, or spill emission:

```sh
cargo build -p soyo_compiler
target/debug/soyo_compiler --target aarch64 --emit asm -O 1 \
  --log taki_mir::reg_alloc=debug \
  -o /tmp/29_long_line.s tests/h_functional/29_long_line.sy
rg -c '^\s*ldr\b' /tmp/29_long_line.s
rg -c '^\s*str\b' /tmp/29_long_line.s
rg -c '^\s*[A-Za-z][A-Za-z0-9.]*\b' /tmp/29_long_line.s
```

The allocation log reports per-function logical spill units, spill/frame bytes,
edit kinds, and allocation time. The instruction count intentionally includes
assembly directives and labels so it remains a stable text-level comparison.

## 2026-07-24, AArch64, `h_functional/29_long_line.sy -O1`

| Function | Logical spill units | Spill bytes | Frame bytes | Edits | Reg->stack | Stack->reg | Stack->stack | Allocation time |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `fib` | 622 | 4,976 | 5,072 | 1,770 | 627 | 1,143 | 0 | 15,593 us |

| `ldr` | `str` | Static assembly lines |
| ---: | ---: | ---: |
| 1,174 | 649 | 6,461 |

The end-to-end harness also passed this case at `-O1` in 178.60 ms. Runtime
includes compilation, linking, and QEMU execution and is informational only.
