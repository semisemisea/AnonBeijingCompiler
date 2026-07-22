# AArch64 Backend Replacement Plan

## Decision

Replace the existing AArch64 implementation wholesale. The repository will have
one AArch64 assembly backend built on the current `taki_mir` VCode pipeline.
There is no compatibility period, no direct-backend fallback, and no parallel
old/new AArch64 codegen path.

This plan uses `taki_mir/src/riscv64/` as the structural reference. It adopts
the generic backend contracts and module boundaries, but does not copy RISC-V
instruction selection, calling convention details, or known RISC-V limitations.

The target remains GNU assembly for `aarch64-linux-gnu`:

```text
SysY
  -> Raana HIR
  -> generic block ordering and virtual-register assignment
  -> AArch64 HIR selector
  -> typed AArch64 MInst / VCode
  -> taki_mir register allocation
  -> allocation write-back and allocator edits
  -> AAPCS64 frame finalization
  -> AArch64 late pseudo expansion
  -> GNU AArch64 assembly
  -> assembler/linker/QEMU
```

The assembler and linker own ELF generation, relocations, symbol resolution,
branch-range handling, and veneers. The compiler owns selection, register
allocation integration, ABI behavior, frame layout, and pseudo expansion.

## Milestones

### Phase 1 Complete: Legacy Path Removed

Completed on 2026-07-22.

- Deleted the direct backend, old VCode driver, post-RA emitter, old instruction
  model, and private ABI/frame/register implementation.
- Deleted old path-specific binaries, VCode-only fixtures, assembly shape files,
  and Makefile VCode targets.
- Removed `--asm-backend`, its CLI dispatch, and the Python harness marker guard.
- Removed legacy-only `anon_armv8` dependencies.
- Left AArch64 assembly generation unavailable until the generic replacement is
  introduced; there is no fallback backend.

### Phase 2 Complete: Registers And Labels

Completed on 2026-07-22.

- Added `anon_armv8::regs` with AArch64 physical register units, AAPCS64
  argument/return registers, allocator and post-RA scratch policy, callee-save
  classification, call clobbers, physical names, and `MachineEnv`.
- Kept SP/ZR as explicit `Gpr` variants rather than allocator registers, and
  reserved FP/LR plus all scratch registers from allocation.
- Added `anon_armv8::labels::Label` to emit MIR block, HIR function, and HIR
  global references through the shared `EmitContext`.

### Phase 3 Foundation Complete: Typed Operands And MInst Model

Completed on 2026-07-22.

- Added `anon_armv8::instructions` as the sole typed AArch64 instruction model.
- Added validated immediate and offset types, typed memory address modes,
  condition/operation descriptors, and all planned MInst family data forms.
- Added static MInst verification for width, extension shift, bit-test, and pair
  memory-width invariants.
- Deferred `MachInst` operand visiting and GNU emission until `AArch64Abi` is
  introduced, because the public generic contract requires the ABI associated
  type rather than a compatibility placeholder.

### Phase 4 Foundation Complete: Shared Constant Planning

Completed on 2026-07-22.

- Added a shared i32/i64 AArch64 constant planner that chooses zero, one
  `movz`/`movn`, logical-immediate `orr`, or a minimal move-wide seed and
  `movk` patches without depending on host width.
- Added a target-register materialization helper that emits only typed MInst and
  never allocates a vreg, stack slot, or undeclared scratch register.
- Deferred post-RA frame/address legalization until the ABI supplies finalized
  frame offsets and the generic emitter can consume the resulting MInst stream.

## Scope And Constraints

### In Scope

- AArch64 GNU assembly generation through `taki_mir::compile::<AArch64Backend>`.
- The complete currently accepted SysY HIR surface: i32, f32, pointers, arrays,
  globals, calls, recursion, control flow, local stack objects, and aggregates.
- AAPCS64 scalar argument, return, caller-save, callee-save, and stack-frame
  behavior needed by the source language.
- Typed AArch64 MInst, address modes, immediate validation, constant planning,
  late legalization, and target assembly emission.
- Generic VCode block ordering, register allocation, allocation write-back, and
  generic function emission.
- Final semantic, ABI, QEMU, LLVM differential, diagnostic, and performance
  validation after implementation is complete.

### Out Of Scope

- Direct ELF or object-file generation.
- JIT code generation, stack maps, trap metadata, unwind metadata, runtime
  patching, or compiler-managed branch veneers/islands.
- SIMD, SVE, LSE atomics, pointer authentication, TLS, tail calls, exceptions,
  or indirect calls unless the accepted HIR requires them.
- Compatibility wrappers, fallback dispatch, old CLI flags, or preservation of
  the old direct and experimental VCode APIs.
- Intermediate test gates while this replacement is being implemented.

### Implementation Rule

Do not run focused or workspace tests as intermediate milestones. Build the
replacement coherently, add its full test suite, then execute the validation
matrix only after the implementation phases below are complete. Static source
inspection and careful API conformance are the implementation-time controls.

Known defects or incomplete behavior in `taki_mir` and `riscv64` are outside
this plan unless they directly prevent use of the current public generic
contract. Do not add AArch64-only compatibility APIs to work around upstream
drift.

## Final Architecture

### Target Contract

```rust
AArch64Backend: LowerBackend<MInst = MInst>
MInst: MachInst<ABISpec = AArch64Abi> + MachInstEmit
AArch64Abi: ABIMachineSpec<I = MInst>
```

The compiler entry point for AArch64 is exactly:

```rust
taki_mir::compile::<anon_armv8::AArch64Backend>(program)
```

There is no target-local lowering driver that invokes register allocation,
builds a frame, or walks allocator output itself.

### Module Layout

`anon_armv8/src/` will contain only these production backend modules:

```text
lib.rs
abi.rs
instructions.rs
labels.rs
lower.rs
regs.rs
```

| Module | Structural reference | Responsibility |
|---|---|---|
| `lib.rs` | `taki_mir/src/riscv64.rs` | Module exports and `AArch64Backend` public surface. |
| `abi.rs` | `riscv64/abi.rs` | AAPCS64 argument locations, machine environment access, stack/frame hooks, spill and callee-save expansion. |
| `instructions.rs` | `riscv64/instructions.rs` | Typed MInst, immediates, address modes, operand visiting, verification, and GNU emission. |
| `labels.rs` | `riscv64/labels.rs` | MIR block, HIR function, and HIR global symbolic labels. |
| `lower.rs` | `riscv64/lower.rs` | `LowerBackend` implementation and HIR instruction selection. |
| `regs.rs` | `riscv64/regs.rs` | Physical register definitions, views, ABI register lists, clobbers, scratch policy, and names. |

Target-specific behavior belongs only in these modules. Generic block ordering,
VCode construction, register allocation, allocation write-back, edit traversal,
and common function emission remain in `taki_mir`.

### Frame Shape

The target uses `taki_mir::abi::FrameLayout`; it does not retain a private
`anon_armv8::FrameLayout`.

```text
low addresses after allocating the frame
  outgoing stack-argument area
  local frame objects
  register-allocation spill area
  saved callee-save registers
  saved FP/LR setup area
high addresses / caller frame
```

`sp` is 16-byte aligned at every call. `x29` is the frame pointer and `x30` is
the link register. All symbolic stack locations remain unresolved until frame
finalization.

## Phase 1: Remove Replaced Code

Delete the old AArch64 implementation before adding the replacement. This
prevents new code from inheriting stale APIs or accidentally retaining a
fallback path.

### Delete

- `anon_armv8/src/lower.rs`: direct HIR-to-string backend.
- `anon_armv8/src/vcode_lower.rs`: old selector and private VCode driver.
- `anon_armv8/src/emit.rs`: custom post-RA allocator/output/frame emitter.
- `anon_armv8/src/inst.rs`: old operation-shaped instruction model.
- `anon_armv8/src/bin/vcode_m1_manual.rs`: old path-specific test binary.
- `anon_armv8/src/bin/vcode_msub_spills.rs`: old post-RA emitter test binary.
- `tests/vcode_m1_manual_start.s` and `tests/vcode_msub_spills_start.s`.
- Old VCode-only SysY fixtures and assembly substring files once replacement
  fixtures are designed.

### Remove Integration Paths

- Remove `compile_program_to_asm`, `compile_program_vcode`, and
  `compile_function_vcode` exports.
- Remove `AsmProgram`, `AsmFunction`, `AsmBlock`, `PostRaBlock`, and all
  `emit_post_ra_*` public APIs.
- Remove `AsmBackend` and `--asm-backend` from `soyo_compiler/src/cli.rs`.
- Replace target/backend dispatch in `soyo_compiler/src/main.rs` with one
  AArch64 call to `taki_mir::compile::<AArch64Backend>()`.
- Remove `test-vcode`, `test-vcode-m0-msub`, `test-vcode-m1-manual`, and
  `test-vcode-diagnostic` Makefile targets.
- Remove the `.ident "soyo-vcode"` fallback guard from the Python harness.
- Remove unused target-local dependencies from `anon_armv8/Cargo.toml`.

### Result

After this phase, the AArch64 target has no assembly generator. The next phases
create its sole implementation against the upstream generic API.

## Phase 2: Registers And Labels

Implement `regs.rs` and `labels.rs` first. No instruction or lowering code may
create architecture registers, call clobbers, or formatted symbols ad hoc.

### Register Model

Define physical allocation units:

- Integer: x0 through x30.
- Floating: v0 through v31, emitted as scalar `sN` for f32.
- `x29`: FP, never allocatable.
- `x30`: LR, never allocatable.
- SP and ZR: explicit special operands, never represented as ordinary
  allocatable `Reg` values.

Define explicit views:

```rust
enum OperandSize {
    Size32,
    Size64,
}

enum Gpr {
    Reg(Reg),
    Sp,
    Zr,
}
```

The MInst emitter selects `wN` or `xN` from `OperandSize`. Scalar floating
instructions select `sN` while the register allocator still sees `RegClass::Float`.

### AAPCS64 Register Policy

Centralize these lists in `regs.rs`:

- Integer arguments: x0..x7.
- Float arguments: v0..v7.
- Integer return: x0/w0.
- Float return: v0/s0.
- Preferred integer caller-save registers: x0..x13.
- Non-preferred integer callee-save registers: x19..x28.
- Preferred float registers: v0..v7 and v16..v29.
- Deferred float callee-save registers: v8..v15.

Reserve:

- Integer allocator scratch: x16.
- Float allocator scratch: v31.
- Integer post-RA scratch: x14, x15, x16, x17.
- Float post-RA scratch: v30, v31.

Do not expose vector allocation. Any vector HIR/MInst path must fail explicitly.

Define one `DEFAULT_CLOBBERS` and one `is_callee_saved` policy. Calls and ABI
code consume these declarations rather than rebuilding `PRegSet` values locally.

### Labels

Implement:

```rust
enum Label {
    Block(MirBlockIndex),
    Function(HirFunction),
    GlobalValue(HirInst),
}
```

Labels emit through `EmitContext`. Selector code and MInst never store a
preformatted block label or assembly symbol string.

## Phase 3: Typed AArch64 Instruction System

Implement `instructions.rs` as the only machine instruction representation.
The instruction system is encoding-shaped: each variant describes a legal A64
operand form, not a generic operation later rendered into arbitrary text.

### Validated Operand Types

Implement constructors that reject invalid encodings:

- `Imm12`: add/sub immediate with optional `lsl #12`.
- `ImmLogic`: AArch64 logical bitmask immediate for 32- and 64-bit forms.
- `ImmShift`: width-valid shift amount.
- `ShiftOp`: LSL, LSR, ASR, ROR.
- `ExtendOp`: UXTB, UXTH, UXTW, UXTX, SXTB, SXTH, SXTW, SXTX.
- `MoveWideConst`: one 16-bit move-wide chunk and legal shift.
- `SImm9`: signed unscaled memory offset.
- `UImm12Scaled`: unsigned scaled memory offset.
- `SImm7Scaled`: signed scaled pair-memory offset.
- `MemoryType`: i32, i64/pointer, f32 width and access mode.

### Address Modes

Implement symbolic and encodable address forms:

```rust
enum AMode {
    Reg { base: Gpr },
    UnsignedOffset { base: Gpr, offset: UImm12Scaled },
    SignedOffset { base: Gpr, offset: SImm9 },
    RegOffset { base: Gpr, index: Reg },
    ScaledRegOffset { base: Gpr, index: Reg, shift: u8 },
    ExtendedRegOffset {
        base: Gpr,
        index: Reg,
        extend: ExtendOp,
        shift: u8,
    },
    FrameSlot(i64),
    IncomingArg(i64),
    OutgoingArg(i64),
}

enum PairAMode {
    SignedOffset { base: Gpr, offset: SImm7Scaled },
    PreIndex { base: Gpr, offset: SImm7Scaled },
    PostIndex { base: Gpr, offset: SImm7Scaled },
}
```

Symbol addresses are materialized by `LoadAddr`; they are not disguised as a
generic load/store memory operand.

### MInst Families

Implement these instruction families.

Integer and pointer instructions:

- `AluRRR`
- `AluRRRR`
- `AluRRImm12`
- `AluRRImmLogic`
- `AluRRImmShift`
- `AluRRRShift`
- `AluRRRExtend`
- `SDiv`
- `MAdd`
- `MSub`
- `CmpRR`
- `CmpImm`

Moves and constant materialization:

- `Mov`
- `MovPhys`
- `MovZ`
- `MovN`
- `MovK`
- `MovFromZero`
- `LoadAddr`

Control and conditions:

- `BCond`
- `Cbz`
- `Cbnz`
- `Tbz`
- `Tbnz`
- `CondBr` as the single allocator-visible two-edge terminator
- `Jump`
- `CSet`

Floating scalar instructions:

- `FMov`
- `FAdd`
- `FSub`
- `FMul`
- `FDiv`
- `FCmp`
- `Scvtf`
- `Fcvtzs`

Memory and frame instructions:

- `Load`
- `Store`
- `LoadPair`
- `StorePair`
- target ABI spill and reload operations

ABI pseudos:

- `Args`
- `Call`
- `RetVal`
- `Ret`
- `Nop`

### Operand And Metadata Contract

Implement the current `MachInst` methods for every MInst variant:

- `get_operands`
- `is_move`
- `is_term`
- `call_type`
- `is_mem_access`
- `rc_for_type`
- `gen_jump`

Requirements:

- Every virtual use and definition is visited once and in deterministic order.
- Fixed ABI arguments/results use fixed use/def visitor calls.
- Calls publish `DEFAULT_CLOBBERS`.
- `MovK` uses a reuse constraint for its tied source/destination.
- Address base/index registers are visited.
- SP/ZR and physical fixed operands do not become allocator operands.
- `Ret` is the only return terminator.
- Each block exposes only one allocator-visible branch terminator.
- `CondBr` owns both conditional successor labels and expands only after
  allocation; separate `BCond` plus `Jump` is not used for HIR two-edge blocks.

### Verification And Emission

Implement an internal `verify()` for MInst. It rejects invalid width, immediate,
address mode, SP/ZR position, tied operand, flags dependency, and pseudo-phase
combinations before assembly emission.

Implement `MachInstEmit` directly on MInst. It may format final GNU syntax, but
it may not select instructions, decide constant strategy, allocate registers, or
invent frame/spill locations.

## Phase 4: Shared Constant Planning And Late Legalization

Implement a single integer constant planner used by selector lowering and
AArch64 ABI/frame expansion:

```rust
plan_integer_constant(value: u64, size: OperandSize) -> SmallVec<[MInst; 4]>
```

Selection order:

1. Fold the constant into an encoded consumer immediate.
2. Use ZR when the instruction permits zero.
3. Use one `movz` when possible.
4. Use one `movn` when possible.
5. Use one logical-immediate `orr` when possible.
6. Use the minimal `movz`/`movn` seed followed by required `movk` patches.

The planner handles i32 and pointer/i64 independently of host width. It covers
negative values, 16-bit boundaries, 32-bit truncation, and all legal 64-bit
move-wide shifts.

Late legalization resolves only work whose temporary registers are all declared
in `MachineEnv.post_ra_scratch_by_class`:

- large stack adjustment
- large frame offset
- large spill offset
- stack-to-stack allocator edit
- symbolic frame location not representable by a direct A64 address mode
- large symbol-relative address sequence

Late legalization may not allocate a vreg, create an untracked stack slot, or
borrow an allocatable physical register.

## Phase 5: AAPCS64 ABI And Frame Integration

Rewrite `abi.rs` around the current `ABIMachineSpec` trait. Do not retain the
old local `Signature`, `ValueLocation`, or `FrameLayout` abstractions.

### Required ABIMachineSpec Methods

Implement:

- `stack_align`
- `spillslot_size`
- `is_callee_saved`
- `gen_load_stack`
- `gen_store_stack`
- `gen_spill_load`
- `gen_spill_store`
- `gen_incoming_arg_load`
- `gen_load_imm`
- `gen_load_addr`
- `gen_args`
- `gen_ret`
- `gen_move`
- `compute_arg_loc`
- `get_machine_env`
- `gen_prologue_frame_setup`
- `gen_epilogue_frame_restore`
- `gen_clobber_save`
- `gen_clobber_restore`

### Argument And Return Rules

- i32, pointer, and string scalar arguments use x0..x7.
- f32 arguments use v0..v7 independently of integer arguments.
- Overflow i32, pointer, and f32 arguments occupy eight-byte stack slots.
- The maximum outgoing overflow area is reserved once per frame; calls do not
  adjust SP dynamically.
- i32 returns through w0.
- pointer/string/i64 returns through x0.
- f32 returns through s0.
- Unit returns have no result pair.
- Direct declarations are valid call labels even when the program emits no body.
- Indirect calls remain unsupported and produce a codegen error.

### Frame And Save Rules

- `sp` remains 16-byte aligned at all ABI boundaries.
- The generic frame layout owns outgoing arguments, locals, spills, callee saves,
  setup area, and total size.
- Spill slots are indexes. Their byte offset is always `slot * spillslot_size`.
- Save the actually allocated integer callee-save subset x19..x28 in deterministic
  order.
- v8..v15 remain outside allocation until low-64-bit AAPCS64 save/restore is
  implemented.
- Pair save/restore instructions are used only for legal adjacent locations.
- Prologue and epilogue use typed MInst and constant legalization; they do not
  write assembly strings directly.

## Phase 6: AArch64 HIR Selection

Implement `AArch64Backend` in `lower.rs` using only the current generic
`LowerContext` API:

- `ctx.arena`
- `ctx.reg_map`
- `ctx.put_value_in_reg`
- `ctx.alloc_tmp`
- `ctx.emit`
- `ctx.vcode`

Do not restore old convenience APIs such as `lower_function`, `value_reg`,
`result_reg`, `emit_inst`, or a target-local allocation driver.

### LowerBackend Hooks

Implement:

- `lower`
- `lower_branch`
- data/text/global/word/zero directives
- `preg_name`
- block-label formatting
- `emit_long_jump`

### Integer And Pointer Selection

Lower:

- i32 arithmetic: add, sub, mul, signed div, signed remainder.
- i32 bit operations: and, or, xor.
- i32 shifts: logical left, logical right, arithmetic right.
- pointer/i64 add/sub and address arithmetic.
- integer and pointer comparisons.
- i32/pointer constants through the constant planner.

Select, when legal:

- add/sub immediate including negated-immediate conversion.
- logical immediates.
- immediate and register shifts.
- shifted-register arithmetic.
- extended-register arithmetic.
- zero-register forms.
- `mul + add` to `madd` only if the multiply has no independent use.
- `sub - mul` to `msub` only if the multiply has no independent use.
- signed remainder as `sdiv` followed by `msub`.

### Conditions

Introduce a selector-level condition representation:

```rust
enum LoweredCond {
    Zero(Reg),
    NonZero(Reg),
    Flags(Cond),
    Bit { reg: Reg, bit: u8, set: bool },
}
```

Rules:

- Direct compare-to-branch keeps NZCV live and uses `CondBr`.
- Direct integer truthiness selects `cbz` or `cbnz`.
- `cset` is emitted only when HIR needs an i32 boolean value.
- `tbz/tbnz` is selected only for a proven one-bit test.
- No flags-clobbering MInst is inserted between a flags producer and consumer.
- Branch inversion is deferred until edge copies and physical layout are known.

### Floating Scalar Selection

Lower:

- f32 constants via an assembler-valid constant pool or relocatable load scheme.
- fadd, fsub, fmul, fdiv.
- ordered f32 comparisons and truthiness with correct NaN behavior.
- i32-to-f32 `scvtf`.
- f32-to-i32 `fcvtzs`.

Floating remainder is either lowered through an explicit runtime `fmodf` strategy
or rejected with a function/instruction-specific diagnostic; it is never silently
miscompiled.

### Terminators

`lower_branch` handles only Return, Jump, and Branch HIR instructions:

- Return emits optional `RetVal` then `Ret`.
- Jump emits one target transfer.
- Branch emits one `CondBr` carrying both labels.
- Branch operands are materialized before the terminator so generic block
  parameter/edge-copy lowering can consume them.

## Phase 7: Memory, Frame Objects, GEP, Globals, And Aggregates

### Local Frame Objects

Lower HIR `Alloc` to fixed frame-object requests before register allocation.

Target layout:

- i32: size 4, alignment 4.
- f32: size 4, alignment 4.
- pointer/string: size 8, alignment 8.
- arrays: recursive target size and alignment.
- aggregate values remain in memory and never receive a scalar virtual register.

### Typed Loads And Stores

Support i32, pointer/i64, and f32 loads/stores from:

- local frame objects
- GEP results
- globals
- incoming stack arguments
- outgoing stack arguments
- spill slots

Address-selection order:

1. Direct unsigned scaled offset.
2. Direct signed unscaled offset.
3. Register offset.
4. Scaled or extended register offset.
5. Reserved scratch address materialization after frame finalization.

Program-memory metadata remains distinct from spill-memory metadata. Unsupported
volatile or atomic HIR behavior is rejected explicitly.

### GEP

Audit GEP semantics against `raana_ir/src/llvm/writer.rs`, then lower:

- constant byte-offset folding.
- i32 dynamic indices through `sxtw`/`uxtw` as justified by HIR semantics.
- widened indices through `lsl`.
- power-of-two element sizes through scaled indexing.
- other strides through multiply-add address formation.
- nested arrays and multidimensional dynamic indices.

### Globals And Aggregate Initialization

Use the generic program-level global emission path with target directives for:

- integer scalar initialization
- f32 bit-pattern initialization
- zero-initialized storage
- nested aggregate layout
- global symbol declarations

Use `adrp` plus `:lo12:` for global addresses.

Recursively lower local and global aggregate initialization. Zero initialization
is semantically complete before any memset, loop-fill, or pair-store optimization.

## Phase 8: CFG, Edge Copies, And Block Layout

The target follows generic `BlockLoweringOrder` and explicit edge-block behavior.
It does not synthesize its own parallel-copy protocol in the emitter.

### Edge Semantics

- Every predecessor/successor edge has its own copy location.
- True and false branch edges retain distinct block arguments.
- Critical edges use explicit edge blocks.
- Loop-carried block parameters are preserved.
- Parallel-copy cycles retain allocator order and use declared scratch registers.
- Register-to-register, register-to-spill, spill-to-register, and spill-to-spill
  copies are emitted via ABI move/spill hooks.
- i32, pointer, and f32 block parameters use the appropriate register class and
  memory width.

### Layout Rules

- Remove an unconditional jump only when its target is physically next and the
  edge has no required code.
- Invert a conditional branch only when target labels and edge-copy semantics are
  exactly preserved.
- Use successor fallthrough only when it does not erase edge-specific work.
- Add `csel`, `csneg`, and related conditional data operations only for
  side-effect-free diamonds without required edge copies.

## Phase 9: Diagnostics, Observability, And Documentation

There is no fallback backend. Every unsupported construct must return a concise
code-generation error containing:

- function name
- basic block, where available
- HIR instruction
- source/target type
- lowering phase
- unsupported construct or legality reason

Add debug observability for:

- HIR function
- block lowering order
- selected MInst
- MInst verification failure
- operand constraints
- register allocations
- allocator edits
- frame layout
- late legalized MInst
- final assembly

Update:

- `anon_armv8/README.md`
- root `README.md`
- `AGENTS.md`
- CLI help text
- Makefile usage documentation

Document the single AArch64 backend, current feature status, AAPCS64 policy,
debug interfaces, and final validation commands. Do not document removed backend
selection or fallback behavior.

## Phase 10: Performance Work

Implement performance work only after all semantic implementation phases are
complete. The main expected improvement is avoiding the removed direct backend's
per-value stack traffic.

### Measurement Infrastructure

Implement measurement that separates:

- compiler execution
- IR dumping
- assembly/linking
- QEMU startup
- application runtime

Parse SysY `TOTAL` timer output into structured data. Run sequential warm-up and
measurement samples, randomize A/B ordering, and use 20-30 native AArch64 samples
per configuration. Report median, geometric mean, code size, instruction mix, and
bootstrap 95% confidence intervals. On native hardware collect cycles,
instructions, branches, branch misses, and relevant cache counters where possible.

QEMU is a semantic and gross instruction-count tool, not evidence for
microarchitectural performance claims.

### Optimization Order

1. Address-mode folding and address retention.
2. Add/sub/cmp immediates.
3. Logical immediates, immediate shifts, and zero-register use.
4. Direct compare-to-branch and safe fallthrough elimination.
5. Shifted/extended ALU operands and madd/msub fusion.
6. Safe callee-save pair load/store selection.
7. Proven `tbz/tbnz` patterns.
8. `csel` after edge-copy correctness and branch measurements.

Do not add an optimization that increases spill pressure or regresses measured
performance without a documented tradeoff.

### Benchmark Groups

- Dense arrays: `matmul*`, `01_mm*`, `conv2d-*`, `many_mat_cal-*`.
- Memory permutations: `transpose*`, `shuffle*`.
- Bit manipulation: `crc*`, `crypto-*`, `fft*`.
- Branch-heavy code: `03_sort*`, `huffman-*`, `knapsack_naive-*`.
- Scalar scheduling: `optimization_scheduling*`, `sl*`.
- Recursive/control-flow: `h-1-01` and related `h-*` cases.

## Phase 11: Complete Test Suite And Final Validation

Only after Phases 1-10 are implemented, add and run the replacement test suite.
There are no intermediate test gates.

### MInst And Immediate Tests

Cover every MInst variant for:

- operand visiting
- fixed ABI operands
- tied/reuse constraints
- clobbers
- terminator/call/memory classification
- verification failures
- allocation write-back
- GNU formatting

Cover every immediate/address constructor at boundaries and with randomized
property-style inputs. Confirm constant planning chooses legal minimal sequences
for i32 and i64 values.

### ABI And Register Allocation Tests

Cover:

- 0, 1, 8, and 9 integer arguments.
- 0, 1, 8, and 9 f32 arguments.
- mixed integer, pointer, and f32 signatures.
- integer, pointer, f32, and void return values.
- nested calls and recursion.
- values live across calls under integer, pointer, and float pressure.
- no-frame, leaf-frame, call-frame, large-frame, and callee-save behavior.
- true spill-slot indexes from allocator output, including slots 0, 1, and 2.
- large spill offsets and stack-to-stack moves.
- 16-byte stack alignment with locals, spills, calls, and saves combined.

### Selector, Memory, And CFG Tests

Cover:

- immediate, shifted, extended, and fused arithmetic selection.
- signed division/remainder and signed shift behavior.
- f32 arithmetic, casts, ordered comparisons, and NaN behavior.
- local scalar variables, arrays, nested arrays, globals, dynamic indexing, and
  aggregate initialization.
- pointer arithmetic, pointer calls, and pointer return values.
- diamonds, loops, break, continue, critical edges, and loop-carried parameters.
- true/false edges carrying different block arguments.
- register/spill parallel-copy cycles.
- f32 and pointer block-parameter transfers.
- unsupported feature diagnostics, including indirect calls and float remainder
  when no runtime strategy is selected.

### End-To-End Validation Commands

Run only after the implementation and tests above are present:

```bash
cargo fmt --check
cargo test -p taki_mir
cargo test -p anon_armv8
cargo test --workspace
cargo build -p soyo_compiler
make test
make test ARGS="-O 1"
make test-llvm
git diff --check
```

Then run focused Clang assembly acceptance, AArch64 static linking, QEMU
functional/high-functional suites, LLVM differential fixtures, diagnostics
fixtures, and debug-output checks. Finally execute the Phase 10 benchmark plan.

## Completion Criteria

The replacement is complete only when:

- `anon_armv8` has only the new RISC-V-shaped module architecture.
- The compiler dispatches AArch64 exclusively through
  `taki_mir::compile::<AArch64Backend>`.
- No direct backend, old VCode driver, custom post-RA emitter, private frame
  layout, `--asm-backend` option, or fallback code remains.
- Every currently accepted SysY HIR construct lowers through typed AArch64 VCode.
- AAPCS64 integer, pointer, f32, stack argument, caller-save, callee-save, and
  frame behavior is implemented.
- Globals, arrays, aggregates, dynamic GEP, recursion, and edge-specific CFG
  transfers are implemented.
- All final unit, workspace, Clang, QEMU, LLVM, diagnostic, and formatting
  validation commands pass.
- Performance claims, if any, are supported by the documented native AArch64
  measurement procedure.

## Deferred Work

The following remain explicitly deferred after the replacement:

- Per-type spill-slot sizes after the fixed eight-byte slot policy is proven.
- General load/store pair formation beyond callee saves and explicitly paired
  semantic operations.
- Constant-pool deduplication after f32 constant lowering stabilizes.
- Aggressive block placement beyond correctness-preserving local fallthrough.
- SIMD/vector MInst and vector allocation.
- Indirect calls, tail calls, exceptions, TLS, atomics, and AArch64 extensions
  not required by accepted HIR.
