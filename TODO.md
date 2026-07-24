# Compiler TODO

Only pending work belongs in this file. Completed implementation notes and
historical measurements belong in commits, tests, or dedicated documentation.

## Current Priority: RISC-V Branch Layout And ABI Conformance

The Ion CFG invariant and atomic RISC-V CFG transfer milestone is complete:
allocator-visible CFG targets are no longer held in allocatable virtual
registers, and `make test-riscv h_functional -- -O1` passes. Remaining work is
branch range/layout quality and a shared LP64D scalar argument layout.

### 1. Lower Branch Range Expansion After Register Allocation

- [ ] Keep logical `Jump` and conditional branch targets symbolic until after
  register allocation and edge-edit insertion. Do not allocate a program VReg
  merely to hold a static CFG label address.
- [ ] Initially expand all logical RISC-V CFG jumps using the reserved post-RA
  `x31` scratch register where a long form is required. `x31` and `x30` must
  remain absent from allocatable register sets; `x31` may be reused only after
  each spill/edit or long-jump sequence has consumed it.
- [ ] Define an explicit long unconditional form and an explicit long
  conditional form, with all control transfers contained in the expansion. Do
  not rely on an allocator-visible `LoadAddr`/`JumpReg` pair remaining adjacent.
- [ ] Add post-RA branch layout selection: direct conditional branch for B-type
  in-range targets, direct `j`/`jal` for in-range unconditional targets, and an
  inverted conditional over a fixed-scratch long jump when needed.
- [ ] Implement monotonic iterative branch relaxation after allocator edits are
  materialized. Recompute layout after every promotion because one long branch
  can make a later branch out of range.
- [ ] Test B-type and JAL boundary values in both directions, a second-pass
  promotion case, long loop backedges, long value-carrying edge blocks, and safe
  sequential reuse of `x31` by a stack-to-stack move followed by a long jump.
- [ ] Add fallthrough-aware branch inversion and empty jump-only edge threading
  only after correctness is established. Never thread through an edge block
  containing semantic work or regalloc edits.
- [ ] Defer Cranelift-style islands/veneers until the compiler has a structured
  post-RA layout representation with known instruction sizes, label-use ranges,
  and safe insertion points. Current direct text emission cannot safely port
  `MachBuffer` deadlines piecemeal.

### 2. Centralize RISC-V ABI Argument Locations

- [ ] Replace duplicated caller, callee, and outgoing-size loops with one shared
  RISC-V ABI argument-location computation. It must be consumed by
  `compute_arg_loc`, call lowering, and outgoing-area precomputation.
- [ ] Record per argument: integer or float register location, stack offset,
  storage width, alignment, and lowered load/store type. Use this record rather
  than independently maintaining integer/float counters in three locations.
- [ ] Match the current Cranelift LP64D scalar stack policy for normal calls:
  use independent `a0..a7` and `fa0..fa7` banks; use at least an XLEN-sized
  stack slot for overflow scalar arguments; align each slot; round the complete
  outgoing argument area to 16 bytes.
- [ ] Preserve the correct value access width inside an ABI slot: `i32` uses
  `sw`/`lw`, `f32` uses `fsw`/`flw`, and pointer values use `sd`/`ld`.
- [ ] Verify that incoming `s0`-relative offsets and outgoing `sp`-relative
  offsets are derived from the same signature and remain correct after frame
  legalization.
- [ ] Explicitly document unsupported RISC-V C psABI cases before claiming ABI
  interoperability: variadic floating-point classification, aggregates,
  register-pair alignment, split values, hidden return areas, and wider scalar
  types. Implement them only with accepted source-level requirements and tests.
- [ ] Add ABI unit and selector tests for 0/1/8/9 integer arguments, 0/1/8/9
  float arguments, independent exhaustion of each bank, alternating mixed
  arguments, and overflow pointer/i32/pointer/f32/pointer alignment.
- [ ] Add high-pressure call tests for integer and float fixed-register cycles,
  stack arguments, recursive calls, and values live across calls. Include a
  reduced `params_f40_i24`-style fixture that validates every overflow argument.

### 3. RISC-V Acceptance Gates

- [ ] First pass focused regressions at `-O0` and `-O1`:
  `h_functional/09_BFS.sy`, `10_DFS.sy`, `11_BST.sy`, `12_DSU.sy`,
  `16_k_smallest.sy`, `17_maximal_clique.sy`, `18_prim.sy`, `19_search.sy`,
  `20_sort.sy`, `21_union_find.sy`, `29_long_line.sy`, and `39_fp_params.sy`.
- [ ] Require the focused cases to execute with exact stdout and return-code
  agreement. Require `39_fp_params` to produce the expected
  `8: 7 5 6 5 5 6 9 8` integer-array line before considering ABI work complete.
- [ ] Verify generated assembly for focused regressions: no edge edit may occur
  between a conditionally reachable control transfer and its logical false leg;
  no allocator-visible CFG jump target may reside in an allocatable VReg; and
  all overflow ABI slots meet the declared width/alignment policy.
- [ ] Run `cargo fmt --check`, `git diff --check`, `cargo test -p taki_mir`,
  `cargo test --workspace`, `make test-riscv h_functional`, and
  `make test-riscv h_functional -- -O1`.
- [ ] Only after `h_functional` is clean, run `make test-riscv functional` and
  `make test-riscv functional -- -O1`, then record the exact pass/fail matrix
  and any remaining target-specific unsupported feature.
- [ ] Validate RISC-V `MemZero` only after the allocator and ABI gates above
  pass; test both inline 4-word clears and large `memset` fallback under `-O0`
  and `-O1`.
- [ ] Run the full AArch64 matrix after shared allocator or ABI changes:
  `make test`, `make test ARGS="-O 1"`, and `make test-llvm`. Treat any AArch64
  regression as a blocker because Ion and generic ABI/frame code are shared.

### Follow-Up Optimization Work

- [ ] Benchmark inline-clear thresholds on representative AArch64 and RISC-V
  targets. Compare code size, runtime, call overhead, alignment, and library
  implementation behavior; retain a threshold only with measurements.
- [ ] Add target-specific paired/wider clear stores only after the generic
  `MemZero` path is stable and verified.
- [ ] Design general memory intrinsics (`Memset`, `Memcpy`, `Memmove`) only when
  an accepted source feature needs arbitrary fill bytes, dynamic sizes, or bulk
  copies. Define target-width integer and alias/effect semantics first.
- [ ] Introduce target-aware data layout before supporting targets whose pointer
  width differs from the compiler host. Current `Type::size()` derives pointer
  size from the host and is not a complete target layout model.

## Parallel Priority: Measure And Accept Ion

- [ ] Re-run focused spill/call/loop/edge/large-frame/float cases at `-O0` and
  `-O1`, then run `cargo fmt --check`, `git diff --check`, crate tests, workspace
  tests, `make test`, `make test ARGS="-O 1"`, and `make test-llvm`.
- [ ] Compare Ion with the frozen Phase-1 baseline: allocation/compile time,
  logical spill units, frame bytes, edits by kind, spill loads/stores, static
  instruction count, and semantic output. Investigate regressions per function.
- [ ] For `h_functional/29_long_line.sy -O1`, require substantial reductions in
  block-parameter spill traffic and frame size. Track stack-to-stack moves,
  logical spill units, frame bytes, `ldr`/`str`, and total instructions.
- [ ] Do not require Ion to reduce labels or CFG branches. Schedule CFG cleanup,
  if-conversion, and branch layout as separate measured work.
- [ ] Remove obsolete current allocator modules only after Ion passes the full test
  and measurement period; retain shared validators, frame/emitter code, and tests.

## Follow-Up Backend Correctness

- [ ] Add VCode verification tests for terminators, edge argument counts/classes,
  target instruction verification failures, and allocation write-back.
- [ ] Add AArch64 MInst tests for fixed ABI constraints, reuse constraints,
  clobbers, metadata, GNU formatting, and immediate/address boundaries.
- [ ] Expand ABI tests for 0/1/8/9 integer and float arguments, mixed signatures,
  returns, recursion, live-across-call values, frame variants, and alignment.
- [ ] Add selector/memory/CFG tests for immediate and fused arithmetic, signed
  division/remainder/shifts, f32/NaN behavior, locals, arrays, globals,
  aggregates, GEPs, edge copies, and unsupported diagnostics.
- [ ] Confirm every accepted SysY HIR construct lowers through typed AArch64
  VCode, including globals, arrays, aggregates, dynamic GEPs, recursion, and
  edge-specific CFG transfers.

## Follow-Up Optimization Work

- [ ] Implement measured SimplifyCFG work: jump threading, empty-block merging,
  and elimination of jumps only when block layout and allocator edge work make it
  safe.
- [ ] Add post-boolean DPE/DCE/SimplifyCFG cleanup and measure whether a second
  fixed-point group is needed after boolean rewriting.
- [ ] Add domain-safe comparison-boundary canonicalizations and comparison-pair
  composition only with explicit integer/float semantic proofs.
- [ ] Add missing frontend/IR-shape tests for comparisons, `!x`, nested not,
  short-circuit expressions, and parameterized branch inversion.
- [ ] Add float tests for `+0.0`, `-0.0`, finite values, infinities, and NaN
  through frontend, RaanaIR, AArch64, and LLVM paths.
- [ ] Add and measure `cbz`/`cbnz`, `tbz`/`tbnz`, fallthrough-aware branch
  inversion, and `csel` for proven profitable side-effect-free patterns.
- [ ] Implement signed constant-power-of-two division/remainder strength reduction
  and quotient/remainder reuse; preserve truncation-toward-zero semantics.
- [ ] Measure address folding, immediate forms, shifted/extended operands,
  `madd`/`msub`, and safe load/store pairs before retaining target peepholes.

## Measurement And Deferred Scope

- [ ] Build reproducible measurement tooling that separates compiler,
  optimization, lowering/allocation/emission, assembly/linking, QEMU startup, and
  application runtime. Report repeated samples, code size, and instruction mix.
- [ ] Use QEMU for semantics and gross instruction-count checks only; use native
  AArch64 measurements for microarchitectural performance claims.
- [ ] Re-evaluate narrower physical spill storage only after Ion is stable.
  Current Int/Float values use one logical unit backed by eight bytes; vectors
  require new allocation, frame, move, and target-emission support.
- [ ] Defer general load/store pairing, f32 constant-pool deduplication,
  aggressive block placement, SIMD/vector allocation, indirect calls, tail calls,
  exceptions, TLS, atomics, and unneeded AArch64 extensions until accepted HIR
  requires them.
