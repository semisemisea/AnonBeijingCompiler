# Compiler TODO

Only pending work belongs in this file. Completed implementation notes and
historical measurements belong in commits, tests, or dedicated documentation.

---

# IPSCCP block-parameter constant propagation breaks LLVM writer

## Symptom

22 functional tests fail with `CE` in the LLVM backend at `-O1`
(`21_if_test2`, `22_if_test3`, `24_if_test5`, `102_phase1_large_frame`,
`104_gep_scaling`, …). AArch64 and RISC-V backends are unaffected (149/149 each).

## Root cause

IPSCCP's constant-replacement pass mutates a `BlockArgRef` (entry/block parameter)
**in-place** to `Integer(val)` via `replace_inst_with(node.inst).integer(val)`
when the parameter's lattice converges to a constant. The subsequent
`layout().parent_bb(node.inst)` check returns `None` for parameters (they are not
in the instruction layout), so `detach_layout_inst` is skipped — but the data
mutation has already happened. The block parameter's `InstData` is now an
`Integer` instead of a `BlockArgRef`.

The LLVM writer then emits a PHI for this "parameter":

```llvm
L3:
  -5 = phi i32 [ -5, %L2 ]     ; ← invalid: constant as SSA name
```

which `llc` rejects: `error: expected instruction opcode`.

## Evidence

```
results/functional/21_if_test2.runtime.stderr:
  llc: error: llc: /work/.../21_if_test2.ll:25:3: error: expected instruction opcode
    -5 = phi i32 [ -5, %L2 ]
    ^
```

Raana IR confirms the block parameter was folded:
```
end_3(-5: i32):
    jump end_1
```

## Fix direction

In IPSCCP's `const_replace_list` replacement loop, skip nodes whose instruction is
a `BlockArgRef` (block/function parameter). Parameters should not be mutated
in-place; instead, their *uses* should be replaced with the constant via
`visit_and_replace` (the same mechanism used for `Call` results).

Alternatively, the LLVM writer should detect constant block parameters and emit
the literal value instead of a PHI — but fixing IPSCCP is the correct approach
since the IR invariant "block parameters are `BlockArgRef`" should be preserved.

## Scope

Pre-existing — introduced by origin/main's CI-IP-SCCP (`2af7360`). Unrelated to
the TailCall ICFG fix. AArch64/RISC-V backends tolerate the mutated parameter
because they lower block parameters via vregs, not by inspecting the
`InstKind`.
