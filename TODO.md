# AArch64 Backend TODO

## Current State

The production native backend remains the direct HIR lowering path in
`anon_armv8/src/lower.rs`. It is connected to `soyo_compiler -S` and has passed:

- Native QEMU functional suite: `100/100`.
- Native QEMU high-functional suite: `40/40`.
- LLVM reference functional suite: `100/100`.

The opt-in VCode path is `anon_armv8::compile_function_vcode(program, func)`.
It is intentionally not connected to the compiler driver until it can cover the
complete SysY HIR surface.

Verified VCode coverage:

- i32 constants, arithmetic, remainder, bitwise operations, shifts, and signed
  comparisons.
- i32 branches, returns, direct calls, void calls, and up to eight i32 register
  arguments in `w0..w7`.
- f32 entry arguments, `fadd`, f32 direct call arguments/results in `s0..s7` and
  `s0`, and f32 returns.
- i32 and f32 caller-save preservation and spill/reload under cross-call
  register pressure.
- One-successor i32 block-parameter transfer.
- Post-RA typed integer, pointer, and f32 moves/spills, callee-save frame
  handling, and large spill offsets.

Known non-blocking test issue:

- `cargo test -p raana_ir` fails because `raana_ir/src/fmt/writer.rs` enables
  verbose formatting while existing snapshots expect non-verbose IDs. This is
  independent of the AArch64 backend work.

## M6: VCode RA And Emission Completion

### VCode Scalar Lowering

- [ ] Add VCode pointer entry arguments, returns, and direct-call arguments/results.
- [ ] Add pointer constants and address materialization in VCode.
- [ ] Add VCode f32 constants using assembler-valid constant-pool loads.
- [ ] Add `fsub`, `fmul`, `fdiv`, ordered f32 comparisons, and f32 truthiness.
- [ ] Add VCode integer/f32 casts with `scvtf` and `fcvtzs`.
- [ ] Define and implement f32 remainder behavior, or explicitly reject it with
  a descriptive code-generation error after confirming HIR reachability.

### VCode Memory And Data

- [ ] Lower fixed local `Alloc` objects through VCode and finalized frame offsets.
- [ ] Lower typed i32, pointer, and f32 loads/stores through VCode.
- [ ] Lower GEP with target-layout strides, dynamic indices, and large offsets.
- [ ] Lower aggregate initialization through memory rather than scalar aggregate registers.
- [ ] Emit VCode global data and PC-relative global-address materialization.

### VCode ABI And Calls

- [ ] Support stack-passed i32, pointer, and f32 entry parameters.
- [ ] Reserve and write the outgoing stack-argument area for direct calls.
- [ ] Support mixed integer, pointer, and f32 call signatures with independent
  AAPCS64 register windows.
- [ ] Support pointer and f32 void-adjacent call combinations with correct
  fixed return-register constraints.
- [ ] Add QEMU ABI fixtures for 0, 1, 8, and 9 integer/f32 arguments, mixed
  signatures, recursion, and stack alignment.

### CFG And Parallel Copies

- [ ] Emit separate allocator edits for both conditional branch edges.
- [ ] Validate conditional block-parameter transfers with distinct edge values.
- [ ] Validate loop-carried block parameters.
- [ ] Validate critical-edge and parallel-copy swap cycles.
- [ ] Preserve allocator edit order for all edge-copy forms.

### Frame And Spill Hardening

- [ ] Track and save/restore allocated float callee-save registers `v8..v15`
  when float allocation expands to them.
- [ ] Validate mixed integer/f32/pointer spills with local arrays and calls.
- [ ] Validate spill-to-spill moves and large offsets in complete VCode functions.
- [ ] Validate 16-byte stack alignment with locals, outgoing arguments,
  callee saves, and spills combined.
- [ ] Add QEMU mixed call/array/spill pressure fixtures.

### M6 Exit Gate

- [ ] Run VCode HIR -> RA -> post-RA functions with integer, f32, pointer,
  memory, calls, and control flow under QEMU.
- [ ] Pass artificial integer and float pressure tests, including values live
  across calls.
- [ ] Pass block-parameter edge-copy and parallel-copy-cycle regressions.

## M7: Driver Integration And Cleanup

- [ ] Replace the direct HIR lowering production path only after the VCode path
  satisfies every M6 exit gate.
- [ ] Keep `-S`, `--emit asm`, `--emit llvm`, and multi-emit output naming stable.
- [ ] Report unsupported code generation as concise diagnostics with function
  and instruction context; do not panic.
- [ ] Add opt-in debug logging for HIR/VCode, constraints, RA allocations,
  move edits, frame layout, and emitted assembly.
- [ ] Run the full native and LLVM suites after VCode driver integration.
- [ ] Investigate and either optimize or document the `perf/h-1-01.sy` timeout.

### M7 Exit Gate

- [ ] `cargo test --workspace` passes, excluding only separately documented
  pre-existing failures.
- [ ] `cargo build -p soyo_compiler` passes.
- [ ] `make test` passes with VCode-backed native assembly.
- [ ] `make test-llvm` remains passing.
- [ ] Generated AArch64 assembly has no LLVM code-generation dependency.

## Validation Commands

- [ ] `cargo test -p anon_armv8`
- [ ] `cargo test -p taki_mir`
- [ ] `cargo test --workspace`
- [ ] `cargo build -p soyo_compiler`
- [ ] `make test`
- [ ] `make test-llvm`
- [ ] `git diff --check`

## Deferred Optimizations

- [ ] Fold integer immediates into `add`, `sub`, and `cmp` immediate forms.
- [ ] Fold shifts into arithmetic operations where instruction forms permit it.
- [ ] Select `madd`/`msub` for fused multiply-add patterns.
- [ ] Fold small constant GEP offsets into addressing modes.
- [ ] Deduplicate global and float constants.
- [ ] Use type-sensitive i32/f32 spill slots instead of conservative eight-byte slots.
- [ ] Improve function/block placement for fallthrough branches.

Every optimization must retain ABI, memory, NaN, signed-division, and
block-parameter semantics, with a differential test against the LLVM path.
