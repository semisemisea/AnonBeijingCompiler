# AArch64 Backend

`anon_armv8` is the AArch64 target implementation for the generic `taki_mir`
VCode pipeline. Production compilation uses:

```text
SysY -> RaanaIR -> VCode/MIR -> register allocation -> AAPCS64 frame -> GNU AArch64 assembly
```

The crate owns only AArch64-specific pieces:

- `regs.rs`: AAPCS64 physical-register and allocator policy.
- `instructions.rs`: typed machine instructions, operand constraints, and GNU
  assembly emission.
- `constants.rs`: width-aware integer constant planning.
- `abi.rs`: AAPCS64 argument locations, frame hooks, and post-allocation
  address legalization.
- `lower.rs`: RaanaIR instruction and branch selection.
- `labels.rs`: typed block, function, and global labels.

The generic `taki_mir` crate owns VCode construction, block-parameter edge
transfers, register allocation, frame layout, and final output sequencing.
There is no direct AArch64 allocation driver, compatibility backend, or
fallback assembly path.

## ABI Policy

- Integer and pointer arguments use `x0` through `x7`; f32 arguments use
  `v0` through `v7` independently.
- Scalar overflow arguments occupy eight-byte stack slots.
- Calls reserve the maximum outgoing overflow-argument area in the frame and
  do not adjust SP dynamically.
- SP remains 16-byte aligned at ABI boundaries.
- Integer callee saves are allocated `x19` through `x28`; `v8` through `v15`
  remain unallocatable until their low-64-bit preservation is implemented.

## Output

Use the CLI's generic entrypoint:

```bash
soyo_compiler -S --target aarch64 -o testcase.s testcase.sy
```

The emitted assembly uses GNU AArch64 syntax and is intended for the AArch64
test harness and cross-linker described in the workspace `AGENTS.md`.

## Diagnostics

Unsupported user HIR is reported as a code-generation error rather than a
selector panic. The diagnostic identifies the lowering phase, function, source
block when available, HIR instruction, relevant source/target types, and the
unsupported legality reason. Internal VCode, allocator, and target-encoding
invariants remain fail-fast errors because they indicate compiler defects.
