# Compiler TODO

Only pending work belongs in this file. Completed implementation notes and
historical measurements belong in commits, tests, or dedicated documentation.

## Remaining Validation: Integer Strength Reduction

- [ ] Investigate the pre-existing RISC-V register-allocation assertion exposed
  by compiling `tests/debug/01_and.sy` at both `-O0` and `-O1`:
  `PReg { repr: 10 } should not be allocatable` from the `StoreWord` operand
  path. `tests/functional/pow2_div_rem.sy` passes on RISC-V at `-O1`, so this
  is not caused by the new immediate-shift lowering.
- [ ] Record static `mul`, `div`, `rem`, `shift`, `and`, and total instruction
  counts for `01_and`, Huffman, CRC, and crypto only after the semantic tests
  pass. Do not treat one assembly snippet as a substitute for full regression
  coverage.

## Explicitly Deferred

- [ ] Arbitrary constant signed/unsigned division and remainder magic-number
  optimization remains deferred until RaanaIR defines `mulhi` semantics and
  AArch64/RISC-V lowering, constant propagation, GVN, LLVM writing, and edge
  tests all support it.
- [ ] Cost-model-driven negative power-of-two multiplication, arbitrary
  shift/add/sub constant multiplication synthesis, target-specific peepholes,
  and loop induction-variable optimizations require separate measurement.
- [ ] `&&` to AArch64 `ccmp`, condition updates to `csel`, loop rotation, and
  stack-frame elimination remain separate boolean/if-conversion, instruction
  selection, loop optimization, and frame/liveness tasks.
