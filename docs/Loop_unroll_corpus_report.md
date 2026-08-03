# U1 Loop Unroll Corpus Report

## Scope

The U1 full-unroll policy was measured at `-O2` on all 214 SysY corpus cases for
both AArch64 and RISC-V. The compared modes were:

- `--loop-unroll on`: apply the transformation.
- `--loop-unroll off`: omit the pass from the IR pipeline.
- `--loop-unroll dry-run --pass-stats`: run the same candidate analysis without
  rewriting IR and emit structured statistics.

The harness produced final Raana IR and target assembly for every case. Both
targets also passed the complete QEMU runtime suite in `on` mode: 214/214.
`off` and `dry-run` target assembly was byte-identical across the complete
corpus.

## Candidate Funnel

The IR-level candidate counts were identical for both targets because U1 uses a
target-independent policy:

| Stage | Loops | Share of previous stage |
| --- | ---: | ---: |
| Natural loops observed | 1183 | - |
| Supported two-block shape | 425 | 35.9% |
| Exact constant trip count | 126 | 29.6% |
| Accepted by `8/64` | 43 | 34.1% |

The accepted trip-count distribution was:

| Trip count | Accepted loops |
| ---: | ---: |
| 2 | 4 |
| 3 | 18 |
| 4 | 5 |
| 5 | 7 |
| 6 | 1 |
| 7 | 2 |
| 8 | 6 |

The main rejection counts were:

| Reason | Loops |
| --- | ---: |
| Unsupported loop shape | 755 |
| Non-constant trip count | 199 |
| Trip count above 8 | 79 |
| No supported strict exit | 73 |
| No basic induction variable | 27 |
| Projected size above 64 | 4 |
| Unsupported header edges | 3 |

## Code Size

The report counts non-empty final IR instruction lines and non-directive target
assembly instruction lines. This is a static instruction-count proxy rather
than object-file byte size.

| Target | IR off | IR on | IR delta | Assembly off | Assembly on | Assembly delta |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| AArch64 | 45416 | 45940 | +524 (+1.15%) | 65139 | 65629 | +490 (+0.75%) |
| RISC-V | 45182 | 45706 | +524 (+1.16%) | 104164 | 104585 | +421 (+0.40%) |

Only 22 AArch64 cases and 23 RISC-V cases changed final assembly. The largest
increases came from dedicated pointer-strength-reduction debug cases, where the
small exact loops intentionally contain multiple address expressions.

## Threshold Sensitivity

For the 126 exact-trip candidates, simulated acceptance counts were:

| Trip limit | Size 32 | Size 48 | Size 64 | Size 96 |
| ---: | ---: | ---: | ---: | ---: |
| 4 | 24 | 27 | 27 | 27 |
| 6 | 32 | 35 | 35 | 35 |
| 8 | 33 | 37 | 43 | 44 |
| 10 | 33 | 38 | 54 | 57 |
| 16 | 33 | 38 | 54 | 58 |

The size limit of 64 is not currently the dominant restriction. All 43 accepted
loops have projected size at most 49. Four exact-trip loops with trip count at
most 8 exceed 64; only one additional loop would be admitted by raising the
size limit from 64 to 96.

Raising the trip limit from 8 to 10 would admit 11 additional loops whose
projected sizes are 41, 51, or 61. They occur in sorting, max-flow,
many-parameter, and nested-call functional cases. This is a material policy
expansion and requires runtime or cycle evidence before adoption.

## Decision

Keep `MAX_FULL_UNROLL_TRIPS = 8` and `MAX_UNROLLED_NON_TERMINATORS = 64`.

- The current policy hits 43 loops while limiting whole-corpus assembly growth
  to less than 0.8% on both targets.
- Raising the size cap alone adds almost no coverage.
- Raising the trip cap to 10 adds 11 candidates, but this report measures only
  static size and correctness, not a demonstrated speedup.
- The structured dry-run data now makes a future `8` versus `10` performance
  experiment reproducible without changing the public analysis format.

The raw generated reports are intentionally kept under ignored `results/u1/`.
They can be regenerated with `tests/test.py` and
`scripts/loop_unroll_report.py`.
