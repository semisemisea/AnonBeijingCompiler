# Compiler TODO

Only pending work belongs in this file. Completed implementation notes and
historical measurements belong in commits, tests, or dedicated documentation.

## Performance Priorities

Preserve the fully passing functional suite while improving `-O1` AArch64
performance. Tail-call optimization is being developed separately; do not use
the `h-1-*` results to prioritize the work below.

The current bottlenecks are primarily calls, branches, address calculation,
memory traffic, and loop optimization. Extending scalar GVN/GVN-PRE is not the
next priority.

### Phase 1: Broad, Low-Risk Backend and IR Wins

- [ ] Add selective inlining for small non-recursive functions.
  - Always consider tiny arithmetic/index helpers such as `idx`, `max`, and
    `hash`.
  - Increase the budget for calls inside loops.
  - Exclude recursive SCCs and large functions initially.
  - Preserve valid SSA/block-parameter form after cloning the callee CFG.
- [ ] Make block layout and branch emission fallthrough-aware.
  - Remove jumps to the immediately following block.
  - Emit one conditional branch when either successor is the fallthrough.
  - Invert conditions when that avoids an unconditional branch.
  - Prefer contiguous loop bodies with exits placed out of the hot path.
- [ ] Fold single-use GEPs into AArch64 load/store addressing modes.
  - Support base plus encodable constant offset.
  - Support scaled or extended register offsets such as
    `[base, index, sxtw #2]`.
  - Fall back to explicit address materialization when the expression is not
    directly encodable.
- [ ] Stop unconditionally homing register arguments to stack at function
  entry.
  - Model incoming argument registers as fixed definitions or entry copies.
  - Spill arguments only when required by register allocation.
  - Verify recursive functions, leaf functions, mixed integer/float arguments,
    and stack-passed arguments.

### Phase 2: Loop Infrastructure

- [ ] Implement reusable natural-loop analysis.
  - Record headers, backedges, latches, exits, nesting, parent/child loops, and
    the innermost loop containing each block.
  - Handle multiple latches and reject or conservatively classify irreducible
    CFGs.
- [ ] Canonicalize loops for later transformations.
  - Create preheaders.
  - Create dedicated latches or exits only when required.
  - Correctly rewrite block parameters and logical CFG edges.
- [ ] Implement conservative LICM.
  - Initially hoist non-trapping integer arithmetic, casts, selects, GEPs, and
    global addresses whose operands are loop-invariant.
  - Do not hoist loads, calls, or possibly trapping operations without the
    required safety analysis.
- [ ] Implement basic induction-variable and pointer strength reduction.
  - Recognize constant-step basic induction variables.
  - Replace repeated `base + index * stride` address calculations with
    loop-carried pointers where profitable.
  - Begin with single-latch canonical loops and retain the original induction
    variable when other users still need it.

### Phase 3: Effects and Memory Optimization

- [ ] Replace the current purity scaffolding with program-local function effect
  summaries computed to a call-graph fixed point.
  - At minimum distinguish `ReadNone`, `ReadOnly`, argument-memory writes, and
    global/unknown writes.
  - Avoid thread-local caches that can retain data from another `Program`.
  - Integrate summaries with DCE, GVN, LICM, and later call optimization.
- [ ] Add conservative block-local memory value numbering.
  - Exact-address load CSE.
  - Exact-address store-to-load forwarding.
  - Dead overwrite and redundant-store removal.
  - Invalidate state on aliasing stores, unknown calls, and memory intrinsics.
- [ ] Add simple alias/provenance facts before attempting cross-block memory
  optimization.
  - Distinguish separate stack allocations and globals.
  - Track GEP base objects and known constant offsets.
  - Track whether local objects escape to calls or stores.
- [ ] Add scalar replacement for small non-escaping local arrays with constant
  indices.
- [ ] Remove stores to private globals when whole-program use/effect analysis
  proves that the stored values are never observed.

### Phase 4: Loop Expansion and Locality

- [ ] Add fixed-trip and partial loop unrolling with a strict code-size budget.
  - Prioritize the 5x5 convolution kernel, 32-iteration bit helpers, crypto
    rounds, radix-sort buckets, and small matrix inner-loop factors.
- [ ] Add interior/border loop splitting for stencil and convolution kernels so
  bounds checks can be removed from interior iterations.
- [ ] Add scalar reduction promotion and multiple accumulators for suitable
  inner loops.
- [ ] Investigate dependence-aware loop interchange and cache tiling for matrix
  multiplication, LU decomposition, transpose, and dynamic-programming cases.
  Do not transform in-place kernels without proving dependence legality.
- [ ] Consider SIMD only after scalar loop, memory, and addressing work is
  stable. This requires vector HIR/MIR types, AArch64 vector lowering, register
  allocation support, legality checks, and a cost model.

## Benchmark Guidance

- `many_mat_cal-*`: current TLE cases. Prioritize address induction, branch
  reduction, unrolling, locality, and repeated-reduction recognition. The
  matrix update writes `A` in place, so generic GEMM transformations may be
  illegal.
- `huffman-*`: dominated by calls and fixed 32-iteration software bit
  operations. Prioritize inlining, constant-argument specialization, unrolling,
  and only use native bit idioms when range/sign semantics prove equivalence.
- `matmul*`, `h-5-*`, `01_mm*`, `h-8-*`: prioritize GEP folding, LICM, pointer
  induction, scalar reduction promotion, unrolling, and eventually tiling or
  SIMD.
- `h-4-*`: use as the main inlining/ABI/branch-quality test. Tiny `max` calls
  should become straight-line compare/select code.
- `fft*`: after effect analysis, eliminate duplicate pure calls and hoist
  `power(d, mod - 2)` out of the normalization loop. FFT recursion is not
  tail-call dominated.
- `conv2d-*`: inline `idx`, specialize the fixed 5x5 kernel, split interior from
  borders, and investigate removal of overwritten repeated convolution runs.
- `transpose*`: target redundant stores, address induction, and blocked
  traversal.
- `sl*`: target address generation and stores to the unobserved `y` array;
  preserve the in-place dependence of `x` updates.
- `crc*`: investigate elimination of overwritten outer-loop results, then
  inline and specialize the software bit helpers.
- `shuffle*`: prioritize inlining, ABI overhead, branch layout, and register
  allocation; irregular pointer/index chasing limits classical loop transforms.

## Verification

- [ ] Keep targeted unit tests for every new analysis and transformation,
  including parallel edges, critical edges, block parameters, nested loops,
  and negative legality cases.
- [ ] Run `cargo test --offline --locked --workspace` after optimizer or backend
  changes.
- [ ] Run the full Docker/QEMU functional suite before accepting a performance
  change.
- [ ] Compare generated AArch64 assembly, instruction counts, spills, and perf
  runtime before and after each optimization family.
- [ ] Add profitability limits where a transformation can increase live ranges,
  register pressure, CFG size, or code size.
