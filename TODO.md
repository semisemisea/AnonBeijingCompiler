# Backend TODO

Only pending work belongs in this file. Completed implementation notes and
historical measurements belong in commits, tests, or dedicated documentation.

## Current Priority: Spill Repair And Ion Register Allocation

### Scope And Non-Goals

- [ ] Preserve the sole pipeline: SysY -> RaanaIR -> generic VCode -> register
  allocation -> target ABI frame finalization -> GNU assembly.
- [ ] Keep target-specific production code in
  `anon_armv8/src/{abi,constants,instructions,labels,lower,regs}.rs`; retain
  generic allocation, frame, and emission ownership in `taki_mir`.
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

### Phase 1: Stabilize Spill Correctness

#### Regression Coverage

- [ ] Add deterministic integer, f32, and pointer-width spill fixtures with
  values live across calls, loops, and merge/block-parameter edges.
- [ ] Add synthetic VCode tests for a parameterized jump, diamond merge,
  parameterized and non-parameterized critical edges, repeated successor edges,
  loop backedges, mixed integer/float parameters, and branch-defined edge values.
- [ ] Add semantic parallel-copy tests for register/register, register/stack, and
  stack/register moves; two-way and three-way cycles; no-free-scratch handling;
  and temporary spill slots. Test resulting values, not only edit order.
- [ ] Add end-to-end cases combining local stack objects, outgoing arguments,
  register-argument backing slots, callee saves, and allocator spills. Include
  large frames that require AArch64 and RISC-V offset legalization.
- [ ] Run all spill fixtures at `-O0` and `-O1`. Capture the current baseline:
  logical spill units, spill/frame bytes, edits by kind, `ldr`/`str`, static
  instruction count, and allocation time.

#### Logical Spill-Unit Contract

- [ ] Define `SpillSlot` and `Output::num_spillslots` as logical allocator units.
  Convert units to bytes only in target ABI/frame code.
- [ ] Change `Function::spillslot_size(RegClass)` in
  `taki_mir/src/reg_alloc/function.rs` to return logical units. Current `Int` and
  `Float` values require one unit; vectors remain unsupported.
- [ ] Add a distinct ABI/frame query for bytes per logical spill unit. Retain the
  current eight-byte physical unit on AArch64 and RISC-V without returning `8`
  from the allocator-facing query.
- [ ] Update `taki_mir/src/vcode.rs`, `taki_mir/src/lib.rs`,
  `taki_mir/src/emit.rs`, `anon_armv8/src/abi.rs`, and
  `taki_mir/src/riscv64/abi.rs` together so slot counts, frame size, and emitted
  offsets cannot mix logical units with bytes.
- [ ] Use checked arithmetic for spill-slot alignment, frame size, and byte
  offsets; report an explicit compilation error instead of wrapping.
- [ ] Reject vector spill allocations and edits explicitly until vector register
  allocation and multi-unit target moves exist.

#### Slot Allocation And Frame Layout

- [ ] Make `Stack::allocstack()` allocate and align by
  `Function::spillslot_size(class)`, including the named-first/named-last policy
  for multi-unit slots if added to the local function contract.
- [ ] Route parallel-copy temporary slots through the same class-aware allocator;
  remove direct `num_spillslots += 1` allocation in `process_branch()`.
- [ ] Add unit tests for size-one and synthetic size-two slots, alignment holes,
  mixed-size allocation, returned slot naming, and temporary-copy slots.
- [ ] Centralize allocator spill addressing with frame helpers equivalent to
  `spill_base_bytes()`, `spill_slot_offset(slot)`, and `spill_region_end()`.
- [ ] Preserve and document the post-prologue layout: outgoing arguments, normal
  ABI/HIR stack objects, allocator spills, callee saves, and setup area.
- [ ] Assert that every spill access stays within the spill region; regions do not
  overlap; and final frame size satisfies target alignment.
- [ ] Test zero/one/multiple spills, alignment padding, coexisting stack regions,
  and the first out-of-range target load/store offset.

#### Edit Semantics And Verification

- [ ] Define `Output.edits` in runtime execution order by `ProgPoint`, including
  a deterministic order for edits at the same point. Remove implicit dependence
  on reverse scans and repeated vector reversal.
- [ ] Verify scratch preservation for stack-to-stack copies and parallel-copy
  cycles; a borrowed scratch register must be saved before its live value is
  destroyed and restored afterwards.
- [ ] Document and test edit width semantics: Int copies preserve a full
  eight-byte unit, Float spills/reloads use F32 instructions at eight-byte-strided
  slots, stack-to-stack copies preserve a complete unit, and cross-class edits
  are rejected.
- [ ] Add an allocator-independent verifier before
  `VCodeContainer::write_back_allocs` that checks allocation arity/order,
  constraints, register classes, fixed/reuse operands, clobber conflicts, edit
  points/order, and spill-slot bounds.
- [ ] Include function, instruction, operand, constraint, allocation, program
  point, and edit context in verifier failures.
- [ ] Keep post-writeback VCode verification and add tests for malformed allocator
  output as well as valid outputs from the current allocator.

#### Phase-1 Exit Criteria

- [ ] Pass new spill tests and focused functional cases
  `functional/92_register_alloc.sy`, `functional/93_nested_calls.sy`, and
  `functional/94_nested_loops.sy` at `-O0` and `-O1`.
- [ ] Pass `cargo test -p taki_mir`, `cargo test -p anon_armv8`,
  `cargo test --workspace`, and `cargo build -p soyo_compiler`.
- [ ] Run `cargo fmt --check` and `git diff --check`; document any external tool
  failure separately from compiler failures.
- [ ] Freeze Phase-1 metrics as the Ion baseline. Do not require this phase to
  reduce block-parameter stack traffic.

### Phase 2: Make VCode Valid Ion Input

#### Strict SSA And Dense Numbering

- [ ] Add a strict VCode SSA/CFG validator: one definition per VReg, dominance of
  every use, block parameters as block-entry definitions, no entry live-ins,
  terminators at block ends, symmetric CFG metadata, and edge argument
  arity/class agreement.
- [ ] Repair repeated VReg definitions from rematerialized constants, floats, and
  globals in `taki_mir/src/lower.rs`. Prefer a fresh use-site VReg for each
  rematerialization unless a single dominating definition is required.
- [ ] Repair register-argument double definitions: model fixed incoming-register
  values and values loaded/copied from backing slots with distinct VRegs and
  explicit dataflow.
- [ ] Preserve operand order while making lowering SSA because reuse constraints
  index earlier operands and write-back consumes allocations in that order.
- [ ] Map the local pinned-PReg VReg representation to dense Ion VReg indices
  beginning at zero, then map allocations back for local write-back. Keep this
  mapping allocator-local rather than rewriting the repository-wide `Reg` format.

#### CFG, Calls, And Target Environment

- [ ] Split every critical edge before Ion; also retain dedicated edge blocks for
  value-carrying multi-successor edges. Preserve successor-index identity for
  distinct edges targeting the same block.
- [ ] Add CFG tests for repeated targets, parameterized/non-parameterized critical
  edges, edge-block copy ownership, loops, and backedges.
- [ ] Remove a call's fixed late result register, such as AArch64 `x0` or RISC-V
  `a0`, from that instruction's clobber set while preserving all other ABI
  clobbers.
- [ ] Assert that allocator scratch, post-RA scratch, SP/ZR/FP/LR, and fixed stack
  pseudo-registers are excluded from allocatable sets according to their role.
- [ ] Keep post-RA scratch metadata outside Ion's allocation environment; target
  frame-offset legalization must not consume an allocated register.
- [ ] Keep Ion SSA validation enabled while fixing input failures. Treat failures
  as lowering, operand, CFG, or ABI bugs; do not weaken validation to proceed.

#### Phase-2 Exit Criteria

- [ ] Run the strict validator on every VCode function reached by unit tests,
  focused functional tests, and `make test`.
- [ ] Keep the current allocator operational until all Phase-1 spill regressions
  and Phase-2 validation pass together.

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
