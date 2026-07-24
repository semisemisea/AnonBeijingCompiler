# Compiler TODO

Only pending work belongs in this file. Completed implementation notes and
historical measurements belong in commits, tests, or dedicated documentation.

## Current Priority: Bulk Local Array Initialization Follow-Up

- [ ] Implement and measure a small-clear inline-store policy before replacing the
  current target `memset` calls for small local arrays.
- [ ] Validate RISC-V `MemZero` code generation after fixing the pre-existing
  incoming-register argument spill model, which currently violates the allocator
  contract for an allocatable physical argument register.
- [ ] Run the full AArch64 and RISC-V acceptance matrix after the focused coverage
  and RISC-V entry-argument repair are available.

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
