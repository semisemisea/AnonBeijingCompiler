# AArch64 Backend TODO

## Scope And Invariants

- [ ] Preserve the sole AArch64 pipeline: SysY -> Raana HIR -> generic VCode ->
  register allocation -> AAPCS64 frame finalization -> GNU AArch64 assembly.
- [ ] Keep target-specific production code in `anon_armv8/src/{abi,constants,
  instructions,labels,lower,regs}.rs` and module exports in `lib.rs`.
- [ ] Do not restore a direct backend, custom allocation driver, post-RA emitter,
  private frame layout, compatibility wrapper, fallback backend, or
  `--asm-backend` flag.
- [ ] Keep selector code on public `LowerContext` APIs and preserve explicit,
  non-allocatable SP and ZR operands.

## Correctness Test Suite

- [ ] Add VCode verification tests for CFG metadata, terminators, edge argument
  counts/classes, target instruction verification failures, and allocation
  write-back.
- [ ] Add AArch64 MInst tests for operands, fixed ABI constraints, reuse
  constraints, clobbers, metadata, GNU formatting, and immediate/address
  boundary cases.
- [ ] Add AArch64 ABI/allocation tests for 0/1/8/9 integer and float arguments,
  mixed signatures, returns, nested calls, recursion, live-across-call values,
  frame variants, spills, large offsets, stack-to-stack moves, and alignment.
- [ ] Add selector, memory, and CFG tests for immediate/fused arithmetic, signed
  division/remainder/shifts, f32 behavior and NaNs, locals, arrays, globals,
  aggregates, GEPs, edge copies, and unsupported diagnostics.
- [ ] Add integration fixtures for critical edges, distinct edge arguments,
  loop-carried values, forced spill cycles, large frames, and mixed overflow
  integer/float argument windows.

## Correctness Validation

- [ ] Run `cargo fmt --check` and `git diff --check` after the replacement test
  suite is complete.
- [ ] Run focused new MIR and AArch64 unit tests, then `cargo test -p taki_mir`
  and `cargo test -p anon_armv8`.
- [ ] Run `cargo test --workspace` and `cargo build -p soyo_compiler`.
- [ ] Run focused AArch64 end-to-end cases for remainder, float/NaN behavior,
  calls, register pressure, loops, arrays, globals, and CFG edges.
- [ ] Run `make test`, `make test ARGS="-O 1"`, and `make test-llvm`.
- [ ] Run focused Clang assembly acceptance, AArch64 static-linking, QEMU,
  LLVM-differential, diagnostic, and debug-output checks.

## Performance Measurement

- [ ] Build measurement infrastructure that separates compiler/frontend time,
  optimization, lowering/allocation/emission, optional IR dumping,
  assembly/object generation, linking, QEMU startup, and application runtime.
- [ ] Parse SysY `TOTAL` timing into structured repeated samples and report
  median, geometric mean, confidence intervals, code size, and instruction mix.
- [ ] Freeze reproducible `-O0` and `-O1` baselines only after correctness
  validation passes.
- [ ] Use QEMU only for semantic and gross instruction-count checks; require
  native AArch64 measurements for microarchitectural performance claims.

## Performance Optimizations

- [ ] Remove an unconditional jump only when its successor is physically next
  and no edge block, allocator edit, or block-parameter work is bypassed.
- [ ] Measure and audit existing address folding, immediate forms, direct
  compare-to-branch, shifted/extended operands, `madd`/`msub`, safe pairs, and
  `tbz`/`tbnz`; retain changes only with demonstrated benefit.
- [ ] Add `csel` only for measured profitable, side-effect-free patterns after
  branch and register-pressure behavior is proven.
- [ ] Re-run affected unit and end-to-end correctness tests after every retained
  optimization.

## Final Validation And Completion

- [ ] Re-run formatting, crate, workspace, build, AArch64 `-O0`, AArch64 `-O1`,
  LLVM, Clang, static-linking, QEMU, differential, diagnostic, and debug-output
  validation after optimization.
- [ ] Confirm every currently accepted SysY HIR construct lowers through typed
  AArch64 VCode.
- [ ] Confirm AAPCS64 integer, pointer, f32, stack-argument, caller-save,
  callee-save, and frame behavior is covered by passing tests.
- [ ] Confirm globals, arrays, aggregates, dynamic GEPs, recursion, and
  edge-specific CFG transfers are covered by passing tests.
- [ ] Publish final native AArch64 performance results with reproducible
  measurement metadata and benchmark-level regressions identified.

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
