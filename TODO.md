# AArch64 Backend TODO

## Current State

- [x] Remove the legacy direct backend, old VCode driver, custom post-RA
  emitter, fallback CLI path, and legacy-only tests.
- [x] Add the AArch64 register model, labels, typed MInst forms, constant
  planner, AAPCS64 ABI hooks, GNU assembly emission, and generic VCode traits.
- [x] Add the first `AArch64Backend` selector slice for scalar integer/pointer
  arithmetic, comparisons, returns, jumps, and two-edge control flow.
- [x] Dispatch AArch64 assembly exclusively through
  `taki_mir::compile::<anon_armv8::AArch64Backend>(program)`.

## Architecture Constraints

- [ ] Keep the sole AArch64 pipeline as SysY -> Raana HIR -> generic VCode ->
  register allocation -> AAPCS64 frame finalization -> GNU AArch64 assembly.
- [ ] Keep target-specific production code limited to `anon_armv8/src/{abi,
  constants,instructions,labels,lower,regs}.rs` and module exports in `lib.rs`.
- [ ] Do not restore a direct backend, custom allocation driver, post-RA
  emitter, private frame layout, compatibility wrapper, fallback backend, or
  `--asm-backend` flag.
- [ ] Use only public `LowerContext` APIs in selector code: `arena`, `reg_map`,
  `put_value_in_reg`, `alloc_tmp`, `emit`, and `vcode`.
- [ ] Keep SP and ZR explicit special operands; do not expose them as
  allocatable registers.
- [ ] Do not run focused or workspace tests until all implementation phases and
  the replacement test suite are complete. Static inspection, `rustfmt`, and
  `git diff --check` are permitted during implementation.

## Selector

- [x] Audit and correct scalar integer/pointer lowering instruction order for
  reverse VCode construction, including remainder's `sdiv` + `msub` sequence,
  compare-plus-`cset`, and return-value setup.
- [ ] Lower i32 and pointer constants through the shared constant planner with
  correct 32-bit and 64-bit widths.
- [x] Select add/sub immediates, including negated-immediate conversion.
- [x] Select logical immediates and immediate shifts when legal.
- [ ] Select zero-register forms, shifted-register arithmetic, and
  extended-register address arithmetic when legal.
- [ ] Add safe `madd` and `msub` fusion only when the multiply result has no
  independent use.
- [ ] Introduce selector-level conditions for zero/nonzero, NZCV flags, and
  proven one-bit tests.
- [ ] Select direct compare-to-branch, `cbz`/`cbnz`, and proven `tbz`/`tbnz`
  patterns without inserting a flags-clobbering instruction between producer
  and consumer.
- [ ] Preserve generic edge-block ownership of block-parameter parallel copies
  and defer branch inversion/fallthrough elimination until layout is known.
- [ ] Lower f32 constants through an assembler-valid constant-pool or
  relocatable-load scheme.
- [x] Lower f32 add, sub, mul, and div.
- [ ] Lower ordered f32 comparisons and truthiness with correct NaN behavior.
- [x] Lower `scvtf` and `fcvtzs` casts.
- [ ] Reject f32 remainder with an instruction-specific diagnostic unless an
  explicit runtime `fmodf` strategy is implemented.
- [ ] Lower direct calls, mixed integer/float arguments, overflow stack
  arguments, and integer/pointer/f32 results.
- [ ] Reject unsupported indirect calls with a concise code-generation error.

## Memory And Data

- [ ] Lower `Alloc` into generic fixed frame-object requests with target size
  and alignment for i32, f32, pointers, strings, arrays, and nested arrays.
- [ ] Lower typed i32, pointer/i64, and f32 loads/stores for locals, GEPs,
  globals, incoming arguments, outgoing arguments, and spill slots.
- [ ] Select direct unsigned-scaled and signed-unscaled offsets before register,
  scaled-register, extended-register, or late scratch-address forms.
- [ ] Keep program-memory metadata distinct from spill-memory metadata.
- [ ] Audit GEP semantics against `raana_ir/src/llvm/writer.rs`.
- [ ] Lower folded constant GEP offsets, dynamic i32 indices, widened indices,
  power-of-two scaled indexing, non-power-of-two multiply-add indexing, and
  nested array indexing.
- [ ] Materialize global addresses with `adrp` plus `:lo12:`.
- [ ] Lower scalar and nested aggregate global initialization through generic
  global emission.
- [ ] Lower local aggregate and zero initialization completely before optional
  store-pair, memset, or loop-fill optimization.

## ABI, Frames, And Late Legalization

- [ ] Verify AAPCS64 argument and return behavior for independent x0..x7 and
  v0..v7 windows, eight-byte overflow slots, unit values, and declarations
  without bodies.
- [ ] Reserve the maximum outgoing overflow-argument area once per frame;
  calls must not dynamically adjust SP.
- [ ] Preserve 16-byte SP alignment at every ABI boundary.
- [ ] Save and restore only allocated x19..x28 registers in deterministic
  order; keep v8..v15 unallocatable until their low-64-bit save/restore is
  implemented.
- [ ] Add late legalization for large stack adjustments, frame offsets, spill
  offsets, stack-to-stack edits, and symbolic locations not directly encodable
  by AArch64.
- [ ] Restrict late-legalization temporaries to
  `MachineEnv.post_ra_scratch_by_class`; never allocate vregs or untracked
  stack slots during legalization.
- [ ] Use pair save/restore only for legal adjacent registers and locations.

## CFG And Allocation

- [ ] Validate diamonds, loops, break/continue, critical edges, loop-carried
  block parameters, and true/false edges with different arguments.
- [ ] Validate register-to-register, register-to-spill, spill-to-register, and
  spill-to-spill parallel-copy cycles using generic ABI move/spill hooks.
- [ ] Support i32, pointer, and f32 block-parameter transfers with correct
  register classes and memory widths.
- [ ] Remove unconditional jumps only when the successor is physically next and
  its edge has no required work.

## Diagnostics And Documentation

- [ ] Replace selector `unreachable!` paths for unsupported user HIR with
  concise code-generation errors containing function, block where available,
  HIR instruction, source/target type, phase, and legality reason.
- [ ] Add debug observability for HIR functions, block order, selected MInst,
  verification failures, operand constraints, allocations, allocator edits,
  frame layout, legalized MInst, and final assembly.
- [ ] Update `anon_armv8/README.md`, root `README.md`, `AGENTS.md`, CLI help,
  and Makefile documentation for the single AArch64 backend and its AAPCS64
  policy.

## Performance

- [ ] Build measurement infrastructure separating compiler time, IR dumping,
  assembly/linking, QEMU startup, and application runtime.
- [ ] Parse SysY `TOTAL` timing into structured samples and report median,
  geometric mean, code size, instruction mix, and confidence intervals.
- [ ] Apply optimizations only after semantic implementation and validation:
  address folding, immediates, direct compare-to-branch, safe fallthrough,
  shifted/extended ALU operands, `madd`/`msub`, safe pairs, `tbz`/`tbnz`, and
  finally `csel`.
- [ ] Use QEMU for semantic and gross instruction-count checks only; require
  native AArch64 measurements for microarchitectural performance claims.

## Tests And Final Validation

- [ ] Add MInst tests for operands, fixed ABI constraints, reuse constraints,
  clobbers, metadata, verification failures, allocation write-back, GNU
  formatting, and immediate/address boundary cases.
- [ ] Add ABI and allocation tests for 0/1/8/9 integer and float arguments,
  mixed signatures, returns, nested calls, recursion, live-across-call values,
  frame variants, spills, large offsets, stack-to-stack moves, and alignment.
- [ ] Add selector, memory, and CFG tests for immediate/fused arithmetic,
  signed division/remainder/shifts, f32 behavior and NaNs, locals, arrays,
  globals, aggregates, GEPs, edge copies, and unsupported diagnostics.
- [ ] Run `cargo fmt --check`.
- [ ] Run `cargo test -p taki_mir`.
- [ ] Run `cargo test -p anon_armv8`.
- [ ] Run `cargo test --workspace`.
- [ ] Run `cargo build -p soyo_compiler`.
- [ ] Run `make test`.
- [ ] Run `make test ARGS="-O 1"`.
- [ ] Run `make test-llvm`.
- [ ] Run `git diff --check`.
- [ ] Run focused Clang assembly acceptance, AArch64 static-linking, QEMU,
  LLVM-differential, diagnostic, and debug-output checks.

## Completion Criteria

- [ ] Every currently accepted SysY HIR construct lowers through typed AArch64
  VCode.
- [ ] AAPCS64 integer, pointer, f32, stack-argument, caller-save, callee-save,
  and frame behavior is implemented.
- [ ] Globals, arrays, aggregates, dynamic GEPs, recursion, and edge-specific
  CFG transfers are implemented.
- [ ] All final unit, workspace, Clang, QEMU, LLVM, diagnostic, formatting, and
  performance validation requirements pass.

## Deferred After Completion

- [ ] Re-evaluate per-type spill-slot sizes after the fixed eight-byte policy is
  proven.
- [ ] Consider general load/store-pair formation beyond callee saves and
  explicitly paired semantic operations.
- [ ] Consider f32 constant-pool deduplication after lowering stabilizes.
- [ ] Consider aggressive block placement beyond local correctness-preserving
  fallthrough.
- [ ] Keep SIMD/vector allocation, indirect calls, tail calls, exceptions, TLS,
  atomics, and unneeded AArch64 extensions out of scope unless accepted HIR
  requires them.
