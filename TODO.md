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

## Boolean Optimization

### Completion Record (2026-07-24)

The foundational target-independent boolean optimization milestone is complete.
The remaining unchecked items below are deliberately deferred follow-up work:
comparison-pair composition, boundary canonicalization, additional frontend and
float coverage, CFG dataflow proofs, post-pass CFG fixed points, and a
`CondResult` lowering abstraction.

- `BooleanSimplification` runs to a local fixed point after `IfConversion` and
  before DPE/DCE at `-O1`.
- Dynamic `!x` now lowers directly to `x == 0` (or `x == 0.0`), and direct
  comparison values are no longer re-tested for truthiness by the frontend.
- Integer comparison wrappers, self-comparisons, branch/select zero tests, and
  proven canonical-boolean selects are simplified without conflating arbitrary
  nonzero `i32` values with `0/1` booleans.
- Float relational complements remain intentionally disabled for NaN safety.
  LLVM float `!=` now uses `fcmp une`, agreeing with frontend folding and
  AArch64 `fcmp` plus `cset ne`.
- `h_functional/29_long_line.sy -O1`: RaanaIR lines `7774 -> 5965`, AArch64
  assembly lines `25205 -> 8949`, `neq(..., 0)` `1390 -> 94`, and `cset`
  `861 -> 248`. The optimized case passes under QEMU.
- Validation passed: `cargo test --workspace`, `cargo test -p anon_armv8 lower`,
  targeted O0/O1 AArch64 and LLVM functional cases, the dynamic nested-negation
  fixture, and `git diff --check`. `cargo fmt --check` remains blocked by the
  pre-existing missing `dependencies/rust-smallvec/tests/debugger_visualizer.rs`.

### Scope And Semantics

- [x] Distinguish three concepts in all optimization design and reviews:
  arbitrary integer truth values (`0` is false and any nonzero value is true),
  canonical scalar booleans (exactly `0` or `1`), and comparisons (operands plus
  a condition code). Do not treat every `i32` as a canonical boolean.
- [x] Preserve the current RaanaIR convention that every `BinaryOp::is_compare()`
  result has type `i32` and value `0` or `1`; document this as the proof source
  for boolean rewrites in `raana_ir/src/ir/inst_kind/binary.rs`.
- [x] Preserve SysY/C truthiness at all control-flow and select consumers: an
  arbitrary integer `x` is true iff `x != 0`. This permits stripping
  truthiness-preserving wrappers from branch/select conditions, but does not
  permit replacing a boolean-producing expression with arbitrary `x`.
- [x] Never model logical not as bitwise complement. `~0` and `~1` are both
  nonzero under branch truthiness; `x xor 1` is logical inversion only after
  proving `x` is canonical `0/1`.
- [x] Keep integer comparison semantics signed `i32` unless the IR explicitly
  gains unsigned predicates. Ensure comparison rewrites preserve operand type,
  signedness, and `i32` result type.
- [x] Treat float comparison complement separately from integer complement.
  Do not rewrite `!(a < b)` to `a >= b`, or analogous ordered relational forms,
  because NaN makes them inequivalent. Restrict target-independent float
  rewrites to proven identity/truthiness cases until RaanaIR has explicit
  ordered/unordered float predicates.
- [x] Resolve and test the existing float `NotEq` semantic discrepancy before
  adding float `Eq`/`NotEq` complement rules: the LLVM writer currently emits
  `fcmp one`, while frontend folding and AArch64 `cset ne` treat unordered/NaN
  as not-equal. Choose one documented SysY semantic contract and make frontend,
  RaanaIR, LLVM, and AArch64 agree.

### Comparison Algebra

- [x] Add a centralized, target-independent comparison algebra API adjacent to
  `BinaryOp` in `raana_ir/src/ir/inst_kind/binary.rs`. At minimum provide
  `is_compare`, integer-only `complement`, and `swap_args`; return `Option` for
  operations that are not semantically valid for every comparison domain.
- [x] Implement and unit-test integer complement mapping:
  `Eq <-> NotEq`, `Lt <-> Ge`, `Le <-> Gt`, `Gt <-> Le`, and `Ge <-> Lt`.
  Do not expose this as a generic float complement API.
- [x] Implement and unit-test operand-swap mapping:
  `Eq -> Eq`, `NotEq -> NotEq`, `Lt <-> Gt`, and `Le <-> Ge`. Canonicalize
  compare constants to the RHS when doing so improves matching and immediate
  instruction selection.
- [x] Fold integer self-comparisons where operand identity is exact:
  `x == x -> 1`, `x != x -> 0`, `x < x -> 0`, `x <= x -> 1`, `x > x -> 0`, and
  `x >= x -> 1`. Do not apply these to float comparisons because NaN breaks the
  equality and non-strict-order cases.
- [ ] Add only measured, domain-safe compare-against-boundary canonicalizations
  after the basic boolean pass is stable, such as signed comparisons against
  `0`, `1`, `i32::MIN`, and `i32::MAX`. Keep ISA encoding-driven rewrites out of
  RaanaIR and in the respective target lowering.
- [ ] Defer comparison-pair composition until basic canonicalization is proven.
  The later design should use a `{LT, EQ, GT}` outcome mask so that same-operand
  expressions can safely compose, e.g. `(x < y) || (x == y) -> x <= y` and
  `(x <= y) && (x != y) -> x < y`. Require identical operands and compatible
  comparison domains; do not introduce general SAT-like boolean reasoning.

### Frontend Canonicalization

- [x] Make `AstGenContext::truthy_local` in
  `soyo_compiler/src/frontend/utils.rs` idempotent for direct comparison values:
  `truthy_local(compare(...)) -> compare(...)`. Initially recognize direct
  `BinaryOp::is_compare()` producers and integer constants `0`/`1`; do not
  classify arbitrary `i32` values as canonical booleans.
- [x] Change dynamic unary `!` lowering in `soyo_compiler/src/frontend/ast.rs`
  from `(rhs != 0) == 0` to a direct `rhs == zero_of(rhs.type)` after the
  AArch64 compare-select path accepts the newly exposed zero-comparison shape.
  Preserve the existing constant folding and ensure both integer and float zero
  construction retain their operand type.
- [x] Verify direct float `!rhs -> rhs == 0.0` preserves desired semantics for
  `+0.0`, `-0.0`, infinities, and NaN before enabling the change.
- [x] Keep `&&` and `||` short-circuit CFG lowering intact. Only remove repeated
  truth conversion of already canonical operands; never eagerly replace
  side-effecting logical expressions with arithmetic `And`/`Or`.
- [ ] Add frontend IR-shape tests for `if (x < y)`, `while (x != 0)`, `!x`,
  `!!x`, `!!!x`, comparison operands of `&&`/`||`, and side-effecting short
  circuit expressions. Assert both semantic result and absence of avoidable
  `neq(compare, 0)` nodes.

### RaanaIR Boolean Simplification Pass

- [x] Add a dedicated target-independent `BooleanSimplification` pass under
  `raana_ir/src/opt/passes/`, export it through `passes.rs`, and register it in
  `raana_ir/src/opt/pass.rs` after `IfConversion` and before
  `DeadPhiElimination`/`DeadCodeElimination`.
- [x] Make the pass iterate to a local fixed point. Replacing an outer boolean
  wrapper can expose another wrapper, so a single forward scan is insufficient
  for chains such as `neq(eq(neq(x, 0), 0), 0)`.
- [x] Reuse existing use-def replacement and layout-removal APIs rather than
  mutating operands directly. Ensure every rewrite maintains inverse user sets,
  block layout ownership, and instruction type invariants; leave newly dead
  producers for final DCE unless immediate removal is demonstrably safe.
- [x] Define a conservative `CanonicalBool` recognizer for the first pass
  version. It must accept direct comparison results and constants `0`/`1`; it
  may recursively accept `select` values only when both arms are canonical.
  Defer block-parameter/cyclic CFG dataflow proof until a separate analysis is
  justified.
- [x] Fold canonical-boolean identity wrappers in either operand order:
  `b != 0 -> b`, `b == 1 -> b`, `0 != b -> b`, and `1 == b -> b`.
- [x] Fold canonical-boolean inversion wrappers when the result can remain a
  semantic boolean: `b == 0`, `0 == b`, `b != 1`, and `1 != b`. If `b` is an
  integer comparison, replace these with the centralized integer-complement
  comparison; otherwise retain a minimal canonical inversion representation.
- [x] Apply compare-of-compare rules corresponding to Cranelift's
  `opts/icmp.isle`: `compare != 0 -> compare`, `compare == 1 -> compare`,
  integer `compare == 0 -> complemented compare`, and integer
  `compare != 1 -> complemented compare`. For floats, initially apply only the
  non-inverting identity rules unless the chosen NaN semantics provide an exact
  representable complement.
- [x] Simplify truthiness only in truthiness consumers. Rewrite
  `branch(x != 0, T, F)` to `branch(x, T, F)` for arbitrary integer `x`.
  Rewrite `branch(x == 0, T, F)` to `branch(x, F, T)` only when targets and both
  edge-argument vectors are swapped as one atomic transformation.
- [x] Evaluate equivalent select-condition rewrites for arbitrary integer values:
  `select(x != 0, a, b) -> select(x, a, b)` and
  `select(x == 0, a, b) -> select(x, b, a)`. Do not enable them until the
  AArch64 compare-select path accepts arbitrary truthy values without violating
  fixed zero-register constraints.
- [x] Simplify select forms only when their value-range proof is sufficient:
  `select(canonical_bool, 1, 0) -> canonical_bool` and
  `select(canonical_bool, 0, 1) -> logical_not(canonical_bool)`. Explicitly
  retain `select(arbitrary_i32, 1, 0)`, whose result is canonical while its
  condition is not.
- [x] Fold unconditional select identities independent of boolean provenance
  when sound, including `select(x, x, 0) -> x` and equal-arm selects. Verify
  every candidate against the IR's nonzero condition semantics before adding it.
- [ ] Ensure branch inversion handles parameterized successors correctly. Add
  focused tests where true and false edges target different blocks, target the
  same block with distinct argument vectors, and carry values into a merge.
- [ ] Run `DeadPhiElimination`, DCE, and CFG cleanup after boolean rewriting so
  bypassed comparisons, selects, edge arguments, and empty blocks are removed.
  Re-evaluate whether a second DPE/DCE/SimplifyCFG fixed-point group is needed
  after measuring real artifacts.

### AArch64 Condition Selection

- [x] Retain existing direct condition selection in
  `anon_armv8/src/lower.rs`: single-use comparisons should lower to
  `cmp/fcmp + b.cond` for branches and to the atomic compare-select pseudo for
  selects, rather than materializing `cset` followed by another comparison.
- [x] Validate that the new RaanaIR pass increases hits for existing
  `select_branch_condition` and `select_comparison` without violating their
  single-use producer-claim rules.
- [ ] Keep `cbz`/`cbnz` selection for arbitrary integer equality/non-equality
  against zero, and `tbz`/`tbnz` selection for one-bit mask tests. These are
  target-specific lowering choices, not RaanaIR rewrites.
- [ ] Do not implement a post-emission `cmp/cset/cmp` assembly peephole. It
  cannot reliably reconstruct comparison provenance or NZCV liveness and would
  duplicate semantics that belong in the frontend/mid-end/lowering layers.
- [ ] Preserve the current atomic compare-and-select representation so register
  allocation cannot separate an NZCV-producing comparison from its `csel` or
  `cset` consumer.
- [ ] Consider a private AArch64 lowering `CondResult` abstraction only after
  the target-independent pass is complete and measured. It should represent
  zero/nonzero register tests and a flags producer plus condition code, shared
  by branch/select/boolean materialization; it must not expose NZCV as a normal
  RaanaIR or MIR virtual register.
- [ ] Keep final branch inversion/fallthrough selection separate from semantic
  boolean rewriting. Two-target branch pseudos may invert their condition after
  block layout to make one successor fall through, provided edge-copy ownership
  and branch-range handling remain correct.

### Test Matrix And Acceptance Criteria

- [x] Add RaanaIR unit tests for comparison algebra, compare-of-compare rules,
  fixed-point convergence, use-def integrity, and preserved instruction layout.
- [x] Add positive and negative canonical-boolean tests: direct comparisons and
  `select(..., 0, 1)` must simplify where proven, while arbitrary integer
  conditions must not be substituted for a boolean result.
- [x] Add integer branch/select tests for `x != 0` and `x == 0`, including
  swapped true/false targets and edge arguments. Assert target/argument pairing
  rather than only final textual IR.
- [ ] Add float tests covering `+0.0`, `-0.0`, finite nonzero values, infinities,
  and NaN for `!x`, `x == 0.0`, `x != 0.0`, nested logical not, branch use, and
  select use. Run the same cases through AArch64 and LLVM emission once float
  semantics are unified.
- [ ] Add AArch64 lowering tests showing a comparison used only by a branch
  emits comparison plus conditional branch without `cset`; a comparison used
  only by select emits one atomic compare-select; and a comparison with an
  additional value use remains materialized safely.
- [ ] Add tests for direct `cbz`/`cbnz` and one-bit `tbz`/`tbnz` paths after
  truthiness canonicalization, including false-edge layout and condition
  inversion.
- [x] Exercise existing functional cases `functional/40_unary_op.sy`,
  `functional/41_unary_op2.sy`, `functional/50_short_circuit.sy`, and
  `functional/51_short_circuit3.sy` at both `-O0` and `-O1`; add a dynamic
  nested-negation fixture that cannot be constant-folded.
- [x] Re-run `h_functional/29_long_line.sy` at `-O1` and record before/after
  RaanaIR counts for `neq(compare, 0)`, nested comparison wrappers,
  `select(condition, 1, 0)`, and parameterized boolean merge values.
- [x] Record before/after AArch64 counts for `cset`, `cmp`/`fcmp`, conditional
  branches, branch edge blocks, loads/stores attributable to boolean temporaries,
  spill slots, frame size, and total static instructions. Attribute improvements
  separately from jump-only-block elimination, division/remainder strength
  reduction, and register-allocation changes.
- [x] Require focused unit tests, `cargo test --workspace`, focused `make test`
  cases with and without `ARGS="-O 1"`, and `make test-llvm` before accepting
  each retained boolean optimization. Run `cargo fmt --check` and
  `git diff --check` after the complete series.

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
