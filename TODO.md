# AArch64 VCode Backend Plan

## Goal And Scope

Complete the production-quality AArch64 pipeline:

```text
Raana HIR
  -> AArch64 selector
  -> typed AArch64 VCode / MInst
  -> register allocation
  -> frame finalization
  -> post-RA pseudo expansion
  -> GNU AArch64 assembly
  -> clang + linker + QEMU
```

The output target is GNU AArch64 assembly for `aarch64-linux-gnu`. The
assembler and linker own ELF generation, relocations, branch-range handling,
and veneers. The compiler owns instruction selection, AAPCS64 ABI behavior,
frame layout, register allocation, pseudo expansion, and correct assembler
syntax.

This plan takes design direction from Cranelift's AArch64 backend, especially
its encoding-shaped MInst variants, typed immediates, address modes, condition
handling, and late pseudo expansion. It does not copy Wasmtime-specific JIT,
binary-emission, or ISLE infrastructure.

### Explicit Non-goals

- Do not add direct ELF object generation.
- Do not add JIT code generation, trap metadata, stack maps, or runtime
  patching machinery.
- Do not add compiler-side branch islands, veneers, or constant islands.
- Do not add SIMD, SVE, LSE atomics, pointer authentication, TLS, tail calls,
  or exception handling unless a concrete SysY requirement reaches the backend.
- Do not replace the direct production lowerer until every VCode exit gate
  below passes.

## Current Baseline

### Production Direct Backend

The existing production backend remains `anon_armv8/src/lower.rs`, selected by
`soyo_compiler -S` through `anon_armv8::compile_program_to_asm()`.

- Native QEMU functional suite: `100/100` passed.
- Native QEMU high-functional suite: `40/40` passed.
- LLVM reference functional suite: `100/100` passed.
- The direct backend already supports the full currently accepted SysY language:
  i32, f32, pointers, arrays, globals, calls, recursion, and control flow.
- The direct backend is intentionally retained as the semantic reference while
  VCode is completed.

### Existing VCode Coverage

The opt-in entry point is:

```rust
anon_armv8::compile_function_vcode(program, func)
```

It currently validates HIR -> VCode -> RA -> post-RA assembly for:

- i32 constants, arithmetic, signed division/remainder, bitwise operations,
  shifts, and signed comparisons.
- i32 branches, returns, direct calls, and direct void calls.
- Up to eight i32 register arguments in `w0..w7`.
- f32 entry arguments, `fadd`, direct f32 call arguments/results in `s0..s7`
  and `s0`, and f32 returns.
- i32 and f32 caller-save preservation and forced cross-call spill/reload.
- One-successor i32 block-parameter transfer.
- Typed i32, pointer, and f32 post-RA moves/spills, integer callee-save frame
  handling, and large spill offsets.

### Existing Constraints To Preserve

- Preserve typed SSA semantics, AAPCS64 behavior, signed arithmetic, NaN
  semantics, and block-parameter transfer semantics through every migration.
- Keep direct and VCode behavior comparable through focused HIR fixtures,
  Clang assembly validation, QEMU execution, and LLVM differential tests.
- Do not hide unsupported HIR behind `todo!()`, `unimplemented!()`, silent
  omission, or a backend panic reachable from the driver.
- Keep target-specific logic in `anon_armv8`; retain generic VCode and RA
  responsibilities in `taki_mir`.

### Known Independent Issue

- [ ] Decide whether to repair or separately document the `cargo test -p
  raana_ir` formatter snapshots. `raana_ir/src/fmt/writer.rs` currently enables
  verbose output while snapshots expect non-verbose IDs; this is independent of
  the AArch64 work and must not block focused backend checks.

## Design Rules

### Typed MInst, Not Assembly Strings

- Keep HIR selection, register allocation, frame resolution, pseudo
  expansion, and GNU formatting as distinct phases.
- Represent encodable AArch64 operand forms explicitly instead of emitting
  arbitrary assembly text from selector code.
- Give every instruction one authoritative operand declaration shared by RA
  and post-RA rewriting; do not rely on duplicated unnamed operand indexes.
- Keep GNU assembly aliases such as `cmp` and `cset` in the printer while
  preserving their underlying machine constraints in MInst.

### Immediate And Address Legality

- Model encoding-valid immediates with typed constructors returning
  `Option`, so an invalid immediate cannot reach final emission.
- Use a single constant-materialization planner from normal selection and
  late frame/pseudo expansion.
- Keep frame slots, incoming arguments, and outgoing arguments symbolic
  until final frame layout is known.
- Resolve symbolic memory locations to a direct AArch64 address mode when
  legal, otherwise materialize an address with reserved post-RA scratch
  registers.

### Conditions And CFG

- Preserve comparison flags until the consumer chooses `b.cond`,
  `cbz/cbnz`, `tbz/tbnz`, `cset`, or later `csel`.
- Never insert a flags-clobbering instruction between a flags producer and
  its consumer without materializing the condition first.
- Treat block-argument copies as edge-specific work; do not attach distinct
  true/false copies to one ambiguous program point.

### Late Expansion

- Only expand a pseudo after RA when its expansion uses exclusively reserved
  physical scratch registers and has a declared scratch/clobber contract.
- Expand any sequence needing allocatable temporaries before RA.
- Keep printer input fully legalized; the printer may format final MInst but
  must not make profitability or semantic lowering decisions.

## Priority-Ordered Milestones

## M0: VCode Contract And RA Safety

**Priority: P0.** Complete this before broadening the instruction set. Current
instruction fields, operand collection, post-RA rewriting, and formatting are
coupled by anonymous allocation positions. Adding complex MInst forms before
fixing that contract will create fragile, untestable spill failures.

### Goals

- Establish a structured, mechanically checkable mapping between machine
  instruction register fields and RA allocations.
- Make RA constraints sufficient for tied operands, fixed operands, stack
  operands, clobbers, and multiple spilled values.
- Make post-RA rewrite robust for every operand count needed by AArch64 MInst.

### Tasks

- [x] Add a target-neutral register-mapping or operand-location interface so
  post-RA rewriting does not hand-index `allocs[0]`, `allocs[1]`, and so on in
  every AArch64 instruction arm.
- [x] Retain operand identity or a target-provided rewrite callback in addition
  to flattened allocation order.
- [x] Ensure alias resolution updates the actual instruction fields for both
  uses and defs rather than only temporary `Writable<Reg>` wrappers.
- [x] Add test coverage for VReg aliases through use operands, def operands,
  block parameters, and branch arguments.
- [x] Implement and test `OperandConstraint::Limit`; allocator candidate
  selection must honor the encoded physical-register subset.
- [x] Add and test tied use/def (`Reuse`) constraints, including a real AArch64
  `movk`-style input/output reuse fixture.
- [x] Define explicit behavior for nonallocatable fixed architectural registers
  such as SP, ZR, FP, LR, and reserved scratch registers.
- [x] Support multiple spilled defs and emit every required post-instruction
  store in deterministic order.
- [x] Support instructions with three or more spilled use operands, or legalize
  them before post-RA rewrite into forms requiring no more simultaneously live
  scratch registers than the target reserves.
- [x] Unify allocator and emitter scratch-register contracts by register class.
- [x] Remove or replace fabricated untracked spill-slot fallback behavior when
  no move scratch register is available.
- [x] Add a vector-class failure path or implementation; no vector operand may
  be silently dropped during branch-copy handling.
- [x] Make `MachInst` classification useful to consumers: terminator, call,
  memory, move, flags, and side-effect metadata must not be dead interfaces.

### Required Regressions

- [ ] Three spilled integer inputs to `MSub` assemble and execute correctly.
- [x] Three spilled f32 inputs to a future three-source float form are either
  correctly legalized or rejected before invalid assembly is emitted.
- [x] Multiple spilled defs are stored to distinct slots in correct order.
- [ ] Tied input/output allocation preserves values under register pressure.
- [ ] Fixed ABI operands, physical SP/ZR operands, and call clobbers coexist.
- [x] Spill-to-spill copies work with large offsets and no untracked stack slot.

### Exit Gate

- [x] `taki_mir` unit tests cover aliases, Limit, reuse, clobbers, edge edits,
  and allocation output mapping.
- [x] `anon_armv8` tests cover post-RA instructions with all supported spilled
  operand shapes and no scratch-register overflow.
- [x] `cargo test -p taki_mir`, `cargo test -p anon_armv8`, and `git diff
  --check` pass.

## M1: Encoding-Shaped AArch64 MInst

**Priority: P0.** Replace the current register-only instruction surface with a
small, typed instruction architecture modeled after the reusable part of
Cranelift `inst.isle`.

### Core Representation

- [ ] Introduce `OperandSize::{Size32, Size64}` independent of source HIR type.
- [ ] Introduce an explicit `Gpr` abstraction that distinguishes allocatable
  `xN/wN`, `sp`, and `xzr/wzr` even though SP and ZR share encoding number 31.
- [ ] Keep FP scalar views (`sN`) distinct from their allocation unit (`vN`).
- [ ] Add `ALUOp` for operations sharing two-register encodings.
- [ ] Add `ALUOp3` for `madd`/`msub`-style three-source encodings.
- [ ] Add encoding-shaped ALU variants: `AluRRR`, `AluRRRR`, `AluRRImm12`,
  `AluRRImmLogic`, `AluRRImmShift`, `AluRRRShift`, and `AluRRRExtend`.
- [ ] Add structured move forms: normal move, move-wide seed, move-wide keep,
  physical-register move, and zero-register move where required.
- [ ] Keep entry ABI constraints, call ABI constraints, and return ABI
  constraints as explicit no-text or pseudo MInst operations.
- [ ] Add an instruction verification method that rejects invalid combinations
  before formatting assembly.

### Typed Operands

- [ ] Add `Imm12` for add/sub immediate values, including the optional 12-bit
  shift form.
- [ ] Add `ImmLogic` for AArch64 logical-bitmask immediate values.
- [ ] Add `ImmShift` and `ShiftOp` for immediate/register shift forms.
- [ ] Add `ExtendOp` for `uxtb`, `uxth`, `uxtw`, `uxtx`, `sxtb`, `sxth`, `sxtw`,
  and `sxtx` forms.
- [ ] Add `MoveWideConst` for validated 16-bit chunks at legal shifts.
- [ ] Add signed/scaled memory offset immediate types in M2.
- [ ] Add property-style boundary tests for every immediate constructor.

### Constant Planning

- [ ] Implement one integer constant planner shared by selection and late
  expansion.
- [ ] Prefer consumer folding over standalone materialization.
- [ ] Select zero-register use when legal.
- [ ] Select one-instruction `movz`, `movn`, or logical-immediate `orr` when
  possible.
- [ ] Otherwise choose the minimal `movz`/`movn` seed plus `movk` patches.
- [ ] Add i32 and pointer/i64 constant paths without depending on host width.
- [ ] Defer literal pools until f32 constants and symbol placement require them.

### Exit Gate

- [ ] Every new MInst variant has operand, fixed-register, reuse, clobber,
  validation, rewrite, and formatting tests.
- [ ] Clang accepts hand-constructed MInst sequences for every ALU shape.
- [ ] Constant planner chooses legal minimal sequences for boundary and random
  i32/i64 values.

## M2: Scalar And Condition Selection

**Priority: P1.** Use the M1 instruction forms for immediate folding and flag
preservation before adding broad memory coverage. These optimizations are local,
high-frequency, and reduce register pressure for later memory lowering.

### Integer And Pointer Arithmetic

- [ ] Select `add/sub` immediate forms, including negated-immediate conversion
  between add and sub.
- [ ] Select `and/orr/eor` logical immediate forms.
- [ ] Select immediate shifts and register shifts with legal shift descriptors.
- [ ] Fold power-of-two multiply only when signed overflow and use-count
  semantics remain correct.
- [ ] Fold extension producers into `AluRRRExtend` where source type semantics
  prove the selected extension is correct.
- [ ] Select shifted-register arithmetic for add/sub and logical forms.
- [ ] Select `madd` for `mul + add` and `msub` for `sub - mul` only when the
  multiply result has no independent use.
- [ ] Preserve existing signed remainder selection as `sdiv` followed by `msub`.

### Condition Narrow Waist

- [ ] Introduce a selector-level condition representation for zero/nonzero,
  flags condition, and eventually one-bit tests.
- [ ] Lower integer comparison directly to flags when its only consumer is a
  branch, avoiding `cset` materialization.
- [ ] Select `cbz/cbnz` for direct integer truthiness.
- [ ] Retain `cset` only when HIR needs an i32 boolean value.
- [ ] Add condition inversion support for branch layout when no edge copy is
  required.
- [ ] Add `tbz/tbnz` only for proven exact one-bit tests; do not rewrite signed
  arithmetic patterns into bit tests without a semantic proof.
- [ ] Keep NZCV producer/consumer dependencies explicit until formatting.

### Floating Scalar Completion

- [ ] Add f32 constants with an assembler-valid constant pool or equivalent
  relocatable load sequence.
- [ ] Add `fsub`, `fmul`, and `fdiv` MInst and selector support.
- [ ] Add ordered f32 comparison and truthiness rules matching LLVM behavior,
  including NaN cases.
- [ ] Add `scvtf` and `fcvtzs` for i32/f32 casts.
- [ ] Audit floating remainder reachability; implement an explicit `fmodf`
  runtime strategy or reject it with a descriptive error.

### Exit Gate

- [ ] HIR -> VCode -> RA -> assembly fixtures cover immediate, shifted,
  extended, compare-branch, f32 arithmetic, casts, and NaN comparison cases.
- [ ] Clang accepts every fixture and QEMU checks semantic output.
- [ ] LLVM differential tests verify selected behavior for integer overflow,
  signed shifts/division, extensions, and float NaN semantics.

## M3: Memory, Frame Objects, And Address Modes

**Priority: P1.** Add memory as typed MInst rather than recreating the direct
backend's address temporary and stack round-trip behavior.

### Address Model

- [ ] Add `AMode` with base-only, unsigned scaled offset, signed unscaled
  offset, register offset, scaled register offset, and extended register offset
  forms.
- [ ] Add `PairAMode` separately for pair load/store encodings and their signed
  scaled offset range.
- [ ] Add symbolic `FrameSlot`, `IncomingArg`, `OutgoingArg`, and symbol address
  forms that remain unresolved until their relevant phase.
- [ ] Add `SImm9`, `UImm12Scaled`, and `SImm7Scaled` validated offset types.
- [ ] Make memory access width/type explicit for i32, pointer/i64, and f32.
- [ ] Add memory-effect metadata sufficient for normal versus volatile access;
  reject unsupported atomic semantics explicitly.

### Local And Spill Integration

- [ ] Lower HIR `Alloc` into fixed frame-object requests before RA.
- [ ] Compute local object size/alignment using AArch64 layout, never host
  pointer size.
- [ ] Resolve frame objects, spill slots, callee saves, and outgoing arguments
  together after RA.
- [ ] Lower i32/pointer/f32 loads and stores through typed load/store MInst.
- [ ] Use direct scaled or unscaled addressing when legal.
- [ ] Materialize large offsets with reserved scratch registers only after frame
  finalization.
- [ ] Keep spill memory accesses distinct from program memory accesses in tests
  and debug output.

### GEP And Globals

- [ ] Audit HIR GEP semantics against `raana_ir/src/llvm/writer.rs` before
  implementation.
- [ ] Fold small constant GEP offsets into address modes.
- [ ] Select `[base, wIndex, sxtw/uxtw #scale]` for valid i32 indices.
- [ ] Select `[base, xIndex, lsl #scale]` when the index is already widened.
- [ ] Select multiply-add address formation for non-power-of-two strides.
- [ ] Lower nested-array strides and dynamic multidimensional indices.
- [ ] Lower global addresses using GNU AArch64 PC-relative symbol syntax.
- [ ] Emit global data, zero-initialized storage, scalar constants, and nested
  aggregate layout through the VCode program path.

### Aggregate Initialization

- [ ] Keep aggregate values in memory rather than allocating aggregate registers.
- [ ] Lower local and global aggregate initialization recursively.
- [ ] Implement correct all-zero initialization before optimizing it into loops
  or runtime calls.
- [ ] Preserve f32 aggregate element width and layout.

### Exit Gate

- [ ] VCode QEMU fixtures pass for local scalar variables, arrays, nested
  arrays, globals, dynamic indexing, and large frame offsets.
- [ ] Load/store address selection tests cover scaled, unscaled, extended-index,
  register-index, pair, and scratch-address fallback forms.
- [ ] Mixed local-array, spill, and call tests preserve 16-byte stack alignment.

## M4: Complete AAPCS64 ABI And Calls

**Priority: P1.** Complete ABI behavior once memory/frame locations are
available. Do not make stack argument support a special direct-emission path.

### Entry And Return ABI

- [ ] Support pointer entry arguments, returns, and fixed `x0` call results.
- [ ] Support stack-passed i32, pointer, and f32 entry parameters.
- [ ] Preserve independent integer/pointer (`x0..x7`) and f32 (`s0..s7`)
  register windows.
- [ ] Load incoming stack arguments through symbolic frame-relative MInst.
- [ ] Support i32, pointer, f32, and void returns with fixed ABI locations.

### Outgoing Calls

- [ ] Compute the largest outgoing stack-argument area before frame finalization.
- [ ] Store overflow call arguments into the outgoing area without moving `sp`
  per call.
- [ ] Support mixed integer, pointer, and f32 direct-call signatures.
- [ ] Preserve caller-save clobbers and live values across all call result types.
- [ ] Support declarations as direct call targets without emitted bodies.
- [ ] Keep indirect calls unsupported until HIR requires them; return a
  descriptive code-generation error rather than panic.

### Callee Saves And Frames

- [ ] Save/restore used integer callee saves in deterministic frame order.
- [ ] Add `v8..v15` allocation only with the AAPCS64 low-64-bit save/restore
  implementation and QEMU coverage.
- [ ] Pair safe adjacent callee-save operations only after their frame offsets
  and unwind-free semantics are proven.
- [ ] Verify calls maintain 16-byte `sp` alignment with every combination of
  locals, spills, outgoing area, and callee saves.

### Exit Gate

- [ ] QEMU ABI fixtures cover 0, 1, 8, and 9 i32 arguments.
- [ ] QEMU ABI fixtures cover 0, 1, 8, and 9 f32 arguments.
- [ ] QEMU ABI fixtures cover mixed i32/pointer/f32 signatures, nested calls,
  recursion, void calls, and stack alignment.
- [ ] Values live across calls survive caller-save clobbers under forced integer,
  float, and pointer pressure.

## M5: CFG, Edge Copies, And Block Layout Safety

**Priority: P1.** Complete edge-specific semantics before applying general
branch inversion, `csel`, or aggressive block placement.

### Edge Copy Correctness

- [ ] Represent allocator edits at a `(predecessor, successor)` edge or use
  explicit edge blocks with emitted terminators.
- [ ] Emit distinct copies for true and false conditional branch edges.
- [ ] Validate conditional block parameters with distinct values on each edge.
- [ ] Validate loop-carried block parameters.
- [ ] Validate critical-edge splitting.
- [ ] Validate parallel-copy cycles, including two-value swaps and mixed
  register/spill locations.
- [ ] Preserve allocator edit order exactly for every edge-copy form.
- [ ] Add f32 and pointer block-parameter transfers after scalar support exists.

### Safe Layout Improvements

- [ ] Elide an unconditional jump only when the target is physically next and
  no edge copy or edge-specific code is required.
- [ ] Invert a conditional branch only after preserving exact edge-copy behavior.
- [ ] Place a chosen successor as fallthrough when CFG semantics permit it.
- [ ] Add loop-aware or hot-successor placement only after static correctness
  tests and measurable benchmark evidence.

### Deferred Conditional Data Processing

- [ ] Add `csel`, `csneg`, and related conditional data-processing forms.
- [ ] Fold a side-effect-free diamond into conditional select only when both
  arms have no required edge copies and value liveness remains valid.
- [ ] Benchmark `csel` against predictable and unpredictable branches; do not
  assume branchless code is universally faster.

### Exit Gate

- [ ] QEMU fixtures cover diamonds, loops, `break`, `continue`, critical edges,
  edge-specific block parameters, and parallel-copy cycles.
- [ ] Branch layout changes preserve assembly labels, copies, and runtime output.

## M6: Performance Selection And Measurement

**Priority: P2.** Optimize only after representative programs can use VCode
correctly. The largest expected win is eliminating direct-backend stack traffic,
not generic post-RA peepholes.

### Required Measurement Infrastructure

- [ ] Separate compiler, IR-dump, link, QEMU startup, and application runtime
  measurements; current harness wall time combines them.
- [ ] Parse SysY `TOTAL` timer output into structured benchmark results.
- [ ] Run performance measurements sequentially and add warm-up runs.
- [ ] Add paired A/B benchmark execution with randomized order.
- [ ] Collect at least 20-30 samples per configuration on native AArch64 before
  claiming a performance improvement.
- [ ] Report median, geometric mean, code size, static instruction mix, and
  bootstrap 95% confidence intervals.
- [ ] On native hardware, collect `cycles`, `instructions`, `branches`,
  `branch-misses`, and relevant cache counters when available.
- [ ] Keep QEMU as a semantic and gross-instruction-count tool, not a source of
  microarchitecture-specific performance claims.

### Optimization Order

- [ ] Measure baseline VCode code size, spills, reloads, address-generation
  instructions, and branch forms on representative workloads.
- [ ] Prioritize address-mode folding and retention of addresses in registers.
- [ ] Prioritize add/sub/cmp immediates, logical immediates, immediate shifts,
  and zero-register use.
- [ ] Prioritize direct compare-to-branch and safe fallthrough elimination.
- [ ] Prioritize shifted/extended ALU operands and `madd`/`msub` fusion.
- [ ] Add limited pair load/store selection first for callee saves and explicit
  semantically paired operations; defer generic adjacent-memory pairing.
- [ ] Add `tbz/tbnz` only after bit-test patterns are fully proven.
- [ ] Add `csel` only after CFG edge-copy correctness and branch benchmarks.
- [ ] Do not apply an optimization that increases spill pressure or regresses
  measured performance without a documented tradeoff.

### Benchmark Groups

- [ ] Dense arrays: `matmul*`, `01_mm*`, `conv2d-*`, `many_mat_cal-*`.
- [ ] Memory permutations: `transpose*`, `shuffle*`.
- [ ] Bit manipulation: `crc*`, `crypto-*`, `fft*`.
- [ ] Branch-heavy code: `03_sort*`, `huffman-*`, `knapsack_naive-*`.
- [ ] Scalar scheduling: `optimization_scheduling*`, `sl*`.
- [ ] Recursive/control-flow code: `h-1-01` and related `h-*` cases.

### Exit Gate

- [ ] Every claimed optimization has correctness fixtures, LLVM differential
  coverage, static selection checks, and benchmark evidence.
- [ ] Performance reports distinguish QEMU results from native AArch64 results.
- [ ] `perf/h-1-01.sy` timeout is either resolved or documented with a measured
  root cause and a scoped follow-up task.

## M7: Driver Integration And Release Gates

**Priority: P0 only after M0-M5 exit gates pass.** Driver replacement is the
last migration step, not a mechanism for testing incomplete VCode coverage.

### Driver And Diagnostics

- [ ] Add a temporary explicit VCode backend mode for end-to-end differential
  testing before changing the default native backend.
- [ ] Keep `-S`, `--emit asm`, `--emit llvm`, and multi-emit naming stable.
- [ ] Replace direct production lowering only when VCode compiles the complete
  supported SysY HIR surface and passes all release gates.
- [ ] Return concise code-generation diagnostics with function and instruction
  context for unsupported constructs; no driver-reachable panic.
- [ ] Preserve existing direct-backend debug logging until an equivalent VCode
  debug path reports HIR, MInst, constraints, RA allocations, edge edits, frame
  layout, and final assembly.

### Final Validation

- [ ] Run `cargo test -p taki_mir`.
- [ ] Run `cargo test -p anon_armv8`.
- [ ] Run `cargo test --workspace`, documenting any independent failures.
- [ ] Run `cargo build -p soyo_compiler`.
- [ ] Run focused QEMU VCode fixtures before each broader suite.
- [ ] Run `make test` with VCode-backed native assembly.
- [ ] Run `make test-llvm` unchanged.
- [ ] Run `git diff --check` before every milestone commit.

### Release Exit Gate

- [ ] Native `make test` passes with VCode-backed assembly.
- [ ] Native high-functional ABI coverage passes with VCode-backed assembly.
- [ ] LLVM reference suite remains passing.
- [ ] Generated assembly has no dependency on LLVM code generation.
- [ ] Production diagnostics and debug observability are available for VCode.

## Deferred Optimizations

- [ ] Type-sensitive i32/f32 spill slots after fixed eight-byte slots are fully
  validated.
- [ ] Broader load/store pair formation after profiling justifies aliasing and
  scheduling complexity.
- [ ] Constant-pool deduplication after f32 constant lowering is stable.
- [ ] More aggressive block placement after edge copies are correct and profile
  data or robust static heuristics are available.
- [ ] SIMD/vector MInst only after the source language requires vector values.

Every optimization must preserve ABI, memory, NaN, signed-division, and
block-parameter semantics, and must retain a differential test against the LLVM
path.
