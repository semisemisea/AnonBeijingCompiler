# Backend TODO

Only pending work belongs in this file. Completed implementation notes and
historical measurements belong in commits, tests, or dedicated documentation.

## Current Priority: Spill Repair And Ion Register Allocation

### Scope And Non-Goals

- [ ] Port regalloc2 0.15.1 Ion into `taki_mir`; do not retain a permanent
  external compatibility wrapper, a second production allocator, a fallback path,
  or a user-facing allocator selection flag.
- [ ] Keep the allocator output consumer shared: Ion must produce the local
  allocations, edits, and logical spill-slot count used by VCode write-back,
  frame layout, and `AsmWriter`.
- [ ] Do not attempt to improve the current allocator's block-parameter
  coalescing, global live-range splitting, spill heuristics, or spill-slot reuse.
  Phase 1 establishes correctness; Ion replaces those allocation policies.
- [ ] Use `h_functional/29_long_line.sy -O1` as the allocator stress benchmark.
  Attribute edge-copy/frame improvements to register allocation separately from
  CFG cleanup, if-conversion, boolean simplification, and arithmetic lowering.

### Phase 3: Port And Enable Ion

#### Porting Work

- [ ] Port the required production code from regalloc2 0.15.1 Ion into
  `taki_mir/src/reg_alloc/ion/`: data structures, CFG/dominator support,
  liveness, live ranges, bundle merge, requirements, allocation/backtracking,
  splitting, spilling, move placement, parallel-copy handling, and redundant-move
  elimination.
- [ ] Preserve upstream copyright and Apache-2.0 WITH LLVM-exception notices,
  record the source version, and mark local modifications. Do not copy fuzzing,
  serialization, or debug-only modules without a production dependency.
- [ ] Reuse local `Function`, register/index, operand, allocation, `ProgPoint`,
  `Edit`, and `Output` semantics where they match Ion. Avoid a permanent
  regalloc2-to-local adapter layer.
- [ ] Map the local pinned-PReg VReg representation to dense Ion VReg indices
  beginning at zero, then map allocations back for local write-back. Keep this
  mapping allocator-local rather than rewriting the repository-wide `Reg` format.
- [ ] Keep exactly one parallel-move resolver after the port; migrate Ion's
  resolver or prove the existing one meets Ion's move contract, then delete the
  duplicate implementation.

#### Required Ion Behavior

- [ ] Include outgoing block arguments in predecessor live-out analysis and treat
  incoming block parameters as successor-entry definitions.
- [ ] Build an explicit `from_vreg@predecessor -> to_vreg@successor` relation for
  every block argument.
- [ ] Merge compatible block-parameter/input bundles. A successful merge assigns
  one allocation and emits no edge move.
- [ ] Honor fixed-register constraints, reuse constraints, late/early operand
  positions, and call clobbers during allocation and splitting.
- [ ] Implement pressure-driven spilling plus spill-set slot coloring/reuse using
  Phase-1 logical units; only frame/emission code may convert units to bytes.
- [ ] Place required edge moves before a single-successor predecessor terminator
  or at a single-predecessor successor entry. Rely on critical-edge splitting for
  all other cases.
- [ ] Emit only required register/register, register/stack, and stack/register
  edits. Ion's final edit stream must not contain stack-to-stack moves.
- [ ] Produce the local `Output` contract and require the Phase-1 output verifier
  to pass before VCode write-back.

#### Ion Tests And Cutover

- [ ] Add SSA VCode tests proving trivial block-parameter transfers elide moves,
  compatible diamond inputs coalesce, and interfering inputs receive only needed
  parallel copies.
- [ ] Test Ion with loops, calls, fixed/reuse operands, clobbers, edge-copy cycles,
  integer/float classes, pressure spills, and spill-slot reuse.
- [ ] Execute every focused fixture under QEMU and compare semantic output rather
  than exact physical registers, allocations, or edit sequences.
- [ ] Switch `taki_mir/src/lib.rs` to Ion only after Phase 1 and Phase 2 gates and
  all Ion tests pass. Do not add a silent fallback to the old allocator.
- [ ] Remove obsolete current allocator modules after Ion passes the full test and
  measurement period; retain shared validators, frame/emitter code, and tests.

### Phase 4: Measure And Accept The Migration

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
