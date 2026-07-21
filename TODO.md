# ARMv8/AArch64 Assembly Backend Plan

## Scope

Implement a native ARMv8-A AArch64 backend that emits GNU AArch64 assembly
for `aarch64-linux-gnu`. The backend is selected by the existing `-S` and
`--emit asm` paths and produces assembly that can be assembled and linked by
the existing test toolchain:

```text
soyo_compiler -S input.sy -o output.s
clang --target=aarch64-linux-gnu ... output.s sysylib/libsysy_arm.a -o output.elf
qemu-aarch64-static output.elf
```

The initial target is the complete SysY source language currently accepted by
the frontend: i32, f32, pointers, arrays, globals, functions, recursion,
control flow, and calls to the SysY runtime.

### Explicit Non-goals

- Direct ELF object (`.o`) writing.
- JIT compilation or in-memory executable code.
- Raw AArch64 instruction-byte encoding.
- Linker relocation generation in the compiler.
- Exception handling, stack maps, GC metadata, traps, or unwind metadata.
- Constant islands, branch veneers, jump tables, SIMD/vector lowering, or
  AArch64 extensions such as LSE, SVE, Pointer Authentication, and BTI.
- Support for targets other than Linux AArch64 GNU assembly syntax.

Assembly generation still needs stable labels, proper ELF assembler directives,
and standard symbol-address sequences. The assembler and linker are
responsible for relocations and branch-distance handling.

## Current Baseline

### Existing Components

- High-level Raana IR is typed SSA with basic-block parameters:
  `raana_ir/src/ir/`.
- HIR instructions cover constants, binary operations, control flow, casts,
  stack/global allocation, loads/stores, GEP, calls, and aggregates:
  `raana_ir/src/ir/inst_kind.rs`.
- `taki_mir` is intended to be the target-neutral virtual-register machine IR,
  inspired by Cranelift VCode:
  `taki_mir/src/lower.rs`, `taki_mir/src/vcode.rs`.
- `taki_mir` already includes block ordering, edge splitting, virtual-register
  allocation, operand constraints, register allocation, and an ABI outline.
- `anon_armv8` exists as an empty target crate and is the implementation home
  for the AArch64 backend.
- The driver exposes `-S` and `--emit asm`, but `dump_asm()` remains unimplemented:
  `soyo_compiler/src/main.rs`.
- The integration harness can assemble, statically link, and execute AArch64
  programs through QEMU: `tests/test.py` and `Makefile`.
- `--emit llvm` plus `llc --mtriple=aarch64-linux-gnu` provides an independent
  semantic reference during implementation.

### Framework Gaps To Resolve

- `LowerContext::lower()` is private and target lowering cannot access the
  HIR value/register/emission operations it requires:
  `taki_mir/src/lower.rs`.
- `CalleeABI::new()` is unfinished:
  `taki_mir/src/abi.rs`.
- `MachInstEmit` is only a marker trait:
  `taki_mir/src/vcode.rs`.
- No top-level HIR -> VCode -> register allocation -> assembly pipeline exists.
- Register allocation results are not yet applied to target instructions.
- VCode's physical-register pinning convention requires verification before any
  target implementation: the first 192 virtual-register IDs represent physical
  registers, but `VRegAllocator` must reserve those IDs consistently.
- Aggregate machine values are unsupported. Arrays must remain addresses or
  memory objects during lowering; only scalar i32, f32, and pointer values need
  virtual registers.

## Design Principles Taken From Cranelift

The implementation follows the useful API boundaries of Cranelift without
copying its source, generated files, WebAssembly conventions, JIT machinery, or
its full binary-emission infrastructure.

1. Keep target-neutral lowering and register allocation in `taki_mir`; keep
   AArch64 ISA, ABI, lowering, frame layout, and text emission in `anon_armv8`.
2. Preserve the pipeline `Raana IR -> VCode with virtual registers -> register
   allocation result -> post-allocation assembly emission`.
3. Do not mutate VCode to contain physical allocations. Treat register
   allocation output as a separate mapping and apply it only while emitting.
4. Define every target instruction's use/def, fixed-register, reuse, and call
   clobber constraints in one place through `MachInst::get_operands()`.
5. Separate abstract ABI argument placement from concrete `ldr`, `str`, `mov`,
   stack-address, prologue, and epilogue instruction generation.
6. Model `sp`, `xzr/wzr`, ordinary GPRs, FP/SIMD registers, and scratch
   registers distinctly even where their AArch64 encodings overlap.
7. Reserve post-register-allocation scratch registers up front. This is simpler
   and safer than allocating a scratch register after final frame size is known.
8. Start with direct assembly text and labels. Internal block labels and
   external symbols must be distinct, but no compiler-side relocation buffer is
   needed while Clang/GNU assembler owns object generation.
9. Make unsupported HIR forms return a descriptive code-generation error. Do
   not leave user-reachable `todo!()`, `unimplemented!()`, or silent omissions.
10. Test from instruction constraints upward through ABI/runtime tests and the
    existing QEMU end-to-end suite.

## Target Interface And Module Layout

### New/Updated `taki_mir` Interface

Add a small target-neutral public facade. Exact names can vary, but the
ownership and responsibilities should remain equivalent.

```rust
pub trait TargetIsa {
    type Inst: VCodeInst;

    fn compile_function(
        &self,
        program: &HirProgram,
        func: HirFunction,
    ) -> Result<CompiledFunction<Self::Inst>, CodegenError>;
}

pub fn compile_program(
    program: &HirProgram,
    isa: &impl TargetIsa,
) -> Result<String, CodegenError>;
```

`CompiledFunction` should retain at least:

- The pre-allocation `VCodeContainer` for diagnostics and test dumps.
- The `reg_alloc::Output` allocation and move edits.
- Final frame-layout data.
- Target-specific assembly-emission state or final text.

`CodegenError` must include the function name and enough contextual information
to identify the unsupported HIR instruction or invariant failure.

Expose only the lowering context operations a backend needs:

- Read current function and current HIR instruction data.
- Resolve a HIR value to its allocated virtual register while recording a use.
- Retrieve the result virtual register of the current HIR instruction.
- Allocate temporary virtual registers.
- Emit one machine instruction into the current HIR instruction buffer.
- Query the current lowered block and branch targets.
- Query HIR type, constant, side-effect, and use-count information.
- Invoke public single-function lowering.

Keep the `LowerContext` storage private. The target backend should not mutate
VCode ranges, use counters, block order, or HIR arena state directly.

### `anon_armv8` Layout

Create the following modules under `anon_armv8/src/`:

```text
lib.rs       Public AArch64 backend facade and program assembly entry point.
isa.rs       Immutable Linux AArch64 target configuration and TargetIsa impl.
regs.rs      Physical register identities, register printing, and MachineEnv.
inst.rs      Machine instruction enum, constructors, and operand constraints.
abi.rs       AAPCS64 signature placement and call lowering helpers.
frame.rs     Local/spill/outgoing/callee-save frame layout and prologue/epilogue.
lower.rs     Raana IR to AArch64 VCode instruction selection.
emit.rs      Post-RA instruction rewriting and GNU assembly text generation.
```

No generated instruction-selection system is needed initially. A direct Rust
`match` over Raana `InstKind` is clearer for the current, small IR. Reconsider a
declarative rule system only after the direct lowering has stable coverage and
has accumulated enough repetitive pattern selection to justify it.

## Milestone 0: Make The Common Pipeline Usable

**Status: Completed (2026-07-21).** The common lowering entry is public, a
target-neutral `lower_function()` facade builds block order and ABI state, and
VCode can invoke register allocation through a stable read-only bridge. The
physical-register pinned vreg range is now reserved in the allocator, and unit
coverage verifies ordinary vregs cannot alias it. `ABIMachineSpec` now returns
argument locations and stack size explicitly; target ABI implementations remain
the responsibility of the next milestones.

### 0.1 Complete The Public Lowering Entry

- Make the function-lowering entry in `LowerContext` public.
- Add a narrow backend-facing API for HIR inspection, register lookup, temporary
  vreg allocation, and instruction emission.
- Expose immutable VCode accessors for instructions, vreg types, blocks,
  operands, clobbers, ABI state, and block-argument metadata.
- Add a stable constructor for `reg_alloc::VCodeRef` from `VCodeContainer`.
- Add stable queries on `reg_alloc::Output` for instruction operand allocation,
  before/after edits, spill count, and allocation kind.
- Introduce `CodegenError` and replace user-reachable backend `todo!()` paths.

### 0.2 Validate Virtual/Physical Register Identity

- Audit `PINNED_PREG` handling in `taki_mir/src/register.rs`.
- Ensure virtual register allocation starts after the physical-register-pinned
  range, or change the representation so physical and virtual ranges cannot
  collide.
- Add assertions that an ordinary virtual register is never decoded as a
  physical register.
- Add tests for integer and floating-point vreg allocation, aliases, physical
  register conversion, and spill-slot conversion.

### 0.3 Define A Real Emission Contract

Replace the empty `MachInstEmit` marker with an assembly-oriented contract.
The first version may use target-specific post-allocation wrappers rather than
placing all emitter methods directly in the generic trait, but it must support:

- Printing a fully physical instruction.
- Rewriting a machine instruction according to per-operand allocations.
- Emitting a move requested by register allocation.
- Emitting reload/store sequences when an allocated operand is a spill slot.

The generic layer must not know AArch64 spelling, instruction widths, or stack
addressing forms.

### Acceptance Criteria

- `cargo test -p taki_mir` includes unit tests for a simple CFG and allocator
  output.
- A dummy target can lower a HIR function to VCode, invoke RA, and inspect the
  result without private-field access.
- The full workspace compiles without a user-reachable backend `todo!()` in the
  new code path.

## Milestone 1: AArch64 Register Model And Assembly Skeleton

**Status: Completed (2026-07-21).** `anon_armv8` now defines Linux AArch64
physical-register identities, an initial allocator environment with ABI/scratch
register reservations, a machine-instruction contract for moves, arithmetic,
control flow, calls, and returns, plus a GNU assembly program emitter with
stable function/block labels. Unit tests cover register views, allocator
reservations, exact emitted skeleton text, and invoke
`clang --target=aarch64-linux-gnu` to assemble generated output.

### 1.1 Register Definitions

Define physical registers and assembler names:

- GPRs: `x0..x30`, with `w0..w30` used for 32-bit operations.
- Scalar floating-point values: `s0..s31`, backed by SIMD/FP `v0..v31`.
- Frame pointer: `x29`.
- Link register: `x30`.
- Stack pointer: `sp`.
- Zero register: `xzr/wzr`.
- Reserved post-RA scratch GPRs: `x16`, `x17`.
- Reserved post-RA scratch FP register: choose `v31`/`s31` and exclude it from
  the allocatable set.
- Never allocate `x18`, `x29`, `x30`, `sp`, `xzr`, `x16`, or `x17`.

Use the existing `RegClass::Int` for i32, i64, and addresses, and
`RegClass::Float` for f32. Vector registers are out of scope.

### 1.2 Initial Register Allocation Environment

Initial allocatable sets:

- Integer caller-save preferred: `x0..x15`.
- Integer callee-save non-preferred: `x19..x28`.
- Float caller-save preferred: `v0..v7`, `v16..v30` excluding scratch.
- Do not allocate `v8..v15` in the first ABI implementation. This avoids
  needing the AAPCS64 low-64-bit preservation rule until the frame layer is
  proven correct.

Calls clobber all ABI caller-save registers. This must be represented as VCode
instruction clobbers so live values are saved or relocated before the call.

### 1.3 Initial Machine Instruction Set

Implement an AArch64 `Inst` enum sufficient for all later milestones:

- `Mov`, `FMov`, and zero-register moves.
- `MovZ`, `MovN`, `MovK` for integer/pointer constants.
- `Add`, `Sub`, `Mul`, `SDiv`, `MSub`.
- `And`, `Orr`, `Eor`, `Lsl`, `Lsr`, `Asr`.
- `Cmp`, `FCmp`, `CSet`.
- `FAdd`, `FSub`, `FMul`, `FDiv`.
- `SCVTF`, `FCVTZS`.
- `Ldr`, `Str` for i32, i64/address, and f32.
- `Stp`, `Ldp`, `AddSp`, `SubSp` for frames.
- `AdrP`, `AddLo12` for external/global addresses.
- `B`, conditional `B`, `CBZ`/`CBNZ`, `BL`, and `Ret`.
- Pseudo-instructions where useful before final emission, such as
  `LoadImm32`, `LoadImm64`, `LoadF32Const`, `LoadSymbolAddr`, `Copy`, and
  `Call`. Pseudos must be fully expanded before final text output.

For every instruction define:

- Register uses and definitions.
- Fixed register constraints.
- Input/output reuse constraints where required.
- Call clobbers.
- Whether the instruction is a move, terminator, call, or memory access.
- Its textual GNU assembler syntax after allocation.

### 1.4 Assembly Program Skeleton

Implement section and function output:

```asm
    .text
    .p2align 2
    .globl function_name
    .type function_name, %function
function_name:
    // prologue
.Lfunction_name_bb0:
    // body
    .size function_name, .-function_name
```

Use stable, collision-free local labels based on function index/name and MIR
block index. Declaration-only functions produce no body. Function and global
names from the source must be emitted as external symbols without compiler-side
renaming unless escaping becomes necessary.

### Acceptance Criteria

- Unit tests cover register spelling and special-register distinction.
- Unit tests verify representative instruction operand constraints.
- Hand-constructed AArch64 instruction sequences emit valid assembly.
- `clang --target=aarch64-linux-gnu -c` assembles emitted skeletons.

## Milestone 2: Linux AAPCS64 ABI And Stack Frames

**Status: Completed (2026-07-21).** The target assigns i32/pointer and f32
parameters through independent eight-register AAPCS64 windows, assigns overflow
scalars to eight-byte stack slots, and uses the correct scalar return registers.
Functions use a fixed 16-byte-aligned frame with an x29/x30 prologue and shared
epilogue; the precomputed outgoing area supports direct calls without moving
`sp`. QEMU passes the mixed register/stack ABI stress case
`tests/h_functional/39_fp_params.sy` and the large integer argument case
`tests/functional/88_many_params2.sy`.

### 2.1 ABI Scope

Support only the normal Linux AAPCS64 calling convention used by the test
runtime and cross-linker.

- i32 values use `w` registers, but their argument slots consume `x0..x7`.
- Pointer values use `x0..x7`.
- f32 values use `s0..s7`.
- Integer/pointer and float argument register counters are independent.
- i32 return: `w0`.
- pointer return: `x0`.
- f32 return: `s0`.
- Void functions do not define a return register.
- Stack arguments are assigned in source order after their register class is
  exhausted and are aligned as required by AAPCS64.
- At every call boundary, `sp` is 16-byte aligned.

Implement signature location assignment as a pure operation returning explicit
`ArgSlot` data. Refactor `ABIMachineSpec::compute_arg_loc()` to return rather
than mutate hidden context. `CalleeABI::new()` must consume this data and stop
using its current `todo!()` placeholders.

### 2.2 Frame Layout

All function stack needs must be resolved before emitting the prologue:

- Saved frame pointer/link register when the function makes calls or uses a
  conventional frame pointer.
- Used integer callee-save registers.
- Local `Alloc` objects.
- Register-allocator spill slots.
- Outgoing stack argument area sized for the largest call in the function.
- Alignment padding.

Use a fixed frame for the first implementation. `Alloc` must not adjust `sp` at
its HIR position; scan/lower it into a fixed local slot. This prevents stack
growth in loops and makes all offsets stable.

Recommended layout from high to low address:

```text
incoming caller stack arguments
saved x29/x30
saved callee registers
local Alloc objects
register allocator spill slots
outgoing stack argument area
current sp
```

Use `x29` as a stable base for incoming arguments and `sp` as the base for
local, spill, and outgoing areas. If an offset cannot be encoded by a direct
load/store form, construct the address with `x16` or `x17`.

### 2.3 Prologue/Epilogue

- Emit `stp x29, x30, [sp, #-16]!` and `mov x29, sp` for framed functions.
- Allocate/deallocate the remaining frame in amounts preserving 16-byte
  alignment.
- Save only callee-saved registers that occur in the final allocation output.
- Restore callee saves in reverse frame order.
- Move the return value to its ABI fixed register before restoring frame state.
- Emit one shared epilogue label for functions with multiple HIR returns, or
  emit independent correct epilogues. Prefer a shared epilogue once return
  value lowering is implemented.

### 2.4 Call Sequence

- Reserve or address the precomputed outgoing area; do not move `sp` per call
  in the first implementation.
- Place register arguments in fixed ABI registers through VCode constraints and
  generated copies.
- Store stack arguments to the outgoing area.
- Mark caller-save registers as clobbered on `bl`.
- Read the fixed ABI return register into the call-result vreg when used.
- Use `bl symbol` for direct HIR calls only. Indirect calls are not currently
  required by Raana IR.

### Acceptance Criteria

- Dedicated runtime tests for 0, 1, 8, and 9 integer arguments.
- Dedicated runtime tests for 0, 1, 8, and 9 f32 arguments.
- Mixed integer, pointer, and f32 argument tests that verify independent ABI
  register sequences.
- Tests for integer, pointer, f32, and void returns.
- Nested calls, recursion, values live across a call, and stack alignment.
- ABI tests assemble, static-link against `libsysy_arm.a`, and pass under QEMU.

## Milestone 3: Integer Core Lowering And Control Flow

**Status: Completed (2026-07-21).** Native `-S` lowering supports integer
constants, arithmetic, comparisons, branches, returns, scalar memory,
block-parameter transfers, direct calls, loops, and recursion using fixed SSA
value slots. QEMU passes the targeted arithmetic/control-flow set and the full
functional suite has no native backend runtime failures; remaining compile
errors are the pre-existing `raana_ir` `not implemented` panic.

### 3.1 Constants And Scalar Values

- Lower `Integer` and integer `ZeroInit` to integer constant materialization.
- Materialize i32 values with 32-bit-safe `movz`/`movk` patterns.
- Materialize pointers and addresses with 64-bit patterns.
- Do not assume every constant is present in HIR block layout; constants may be
  discovered through operand use chains.
- Use the existing lowering use-count machinery only after it is verified. The
  first correct version may materialize a constant in its assigned vreg instead
  of sinking/folding it.

### 3.2 Integer Operations

Implement signed SysY i32 semantics:

- `Add`, `Sub`, `Mul`.
- `Div` with `sdiv`.
- `Rem` with `sdiv` followed by `msub`.
- `And`, `Or`, `Xor`.
- `Shl`, logical `Shr`, arithmetic `Sar`.
- `Eq`, `NotEq`, `Gt`, `Lt`, `Ge`, `Le`.

Comparisons must define a full i32 boolean `0` or `1`, not a flags-only value,
because Raana IR represents comparison results as i32. Use `cmp` plus the
appropriate `cset` condition code.

### 3.3 Branches, Returns, And Basic Block Parameters

- Lower unconditional HIR `Jump` to `b label`.
- Lower HIR `Branch` as nonzero i32 truthiness. Prefer `cbnz` when possible;
  otherwise use `cmp wN, #0` plus `b.ne`.
- Lower HIR `Return` with an optional ABI return-register copy and branch to
  epilogue.
- Keep the existing VCode edge splitting for critical edges.
- Preserve block-parameter transfer through the allocator's branch-argument
  mechanism. Do not emit naïve sequential copies that corrupt cycles.
- Add tests for loop-carried values, diamond joins, critical edges, and swap
  cycles in block-argument transfers.

### 3.4 Initial Function Support

- Copy incoming integer/pointer arguments from their fixed ABI locations into
  VCode virtual registers at entry.
- Lower direct calls with integer/pointer arguments and results.
- Declarations are valid call targets but emit no definition.

### Acceptance Criteria

- Small standalone SysY programs covering arithmetic, comparisons, branches,
  loops, `break`, `continue`, returns, calls, and recursion pass under QEMU.
- Generated output is accepted by Clang's AArch64 assembler before runtime
  execution is attempted.
- No arithmetic or comparison lowering depends on host architecture width.

## Milestone 4: Memory, Arrays, And Global Data

**Status: Completed (2026-07-21).** The native backend uses explicit AArch64
target size/alignment rules for scalar and nested-array frame objects, fixed
local allocation, scalar and aggregate memory initialization, global data, and
dynamic multidimensional GEP. Scalar value slots and local addresses support
large offsets using `x16` scratch-address formation. QEMU validation passes for
array/global programs as well as the large-frame cases `74_kmp.sy`,
`83_long_array.sy`, and `88_many_params2.sy`. Global constants are resolved
through the correct arena when used by function operands, and large dynamic GEP
strides use full i32 materialization. `make test tests/functional` passes all
100 cases under QEMU.

### 4.1 Local Allocation, Load, And Store

- Pre-scan every defined function for `Alloc` instructions and create fixed
  frame slots from the pointee type's target size and alignment.
- Lower an `Alloc` value to the address of its frame slot.
- Lower i32 and pointer `Load`/`Store` with correct width (`ldr/str wN` for
  i32 and `ldr/str xN` for pointers).
- Use the target's explicit size/alignment calculation, never host pointer
  width. AArch64 pointers are always eight bytes in this backend.
- Support large frame offsets through scratch-address formation.

### 4.2 GEP

Implement `GetElemPtr` with target-layout stride calculation:

- Base pointer begins in a 64-bit GPR.
- Constant offsets are folded into direct add or addressing displacement where
  encodable.
- Dynamic offsets are sign/zero-extended according to IR index semantics, then
  multiplied by the pointee/array stride with shift, multiply, or `madd`.
- Multi-dimensional arrays process each index with the correct nested-array
  stride.
- The output is always a 64-bit address vreg.

Document the exact meaning of HIR GEP indices and validate it against the LLVM
writer implementation in `raana_ir/src/llvm/writer.rs` before coding. The
native backend and LLVM backend must agree on array-pointer and nested-index
semantics.

### 4.3 Aggregate Initialization

- Keep aggregate values in memory; never convert arrays to a MIR register type.
- Lower local aggregate stores recursively to scalar stores at calculated
  offsets.
- For all-zero aggregates, first implement a correct scalar-store expansion.
- Add a later optional zero-fill loop or `memset` call only after ABI calls and
  large offsets are stable.
- Preserve f32 aggregate support in the recursive structure even if float
  scalar lowering is completed in the next milestone.

### 4.4 Global Data

Emit defined HIR globals before `.text`:

- `.bss` with `.zero` for zero-initialized storage.
- `.data` with `.word` for i32 and f32 bit patterns.
- Recursive array layout with element alignment and complete zero fill where
  needed.
- `.p2align` according to target type alignment.
- `.globl` for externally visible globals when source semantics require it.
- Standard PC-relative symbol address construction:

```asm
    adrp xN, global_symbol
    add  xN, xN, :lo12:global_symbol
```

Leave relocation selection to the assembler/linker.

### Acceptance Criteria

- Local scalar variables, arrays, nested arrays, and dynamic indexing pass.
- Global scalar, zero-initialized array, and nonzero nested-array tests pass.
- Existing integer array I/O tests using `getarray` and `putarray` pass.
- Stress tests cover locals/arrays large enough to require nontrivial offsets.

## Milestone 5: Floating Point And Conversion Lowering

**Status: Completed (2026-07-21).** The native backend now lowers f32
constants, arithmetic, ordered comparisons, conversions, scalar memory access,
aggregate initialization, function arguments/results, and float truthiness.
Pointer values now preserve their 64-bit ABI representation across calls,
returns, block-parameter transfers, and aggregate initialization. Large fixed
frames use legal chunked stack adjustments and address formation. QEMU runtime
validation passes for `tests/functional/95_float.sy` and
`tests/h_functional/39_fp_params.sy`.

### 5.1 f32 Constants

Do not rely on the limited AArch64 FP-immediate encoding for arbitrary literals.
Emit arbitrary f32 constants into a deduplicated `.rodata` constant pool and
load them by symbol address:

```asm
    adrp x16, .LCf0
    ldr  sN, [x16, :lo12:.LCf0]
```

The constant pool key is the exact `f32::to_bits()` result, so `-0.0`, NaNs,
and values with identical textual but distinct bit representations remain
semantically controlled. Ensure the resulting assembly syntax is accepted by
the selected Clang/GNU assembler.

### 5.2 f32 Operations

Implement:

- `FAdd`, `FSub`, `FMul`, `FDiv`.
- Float `Eq`, `NotEq`, `Gt`, `Lt`, `Ge`, `Le` using `fcmp` and `cset`.
- `Cast i32 -> f32` using `scvtf`.
- `Cast f32 -> i32` using `fcvtzs`.
- f32 loads/stores with `ldr/str sN`.
- f32 ABI entry arguments, outgoing arguments, return values, and call results.

Comparison NaN behavior must match the existing LLVM lowering, which uses
ordered comparisons (`oeq`, `one`, `ogt`, `olt`, `oge`, `ole`). Select the
corresponding AArch64 condition codes and add explicit NaN tests.

### 5.3 Floating Remainder

AArch64 has no scalar f32 remainder instruction. Before implementation, audit
whether Raana IR can produce floating `Rem` for valid SysY input. If it can,
choose and implement one explicit semantic strategy:

- Emit an ABI-compliant call to `fmodf`, if the static target environment
  guarantees that symbol is available; or
- Add a compiler/runtime helper with a documented ABI and link it through the
  SysY runtime library.

Do not map floating remainder to integer remainder or approximate it silently.

### Acceptance Criteria

- `tests/functional/95_float.sy` passes.
- Float comparisons include equality, inequality, ordering, and NaN behavior.
- `tests/h_functional/39_fp_params.sy` passes, covering extensive float,
  mixed-type, and stack-passed parameter combinations.
- Matrix tests and float-array runtime calls pass.

## Milestone 6: Apply Register Allocation And Emit Final Assembly

**Status: In progress (2026-07-21).** The target-neutral post-RA contract now
exposes finalized VCode block/instruction operands and virtual-register types.
Allocator move edits retain the source value's machine type, so a target emitter
can select `w`/`x`/`s` register views and matching spill access widths without
guessing from register class. The active native path remains the verified direct
lowerer while instruction selection, physical instruction emission, and
RA-dependent frame finalization are implemented incrementally. The AArch64
emitter now has a tested post-RA move primitive for register and eight-byte spill
locations, including typed integer/pointer/f32 accesses, spill-to-spill copies,
and large-offset scratch-address formation. It is not yet wired into full VCode
instruction rewriting or the native compiler path. The same emitter can now
rewrite the current `Mov`/`Add`/`Cmp`/`CSet` VCode subset from operand
allocations, inserting typed reloads and spill stores around physical
instructions. A stream emitter now preserves the allocator's exact
`Before -> instruction -> After` edit order; integration still requires full
instruction selection coverage and prologue/epilogue integration. Frame layout
can now be finalized from RA output: allocator spill bytes are incorporated
directly and the used integer callee-save set is deduplicated and recorded for
save/restore emission. Finalized frames now emit tested prologue/epilogue
sequences that save and restore those integer callee-saves, including legal
large-frame stack adjustment; they are not yet used by the native compiler path.
The post-RA float scratch pair is `v30`/`v31`, both excluded from allocation, so
spill rewriting does not clobber AAPCS64 callee-saved `v8..v15` state.
Finalized VCode now exposes each block's global instruction range, matching the
index space used by allocator allocations and edit program points for upcoming
block-labelled function emission. The post-RA emitter now has a function-level
path for constructed VCode blocks: it emits directives, finalized frame state,
globally indexed edits/allocations, block labels, and a shared epilogue. A
deliberately opt-in HIR selector now drives the complete
`HIR -> VCode -> RA -> post-RA` path for zero-parameter i32 functions containing
integer constants, `Add`, `Sub`, `Mul`, signed `Div`, signed `Rem`, integer
bitwise operations, all three integer shifts, and all six signed comparisons,
zero-argument `Jump`/nonzero i32 `Branch`, and i32 return. Its focused tests
construct chained HIR arithmetic expressions, individually exercise every
comparison condition through real allocation, and validate both arithmetic and a
four-block control-flow fixture with Clang. Branches select `cmp wN, #0`, `b.ne`
to the true target, and an unconditional false-edge branch. Selecting HIR blocks
in reverse order ensures each consumer records its uses before the producer is
considered. This integration
exposed and fixed three generic allocator issues: the invalid VReg sentinel was
unconstructable, register-only operand demand was not decremented after a fresh
allocation, and reverse liveness retained values after their definitions. The
selector is not connected to the native driver: function parameters, calls,
memory, floats, and block parameters still require VCode instruction selection
and ABI coverage before M6 can be completed. Signed remainder selects `sdiv`
followed by `msub`; all remaining integer binary operations are validated through
the post-RA assembler path.
The selector now verifies a single-successor i32 block-parameter transfer through
the allocator's spill-oriented edge edits. Conditional block-parameter edges,
loop-carried values, and critical-edge copy cycles remain excluded because the
current conditional branch sequence needs per-edge edit placement.

### 6.1 Post-allocation Instruction Stream

Implement a target-specific emitter that consumes:

- Pre-allocation VCode.
- `reg_alloc::Output` operand allocation mapping.
- Register allocator `Edit::Move` entries at before/after instruction program
  points.
- Final frame layout.

Per instruction:

1. Emit all `Before` edits in allocator order.
2. Resolve each operand allocation to a physical register or spill slot.
3. Reload spilled use operands to reserved scratch registers where required.
4. Route spilled definitions through a reserved scratch register and store them
   afterward.
5. Emit the physical AArch64 instruction.
6. Emit any required spill stores and all `After` edits in allocator order.

Do not reorder allocator edits. In particular, block-argument transfers may
contain parallel-copy cycles whose allocator-produced sequence is semantically
significant.

### 6.2 Spill Policy

For the initial backend:

- Use fixed-size eight-byte spill slots for all scalar classes, aligned to eight
  bytes. This is conservative for f32 but simplifies the initial frame and
  allocator integration.
- Use `ldr/str wN` or `ldr/str sN` at the slot address for 32-bit payloads.
- Use `ldr/str xN` for pointers.
- Reserve scratch registers so spill-to-spill and reload/store handling cannot
  create an allocation cycle after RA.
- Reject an impossible instruction form with an explicit error rather than
  producing invalid assembly.

After correctness is established, type-sensitive four-byte f32/i32 spill slots
may be introduced as an optimization.

### 6.3 Frame Finalization Dependency

The final frame size depends on RA spill count and actual callee-save usage.
Therefore:

1. Lower first, including all `Alloc` frame-object requests and outgoing-call
   area requirements.
2. Run register allocation.
3. Compute final frame offsets from local objects, spill slots, outgoing area,
   and used callee saves.
4. Emit prologue/body/epilogue using that finalized layout.

The post-RA emitter must have access to final offsets; it must not embed
provisional stack offsets in pre-allocation instructions.

### Acceptance Criteria

- Artificial register-pressure tests force integer and floating spills.
- Values live across calls survive caller-save clobbers.
- Spill-to-spill moves and block-parameter cycles are correct.
- Functions with local arrays, calls, callee saves, and spills preserve 16-byte
  stack alignment and pass QEMU runtime tests.

## Milestone 7: Driver Integration And Diagnostics

**Status: In progress (2026-07-21).** The driver now reports native AArch64
code-generation failures as a concise `soyo_compiler:` diagnostic with a
nonzero exit status instead of panicking. Existing `-S` and multi-emit output
paths remain verified. Remaining M7 work is debug observability and final suite
cleanup after M6's VCode-to-post-RA integration. QEMU validation passes all
100 `tests/functional` cases and all 40 `tests/h_functional` cases; aggregate
zero initialization uses compact loops and `.zero` directives so large local
and global arrays remain assembleable. `RUST_LOG=anon_armv8=debug` reports
each function's frame, outgoing area, local size, and block-label map without
altering generated assembly. The independent LLVM path also passes all 100
functional cases. The direct stack-slot emitter is correct but unoptimized:
the serial performance run passed 35 of its first 36 cases, with
`perf/h-1-01.sy` exceeding the 120-second harness limit.

### 7.1 Compiler Driver

- Add `anon_armv8` as a `soyo_compiler` dependency.
- Replace `dump_asm()` in `soyo_compiler/src/main.rs` with a call to
  `anon_armv8::compile_program_to_asm()` or equivalent.
- Preserve all existing CLI behavior:
  - `-S` and `--emit asm` select native AArch64 assembly.
  - `--emit llvm` remains a working independent reference path.
  - Multi-emit output naming remains unchanged.
- Convert `CodegenError` into a concise diagnostic that includes function and
  instruction context. Avoid `unwrap()` for backend failures.

### 7.2 Debug Observability

Add opt-in debug logging or a backend debug option. It must not alter normal
assembly output. Useful dumps:

- HIR function and block label mapping.
- VCode before RA.
- Operand constraints and call clobbers.
- RA allocation mapping and inserted move edits.
- Final frame layout and stack-slot offsets.
- Final assembly per function.

Use `log` levels where possible so normal `-S` remains clean.

### Acceptance Criteria

- `soyo_compiler -S` creates a nonempty AArch64 `.s` file.
- `--emit ir,asm` creates both outputs at the documented paths.
- Unsupported code reports a codegen diagnostic rather than panicking.
- Existing `make test-llvm` continues to pass unchanged.

## Test Plan

### Unit Tests

Place focused unit tests in `taki_mir` and `anon_armv8` for:

- Physical register identity and GNU spelling.
- Type-to-register-class mapping.
- Machine instruction operand use/def/fixed/clobber constraints.
- Integer immediate materialization sequences.
- f32 constant-pool deduplication and bit-pattern output.
- Addressing-mode selection and large-offset fallback.
- AAPCS64 argument and return placement.
- Frame layout alignment, callee-save placement, and local/spill offsets.
- Block argument/parallel copy handling.
- Register allocation output to assembly rewrite behavior.

### Assembly Validation Tests

For each small constructed program:

1. Generate `.s`.
2. Run `clang --target=aarch64-linux-gnu -c generated.s -o generated.o`.
3. Fail on assembler diagnostics before running an executable.

This catches invalid syntax, illegal register-width combinations, malformed
address syntax, and missing labels independently from runtime correctness.

### ABI Runtime Tests

Add focused SysY or low-level fixture tests for:

- Register and stack integer arguments.
- Register and stack float arguments.
- Interleaved integer/pointer/float parameter lists.
- Calls with outgoing stack arguments.
- Integer, pointer, f32, and void returns.
- Recursive calls.
- Callee-save preservation.
- Stack alignment observed by a called helper.

### Differential Tests

Use the LLVM backend as an oracle during development:

1. Compile a SysY source with `--emit asm`; assemble/link/run under QEMU.
2. Compile the same source with `--emit llvm`; lower using `llc` and run under
   QEMU.
3. Compare normalized runtime output and exit status.

Differences are triaged as frontend/IR problems only after confirming that the
LLVM path behaves as expected.

### Existing End-to-end Suite

Use the existing commands as gates:

```bash
cargo test --workspace
cargo build -p soyo_compiler
make test TESTS="..."
make test
make test-llvm
```

Progression of `make test` coverage:

1. Small integer/control-flow subset.
2. Integer `functional` suite.
3. Array/global/call-heavy subset.
4. Floating functional suite, including `95_float.sy`.
5. High-functional ABI stress suite, including `39_fp_params.sy`.
6. Full functional and performance suites.

## Suggested Delivery Sequence

### M0: Backend Infrastructure

- Complete generic lowering and VCode/RA access APIs.
- Fix/verify pinned physical-vreg representation.
- Define a real post-RA assembly emission boundary.

Exit gate: dummy backend runs HIR -> VCode -> RA without private access or
unimplemented paths.

### M1: AArch64 Assembly Skeleton

- Implement `anon_armv8` registers, machine instruction representation,
  constraints, and valid text emission.

Exit gate: manually constructed function assembly compiles with AArch64 Clang.

### M2: ABI And Frames

- Implement Linux AAPCS64 signature placement, frame layout, prologue,
  epilogue, and direct-call argument/result mechanics.

Exit gate: dedicated ABI test binaries pass under QEMU.

### M3: Integer Language Core

- Implement integer scalar operations, comparisons, branches, returns, direct
  calls, and block parameters.

Exit gate: basic integer SysY programs, loops, recursion, and control-flow
fixtures pass under QEMU.

### M4: Memory And Globals

- Implement fixed local allocation, loads/stores, GEP, aggregate stores, and
  global data.

Exit gate: integer array/global functional tests pass.

### M5: Float Language Core

- Implement f32 constants, arithmetic, comparisons, casts, memory, calls, and
  ABI behavior.

Exit gate: `95_float.sy`, matrix tests, and `39_fp_params.sy` pass.

### M6: RA/Emission Hardening

- Complete spill rewriting, large offsets, callee-save tracking, edge-copy
  correctness, and frame finalization under pressure.

Exit gate: artificial pressure tests and mixed call/array/spill tests pass.

### M7: Full Integration And Cleanup

- Wire `dump_asm`, improve diagnostics/debug dumps, and run complete native
  suite.

Exit gate: `make test` passes; `make test-llvm` remains passing; generated
assembly has no dependency on an LLVM code-generation step.

## Deferred Optimizations

Implement only after all correctness gates pass:

- Fold integer immediates into `add/sub/cmp` immediate forms.
- Fold shifts into arithmetic operations where instruction forms permit it.
- Use `madd/msub` for fused multiply-add patterns.
- Use address-mode folding for small constant GEP offsets.
- Safely sink one-use pure constants and arithmetic based on explicit
  side-effect/use-count rules.
- Deduplicate global and float constants.
- Use type-sensitive spill slots instead of conservative eight-byte slots.
- Allocate/preserve `v8..v15` once their ABI save/restore path is tested.
- Improve function/block placement for fallthrough branches.

No optimization may obscure ABI, memory, NaN, signed-division, or block
parameter semantics. Every optimization must retain a differential test against
the LLVM reference path.

## Risks And Required Decisions

### Identified Risks

- `taki_mir` is incomplete infrastructure, not merely a missing target
  instruction set. M0 is mandatory before ISA work can integrate safely.
- Current block-parameter allocation uses spill-oriented moves at CFG joins.
  Loop and multi-predecessor cases need early, direct tests.
- Host-dependent pointer sizing in HIR cannot determine target layout. The
  backend must own all AArch64 object-size, alignment, and GEP-stride logic.
- Float comparison and remainder semantics can diverge subtly from LLVM/SysY;
  they need explicit tests rather than incidental suite coverage.
- Frame layout is coupled to allocator spill counts and callee-save use. Do not
  emit fixed stack offsets before allocation/frame finalization.
- AArch64 `sp` and zero register share encoding fields in some instructions.
  Their abstract register representations must prevent invalid substitutions.
- All generated code is expected to cross-link with the SysY runtime. The ABI
  between generated functions and `sysylib/libsysy_arm.a` is a primary
  correctness boundary.

### Decisions Already Fixed By Scope

- Output is GNU AArch64 assembly only.
- The first target is `aarch64-linux-gnu` only.
- Clang/GNU assembler and linker own ELF generation and relocations.
- `anon_armv8` is the only AArch64-specific crate.
- `taki_mir` remains the shared lowering/register-allocation framework.
- Correctness, ABI conformance, and test coverage take precedence over target
  instruction-selection optimization.
