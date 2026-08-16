//! M44 v1: loop vectorizer (AArch64, IR layer, NEON, VF=4, i32/f32).
//!
//! Vectorizes innermost, rotated (test-at-bottom) count-up loops whose body
//! is a pure element-wise computation over contiguous 4-byte accesses:
//!
//! ```text
//! preheader: t0 = sub(bound, i0); positive = gt(t0, 0);
//!            br positive, header([i0, t0]), exit
//! header([iv, t]): jump latch
//! latch:    [gep/load/binary/store ... indexed by iv]
//!            iv' = add(iv, 1); t' = sub(t, 1);
//!            br t', header([iv', t']), exit
//! ```
//!
//! The entry edge may also be a plain `jump header([i0, t0])` when an
//! earlier pass constant-folded the trip-zero guard.
//!
//! With an exact compile-time trip `N = 4Q + R` the loop is rewritten in
//! place to run `Q` vector iterations (`<4 x i32>` / `<4 x f32>`):
//!
//!   1. The counter enters at `4Q` and steps by -4 (the latch test is
//!      unchanged: continue while the counter is nonzero); the index IV
//!      steps by +4.
//!   2. Contiguous loads become vector loads (same address), scalar
//!      Add/Sub/Mul/Shl/Shr/Sar/And/Or/Xor/Div/Min/Max become lane-wise
//!      vector ops (invariant operands are `VectorSplat`ed), contiguous
//!      stores become vector stores. Shifts/bitwise ops are integer-only
//!      (NEON sshl/and/orr/eor); Div is f32-only (fdiv v.4s — NEON has no
//!      integer vector divide); Min/Max cover i32 and f32
//!      (smin/smax/fmin/fmax).
//!   3. The `R` remaining scalar iterations (a compile-time constant in
//!      `0..=3`) are peeled as straight-line epilogue blocks with the index
//!      IV substituted by constants — no runtime guards, no remainder loop.
//!
//! v1 boundaries (宁漏勿错): no select, no reductions, no gathers
//! (non-4B strides rejected), no `MemZero` in the body, no loop versioning
//! (only exact trips), no loop interchange, and memory bases are accepted
//! only when provably 16B-aligned (array params are rejected; versioning
//! for them is M43).
//!
//! M42 verdict note: `DependenceAnalysis` labels the rotated trip counter
//! (`t' = t - 1`, otherwise unused in the body) as a `Reducible` IntSub
//! accumulator, so every rotated loop reports Reducible even when it has no
//! data reduction. Real reductions are rejected here by the operand rules
//! (an accumulator block-parameter cannot be a vector operand), so a
//! non-Forbidden verdict is accepted and the per-instruction checks below
//! do the actual filtering.
//!
//! Idempotence: after the rewrite the latch holds vector-typed loads/stores
//! and a counter step of 4, which the recognition checks reject, so the
//! fixed-point pipeline converges after one application.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::ir::{
    arena::Arena,
    builder_trait::*,
    inst_kind::{
        Binary, BlockArgRef, Float, GetElemPtr, InstKind, Integer, Load, Select, Store,
        VectorExtractElement, VectorInsertElement, VectorReduce, VectorReduceOp, VectorSplat,
    },
    types::{Type, TypeKind},
    BasicBlock, BinaryOp, Function, Inst, Program,
};
use crate::ir::function::FunctionData;
use crate::opt::{
    analysis_passes::{
        dependence::{AccessKind, DependenceAnalysis, ReductionOp, Verdict},
        dom_tree::v2::DominanceTree,
        effects::{AbstractObject, EffectAnalysis},
        induction_variable::BasicInductionVariableAnalysis,
        loop_analysis::{Loop, LoopAnalysis},
        memory::MemObject,
        range::RangeAnalysis,
        return_summary,
    },
    pass::{ArenaContext, ArenaContextMut, Pass},
    utils::cfg::CFG,
    utils::logical_edge::{incoming_edges, LogicalEdge, LogicalEdgeRewriter},
};

/// Vector width: 128-bit NEON registers over 4-byte elements.
const VF: i64 = 4;

pub struct LoopVectorize {
    /// Whole-program effect/alias analysis, rebuilt per `run` so a stale
    /// snapshot is never reused (same pattern as LICM).
    analysis: Option<EffectAnalysis>,
}

impl LoopVectorize {
    pub fn new() -> Self {
        Self { analysis: None }
    }
}

impl Pass for LoopVectorize {
    fn run(&mut self, program: &mut Program) -> bool {
        self.analysis = Some(EffectAnalysis::new(program));
        let func_layout = program.function_layout().to_vec();
        let mut changed = false;
        for func in func_layout {
            let mut arena_context = ArenaContextMut {
                program,
                curr_func: Some(func),
            };
            changed |= self.run_on(&mut arena_context);
        }
        changed
    }

    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        let Some(func) = data.curr_func else {
            return false;
        };
        if data.layout().entry_bb().is_none() {
            return false;
        }
        // Fusion pre-step: collapse 1-pred/1-succ chains inside loop bodies
        // (inlined `idx()` computations land in their own blocks) so the
        // shape contract below sees {header, latch} bodies. Semantically a
        // no-op; benefits every later pass in the same iteration too.
        let fused = fuse_loop_body_chains(data);
        // Vectorize as many loops as this function has in one invocation:
        // a single-apply call would leave sibling loops to a later
        // fixed-point iteration, by which point PSR has rewritten them into
        // advancing-pointer form (unanalyzable). Each apply mutates the IR
        // (exit-region rewrites touch sibling blocks), so the analysis is
        // refreshed between applications.
        let mut changed = fused;
        loop {
            // Direct invocations (unit tests) have no analysis yet; every
            // apply invalidates the previous whole-program effect snapshot.
            self.analysis = Some(EffectAnalysis::new(data.program));
            let program: &Program = data.program;
            let effects = self.analysis.as_ref().expect("analysis built above");
            let plan = find_vectorizable(program, func, effects);
            let Some(plan) = plan else {
                break;
            };
            // Mutation phase. A rejection leaves the IR untouched (or
            // partially rewritten by a later-stage guard) — break instead of
            // spinning: `find_vectorizable` is deterministic, so re-running
            // it without an IR change yields the same plan forever.
            if apply_vectorize(data, plan) {
                changed = true;
            } else {
                break;
            }
        }
        changed
    }
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

/// How a payload instruction is rewritten by the vectorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Contiguous load (byte_coefficient == 4): becomes a vector load.
    VecLoad,
    /// Invariant load (byte_coefficient == 0): stays scalar, splatted at use.
    InvLoad,
    /// Scalar Add/Sub/Mul: becomes a lane-wise vector op.
    VecBinary,
    /// Contiguous store (byte_coefficient == 4): becomes a vector store.
    VecStore,
    /// `Select(cond, t, f)` with a loop-invariant condition: rewritten as a
    /// lane-wise mask selection `(t & ~m) | (f & m)` with `m = -(cond == 0)`
    /// (all-ones when the condition is false). Lane-wise conditions are
    /// rejected (v3: needs a vector select / bsl at lowering).
    VecSelect,
    /// GEPs and constants: left untouched (kept for the epilogue clone).
    Keep,
}

struct VecPlan {
    header: BasicBlock,
    latch: BasicBlock,
    exit: BasicBlock,
    /// Entry edge into the header (guard branch or plain jump).
    entry_edge: Inst,
    /// Index induction variable (header parameter 0).
    iv: Inst,
    /// Trip counter (header parameter 1).
    counter: Inst,
    /// Compile-time initial index value.
    entry_i0: i64,
    /// Exact trip count (the counter's initial value).
    trip: i64,
    /// Latch terminator: `br t', header([iv', t']), exit`.
    latch_branch: Inst,
    /// `t' = sub(counter, 1)` (the branch condition and back-edge arg).
    t_next: Inst,
    /// `iv' = add(iv, 1)` (the back-edge arg).
    iv_next: Inst,
    /// Test-at-top bound instruction (the `lt`'s rhs). For a
    /// compile-time-constant bound it equals `i0 + trip`; for a runtime
    /// bound (`runtime_trip`) it is reused by the scalar tail loop as the
    /// tail's upper bound and by the exit edge as the IV final value.
    bound_inst: Inst,
    /// True when the test-at-top bound is a runtime value: the vector
    /// counter enters at `trip & -4` (computed on entry), and the
    /// `trip & 3` remainder is peeled as a scalar tail loop instead of
    /// constant epilogue blocks.
    runtime_trip: bool,
    /// Latch instructions to rewrite / clone, in layout order (excludes the
    /// latch machinery: terminator, condition, back-edge args).
    payload: Vec<Inst>,
    /// Rewrite class per payload instruction.
    classes: FxHashMap<Inst, Class>,
    /// `<4 x T>` result type (T = the loop's element type).
    vector_ty: Type,
    /// B1: register-reduction plan, or None for plain elementwise loops.
    reduction: Option<ReductionPlan>,
    /// Mod-wrapped scalar reduction `acc' = (acc + E + C) % P` (see
    /// [`ModReductionPlan`]). When Some, `reduction` is None and the
    /// accumulator stays scalar in the vector loop.
    mod_reduction: Option<ModReductionPlan>,
    /// Loop-invariant passthrough parameters: outer induction values the
    /// back edge forwards unchanged (e.g. the inner loop of a nested kernel
    /// reads the outer IVs). Their entry arguments stay live across the
    /// vectorized loop and are forwarded to the exit / epilogue chain.
    passthrough_args: Vec<Inst>,
    /// Exit-parameter classification, in exit-parameter order (how each
    /// exit parameter is fed at the rewritten exit edge).
    exit_specs: Vec<ExitArgSpec>,
    /// True when the loop has the test-at-top shape (header ends in
    /// `lt iv, bound; br`, the latch is a plain jump). The mutation phase
    /// materializes a trip counter as a new header parameter and replaces
    /// the bound test with the counter test.
    test_at_top: bool,
    /// B1: single-arm if body (masked store). When Some, the loop body is
    /// {header, body_br, arm, merge, latch}: body_br ends in
    /// `br mask_cond, merge, arm` (arm on the false edge, conservative),
    /// the arm is a single-exit block ending in `jump merge` holding the
    /// masked stores, and the merge reaches the latch. The arm's stores
    /// are rewritten with a lane-wise mask so the branch disappears.
    arm: Option<ArmPlan>,
    /// Milestone 2: a chain of masked multiply-accumulate units (the conv2d
    /// 5×5 kernel). Mutually exclusive with `arm`. When Some, the payload is
    /// collected from every unit's `then` block and the accumulator chain is
    /// re-typed to a vector.
    multi_arm: Option<MultiArmPlan>,
    /// 2x unroll the vector loop body: duplicate the payload so each array's
    /// two loads/stores are adjacent with a constant continuation GEP
    /// (`getelemptr %addr, 4`), which lowering folds into `[x, #16]` and the
    /// post-RA pair_combine fuses into `ldp q`/`stp q`. The counter and IV
    /// step by 8 instead of 4; the tail / epilogue cover `trip & 7`. Only
    /// elementwise loops (no reduction / mod / arm) with at least one vector
    /// load and one vector store, and a trip of at least 8 (const) or a
    /// runtime trip.
    unroll: bool,
}

/// B1 single-arm if body.
struct ArmPlan {
    /// The block ending in `br mask_cond, merge, arm`.
    body_br: BasicBlock,
    /// The arm block (false edge; terminator `jump merge`).
    arm: BasicBlock,
    /// The merge block (the branch's true edge; the arm jumps here).
    merge: BasicBlock,
    /// The branch condition selecting the arm (false edge).
    mask_cond: Inst,
    /// The branch instruction rewritten into `jump arm`.
    branch: Inst,
    /// True when the arm sits on the branch's *true* edge
    /// (`br cond, arm, merge` — the matmul masked-kernel shape), so the
    /// mask polarity flips (`m = -(cond != 0)` instead of `-(cond == 0)`).
    arm_on_true: bool,
}

/// One masked multiply-accumulate unit of a multi-arm if body (milestone
/// 2, the conv2d 5×5 convolution kernel). The unit is
/// `br mask, then, end(acc)`; `then` computes `acc' = acc + In[rr][cc] * K[k]`
/// (a contiguous In load, an invariant K load, a mul, an add) and jumps to
/// `end`, which forwards `acc'` to the next unit. When the mask is false the
/// accumulator passes through `end` unchanged. The accumulator chain is
/// carried by block parameters; the final value is stored by the latch.
struct MultiArmUnit {
    /// The block ending in `br mask, then, end(acc_in)`.
    body_br: BasicBlock,
    /// The block computing the multiply-accumulate (`jump end(acc_out)`).
    then: BasicBlock,
    /// The block forwarding the accumulator (`jump next`).
    end: BasicBlock,
    /// The branch's mask condition (a scalar bound test on `cc = iv + kc_off`
    /// conjoined with an optional row-bound invariant).
    mask_cond: Inst,
    /// The accumulator entering this unit (constant `acc_init` for unit 0,
    /// else the previous unit's `end` block parameter).
    acc_in: Inst,
    /// `load In[rr][cc]` (contiguous — vectorized to a `<4 x T>` load).
    load_in: Inst,
    /// `load K[k]` (loop-invariant — splatted at use).
    load_k: Inst,
    /// `mul(load_in, load_k)`.
    mul: Inst,
    /// `add(acc_in, mul)` — the masked accumulator update.
    add: Inst,
    /// The value jumped into `end` (`acc_out = add`).
    acc_out: Inst,
    /// The contiguous access column `cc = iv + kc_off`.
    cc: Inst,
    /// The constant offset of `cc` from the IV (`kc - 2` for the kernel).
    kc_off: i64,
    /// The row-bound invariant scalar (`rr_ok`, 0/1), None when the mask
    /// has no row term.
    rr_ok: Option<Inst>,
    /// The upper bound on `cc` (loop-invariant), e.g. `N_eff`.
    bound: Inst,
}

/// Milestone 2: a chain of masked multiply-accumulate units forming a
/// multi-arm if body. All units share one accumulator chain seeded from
/// `acc_init` (a constant 0) and ending in `acc_final` (the last unit's
/// `end` block parameter), which the latch stores and forwards as the
/// header accumulator slot's back-edge argument. The slot is re-typed to a
/// vector and horizontally reduced at the loop exit (like a B1 reduction).
struct MultiArmPlan {
    /// Units in program order.
    units: Vec<MultiArmUnit>,
    /// The first unit's incoming accumulator (a compile-time 0).
    acc_init: Inst,
    /// The final accumulator value (the last unit's `end` block parameter).
    acc_final: Inst,
    /// Header parameter slot carrying the accumulator to the exit.
    acc_slot: usize,
    /// The header accumulator parameter itself.
    acc: Inst,
    /// The entry edge's argument for `acc_slot` (re-typed to a vector in
    /// place; the body chain seeds from `acc_init`, so the initial lanes
    /// are irrelevant, but the slot must stay type-consistent).
    acc_entry_arg: Inst,
}

/// B1: a register accumulator recognized by M42 (`Reducible`) on a 3-param
/// loop `[iv, acc, t]`. The accumulator is carried as a vector and reduced
/// horizontally at the loop exit (`VectorReduce` / `addv`).
#[derive(Debug, Clone)]
struct ReductionPlan {
    /// The accumulator header parameter, re-typed to `<4 x T>` in place.
    acc: Inst,
    /// Position of `acc` in the header parameter list.
    acc_slot: usize,
    /// Lane-wise accumulation op (IntAdd -> Add, IntSub -> Sub).
    op: BinaryOp,
    /// The latch instruction producing the next accumulator value
    /// (`acc' = binary(op, acc, delta)`; re-typed in place, id preserved).
    acc_update: Inst,
    /// Per-iteration delta: a payload value (vectorized) or a loop-invariant
    /// scalar (splatted).
    delta: Inst,
    /// Entry-edge argument for the accumulator slot (initial value).
    acc_init: Inst,
    /// The exit block's accumulator parameter (receives the reduced value),
    /// or None when the exit takes no parameters.
    exit_acc_param: Option<Inst>,
}

/// How one exit-block parameter receives its value at the rewritten exit
/// edge, in exit-parameter order.
///
/// Mod-wrapped scalar reduction `acc' = (acc + E + C) % P`: the element
/// computation `E` is vectorized and horizontally reduced (`addv`) once per
/// vector round, while the accumulator stays scalar and is modded in range
/// every round:
/// `acc = (acc + addv(E_vec) + VF*C) % P`.
///
/// Lane-wise accumulation cannot be used here: `(a % P) + (b % P)` is not
/// `(a + b) % P`, and `addv` of the vector accumulator overflows for large
/// `P`. Keeping the accumulator scalar sidesteps both.
struct ModReductionPlan {
    /// The scalar accumulator header parameter (kept `<4 x T>`-free).
    acc: Inst,
    /// Position of `acc` in the header parameter list.
    acc_slot: usize,
    /// The element computation feeding the accumulation (vectorized per
    /// round and reduced with `addv`).
    value: Inst,
    /// Constant added to the accumulator per scalar iteration.
    const_sum: i64,
    /// The modulus (a compile-time positive constant).
    modulus: i32,
    /// The latch's accumulator update `rem(..., P)` (rewritten in place).
    acc_update: Inst,
    /// The accumulator update chain's add instructions (bottom-to-top),
    /// excluded from the payload and rebuilt flat in the mutation phase.
    add_chain: Vec<Inst>,
    /// Entry-edge argument for the accumulator slot (initial value).
    acc_init: Inst,
    /// The exit block's accumulator parameter (receives the scalar sum).
    exit_acc_param: Option<Inst>,
}

/// How one exit-block parameter receives its value at the rewritten exit
/// edge, in exit-parameter order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitArgSpec {
    /// Final accumulator of a B1 reduction (the exit's first parameter).
    Acc,
    /// Loop-invariant passthrough value (index into `passthrough_args`).
    Passthrough(usize),
    /// IV final value after the vectorized loop: the latch's `iv'` value
    /// (`j + 1` at the last scalar iteration, e.g. 01_mm1's kernel exit
    /// `while_end_18(%vid_4, %122, %vid_6)` where `%122 = add(j, 1)`) is
    /// replaced by the computed `i0 + VF*q` — the loop now runs `q` vector
    /// iterations, so the exit must not receive the latch's per-iteration
    /// update.
    IvFinal,
}

// ---------------------------------------------------------------------------
// Read-only analysis
// ---------------------------------------------------------------------------

/// Debug tracing for rejection reasons (M44_TRACE=1). Temporary diagnostic
/// tool for the v2 planning scan; not part of the pass contract.
fn trace(data: &FunctionData, looop: &Loop, reason: &str) {
    if std::env::var("M44_TRACE").is_ok() {
        let name = data.name();
        eprintln!(
            "[M44] func={name} header={:?} hdr_name={} reject={reason}",
            looop.header(),
            data.bb_data(looop.header()).name()
        );
    }
}

fn inst_kind_name(kind: &InstKind) -> &'static str {
    match kind {
        InstKind::Binary(b) => match b.op() {
            BinaryOp::Shl => "Shl",
            BinaryOp::Shr => "Shr",
            BinaryOp::Sar => "Sar",
            BinaryOp::And => "And",
            BinaryOp::Or => "Or",
            BinaryOp::Xor => "Xor",
            BinaryOp::Div => "Div",
            BinaryOp::Rem => "Rem",
            BinaryOp::Eq => "Eq",
            BinaryOp::NotEq => "NotEq",
            BinaryOp::Gt => "Gt",
            BinaryOp::Lt => "Lt",
            BinaryOp::Ge => "Ge",
            BinaryOp::Le => "Le",
            BinaryOp::Min => "Min",
            BinaryOp::Max => "Max",
            BinaryOp::Add => "Add",
            BinaryOp::Sub => "Sub",
            BinaryOp::Mul => "Mul",
        },
        InstKind::Select(_) => "Select",
        InstKind::Call(_) => "Call",
        InstKind::TailCall(_) => "TailCall",
        InstKind::Cast(_) => "Cast",
        InstKind::MemZero(_) => "MemZero",
        InstKind::Fma(_) => "Fma",
        InstKind::VectorSplat(_) => "VectorSplat",
        InstKind::VectorExtractElement(_) => "VectorExtractElement",
        InstKind::VectorInsertElement(_) => "VectorInsertElement",
        InstKind::VectorReduce(_) => "VectorReduce",
        InstKind::Load(_) => "Load",
        InstKind::Store(_) => "Store",
        InstKind::GetElemPtr(_) => "GetElemPtr",
        InstKind::Jump(_) => "Jump",
        InstKind::Branch(_) => "Branch",
        InstKind::Return(_) => "Return",
        InstKind::Undef => "Undef",
        InstKind::Aggregate(_) => "Aggregate",
        InstKind::Alloc => "Alloc",
        InstKind::GlobalAlloc(_) => "GlobalAlloc",
        InstKind::BlockArgRef(_) => "BlockArgRef",
        InstKind::Integer(_) => "Integer",
        InstKind::Float(_) => "Float",
        InstKind::ZeroInit => "ZeroInit",
    }
}

/// Decompose a mod-wrapped accumulator update `update = rem(X, P)` where `X`
/// is an add chain `acc + value + C` (constant adds folded into `const_sum`)
/// and `P` is a compile-time constant. Returns `(value, const_sum, modulus)`.
fn decompose_mod_reduction(
    arena: &ArenaContext<'_>,
    update: Inst,
    acc: Inst,
) -> Option<(Inst, i64, i32)> {
    fn walk(
        arena: &ArenaContext<'_>,
        node: Inst,
        acc: Inst,
        value: &mut Option<Inst>,
        const_sum: &mut i64,
    ) -> bool {
        if node == acc {
            return true;
        }
        match arena.inst_data(node).kind() {
            InstKind::Integer(v) => {
                *const_sum += i64::from(v.value());
                true
            }
            InstKind::Binary(b) if b.op() == BinaryOp::Add => {
                walk(arena, b.lhs(), acc, value, const_sum)
                    && walk(arena, b.rhs(), acc, value, const_sum)
            }
            _ => {
                if value.is_some() {
                    return false;
                }
                *value = Some(node);
                true
            }
        }
    }
    let InstKind::Binary(rem) = arena.inst_data(update).kind() else {
        return None;
    };
    if rem.op() != BinaryOp::Rem {
        return None;
    }
    let InstKind::Integer(modulus) = arena.inst_data(rem.rhs()).kind() else {
        return None;
    };
    let mut value = None;
    let mut const_sum = 0i64;
    if !walk(arena, rem.lhs(), acc, &mut value, &mut const_sum) {
        return None;
    }
    Some((value?, const_sum, modulus.value()))
}

/// Collect the add-chain instructions of a mod-wrapped accumulator update
/// (bottom-to-top: closest to `acc` first), for payload exclusion and the
/// flat rebuild in the mutation phase.
fn mod_reduction_add_chain(arena: &ArenaContext<'_>, update: Inst, acc: Inst) -> Vec<Inst> {
    let mut chain = Vec::new();
    fn walk(arena: &ArenaContext<'_>, node: Inst, acc: Inst, chain: &mut Vec<Inst>) {
        if node == acc {
            return;
        }
        let InstKind::Binary(b) = arena.inst_data(node).kind() else {
            return;
        };
        if b.op() != BinaryOp::Add {
            return;
        }
        walk(arena, b.lhs(), acc, chain);
        walk(arena, b.rhs(), acc, chain);
        chain.push(node);
    }
    let InstKind::Binary(rem) = arena.inst_data(update).kind() else {
        return chain;
    };
    walk(arena, rem.lhs(), acc, &mut chain);
    chain
}

/// Prove that the per-round scalar update `acc = (acc + addv(E) + VF*C) % P`
/// cannot wrap i32. The accumulator stays in `[-(P-1), P-1]` after every
/// round; `addv(E)` spans `[4*min(E), 4*max(E)]` from the element range.
fn mod_reduction_bounds_hold(
    arena: &ArenaContext<'_>,
    data: &FunctionData,
    cfg: &CFG,
    loops: &LoopAnalysis,
    acc: Inst,
    value: Inst,
    modulus: i32,
    const_sum: i64,
) -> bool {
    let p = i64::from(modulus);
    if p <= 1 {
        return false;
    }
    let ivs = BasicInductionVariableAnalysis::new(data, cfg, loops);
    let range_arena = ArenaContext {
        program: arena.program,
        curr_func: arena.curr_func,
    };
    let nonneg = return_summary::nonneg_preserving_functions(arena.program);
    let all_params = return_summary::always_nonneg_params(arena.program, &nonneg);
    let self_params = all_params
        .get(&arena.curr_func.expect("run_on always sets curr_func"))
        .cloned()
        .unwrap_or_default();
    let ranges = RangeAnalysis::new(&range_arena, cfg, loops, &ivs, &nonneg, &self_params);
    let (Some(vmin), Some(vmax)) = (ranges.range_of(value).min(), ranges.range_of(value).max())
    else {
        return false;
    };
    let acc_min = ranges.range_of(acc).min().map(i64::from).unwrap_or(-(p - 1));
    let acc_max = ranges.range_of(acc).max().map(i64::from).unwrap_or(p - 1);
    let upper = acc_max + 4 * i64::from(vmax) + 4 * const_sum;
    let lower = acc_min + 4 * i64::from(vmin) + 4 * const_sum;
    upper <= i64::from(i32::MAX) && lower >= i64::from(i32::MIN)
}

fn find_vectorizable(
    program: &Program,
    func: Function,
    effects: &EffectAnalysis,
) -> Option<VecPlan> {
    let arena = ArenaContext {
        program,
        curr_func: Some(func),
    };
    let data = program.func_data(func);
    let (cfg, dom, loops) = LoopAnalysis::new(data);
    let dependence = DependenceAnalysis::new(program, func, effects, VF as u32);
    for looop in loops.loops() {
        if let Some(plan) =
            analyze_loop(&arena, data, &cfg, &dom, &loops, &dependence, looop, func, effects)
        {
            return Some(plan);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Chain fusion: collapse 1-pred/1-succ loop-body chains
// ---------------------------------------------------------------------------

struct ParamSubst<'a> {
    subst: &'a FxHashMap<Inst, Inst>,
}

impl crate::ir::remap::EntityMapper for ParamSubst<'_> {
    type Error = ();
    fn map_inst(&mut self, inst: Inst) -> Result<Inst, ()> {
        Ok(self.subst.get(&inst).copied().unwrap_or(inst))
    }
    fn map_block(&mut self, block: BasicBlock) -> Result<BasicBlock, ()> {
        Ok(block)
    }
}

/// Fuse linear `1-pred/1-succ` blocks inside loop bodies so the body
/// collapses to `{header, latch}` — M42's shape contract. The inlined
/// `idx()` calls land in their own blocks (`forwarder -> index-latch ->
/// payload` chains); fusing them is a semantic no-op (block parameters are
/// resolved positionally from the edge args) that every later pass also
/// benefits from. Runs to a fixpoint (each fusion removes one block).
fn fuse_loop_body_chains(data: &mut ArenaContextMut<'_>) -> bool {
    let mut changed = false;
    loop {
        let (cfg, _dom, loops) = {
            let fdata = data.curr_func_data();
            LoopAnalysis::new(fdata)
        };
        let fdata = data.curr_func_data();
        // Innermost loops only: their body blocks are all in the same loop
        // (no nested headers), so a fusion can never cross a loop boundary.
        let innermost: Vec<BasicBlock> = loops
            .loops()
            .iter()
            .filter(|looop| {
                !loops
                    .loops()
                    .iter()
                    .any(|other| other.header() != looop.header() && looop.contains(other.header()))
            })
            .map(|looop| looop.header())
            .collect();
        // Every loop header (any nesting level) is off-limits as a fusion
        // source or target — collapsing a header into its latch would turn
        // the loop into a self-loop.
        let all_headers: FxHashSet<BasicBlock> =
            loops.loops().iter().map(|looop| looop.header()).collect();
        let mut candidate: Option<(BasicBlock, BasicBlock, LogicalEdge)> = None;
        'scan: for &header in &innermost {
            let Some(looop) = loops.loops().iter().find(|l| l.header() == header) else {
                continue;
            };
            for &b in looop.body() {
                if all_headers.contains(&b) {
                    continue;
                }
                let term = fdata.layout().basicblock(b).terminator();
                let InstKind::Jump(jump) = fdata.inst_data(term).kind() else {
                    continue; // Only jump-terminated blocks are pure links.
                };
                // Structural predecessors over the *whole layout* (not just
                // the CFG, which can omit unreachable blocks): a dangling
                // terminator in an unreachable block pointing at `b` would
                // otherwise be missed, and fusing `b` away would leave that
                // terminator targeting a removed block.
                let mut b_preds = Vec::new();
                for bb_l in fdata.layout().basicblocks() {
                    let src = bb_l.bb();
                    let Some(&t) = bb_l.insts().get_last() else {
                        continue;
                    };
                    match fdata.inst_data(t).kind() {
                        InstKind::Jump(j) => {
                            if j.target() == b {
                                b_preds.push(src);
                            }
                        }
                        InstKind::Branch(br) => {
                            if br.t_target() == b {
                                b_preds.push(src);
                            }
                            if br.f_target() == b {
                                b_preds.push(src);
                            }
                        }
                        _ => {}
                    }
                }
                if b_preds.len() != 1 {
                    continue;
                }
                let edge = incoming_edges(fdata, &cfg, b)[0];
                if edge.source() == b {
                    continue; // Self loop: not a link.
                }
                let s = jump.target();
                if s == b || all_headers.contains(&s) || !looop.contains(s) {
                    continue;
                }
                // Soundness: `s` must be reached *only* through `b` (see
                // `fuse_block_into_succ`), and `b`'s jump args must cover
                // `s`'s parameters. A candidate failing these checks is
                // rejected here — the fixpoint would otherwise re-select it
                // forever.
                let s_in_edges = incoming_edges(fdata, &cfg, s);
                if s_in_edges.len() != 1 || s_in_edges[0].source() != b {
                    continue;
                }
                if jump.args().len() != fdata.bb_data(s).params().len() {
                    continue;
                }
                candidate = Some((b, s, edge));
                break 'scan;
            }
        }
        let Some((b, s, edge)) = candidate else {
            break;
        };
        if fuse_block_into_succ(data, b, s, edge) {
            changed = true;
        } else {
            // The scan's checks should have rejected this candidate; refuse
            // to re-select it forever.
            break;
        }
    }
    changed
}

/// Merge `b` into `s` (its single successor): `b`'s non-terminator
/// instructions are prepended to `s`, `b`'s parameters are substituted by
/// the incoming edge's args, and the edge is retargeted to `s`. Sound only
/// when `s`'s *only* predecessor is `b`: then the merged block executes
/// exactly when `b` executed (any other predecessor of `s` would make `b`'s
/// instructions run on paths that never reached `b` — e.g. a conditional
/// arm's masked store becoming unconditional). `s`'s parameters are
/// substituted by `b`'s jump args (which may themselves reference `b`'s
/// parameters — substituted through the same map) and dropped, so the
/// retargeted edge passes no args.
fn fuse_block_into_succ(
    data: &mut ArenaContextMut<'_>,
    b: BasicBlock,
    s: BasicBlock,
    edge: LogicalEdge,
) -> bool {
    let (a_p, a_b, b_params, s_params, b_insts, s_insts) = {
        let fdata = data.curr_func_data();
        let a_p: Vec<Inst> = edge.args(fdata).to_vec();
        let term = fdata.layout().basicblock(b).terminator();
        let InstKind::Jump(jump) = fdata.inst_data(term).kind() else {
            return false;
        };
        let a_b: Vec<Inst> = jump.args().to_vec();
        let b_params: Vec<Inst> = fdata.bb_data(b).params().to_vec();
        let s_params: Vec<Inst> = fdata.bb_data(s).params().to_vec();
        if a_p.len() != b_params.len() {
            return false;
        }
        let (cfg, _, _) = LoopAnalysis::new(fdata);
        let s_in_edges = incoming_edges(fdata, &cfg, s);
        // `s` must be reached only through `b` (see the soundness note
        // above); its parameters must be fully covered by `b`'s jump args.
        if s_in_edges.len() != 1 || s_in_edges[0].source() != b || a_b.len() != s_params.len() {
            return false;
        }
        let b_insts: Vec<Inst> = fdata.layout().basicblock(b).insts().iter().copied().collect();
        let s_insts: Vec<Inst> = fdata.layout().basicblock(s).insts().iter().copied().collect();
        (a_p, a_b, b_params, s_params, b_insts, s_insts)
    };

    // Substitution for `b`'s parameters: the incoming edge's args.
    let mut subst = FxHashMap::<Inst, Inst>::default();
    for (&p, &v) in b_params.iter().zip(a_p.iter()) {
        if p != v {
            subst.insert(p, v);
        }
    }
    // `b`'s jump args may reference `b`'s parameters (passthrough values);
    // resolve them through the same map.
    let a_b_sub: Vec<Inst> = a_b
        .iter()
        .map(|&v| subst.get(&v).copied().unwrap_or(v))
        .collect();
    // Substitution for `s`'s parameters: `b`'s jump args.
    let mut s_subst = FxHashMap::<Inst, Inst>::default();
    for (&p, &v) in s_params.iter().zip(a_b_sub.iter()) {
        if p != v {
            s_subst.insert(p, v);
        }
    }
    // Remap every user of the removed parameters — `b`'s own instructions,
    // `s`'s instructions, and any later block that `b` dominated (its
    // params dominated them through the chain). `b`'s terminator is
    // dropped (the retarget rewrites the edge), so it is skipped.
    if !subst.is_empty() || !s_subst.is_empty() {
        let mut combined = subst.clone();
        combined.extend(s_subst.iter().map(|(&k, &v)| (k, v)));
        let mut users = FxHashSet::<Inst>::default();
        for p in b_params.iter().chain(s_params.iter()) {
            for &u in data.inst_data(*p).used_by() {
                users.insert(u);
            }
        }
        let b_term = data.layout().basicblock(b).terminator();
        for inst in users {
            if inst == b_term {
                continue;
            }
            let remapped = data
                .inst_data(inst)
                .clone()
                .remap_refs(&mut ParamSubst { subst: &combined })
                .expect("mapping block parameters cannot fail");
            data.replace_inst_with(inst).raw(remapped);
        }
    }
    // `s`'s parameters are substituted away (their block-arg indices are no
    // longer referenced), so drop them — the retargeted edge passes no args.
    let n_params = data.bb_data(s).params().len();
    for _ in 0..n_params {
        data.bb_data_mut(s).params_mut().pop();
    }
    // Move `b`'s non-terminator instructions to the head of `s`.
    let b_payload: Vec<Inst> = b_insts
        .iter()
        .copied()
        .filter(|&inst| inst != *data.layout().basicblock(b).insts().get_last().unwrap())
        .collect();
    let s_first = s_insts.first().copied();
    for &inst in &b_payload {
        data.layout_mut().remove_inst(b, inst);
    }
    if let Some(first) = s_first {
        // Forward order: each insertion lands immediately before `first`,
        // so the sequence i0, i1, ... stays in program order.
        for &inst in &b_payload {
            data.layout_mut().insert_inst_before(first, inst);
        }
    } else {
        for inst in b_payload {
            data.layout_mut().insert_before_terminator(s, inst);
        }
    }
    // Retarget the incoming edge to `s` (no args: `s` has no parameters).
    let mut rewriter = LogicalEdgeRewriter::new();
    rewriter.retarget(data.curr_func_data(), edge, s, Vec::new());
    rewriter.apply(data);
    data.curr_func_data_mut().remove_layout_basicblock(b);
    true
}

/// If `inst` is the IV plus a compile-time constant (coefficient +1),
/// return the constant. Recurses through `add`/`sub` chains, so conv2d's
/// `cc = sub(add(iv, k), 2)` decomposes to `-2`. Any non-unit coefficient
/// (`mul iv, k`), a `const - iv` term, or a non-IV leaf yields `None`.
fn iv_offset(arena: &ArenaContext<'_>, inst: Inst, iv: Inst) -> Option<i64> {
    if inst == iv {
        return Some(0);
    }
    match arena.inst_data(inst).kind() {
        InstKind::Integer(i) => Some(i.value() as i64),
        InstKind::Binary(b) => {
            let (lo, ro) = (iv_offset(arena, b.lhs(), iv), iv_offset(arena, b.rhs(), iv));
            match (b.op(), lo, ro) {
                (BinaryOp::Add, Some(l), Some(r)) => Some(l + r),
                (BinaryOp::Sub, Some(l), Some(r)) => Some(l - r),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Split a conjunction tree into its leaf instructions.
fn collect_and_leaves(arena: &ArenaContext<'_>, inst: Inst, out: &mut Vec<Inst>) {
    if let InstKind::Binary(b) = arena.inst_data(inst).kind() {
        if b.op() == BinaryOp::And {
            collect_and_leaves(arena, b.lhs(), out);
            collect_and_leaves(arena, b.rhs(), out);
            return;
        }
    }
    out.push(inst);
}

/// The mask's deconstructed bound test on the contiguous column `cc`.
struct MaskDecomp {
    /// `cc = iv + kc_off` (the access column).
    cc: Inst,
    /// Constant offset of `cc` from the IV.
    kc_off: i64,
    /// The upper bound (`cc < bound`), loop-invariant.
    bound: Inst,
    /// The optional row-bound invariant scalar (0/1), None when absent.
    rr_ok: Option<Inst>,
}

/// Match an upper-bound leaf `lt(cc, bound)` / `gt(bound, cc)` (le/ge with
/// swapped polarity are rejected: the canonical kernel uses strict tests).
fn match_bound_test(arena: &ArenaContext<'_>, leaf: Inst, iv: Inst) -> Option<(Inst, Inst)> {
    let InstKind::Binary(b) = arena.inst_data(leaf).kind() else {
        return None;
    };
    let (cc, bound) = match b.op() {
        BinaryOp::Lt if iv_offset(arena, b.lhs(), iv).is_some() => (b.lhs(), b.rhs()),
        BinaryOp::Gt if iv_offset(arena, b.rhs(), iv).is_some() => (b.rhs(), b.lhs()),
        _ => return None,
    };
    Some((cc, bound))
}

/// Match a lower-bound leaf on `cc`: `neq(and(rr_ok, ge(cc, 0)), 0)` — the
/// row-bound invariant conjoined with `cc >= 0` (or `gt(cc, -1)`), normalized
/// through `neq(_, 0)`. Returns `(cc, rr_ok)`; `rr_ok` is None when the lower
/// test is a bare `ge(cc, 0)`.
fn match_lower_test(
    arena: &ArenaContext<'_>,
    data: &FunctionData,
    leaf: Inst,
    iv: Inst,
    header: BasicBlock,
    latch: BasicBlock,
) -> Option<(Inst, Option<Inst>)> {
    // `neq(and(rr_ok, ge(cc, 0)), 0)`: peel the neq normalization.
    let inner = if let InstKind::Binary(b) = arena.inst_data(leaf).kind() {
        if b.op() == BinaryOp::NotEq {
            b.lhs()
        } else {
            leaf
        }
    } else {
        leaf
    };
    if let InstKind::Binary(b) = arena.inst_data(inner).kind() {
        if b.op() == BinaryOp::And {
            // One conjunct is the row invariant, the other `ge(cc, 0)`.
            let (l, r) = (b.lhs(), b.rhs());
            let ge_l = match_ge_zero(arena, l, iv);
            let ge_r = match_ge_zero(arena, r, iv);
            if let Some(cc) = ge_l {
                if is_loop_invariant(arena, r, header, latch) {
                    return Some((cc, Some(r)));
                }
            }
            if let Some(cc) = ge_r {
                if is_loop_invariant(arena, l, header, latch) {
                    return Some((cc, Some(l)));
                }
            }
            return None;
        }
    }
    // Bare `ge(cc, 0)` / `gt(cc, -1)`.
    match_ge_zero(arena, inner, iv).map(|cc| (cc, None))
}

/// `ge(cc, 0)` or `gt(cc, -1)` (the canonical non-negative column test).
fn match_ge_zero(arena: &ArenaContext<'_>, inst: Inst, iv: Inst) -> Option<Inst> {
    let InstKind::Binary(b) = arena.inst_data(inst).kind() else {
        return None;
    };
    let (cc, bound) = match b.op() {
        BinaryOp::Ge if iv_offset(arena, b.lhs(), iv).is_some() => (b.lhs(), b.rhs()),
        BinaryOp::Gt if iv_offset(arena, b.rhs(), iv).is_some() => (b.rhs(), b.lhs()),
        _ => return None,
    };
    if !matches!(arena.inst_data(bound).kind(), InstKind::Integer(i) if i.value() == 0) {
        return None;
    }
    Some(cc)
}

/// Deconstruct a unit's scalar mask into its bound tests on `cc`. The mask is
/// the conjunction of an upper test `cc < bound` and a lower test
/// `neq(and(rr_ok, ge(cc, 0)), 0)` (rr_ok optional). Both leaves must test the
/// same `cc` column and decompose to the same IV offset.
fn deconstruct_mask(
    arena: &ArenaContext<'_>,
    data: &FunctionData,
    mask_cond: Inst,
    iv: Inst,
    header: BasicBlock,
    latch: BasicBlock,
) -> Option<MaskDecomp> {
    let mut leaves = Vec::new();
    collect_and_leaves(arena, mask_cond, &mut leaves);
    if leaves.is_empty() {
        return None;
    }
    let mut cc: Option<Inst> = None;
    let mut kc_off: Option<i64> = None;
    let mut bound: Option<Inst> = None;
    let mut rr_ok: Option<Inst> = None;
    for &leaf in &leaves {
        if let Some((c, b)) = match_bound_test(arena, leaf, iv) {
            if cc.is_some() && cc != Some(c) {
                return None;
            }
            cc = Some(c);
            kc_off = Some(iv_offset(arena, c, iv)?);
            if bound.is_some() && bound != Some(b) {
                return None;
            }
            bound = Some(b);
            continue;
        }
        if let Some((c, rok)) = match_lower_test(arena, data, leaf, iv, header, latch) {
            if cc.is_some() && cc != Some(c) {
                return None;
            }
            cc = Some(c);
            kc_off = Some(iv_offset(arena, c, iv)?);
            if rok.is_some() {
                if rr_ok.is_some() && rr_ok != rok {
                    return None;
                }
                rr_ok = rok;
            }
            continue;
        }
        return None;
    }
    let cc = cc?;
    if !contains_iv(arena, cc, iv) {
        return None;
    }
    Some(MaskDecomp {
        cc,
        kc_off: kc_off?,
        bound: bound?,
        rr_ok,
    })
}

/// Find the IV term inside a contiguous access offset: the offset is
/// `invariant + (iv + kc_off)` (or directly `iv + const`). Returns the `cc`
/// instruction and its offset from the IV.
fn find_iv_term(
    arena: &ArenaContext<'_>,
    data: &FunctionData,
    offset: Inst,
    iv: Inst,
    header: BasicBlock,
    latch: BasicBlock,
) -> Option<(Inst, i64)> {
    if let Some(off) = iv_offset(arena, offset, iv) {
        return Some((offset, off));
    }
    if let InstKind::Binary(b) = arena.inst_data(offset).kind() {
        if b.op() == BinaryOp::Add {
            if let Some(off) = iv_offset(arena, b.lhs(), iv) {
                if is_loop_invariant(arena, b.rhs(), header, latch) {
                    return Some((b.lhs(), off));
                }
            }
            if let Some(off) = iv_offset(arena, b.rhs(), iv) {
                if is_loop_invariant(arena, b.lhs(), header, latch) {
                    return Some((b.rhs(), off));
                }
            }
        }
    }
    None
}

/// Whether `inst` references the IV (anywhere in an add/sub chain). A bare
/// constant or loop-invariant expression is *not* an IV term — `iv_offset`
/// alone cannot distinguish `iv + 0` from a constant `0` (both yield 0).
fn contains_iv(arena: &ArenaContext<'_>, inst: Inst, iv: Inst) -> bool {
    if inst == iv {
        return true;
    }
    match arena.inst_data(inst).kind() {
        InstKind::Binary(b) => contains_iv(arena, b.lhs(), iv) || contains_iv(arena, b.rhs(), iv),
        _ => false,
    }
}

/// A step-pointer header parameter: a pointer whose back-edge argument
/// advances it by one element (`getelemptr(param, 1)` — the compiler's
/// row-pointer form of `Out[r][c]` in the conv2d kernel). Excluded from
/// `effective` (it is neither the IV, an accumulator, nor a passthrough);
/// the mutation phase advances it by `VF` elements per vector round.
fn is_step_ptr(arena: &ArenaContext<'_>, back_arg: Inst, param: Inst) -> bool {
    matches!(
        arena.inst_data(back_arg).kind(),
        InstKind::GetElemPtr(g)
            if g.base() == param
                && g.offsets().len() == 1
                && matches!(arena.inst_data(g.offsets()[0]).kind(), InstKind::Integer(i) if i.value() == 1)
    )
}

/// A load whose address is a GEP whose *element* offset (the last one; the
/// outer offsets resolve the pointer/array nesting) has no IV term and is
/// loop-invariant (a `K[k]`-style element fetch — splatted at vector use).
fn is_invariant_load(
    arena: &ArenaContext<'_>,
    data: &FunctionData,
    load: Inst,
    iv: Inst,
    header: BasicBlock,
    latch: BasicBlock,
) -> bool {
    let InstKind::Load(l) = arena.inst_data(load).kind() else {
        return false;
    };
    let InstKind::GetElemPtr(gep) = arena.inst_data(l.src()).kind() else {
        return false;
    };
    let Some(&elem_off) = gep.offsets().last() else {
        return false;
    };
    !contains_iv(arena, elem_off, iv)
        && is_loop_invariant(arena, elem_off, header, latch)
}

/// A contiguous In load: a GEP whose element offset (the last one) carries
/// the IV term (`invariant + iv + kc_off`). Returns the `cc` instruction.
fn is_contiguous_iv_load(
    arena: &ArenaContext<'_>,
    data: &FunctionData,
    load: Inst,
    iv: Inst,
    header: BasicBlock,
    latch: BasicBlock,
) -> Option<Inst> {
    let InstKind::Load(l) = arena.inst_data(load).kind() else {
        return None;
    };
    let InstKind::GetElemPtr(gep) = arena.inst_data(l.src()).kind() else {
        return None;
    };
    let &elem_off = gep.offsets().last()?;
    if !contains_iv(arena, elem_off, iv) {
        return None;
    }
    let (cc, _) = find_iv_term(arena, data, elem_off, iv, header, latch)?;
    Some(cc)
}

/// Build the multi-arm masked-accumulate plan for a test-at-top loop whose
/// body is a chain of `br mask, then, end(acc)` units (the conv2d 5×5
/// kernel). Returns None when any structural check fails (conservative).
fn build_multi_arm_plan(
    arena: &ArenaContext<'_>,
    data: &FunctionData,
    looop: &Loop,
    header: BasicBlock,
    latch: BasicBlock,
) -> Option<MultiArmPlan> {
    // Rotated multi-arm deferred (the conv2d kernel is test-at-top). The
    // IV is header slot 0 (the pass-wide convention) and the test-at-top
    // header is exactly `[lt, br]` ending in a branch.
    let header_term = data.layout().basicblock(header).terminator();
    let InstKind::Branch(hb) = arena.inst_data(header_term).kind() else {
        return None;
    };
    let iv = data.bb_data(header).params()[0];
    let mut units = Vec::new();
    let mut cur = hb.t_target();
    let mut prev_end_param: Option<Inst> = None;
    while looop.contains(cur) && cur != latch {
        let body_br = cur;
        let term = data.layout().basicblock(body_br).terminator();
        let InstKind::Branch(branch) = arena.inst_data(term).kind() else {
            return None;
        };
        let then = branch.t_target();
        let end = branch.f_target();
        if !looop.contains(then) || !looop.contains(end) {
            return None;
        }
        // `then`: single-exit `jump end(...)` holding the multiply-accumulate.
        let then_term = data.layout().basicblock(then).terminator();
        let InstKind::Jump(tjump) = arena.inst_data(then_term).kind() else {
            return None;
        };
        if tjump.target() != end {
            return None;
        }
        // `end`: single-exit `jump next` (the next unit's body_br or the latch).
        let end_term = data.layout().basicblock(end).terminator();
        let InstKind::Jump(ejump) = arena.inst_data(end_term).kind() else {
            return None;
        };
        let next = ejump.target();
        if !looop.contains(next) && next != latch {
            return None;
        }
        // The accumulator enters this unit through the branch's false edge
        // (`br mask, then, end(acc_in)`): a constant for unit 0, the previous
        // unit's `end` parameter afterwards.
        if branch.f_args().len() != 1 {
            return None;
        }
        let acc_in = branch.f_args()[0];
        if units.is_empty() {
            if !matches!(arena.inst_data(acc_in).kind(), InstKind::Integer(_)) {
                return None;
            }
        } else if prev_end_param != Some(acc_in) {
            return None;
        }
        // `then` body: `acc_out = add(acc_in, mul(load_in, load_k)); jump end`.
        if tjump.args().len() != 1 {
            return None;
        }
        let add = tjump.args()[0];
        let InstKind::Binary(add_b) = arena.inst_data(add).kind() else {
            return None;
        };
        if add_b.op() != BinaryOp::Add {
            return None;
        }
        let (acc_op, mul_candidate) = if add_b.lhs() == acc_in {
            (add_b.lhs(), add_b.rhs())
        } else if add_b.rhs() == acc_in {
            (add_b.rhs(), add_b.lhs())
        } else {
            return None;
        };
        debug_assert_eq!(acc_op, acc_in);
        let InstKind::Binary(mul_b) = arena.inst_data(mul_candidate).kind() else {
            return None;
        };
        if mul_b.op() != BinaryOp::Mul {
            return None;
        }
        let (load_in, load_k) = match (
            is_contiguous_iv_load(arena, data, mul_b.lhs(), iv, header, latch),
            is_contiguous_iv_load(arena, data, mul_b.rhs(), iv, header, latch),
        ) {
            (Some(_), None) => (mul_b.lhs(), mul_b.rhs()),
            (None, Some(_)) => (mul_b.rhs(), mul_b.lhs()),
            _ => return None,
        };
        if !is_invariant_load(arena, data, load_k, iv, header, latch) {
            return None;
        }
        // The mask deconstructs to the same `cc` column the In load uses
        // (same instruction and same offset from the IV).
        let decomp = match deconstruct_mask(arena, data, branch.cond(), iv, header, latch) {
            Some(d) => d,
            None => {
                return None;
            }
        };
        let load_cc = match is_contiguous_iv_load(arena, data, load_in, iv, header, latch) {
            Some(c) => c,
            None => {
                return None;
            }
        };
        // The In offset is `invariant + (iv + kc_off)`, so the load and the
        // mask agree when their IV offsets match (not instruction identity —
        // an integer row offset may fold into the iv term).
        if iv_offset(arena, load_cc, iv)? != decomp.kc_off {
            return None;
        }
        units.push(MultiArmUnit {
            body_br,
            then,
            end,
            mask_cond: branch.cond(),
            acc_in,
            load_in,
            load_k,
            mul: mul_candidate,
            add,
            acc_out: add,
            cc: decomp.cc,
            kc_off: decomp.kc_off,
            rr_ok: decomp.rr_ok,
            bound: decomp.bound,
        });
        prev_end_param = Some(data.bb_data(end).params()[0]);
        cur = next;
    }
    if cur != latch || units.is_empty() {
        return None;
    }
    let last_end = units.last().unwrap().end;
    let acc_final = data.bb_data(last_end).params()[0];
    let acc_init = units[0].acc_in;
    // The accumulator header slot: the parameter whose back-edge argument
    // is the final accumulated value (the last unit's `end` parameter).
    // The header parameter is the compiler's carrier of the final value to
    // the loop exit (SSA dominance); the body chain seeds from `acc_init`.
    let acc_slot = {
        let latch_term = data.layout().basicblock(latch).terminator();
        let args: &[Inst] = match arena.inst_data(latch_term).kind() {
            InstKind::Jump(j) => j.args(),
            _ => return None, // multi-arm requires a test-at-top plain-jump latch
        };
        let slot = args.iter().position(|&a| a == acc_final)?;
        let params = data.bb_data(header).params();
        if slot >= params.len() {
            return None;
        }
        slot
    };
    let acc = data.bb_data(header).params()[acc_slot];
    Some(MultiArmPlan {
        units,
        acc_init,
        acc_final,
        acc_slot,
        acc,
        acc_entry_arg: acc_init,
    })
}

fn analyze_loop(
    arena: &ArenaContext<'_>,
    data: &FunctionData,
    cfg: &CFG,
    dom: &DominanceTree,
    loops: &LoopAnalysis,
    dependence: &DependenceAnalysis,
    looop: &Loop,
    func: Function,
    effects: &EffectAnalysis,
) -> Option<VecPlan> {
    // 1. Innermost only (a contained loop invalidates the shape below).
    if loops
        .loops()
        .iter()
        .any(|other| other.header() != looop.header() && looop.contains(other.header()))
    {
        trace(data, looop, "not_innermost");
        return None;
    }
    // Scalar tail loops produced by a previous vectorization of a
    // runtime-bound loop must never be vectorized again — the fixed-point
    // pipeline would otherwise keep vectorizing the new tail forever.
    // The `vec_tail` prefix is this pass's product namespace (like
    // `vec_epi_` / `vec_reduce`); it is a structural marker, not a
    // benchmark/input condition.
    if data.bb_data(looop.header()).name().starts_with("vec_tail") {
        trace(data, looop, "tail_loop_skip");
        return None;
    }

    // 2. M42 verdict: not Forbidden. (Reducible is accepted; the trip
    //    counter is always labeled a reduction, see the module docs.)
    let dep = dependence.for_loop(looop)?;
    if let Verdict::Reducible { accumulator, op } = &dep.verdict {
        trace(
            data,
            looop,
            &format!("verdict=Reducible{{acc={accumulator:?},op={op:?}}}"),
        );
    }
    if let Verdict::Forbidden { reason } = &dep.verdict {
        trace(data, looop, &format!("m42_forbidden:{reason:?}"));
        return None;
    }
    if dep.accesses.iter().any(|a| a.kind == AccessKind::ZeroFill) {
        trace(data, looop, "memzero_in_body");
        return None;
    }

    // 3. Shape: body == {header, latch}, single latch. Two accepted forms:
    //    (a) rotated: the header is a plain jump to the latch whose
    //        terminator branches back to the header;
    //    (b) test-at-top: the header holds exactly `lt iv, bound; br cond,
    //        latch, exit` (compare + branch, nothing else) and the latch is
    //        a plain jump back to the header. The bound must be a
    //        compile-time constant (no versioning).
    if looop.latches().len() != 1 {
        trace(data, looop, "shape_body_not_2_blocks");
        return None;
    }
    let header = looop.header();
    let latch = looop.latches()[0];
    if !looop.contains(latch) || latch == header {
        trace(data, looop, "shape_latch_is_header");
        return None;
    }
    // B1: accept a single-arm if body — a body block ends in
    // `br mask_cond, merge, arm` with the arm a single-exit block
    // (`jump merge`) on the false edge (conservative). The arm's stores
    // are mask-rewritten so the branch disappears.
    let mut arm_plan = None;
    let mut multi_arm = None;
    if looop.body().len() != 2 {
        // Milestone 2: a chain of masked multiply-accumulate units (the
        // conv2d 5×5 kernel). Tried before the single-arm scan because a
        // multi-arm body also satisfies the single-arm shape for some
        // interior unit (whose body_br then mismatches the header's
        // direct target, failing the test-at-top header check below).
        multi_arm = build_multi_arm_plan(arena, data, looop, header, latch);
        if multi_arm.is_none() {
        for &bb in looop.body() {
            if bb == header || bb == latch {
                continue;
            }
            let term = data.layout().basicblock(bb).terminator();
            let InstKind::Branch(branch) = arena.inst_data(term).kind() else {
                continue;
            };
            let arm = branch.f_target();
            let merge = branch.t_target();
            if !looop.contains(arm) || !looop.contains(merge) {
                continue;
            }
            let arm_term = data.layout().basicblock(arm).terminator();
            if let InstKind::Jump(jump) = arena.inst_data(arm_term).kind() {
                if jump.target() == merge {
                    arm_plan = Some(ArmPlan {
                        body_br: bb,
                        arm,
                        merge,
                        mask_cond: branch.cond(),
                        branch: term,
                        arm_on_true: false,
                    });
                    break;
                }
            }
            // Masked-kernel shape: `br cond, payload, latch` — the
            // payload sits on the *true* edge and jumps to the latch
            // (the branch's false edge). Same masked-store rewrite, but
            // the mask polarity flips (arm executes when cond != 0).
            let payload = branch.t_target();
            let target2 = branch.f_target();
            if looop.contains(payload) && looop.contains(target2) {
                let payload_term = data.layout().basicblock(payload).terminator();
                if let InstKind::Jump(jump2) = arena.inst_data(payload_term).kind() {
                    if jump2.target() == target2 {
                        arm_plan = Some(ArmPlan {
                            body_br: bb,
                            arm: payload,
                            merge: target2,
                            mask_cond: branch.cond(),
                            branch: term,
                            arm_on_true: true,
                        });
                        break;
                    }
                }
            }
        }
        if arm_plan.is_none() && multi_arm.is_none() {
            trace(data, looop, "shape_body_not_2_blocks");
            return None;
        }
        }
    }
    let header_insts = data.layout().basicblock(header).insts();
    if std::env::var("VECDBG_SHAPE").is_ok() {
        let kinds: Vec<String> = header_insts
            .iter()
            .map(|i| {
                let k = data.inst_data(*i).kind();
                match k {
                    InstKind::Binary(b) => format!("binary:{:?}", b.op()),
                    InstKind::Load(_) => "load".into(),
                    InstKind::Store(_) => "store".into(),
                    InstKind::GetElemPtr(_) => "gep".into(),
                    InstKind::Branch(_) => "br".into(),
                    InstKind::Jump(_) => "jump".into(),
                    InstKind::Integer(i) => format!("int({})", i.value()),
                    InstKind::BlockArgRef(_) => "blockarg".into(),
                    other => format!("{other:?}"),
                }
            })
            .collect();
        eprintln!("[SHAPE-HDR] header={header:?} insts={kinds:?}");
    }
    let mut iter = header_insts.iter().copied();
    let Some(first) = iter.next() else {
        return None;
    };
    let (test_at_top, bound_inst): (bool, Inst) = if let Some(second) = iter.next() {
        // Test-at-top: exactly [lt, branch] in the header.
        if iter.next().is_some() {
            trace(data, looop, "shape_header_multi_inst");
            return None;
        }
        let InstKind::Binary(binary) = arena.inst_data(first).kind() else {
            trace(data, looop, "shape_header_multi_inst");
            return None;
        };
        let InstKind::Branch(branch) = arena.inst_data(second).kind() else {
            trace(data, looop, "shape_header_multi_inst");
            return None;
        };
        if branch.cond() != first {
            trace(data, looop, "shape_header_multi_inst");
            return None;
        }
        if branch.t_target() != latch
            && !arm_plan
                .as_ref()
                .is_some_and(|a| branch.t_target() == a.body_br)
            && !multi_arm
                .as_ref()
                .is_some_and(|m| branch.t_target() == m.units[0].body_br)
        {
            trace(data, looop, "shape_header_multi_inst");
            return None;
        }
        if looop.contains(branch.f_target()) {
            trace(data, looop, "shape_header_multi_inst");
            return None;
        }
        if binary.op() != BinaryOp::Lt {
            trace(data, looop, "shape_header_multi_inst");
            return None;
        }
        (true, binary.rhs())
    } else {
        // Rotated: the single header instruction is a plain jump to the
        // latch (or to the body_br of a B1 single-arm body).
        let InstKind::Jump(jump) = arena.inst_data(first).kind() else {
            return None;
        };
        let header_target_ok = jump.target() == latch
            || arm_plan
                .as_ref()
                .is_some_and(|a| jump.target() == a.body_br);
        if !header_target_ok || !jump.args().is_empty() {
            trace(data, looop, "shape_header_jump_not_plain");
            return None;
        }
        // `bound_inst` is unused for the rotated form (the trip counter is
        // a header parameter).
        (false, first)
    };

    // 4. Header parameters: [iv, t] (elementwise), [iv, acc, t] (B1
    //    reduction), optionally extended with loop-invariant *passthrough*
    //    parameters — outer induction values the back edge forwards
    //    unchanged (the common nested-kernel shape: an inner loop reads the
    //    outer IVs). All parameters must be i32 scalars (the vector
    //    re-typing below happens in the mutation phase, and re-running this
    //    analysis on an already-vectorized loop is rejected here).
    //
    //    The latch terminator is inspected here to distinguish passthrough
    //    params (back-edge argument == the parameter itself) from the loop's
    //    own [iv, t] / [iv, acc, t] parameter set.
    let params = data.bb_data(header).params();
    let n_params = params.len();
    // Test-at-top headers carry the IV plus optional loop-invariant
    // passthrough parameters (outer induction values, e.g. the enclosing
    // loop's IV threaded through the inner loop); the single-parameter
    // gate is replaced by the effective (non-passthrough) count check
    // below. Rotated headers carry [iv, t] / [iv, acc, t] plus optional
    // passthroughs.
    // The exit test: rotated loops test on the latch terminator (a branch
    // back to the header); test-at-top loops test on the header terminator
    // (a branch to the latch / exit). The back-edge arguments (rotated:
    // the branch's t_args; test-at-top: the latch's plain-jump args) drive
    // the IV / counter identification below.
    let exit: BasicBlock;
    let latch_branch: Inst;
    let back_args: Vec<Inst>;
    let exit_args: Vec<Inst>;
    if test_at_top {
        let header_term = data.layout().basicblock(header).terminator();
        let InstKind::Branch(branch) = arena.inst_data(header_term).kind() else {
            return None;
        };
        latch_branch = header_term;
        exit = branch.f_target();
        exit_args = branch.f_args().to_vec();
        if looop.contains(exit) {
            trace(data, looop, "latch_exit_inside_or_args");
            return None;
        }
        let latch_term = data.layout().basicblock(latch).terminator();
        let InstKind::Jump(jump) = arena.inst_data(latch_term).kind() else {
            return None;
        };
        if jump.target() != header || jump.args().len() != n_params {
            trace(data, looop, "test_at_top_latch_jump_shape");
            return None;
        }
        back_args = jump.args().to_vec();
    } else {
        latch_branch = data.layout().basicblock(latch).terminator();
        let InstKind::Branch(branch) = arena.inst_data(latch_branch).kind() else {
            return None;
        };
        if branch.t_target() != header || !looop.contains(branch.t_target()) {
            trace(data, looop, "latch_target_not_header");
            return None;
        }
        exit = branch.f_target();
        exit_args = branch.f_args().to_vec();
        if looop.contains(exit) {
            trace(data, looop, "latch_exit_inside_or_args");
            return None;
        }
        back_args = branch.t_args().to_vec();
    }
    if back_args.len() != n_params {
        trace(data, looop, "back_args_not_2");
        return None;
    }
        if std::env::var("VECDBG_SHAPE").is_ok() {
            let latch_term = data.layout().basicblock(latch).terminator();
            let latch_kind = match arena.inst_data(latch_term).kind() {
                InstKind::Jump(_) => "jump".to_string(),
                InstKind::Branch(_) => "branch".to_string(),
                other => format!("{other:?}"),
            };
            let slot_desc: Vec<String> = params
                .iter()
                .zip(back_args.iter())
                .enumerate()
                .map(|(i, (p, ba))| {
                    let ba_kind = if ba == p {
                        "IDENTITY".to_string()
                    } else {
                        match arena.inst_data(*ba).kind() {
                            InstKind::Binary(b) => format!("binary:{:?}", b.op()),
                            InstKind::BlockArgRef(_) => "blockarg".to_string(),
                            InstKind::Integer(v) => format!("int({})", v.value()),
                            other => format!("{other:?}"),
                        }
                    };
                    format!("[{i}]p={p:?} ba={ba:?} {ba_kind}")
                })
                .collect();
            eprintln!(
                "[SHAPE-BACK] func={} header={:?} name={} test_at_top={test_at_top} latch={latch:?} latch_term={latch_kind} n_params={n_params} slots={}",
                data.name(),
                header,
                data.bb_data(header).name(),
                slot_desc.join(" | "),
            );
            if test_at_top {
                let bound_kind = match arena.inst_data(bound_inst).kind() {
                    InstKind::Integer(v) => format!("int({})", v.value()),
                    InstKind::BlockArgRef(_) => "blockarg".to_string(),
                    InstKind::Load(_) => "load".to_string(),
                    InstKind::Binary(b) => format!("binary:{:?}", b.op()),
                    InstKind::GlobalAlloc(_) => "global".to_string(),
                    other => format!("{other:?}"),
                };
                let bound_const = constant_i64(arena, data, bound_inst).map(|v| v.to_string());
                eprintln!(
                    "[SHAPE-BOUND] func={} header={:?} name={} bound={bound_inst:?} kind={bound_kind} const={bound_const:?}",
                    data.name(),
                    header,
                    data.bb_data(header).name(),
                );
            }
            let exit_kind = match arena.inst_data(data.layout().basicblock(exit).terminator()).kind() {
                InstKind::Jump(_) => "jump".to_string(),
                InstKind::Branch(_) => "branch".to_string(),
                other => format!("{other:?}"),
            };
            let exit_param_desc: Vec<String> = data
                .bb_data(exit)
                .params()
                .iter()
                .map(|p| format!("{p:?}"))
                .collect();
            let exit_arg_desc: Vec<String> = exit_args
                .iter()
                .map(|a| {
                    let k = match arena.inst_data(*a).kind() {
                        InstKind::Binary(b) => format!("binary:{:?}", b.op()),
                        InstKind::BlockArgRef(_) => "blockarg".to_string(),
                        InstKind::Integer(v) => format!("int({})", v.value()),
                        other => format!("{other:?}"),
                    };
                    format!("{a:?}({k})")
                })
                .collect();
            eprintln!(
                "[SHAPE-EXIT] func={} header={:?} name={} exit={exit:?} exit_term={exit_kind} exit_params={} exit_args={}",
                data.name(),
                header,
                data.bb_data(header).name(),
                exit_param_desc.join(","),
                exit_arg_desc.join(","),
            );
        }
    // Passthrough slots: the back-edge argument is the parameter itself.
    // The trip counter is the last parameter (its back-edge arg is the
    // `t' = sub(t, 1)` condition, never the parameter itself).
    let passthrough: Vec<usize> = (0..n_params)
        .filter(|&i| back_args[i] == params[i])
        .collect();
    // Step-pointer slots: pointer parameters advanced by one element per
    // iteration (the conv2d kernel's Out row pointer `%146`). Excluded from
    // `effective` like passthroughs; the mutation phase advances them by VF.
    let step_ptrs: FxHashSet<usize> = (0..n_params)
        .filter(|&i| is_step_ptr(arena, back_args[i], params[i]))
        .collect();
    let counter_slot = n_params - 1;
    let effective_initial: Vec<usize> = if test_at_top {
        // No counter parameter yet (materialized in the mutation phase), so
        // nothing is excluded beyond the passthroughs and loop-invariant
        // back-edge arguments (constants / loop-outside values forwarded
        // unchanged — crypto's md5/sha1 state loops carry many such
        // invariant pass-throughs alongside the real IV).
        (0..n_params)
            .filter(|&i| {
                !passthrough.contains(&i)
                    && !step_ptrs.contains(&i)
                    && !is_loop_invariant(arena, back_args[i], header, latch)
            })
            .collect()
    } else {
        (0..n_params)
            .filter(|&i| i != counter_slot && !passthrough.contains(&i) && !step_ptrs.contains(&i))
            .collect()
    };
    // Test-at-top loops carry exactly one real induction parameter (the
    // IV); a second one is accepted as the B1 accumulator ([iv, acc]) and
    // goes through the acc_info matching below — a non-Reducible
    // effective==2 shape is rejected there (`b1_not_reducible` etc.).
    // Rotated loops may also carry the B1 accumulator. More than two
    // effective parameters is rejected — except that the compiler may emit
    // *exit-only dead* parameters: a header parameter with no body use that
    // is merely forwarded to the latch (back edge) and exit (a stale carry
    // like the kernel's `%9`, whose back-edge argument `iv + 2` is
    // loop-variant and would otherwise be counted as effective). Such
    // parameters are dropped; the M42 accumulator (recognized below) is
    // never exit-only and is protected.
    let effective: Vec<usize> = if test_at_top && effective_initial.len() > 2 {
        let red_acc = match &dep.verdict {
            Verdict::Reducible { accumulator, .. } => Some(*accumulator),
            _ => None,
        };
        // The multi-arm accumulator carrier is a header parameter whose body
        // chain re-seeds from a constant, so it has no body use — protect it
        // alongside the M42 accumulator.
        let multi_acc = multi_arm.as_ref().map(|m| m.acc);
        let kept: Vec<usize> = effective_initial
            .iter()
            .copied()
            .filter(|&i| {
                let p = params[i];
                red_acc == Some(p)
                    || multi_acc == Some(p)
                    || data.inst_data(p).used_by().iter().any(|&u| {
                        data.layout()
                            .parent_bb(u)
                            .is_some_and(|bb| bb != header && bb != latch && bb != exit)
                    })
            })
            .collect();
        if kept.len() > 2 {
            trace(data, looop, "test_at_top_multi_param");
            return None;
        }
        kept
    } else {
        effective_initial
    };
    if test_at_top && effective.len() > 2 {
        trace(data, looop, "test_at_top_multi_param");
        return None;
    }
    // A mod-wrapped accumulation `acc' = (acc + E + C) % P` is invisible to
    // the M42 verdict (its outermost op is Rem, so the verdict labels the
    // *counter's* IntSub instead and the loop is rejected as
    // `b1_acc_is_passthrough_or_counter`). Scan the parameters directly and
    // treat the inner add as the accumulation; the per-round `% P` keeps the
    // accumulator in range and the element computation is vectorized +
    // `addv`'d once per round (see [`ModReductionPlan`]).
    let mut mod_plan: Option<(Inst, usize, BinaryOp)> = None;
    let mut mod_params: Option<(Inst, i64, i32, Vec<Inst>)> = None;
    if effective.len() <= 2 {
        'mod_scan: for (slot, &param) in params.iter().enumerate() {
            if passthrough.contains(&slot) || (!test_at_top && slot == counter_slot) {
                continue;
            }
            let update = back_args[slot];
            if let Some((value, const_sum, modulus)) =
                decompose_mod_reduction(arena, update, param)
            {
                for &user in arena.inst_data(param).used_by() {
                    let Some(user_bb) = data.layout().parent_bb(user) else {
                        continue;
                    };
                    if user != update
                        && user_bb != latch
                        && !(test_at_top && dom.dominates(exit, user_bb))
                    {
                        trace(data, looop, "mod_acc_used_outside_latch");
                        continue 'mod_scan;
                    }
                }
                if !mod_reduction_bounds_hold(
                    arena,
                    data,
                    cfg,
                    loops,
                    param,
                    value,
                    modulus,
                    const_sum,
                ) {
                    trace(data, looop, "mod_bounds_overflow");
                    continue 'mod_scan;
                }
                mod_plan = Some((param, slot, BinaryOp::Add));
                mod_params = Some((
                    value,
                    const_sum,
                    modulus,
                    mod_reduction_add_chain(arena, update, param),
                ));
                break;
            }
        }
    }
    let acc_info: Option<(Inst, usize, BinaryOp)> = if let Some(multi) = &multi_arm {
        // Milestone 2: the accumulator chain lives in the body; M42 sees the
        // loop as `Vectorizable` (the latch back argument is the final block
        // parameter, not a binary update), so the accumulator is recognized
        // from the chain's header carrier slot instead.
        Some((multi.acc, multi.acc_slot, BinaryOp::Add))
    } else if let Some(plan) = mod_plan {
        Some(plan)
    } else {
        match effective.len() {
            1 => None,
            2 => {
            let Verdict::Reducible { accumulator, op } = &dep.verdict else {
                trace(data, looop, "b1_not_reducible");
                return None;
            };
            let acc_slot = match params.iter().position(|&p| p == *accumulator) {
                Some(slot) => slot,
                None => {
                    trace(data, looop, "b1_acc_not_param");
                    return None;
                }
            };
            if passthrough.contains(&acc_slot)
                || (!test_at_top && acc_slot == counter_slot)
            {
                // The counter slot only exists for rotated loops; a
                // test-at-top accumulator may legally be the last header
                // parameter (e.g. 01_mm1's kernel `[outer, iv, acc]`).
                trace(data, looop, "b1_acc_is_passthrough_or_counter");
                return None;
            }
            let bop = match op {
                ReductionOp::IntAdd => BinaryOp::Add,
                ReductionOp::IntSub => BinaryOp::Sub,
                _ => {
                    // IntMul/IntMin/IntMax accumulate differently; B1 keeps
                    // them scalar.
                    trace(data, looop, "b1_op_not_add_sub");
                    return None;
                }
            };
            Some((*accumulator, acc_slot, bop))
        }
        _ => {
            trace(data, looop, "params_not_2");
            return None;
        }
    }
    };
    for &p in params.iter() {
        if !arena.inst_data(p).ty().is_i32() {
            trace(data, looop, "params_not_i32");
            return None;
        }
    }

    // 5. Latch terminator details. Rotated: `br t', header([...']), exit` —
    //    the last header parameter is the trip counter (`t' = sub(t, 1)`
    //    doubles as the condition), the remaining non-accumulator,
    //    non-passthrough parameter is the index IV (`iv' = add(iv, 1)`).
    //    Test-at-top: the single parameter is the IV (its back-edge arg is
    //    `iv' = add(iv, 1)`); a counter is materialized in the mutation
    //    phase, where the bound test is replaced by the counter test. The
    //    exit takes the passthrough values, the B1 final accumulator (as
    //    its first parameter), and — with A4 — the IV's final value (the
    //    latch's `iv'` value, e.g. 01_mm1's `while_end_18(%vid_4, %122,
    //    %vid_6)`); the exact per-parameter classification happens in step
    //    6b once the IV update is identified.
    let exit_params = data.bb_data(exit).params();
    if exit_args.len() != exit_params.len() {
        trace(data, looop, "exit_has_params");
        return None;
    }
    let (iv, iv_next, iv_slot, counter, t_next): (Inst, Inst, usize, Inst, Inst) =
        if test_at_top {
            // The effective (non-passthrough) parameters are `[iv]` or
            // `[iv, acc]` (B1). With an accumulator, the IV is the
            // non-acc effective slot; otherwise it is effective[0].
            // `iv' = add(iv, 1)` is its plain-jump argument in the latch.
            // The remaining parameters are loop-invariant passthroughs.
            let iv_slot = match acc_info {
                Some((acc_inst, acc_slot, _)) => {
                    let slots: Vec<usize> = effective
                        .iter()
                        .copied()
                        .filter(|&i| i != acc_slot)
                        .collect();
                    // A fixed-point sibling pass (PSR) may have rewritten the
                    // loop mid-iteration into a shape this analyzer does not
                    // recognize — reject conservatively instead of indexing
                    // out of bounds.
                    if slots.len() != 1 {
                        trace(data, looop, "multi_iv_slot");
                        return None;
                    }
                    slots[0]
                }
                None => {
                    if effective.len() != 1 {
                        trace(data, looop, "multi_iv_slot");
                        return None;
                    }
                    effective[0]
                }
            };
            let iv = params[iv_slot];
            let iv_next = back_args[iv_slot];
            // Placeholders; unused for test-at-top (no counter yet).
            (iv, iv_next, iv_slot, iv, iv_next)
        } else {
            let t_next = match arena.inst_data(latch_branch).kind() {
                InstKind::Branch(b) => b.cond(),
                _ => unreachable!(),
            };
            if back_args[counter_slot] != t_next {
                trace(data, looop, "counter_not_cond");
                return None;
            }
            let counter = params[counter_slot];
            let (iv, iv_next, iv_slot) = match acc_info {
                Some((acc, acc_slot, _)) => {
                    let slots: Vec<usize> = (0..n_params)
                        .filter(|&i| i != acc_slot && i != counter_slot && !passthrough.contains(&i))
                        .collect();
                    debug_assert_eq!(slots.len(), 1);
                    (params[slots[0]], back_args[slots[0]], slots[0])
                }
                None => {
                    debug_assert_eq!(effective.len(), 1);
                    let slot = effective[0];
                    (params[slot], back_args[slot], slot)
                }
            };
            (iv, iv_next, iv_slot, counter, t_next)
        };
    if !is_add_one(arena, iv_next, iv) {
        trace(data, looop, "non_unit_step");
        return None;
    }
    if !test_at_top && !is_sub_one(arena, t_next, counter) {
        trace(data, looop, "non_unit_step");
        return None;
    }
    // For a reduction, the accumulator's back-edge arg must be its update
    // `acc' = binary(op, acc, delta)`. The exit's first argument carries the
    // same final value — verified by the exit classification in step 6b.
    let acc_update: Option<(Inst, Inst)> = match acc_info {
        Some((acc, acc_slot, bop)) => {
            let update = back_args[acc_slot];
            // M42's `match_acc_update_op` cannot distinguish an induction
            // step (`j' = add(j, 1)`) from a reduction update — the first
            // matching parameter wins, which is the IV when it precedes the
            // accumulator in the parameter list (e.g. `sum += c[i][j]`
            // loops [i, j, sum, t]). A candidate whose update is exactly
            // the latch's `iv_next` is the IV, not an accumulator.
            if update == iv_next {
                trace(data, looop, "b1_acc_is_iv");
                return None;
            }
            let (update, delta) = if let Some((value, _, _, _)) = mod_params.as_ref() {
                // Mod-wrapped: `update = rem(acc + value + C, P)`; the delta
                // is the element computation (vectorized + addv per round).
                (update, *value)
            } else if let Some(multi) = &multi_arm {
                // Milestone 2: the accumulator chain lives in the body — the
                // units' `end` block parameters — and the latch back argument
                // is the final accumulated value, not a binary update. The
                // mutation phase re-uses the chain directly (no delta).
                let _ = multi;
                (update, update)
            } else if let Some(arm_plan) = &arm_plan {
                // B1 masked register reduction is validated for *rotated*
                // loops. A test-at-top loop whose latch back-argument is a
                // phi (e.g. a fused `clusters = select(cond, clusters+1,
                // clusters)` single-arm body) has an arm jump carrying the
                // binary update but a latch arg that is the select phi — the
                // apply side (merge re-typing, exit edge) is only wired for
                // the rotated shape. Keep it conservative until the
                // test-at-top arm reduction path is completed.
                if test_at_top {
                    trace(data, looop, "b1_arm_test_at_top_unsupported");
                    return None;
                }
                // B1 masked reduction: the accumulator update lives in the
                // arm (`delta = binary(op, acc, rhs)`); the arm's jump
                // forwards `delta` to the merge, whose back edge feeds the
                // header's accumulator slot. The merge phi is the masked
                // value `select(cond, acc, delta)` — the apply phase rewrites
                // the arm's `delta` in place and the merge carries the
                // vector accumulator.
                let arm_term = data.layout().basicblock(arm_plan.arm).terminator();
                let InstKind::Jump(arm_jump) = arena.inst_data(arm_term).kind() else {
                    trace(data, looop, "b1_arm_not_jump");
                    return None;
                };
                let Some(arm_arg) = arm_jump.args().first().copied() else {
                    trace(data, looop, "b1_arm_no_acc_arg");
                    return None;
                };
                let InstKind::Binary(binary) = arena.inst_data(arm_arg).kind() else {
                    trace(data, looop, "b1_arm_update_not_binary");
                    return None;
                };
                if binary.op() != bop || binary.lhs() != acc {
                    trace(data, looop, "b1_arm_update_shape");
                    return None;
                }
                (arm_arg, binary.rhs())
            } else {
                let InstKind::Binary(binary) = arena.inst_data(update).kind() else {
                    trace(data, looop, "b1_acc_update_not_binary");
                    return None;
                };
                if binary.op() != bop || binary.lhs() != acc {
                    trace(data, looop, "b1_acc_update_shape");
                    return None;
                }
                (update, binary.rhs())
            };
            // The accumulator must be consumed only by its update chain,
            // the latch, and — for test-at-top loops — the exit region
            // (the exit block and the blocks it dominates, which read the
            // header parameter directly via SSA dominance; the apply
            // phase rewrites those reads to the reduced scalar). Rotated
            // loops carry `acc'` through the latch branch's f_args, so
            // any other user stays scalar. Orphaned instructions
            // (removed from the layout by a transform that failed to
            // detach `used_by`) are unreachable and observe nothing, so
            // they are not an escape.
            for &user in arena.inst_data(acc).used_by() {
                let Some(user_bb) = data.layout().parent_bb(user) else {
                    continue;
                };
                // B1 single-arm bodies: the body_br's branch forwards the
                // untouched accumulator on its true edge (the mask's "no
                // update" path), so the body_br block is a legal consumer.
                let in_body_br = arm_plan
                    .as_ref()
                    .is_some_and(|a| user_bb == a.body_br);
                if user != update
                    && user_bb != latch
                    && !in_body_br
                    && !(test_at_top && dom.dominates(exit, user_bb))
                {
                    trace(data, looop, "b1_acc_used_outside_latch");
                    return None;
                }
            }
            Some((update, delta))
        }
        None => None,
    };

    // 6. Entry edge: exactly one outside predecessor, a guard branch or a
    //    plain jump into the header with [i0, t0].
    let preds: Vec<BasicBlock> = cfg.predecessors_of(header).to_vec();
    if preds.len() != 2 {
        trace(data, looop, "entry_preds_not_2");
        return None;
    }
    let preheader = preds.iter().copied().find(|&b| b != latch)?;
    let entry_edge = data.layout().basicblock(preheader).terminator();
    let entry_args: Vec<Inst> = match arena.inst_data(entry_edge).kind() {
        InstKind::Branch(b) if b.t_target() == header => b.t_args().to_vec(),
        InstKind::Jump(j) if j.target() == header => j.args().to_vec(),
        _ => {
            trace(data, looop, "entry_edge_shape");
            return None;
        }
    };
    if entry_args.len() != n_params {
        trace(data, looop, "entry_args_not_2");
        return None;
    }
    // Passthrough entry arguments: loop-invariant values forwarded to the
    // exit / epilogue chain after vectorization (their params stay i32).
    let passthrough_args: Vec<Inst> = passthrough
        .iter()
        .map(|&slot| entry_args[slot])
        .collect();

    // 6b. Exit parameter classification: each exit parameter is fed one of
    //     three value classes at the rewritten exit edge — the final
    //     accumulator (B1 reduction, parameter 0), a loop-invariant
    //     passthrough value (the header parameter itself, or its entry
    //     value — both are the same loop-invariant quantity), or the IV's
    //     final value. The IV-final parameter is the back-edge argument
    //     equal to the IV update `iv' = add(iv, 1)`: e.g. 01_mm1's kernel
    //     exit `while_end_18(%vid_4, %122, %vid_6)` receives `%122 =
    //     add(j, 1)` as the final `j`. After vectorization the loop runs
    //     `q` iterations of width VF, so the exit must receive the computed
    //     `i0 + VF*q` (plus the peeled remainder through the epilogue
    //     chain) instead of the latch's per-iteration `iv + 1`; the rewrite
    //     happens in `apply_vectorize` (2c). Anything else is a shape the
    //     vectorizer does not recognize.
    let mut exit_specs = Vec::with_capacity(exit_params.len());
    for &arg in exit_args.iter() {
        // Value-based classification (the accumulator is not assumed to be
        // exit parameter 0 — e.g. a min-reduction passes [i, min]).
        let spec = if arg == iv_next {
            ExitArgSpec::IvFinal
        } else if let Some(idx) = passthrough.iter().position(|&slot| {
            arg == params[slot] || arg == entry_args[slot]
        }) {
            // `idx` is the index into `passthrough` / `passthrough_args`
            // (both are ordered identically), not the header slot.
            ExitArgSpec::Passthrough(idx)
        } else if let Some((acc, acc_slot, _)) = acc_info.as_ref() {
            // Rotated: the exit receives the accumulator's update value
            // (`acc'`, computed in the latch). Test-at-top: the exit may
            // carry the header parameter itself — the f-edge args are
            // evaluated at the header, where the parameter's current
            // value is the final accumulation. Both resolve to
            // ExitArgSpec::Acc (fed the reduced scalar by the mutation
            // phase).
            let ok = acc_update
                .as_ref()
                .is_some_and(|(update, _)| arg == *update)
                || (test_at_top && arg == params[*acc_slot])
                // B1 masked reduction: the exit leaves through the merge
                // (latch) block, whose accumulator phi (fed `acc` on the
                // body_br true edge and the masked delta on the arm edge) is
                // the final value. `arg` is that merge parameter.
                || (arm_plan.is_some() && arg == back_args[*acc_slot]);
            if !ok {
                trace(data, looop, "exit_arg_not_acc");
                return None;
            }
            ExitArgSpec::Acc
        } else {
            trace(data, looop, "exit_has_params");
            return None;
        };
        exit_specs.push(spec);
    }

    // 7. Trip: the initial index must be a compile-time constant. Rotated
    //    loops carry the trip as a constant counter entry; test-at-top
    //    loops derive it from the bound — a compile-time constant
    //    (`trip = bound - i0`, peeled as constant epilogue blocks) or a
    //    runtime value (the vector counter enters at `trip & -4` and a
    //    scalar tail loop covers the remainder, so no versioning is
    //    needed). When a const trip leaves a remainder the exit must not
    //    read the IV (its final value after `q` vector iterations differs
    //    from the scalar `bound`); with R == 0 the values coincide. A
    //    runtime trip rewrites the exit's IV reads to the final `bound`
    //    instead.
    let Some(entry_i0) = constant_i64(arena, data, entry_args[iv_slot]) else {
        trace(data, looop, "entry_i0_not_const");
        return None;
    };
    let (trip, runtime_trip): (i64, bool) = if test_at_top {
        match constant_i64(arena, data, bound_inst) {
            Some(bound) => (bound - entry_i0, false),
            // Runtime bound: the exact trip is unknown; the vector loop
            // counter enters at `trip & -4` and the `trip & 3` remainder
            // runs in a scalar tail loop.
            None => (0, true),
        }
    } else {
        match constant_i64(arena, data, entry_args[counter_slot]) {
            Some(t) => (t, false),
            // Rotated runtime trip: the counter enters at a runtime value
            // (`rotate_loops` computes `t0 = sub(bound, i0)` in the
            // preheader). The vector counter enters at `t0 & -4` and a
            // scalar tail loop covers the `t0 & 3` remainder — the same
            // scheme as test-at-top. This is exact because the scalar
            // loop runs exactly `t0` iterations: the counter steps down
            // by 1 and the latch tests the stepped value for non-zero
            // (both checked above).
            None => (0, true),
        }
    };
    if runtime_trip {
        // The counter computation (`trip = sub(bound, i0)`) is inserted
        // in the preheader and the tail reuses the bound, so the bound
        // must be a loop-invariant value defined outside the loop (not a
        // header/latch parameter — a parameter does not dominate the
        // preheader). Rotated loops carry the trip as the counter's entry
        // value: it must likewise be loop-invariant (its def dominates
        // the preheader), and the scalar tail's upper bound is rebuilt as
        // `i0 + counter_entry` in the mutation phase.
        if test_at_top {
            let bound_bb = data.layout().parent_bb(bound_inst);
            if data.bb_data(header).params().contains(&bound_inst)
                || data.bb_data(latch).params().contains(&bound_inst)
                || bound_bb == Some(header)
                || bound_bb == Some(latch)
                || !is_loop_invariant(arena, bound_inst, header, latch)
            {
                trace(data, looop, "runtime_bound_loop_variant");
                return None;
            }
        } else {
            let counter_entry = entry_args[counter_slot];
            let entry_bb = data.layout().parent_bb(counter_entry);
            if data.bb_data(header).params().contains(&counter_entry)
                || data.bb_data(latch).params().contains(&counter_entry)
                || entry_bb == Some(header)
                || entry_bb == Some(latch)
                || !is_loop_invariant(arena, counter_entry, header, latch)
            {
                trace(data, looop, "runtime_counter_loop_variant");
                return None;
            }
        }
    } else if trip < VF {
        trace(data, looop, "trip_below_vf");
        return None;
    }
    // The mod-wrapped scalar reduction is supported for rotated loops
    // (test-at-top would need the exit region's direct accumulator reads
    // rewritten; the accumulator stays scalar, so that path is deferred) and
    // for trips that are an exact multiple of VF (no scalar epilogue) or
    // runtime trips (the scalar tail covers the remainder).
    if mod_plan.is_some() && (test_at_top || (!runtime_trip && trip % VF != 0)) {
        trace(data, looop, "mod_shape_not_supported");
        return None;
    }
    // A test-at-top reduction rewrites the exit region's direct reads of
    // the header accumulator (and, with a runtime trip, the IV) to the
    // reduced scalar. That is only valid when the exit has no other
    // predecessors whose argument values we cannot supply: the preheader
    // guard edge (feeds the seed when the loop never runs) and the
    // header's own exit edge (rewritten by the mutation phase).
    if test_at_top && (acc_info.is_some() || runtime_trip) {
        for &p in cfg.predecessors_of(exit) {
            if p != preheader && p != header {
                trace(data, looop, "exit_extra_pred");
                return None;
            }
        }
    }
    // The exit region's post-exit reads (blocks dominated by the exit)
    // are rewritten to the reduce block's scalar sum, which only
    // dominates them when every path goes through the reduce block. A
    // preheader guard edge (trip == 0) bypasses it: exit-block users
    // still resolve through the freshly added exit parameter (fed the
    // seed), but post-exit users would read an undefined sum — reject
    // that combination.
    if test_at_top && acc_info.is_some() {
        let guard_to_exit = matches!(
            arena.inst_data(entry_edge).kind(),
            InstKind::Branch(b) if b.f_target() == exit
        );
        if guard_to_exit {
            let (acc, _, _) = acc_info.expect("acc_info set");
            for &user in arena.inst_data(acc).used_by() {
                let Some(user_bb) = data.layout().parent_bb(user) else {
                    continue;
                };
                if user_bb != header && user_bb != latch && user_bb != exit {
                    trace(data, looop, "exit_region_guard");
                    return None;
                }
            }
        }
    }
    if test_at_top && (runtime_trip || trip % VF != 0) {
        // The vectorized loop runs `q` iterations; the scalar epilogue
        // substitutes constant IVs, so the header parameter's final value
        // is `i0 + 4q`, not `bound`. An exit that reads the IV is only
        // correct when R == 0 (then `i0 + 4q == bound`). With a runtime
        // trip the exit's IV reads are rewritten to the final `bound`
        // (apply phase), so they must be confined to the exit block.
        for &user in arena.inst_data(iv).used_by() {
            let bb = data.layout().parent_bb(user);
            if bb != Some(header)
                && bb != Some(latch)
                && !(runtime_trip && bb == Some(exit))
                && !arm_plan
                    .as_ref()
                    .is_some_and(|a| bb == Some(a.body_br) || bb == Some(a.arm))
                && !multi_arm.as_ref().is_some_and(|m| {
                    // The multi-arm kernel's mask bounds and index math are
                    // computed inside the units' body_br / then blocks.
                    m.units.iter().any(|u| {
                        bb == Some(u.body_br) || bb == Some(u.then) || bb == Some(u.end)
                    })
                })
            {
                trace(data, looop, "test_at_top_exit_reads_iv");
                return None;
            }
        }
    }

    // 8. Payload classification. Latch machinery (terminator, condition,
    //    back-edge args) is excluded; everything else must be a GEP, a
    //    Load/Store, a whitelisted binary op (see
    //    `is_vectorizable_binary_op`), or a constant.
    let latch_insts: Vec<Inst> = data
        .layout()
        .basicblock(latch)
        .insts()
        .iter()
        .copied()
        .collect();
    let machinery = {
        let mut set = FxHashMap::<Inst, ()>::default();
        set.insert(latch_branch, ());
        set.insert(t_next, ());
        set.insert(iv_next, ());
        if test_at_top {
            // The latch ends in a plain jump back to the header; exclude it
            // from the payload alongside the IV update it carries.
            set.insert(data.layout().basicblock(latch).terminator(), ());
        }
        if let Some((update, _)) = acc_update {
            set.insert(update, ());
            // The mod-wrapped update's add chain is rebuilt flat in the
            // mutation phase; excluding it keeps the accumulator operand out
            // of the payload's vector-operand machinery.
            if let Some((_, _, _, chain)) = mod_params.as_ref() {
                for &add in chain {
                    set.insert(add, ());
                }
            }
        }
        set
    };
    let payload: Vec<Inst> = if let Some(multi) = &multi_arm {
        // Milestone 2: the payload lives in every unit's `then` block —
        // the contiguous In load, the invariant K load, the mul, and the
        // masked accumulator add. The `end` blocks carry only block
        // parameters (accumulator chain); the latch holds machinery.
        let mut insts = Vec::new();
        for unit in &multi.units {
            for &inst in data.layout().basicblock(unit.then).insts() {
                if inst != data.layout().basicblock(unit.then).terminator() {
                    insts.push(inst);
                }
            }
        }
        insts
    } else if let Some(arm_plan) = &arm_plan {
        // B1 single-arm body: the payload lives in body_br (before the
        // branch) and in the arm (before the jump). The latch only holds
        // machinery.
        let mut insts = Vec::new();
        for &inst in data.layout().basicblock(arm_plan.body_br).insts() {
            if inst != arm_plan.branch {
                insts.push(inst);
            }
        }
        for &inst in data.layout().basicblock(arm_plan.arm).insts() {
            if inst != data.layout().basicblock(arm_plan.arm).terminator() {
                insts.push(inst);
            }
        }
        insts
    } else {
        latch_insts
            .iter()
            .copied()
            .filter(|inst| !machinery.contains_key(inst))
            .collect()
    };

    let mut classes = FxHashMap::<Inst, Class>::default();
    let mut elem_ty: Option<Type> = None;
    let mut vectorized_any = false;

    // Index math: payload instructions that (transitively) feed a GEP
    // offset. The offset of a contiguous access is an affine expression of
    // the IV (`add(mul(r, N), iv)` after the fused `idx()` inline); these
    // instructions must stay scalar — the vector body re-derives them with
    // the vector counter substituted for the IV — and are legal GEP offset
    // sources even though they are loop-variant.
    let mut index_math = FxHashSet::<Inst>::default();
    for &inst in &payload {
        let InstKind::GetElemPtr(gep) = arena.inst_data(inst).kind() else {
            continue;
        };
        for &offset in gep.offsets() {
            if offset != iv && !is_loop_invariant(arena, offset, header, latch) {
                collect_index_math(offset, &payload, arena, &mut index_math);
            }
        }
    }

    // First pass: loads and stores (their classes feed the binary checks).
    for &inst in &payload {
        let kind = arena.inst_data(inst).kind();
        match kind {
            InstKind::GetElemPtr(gep) => {
                // GEP offsets may reference only the index IV, loop
                // invariants, or payload index math (a scalar affine
                // expression of the IV that the vector body re-derives);
                // the base must be loop-invariant (a global, stack alloc,
                // or a dominating value).
                if !is_loop_invariant(arena, gep.base(), header, latch) {
        trace(data, looop, "gep_base_loop_variant");
        return None;
                }
                for &offset in gep.offsets() {
                    if offset != iv
                        && !is_loop_invariant(arena, offset, header, latch)
                        && !index_math.contains(&offset)
                    {
        trace(data, looop, "gep_offset_loop_variant");
        return None;
                    }
                }
                classes.insert(inst, Class::Keep);
            }
            InstKind::Load(load) => {
                let access = dep.accesses.iter().find(|a| a.inst == inst)?;
                let Some((class, elem)) =
                    classify_load(arena, func, effects, load.src(), access, &payload, header, latch)
                else {
                                        trace(data, looop, "load_unmodeled");
        trace(data, looop, "load_classify");
        return None;
                };
                merge_elem(&mut elem_ty, elem)?;
                if class == Class::VecLoad {
                    vectorized_any = true;
                }
                classes.insert(inst, class);
            }
            InstKind::Store(store) => {
                let access = dep.accesses.iter().find(|a| a.inst == inst)?;
                if access.kind != AccessKind::Write || access.byte_coefficient != VF {
        trace(data, looop, "store_not_contig_write");
        return None; // invariant / strided stores are not vectorized
                }
                if !base_is_16b_aligned(arena, effects, func, access.base) {
        trace(data, looop, "base_not_aligned");
        return None;
                }
                let elem = arena.inst_data(store.src()).ty().clone();
                if !elem.is_i32() && !elem.is_f32() {
        trace(data, looop, "elem_not_i32_f32");
        return None;
                }
                merge_elem(&mut elem_ty, elem)?;
                if !is_address_operand(arena, store.dest(), &payload, header, latch) {
        trace(data, looop, "store_dest_not_gep");
        return None;
                }
                vectorized_any = true;
                classes.insert(inst, Class::VecStore);
            }
            InstKind::Binary(binary) if is_vectorizable_binary_op(binary.op()) => {
                if index_math.contains(&inst) {
                    // Index math feeding a GEP offset stays scalar (the
                    // vector body re-derives it with the vector counter
                    // substituted for the IV).
                    classes.insert(inst, Class::Keep);
                } else {
                    // Classified in the second pass (needs the load classes).
                    classes.insert(inst, Class::VecBinary);
                }
            }
            InstKind::Select(select) => {
                // A select whose condition is a payload value (lane-wise,
                // e.g. a comparison fed by loads) or a loop-invariant is
                // rewritten as a lane-wise mask selection
                // `(t & ~m) | (f & m)` with `m = -(eq(cond, 0))`. The
                // rewrite uses only whitelisted vector ops, so no vector
                // select / bsl lowering is needed (v3 gap eliminated).
                let cond_ok = payload.contains(&select.cond())
                    || is_loop_invariant(arena, select.cond(), header, latch);
                if !cond_ok {
                    trace(data, looop, "select_cond_unknown");
                    return None;
                }
                classes.insert(inst, Class::VecSelect);
            }
            InstKind::Integer(_) | InstKind::Float(_) | InstKind::ZeroInit => {
                classes.insert(inst, Class::Keep);
            }
            other => {
                trace(
                    data,
                    looop,
                    &format!("payload_inst_rejected:{}", inst_kind_name(other)),
                );
                return None; // Select/Call/Cast/MemZero/Vector ops/...
            }
        }
    }

    // Second pass: binaries and store sources (in layout order, so every
    // operand that is a payload value is already classified).
    for &inst in &payload {
        if classes.get(&inst) != Some(&Class::VecBinary) {
            continue;
        }
        let InstKind::Binary(binary) = arena.inst_data(inst).kind() else {
            unreachable!();
        };
        let ty = arena.inst_data(binary.lhs()).ty().clone();
        if !ty.is_i32() && !ty.is_f32() {
        trace(data, looop, "binary_elem_not_scalar");
        return None;
        }
        // Shifts and bitwise ops exist only for integers: NEON has no float
        // shift/and/or/xor forms and float shift semantics do not exist.
        // Min/Max vectorize for both i32 and f32: smin/smax/fmin/fmax.
        if matches!(
            binary.op(),
            BinaryOp::Shl
                | BinaryOp::Shr
                | BinaryOp::Sar
                | BinaryOp::And
                | BinaryOp::Or
                | BinaryOp::Xor
        ) && !ty.is_i32()
        {
            trace(data, looop, "binary_shift_bitwise_not_i32");
            return None;
        }
        // NEON has no integer vector divide (`sdiv` is scalar-only). Integer
        // Div/Rem vectorize only with a *constant* divisor, which the
        // AArch64 backend lowers to a multiply-high magic sequence
        // (`smull`/`xtn`/`mls`); a variable divisor stays scalar. f32 Div
        // uses `fdiv`; NEON has no float remainder form.
        match binary.op() {
            BinaryOp::Div if ty.is_f32() => {}
            BinaryOp::Div | BinaryOp::Rem if ty.is_i32() => {
                if !matches!(arena.inst_data(binary.rhs()).kind(), InstKind::Integer(_)) {
                    trace(data, looop, "binary_int_div_rem_non_const");
                    return None;
                }
            }
            BinaryOp::Div | BinaryOp::Rem => {
                trace(data, looop, "binary_div_rem_unsupported_lane");
                return None;
            }
            _ => {}
        }
        if arena.inst_data(binary.rhs()).ty() != &ty {
            trace(data, looop, "binary_operand_type_mismatch");
            return None;
        }
        for operand in [binary.lhs(), binary.rhs()] {
            let operand_class = payload
                .iter()
                .position(|&p| p == operand)
                .and_then(|idx| classes.get(&payload[idx]).copied());
            match operand_class {
                Some(Class::VecLoad) | Some(Class::VecBinary) | Some(Class::VecSelect) => {}
                _ => {
                    // B3c: the index IV consumed as a value (not a GEP
                    // offset) is rewritten to the lane counter at mutation
                    // time, so it is a legal binary operand.
                    if operand == iv {
                        continue;
                    }
                    // The B1 accumulator operand (a header parameter re-typed
                    // to a vector in the mutation phase) is a legal vector
                    // operand even though its back-edge value changes: it
                    // feeds the arm's masked reduction update.
                    let is_acc = acc_info
                        .as_ref()
                        .is_some_and(|(acc, _, _)| *acc == operand);
                    if !is_acc && !is_loop_invariant(arena, operand, header, latch) {
                        trace(data, looop, "binary_operand_loop_variant");
                        return None;
                    }
                }
            }
        }
        merge_elem(&mut elem_ty, ty)?;
        vectorized_any = true;
    }
    // Second pass (selects): the selected values must be payload values or
    // loop-invariant (they resolve through the same machinery as binary
    // operands at mutation time).
    for &inst in &payload {
        if classes.get(&inst) != Some(&Class::VecSelect) {
            continue;
        }
        let InstKind::Select(select) = arena.inst_data(inst).kind() else {
            unreachable!();
        };
        let ty = arena.inst_data(select.if_true()).ty().clone();
        if !ty.is_i32() && !ty.is_f32() {
            trace(data, looop, "select_elem_not_scalar");
            return None;
        }
        if arena.inst_data(select.if_false()).ty() != &ty {
            trace(data, looop, "select_operand_type_mismatch");
            return None;
        }
        for operand in [select.if_true(), select.if_false()] {
            let operand_class = payload
                .iter()
                .position(|&p| p == operand)
                .and_then(|idx| classes.get(&payload[idx]).copied());
            match operand_class {
                Some(Class::VecLoad) | Some(Class::VecBinary) | Some(Class::VecSelect) => {}
                _ => {
                    if !is_loop_invariant(arena, operand, header, latch) {
                        trace(data, looop, "select_operand_loop_variant");
                        return None;
                    }
                }
            }
        }
        merge_elem(&mut elem_ty, ty)?;
        vectorized_any = true;
    }
    for &inst in &payload {
        if classes.get(&inst) != Some(&Class::VecStore) {
            continue;
        }
        let InstKind::Store(store) = arena.inst_data(inst).kind() else {
            unreachable!();
        };
        let src_class = payload
            .iter()
            .position(|&p| p == store.src())
            .and_then(|idx| classes.get(&payload[idx]).copied());
        match src_class {
            Some(Class::VecLoad) | Some(Class::VecBinary) | Some(Class::VecSelect) => {}
            _ => {
                // B3c: the index IV written as a value becomes the lane
                // counter at mutation time.
                if store.src() == iv {
                    continue;
                }
                if !is_loop_invariant(arena, store.src(), header, latch) {
                                        trace(data, looop, "store_src_loop_variant");
        return None;
                }
            }
        }
    }

    // 9. Every payload value must stay inside the loop (no uses in the exit
    //    region), and at least one real vector operation must be produced.
    //    With a B1 single-arm body the uses may sit in any body block
    //    (body_br / arm / merge), not just the latch. A user whose owning
    //    block is None is an orphaned instruction (removed from the layout
    //    by a transform that failed to detach its operands' used_by lists);
    //    it is unreachable and observes nothing, so it is not an escape.
    for &inst in &payload {
        for &user in arena.inst_data(inst).used_by() {
            let Some(user_bb) = data.layout().parent_bb(user) else {
                continue;
            };
            if !looop.contains(user_bb) {
        trace(data, looop, "value_escapes_loop");
        return None;
            }
        }
    }
    if !vectorized_any {
        trace(data, looop, "no_vector_ops");
        return None;
    }
    let elem_ty = elem_ty?;

    // B1: the accumulator delta must be a payload value (vectorized) or a
    // loop-invariant scalar (splatted); then build the reduction plan.
    // Mod-wrapped scalar reductions build no B1 plan (`mod_reduction`
    // handles the scalar accumulator). Milestone 2 multi-arm kernels keep
    // the accumulator chain in the body (see `MultiArmPlan`); the B1
    // ReductionPlan is left unset and `apply_vectorize` branches on
    // `multi_arm` instead.
    let reduction = if mod_params.is_some() || multi_arm.is_some() {
        None
    } else {
        match acc_info {
        None => None,
        Some((acc, acc_slot, bop)) => {
            let (update, delta) = acc_update.expect("acc_update set alongside acc_info");
            // The accumulator seed is splatted as a zero vector and added
            // back at the exit, so a block-arg seed (outer loop's `sum`)
            // is no longer splatted directly. The exit `add(seed, Σc)`
            // references the seed across loop levels — safe since IPSCCP
            // no longer folds vector results to a constant (fixed
            // 2026-08-05: VectorReduce etc. now take Lattice::Bottom).
            let delta_class = payload
                .iter()
                .position(|&p| p == delta)
                .and_then(|idx| classes.get(&payload[idx]).copied());
            match delta_class {
                Some(Class::VecLoad) | Some(Class::VecBinary) | Some(Class::Keep) => {}
                Some(_) => {
                    trace(data, looop, "b1_delta_not_vectorizable");
                    return None;
                }
                None => {
                    if !is_loop_invariant(arena, delta, header, latch) {
                        trace(data, looop, "b1_delta_loop_variant");
                        return None;
                    }
                }
            }
            Some(ReductionPlan {
                acc,
                acc_slot,
                op: bop,
                acc_update: update,
                delta,
                acc_init: entry_args[acc_slot],
                exit_acc_param: exit_specs
                    .iter()
                    .position(|spec| matches!(spec, ExitArgSpec::Acc))
                    .map(|pos| exit_params[pos]),
            })
        }
        }
    };
    // Mod-wrapped scalar reduction: the element computation is vectorized
    // and `addv`'d per round; the accumulator stays scalar and modded.
    let mod_reduction = mod_params.map(|(value, const_sum, modulus, add_chain)| {
        let (acc, acc_slot, _) = mod_plan.expect("mod_params set alongside mod_plan");
        ModReductionPlan {
            acc,
            acc_slot,
            value,
            const_sum,
            modulus,
            acc_update: acc_update
                .as_ref()
                .map(|(update, _)| *update)
                .expect("acc_update set alongside mod_params"),
            add_chain,
            acc_init: entry_args[acc_slot],
            exit_acc_param: exit_specs
                .iter()
                .position(|spec| matches!(spec, ExitArgSpec::Acc))
                .map(|pos| exit_params[pos]),
        }
    });

    // B1 single-arm bodies: a compile-time trip that is not a multiple of
    // VF is unsupported (the epilogue chain for multi-block bodies is not
    // implemented). A runtime trip is supported: the scalar tail loop
    // clones the arm's store as a scalar masked store (B3b), so the tail
    // preserves the conditional overwrite without a branch.
    if arm_plan.is_some() && !runtime_trip && trip % VF != 0 {
        trace(data, looop, "arm_remainder_not_supported");
        return None;
    }

    // 2x unroll: duplicate the payload so each array's two loads/stores are
    // adjacent with a constant continuation GEP, triggering the post-RA
    // pair_combine to form `ldp q`/`stp q`. The counter and IV then step by
    // 8; the scalar tail / epilogue covers the `trip & 7` remainder. Gate to
    // the shapes pair_combine can fuse and the continuation GEP can fold:
    // elementwise (no reduction / mod / arm), at least one vector load and
    // one vector store (a pure store-only loop cannot form a load pair), and
    // a trip of at least 8 elements (const) or any runtime trip.
    let unroll = reduction.is_none()
        && mod_reduction.is_none()
        && arm_plan.is_none()
        && !test_at_top
        && (runtime_trip || trip >= 2 * VF)
        && payload
            .iter()
            .any(|inst| classes.get(inst) == Some(&Class::VecLoad))
        && payload
            .iter()
            .any(|inst| classes.get(inst) == Some(&Class::VecStore));

    Some(VecPlan {
        header,
        latch,
        exit,
        entry_edge,
        iv,
        counter,
        entry_i0,
        trip,
        bound_inst,
        runtime_trip,
        latch_branch,
        t_next,
        iv_next,
        payload,
        classes,
        vector_ty: Type::get_vector(elem_ty, VF as usize),
        reduction,
        mod_reduction,
        passthrough_args,
        exit_specs,
        test_at_top,
        arm: arm_plan,
        multi_arm,
        unroll,
    })
}

/// Classify one load against its access function.
fn classify_load(
    arena: &ArenaContext<'_>,
    func: Function,
    effects: &EffectAnalysis,
    addr: Inst,
    access: &crate::opt::analysis_passes::dependence::AccessFunction,
    payload: &[Inst],
    header: BasicBlock,
    latch: BasicBlock,
) -> Option<(Class, Type)> {
    if access.kind != AccessKind::Read {
        return None;
    }
    let elem = arena.inst_data(addr).ty().derefernce();
    if !elem.is_i32() && !elem.is_f32() {
        return None;
    }
    if access.size != VF || !base_is_16b_aligned(arena, effects, func, access.base) {
        return None;
    }
    if !is_address_operand(arena, addr, payload, header, latch) {
        return None;
    }
    match access.byte_coefficient {
        0 => Some((Class::InvLoad, elem)),
        VF => Some((Class::VecLoad, elem)),
        _ => None,
    }
}

/// Whether two GEP instructions resolve to the same address (equal base and
/// semantically equal offsets — constants compared by value, since distinct
/// `Integer` instructions may hold the same constant). Used to pair a body
/// store with a same-address arm store.
fn same_gep_addr<A: Arena>(arena: &A, a: Inst, b: Inst) -> bool {
    let (InstKind::GetElemPtr(ga), InstKind::GetElemPtr(gb)) =
        (arena.inst_data(a).kind(), arena.inst_data(b).kind())
    else {
        return false;
    };
    if ga.base() != gb.base() || ga.offsets().len() != gb.offsets().len() {
        return false;
    }
    ga.offsets().iter().zip(gb.offsets()).all(|(&x, &y)| {
        x == y
            || match (arena.inst_data(x).kind(), arena.inst_data(y).kind()) {
                (InstKind::Integer(cx), InstKind::Integer(cy)) => cx.value() == cy.value(),
                _ => false,
            }
    })
}

/// `load.src` / `store.dest` must be a payload GEP or a loop-invariant
/// pointer. GEPs always classify as `Keep`, so the class map is not
/// consulted here.
fn is_address_operand(
    arena: &ArenaContext<'_>,
    addr: Inst,
    payload: &[Inst],
    header: BasicBlock,
    latch: BasicBlock,
) -> bool {
    if is_loop_invariant(arena, addr, header, latch) {
        return true;
    }
    payload.contains(&addr) && matches!(arena.inst_data(addr).kind(), InstKind::GetElemPtr(_))
}

fn is_add_one(arena: &ArenaContext<'_>, inst: Inst, lhs: Inst) -> bool {
    match arena.inst_data(inst).kind() {
        InstKind::Binary(binary) => {
            binary.op() == BinaryOp::Add
                && binary.lhs() == lhs
                && matches!(
                    arena.inst_data(binary.rhs()).kind(),
                    InstKind::Integer(one) if one.value() == 1
                )
        }
        _ => false,
    }
}

fn is_sub_one(arena: &ArenaContext<'_>, inst: Inst, lhs: Inst) -> bool {
    match arena.inst_data(inst).kind() {
        InstKind::Binary(binary) => {
            binary.op() == BinaryOp::Sub
                && binary.lhs() == lhs
                && matches!(
                    arena.inst_data(binary.rhs()).kind(),
                    InstKind::Integer(one) if one.value() == 1
                )
        }
        _ => false,
    }
}

fn is_vectorizable_binary_op(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::Mul
            | BinaryOp::Shl
            | BinaryOp::Shr
            | BinaryOp::Sar
            | BinaryOp::And
            | BinaryOp::Or
            | BinaryOp::Xor
            | BinaryOp::Div
            | BinaryOp::Rem
            | BinaryOp::Min
            | BinaryOp::Max
            | BinaryOp::Eq
            | BinaryOp::Gt
    )
}

/// Collect the payload instructions that (transitively) produce `seed` —
/// the scalar index math behind a GEP offset. Only payload binary ops are
/// followed (constants / invariants / the IV are leaves).
fn collect_index_math(
    seed: Inst,
    payload: &[Inst],
    arena: &ArenaContext<'_>,
    out: &mut FxHashSet<Inst>,
) {
    let mut work = vec![seed];
    let mut seen = FxHashSet::<Inst>::default();
    while let Some(v) = work.pop() {
        if !seen.insert(v) || !payload.contains(&v) {
            continue;
        }
        let InstKind::Binary(binary) = arena.inst_data(v).kind() else {
            continue;
        };
        out.insert(v);
        work.push(binary.lhs());
        work.push(binary.rhs());
    }
}

/// Loop-invariant value: a compile-time constant, a global, a dominating
/// (outside-the-loop) definition, an outer block parameter, or a header
/// *passthrough* parameter (an outer induction value the back edge forwards
/// unchanged — its value does not vary across iterations). Header/latch
/// block parameters with a real back-edge update are loop-variant.
fn is_loop_invariant(arena: &ArenaContext<'_>, inst: Inst, header: BasicBlock, latch: BasicBlock) -> bool {
    if matches!(
        arena.inst_data(inst).kind(),
        InstKind::Integer(_) | InstKind::Float(_) | InstKind::ZeroInit
    ) {
        return true;
    }
    if let Some(slot) = arena.bb_data(header).params().iter().position(|&p| p == inst) {
        // A passthrough parameter (the back-edge argument is the parameter
        // itself) or a loop-invariant one (the back-edge argument is a
        // constant — crypto's md5/sha1 state loops forward constants like
        // 256000 / 1 alongside the real IV). Rotated loops branch back to
        // the header; test-at-top loops jump back to it.
        let term = arena.curr_func_data().layout().basicblock(latch).terminator();
        return match arena.inst_data(term).kind() {
            InstKind::Branch(branch) => {
                branch.t_args().get(slot) == Some(&inst)
                    || branch.t_args().get(slot).is_some_and(|&ba| {
                        matches!(
                            arena.inst_data(ba).kind(),
                            InstKind::Integer(_) | InstKind::Float(_) | InstKind::ZeroInit
                        )
                    })
            }
            InstKind::Jump(jump) => {
                jump.args().get(slot) == Some(&inst)
                    || jump.args().get(slot).is_some_and(|&ba| {
                        matches!(
                            arena.inst_data(ba).kind(),
                            InstKind::Integer(_) | InstKind::Float(_) | InstKind::ZeroInit
                        )
                    })
            }
            _ => false,
        };
    }
    if arena.bb_data(latch).params().contains(&inst) {
        return false;
    }
    match arena.curr_func_data().layout().parent_bb(inst) {
        Some(bb) => bb != header && bb != latch,
        None => true, // globals, outer block parameters, unplaced constants
    }
}

/// v2 alignment gate: the base object must be provably 16B-aligned (globals
/// with size >= 16 are `.p2align 4`; AArch64 array stack slots round up to
/// 16). Array parameters are aligned iff every call site passes a 16B-aligned
/// actual (whole-program points-to from `EffectAnalysis`); unknown-provenance
/// bases stay rejected (that case needs M43 versioning).
fn base_is_16b_aligned(
    arena: &ArenaContext<'_>,
    effects: &EffectAnalysis,
    func: Function,
    base: MemObject,
) -> bool {
    match base {
        MemObject::Global(global) => arena.inst_data(global).ty().derefernce().size() >= 16,
        MemObject::Alloc(alloc) => {
            let ty = arena.inst_data(alloc).ty().derefernce();
            matches!(ty.kind(), TypeKind::Array(_, _)) && ty.size() >= 16
        }
        MemObject::Param(index) => param_is_16b_aligned(arena, effects, func, index),
        MemObject::Unknown => false,
    }
}

/// IPA parameter-alignment inference: a function parameter is 16B-aligned
/// when every actual at every call site resolves to a 16B-aligned base
/// object. The `EffectAnalysis` points-to sets are a conservative may-set
/// over all call sites (unknown-provenance actuals contribute
/// `AbstractObject::Unknown` and reject the parameter); an absent or empty
/// set means the function has no resolvable call sites — conservatively
/// rejected.
fn param_is_16b_aligned(
    arena: &ArenaContext<'_>,
    effects: &EffectAnalysis,
    func: Function,
    index: usize,
) -> bool {
    let Some(set) = effects.points_to_of(func, index) else {
        return false;
    };
    if set.is_empty() {
        return false;
    }
    set.iter().all(|obj| match obj {
        AbstractObject::Global(g) => arena.inst_data(*g).ty().derefernce().size() >= 16,
        AbstractObject::Alloc(_, a) => {
            let ty = arena.inst_data(*a).ty().derefernce();
            matches!(ty.kind(), TypeKind::Array(_, _)) && ty.size() >= 16
        }
        AbstractObject::Unknown => false,
    })
}

fn merge_elem(slot: &mut Option<Type>, elem: Type) -> Option<()> {
    match slot {
        Some(existing) if *existing != elem => None,
        Some(_) => Some(()),
        None => {
            *slot = Some(elem);
            Some(())
        }
    }
}

/// A compile-time i32 value: a literal or a constant-foldable subtraction
/// (the rotation computes `t0 = bound - i0`). i64 arithmetic keeps the
/// folding exact.
fn constant_i64(arena: &ArenaContext<'_>, data: &FunctionData, inst: Inst) -> Option<i64> {
    match arena.inst_data(inst).kind() {
        InstKind::Integer(value) => Some(i64::from(value.value())),
        InstKind::Binary(binary) if binary.op() == BinaryOp::Sub => {
            Some(constant_i64(arena, data, binary.lhs())? - constant_i64(arena, data, binary.rhs())?)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Mutation
// ---------------------------------------------------------------------------

fn apply_vectorize(data: &mut ArenaContextMut<'_>, plan: VecPlan) -> bool {
    // Milestone 2 phase 3: the multi-arm masked-accumulate kernel has its
    // own apply path (the elementwise/B1 machinery does not understand the
    // body accumulator chain).
    if plan.multi_arm.is_some() {
        let _ = std::env::var("MULTI_ARM_OFF");
        if std::env::var("MULTI_ARM_OFF").is_ok() {
            return false;
        }
        return apply_multi_arm(data, plan);
    }
    let VecPlan {
        header,
        latch,
        exit,
        entry_edge,
        iv,
        counter,
        entry_i0,
        trip,
        bound_inst,
        runtime_trip,
        latch_branch,
        t_next,
        iv_next,
        payload,
        classes,
        vector_ty,
        reduction,
        mod_reduction,
        passthrough_args,
        exit_specs,
        test_at_top,
        arm,
        multi_arm: _,
        unroll,
    } = plan;
    let step: i64 = if unroll { VF * 2 } else { VF };
    let q = trip / step;
    let r = trip % step;
    let i32 = Type::get_i32();
    let n_passthrough = passthrough_args.len();
    // B1 test-at-top exits read the header's accumulator parameter
    // directly (SSA dominance) instead of receiving it through an exit
    // parameter. The accumulator is re-typed to a vector in step 2b, so
    // those reads are rewritten to a fresh scalar exit parameter fed the
    // reduced sum by the reduce block / epilogue chain. The parameter is
    // appended after any existing exit parameters (the exit edge args are
    // rebuilt accordingly). The analyze gates (`exit_extra_pred`,
    // `exit_region_guard`) guarantee the exit has no other predecessors
    // and that any read outside the exit block lives in a block the
    // reduce block dominates, so the parameter can be added and the
    // post-exit reads rewritten without touching unknown edges.
    // Reads beyond the exit block (dominated by it) are rewritten the
    // same way: for a runtime trip they must observe the scalar tail's
    // final accumulator (threaded through this parameter by the tail's
    // exit edge), not the reduce block's vector-only sum.
    let post_exit_acc_users: Vec<Inst> = if test_at_top && reduction.is_some() {
        let acc = reduction.as_ref().expect("reduction set").acc;
        data.inst_data(acc)
            .used_by()
            .iter()
            .copied()
            .filter(|&u| {
                data.layout()
                    .parent_bb(u)
                    .is_some_and(|bb| bb != exit && bb != header && bb != latch)
            })
            .collect()
    } else {
        Vec::new()
    };
    let exit_extra_acc: Option<Inst> = if test_at_top && reduction.is_some() {
        let acc = reduction.as_ref().expect("reduction set").acc;
        let exit_users: Vec<Inst> = data
            .inst_data(acc)
            .used_by()
            .iter()
            .copied()
            .filter(|&u| data.layout().parent_bb(u) == Some(exit))
            .collect();
        if exit_users.is_empty() && post_exit_acc_users.is_empty() {
            None
        } else {
            let index = data.bb_data(exit).params().len() + 1;
            let ep = alloc_inst(data, BlockArgRef::new_data(index, i32.clone()));
            data.bb_data_mut(exit).params_mut().push(ep);
            for user in exit_users.into_iter().chain(post_exit_acc_users.iter().copied()) {
                subst_operand(data, user, acc, ep);
            }
            Some(ep)
        }
    } else {
        None
    };
    // A4: does any exit parameter carry the IV's final value? Those slots
    // must be fed the computed `i0 + VF*q` (+ the peeled remainder through
    // the epilogue chain) instead of the latch's per-iteration update.
    let has_iv_final = exit_specs.iter().any(|spec| *spec == ExitArgSpec::IvFinal);
    // Runtime-bound counter: `trip = sub(bound, i0)`, `cnt0 = and(trip,
    // -4)` — the vector loop runs `cnt0 / 4` iterations and a scalar tail
    // loop covers the `trip & 3` remainder. `cnt0 <= 0` (trip < 4, or a
    // negative trip) skips the vector loop entirely; the counter test
    // becomes `gt(counter, 0)` (not `counter != 0`) so a negative trip
    // cannot spin forever. `i0_inst` is the compile-time initial index as
    // an instruction (constants are not placed in layout).
    let i0_inst = data.new_local_inst().integer(entry_i0 as i32);
    // Rotated loops have no bound instruction (the trip counter is a
    // header parameter, entered from the preheader at `t0 = bound - i0`);
    // the counter's entry value is the trip itself and the scalar tail's
    // upper bound is rebuilt as `i0 + t0` below. Test-at-top loops derive
    // the trip from the bound.
    let rotated_counter_entry: Option<Inst> = if runtime_trip && !test_at_top {
        let args = match data.inst_data(entry_edge).kind() {
            InstKind::Branch(b) if b.t_target() == header => b.t_args(),
            InstKind::Jump(j) if j.target() == header => j.args(),
            _ => unreachable!("entry edge shape checked in analyze"),
        };
        args.last().copied() // rotated: the counter is the last header param
    } else {
        None
    };
    let (trip_inst, cnt0): (Inst, Inst) = if runtime_trip {
        let trip_rt = match rotated_counter_entry {
            Some(entry) => entry,
            None => {
                let t = alloc_inst(
                    data,
                    Binary::new_data(bound_inst, i0_inst, BinaryOp::Sub, i32.clone()),
                );
                data.layout_mut().insert_inst_before(entry_edge, t);
                t
            }
        };
        let neg_step = data.new_local_inst().integer(-step as i32);
        let cnt = alloc_inst(
            data,
            Binary::new_data(trip_rt, neg_step, BinaryOp::And, i32.clone()),
        );
        data.layout_mut().insert_inst_before(entry_edge, cnt);
        (trip_rt, cnt)
    } else {
        // Unused placeholders for the const path.
        let t = data.new_local_inst().integer((entry_i0 + step * q) as i32);
        (t, t)
    };
    // The tail's entry index: `iv0 = i0 + max(cnt0, 0)` — the vector
    // loop advanced the index by `cnt0`, and a negative trip clamps back
    // to `i0` so the tail runs zero iterations. The clamp is a scalar
    // select (the AArch64 backend has no scalar min/max), lowered to
    // `csel`. Shared by the reduce block (reduction) and the exit-edge
    // rewrite (elementwise).
    let iv0: Option<Inst> = if runtime_trip {
        let zero = data.new_local_inst().integer(0);
        let positive = alloc_inst(
            data,
            Binary::new_data(cnt0, zero, BinaryOp::Gt, i32.clone()),
        );
        let cnt_clamped = alloc_inst(
            data,
            Select::new_data(positive, cnt0, zero, i32.clone()),
        );
        data.layout_mut().insert_inst_before(entry_edge, positive);
        data.layout_mut().insert_inst_before(entry_edge, cnt_clamped);
        let start = alloc_inst(
            data,
            Binary::new_data(i0_inst, cnt_clamped, BinaryOp::Add, i32.clone()),
        );
        data.layout_mut().insert_inst_before(entry_edge, start);
        Some(start)
    } else {
        None
    };
    // Runtime-bound test-at-top: an exit that reads the header IV
    // parameter directly must see the final scalar value — `bound` when
    // the loop ran, `i0` when the trip was non-positive — not the vector
    // loop's stale `i0 + 4Q`. Rewrite those reads to
    // `select(gt(trip, 0), bound, i0)`. Rotated exits receive the final
    // IV through the latch branch's f_args (an `IvFinal` exit parameter,
    // fed `i0 + trip` below), so they need no such rewrite.
    if runtime_trip && test_at_top {
        let exit_iv_users: Vec<Inst> = data
            .inst_data(iv)
            .used_by()
            .iter()
            .copied()
            .filter(|&u| data.layout().parent_bb(u) == Some(exit))
            .collect();
        if !exit_iv_users.is_empty() {
            let zero = data.new_local_inst().integer(0);
            let positive = alloc_inst(
                data,
                Binary::new_data(trip_inst, zero, BinaryOp::Gt, i32.clone()),
            );
            let iv_final = alloc_inst(
                data,
                Select::new_data(positive, bound_inst, i0_inst, i32.clone()),
            );
            data.layout_mut().insert_inst_before(entry_edge, positive);
            data.layout_mut().insert_inst_before(entry_edge, iv_final);
            for user in exit_iv_users {
                subst_operand(data, user, iv, iv_final);
            }
        }
    }
    // The trip counter is the last header parameter; passthrough params
    // precede it ([.. passthroughs .., iv (, acc), t]). Test-at-top loops
    // have no counter parameter: one is materialized here as a new header
    // parameter (fed `step*q` from the entry edge, stepped by `step` in the
    // latch, and tested at the header in place of the bound).
    let four = data.new_local_inst().integer(step as i32);
    let (eff_counter, eff_t_next) = if test_at_top {
        // Materialize the trip counter as a new header parameter (mirrors
        // `BasicBlockBuilder::add_param`, which ArenaContextMut does not
        // implement): allocate the block argument and append it to the
        // header's parameter list.
        let param_index = data.bb_data(header).params().len() + 1;
        let bar = alloc_inst(data, BlockArgRef::new_data(param_index, i32.clone()));
        data.bb_data_mut(header).params_mut().push(bar);
        let t = alloc_inst(data, Binary::new_data(bar, four, BinaryOp::Sub, i32.clone()));
        let latch_term = data.layout().basicblock(latch).terminator();
        data.layout_mut().insert_inst_before(latch_term, t);
        (bar, t)
    } else {
        (counter, t_next)
    };
    let entry_arg_count = if test_at_top {
        // Unused: the counter is *appended* after the existing entry
        // arguments (all original parameters — passthroughs included — keep
        // their slots).
        2
    } else {
        2 + usize::from(reduction.is_some() || mod_reduction.is_some()) + n_passthrough
    };
    // The scalar tail's upper bound. Test-at-top loops reuse the original
    // `bound` instruction (a loop-invariant value). Rotated loops have no
    // bound: the counter entered at `t0 = bound - i0`, so the tail bound is
    // rebuilt as `i0 + t0` (inserted in the preheader, before the entry
    // edge). The tail runs `iv` from `i0 + (t0 & -4)` while `iv < i0 + t0`,
    // i.e. exactly `t0 & 3` iterations.
    let runtime_bound: Inst = match (runtime_trip, rotated_counter_entry) {
        (true, Some(entry)) => {
            let bt = alloc_inst(
                data,
                Binary::new_data(i0_inst, entry, BinaryOp::Add, i32.clone()),
            );
            data.layout_mut().insert_inst_before(entry_edge, bt);
            bt
        }
        _ => bound_inst,
    };

    // 1. Scalar epilogue: peel `r` (0..=3) straight-line copies of the
    //    original body with the index IV substituted by constants. Reduction
    //    epilogues carry the scalar accumulator as a block parameter and
    //    keep accumulating; passthrough values ride along as parameters so
    //    the chain can forward them to the exit.
    let mut epi_blocks: Vec<BasicBlock> = Vec::new();
    if r > 0 {
        for k in 0..r {
            let mut param_tys = vec![i32.clone(); n_passthrough];
            if reduction.is_some() {
                param_tys.insert(0, i32.clone());
            }
            let block = data
                .new_basic_block()
                .basic_block(format!("vec_epi_{k}"), param_tys);
            epi_blocks.push(block);
        }
    }

    // 1b. Runtime-bound scalar tail: a remainder loop
    //     `while (iv < bound) { payload; acc op= delta; iv++; }` entered
    //     when the vector counter reaches zero (or is already <= 0 for
    //     trip < 4). The payload is cloned with the IV substituted by the
    //     tail's own parameter, so the tail runs `trip & 3` iterations
    //     (0..3) and handles negative trips by running zero iterations.
    //     A reduction tail carries the scalar accumulator as its second
    //     parameter, seeded by the reduce block's sum (1c). Built before
    //     the reduce block (whose jump target it is) and before the
    //     in-place vectorization (step 2) so the payload is still scalar
    //     when cloned.
    let tail_header: Option<BasicBlock> = if runtime_trip {
        let one = data.new_local_inst().integer(1);
        let mut param_tys = vec![i32.clone(); 1 + n_passthrough];
        if reduction.is_some() || mod_reduction.is_some() {
            param_tys.insert(1, i32.clone());
        }
        let th = data
            .new_basic_block()
            .basic_block("vec_tail".into(), param_tys);
        let tl = data
            .new_basic_block()
            .basic_block("vec_tail_latch".into(), vec![]);
        // Blocks enter the layout before any instruction is inserted.
        data.layout_mut().insert_bb_after(latch, th);
        data.layout_mut().insert_bb_after(th, tl);
        let iv_t = data.bb_data(th).params()[0];
        let cond_t = alloc_inst(
            data,
            Binary::new_data(iv_t, runtime_bound, BinaryOp::Lt, i32.clone()),
        );
        let iv_final_rt = has_iv_final.then_some(runtime_bound);
        let tail_acc =
            reduction.as_ref().map(|_| data.bb_data(th).params()[1])
                .or_else(|| mod_reduction.as_ref().map(|_| data.bb_data(th).params()[1]));
        let mut exit_args = build_exit_args(&exit_specs, &passthrough_args, tail_acc, iv_final_rt);
        if let Some(_) = exit_extra_acc {
            // The freshly added exit parameter carries the final scalar
            // accumulator (the tail's own, already seeded with the
            // reduce-block sum).
            exit_args.push(tail_acc.expect("reduction tail carries acc"));
        }
        let th_br = data
            .new_local_inst()
            .branch(cond_t, tl, vec![], exit, exit_args);
        data.layout_mut().insert_inst(th, cond_t);
        data.layout_mut().insert_inst(th, th_br);
        // Tail latch: the cloned payload, the accumulator update
        // `acc_t' = op(acc_t, delta_k)` (reduction), then
        // `iv_t' = iv_t + 1`, then the jump back with the passthroughs
        // forwarded.
        let mut map = FxHashMap::<Inst, Inst>::default();
        let mut insts: Vec<Inst> = Vec::with_capacity(payload.len() + 2);
        // B1: arm stores become scalar masked stores in the tail; build
        // the arm store → same-address body store src map first.
        let arm_old_src: FxHashMap<Inst, Inst> = arm
            .as_ref()
            .map(|a| {
                payload
                    .iter()
                    .filter(|&&p| {
                        data.layout().parent_bb(p) == Some(a.arm)
                            && matches!(data.inst_data(p).kind(), InstKind::Store(_))
                    })
                    .map(|&p| {
                        let InstKind::Store(s) = data.inst_data(p).kind() else {
                            unreachable!()
                        };
                        let old = payload
                            .iter()
                            .copied()
                            .find(|&q| {
                                q != p
                                    && data.layout().parent_bb(q) != Some(a.arm)
                                    && matches!(data.inst_data(q).kind(), InstKind::Store(os)
                                        if same_gep_addr(data, os.dest(), s.dest()))
                            })
                            .map(|q| match data.inst_data(q).kind() {
                                InstKind::Store(os) => os.src(),
                                _ => unreachable!(),
                            });
                        (p, old.unwrap_or(iv))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let arm_mask = arm.as_ref().map(|a| ArmMask {
            arm_bb: a.arm,
            mask_cond: a.mask_cond,
            old_src: &arm_old_src,
        });
        for &orig in &payload {
            let cloned =
                clone_payload_inst(data, orig, &mut map, iv, iv_t, arm_mask.as_ref(), &mut insts);
            insts.push(cloned);
        }
        let mut jump_args: Vec<Inst> = Vec::new();
        if let Some(modr) = &mod_reduction {
            // `acc_k = (acc_t + delta_k + C) % P`: one scalar iteration per
            // tail round (the remainder runs 0..=3 scalar iterations).
            let delta_k = map.get(&modr.value).copied().unwrap_or(modr.value);
            let acc_t = data.bb_data(th).params()[1];
            let s0 = alloc_inst(
                data,
                Binary::new_data(acc_t, delta_k, BinaryOp::Add, i32.clone()),
            );
            insts.push(s0);
            let s1 = if modr.const_sum == 0 {
                s0
            } else {
                let c = data.new_local_inst().integer(modr.const_sum as i32);
                let s = alloc_inst(data, Binary::new_data(s0, c, BinaryOp::Add, i32.clone()));
                insts.push(s);
                s
            };
            let p = data.new_local_inst().integer(modr.modulus);
            let acc_k = alloc_inst(data, Binary::new_data(s1, p, BinaryOp::Rem, i32.clone()));
            insts.push(acc_k);
            jump_args.push(acc_k);
        } else if let Some(red) = &reduction {
            // `acc_k = op(acc_t, delta_k)`: the delta clone exists when
            // the delta is a payload inst; invariant deltas are reused.
            let delta_k = map.get(&red.delta).copied().unwrap_or(red.delta);
            let acc_t = data.bb_data(th).params()[1];
            let acc_k = alloc_inst(
                data,
                Binary::new_data(acc_t, delta_k, red.op, i32.clone()),
            );
            insts.push(acc_k);
            jump_args.push(acc_k);
        }
        let iv_t_next = alloc_inst(
            data,
            Binary::new_data(iv_t, one, BinaryOp::Add, i32.clone()),
        );
        insts.push(iv_t_next);
        jump_args.insert(0, iv_t_next);
        jump_args.extend(passthrough_args.iter().copied());
        insts.push(data.new_local_inst().jump(th, jump_args));
        for inst in insts {
            data.layout_mut().insert_inst(tl, inst);
        }
        Some(th)
    } else {
        None
    };

    // 1c. Reduction exit block: `acc_final = VectorReduce(Add, acc_vec)`,
    //     then jump into the epilogue chain (or straight to the exit when
    //     r == 0, or into the scalar tail for a runtime trip). Placed
    //     right after the latch. Passthrough values are forwarded
    //     alongside the reduced accumulator.
    let mut reduce_sum: Option<Inst> = None;
    let reduce_block: Option<BasicBlock> = if let Some(red) = &reduction {
        let mut param_tys = vec![i32.clone(); n_passthrough];
        param_tys.insert(0, vector_ty.clone());
        let rb = data
            .new_basic_block()
            .basic_block("vec_reduce".into(), param_tys);
        data.layout_mut().insert_bb_after(latch, rb);
        let acc_vec_param = data.bb_data(rb).params()[0];
        let acc_final = alloc_inst(
            data,
            VectorReduce::new_data(VectorReduceOp::Add, acc_vec_param, i32.clone()),
        );
        data.layout_mut().insert_inst(rb, acc_final);
        // The scalar sum (`seed + Σc`) is needed when the exit receives
        // the accumulator (through a parameter or the freshly added one),
        // when post-exit reads are rewritten to it, and when the runtime
        // tail continues the reduction from it.
        let sum: Option<Inst> = (red.exit_acc_param.is_some()
            || exit_extra_acc.is_some()
            || runtime_trip
            || !post_exit_acc_users.is_empty())
        .then(|| {
            // The vector loop accumulates with a zero seed
            // (splat(0)), so the outer seed must be added back:
            // sum_final = seed + Σc. (Splatting the seed itself
            // would overcount 4x at the addv.)
            let sum = alloc_inst(
                data,
                Binary::new_data(red.acc_init, acc_final, BinaryOp::Add, i32.clone()),
            );
            data.layout_mut().insert_inst(rb, sum);
            sum
        });
        reduce_sum = sum;
        let (target, args) = if runtime_trip {
            // The scalar tail continues the reduction: its accumulator
            // parameter is seeded with `sum = seed + Σc` and its index
            // with `iv0`; passthroughs ride along.
            let mut fwd = vec![iv0.expect("runtime iv0"), sum.expect("sum built")];
            fwd.extend(data.bb_data(rb).params()[1..].iter().copied());
            (tail_header.expect("tail built when runtime_trip"), fwd)
        } else {
            match epi_blocks.first().copied() {
                Some(first) => {
                    let mut fwd = vec![acc_final];
                    fwd.extend(data.bb_data(rb).params()[1..].iter().copied());
                    (first, fwd)
                }
                None => {
                    // r == 0: the exit edge carries the reduced accumulator
                    // (the scalar sum), the passthrough values, and — when
                    // declared — the computed IV final value `i0 + VF*q`
                    // (the loop ran exactly `q` vector iterations, so
                    // `i0 + 4q == i0 + trip`). A param-less test-at-top
                    // exit receives just the sum through its freshly added
                    // parameter (`exit_extra_acc`).
                    let iv_final_const = has_iv_final
                        .then(|| data.new_local_inst().integer((entry_i0 + step * q) as i32));
                    let mut args =
                        build_exit_args(&exit_specs, &passthrough_args, sum, iv_final_const);
                    if let Some(_) = exit_extra_acc {
                        // The freshly added parameter carries the scalar
                        // sum.
                        args.push(sum.expect("sum built above"));
                    }
                    (exit, args)
                }
            }
        };
        let jump = data.new_local_inst().jump(target, args);
        data.layout_mut().insert_inst(rb, jump);
        Some(rb)
    } else {
        None
    };
    // Post-exit accumulator reads (blocks dominated by the exit, gated in
    // analyze) are rewritten to the reduce block's scalar sum when no exit
    // parameter was created — the sum dominates the whole exit region
    // because every path to it goes through the reduce block. With an
    // `exit_extra_acc` parameter the reads were already rewritten to it
    // above (and for a runtime trip the tail threads its final
    // accumulator through that parameter).
    if exit_extra_acc.is_none() {
        if let (Some(red), Some(sum)) = (&reduction, &reduce_sum) {
            for user in post_exit_acc_users {
                subst_operand(data, user, red.acc, *sum);
            }
        }
    }

    if r > 0 {
        for (idx, &block) in epi_blocks.iter().enumerate() {
            let subst_iv = data
                .new_local_inst()
                .integer((entry_i0 + step * q + idx as i64) as i32);
            let mut map = FxHashMap::<Inst, Inst>::default();
            let mut insts: Vec<Inst> = Vec::with_capacity(payload.len() + 2);
            for &orig in &payload {
                let cloned =
                    clone_payload_inst(data, orig, &mut map, iv, subst_iv, None, &mut insts);
                insts.push(cloned);
            }
            let mut jump_args: Vec<Inst> = Vec::new();
            if let Some(red) = &reduction {
                // acc_k' = op(acc_param, delta_k). The delta clone exists when
                // the delta is a payload inst; invariant deltas are reused.
                let delta_k = map.get(&red.delta).copied().unwrap_or(red.delta);
                let acc_param = data.bb_data(block).params()[0];
                let acc_k = alloc_inst(
                    data,
                    Binary::new_data(acc_param, delta_k, red.op, i32.clone()),
                );
                insts.push(acc_k);
                jump_args.push(acc_k);
            }
            let acc_value = jump_args.first().copied();
            let target = epi_blocks.get(idx + 1).copied();
            let (target, jump_args) = match target {
                Some(next) => {
                    // Forward the invariant passthrough values through the
                    // chain.
                    let epi_params = data.bb_data(block).params();
                    let passthrough_start = usize::from(reduction.is_some());
                    jump_args.extend(epi_params[passthrough_start..].iter().copied());
                    (next, jump_args)
                }
                None => {
                    // The chain's last block jumps to the exit: rebuild the
                    // exit's arguments from its parameter classes (or feed
                    // the single freshly added parameter for a param-less
                    // test-at-top exit). The IV final value is the
                    // computed `i0 + VF*q + r`: the peeled scalar
                    // iterations ran with `iv = i0 + 4q + k`, so the
                    // scalar exit value `i0 + trip` == `i0 + 4q + r`.
                    let iv_final_const = has_iv_final
                        .then(|| data.new_local_inst().integer((entry_i0 + step * q + r) as i32));
                    let mut args = build_exit_args(
                        &exit_specs,
                        &passthrough_args,
                        acc_value,
                        iv_final_const,
                    );
                    if let Some(_) = exit_extra_acc {
                        // The freshly added parameter carries the final
                        // scalar accumulator from the peeled iterations.
                        args.push(acc_value.expect("acc_value set with reduction"));
                    }
                    (exit, args)
                }
            };
            insts.push(data.new_local_inst().jump(target, jump_args));
            let after = if idx == 0 {
                reduce_block.unwrap_or(latch)
            } else {
                epi_blocks[idx - 1]
            };
            data.layout_mut().insert_bb_after(after, block);
            for inst in insts {
                data.layout_mut().insert_inst(block, inst);
            }
        }
    }

    // 2. In-place vectorization of the latch payload (instruction ids are
    //    preserved by ReplaceBuilder, so later operands stay valid).
    //    The B1 accumulator is re-typed *before* the payload is rewritten:
    //    the arm's masked reduction (`acc' = binary(acc, delta)`) reads the
    //    accumulator as a vector operand, so it must already be a vector
    //    when the payload loop splats binary operands.
    if let Some(red) = &reduction {
        data.inst_data_mut(red.acc).set_type(vector_ty.clone());
        // B1 masked reduction: the merge (latch) block's accumulator phi —
        // the back-edge argument for the accumulator slot — must become a
        // vector too (it carries the masked `select(cond, acc, acc+rhs)`).
            if let Some(arm_plan) = &arm {
                if let InstKind::Branch(b) = data.inst_data(latch_branch).kind() {
                    if let Some(&merge_param) = b.t_args().get(red.acc_slot) {
                        data.inst_data_mut(merge_param).set_type(vector_ty.clone());
                    }
                }
            }
    }
    let mut splats = FxHashMap::<Inst, Inst>::default();
    // B3c: the index IV consumed as a payload *value* (a store src or a
    // binary operand — not a GEP offset, which stays scalar) is rewritten
    // to the lane counter `splat(iv) + [0, 1, .., VF-1]`, not a broadcast.
    // The lane-offset constant is built once here; it is loop-invariant and
    // LICM hoists it out of the body.
    let iv_as_value = payload.iter().any(|&p| {
        match data.inst_data(p).kind() {
            InstKind::Binary(b) => b.lhs() == iv || b.rhs() == iv,
            InstKind::Store(s) => s.src() == iv,
            InstKind::Select(s) => {
                s.cond() == iv || s.if_true() == iv || s.if_false() == iv
            }
            _ => false,
        }
    });
    if iv_as_value && !splats.contains_key(&iv) {
        let zero = data.new_local_inst().integer(0);
        let anchor = payload[0];
        // Build the lane-offset constant chain and insert every
        // instruction in def-before-use order (alloc_inst alone leaves
        // them out of the layout, producing dangling references).
        let mut lane_off = alloc_inst(data, VectorSplat::new_data(zero, vector_ty.clone()));
        data.layout_mut().insert_inst_before(anchor, lane_off);
        for k in 1..VF as i64 {
            let c = data.new_local_inst().integer(k as i32);
            let idx = data.new_local_inst().integer(k as i32);
            lane_off = alloc_inst(
                data,
                VectorInsertElement::new_data(lane_off, c, idx, vector_ty.clone()),
            );
            data.layout_mut().insert_inst_before(anchor, lane_off);
        }
        let iv_splat = alloc_inst(data, VectorSplat::new_data(iv, vector_ty.clone()));
        data.layout_mut().insert_inst_before(anchor, iv_splat);
        let iv_vec = alloc_inst(
            data,
            Binary::new_data(iv_splat, lane_off, BinaryOp::Add, vector_ty.clone()),
        );
        data.layout_mut().insert_inst_before(anchor, iv_vec);
        splats.insert(iv, iv_vec);
    }
    // B3b: arm stores fall back to the same-address body store's *original*
    // src as the masked store's "old" side. Captured before the payload
    // loop rewrites the stores (their src becomes a vector then).
    let arm_old_src: FxHashMap<Inst, Inst> = arm
        .as_ref()
        .map(|a| {
            payload
                .iter()
                .filter(|&&p| {
                    data.layout().parent_bb(p) == Some(a.arm)
                        && matches!(data.inst_data(p).kind(), InstKind::Store(_))
                })
                .map(|&p| {
                    let InstKind::Store(s) = data.inst_data(p).kind() else {
                        unreachable!()
                    };
                    let old = payload
                        .iter()
                        .copied()
                        .find(|&q| {
                            q != p
                                && data.layout().parent_bb(q) != Some(a.arm)
                                && matches!(data.inst_data(q).kind(), InstKind::Store(os)
                                    if same_gep_addr(data, os.dest(), s.dest()))
                        })
                        .map(|q| match data.inst_data(q).kind() {
                            InstKind::Store(os) => os.src(),
                            _ => unreachable!(),
                        });
                    (p, old.unwrap_or(iv))
                })
                .collect()
        })
        .unwrap_or_default();
    for &inst in &payload {
        match classes.get(&inst) {
            Some(Class::VecLoad) => {
                let addr = match data.inst_data(inst).kind() {
                    InstKind::Load(load) => load.src(),
                    _ => unreachable!(),
                };
                data.replace_inst_with(inst)
                    .raw(Load::new_data(addr, vector_ty.clone()));
            }
            Some(Class::VecBinary) => {
                let (op, lhs, rhs) = match data.inst_data(inst).kind() {
                    InstKind::Binary(binary) => (binary.op(), binary.lhs(), binary.rhs()),
                    _ => unreachable!(),
                };
                let vlhs = vector_operand(
                    data, &mut splats, lhs, &payload, &classes, &vector_ty, inst,
                );
                let vrhs = vector_operand(
                    data, &mut splats, rhs, &payload, &classes, &vector_ty, inst,
                );
                data.replace_inst_with(inst)
                    .raw(Binary::new_data(vlhs, vrhs, op, vector_ty.clone()));
            }
            Some(Class::VecStore) => {
                let (src, dest) = match data.inst_data(inst).kind() {
                    InstKind::Store(store) => (store.src(), store.dest()),
                    _ => unreachable!(),
                };
                if let Some(arm_plan) = &arm {
                    let store_bb = data.layout().parent_bb(inst).unwrap();
                    if store_bb == arm_plan.arm {
                        // B1 masked store: `store (new & m) | (old & ~m)`
                        // with `m = -(eq(mask_cond, 0))` — all-ones exactly
                        // when the arm executes (it sits on the branch's
                        // false edge). The old value is the same-address
                        // load already inside the arm; when the body has no
                        // such load (a pure overwrite shape
                        // `a[i] = i; if (c) a[i] = 4;`), fall back to the
                        // same-address unconditional store's *original* src
                        // (captured before the payload loop rewrote the
                        // stores) — its vector value re-writes the
                        // unconditional lanes, preserving program order
                        // (B3b).
                        let old = payload
                            .iter()
                            .copied()
                            .find(|&p| {
                                matches!(
                                    data.inst_data(p).kind(),
                                    InstKind::Load(l) if l.src() == dest
                                )
                            })
                            .or_else(|| arm_old_src.get(&inst).copied());
                        let Some(old) = old else {
                            unreachable!("arm store without a same-address load or body store");
                        };
                        let vnew = vector_operand(
                            data, &mut splats, src, &payload, &classes, &vector_ty, inst,
                        );
                        let vold = vector_operand(
                            data, &mut splats, old, &payload, &classes, &vector_ty, inst,
                        );
                        let vcond = vector_operand(
                            data,
                            &mut splats,
                            arm_plan.mask_cond,
                            &payload,
                            &classes,
                            &vector_ty,
                            inst,
                        );
                        let zero = data.new_local_inst().integer(0);
                        let neg_one = data.new_local_inst().integer(-1);
                        let vzero = vector_operand(
                            data, &mut splats, zero, &payload, &classes, &vector_ty, inst,
                        );
                        let vneg = vector_operand(
                            data, &mut splats, neg_one, &payload, &classes, &vector_ty, inst,
                        );
                        let eq0 = alloc_inst(
                            data,
                            Binary::new_data(vcond, vzero, BinaryOp::Eq, vector_ty.clone()),
                        );
                        data.layout_mut().insert_inst_before(inst, eq0);
                        // The mask is the comparison result itself: NEON
                        // `cmeq` returns all-ones per lane for true (not a
                        // 0/1 boolean), so `m = eq(cond, 0)` is already the
                        // all-ones/all-zero mask. The previous
                        // `m = sub(0, eq(cond, 0))` assumed a 0/1 boolean
                        // and produced `1` instead of all-ones, corrupting
                        // the mask (old value's bit 0 was cleared).
                        let m = eq0;
                        let nm = alloc_inst(
                            data,
                            Binary::new_data(m, vneg, BinaryOp::Xor, vector_ty.clone()),
                        );
                        data.layout_mut().insert_inst_before(inst, nm);
                        // Masked-kernel shape (`arm_on_true`): the arm
                        // executes when cond != 0, so swap the polarity —
                        // `new` is gated by `nm` (cond != 0), `old` by `m`.
                        let (m_new, m_old) = if arm_plan.arm_on_true {
                            (nm, m)
                        } else {
                            (m, nm)
                        };
                        let tn = alloc_inst(
                            data,
                            Binary::new_data(vnew, m_new, BinaryOp::And, vector_ty.clone()),
                        );
                        data.layout_mut().insert_inst_before(inst, tn);
                        let fo = alloc_inst(
                            data,
                            Binary::new_data(vold, m_old, BinaryOp::And, vector_ty.clone()),
                        );
                        data.layout_mut().insert_inst_before(inst, fo);
                        let sel = alloc_inst(
                            data,
                            Binary::new_data(tn, fo, BinaryOp::Or, vector_ty.clone()),
                        );
                        data.layout_mut().insert_inst_before(inst, sel);
                        data.replace_inst_with(inst).raw(Store::new_data(sel, dest));
                        continue;
                    }
                }
                let vsrc = vector_operand(
                    data, &mut splats, src, &payload, &classes, &vector_ty, inst,
                );
                data.replace_inst_with(inst).raw(Store::new_data(vsrc, dest));
            }
            Some(Class::VecSelect) => {
                // `select(cond, t, f)` with a loop-invariant condition: the
                // whole vector picks one side. Rewrite as a lane-wise mask
                // selection `(t & ~m) | (f & m)` with `m = -(eq(cond, 0))`
                // (all-ones iff the condition is false); every piece is a
                // whitelisted vector op, so no lowering changes are needed.
                let (cond, t, f) = match data.inst_data(inst).kind() {
                    InstKind::Select(select) => {
                        (select.cond(), select.if_true(), select.if_false())
                    }
                    _ => unreachable!(),
                };
                let vt =
                    vector_operand(data, &mut splats, t, &payload, &classes, &vector_ty, inst);
                let vf =
                    vector_operand(data, &mut splats, f, &payload, &classes, &vector_ty, inst);
                let vcond = vector_operand(
                    data, &mut splats, cond, &payload, &classes, &vector_ty, inst,
                );
                let zero = data.new_local_inst().integer(0);
                let neg_one = data.new_local_inst().integer(-1);
                let vzero = vector_operand(
                    data, &mut splats, zero, &payload, &classes, &vector_ty, inst,
                );
                let vneg = vector_operand(
                    data, &mut splats, neg_one, &payload, &classes, &vector_ty, inst,
                );
                let eq0 = alloc_inst(
                    data,
                    Binary::new_data(vcond, vzero, BinaryOp::Eq, vector_ty.clone()),
                );
                data.layout_mut().insert_inst_before(inst, eq0);
                // `m = eq(cond, 0)` directly: NEON `cmeq` yields all-ones
                // for true, which is the mask (see the B1 masked-store
                // comment above for the full rationale).
                let m = eq0;
                let nm = alloc_inst(
                    data,
                    Binary::new_data(m, vneg, BinaryOp::Xor, vector_ty.clone()),
                );
                data.layout_mut().insert_inst_before(inst, nm);
                let ta = alloc_inst(
                    data,
                    Binary::new_data(vt, nm, BinaryOp::And, vector_ty.clone()),
                );
                data.layout_mut().insert_inst_before(inst, ta);
                let fa = alloc_inst(
                    data,
                    Binary::new_data(vf, m, BinaryOp::And, vector_ty.clone()),
                );
                data.layout_mut().insert_inst_before(inst, fa);
                data.replace_inst_with(inst)
                    .raw(Binary::new_data(ta, fa, BinaryOp::Or, vector_ty.clone()));
            }
            Some(Class::InvLoad) | Some(Class::Keep) => {}
            None => unreachable!("every payload inst is classified"),
        }
    }

    // 2a. 2x unroll: duplicate the vectorized payload so each array's two
    //     loads / stores become adjacent, sharing the first copy's base
    //     register via a constant continuation GEP (`getelemptr %addr, 4`).
    //     Lowering folds the continuation into `[x, #16]`, and the post-RA
    //     pair_combine fuses the adjacent pair into `ldp q` / `stp q`. The
    //     counter and IV already step by `step` (= 8) from the threading
    //     above; the tail / epilogue cover the `trip & 7` remainder.
    if unroll {
        unroll_payload_2x(data, latch, &payload, &classes, &vector_ty, latch_branch, t_next, iv_next);
    }

    // 2b. B1: re-type the accumulator parameter to `<4 x T>` and rewrite its
    //     update chain into a lane-wise accumulation. The delta resolves
    //     through the same machinery as any payload value (vector load /
    //     vector binary) or is splatted when loop-invariant.
    //
    //     Mod-wrapped scalar reduction: the accumulator stays scalar; the
    //     element computation is vectorized and `addv`'d once per round
    //     (`round_sum = Σ value_lanes`), then the update is rebuilt as
    //     `acc = (acc + round_sum + VF*C) % P`.
    if let Some(modr) = &mod_reduction {
        let e_vec = vector_operand(
            data,
            &mut splats,
            modr.value,
            &payload,
            &classes,
            &vector_ty,
            modr.acc_update,
        );
        // Insert before the first chain add (the earliest use of the
        // per-round sum), so def-before-use holds in the latch layout.
        let anchor = modr.add_chain.first().copied().unwrap_or(modr.acc_update);
        let round_sum = alloc_inst(
            data,
            VectorReduce::new_data(VectorReduceOp::Add, e_vec, i32.clone()),
        );
        data.layout_mut().insert_inst_before(anchor, round_sum);
        let total = if modr.const_sum == 0 {
            round_sum
        } else {
            let c = data.new_local_inst().integer((VF * modr.const_sum) as i32);
            let t = alloc_inst(
                data,
                Binary::new_data(round_sum, c, BinaryOp::Add, i32.clone()),
            );
            data.layout_mut().insert_inst_before(anchor, t);
            t
        };
        // Rebuild the excluded add chain flat: the bottom add becomes
        // `acc + total`, the rest are identity forwards (folded by later
        // passes).
        let zero = data.new_local_inst().integer(0);
        let mut prev = modr.acc;
        for &add in &modr.add_chain {
            let rhs = if add == modr.add_chain[0] { total } else { zero };
            data.replace_inst_with(add)
                .raw(Binary::new_data(prev, rhs, BinaryOp::Add, i32.clone()));
            prev = add;
        }
        let p = data.new_local_inst().integer(modr.modulus);
        data.replace_inst_with(modr.acc_update)
            .raw(Binary::new_data(prev, p, BinaryOp::Rem, i32.clone()));
    } else if let Some(red) = &reduction {
        if let Some(arm_plan) = &arm {
            // B1 masked reduction: the arm's `delta` (now `acc + rhs`, the
            // payload loop already rewrote it lane-wise) executes only when
            // the arm runs. Rewrite it into a lane-wise mask selection
            // `(acc & ~m) | (delta & m)` with `m = -(eq(cond, 0))` —
            // all-ones exactly when the arm runs (it sits on the branch's
            // false edge). The merge phi then carries the masked
            // accumulator.
            let vacc = vector_operand(
                data,
                &mut splats,
                red.acc,
                &payload,
                &classes,
                &vector_ty,
                red.acc_update,
            );
            let vcond = vector_operand(
                data,
                &mut splats,
                arm_plan.mask_cond,
                &payload,
                &classes,
                &vector_ty,
                red.acc_update,
            );
            let zero = data.new_local_inst().integer(0);
            let vzero = vector_operand(
                data,
                &mut splats,
                zero,
                &payload,
                &classes,
                &vector_ty,
                red.acc_update,
            );
            let m = alloc_inst(
                data,
                Binary::new_data(vcond, vzero, BinaryOp::Eq, vector_ty.clone()),
            );
            // `tn`/`fo`/`sel` reference `red.acc_update` (the lane-wise
            // `acc + rhs` in the arm). They must be defined *after* the
            // update instruction, so insert them before the arm's terminator
            // (not before the update — `insert_inst_before(red.acc_update, ..)`
            // would place the mask reads before the value they read, a
            // use-before-def that the backend SSA verifier rejects).
            let arm_term = data.layout().basicblock(arm_plan.arm).terminator();
            data.layout_mut().insert_inst_before(arm_term, m);
            let neg_one = data.new_local_inst().integer(-1);
            let vneg = vector_operand(
                data,
                &mut splats,
                neg_one,
                &payload,
                &classes,
                &vector_ty,
                red.acc_update,
            );
            let nm = alloc_inst(
                data,
                Binary::new_data(m, vneg, BinaryOp::Xor, vector_ty.clone()),
            );
            data.layout_mut().insert_inst_before(arm_term, nm);
            // The arm sits on the branch's true edge when `arm_on_true`:
            // then it executes when `cond != 0`, so `delta` is gated by
            // `~m` and `acc` by `m` (inverted from the false-edge case).
            let (m_new, m_old) = if arm_plan.arm_on_true { (nm, m) } else { (m, nm) };
            // `delta` = the payload-rewritten `acc + rhs` (red.acc_update).
            let vdelta = red.acc_update;
            let tn = alloc_inst(
                data,
                Binary::new_data(vdelta, m_new, BinaryOp::And, vector_ty.clone()),
            );
            data.layout_mut().insert_inst_before(arm_term, tn);
            let fo = alloc_inst(
                data,
                Binary::new_data(vacc, m_old, BinaryOp::And, vector_ty.clone()),
            );
            data.layout_mut().insert_inst_before(arm_term, fo);
            let sel = alloc_inst(
                data,
                Binary::new_data(tn, fo, BinaryOp::Or, vector_ty.clone()),
            );
            data.layout_mut().insert_inst_before(arm_term, sel);
            // The merge phi reads the masked accumulator: retarget the arm's
            // jump argument (the delta) to the selection result. The delta
            // instruction becomes dead and is swept by DCE. The mask-expansion
            // instructions created above (`tn`, `fo`, `sel`) reference the
            // delta as their operand and must NOT be rewritten — substituting
            // `sel` for the delta inside `tn = delta & m_new` would make
            // `tn = sel & m_new` while `sel = tn | fo`, an Inst-level cycle
            // (GVN then recurses forever on `And(Or(..),..) <-> Or(And(..),..)`).
            let arm_term = data.layout().basicblock(arm_plan.arm).terminator();
            let users: Vec<Inst> = data.inst_data(red.acc_update).used_by().iter().copied().collect();
            for user in users {
                if data.layout().parent_bb(user) == Some(arm_plan.arm)
                    && user != tn
                    && user != fo
                    && user != sel
                {
                    subst_operand(data, user, red.acc_update, sel);
                }
            }
            let _ = arm_term;
        } else {
            let delta_vec = vector_operand(
                data,
                &mut splats,
                red.delta,
                &payload,
                &classes,
                &vector_ty,
                red.acc_update,
            );
            data.replace_inst_with(red.acc_update)
                .raw(Binary::new_data(red.acc, delta_vec, red.op, vector_ty.clone()));
        }
    }

    // 2c. Retarget the exit edge: the reduce block (reduction) or the
    //     epilogue chain (r > 0); test-at-top loops always rewrite the
    //     header's bound test into a counter test (`t != 0`) here, entering
    //     the epilogue when the counter reaches zero. Runs after the
    //     accumulator update is re-typed (2b) so the branch's f_args
    //     type-check against the reduce block's vector parameter.
    //     Passthrough values (loop-invariant) feed the chain / exit as-is;
    //     an IV-final exit parameter (A4) is replaced by the computed
    //     constant `i0 + VF*q` — the loop now runs `q` vector iterations,
    //     so the exit must not receive the latch's per-iteration update.
    if test_at_top || runtime_trip || reduction.is_some() || r > 0 || has_iv_final {
        let (cond, t_target, t_args) = if test_at_top {
            // The counter test's true edge enters the loop body: the B1
            // single-arm shape starts at body_br (the payload), otherwise
            // the latch holds the payload. The original bound test's true
            // edge is retargeted accordingly.
            let body_target = arm.as_ref().map(|a| a.body_br).unwrap_or(latch);
            if runtime_trip {
                // `gt(counter, 0)`: a negative `cnt0` (negative trip)
                // must not spin the vector loop forever.
                let zero = data.new_local_inst().integer(0);
                let gt = alloc_inst(
                    data,
                    Binary::new_data(eff_counter, zero, BinaryOp::Gt, i32.clone()),
                );
                data.layout_mut().insert_inst_before(latch_branch, gt);
                (gt, body_target, vec![])
            } else {
                (eff_counter, body_target, vec![])
            }
        } else {
            let (orig_cond, t_target, t_args) = match data.inst_data(latch_branch).kind() {
                InstKind::Branch(b) => (b.cond(), b.t_target(), b.t_args().to_vec()),
                _ => unreachable!(),
            };
            let cond = if runtime_trip {
                // Test the *updated* counter (`eff_t_next`, rewritten to
                // `counter - 4`): the rotated latch is a do-while — the body
                // runs once before the test — so testing the entry value
                // would run one extra vector body (reading past the trip and
                // double-counting the first `trip & 3` elements in the tail).
                // `gt(counter', 0)` also guards a negative `cnt0` (negative
                // trip) from spinning forever. The const path keeps the
                // original `t' != 0` test, which already compares the
                // updated counter.
                let zero = data.new_local_inst().integer(0);
                let gt = alloc_inst(
                    data,
                    Binary::new_data(eff_t_next, zero, BinaryOp::Gt, i32.clone()),
                );
                data.layout_mut().insert_inst_before(latch_branch, gt);
                gt
            } else {
                orig_cond
            };
            (cond, t_target, t_args)
        };
        let iv_final_const = has_iv_final
            .then(|| data.new_local_inst().integer((entry_i0 + step * q) as i32));
        let (f_target, f_args) = if test_at_top {
            // B1 reduction: the false edge carries the vector accumulator
            // (the header parameter, re-typed at step 2b) to the reduce
            // block, which reduces it and feeds the scalar sum to the
            // exit (directly, or through the freshly added exit
            // parameter). A runtime-bound elementwise loop enters the
            // scalar tail with `iv0 = i0 + max(cnt0, 0)` — the vector
            // loop advanced the index by `cnt0`, and a negative trip
            // clamps back to `i0` so the tail runs zero iterations.
            // Otherwise the epilogue chain (r > 0) and the exit both
            // carry the loop-invariant passthrough values: their header
            // parameters stay live and unchanged across the vectorized
            // loop, so the entry-edge values are forwarded as-is.
            if let Some(red) = &reduction {
                let mut args = vec![red.acc];
                args.extend(passthrough_args.iter().copied());
                (
                    reduce_block.expect("reduce block built when reduction is set"),
                    args,
                )
            } else if runtime_trip {
                let mut args = vec![iv0.expect("runtime iv0")];
                args.extend(passthrough_args.iter().copied());
                (
                    tail_header.expect("tail built when runtime_trip"),
                    args,
                )
            } else if r > 0 {
                (epi_blocks[0], passthrough_args.clone())
            } else {
                (
                    exit,
                    build_exit_args(&exit_specs, &passthrough_args, None, iv_final_const),
                )
            }
        } else {
            if let Some(modr) = &mod_reduction {
                // Mod-wrapped scalar reduction: the accumulator never leaves
                // the scalar domain, so the false edge carries the final
                // modded value directly (no reduce block).
                if runtime_trip {
                    let mut args = vec![iv0.expect("runtime iv0"), modr.acc_update];
                    args.extend(passthrough_args.iter().copied());
                    (
                        tail_header.expect("tail built when runtime_trip"),
                        args,
                    )
                } else {
                    (
                        exit,
                        build_exit_args(
                            &exit_specs,
                            &passthrough_args,
                            Some(modr.acc_update),
                            iv_final_const,
                        ),
                    )
                }
            } else {
                match &reduction {
                    Some(red) => {
                        let mut args = vec![red.acc_update];
                        args.extend(passthrough_args.iter().copied());
                        (
                            reduce_block.expect("reduce block built when reduction is set"),
                            args,
                        )
                    }
                    None if runtime_trip => {
                        // Rotated runtime-bound elementwise: the false edge
                        // enters the scalar tail at `iv0 = i0 + max(cnt0, 0)`
                        // — the vector loop advanced the index by `cnt0`, and
                        // a negative trip clamps back to `i0` so the tail runs
                        // zero iterations.
                        let mut args = vec![iv0.expect("runtime iv0")];
                        args.extend(passthrough_args.iter().copied());
                        (
                            tail_header.expect("tail built when runtime_trip"),
                            args,
                        )
                    }
                    None if r > 0 => (epi_blocks[0], passthrough_args.clone()),
                    None => {
                        // r == 0: the exit is reached directly; IV-final
                        // parameters are replaced by the computed constant.
                        (
                            exit,
                            build_exit_args(
                                &exit_specs,
                                &passthrough_args,
                                None,
                                iv_final_const,
                            ),
                        )
                    }
                }
            }
        };
        data.replace_inst_with(latch_branch)
            .branch(cond, t_target, t_args, f_target, f_args);
    }

    // 3. Latch machinery: step the IV by the vector width. The loop now
    //    runs `q` iterations because the counter enters at `4Q`. The step
    //    constant stays out of layout (constants are never placed in
    //    blocks; DCE's `is_critical` treats a laid-out Integer as
    //    unreachable). Test-at-top: the materialized counter step is
    //    appended to the latch's plain-jump arguments.
    data.replace_inst_with(iv_next)
        .raw(Binary::new_data(iv, four, BinaryOp::Add, i32.clone()));
    if test_at_top {
        let latch_term = data.layout().basicblock(latch).terminator();
        match data.inst_data(latch_term).kind() {
            InstKind::Jump(j) => {
                let mut args = j.args().to_vec();
                args.push(eff_t_next);
                data.replace_inst_with(latch_term).jump(header, args);
            }
            _ => unreachable!(),
        }
    } else {
        data.replace_inst_with(eff_t_next)
            .raw(Binary::new_data(eff_counter, four, BinaryOp::Sub, i32.clone()));
    }

    // 4. Entry edge: the counter enters at `4Q` (trip % 4 handled by the
    //    epilogue); the index keeps its original initial value; a reduction
    //    accumulator enters splatted across the vector.
    let four_q = data.new_local_inst().integer((step * q) as i32);
    let acc_splat: Option<Inst> = reduction.as_ref().map(|red| {
        // Zero seed: the vector loop accumulates Σc and the seed is added
        // back at the exit (see the reduce-block `acc_value` construction).
        // Splatting a non-zero seed would overcount it 4x at the addv.
        let zero = data.new_local_inst().integer(0);
        let s = alloc_inst(data, VectorSplat::new_data(zero, vector_ty.clone()));
        data.layout_mut().insert_inst_before(entry_edge, s);
        s
    });
    let entry_rewrite = match data.inst_data(entry_edge).kind() {
        InstKind::Branch(b) => {
            let mut t_args = b.t_args().to_vec();
            if test_at_top {
                // The new counter parameter is appended after the IV; a
                // runtime bound enters at `cnt0 = trip & -4` instead of
                // the const `4Q`.
                t_args.push(if runtime_trip { cnt0 } else { four_q });
            } else {
                // The rotated counter is an existing header parameter: its
                // entry value is replaced in place — `cnt0 = t0 & -4` for
                // a runtime trip (the `t0 & 3` remainder runs in the
                // tail), the const `4Q` otherwise.
                t_args[entry_arg_count - 1] = if runtime_trip { cnt0 } else { four_q };
            }
            if let (Some(red), Some(splat)) = (&reduction, &acc_splat) {
                t_args[red.acc_slot] = *splat;
            }
            // The preheader's false edge is the trip-zero guard: when it
            // targets the exit and the exit gained an accumulator
            // parameter (param-less test-at-top reduction), the loop
            // never ran, so the exit receives the seed.
            let mut f_args = b.f_args().to_vec();
            if let (Some(_), Some(red)) = (exit_extra_acc, &reduction) {
                if b.f_target() == exit {
                    f_args.push(red.acc_init);
                }
            }
            EntryRewrite::Branch {
                cond: b.cond(),
                f_target: b.f_target(),
                f_args,
                t_args,
            }
        }
        InstKind::Jump(j) => {
            let mut args = j.args().to_vec();
            if test_at_top {
                // The new counter parameter is appended after the IV; a
                // runtime bound enters at `cnt0 = trip & -4` instead of
                // the const `4Q`.
                args.push(if runtime_trip { cnt0 } else { four_q });
            } else {
                // The rotated counter is an existing header parameter: its
                // entry value is replaced in place — `cnt0 = t0 & -4` for
                // a runtime trip (the `t0 & 3` remainder runs in the
                // tail), the const `4Q` otherwise.
                args[entry_arg_count - 1] = if runtime_trip { cnt0 } else { four_q };
            }
            if let (Some(red), Some(splat)) = (&reduction, &acc_splat) {
                args[red.acc_slot] = *splat;
            }
            if runtime_trip && !test_at_top {
                // Rotated runtime entry guard: the vector loop is a
                // do-while (the latch tests after the body), so an
                // unconditional entry would execute one vector body even
                // when `cnt0 <= 0` — a trip of {1,2,3} (or a negative
                // trip) would read 4 elements past the trip and then
                // double-count the first `trip & 3` elements in the
                // tail. Guard the entry with `gt(cnt0, 0)`: the false
                // edge enters the scalar tail directly at `iv0` with the
                // seed accumulator (the vector loop never ran).
                let zero = data.new_local_inst().integer(0);
                let guard = alloc_inst(
                    data,
                    Binary::new_data(cnt0, zero, BinaryOp::Gt, i32.clone()),
                );
                data.layout_mut().insert_inst_before(entry_edge, guard);
                let mut f_args = vec![iv0.expect("runtime iv0 built")];
                if let Some(red) = &reduction {
                    f_args.push(red.acc_init);
                }
                f_args.extend(passthrough_args.iter().copied());
                EntryRewrite::Branch {
                    cond: guard,
                    f_target: tail_header.expect("tail built when runtime_trip"),
                    f_args,
                    t_args: args,
                }
            } else {
                EntryRewrite::Jump {
                    target: j.target(),
                    args,
                }
            }
        }
        _ => unreachable!(),
    };
    match entry_rewrite {
        EntryRewrite::Branch {
            cond,
            f_target,
            f_args,
            t_args,
        } => {
            data.replace_inst_with(entry_edge)
                .branch(cond, header, t_args, f_target, f_args);
        }
        EntryRewrite::Jump { target, args } => {
            data.replace_inst_with(entry_edge).jump(target, args);
        }
    }

    // 5. B1: drop the arm-selecting branch — the arm's stores are already
    //    mask-rewritten, so the arm can run unconditionally (its
    //    terminator `jump merge` is preserved and becomes the merge's
    //    predecessor).
    if let Some(arm_plan) = &arm {
        data.replace_inst_with(arm_plan.branch).jump(arm_plan.arm, vec![]);
    }

    true
}

/// Milestone 2 phase 3: vectorize a multi-arm masked-accumulate kernel (the
/// conv2d 5×5 shape). The loop's body is a chain of
/// `br mask, then, end(acc)` units; `then` computes
/// `acc' = acc + In[rr][cc] * K[k]` under a boundary mask. Vectorizing:
///   1. The accumulator chain (header carrier param + every `end` block
///      param) is re-typed to `<4 x i32>`, seeded from `splat(0)`.
///   2. Each `then`'s In load becomes a contiguous vector load; the
///      invariant K load is splatted; the mul/add become lane-wise.
///   3. The scalar boundary mask is re-derived lane-wise:
///      `m = (cc_vec >= 0) & (cc_vec < splat(bound)) & splat(rr_ok)`
///      and folded into the add (`acc += mul & m`).
///   4. The latch's Out store carries the vector accumulator; the IV and
///      the step-pointer advance by VF; a materialized counter runs the
///      vector loop; a reduce block horizontally reduces the accumulator
///      for the scalar exit.
/// The scalar tail / epilogue for `trip % VF != 0` and runtime bounds are
/// not wired yet — such loops stay scalar (conservative).
fn apply_multi_arm(data: &mut ArenaContextMut<'_>, plan: VecPlan) -> bool {
    let VecPlan {
        header,
        latch,
        exit,
        entry_edge,
        iv,
        counter: _,
        entry_i0,
        trip,
        bound_inst,
        runtime_trip,
        latch_branch,
        t_next: _,
        iv_next,
        payload,
        classes,
        vector_ty,
        reduction: _,
        mod_reduction: _,
        passthrough_args,
        exit_specs,
        test_at_top,
        arm: _,
        multi_arm,
        unroll: _,
    } = plan;
    let Some(multi) = multi_arm else {
        return false;
    };
    let step = VF;
    let i32 = Type::get_i32();
    if !test_at_top {
        return false;
    }
    // A compile-time trip that is a multiple of VF, or a runtime-bound trip
    // whose `trip & (VF-1)` remainder is covered by the scalar tail loop
    // (`vec_tail_ma`) built in step 6. A runtime-bound loop whose remainder
    // is covered stays vectorized; anything else stays scalar (conservative).
    if trip % step != 0 {
        if std::env::var("M44_TRACE").is_ok() {
            eprintln!("[M44] func={} header={header:?} reject=multi_arm_runtime_unsupported",
                data.name());
        }
        return false;
    }
    let q = trip / step;
    let n_passthrough = passthrough_args.len();
    let has_iv_final = exit_specs.iter().any(|spec| *spec == ExitArgSpec::IvFinal);
    let has_acc = exit_specs.iter().any(|spec| *spec == ExitArgSpec::Acc);
    let i0_inst = data.new_local_inst().integer(entry_i0 as i32);
    // Runtime-bound counter: `trip = bound - i0`, `cnt0 = trip & -4` — the
    // vector loop runs `cnt0 / 4` iterations and the scalar tail covers the
    // `trip & 3` remainder.
    let (counter_entry, iv0): (Inst, Inst) = if runtime_trip {
        let trip_rt = alloc_inst(
            data,
            Binary::new_data(bound_inst, i0_inst, BinaryOp::Sub, i32.clone()),
        );
        data.layout_mut().insert_inst_before(entry_edge, trip_rt);
        let neg_four = data.new_local_inst().integer(-(step as i32));
        let cnt0 = alloc_inst(
            data,
            Binary::new_data(trip_rt, neg_four, BinaryOp::And, i32.clone()),
        );
        data.layout_mut().insert_inst_before(entry_edge, cnt0);
        // `iv0 = i0 + max(cnt0, 0)` — the scalar tail's entry index (a
        // negative trip clamps back to `i0` so the tail runs zero rounds).
        let zero_cmp = data.new_local_inst().integer(0);
        let positive = alloc_inst(
            data,
            Binary::new_data(cnt0, zero_cmp, BinaryOp::Gt, i32.clone()),
        );
        let cnt_clamped = alloc_inst(
            data,
            Select::new_data(positive, cnt0, zero_cmp, i32.clone()),
        );
        data.layout_mut().insert_inst_before(entry_edge, positive);
        data.layout_mut().insert_inst_before(entry_edge, cnt_clamped);
        let start = alloc_inst(
            data,
            Binary::new_data(i0_inst, cnt_clamped, BinaryOp::Add, i32.clone()),
        );
        data.layout_mut().insert_inst_before(entry_edge, start);
        (cnt0, start)
    } else {
        let cq = data.new_local_inst().integer(q as i32);
        (cq, cq)
    };
    let iv_final_const = has_iv_final.then(|| {
        if runtime_trip {
            bound_inst
        } else {
            data.new_local_inst().integer((entry_i0 + step * q) as i32)
        }
    });

    // ---- 1. Trip counter (test-at-top): a new header parameter entered at
    //      `counter_entry` (q, or the runtime `trip & -4`), stepped by
    //      `-step` per vector round, tested at the header in place of the
    //      bound test.
    let four = data.new_local_inst().integer(step as i32);
    let param_index = data.bb_data(header).params().len() + 1;
    let counter_bar = alloc_inst(data, BlockArgRef::new_data(param_index, i32.clone()));
    data.bb_data_mut(header).params_mut().push(counter_bar);
    let counter_next = alloc_inst(
        data,
        Binary::new_data(counter_bar, four, BinaryOp::Sub, i32.clone()),
    );
    let latch_term = data.layout().basicblock(latch).terminator();
    data.layout_mut().insert_inst_before(latch_term, counter_next);

    // ---- 2. Re-type the accumulator chain to the vector type.
    //      The header carrier parameter and every unit's `end` parameter.
    data.inst_data_mut(multi.acc).set_type(vector_ty.clone());
    for unit in &multi.units {
        let end_params: Vec<Inst> = data.bb_data(unit.end).params().to_vec();
        for p in end_params {
            data.inst_data_mut(p).set_type(vector_ty.clone());
        }
    }
    // Unit 0's branch seeds the chain with a compile-time 0; it must become
    // a zero vector (`splat(0)`). Defined in the entry block (it dominates
    // the body_br0 branch and the entry edge's accumulator argument). The
    // then block's `add 0, mul` splats the scalar 0 independently.
    let zero = data.new_local_inst().integer(0);
    let vzero = alloc_inst(data, VectorSplat::new_data(zero, vector_ty.clone()));
    {
        let preheader = data.layout().parent_bb(entry_edge).unwrap();
        let entry_term = data.layout().basicblock(preheader).terminator();
        data.layout_mut().insert_inst_before(entry_term, vzero);
    }
    {
        let (cond, t_target, t_args, f_target, f_args) = {
            let branch = data.layout().basicblock(multi.units[0].body_br).terminator();
            match data.inst_data(branch).kind() {
                InstKind::Branch(b) => (
                    b.cond(),
                    b.t_target(),
                    b.t_args().to_vec(),
                    b.f_target(),
                    b.f_args().to_vec(),
                ),
                _ => unreachable!(),
            }
        };
        let branch = data.layout().basicblock(multi.units[0].body_br).terminator();
        if f_args.len() == 1 {
            let mut f_args = f_args;
            f_args[0] = vzero;
            data.replace_inst_with(branch)
                .branch(cond, t_target, t_args, f_target, f_args);
        }
    }

    // ---- 2.5 Widen each unit's scalar branch guard from "lane 0 in
    //      bounds" to "any lane in bounds". The guard's lower test is
    //      `ge(cc, 0)` with `cc = iv + kc_off` (lane 0's column). A vector
    //      round covers columns `cc .. cc+VF-1`: when `cc < 0` the first
    //      lanes are masked off but higher lanes may still be in bounds, so
    //      the guard must pass when the *highest* lane is non-negative
    //      (`cc + VF - 1 >= 0`) — the per-lane vector mask inside `then`
    //      keeps each masked lane's contribution zero. The upper test
    //      (`cc < bound`) is unchanged: it is the lowest lane's bound, so
    //      any in-bounds lane implies it. Without this the first vector
    //      iteration (iv = 0) drops the kc_off < 0 units' higher lanes.
    for unit in &multi.units {
        let body_term = data.layout().basicblock(unit.body_br).terminator();
        let (t_target, t_args, f_target, f_args) = {
            match data.inst_data(body_term).kind() {
                InstKind::Branch(b) => (
                    b.t_target(),
                    b.t_args().to_vec(),
                    b.f_target(),
                    b.f_args().to_vec(),
                ),
                _ => unreachable!("multi-arm body blocks end in a branch"),
            }
        };
        let vf_m1 = data.new_local_inst().integer(VF as i32 - 1);
        let cc_hi = alloc_inst(
            data,
            Binary::new_data(unit.cc, vf_m1, BinaryOp::Add, i32.clone()),
        );
        let ge_hi = alloc_inst(data, Binary::new_data(cc_hi, zero, BinaryOp::Ge, i32.clone()));
        let lt_b = alloc_inst(
            data,
            Binary::new_data(unit.cc, unit.bound, BinaryOp::Lt, i32.clone()),
        );
        let inner = alloc_inst(data, Binary::new_data(ge_hi, lt_b, BinaryOp::And, i32.clone()));
        let mut cond = inner;
        if let Some(rr) = unit.rr_ok {
            cond = alloc_inst(data, Binary::new_data(cond, rr, BinaryOp::And, i32.clone()));
        }
        data.layout_mut().insert_inst_before(body_term, cc_hi);
        data.layout_mut().insert_inst_before(body_term, ge_hi);
        data.layout_mut().insert_inst_before(body_term, lt_b);
        data.layout_mut().insert_inst_before(body_term, inner);
        data.layout_mut().insert_inst_before(body_term, cond);
        data.replace_inst_with(body_term)
            .branch(cond, t_target, t_args, f_target, f_args);
    }

    // ---- 3. Body vectorization.
    //      The lane counter `iv_vec = splat(iv) + [0,1,2,3]` drives the
    //      mask columns; shared by every unit. It must be defined in the
    //      first unit's *body_br* (the chain's common dominator): the units'
    //      `then` blocks are reachable around each other via the mask's
    //      false edge (`br mask, then, end`), so no single `then` dominates
    //      the rest.
    let body_br0_term = data.layout().basicblock(multi.units[0].body_br).terminator();
    let iv_splat = alloc_inst(data, VectorSplat::new_data(iv, vector_ty.clone()));
    data.layout_mut().insert_inst_before(body_br0_term, iv_splat);
    let mut lane_off = alloc_inst(data, VectorSplat::new_data(zero, vector_ty.clone()));
    data.layout_mut().insert_inst_before(body_br0_term, lane_off);
    for k in 1..VF as i64 {
        let c = data.new_local_inst().integer(k as i32);
        let idx = data.new_local_inst().integer(k as i32);
        lane_off = alloc_inst(
            data,
            VectorInsertElement::new_data(lane_off, c, idx, vector_ty.clone()),
        );
        data.layout_mut().insert_inst_before(body_br0_term, lane_off);
    }
    let iv_vec = alloc_inst(
        data,
        Binary::new_data(iv_splat, lane_off, BinaryOp::Add, vector_ty.clone()),
    );
    data.layout_mut().insert_inst_before(body_br0_term, iv_vec);

    for unit in &multi.units {
        // load_in: contiguous vector load (address stays scalar).
        let load_src = match data.inst_data(unit.load_in).kind() {
            InstKind::Load(l) => l.src(),
            _ => unreachable!(),
        };
        data.replace_inst_with(unit.load_in)
            .raw(Load::new_data(load_src, vector_ty.clone()));
        // mul: lane-wise. The invariant K splat must be defined *before* the
        // mul (anchor on the mul itself, not the later add).
        let vl = vector_operand(
            data,
            &mut FxHashMap::default(),
            unit.load_in,
            &payload,
            &classes,
            &vector_ty,
            unit.mul,
        );
        let vr = vector_operand(
            data,
            &mut FxHashMap::default(),
            unit.load_k,
            &payload,
            &classes,
            &vector_ty,
            unit.mul,
        );
        data.replace_inst_with(unit.mul)
            .raw(Binary::new_data(vl, vr, BinaryOp::Mul, vector_ty.clone()));
        // Mask: `m = (cc_vec >= 0) & (cc_vec < splat(bound)) &
        // splat(rr_ok)` with `cc_vec = iv_vec + splat(kc_off)`.
        let kc = data.new_local_inst().integer(unit.kc_off as i32);
        let kc_splat = alloc_inst(data, VectorSplat::new_data(kc, vector_ty.clone()));
        data.layout_mut().insert_inst_before(unit.add, kc_splat);
        let cc_vec = alloc_inst(
            data,
            Binary::new_data(iv_vec, kc_splat, BinaryOp::Add, vector_ty.clone()),
        );
        data.layout_mut().insert_inst_before(unit.add, cc_vec);
        let vzero_cmp = vector_operand(
            data,
            &mut FxHashMap::default(),
            zero,
            &payload,
            &classes,
            &vector_ty,
            unit.add,
        );
        let ge0 = alloc_inst(
            data,
            Binary::new_data(cc_vec, vzero_cmp, BinaryOp::Ge, vector_ty.clone()),
        );
        data.layout_mut().insert_inst_before(unit.add, ge0);
        let bound_splat = vector_operand(
            data,
            &mut FxHashMap::default(),
            unit.bound,
            &payload,
            &classes,
            &vector_ty,
            unit.add,
        );
        let lt_b = alloc_inst(
            data,
            Binary::new_data(cc_vec, bound_splat, BinaryOp::Lt, vector_ty.clone()),
        );
        data.layout_mut().insert_inst_before(unit.add, lt_b);
        let mut mask = alloc_inst(
            data,
            Binary::new_data(ge0, lt_b, BinaryOp::And, vector_ty.clone()),
        );
        data.layout_mut().insert_inst_before(unit.add, mask);
        if let Some(rr) = unit.rr_ok {
            let rr_splat = vector_operand(
                data,
                &mut FxHashMap::default(),
                rr,
                &payload,
                &classes,
                &vector_ty,
                unit.add,
            );
            // `rr_ok` is a scalar 0/1 (a `neq(..., 0)` result). Splatting it
            // gives `{0 | 1}` per lane, but the vector mask needs all-ones
            // (`-1`) so `mul & mask` keeps full lanes: `rr != 0` re-derives
            // the boolean as an all-ones mask on the vector lanes.
            let zero_splat = vector_operand(
                data,
                &mut FxHashMap::default(),
                zero,
                &payload,
                &classes,
                &vector_ty,
                unit.add,
            );
            let rr_bool = alloc_inst(
                data,
                Binary::new_data(rr_splat, zero_splat, BinaryOp::NotEq, vector_ty.clone()),
            );
            data.layout_mut().insert_inst_before(unit.add, rr_bool);
            mask = alloc_inst(
                data,
                Binary::new_data(mask, rr_bool, BinaryOp::And, vector_ty.clone()),
            );
            data.layout_mut().insert_inst_before(unit.add, mask);
        }
        // add: `acc + (mul & mask)` — masked accumulation.
        let mul_masked = alloc_inst(
            data,
            Binary::new_data(unit.mul, mask, BinaryOp::And, vector_ty.clone()),
        );
        data.layout_mut().insert_inst_before(unit.add, mul_masked);
        let acc_v = vector_operand(
            data,
            &mut FxHashMap::default(),
            unit.acc_in,
            &payload,
            &classes,
            &vector_ty,
            unit.add,
        );
        data.replace_inst_with(unit.add)
            .raw(Binary::new_data(acc_v, mul_masked, BinaryOp::Add, vector_ty.clone()));
    }

    // The scalar tail (runtime trips) references each unit's row-bound
    // `rr_ok` and the In access's invariant row offset — both of which the
    // compiler may have placed inside the loop body. That would leave the
    // loop-external tail with a non-dominating definition (and the vector
    // loop may run zero iterations, so the body never executes). Promote
    // the invariant chains to the header.
    for unit in &multi.units {
        if let Some(rr) = unit.rr_ok {
            promote_body_invariant(data, rr, &multi.units, header, iv);
        }
        // The In offset's invariant row term (`invariant_part + cc`).
        if let InstKind::Load(l) = data.inst_data(unit.load_in).kind() {
            if let InstKind::GetElemPtr(g) = data.inst_data(l.src()).kind() {
                if let Some(&off) = g.offsets().last() {
                    if let InstKind::Binary(b) = data.inst_data(off).kind() {
                        if b.op() == BinaryOp::Add {
                            let (lhs, rhs) = (b.lhs(), b.rhs());
                            let lhs_iv = offset_contains_iv(data, lhs, iv);
                            let rhs_iv = offset_contains_iv(data, rhs, iv);
                            match (lhs_iv, rhs_iv) {
                                (true, false) => {
                                    promote_body_invariant(data, rhs, &multi.units, header, iv);
                                }
                                (false, true) => {
                                    promote_body_invariant(data, lhs, &multi.units, header, iv);
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }

    // ---- 4. Step the IV and the accumulator pointer by VF; append the
    //      counter to the latch's back jump. The latch's Out store already
    //      carries the (re-typed) vector accumulator — no rewrite needed.
    data.replace_inst_with(iv_next)
        .raw(Binary::new_data(iv, four, BinaryOp::Add, i32.clone()));
    let latch_term = data.layout().basicblock(latch).terminator();
    match data.inst_data(latch_term).kind() {
        InstKind::Jump(j) => {
            let mut args = j.args().to_vec();
            // The step pointer advances by VF elements (it stepped by one
            // per scalar iteration).
            for (slot, unit) in multi.units.iter().enumerate() {
                let _ = unit;
                if slot == 0 {
                    break;
                }
            }
            // Find and rewrite the step-pointer back argument (the header
            // slot whose back arg is `getelemptr(ptr, 1)`).
            let header_params = data.bb_data(header).params().to_vec();
            let back_slots: Vec<usize> = (0..header_params.len()).collect();
            for slot in back_slots {
                let p = header_params[slot];
                if !data.inst_data(p).ty().is_pointer() {
                    continue;
                }
                if let Some(a) = args.get(slot).copied() {
                    if let InstKind::GetElemPtr(g) = data.inst_data(a).kind() {
                        if g.base() == p {
                            let step_off = data.new_local_inst().integer(VF as i32);
                            let gep = alloc_inst(
                                data,
                                GetElemPtr::new_data(p, vec![step_off], data.inst_data(p).ty().clone()),
                            );
                            data.layout_mut().insert_inst_before(latch_term, gep);
                            args[slot] = gep;
                        }
                    }
                }
            }
            args.push(counter_next);
            data.replace_inst_with(latch_term).jump(header, args);
        }
        _ => unreachable!(),
    }

    // ---- 5. Entry edge: seed the accumulator carrier with `splat(0)` and
    //      enter the counter at `counter_entry` (q or the runtime `trip&-4`).
    let entry_kind = data.inst_data(entry_edge).kind().clone();
    match entry_kind {
        InstKind::Jump(j) => {
            let mut args = j.args().to_vec();
            // The accumulator carrier slot's entry argument (the outer
            // loop's r value) is irrelevant to the body chain — it is
            // replaced by a zero vector so the re-typed header parameter
            // type-checks. The body chain seeds from `acc_init` (0).
            if multi.acc_slot < args.len() {
                args[multi.acc_slot] = vzero;
            }
            args.push(counter_entry);
            data.replace_inst_with(entry_edge).jump(j.target(), args);
        }
        _ => unreachable!(),
    }

    // ---- 6. Reduce block + (runtime) scalar tail + exit. The header's
    //      bound test becomes the counter test; its false edge enters a
    //      reduce block that horizontally reduces the vector accumulator.
    //      A runtime trip then enters a scalar tail loop that continues the
    //      accumulation over the `trip & 3` remainder; a const trip jumps
    //      straight to the exit.
    let mut reduce_param_tys = vec![i32.clone(); n_passthrough];
    reduce_param_tys.insert(0, vector_ty.clone());
    let reduce_block = data
        .new_basic_block()
        .basic_block("vec_reduce_ma".into(), reduce_param_tys);
    data.layout_mut().insert_bb_after(latch, reduce_block);
    let acc_v_param = data.bb_data(reduce_block).params()[0];
    let acc_scalar = alloc_inst(
        data,
        VectorReduce::new_data(VectorReduceOp::Add, acc_v_param, i32.clone()),
    );
    data.layout_mut().insert_inst(reduce_block, acc_scalar);
    // The tail's scalar accumulator parameter (runtime trips), resolved after
    // the tail loop is built; used to rewrite the exit terminator's reads.
    let mut tail_acc: Option<Inst> = None;
    let (reduce_target, reduce_args): (BasicBlock, Vec<Inst>) = if runtime_trip {
        // Build the scalar tail loop (below) and enter it at `iv0` with a
        // zero accumulator seed — each remainder column's sum is independent
        // (the vector rounds do not feed the tail). The tail's Out writes
        // mirror the latch's Out store (base + iv-t-indexed offsets).
        // The tail's Out write mirrors the latch's Out store: `base +
        // (invariant_row_off + iv_t)`.
        let (out_base, out_row_off) = {
            let lt = data.layout().basicblock(latch);
            let store_dest = lt
                .insts()
                .iter()
                .copied()
                .find_map(|i| match data.inst_data(i).kind() {
                    InstKind::Store(s) => Some(s.dest()),
                    _ => None,
                })
                .expect("a multi-arm latch stores the accumulator");
            let (base, offsets) = match data.inst_data(store_dest).kind() {
                InstKind::GetElemPtr(g) => (g.base(), g.offsets().to_vec()),
                _ => unreachable!(),
            };
            let off = *offsets.last().unwrap();
            let inv = match data.inst_data(off).kind() {
                InstKind::Binary(b) if b.op() == BinaryOp::Add => {
                    let (l, r) = (b.lhs(), b.rhs());
                    let l_iv = offset_contains_iv(data, l, iv);
                    let r_iv = offset_contains_iv(data, r, iv);
                    match (l_iv, r_iv) {
                        (true, false) => r,
                        (false, true) => l,
                        _ => off,
                    }
                }
                _ => off,
            };
            (base, inv)
        };
        let tail_h = build_multi_arm_tail(
            data,
            &multi,
            &exit_specs,
            &passthrough_args,
            bound_inst,
            out_base,
            out_row_off,
            entry_i0,
            exit,
            reduce_block,
            has_acc,
            has_iv_final,
            n_passthrough,
        );
        let mut fwd = vec![iv0, zero];
        fwd.extend(passthrough_args.iter().copied());
        tail_acc = Some(data.bb_data(tail_h).params()[1]);
        (tail_h, fwd)
    } else {
        let exit_args = build_exit_args(
            &exit_specs,
            &passthrough_args,
            has_acc.then_some(acc_scalar),
            iv_final_const,
        );
        (exit, exit_args)
    };
    let reduce_jump = data.new_local_inst().jump(reduce_target, reduce_args);
    data.layout_mut().insert_inst(reduce_block, reduce_jump);
    // The exit block has no parameters (its terminator jumps to the outer
    // loop), but it reads the header IV and accumulator by SSA dominance.
    // Both are now vector / stale: rewrite the terminator's references to
    // the scalar final values (the reduced accumulator — defined in the
    // reduce block, or the tail's scalar accumulator — and the IV final).
    {
        let exit_term = data.layout().basicblock(exit).terminator();
        let mut uses_acc = false;
        let mut uses_iv = false;
        if let InstKind::Jump(j) = data.inst_data(exit_term).kind() {
            uses_acc = j.args().contains(&multi.acc);
            uses_iv = j.args().contains(&iv);
        }
        if uses_acc {
            let acc_v = match tail_acc {
                Some(t) => t,
                None => acc_scalar,
            };
            subst_operand(data, exit_term, multi.acc, acc_v);
        }
        if uses_iv {
            let iv_v = if runtime_trip {
                bound_inst
            } else {
                data.new_local_inst().integer((entry_i0 + step * q) as i32)
            };
            subst_operand(data, exit_term, iv, iv_v);
        }
    }

    let header_term = data.layout().basicblock(header).terminator();
    if let InstKind::Branch(_) = data.inst_data(header_term).kind() {
        let counter_test = if runtime_trip {
            // `gt(counter, 0)`: a negative `cnt0` (negative trip) must not
            // spin the vector loop forever.
            let z = data.new_local_inst().integer(0);
            alloc_inst(
                data,
                Binary::new_data(counter_bar, z, BinaryOp::Gt, i32.clone()),
            )
        } else {
            alloc_inst(
                data,
                Binary::new_data(counter_bar, zero, BinaryOp::NotEq, i32.clone()),
            )
        };
        data.layout_mut().insert_inst_before(header_term, counter_test);
        let body_target = multi.units[0].body_br;
        let mut reduce_args = vec![multi.acc];
        reduce_args.extend(passthrough_args.iter().copied());
        data.replace_inst_with(header_term).branch(
            counter_test,
            body_target,
            vec![],
            reduce_block,
            reduce_args,
        );
    }

    let _ = latch_branch;
    true
}

/// Promote the loop-invariant chain rooted at `root` — instructions defined
/// in the multi-arm body blocks (a row-bound `rr_ok` scalar, whose
/// definition the compiler placed inside the loop) — into the header, so the
/// loop-external scalar tail can reference them. Returns None when any
/// body-defined dependency references the IV (not invariant).
fn promote_body_invariant(
    data: &mut ArenaContextMut<'_>,
    root: Inst,
    units: &[MultiArmUnit],
    header: BasicBlock,
    iv: Inst,
) -> Option<Inst> {
    let mut ordered: Vec<Inst> = Vec::new();
    let mut seen: FxHashSet<Inst> = FxHashSet::default();
    fn visit(
        data: &FunctionData,
        inst: Inst,
        units: &[MultiArmUnit],
        iv: Inst,
        ordered: &mut Vec<Inst>,
        seen: &mut FxHashSet<Inst>,
    ) -> bool {
        if !seen.insert(inst) {
            return true;
        }
        let in_body = data.layout().parent_bb(inst).is_some_and(|bb| {
            units
                .iter()
                .any(|u| u.body_br == bb || u.then == bb || u.end == bb)
        });
        if !in_body {
            return true;
        }
        if offset_contains_iv(data, inst, iv) {
            return false;
        }
        for dep in data.inst_data(inst).inst_usage() {
            if !visit(data, dep, units, iv, ordered, seen) {
                return false;
            }
        }
        ordered.push(inst);
        true
    }
    if !visit(data, root, units, iv, &mut ordered, &mut seen) {
        return None;
    }
    let in_loop_blocks: Vec<(BasicBlock, Inst)> = ordered
        .iter()
        .filter_map(|&inst| data.layout().parent_bb(inst).map(|bb| (bb, inst)))
        .collect();
    for (bb, inst) in in_loop_blocks {
        data.layout_mut().remove_inst(bb, inst);
    }
    let header_term = data.layout().basicblock(header).terminator();
    for inst in ordered {
        data.layout_mut().insert_inst_before(header_term, inst);
    }
    Some(root)
}

/// Build the scalar tail loop for a multi-arm kernel over the `trip & 3`
/// remainder (`while (iv < bound) { acc = 0; acc op= mask·In·K; Out[iv] =
/// acc; iv++ }`). Each column's sum starts fresh from 0 — the accumulator
/// is re-seeded on the entry edge and reset on every back edge.
/// The tail's Out write mirrors the latch's store at `base + (row_off +
/// iv_t)`. Returns the tail header.
#[allow(clippy::too_many_arguments)]
fn build_multi_arm_tail(
    data: &mut ArenaContextMut<'_>,
    multi: &MultiArmPlan,
    exit_specs: &[ExitArgSpec],
    passthrough_args: &[Inst],
    bound: Inst,
    out_base: Inst,
    out_row_off: Inst,
    i0: i64,
    exit: BasicBlock,
    after: BasicBlock,
    has_acc: bool,
    has_iv_final: bool,
    n_passthrough: usize,
) -> BasicBlock {
    let i32 = Type::get_i32();
    let mut param_tys = vec![i32.clone(); 1 + n_passthrough];
    param_tys.insert(1, i32.clone());
    let tail_h = data
        .new_basic_block()
        .basic_block("vec_tail_ma".into(), param_tys);
    let tail_l = data
        .new_basic_block()
        .basic_block("vec_tail_ma_latch".into(), vec![]);
    data.layout_mut().insert_bb_after(after, tail_h);
    data.layout_mut().insert_bb_after(tail_h, tail_l);
    let iv_t = data.bb_data(tail_h).params()[0];
    let acc_t = data.bb_data(tail_h).params()[1];
    let cond = alloc_inst(data, Binary::new_data(iv_t, bound, BinaryOp::Lt, i32.clone()));
    let exit_args = build_exit_args(
        exit_specs,
        passthrough_args,
        has_acc.then_some(acc_t),
        has_iv_final.then_some(bound),
    );
    let tail_br = data
        .new_local_inst()
        .branch(cond, tail_l, vec![], exit, exit_args);
    data.layout_mut().insert_inst(tail_h, cond);
    data.layout_mut().insert_inst(tail_h, tail_br);

    // Tail latch: straight-line masked multiply-accumulate chain.
    let mut insts: Vec<Inst> = Vec::new();
    let one = data.new_local_inst().integer(1);
    let zero = data.new_local_inst().integer(0);
    let i0_inst = data.new_local_inst().integer(i0 as i32);
    let mut acc_cur = acc_t;
    for unit in &multi.units {
        let kc = data.new_local_inst().integer(unit.kc_off as i32);
        let cc_t = alloc_inst(data, Binary::new_data(iv_t, kc, BinaryOp::Add, i32.clone()));
        insts.push(cc_t);
        let ge0 = alloc_inst(data, Binary::new_data(cc_t, zero, BinaryOp::Ge, i32.clone()));
        insts.push(ge0);
        let lt = alloc_inst(data, Binary::new_data(cc_t, bound, BinaryOp::Lt, i32.clone()));
        insts.push(lt);
        let mut m = alloc_inst(data, Binary::new_data(ge0, lt, BinaryOp::And, i32.clone()));
        insts.push(m);
        if let Some(rr) = unit.rr_ok {
            m = alloc_inst(data, Binary::new_data(m, rr, BinaryOp::And, i32.clone()));
            insts.push(m);
        }
        // In[rr][cc] = *(In_base + off), off = row_off + cc_t.
        let (in_base, off) = match data.inst_data(unit.load_in).kind() {
            InstKind::Load(l) => match data.inst_data(l.src()).kind() {
                InstKind::GetElemPtr(g) => (
                    g.base(),
                    g.offsets().last().copied().unwrap_or(unit.cc),
                ),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };
        let inv_part: Inst = match data.inst_data(off).kind() {
            InstKind::Binary(b) if b.op() == BinaryOp::Add => {
                let (l, r) = (b.lhs(), b.rhs());
                if l == unit.cc {
                    r
                } else if r == unit.cc {
                    l
                } else {
                    off
                }
            }
            _ => off,
        };
        let off_t = alloc_inst(data, Binary::new_data(inv_part, cc_t, BinaryOp::Add, i32.clone()));
        insts.push(off_t);
        let addr_t = alloc_inst(
            data,
            GetElemPtr::new_data(in_base, vec![off_t], data.inst_data(in_base).ty().clone()),
        );
        insts.push(addr_t);
        let ld_in = alloc_inst(data, Load::new_data(addr_t, i32.clone()));
        insts.push(ld_in);
        // K[k].
        let (k_base, k_off) = match data.inst_data(unit.load_k).kind() {
            InstKind::Load(l) => match data.inst_data(l.src()).kind() {
                InstKind::GetElemPtr(g) => (
                    g.base(),
                    g.offsets().last().copied().unwrap(),
                ),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };
        let k_addr = alloc_inst(
            data,
            GetElemPtr::new_data(k_base, vec![k_off], data.inst_data(k_base).ty().clone()),
        );
        insts.push(k_addr);
        let ld_k = alloc_inst(data, Load::new_data(k_addr, i32.clone()));
        insts.push(ld_k);
        let mul = alloc_inst(data, Binary::new_data(ld_in, ld_k, BinaryOp::Mul, i32.clone()));
        insts.push(mul);
        let delta = alloc_inst(data, Select::new_data(m, mul, zero, i32.clone()));
        insts.push(delta);
        acc_cur = alloc_inst(data, Binary::new_data(acc_cur, delta, BinaryOp::Add, i32.clone()));
        insts.push(acc_cur);
    }
    // Out[iv_t] = acc: `out_base + (out_row_off + iv_t)`.
    let out_off = alloc_inst(
        data,
        Binary::new_data(out_row_off, iv_t, BinaryOp::Add, i32.clone()),
    );
    insts.push(out_off);
    let out_addr = alloc_inst(
        data,
        GetElemPtr::new_data(out_base, vec![out_off], data.inst_data(out_base).ty().clone()),
    );
    insts.push(out_addr);
    let out_store = alloc_inst(data, Store::new_data(acc_cur, out_addr));
    insts.push(out_store);
    let iv_t_next = alloc_inst(data, Binary::new_data(iv_t, one, BinaryOp::Add, i32.clone()));
    insts.push(iv_t_next);
    let mut back_args: Vec<Inst> = vec![iv_t_next, zero];
    back_args.extend(passthrough_args.iter().copied());
    insts.push(data.new_local_inst().jump(tail_h, back_args));
    for inst in insts {
        data.layout_mut().insert_inst(tail_l, inst);
    }
    tail_h
}

/// Whether a GEP offset expression references the index IV (recursing the
/// add/sub tree); the multi-arm tail uses it to split an Out offset into its
/// invariant row term and the IV term.
fn offset_contains_iv(data: &FunctionData, inst: Inst, iv: Inst) -> bool {
    if inst == iv {
        return true;
    }
    match data.inst_data(inst).kind() {
        InstKind::Binary(b) => {
            offset_contains_iv(data, b.lhs(), iv) || offset_contains_iv(data, b.rhs(), iv)
        }
        _ => false,
    }
}

/// Allocate a local instruction through the mutable arena context. A
/// `FunctionData`-rooted builder cannot touch global operands
/// (`Arena::global_mut` is unimplemented there), and the epilogue clones
/// reference global GEP bases, so allocation goes through the context.
fn alloc_inst(data: &mut ArenaContextMut<'_>, inst_data: crate::ir::instruction::InstData) -> Inst {
    let mut lb = LocalBuilder {
        arena: &mut *data as &mut dyn Arena,
    };
    lb.raw(inst_data)
}

/// 2x unroll the vectorized payload by REBUILDING the latch body in the
/// order that lets the post-RA pair_combine fuse the two loads/stores of
/// each array into `ldp q` / `stp q` and the pre-RA peephole fuse both
/// mul+add pairs into `mla`:
///
/// ```text
///   [geps + continuation geps]
///   [load0, load1] per array        ← the two loads are adjacent
///   [copy1 binaries in order]       ← each mul immediately precedes its add
///   [copy2 binaries in order]
///   [store0, store1] per array      ← the two stores are adjacent
/// ```
///
/// The second copy's loads/stores address the +16-byte element via a shared
/// constant continuation GEP `getelemptr %base, 4` (folded to `[x, #16]`).
/// The counter and IV step by `step` (= 8) instead of 4; the scalar tail /
/// epilogue cover the `trip & 7` remainder.
fn unroll_payload_2x(
    data: &mut ArenaContextMut<'_>,
    latch: BasicBlock,
    payload: &[Inst],
    classes: &FxHashMap<Inst, Class>,
    vector_ty: &Type,
    latch_branch: Inst,
    t_next: Inst,
    iv_next: Inst,
) {
    // Base address GEP -> shared continuation GEP `getelemptr %base, 4`.
    let mut cont_of: FxHashMap<Inst, Inst> = FxHashMap::default();
    // Payload instruction -> its clone.
    let mut clone_of: FxHashMap<Inst, Inst> = FxHashMap::default();
    let four = data.new_local_inst().integer(4);

    // Resolve a clone operand: payload instructions map to their clones;
    // loop-invariant values (splats, constants, globals) are shared as-is.
    let resolve = |clone_of: &FxHashMap<Inst, Inst>, operand: Inst| -> Inst {
        clone_of.get(&operand).copied().unwrap_or(operand)
    };

    // Continuation GEP per distinct address GEP (created on demand, shared
    // between the load and store clones of the same array).
    let ensure_cont = |data: &mut ArenaContextMut<'_>,
                       cont_of: &mut FxHashMap<Inst, Inst>,
                       base: Inst|
     -> Inst {
        *cont_of.entry(base).or_insert_with(|| {
            let ty = data.inst_data(base).ty().clone();
            alloc_inst(data, GetElemPtr::new_data(base, vec![four], ty))
        })
    };

    // Partition the latch body: payload (by class), loop-invariant splat
    // helpers (created by `vector_operand` during step 2), and the three
    // machinery instructions (kept at the tail).
    let latch_insts: Vec<Inst> = data.layout().basicblock(latch).insts().iter().copied().collect();
    let payload_set: FxHashSet<Inst> = payload.iter().copied().collect();
    let mut geps: Vec<Inst> = Vec::new();
    let mut loads: Vec<Inst> = Vec::new();
    let mut bins: Vec<Inst> = Vec::new();
    let mut stores: Vec<Inst> = Vec::new();
    let mut helpers: Vec<Inst> = Vec::new();
    for &inst in &latch_insts {
        if inst == latch_branch || inst == t_next || inst == iv_next {
            continue;
        }
        if payload_set.contains(&inst) {
            match classes.get(&inst) {
                Some(Class::Keep) => geps.push(inst),
                Some(Class::VecLoad) => loads.push(inst),
                Some(Class::VecBinary) | Some(Class::VecSelect) => bins.push(inst),
                Some(Class::VecStore) => stores.push(inst),
                Some(Class::InvLoad) => {}
                None => unreachable!("every payload inst is classified"),
            }
        } else {
            helpers.push(inst); // loop-invariant splat / constant
        }
    }
    for &inst in &latch_insts {
        if inst != latch_branch && inst != t_next && inst != iv_next {
            data.layout_mut().remove_inst(latch, inst);
        }
    }

    let mut insert = |data: &mut ArenaContextMut<'_>, inst: Inst| {
        data.layout_mut().insert_inst_before(iv_next, inst);
    };

    // 1. GEPs (originals) then continuation GEPs for every load src / store
    //    dest, so each continuation dominates its clones.
    for &gep in &geps {
        insert(data, gep);
    }
    for &load in &loads {
        let InstKind::Load(ld) = data.inst_data(load).kind() else { unreachable!() };
        ensure_cont(data, &mut cont_of, ld.src());
    }
    for &store in &stores {
        let InstKind::Store(st) = data.inst_data(store).kind() else { unreachable!() };
        ensure_cont(data, &mut cont_of, st.dest());
    }
    for &cont in cont_of.values() {
        insert(data, cont);
    }

    // 2. Loads: `[load0, load1]` per array — the clone reads the
    //    continuation GEP so the pair shares the base register.
    for &load in &loads {
        let InstKind::Load(ld) = data.inst_data(load).kind() else { unreachable!() };
        let cont = cont_of[&ld.src()];
        let clone = alloc_inst(data, Load::new_data(cont, vector_ty.clone()));
        clone_of.insert(load, clone);
        insert(data, load);
        insert(data, clone);
    }

    // 3. Loop-invariant splat helpers (defined before every binary that uses
    //    them).
    for &helper in &helpers {
        insert(data, helper);
    }

    // 4. Binaries: all copy-1 binaries (in original order), then all copy-2
    //    clones — each mul is immediately followed by its add, so both fuse
    //    into `mla`.
    for &bin in &bins {
        insert(data, bin);
    }
    for &bin in &bins {
        let kind = data.inst_data(bin).kind().clone();
        let clone = match kind {
            InstKind::Binary(binary) => alloc_inst(
                data,
                Binary::new_data(
                    resolve(&clone_of, binary.lhs()),
                    resolve(&clone_of, binary.rhs()),
                    binary.op(),
                    vector_ty.clone(),
                ),
            ),
            InstKind::Select(select) => alloc_inst(
                data,
                Select::new_data(
                    resolve(&clone_of, select.cond()),
                    resolve(&clone_of, select.if_true()),
                    resolve(&clone_of, select.if_false()),
                    vector_ty.clone(),
                ),
            ),
            _ => unreachable!("binaries are Binary or Select"),
        };
        clone_of.insert(bin, clone);
        insert(data, clone);
    }

    // 5. Stores: `[store0, store1]` per array — the clone writes the
    //    continuation GEP, adjacent to the original store.
    for &store in &stores {
        let InstKind::Store(st) = data.inst_data(store).kind() else { unreachable!() };
        let cont = cont_of[&st.dest()];
        let clone = alloc_inst(data, Store::new_data(resolve(&clone_of, st.src()), cont));
        clone_of.insert(store, clone);
        insert(data, store);
        insert(data, clone);
    }
}

/// Build the exit edge's argument list from the exit's parameter classes
/// (in exit-parameter order): the reduced accumulator, loop-invariant
/// passthrough values, and the computed IV final value.
fn build_exit_args(
    exit_specs: &[ExitArgSpec],
    passthrough_args: &[Inst],
    acc_value: Option<Inst>,
    iv_final_value: Option<Inst>,
) -> Vec<Inst> {
    exit_specs
        .iter()
        .map(|spec| match spec {
            ExitArgSpec::Acc => {
                acc_value.expect("Acc exit spec requires the reduced accumulator")
            }
            ExitArgSpec::Passthrough(slot) => passthrough_args[*slot],
            ExitArgSpec::IvFinal => {
                iv_final_value.expect("IvFinal exit spec requires the computed final value")
            }
        })
        .collect()
}

/// Owned snapshot of the entry edge, taken before its in-place rewrite.
enum EntryRewrite {
    Branch {
        cond: Inst,
        f_target: BasicBlock,
        f_args: Vec<Inst>,
        t_args: Vec<Inst>,
    },
    Jump {
        target: BasicBlock,
        args: Vec<Inst>,
    },
}

/// Resolve one operand of a vector operation: an already-rewritten vector
/// value, or an invariant scalar promoted with a `VectorSplat` (created
/// immediately before `anchor` so layout keeps def-before-use).
fn vector_operand(
    data: &mut ArenaContextMut<'_>,
    splats: &mut FxHashMap<Inst, Inst>,
    operand: Inst,
    payload: &[Inst],
    classes: &FxHashMap<Inst, Class>,
    vector_ty: &Type,
    anchor: Inst,
) -> Inst {
    let is_payload = payload.iter().any(|&p| p == operand);
    if is_payload {
        match classes.get(&operand) {
            Some(Class::VecLoad) | Some(Class::VecBinary) => return operand,
            _ => {}
        }
    }
    // Already a vector value (e.g. the B1 accumulator, re-typed in the
    // mutation phase): use it directly rather than splatting.
    if data.inst_data(operand).ty().is_vector() {
        return operand;
    }
    if let Some(&splat) = splats.get(&operand) {
        return splat;
    }
    let splat = alloc_inst(data, VectorSplat::new_data(operand, vector_ty.clone()));
    splats.insert(operand, splat);
    data.layout_mut().insert_inst_before(anchor, splat);
    splat
}

/// B1 arm-store masking for the scalar tail loop: the arm's conditional
/// store is cloned as `store select(eq(mask_cond, 0), new, old), dest`
/// with `old` taken from the same-address body store's src (B3b), so the
/// linear tail block needs no branch.
struct ArmMask<'a> {
    arm_bb: BasicBlock,
    mask_cond: Inst,
    old_src: &'a FxHashMap<Inst, Inst>,
}

/// Clone one payload instruction into an epilogue block, substituting the
/// index IV with a constant. In-loop operands resolve through `map` (the
/// payload is cloned in def-before-use order); constants are re-created per
/// block; globals and dominating values are reused. When `arm_mask` is
/// Some, a store living in the B1 arm is cloned as a scalar masked store
/// (the tail has no branch for the arm condition).
fn clone_payload_inst(
    data: &mut ArenaContextMut<'_>,
    orig: Inst,
    map: &mut FxHashMap<Inst, Inst>,
    iv: Inst,
    subst_iv: Inst,
    arm_mask: Option<&ArmMask<'_>>,
    out: &mut Vec<Inst>,
) -> Inst {
    let (kind_snapshot, ty) = {
        let orig_data = data.inst_data(orig);
        (orig_data.kind().clone(), orig_data.ty().clone())
    };
    let cloned_data = match &kind_snapshot {
        InstKind::GetElemPtr(gep) => GetElemPtr::new_data(
            map_operand(data, gep.base(), map, iv, subst_iv),
            gep.offsets()
                .iter()
                .map(|&offset| map_operand(data, offset, map, iv, subst_iv))
                .collect(),
            ty,
        ),
        InstKind::Load(load) => Load::new_data(
            map_operand(data, load.src(), map, iv, subst_iv),
            ty,
        ),
        InstKind::Store(store) => {
            let dest = map_operand(data, store.dest(), map, iv, subst_iv);
            if let Some(arm) = arm_mask {
                if data.layout().parent_bb(orig) == Some(arm.arm_bb) {
                    // B1 scalar masked store:
                    // `store select(eq(mask_cond, 0), new, old), dest`.
                    // `new` is the arm's value; `old` is the same-address
                    // body store's src (cloned earlier — the payload is
                    // cloned in def-before-use order) or the IV itself.
                    let new = map_operand(data, store.src(), map, iv, subst_iv);
                    let old_src = arm.old_src.get(&orig).copied().unwrap_or(iv);
                    let old = map_operand(data, old_src, map, iv, subst_iv);
                    let cond = map.get(&arm.mask_cond).copied().unwrap_or(arm.mask_cond);
                    let zero = data.new_local_inst().integer(0);
                    let eq0 = alloc_inst(
                        data,
                        Binary::new_data(cond, zero, BinaryOp::Eq, ty.clone()),
                    );
                    let sel = alloc_inst(data, Select::new_data(eq0, new, old, ty.clone()));
                    let cloned = alloc_inst(data, Store::new_data(sel, dest));
                    map.insert(orig, cloned);
                    out.push(eq0);
                    out.push(sel);
                    return cloned;
                }
            }
            Store::new_data(
                map_operand(data, store.src(), map, iv, subst_iv),
                dest,
            )
        }
        InstKind::Binary(binary) => Binary::new_data(
            map_operand(data, binary.lhs(), map, iv, subst_iv),
            map_operand(data, binary.rhs(), map, iv, subst_iv),
            binary.op(),
            ty,
        ),
        InstKind::Select(select) => Select::new_data(
            map_operand(data, select.cond(), map, iv, subst_iv),
            map_operand(data, select.if_true(), map, iv, subst_iv),
            map_operand(data, select.if_false(), map, iv, subst_iv),
            ty,
        ),
        InstKind::Integer(value) => Integer::new_data(value.value()),
        InstKind::Float(value) => Float::new_data(value.value()),
        InstKind::ZeroInit => crate::ir::instruction::InstData::new(ty, InstKind::ZeroInit),
        other => unreachable!("payload inst {other:?} cannot be cloned"),
    };
    let cloned = alloc_inst(data, cloned_data);
    map.insert(orig, cloned);
    cloned
}

fn map_operand(
    data: &mut ArenaContextMut<'_>,
    operand: Inst,
    map: &FxHashMap<Inst, Inst>,
    iv: Inst,
    subst_iv: Inst,
) -> Inst {
    if operand == iv {
        return subst_iv;
    }
    if let Some(&cloned) = map.get(&operand) {
        return cloned;
    }
    match data.inst_data(operand).kind() {
        InstKind::Integer(value) => {
            let v = value.value();
            alloc_inst(data, Integer::new_data(v))
        }
        InstKind::Float(value) => {
            let v = value.value();
            alloc_inst(data, Float::new_data(v))
        }
        InstKind::ZeroInit => {
            let ty = data.inst_data(operand).ty().clone();
            alloc_inst(data, crate::ir::instruction::InstData::new(ty, InstKind::ZeroInit))
        }
        _ => operand, // global or dominating value: reused as-is
    }
}

/// Rebuild `inst` with every operand equal to `old` replaced by `new`,
/// preserving the instruction id (and thus its layout position and
/// `used_by` links). Used to redirect an exit block's direct reads of a
/// header parameter (re-typed to a vector during a B1 reduction) to a
/// fresh scalar exit parameter. The substitution is a full remap, so any
/// instruction kind is handled uniformly.
fn subst_operand(data: &mut ArenaContextMut<'_>, inst: Inst, old: Inst, new: Inst) {
    struct Subst {
        old: Inst,
        new: Inst,
    }
    impl crate::ir::remap::EntityMapper for Subst {
        type Error = ();
        fn map_inst(&mut self, inst: Inst) -> Result<Inst, ()> {
            Ok(if inst == self.old { self.new } else { inst })
        }
        fn map_block(&mut self, block: BasicBlock) -> Result<BasicBlock, ()> {
            Ok(block)
        }
    }
    let new_data = data
        .inst_data(inst)
        .remap_refs(&mut Subst { old, new })
        .expect("identity mapper cannot fail");
    data.replace_inst_with(inst).raw(new_data);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::builder_trait::*;

    /// Global arrays `a`/`b` plus a rotated count-up loop
    /// `for (i = 0; i < trip; i++) b[i] = a[i] * 2 + 1` over them.
    /// `trip` must be a compile-time constant (exact-trip requirement).
    fn build_elementwise(program: &mut Program, elem: Type, trip: i32) -> (Function, BasicBlock, BasicBlock, BasicBlock) {
        build_op_chain(program, elem, trip, &[BinaryOp::Mul, BinaryOp::Add], &[2, 1])
    }

    /// Like `build_elementwise`, but the payload is `b[i] = chain(a[i])`
    /// where `chain` applies `ops` in order with per-op constant rhs values
    /// (`v = a[i]; for (op, c) in ops.zip(consts) { v = v op c }`). The
    /// constants are element-typed (`Integer` for i32, `Float` for f32).
    fn build_op_chain(
        program: &mut Program,
        elem: Type,
        trip: i32,
        ops: &[BinaryOp],
        consts: &[i32],
    ) -> (Function, BasicBlock, BasicBlock, BasicBlock) {
        assert_eq!(ops.len(), consts.len());
        let i32 = Type::get_i32();
        let arr = Type::get_array(elem.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr.clone());
            program.new_value().global_alloc(init)
        };
        let b = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_unit(), "op_chain".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut *program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }

        let zero = data.new_local_inst().integer(0);
        let trip_inst = data.new_local_inst().integer(trip);
        let entry_jump = data.new_local_inst().jump(header, vec![zero, trip_inst]);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, trip_inst);
        data.layout_mut().insert_inst(entry, entry_jump);

        let iv = data.bb_data(header).params()[0];
        let counter = data.bb_data(header).params()[1];
        let header_jump = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, header_jump);

        let one_iv = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let mut payload_insts: Vec<Inst> = vec![one_iv];
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        payload_insts.push(gep_a);
        payload_insts.push(load_a);
        let mut v = load_a;
        for (idx, op) in ops.iter().enumerate() {
            let c = if elem.is_f32() {
                lb.float(consts[idx] as f32)
            } else {
                lb.integer(consts[idx])
            };
            v = lb.binary(*op, v, c);
            payload_insts.push(c);
            payload_insts.push(v);
        }
        let gep_b = lb.get_elem_ptr(b, vec![zero, iv]);
        let store = lb.store(v, gep_b);
        let iv_next = lb.binary(BinaryOp::Add, iv, one_iv);
        let t_next = lb.binary(BinaryOp::Sub, counter, one_iv);
        let back = lb.branch(t_next, header, vec![iv_next, t_next], exit, vec![]);
        drop(lb);
        payload_insts.push(gep_b);
        payload_insts.push(store);
        payload_insts.push(iv_next);
        payload_insts.push(t_next);
        payload_insts.push(back);
        for inst in payload_insts {
            data.layout_mut().insert_inst(latch, inst);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);
        (function, header, latch, exit)
    }

    /// Global array `a` plus a rotated count-up reduction loop
    /// `for (i = 0; i < trip; i++) acc += a[i]` (or `-=` for `Sub`), with the
    /// rotated exit carrying the final accumulator:
    /// `header([acc, iv, t]) -> latch { acc' = op(acc, load(a[iv])); iv+1;
    /// t-1; br t' -> header / exit([acc']) }`, `exit([acc_final])`.
    #[allow(clippy::type_complexity)]
    fn build_reduction(
        program: &mut Program,
        op: BinaryOp,
        trip: i32,
    ) -> (Function, BasicBlock, BasicBlock, BasicBlock, Inst, Inst, Inst) {
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_i32(), "reduce".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut *program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone(), i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data
            .new_basic_block()
            .basic_block("exit".into(), vec![i32.clone()]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        let trip_inst = data.new_local_inst().integer(trip);
        let entry_jump = data.new_local_inst().jump(header, vec![zero, zero, trip_inst]);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, trip_inst);
        data.layout_mut().insert_inst(entry, entry_jump);
        let acc = data.bb_data(header).params()[0];
        let iv = data.bb_data(header).params()[1];
        let counter = data.bb_data(header).params()[2];
        let header_jump = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, header_jump);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let sum = lb.binary(op, acc, load_a);
        let iv_next = lb.binary(BinaryOp::Add, iv, one);
        let t_next = lb.binary(BinaryOp::Sub, counter, one);
        let back = lb.branch(t_next, header, vec![sum, iv_next, t_next], exit, vec![sum]);
        drop(lb);
        for inst in [one, gep_a, load_a, sum, iv_next, t_next, back] {
            data.layout_mut().insert_inst(latch, inst);
        }
        let exit_acc = data.bb_data(exit).params()[0];
        let exit_ret = data.new_local_inst().ret(Some(exit_acc));
        data.layout_mut().insert_inst(exit, exit_ret);
        (function, header, latch, exit, acc, iv, counter)
    }

    /// A rotated `dst[j] = src[j]` loop (`trip` iterations) over two
    /// function-parameter array bases — the h-5/h-8 kernel shape.
    fn param_copy_loop(program: &mut Program, name: &str, trip: i32) -> Function {
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let function = program.new_function(
            Type::get_unit(),
            name.into(),
            vec![Type::get_pointer(arr.clone()), Type::get_pointer(arr)],
        );
        let mut data = ArenaContextMut {
            program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        let trip_inst = data.new_local_inst().integer(trip);
        let entry_jump = data.new_local_inst().jump(header, vec![zero, trip_inst]);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, trip_inst);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let counter = data.bb_data(header).params()[1];
        let _tmp = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, _tmp);
        let one = data.new_local_inst().integer(1);
        let (src_ptr, dst_ptr) = (data.params()[0], data.params()[1]);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_src = lb.get_elem_ptr(src_ptr, vec![zero, iv]);
        let load = lb.load(gep_src);
        let gep_dst = lb.get_elem_ptr(dst_ptr, vec![zero, iv]);
        let store = lb.store(load, gep_dst);
        let iv_next = lb.binary(BinaryOp::Add, iv, one);
        let t_next = lb.binary(BinaryOp::Sub, counter, one);
        let back = lb.branch(t_next, header, vec![iv_next, t_next], exit, vec![]);
        drop(lb);
        for inst in [one, gep_src, load, gep_dst, store, iv_next, t_next, back] {
            data.layout_mut().insert_inst(latch, inst);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);
        function
    }

    /// A `main` that calls `kernel` with the given actuals (its only purpose
    /// is to feed the whole-program IPA points-to analysis).
    fn call_kernel(program: &mut Program, kernel: Function, args: Vec<Inst>) -> Function {
        let main = program.new_function(Type::get_unit(), "main".into(), vec![]);
        let mut data = ArenaContextMut {
            program,
            curr_func: Some(main),
        };
        let entry = data.add_entry_block();
        let call = data.new_local_value().call(kernel, args);
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);
        main
    }

    fn run(program: &mut Program, function: Function) -> bool {
        let mut context = ArenaContextMut {
            program,
            curr_func: Some(function),
        };
        LoopVectorize::new().run_on(&mut context)
    }

    fn vector_load_count(program: &Program, function: Function) -> usize {
        let data = program.func_data(function);
        data.layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .filter(|&inst| matches!(data.inst_data(inst).kind(), InstKind::Load(_)))
            .filter(|&inst| data.inst_data(inst).ty().is_vector())
            .count()
    }

    fn vector_reduce_count(program: &Program, function: Function) -> usize {
        let data = program.func_data(function);
        data.layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .filter(|&inst| matches!(data.inst_data(inst).kind(), InstKind::VectorReduce(_)))
            .count()
    }

    fn scalar_load_count(program: &Program, function: Function) -> usize {
        let data = program.func_data(function);
        data.layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .filter(|&inst| matches!(data.inst_data(inst).kind(), InstKind::Load(_)))
            .filter(|&inst| !data.inst_data(inst).ty().is_vector())
            .count()
    }

    /// Payload binary ops that were rewritten to lane-wise vector ops, in
    /// layout order (the latch machinery stays scalar, so this lists exactly
    /// the vectorized payload operations).
    fn vector_binaries(program: &Program, function: Function) -> Vec<BinaryOp> {
        let data = program.func_data(function);
        data.layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .filter_map(|inst| match data.inst_data(inst).kind() {
                InstKind::Binary(binary) if data.inst_data(inst).ty().is_vector() => {
                    Some(binary.op())
                }
                _ => None,
            })
            .collect()
    }

    fn latch_of(program: &Program, function: Function, latch: BasicBlock) -> Vec<Inst> {
        program
            .func_data(function)
            .layout()
            .basicblock(latch)
            .insts()
            .iter()
            .copied()
            .collect()
    }

    #[test]
    fn vectorizes_exact_16_trip_i32_loop() {
        let mut program = Program::new();
        let (function, _header, latch, exit) = build_elementwise(&mut program, Type::get_i32(), 16);
        assert!(run(&mut program, function), "pass must fire");
        // 2x unroll: two vector loads (a0/a1), two vector stores (b0/b1), no
        // scalar loads left.
        assert_eq!(vector_load_count(&program, function), 2);
        assert_eq!(scalar_load_count(&program, function), 0);
        // R == 0: no epilogue, the latch still exits to `exit`.
        let insts = latch_of(&program, function, latch);
        let terminator = *insts.last().unwrap();
        let InstKind::Branch(branch) = program.func_data(function).inst_data(terminator).kind()
        else {
            panic!("latch must end in a branch");
        };
        assert_eq!(branch.f_target(), exit, "no epilogue for trip % 4 == 0");
        // The counter now steps by 8 (2x unroll) and the index by 8.
        let step_is = |inst: &Inst, op: BinaryOp| {
            matches!(
                program.func_data(function).inst_data(*inst).kind(),
                InstKind::Binary(b) if b.op() == op && matches!(
                    program.func_data(function).inst_data(b.rhs()).kind(),
                    InstKind::Integer(v) if v.value() == 8
                )
            )
        };
        let updates: Vec<Inst> = insts
            .iter()
            .copied()
            .filter(|&i| matches!(program.func_data(function).inst_data(i).kind(), InstKind::Binary(_)))
            .collect();
        assert!(updates.iter().any(|i| step_is(i, BinaryOp::Add)));
        assert!(updates.iter().any(|i| step_is(i, BinaryOp::Sub)));
    }

    #[test]
    fn trip_18_peels_two_scalar_epilogue_iterations() {
        let mut program = Program::new();
        let (function, _header, latch, exit) = build_elementwise(&mut program, Type::get_i32(), 18);
        assert!(run(&mut program, function), "pass must fire");
        let data = program.func_data(function);
        // R = 2: the latch exits into a 2-block epilogue chain ending at exit.
        let insts: Vec<Inst> = data
            .layout()
            .basicblock(latch)
            .insts()
            .iter()
            .copied()
            .collect();
        let terminator = *insts.last().unwrap();
        let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
            panic!("latch must end in a branch");
        };
        let first = branch.f_target();
        assert_ne!(first, exit, "epilogue must be peeled for trip % 4 == 2");
        let mut current = first;
        let mut depth = 0;
        loop {
            depth += 1;
            assert!(depth <= 3, "epilogue must be at most 3 blocks");
            let term = data.layout().basicblock(current).terminator();
            let InstKind::Jump(jump) = data.inst_data(term).kind() else {
                panic!("epilogue blocks must end in a jump");
            };
            if jump.target() == exit {
                break;
            }
            current = jump.target();
        }
        assert_eq!(depth, 2, "R = 18 % 4 = 2 epilogue blocks");
        // The epilogue is scalar: it contains the two peeled iterations with
        // constant indices 16 and 17 (i0 + 4Q + k).
        let const_gep_index = |block: BasicBlock| -> Vec<i32> {
            data.layout()
                .basicblock(block)
                .insts()
                .iter()
                .filter_map(|&i| match data.inst_data(i).kind() {
                    InstKind::GetElemPtr(gep) => {
                        let InstKind::Integer(v) = data.inst_data(gep.offsets()[1]).kind()
                        else {
                            return None;
                        };
                        Some(v.value())
                    }
                    _ => None,
                })
                .collect()
        };
        let mut indexes: Vec<i32> = Vec::new();
        let mut current = first;
        loop {
            indexes.extend(const_gep_index(current));
            let term = data.layout().basicblock(current).terminator();
            let InstKind::Jump(jump) = data.inst_data(term).kind() else {
                panic!("epilogue blocks must end in a jump");
            };
            if jump.target() == exit {
                break;
            }
            current = jump.target();
        }
        indexes.sort_unstable();
        indexes.dedup();
        assert_eq!(indexes, vec![16, 17]);
        assert!(scalar_load_count(&program, function) >= 2, "epilogue keeps scalar loads");
    }

    #[test]
    fn rejects_strided_access() {
        // a[2*i] strides by 8 bytes: not contiguous, must be rejected.
        let mut program = Program::new();
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_unit(), "strided".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        let trip_inst = data.new_local_inst().integer(16);
        let entry_jump = data.new_local_inst().jump(header, vec![zero, trip_inst]);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, trip_inst);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let counter = data.bb_data(header).params()[1];
        let _tmp_inst1 = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, _tmp_inst1);
        let one = data.new_local_inst().integer(1);
        let two = data.new_local_inst().integer(2);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let twice_iv = lb.binary(BinaryOp::Mul, iv, two);
        let gep_a = lb.get_elem_ptr(a, vec![zero, twice_iv]);
        let load_a = lb.load(gep_a);
        let gep_b = lb.get_elem_ptr(a, vec![zero, iv]);
        let store = lb.store(load_a, gep_b);
        let iv_next = lb.binary(BinaryOp::Add, iv, one);
        let t_next = lb.binary(BinaryOp::Sub, counter, one);
        let back = lb.branch(t_next, header, vec![iv_next, t_next], exit, vec![]);
        drop(lb);
        for inst in [one, two, twice_iv, gep_a, load_a, gep_b, store, iv_next, t_next, back] {
            data.layout_mut().insert_inst(latch, inst);
        }
        let _tmp_inst2 = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, _tmp_inst2);
        assert!(
            !run(&mut program, function),
            "8-byte stride must be rejected (v1: no gathers)"
        );
    }

    #[test]
    fn vectorizes_reduction() {
        // B1: `sum += a[i]` — a 3-param loop [acc, iv, t] whose accumulator
        // update is `acc' = add(acc, load(a[iv]))`. The accumulator must
        // become a `<4 x i32>` block parameter and the exit must reduce it
        // horizontally (VectorReduce / addv) before the exit parameter.
        let mut program = Program::new();
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_i32(), "reduce".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone(), i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        // Rotated form: the exit carries the final accumulator value.
        let exit = data
            .new_basic_block()
            .basic_block("exit".into(), vec![i32.clone()]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        let trip_inst = data.new_local_inst().integer(16);
        let entry_jump = data.new_local_inst().jump(header, vec![zero, zero, trip_inst]);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, trip_inst);
        data.layout_mut().insert_inst(entry, entry_jump);
        let acc = data.bb_data(header).params()[0];
        let iv = data.bb_data(header).params()[1];
        let counter = data.bb_data(header).params()[2];
        let header_jump = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, header_jump);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let sum = lb.binary(BinaryOp::Add, acc, load_a);
        let iv_next = lb.binary(BinaryOp::Add, iv, one);
        let t_next = lb.binary(BinaryOp::Sub, counter, one);
        let back = lb.branch(t_next, header, vec![sum, iv_next, t_next], exit, vec![sum]);
        drop(lb);
        for inst in [one, gep_a, load_a, sum, iv_next, t_next, back] {
            data.layout_mut().insert_inst(latch, inst);
        }
        let exit_acc = data.bb_data(exit).params()[0];
        let exit_ret = data.new_local_inst().ret(Some(exit_acc));
        data.layout_mut().insert_inst(exit, exit_ret);

        assert!(
            run(&mut program, function),
            "register reductions are vectorized in B1"
        );
        let data = program.func_data(function);
        // The accumulator parameter is now a vector.
        assert!(
            data.inst_data(acc).ty().is_vector(),
            "accumulator must be re-typed to <4 x i32>"
        );
        // A horizontal reduction exists (the exit of the vector loop).
        let has_reduce = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .any(|inst| matches!(data.inst_data(inst).kind(), InstKind::VectorReduce(_)));
        assert!(has_reduce, "the vector loop must exit through VectorReduce");
        // The exit still receives a scalar accumulator (i32).
        assert!(
            data.inst_data(exit_acc).ty().is_i32(),
            "exit accumulator parameter stays scalar"
        );
    }

    #[test]
    fn vectorizes_mod_reduction() {
        // Mod-wrapped scalar reduction: `acc' = (acc + a[iv] % 100) % 1000`
        // on a rotated `[iv, acc, t]` loop. The M42 verdict labels such a
        // loop Reducible on the *counter* (the acc update's outermost op is
        // Rem), so the accumulator is found by direct parameter scan. The
        // accumulator stays scalar; the element computation `a[iv] % 100` is
        // vectorized and `addv`'d once per vector round.
        let mut program = Program::new();
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_i32(), "modreduce".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone(), i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data
            .new_basic_block()
            .basic_block("exit".into(), vec![i32.clone()]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        let trip_inst = data.new_local_inst().integer(16);
        let entry_jump = data.new_local_inst().jump(header, vec![zero, zero, trip_inst]);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, trip_inst);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let acc = data.bb_data(header).params()[1];
        let counter = data.bb_data(header).params()[2];
        let header_jump = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, header_jump);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let hundred = lb.integer(100);
        let mod100 = lb.binary(BinaryOp::Rem, load_a, hundred);
        let sum = lb.binary(BinaryOp::Add, acc, mod100);
        let thousand = lb.integer(1000);
        let mod1000 = lb.binary(BinaryOp::Rem, sum, thousand);
        let iv_next = lb.binary(BinaryOp::Add, iv, one);
        let t_next = lb.binary(BinaryOp::Sub, counter, one);
        let back =
            lb.branch(t_next, header, vec![iv_next, mod1000, t_next], exit, vec![mod1000]);
        drop(lb);
        for inst in [one, hundred, thousand, gep_a, load_a, mod100, sum, mod1000, iv_next, t_next, back]
        {
            data.layout_mut().insert_inst(latch, inst);
        }
        let exit_acc = data.bb_data(exit).params()[0];
        let exit_ret = data.new_local_inst().ret(Some(exit_acc));
        data.layout_mut().insert_inst(exit, exit_ret);

        assert!(
            run(&mut program, function),
            "mod-wrapped reductions must vectorize"
        );
        let data = program.func_data(function);
        // The accumulator stays scalar (mod keeps it in range each round).
        assert!(
            data.inst_data(acc).ty().is_i32(),
            "mod accumulator stays scalar"
        );
        // The element computation (`a[iv] % 100`) is the only vectorized op.
        assert_eq!(
            vector_binaries(&program, function),
            vec![BinaryOp::Rem],
            "only the element rem is lane-wise"
        );
        // A horizontal reduction feeds the scalar accumulator.
        let has_reduce = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .any(|inst| matches!(data.inst_data(inst).kind(), InstKind::VectorReduce(_)));
        assert!(has_reduce, "per-round addv must exist");
    }

    #[test]
    fn vectorizes_reduction_int_sub() {
        // acc -= a[i]: IntSub reductions accumulate the same way.
        let mut program = Program::new();
        let (function, _header, _latch, _exit, acc, _iv, _counter) =
            build_reduction(&mut program, BinaryOp::Sub, 16);
        assert!(run(&mut program, function), "IntSub reductions vectorize");
        let data = program.func_data(function);
        assert!(data.inst_data(acc).ty().is_vector(), "acc must be re-typed");
        let has_reduce = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .any(|inst| matches!(data.inst_data(inst).kind(), InstKind::VectorReduce(_)));
        assert!(has_reduce, "exit must reduce the accumulator");
    }

    #[test]
    fn reduction_epilogue_carries_acc() {
        // trip = 18 -> Q = 4 vector iterations + R = 2 scalar iterations. The
        // peeled epilogue must keep accumulating: each epi block takes the
        // scalar accumulator as a parameter and forwards its updated value.
        let mut program = Program::new();
        let (function, _header, latch, exit, acc, _iv, _counter) =
            build_reduction(&mut program, BinaryOp::Add, 18);
        assert!(run(&mut program, function), "pass must fire");
        let data = program.func_data(function);
        assert!(
            data.inst_data(acc).ty().is_vector(),
            "vector accumulator in the main loop"
        );
        // The epilogue chain: two blocks after the reduce block (which has a
        // vector parameter), both carrying one i32 accumulator parameter.
        let mut current = latch;
        let mut epi_found = 0;
        loop {
            let term = data.layout().basicblock(current).terminator();
            let next = match data.inst_data(term).kind() {
                InstKind::Branch(branch) => branch.f_target(),
                InstKind::Jump(jump) => jump.target(),
                _ => break,
            };
            if next == exit {
                break;
            }
            let next_params = data.bb_data(next).params();
            if next_params.is_empty() || data.inst_data(next_params[0]).ty().is_vector() {
                // The reduce block (vector accumulator) sits between the
                // latch and the epilogue chain.
                current = next;
                continue;
            }
            assert_eq!(
                next_params.len(),
                1,
                "epi block must carry the scalar accumulator"
            );
            assert!(
                data.inst_data(next_params[0]).ty().is_i32(),
                "epi accumulator stays scalar"
            );
            epi_found += 1;
            current = next;
        }
        assert_eq!(epi_found, 2, "R = 2 peeled iterations");
    }

    #[test]
    fn reduction_is_idempotent() {
        let mut program = Program::new();
        let (function, _header, _latch, _exit, _acc, _iv, _counter) =
            build_reduction(&mut program, BinaryOp::Add, 16);
        assert!(run(&mut program, function), "first run vectorizes");
        assert!(
            !run(&mut program, function),
            "second run must not rewrite the vector loop"
        );
    }

    #[test]
    fn rejects_int_mul_reduction() {
        // acc *= a[i] is Reducible{IntMul}; B1 only accumulates IntAdd/IntSub
        // (IntMul needs a different exit reduction).
        let mut program = Program::new();
        let (function, _header, _latch, _exit, _acc, _iv, _counter) =
            build_reduction(&mut program, BinaryOp::Mul, 16);
        assert!(
            !run(&mut program, function),
            "IntMul reductions stay scalar in B1"
        );
    }

    #[test]
    fn rejects_exit_reads_header_acc() {
        // An exit block that reads the accumulator header parameter directly
        // (not via the rotated exit parameter) is not the rotated form and
        // must stay scalar.
        let mut program = Program::new();
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_i32(), "reduce".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone(), i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        let trip_inst = data.new_local_inst().integer(16);
        let entry_jump = data.new_local_inst().jump(header, vec![zero, zero, trip_inst]);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, trip_inst);
        data.layout_mut().insert_inst(entry, entry_jump);
        let acc = data.bb_data(header).params()[0];
        let iv = data.bb_data(header).params()[1];
        let counter = data.bb_data(header).params()[2];
        let _tmp_inst5 = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, _tmp_inst5);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let sum = lb.binary(BinaryOp::Add, acc, load_a);
        let iv_next = lb.binary(BinaryOp::Add, iv, one);
        let t_next = lb.binary(BinaryOp::Sub, counter, one);
        let back = lb.branch(t_next, header, vec![sum, iv_next, t_next], exit, vec![]);
        drop(lb);
        for inst in [one, gep_a, load_a, sum, iv_next, t_next, back] {
            data.layout_mut().insert_inst(latch, inst);
        }
        let _tmp_inst6 = data.new_local_inst().ret(Some(acc));
        data.layout_mut().insert_inst(exit, _tmp_inst6);
        assert!(
            !run(&mut program, function),
            "real accumulator reduction must be rejected in v1"
        );
    }

    #[test]
    fn vectorizes_runtime_trip_counter_entry() {
        // The trip counter comes from a function parameter (a runtime
        // value — the rotated `t0 = bound - i0` shape). No versioning is
        // needed: the vector counter enters at `t0 & -4` and a scalar
        // tail loop covers the `t0 & 3` remainder, so the loop
        // vectorizes structurally.
        let mut program = Program::new();
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr.clone());
            program.new_value().global_alloc(init)
        };
        let b = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_unit(), "dynamic".into(), vec![i32.clone()]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        let n_param = data.params()[0];
        let entry_jump = data.new_local_inst().jump(header, vec![zero, n_param]);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let counter = data.bb_data(header).params()[1];
        let _tmp_inst7 = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, _tmp_inst7);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let gep_b = lb.get_elem_ptr(b, vec![zero, iv]);
        let store = lb.store(load_a, gep_b);
        let iv_next = lb.binary(BinaryOp::Add, iv, one);
        let t_next = lb.binary(BinaryOp::Sub, counter, one);
        let back = lb.branch(t_next, header, vec![iv_next, t_next], exit, vec![]);
        drop(lb);
        for inst in [one, gep_a, load_a, gep_b, store, iv_next, t_next, back] {
            data.layout_mut().insert_inst(latch, inst);
        }
        let _tmp_inst8 = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, _tmp_inst8);
        assert!(
            run(&mut program, function),
            "a runtime trip counter must vectorize (counter + scalar tail)"
        );
        let data = program.func_data(function);
        // The vectorized latch tests `gt(counter, 0)` (a negative
        // `cnt0` must not spin the `t' != 0` test forever) and the
        // counter's back-edge arg steps by 4.
        let latch_term = data.layout().basicblock(latch).terminator();
        let InstKind::Branch(branch) = data.inst_data(latch_term).kind() else {
            panic!("latch must end in the rewritten branch");
        };
        let InstKind::Binary(gt) = data.inst_data(branch.cond()).kind() else {
            panic!("runtime counter test must be a comparison");
        };
        assert_eq!(gt.op(), BinaryOp::Gt, "counter test is gt(counter, 0)");
        assert_eq!(
            branch.t_target(), header,
            "the true edge keeps looping"
        );
        // The entry is a guarded branch: `gt(cnt0, 0)` — the vector loop
        // is a do-while, so a zero/negative `cnt0` (trip < 4) must skip
        // it entirely and enter the scalar tail directly. The true edge
        // carries `cnt0 = and(t0, -4)`.
        let entry_edge = data.layout().basicblock(entry).terminator();
        let InstKind::Branch(entry_branch) = data.inst_data(entry_edge).kind() else {
            panic!("entry must be a guard branch");
        };
        let InstKind::Binary(guard_gt) = data.inst_data(entry_branch.cond()).kind() else {
            panic!("entry guard must be a comparison");
        };
        assert_eq!(guard_gt.op(), BinaryOp::Gt);
        let InstKind::Binary(and) = data.inst_data(entry_branch.t_args()[1]).kind() else {
            panic!("counter entry must be `and(t0, -4)`");
        };
        assert_eq!(and.op(), BinaryOp::And);
        assert!(
            data.bb_data(entry_branch.f_target())
                .name()
                .starts_with("vec_tail"),
            "the zero-trip edge enters the scalar tail"
        );
        // The scalar tail covers the remainder.
        assert!(
            data.layout()
                .basicblocks()
                .iter()
                .any(|l| data.bb_data(l.bb()).name().starts_with("vec_tail")),
            "the remainder runs in a scalar tail"
        );
        assert!(!run(&mut program, function), "idempotent");
    }

    #[test]
    fn rejects_array_param_base() {
        // dst[j] = src[j] over array parameters: with no call site the IPA
        // alignment inference has no evidence, so the base stays rejected
        // (versioning is M43).
        let mut program = Program::new();
        let i32 = Type::get_i32();
        let function = program.new_function(
            Type::get_unit(),
            "param_base".into(),
            vec![
                Type::get_pointer(Type::get_array(i32.clone(), 64)),
                Type::get_pointer(Type::get_array(i32.clone(), 64)),
            ],
        );
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        let trip_inst = data.new_local_inst().integer(16);
        let entry_jump = data.new_local_inst().jump(header, vec![zero, trip_inst]);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, trip_inst);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let counter = data.bb_data(header).params()[1];
        let _tmp_inst9 = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, _tmp_inst9);
        let one = data.new_local_inst().integer(1);
        let (src_ptr, dst_ptr) = (data.params()[0], data.params()[1]);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_src = lb.get_elem_ptr(src_ptr, vec![zero, iv]);
        let load = lb.load(gep_src);
        let gep_dst = lb.get_elem_ptr(dst_ptr, vec![zero, iv]);
        let store = lb.store(load, gep_dst);
        let iv_next = lb.binary(BinaryOp::Add, iv, one);
        let t_next = lb.binary(BinaryOp::Sub, counter, one);
        let back = lb.branch(t_next, header, vec![iv_next, t_next], exit, vec![]);
        drop(lb);
        for inst in [one, gep_src, load, gep_dst, store, iv_next, t_next, back] {
            data.layout_mut().insert_inst(latch, inst);
        }
        let _tmp_inst10 = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, _tmp_inst10);
        assert!(
            !run(&mut program, function),
            "array parameter bases must be rejected in v1"
        );
    }

    #[test]
    fn vectorizes_inlined_idx_index_computation_chain() {
        // conv2d's row_reduce 同构: the inlined `idx(r, c, n)` lands in its
        // own blocks — `forwarder -> index-latch -> payload` — with the GEP
        // offset `add(mul(r, n), c)` where both `r` and `n` are
        // loop-invariant function params. Before the chain fusion + index
        // math classification this shape was rejected outright
        // (shape_body_not_2_blocks / NonAffineIndex). The full pass must
        // fuse the chain, classify the invariant×invariant product, keep
        // the index math scalar, and vectorize the reduction.
        let mut program = Program::new();
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let kernel = program.new_function(
            Type::get_unit(),
            "row_reduce".into(),
            vec![
                i32.clone(),
                i32.clone(),
                Type::get_pointer(Type::get_array(Type::get_i32(), 64)),
            ],
        );
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(kernel),
        };
        let entry = data.add_entry_block();
        let r = data.params()[0];
        let n = data.params()[1];
        let base = data.params()[2];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone()]);
        let fwd = data.new_basic_block().basic_block("fwd".into(), vec![]);
        let idx_latch = data
            .new_basic_block()
            .basic_block("idx_latch".into(), vec![i32.clone(), i32.clone(), i32.clone()]);
        let payload = data
            .new_basic_block()
            .basic_block("payload".into(), vec![i32.clone()]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [header, fwd, idx_latch, payload, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        let one = data.new_local_inst().integer(1);
        let entry_jump = data.new_local_inst().jump(header, vec![zero, zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let sum = data.bb_data(header).params()[0];
        let c = data.bb_data(header).params()[1];
        let cmp = data.new_local_inst().binary(BinaryOp::Lt, c, n);
        let br = data
            .new_local_inst()
            .branch(cmp, fwd, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, cmp);
        data.layout_mut().insert_inst(header, br);
        let fwd_jump = data.new_local_inst().jump(idx_latch, vec![r, c, n]);
        data.layout_mut().insert_inst(fwd, fwd_jump);
        let i_r = data.bb_data(idx_latch).params()[0];
        let i_c = data.bb_data(idx_latch).params()[1];
        let i_n = data.bb_data(idx_latch).params()[2];
        let idx_mul = data.new_local_inst().binary(BinaryOp::Mul, i_r, i_n);
        let idx_add = data.new_local_inst().binary(BinaryOp::Add, idx_mul, i_c);
        let idx_jump = data.new_local_inst().jump(payload, vec![idx_add]);
        for inst in [idx_mul, idx_add, idx_jump] {
            data.layout_mut().insert_inst(idx_latch, inst);
        }
        let idx = data.bb_data(payload).params()[0];
        let gep = data.new_local_inst().get_elem_ptr(base, vec![zero, idx]);
        let load = data.new_local_inst().load(gep);
        let sum_next = data.new_local_inst().binary(BinaryOp::Add, sum, load);
        let c_next = data.new_local_inst().binary(BinaryOp::Add, i_c, one);
        let back = data.new_local_inst().jump(header, vec![sum_next, c_next]);
        for inst in [gep, load, sum_next, c_next, back] {
            data.layout_mut().insert_inst(payload, inst);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);
        drop(data);
        // IPA alignment: the only call site passes the 16B-aligned global.
        // The i32 actuals are program-level constants (dominate `main`).
        let r_actual = program.new_value().integer(3);
        let n_actual = program.new_value().integer(16);
        call_kernel(&mut program, kernel, vec![r_actual, n_actual, a]);
        assert!(
            run(&mut program, kernel),
            "the inlined-idx chain must fuse and vectorize"
        );
        assert!(vector_load_count(&program, kernel) >= 1, "vector load emitted");
        assert!(vector_reduce_count(&program, kernel) >= 1, "reduction vectorized");
    }

    #[test]
    fn vectorizes_param_base_all_aligned() {
        // dst[j] = src[j] over parameter arrays whose only call site passes
        // 16B-aligned globals: the IPA alignment inference must unlock the
        // loop (both the load and the store pass the v2 alignment gate).
        let mut program = Program::new();
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let src = {
            let init = program.new_value().zero_init(arr.clone());
            program.new_value().global_alloc(init)
        };
        let dst = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let kernel = param_copy_loop(&mut program, "kernel", 16);
        call_kernel(&mut program, kernel, vec![src, dst]);
        assert!(
            run(&mut program, kernel),
            "an all-aligned parameter base must vectorize (IPA)"
        );
        assert!(vector_load_count(&program, kernel) >= 1, "vector load emitted");
    }

    #[test]
    fn rejects_param_mixed_alignment() {
        // Two call sites: one passes a 256-byte global, the other an 8-byte
        // global. The points-to may-set contains a non-aligned base, so the
        // parameter stays scalar (no versioning).
        let mut program = Program::new();
        let i32 = Type::get_i32();
        let arr64 = Type::get_array(i32.clone(), 64);
        let arr2 = Type::get_array(i32.clone(), 2);
        let big = {
            let init = program.new_value().zero_init(arr64);
            program.new_value().global_alloc(init)
        };
        let small = {
            let init = program.new_value().zero_init(arr2);
            program.new_value().global_alloc(init)
        };
        let kernel = param_copy_loop(&mut program, "kernel", 16);
        call_kernel(&mut program, kernel, vec![big, big]);
        call_kernel(&mut program, kernel, vec![small, small]);
        assert!(
            !run(&mut program, kernel),
            "a parameter with a non-aligned call site must stay scalar"
        );
    }

    #[test]
    fn rejects_param_no_callsite() {
        // No call site at all: the points-to table has no entry for the
        // parameter — conservatively rejected (same guarantee as v1).
        let mut program = Program::new();
        let kernel = param_copy_loop(&mut program, "kernel", 16);
        assert!(
            !run(&mut program, kernel),
            "unreachable parameter arrays must stay scalar"
        );
    }

    #[test]
    fn aligns_param_through_recursive_callsite() {
        // The kernel calls itself with its own parameters. The recursive
        // site's actuals are unresolved (the function's own parameters), so
        // it contributes nothing to the points-to set; the concrete main
        // site still carries the aligned globals → the parameter stays
        // aligned. (Alignment is a monotone property; recursion cannot
        // downgrade it.)
        let mut program = Program::new();
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let src = {
            let init = program.new_value().zero_init(arr.clone());
            program.new_value().global_alloc(init)
        };
        let dst = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let kernel = param_copy_loop(&mut program, "kernel", 16);
        {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(kernel),
            };
            let entry = data.layout().entry_bb().expect("entry block").bb();
            let (p0, p1) = (data.params()[0], data.params()[1]);
            let rec = data.new_local_value().call(kernel, vec![p0, p1]);
            data.layout_mut().insert_before_terminator(entry, rec);
        }
        call_kernel(&mut program, kernel, vec![src, dst]);
        assert!(
            run(&mut program, kernel),
            "recursion must not downgrade an all-aligned parameter"
        );
    }

    #[test]
    fn is_idempotent() {
        let mut program = Program::new();
        let (function, _, _, _) = build_elementwise(&mut program, Type::get_i32(), 16);
        assert!(run(&mut program, function), "first run vectorizes");
        assert!(
            !run(&mut program, function),
            "second run must not change the vectorized loop"
        );
    }

    #[test]
    fn vectorizes_f32_loop() {
        let mut program = Program::new();
        let (function, _, _, _) = build_elementwise(&mut program, Type::get_f32(), 16);
        assert!(run(&mut program, function), "f32 loops are vectorized too");
        let data = program.func_data(function);
        let has_f32_vector = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .any(|inst| {
                data.inst_data(inst).ty().is_vector()
                    && matches!(data.inst_data(inst).ty().kind(), TypeKind::Vector(elem, _) if elem.is_f32())
            });
        assert!(has_f32_vector, "the vector load must be <4 x f32>");
    }

    #[test]
    fn vectorizes_shift_xor_loop() {
        // b[i] = (a[i] << 1) ^ 3: shifts and bitwise ops are vectorized
        // (NEON sshl + eor v.4s); the shift amount is a splatted constant.
        let mut program = Program::new();
        let (function, _header, _latch, _exit) = build_op_chain(
            &mut program,
            Type::get_i32(),
            16,
            &[BinaryOp::Shl, BinaryOp::Xor],
            &[1, 3],
        );
        assert!(run(&mut program, function), "shift/xor loop must vectorize");
        // 2x unroll: two vector loads, and the two copies' binaries appear in
        // grouped order `[shl0, xor0, shl1, xor1]`.
        assert_eq!(vector_load_count(&program, function), 2);
        assert_eq!(scalar_load_count(&program, function), 0);
        assert_eq!(
            vector_binaries(&program, function),
            vec![BinaryOp::Shl, BinaryOp::Xor, BinaryOp::Shl, BinaryOp::Xor],
            "payload binaries become lane-wise vector ops in grouped copy order"
        );
    }

    #[test]
    fn rejects_i32_div() {
        // NEON has no integer vector divide (`sdiv` is scalar-only), so a
        // *variable* i32 divisor stays scalar; a constant divisor vectorizes
        // through the multiply-high magic sequence (`smull`/`xtn`/`mls`).
        let mut program = Program::new();
        let (function, _header, _latch, _exit) =
            build_op_chain(&mut program, Type::get_i32(), 16, &[BinaryOp::Div], &[2]);
        assert!(run(&mut program, function), "i32 const div must vectorize");
        assert_eq!(
            vector_binaries(&program, function),
            vec![BinaryOp::Div, BinaryOp::Div],
            "2x unroll duplicates the payload"
        );
    }

    #[test]
    fn vectorizes_f32_div_loop() {
        // b[i] = a[i] / 2.0: float vector divide (fdiv v.4s).
        let mut program = Program::new();
        let (function, _header, _latch, _exit) =
            build_op_chain(&mut program, Type::get_f32(), 16, &[BinaryOp::Div], &[2]);
        assert!(run(&mut program, function), "f32 div loop must vectorize");
        assert_eq!(
            vector_binaries(&program, function),
            vec![BinaryOp::Div, BinaryOp::Div],
            "2x unroll duplicates the payload"
        );
        let data = program.func_data(function);
        let has_f32_vector = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .any(|inst| {
                data.inst_data(inst).ty().is_vector()
                    && matches!(data.inst_data(inst).ty().kind(), TypeKind::Vector(elem, _) if elem.is_f32())
            });
        assert!(has_f32_vector, "the vector ops must be <4 x f32>");
    }

    #[test]
    fn rejects_f32_shift() {
        // Float shifts have no semantics (and no NEON form): rejected even
        // though the op is whitelisted for i32.
        let mut program = Program::new();
        let (function, _header, _latch, _exit) =
            build_op_chain(&mut program, Type::get_f32(), 16, &[BinaryOp::Shl], &[1]);
        assert!(!run(&mut program, function), "f32 shift must be rejected");
    }

    #[test]
    fn shift_loop_is_idempotent() {
        // Same function run twice: the second run must not rewrite the
        // already-vectorized loop (fixed-point convergence).
        let mut program = Program::new();
        let (function, _header, _latch, _exit) = build_op_chain(
            &mut program,
            Type::get_i32(),
            16,
            &[BinaryOp::Shl, BinaryOp::Xor],
            &[1, 3],
        );
        assert!(run(&mut program, function), "first run vectorizes");
        assert!(
            !run(&mut program, function),
            "second run must not change the vectorized loop"
        );
    }

    #[test]
    fn vectorizes_inplace_accumulation() {
        // B3: `b[i] = b[i] + a[i]` — a same-address read-modify-write whose
        // stored value depends on the loaded value is lane-independent
        // (M42 elementwise-inplace relaxation), so the loop vectorizes with
        // two vector loads, one lane-wise add, one vector store.
        let mut program = Program::new();
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr.clone());
            program.new_value().global_alloc(init)
        };
        let b = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_unit(), "inplace".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        let trip_inst = data.new_local_inst().integer(16);
        let entry_jump = data.new_local_inst().jump(header, vec![zero, trip_inst]);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, trip_inst);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let counter = data.bb_data(header).params()[1];
        let header_jump = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, header_jump);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_b = lb.get_elem_ptr(b, vec![zero, iv]);
        let load_b = lb.load(gep_b);
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let sum = lb.binary(BinaryOp::Add, load_b, load_a);
        let store = lb.store(sum, gep_b);
        let iv_next = lb.binary(BinaryOp::Add, iv, one);
        let t_next = lb.binary(BinaryOp::Sub, counter, one);
        let back = lb.branch(t_next, header, vec![iv_next, t_next], exit, vec![]);
        drop(lb);
        for inst in [
            one, gep_b, load_b, gep_a, load_a, sum, store, iv_next, t_next, back,
        ] {
            data.layout_mut().insert_inst(latch, inst);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(
            run(&mut program, function),
            "in-place accumulation must vectorize (B3)"
        );
        assert_eq!(
            vector_load_count(&program, function),
            4,
            "2x unroll: both b[i] and a[i] become two vector loads each"
        );
        assert_eq!(scalar_load_count(&program, function), 0);
        assert_eq!(
            vector_binaries(&program, function),
            vec![BinaryOp::Add, BinaryOp::Add],
            "2x unroll: the update becomes two lane-wise adds"
        );
    }

    /// Build a test-at-top elementwise loop: `while (i < bound) {
    /// b[i] = a[i] + 1; i++; }` — header ends in `lt iv, bound; br`, the
    /// latch is a plain jump back to the header (no rotation).
    fn build_test_at_top(program: &mut Program, bound: i32) -> Function {
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr.clone());
            program.new_value().global_alloc(init)
        };
        let b = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_unit(), "test_at_top".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut *program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let bound = data.new_local_inst().integer(bound);
        let cond = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        let header_br = data.new_local_inst().branch(cond, latch, vec![], exit, vec![]);
        // Constants never occupy layout; the header holds exactly
        // [lt, branch] for the test-at-top shape.
        data.layout_mut().insert_inst(header, cond);
        data.layout_mut().insert_inst(header, header_br);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let sum = lb.binary(BinaryOp::Add, load_a, one);
        let gep_b = lb.get_elem_ptr(b, vec![zero, iv]);
        let store = lb.store(sum, gep_b);
        drop(lb);
        let iv_next = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let latch_jump = data.new_local_inst().jump(header, vec![iv_next]);
        for inst in [
            one, gep_a, load_a, sum, gep_b, store, iv_next, latch_jump,
        ] {
            data.layout_mut().insert_inst(latch, inst);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);
        function
    }

    #[test]
    fn vectorizes_test_at_top_loop() {
        // test-at-top with a constant bound: trip = 16, no remainder — the
        // bound test becomes a counter test, payload vectorizes.
        let mut program = Program::new();
        let function = build_test_at_top(&mut program, 16);
        assert!(
            run(&mut program, function),
            "test-at-top loop with constant bound must vectorize"
        );
        assert_eq!(
            vector_load_count(&program, function),
            1,
            "a[i] becomes a vector load"
        );
        assert_eq!(scalar_load_count(&program, function), 0);
        assert_eq!(
            vector_binaries(&program, function),
            vec![BinaryOp::Add],
            "the update becomes a lane-wise add"
        );
    }

    /// Build a multi-parameter test-at-top elementwise loop (the A3 shape):
    /// the header carries `[passthrough_i, iv]` — `passthrough_i` is an
    /// outer induction value the latch forwards unchanged (back-edge arg ==
    /// parameter) — and the payload consumes it (`b[i] = a[i] +
    /// passthrough_i`). The exit takes the loop-invariant passthrough value
    /// as its single parameter.
    #[allow(clippy::type_complexity)]
    fn build_multi_param_test_at_top(
        program: &mut Program,
        bound: i32,
    ) -> (Function, BasicBlock, BasicBlock, BasicBlock, Inst) {
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr.clone());
            program.new_value().global_alloc(init)
        };
        let b = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_i32(), "tat_multi".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut *program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data
            .new_basic_block()
            .basic_block("exit".into(), vec![i32.clone()]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let seven = data.new_local_inst().integer(7);
        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![seven, zero]);
        data.layout_mut().insert_inst(entry, seven);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, entry_jump);
        let passthrough_i = data.bb_data(header).params()[0];
        let iv = data.bb_data(header).params()[1];
        let bound = data.new_local_inst().integer(bound);
        let cond = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        let header_br = data
            .new_local_inst()
            .branch(cond, latch, vec![], exit, vec![seven]);
        // Constants never occupy layout; the header holds exactly
        // [lt, branch] for the test-at-top shape.
        data.layout_mut().insert_inst(header, cond);
        data.layout_mut().insert_inst(header, header_br);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let sum = lb.binary(BinaryOp::Add, load_a, passthrough_i);
        let gep_b = lb.get_elem_ptr(b, vec![zero, iv]);
        let store = lb.store(sum, gep_b);
        drop(lb);
        let iv_next = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        // The outer IV is forwarded unchanged; the IV steps by one.
        let latch_jump = data
            .new_local_inst()
            .jump(header, vec![passthrough_i, iv_next]);
        for inst in [
            one, gep_a, load_a, sum, gep_b, store, iv_next, latch_jump,
        ] {
            data.layout_mut().insert_inst(latch, inst);
        }
        let exit_param = data.bb_data(exit).params()[0];
        let ret = data.new_local_inst().ret(Some(exit_param));
        data.layout_mut().insert_inst(exit, ret);
        (function, header, latch, exit, seven)
    }

    /// Build a rotated multi-parameter loop whose exit carries the IV's
    /// final value (the A4 shape, mirroring 01_mm1's kernel
    /// `while_end_18(%vid_4, %122, %vid_6)`): header `[passthrough_i, iv,
    /// counter]`, latch payload `b[i] = a[i] + passthrough_i`, latch
    /// terminator `br t', header([passthrough_i, iv', t']),
    /// exit([passthrough_i, iv'])` — the exit's second parameter receives
    /// the IV update value (`iv + 1` at the last scalar iteration).
    #[allow(clippy::type_complexity)]
    fn build_rotated_exit_iv_final(
        program: &mut Program,
        trip: i32,
    ) -> (Function, BasicBlock, BasicBlock, BasicBlock, Inst) {
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr.clone());
            program.new_value().global_alloc(init)
        };
        let b = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_i32(), "exit_iv_final".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut *program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone(), i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data
            .new_basic_block()
            .basic_block("exit".into(), vec![i32.clone(), i32.clone()]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let seven = data.new_local_inst().integer(7);
        let zero = data.new_local_inst().integer(0);
        let trip_inst = data.new_local_inst().integer(trip);
        let entry_jump = data.new_local_inst().jump(header, vec![seven, zero, trip_inst]);
        data.layout_mut().insert_inst(entry, seven);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, trip_inst);
        data.layout_mut().insert_inst(entry, entry_jump);
        let passthrough_i = data.bb_data(header).params()[0];
        let iv = data.bb_data(header).params()[1];
        let counter = data.bb_data(header).params()[2];
        let header_jump = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, header_jump);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let sum = lb.binary(BinaryOp::Add, load_a, passthrough_i);
        let gep_b = lb.get_elem_ptr(b, vec![zero, iv]);
        let store = lb.store(sum, gep_b);
        drop(lb);
        let iv_next = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let t_next = data.new_local_inst().binary(BinaryOp::Sub, counter, one);
        // The exit receives the passthrough value and the IV's update value
        // (the final `j` of the scalar loop, `i0 + trip`).
        let back = data.new_local_inst().branch(
            t_next,
            header,
            vec![passthrough_i, iv_next, t_next],
            exit,
            vec![passthrough_i, iv_next],
        );
        for inst in [one, gep_a, load_a, sum, gep_b, store, iv_next, t_next, back] {
            data.layout_mut().insert_inst(latch, inst);
        }
        let exit_p = data.bb_data(exit).params()[0];
        let exit_f = data.bb_data(exit).params()[1];
        let use_params = data.new_local_inst().binary(BinaryOp::Add, exit_p, exit_f);
        let ret = data.new_local_inst().ret(Some(use_params));
        data.layout_mut().insert_inst(exit, use_params);
        data.layout_mut().insert_inst(exit, ret);
        (function, header, latch, exit, seven)
    }

    #[test]
    fn vectorizes_multi_param_test_at_top() {
        // A3: test-at-top with a constant bound (trip = 16, no remainder)
        // and an extra loop-invariant passthrough parameter (outer IV)
        // forwarded unchanged by the latch. The counter is materialized as
        // a new header parameter, the payload vectorizes, and the
        // passthrough rides through the rewritten exit edge.
        let mut program = Program::new();
        let (function, header, latch, exit, seven) =
            build_multi_param_test_at_top(&mut program, 16);
        assert!(
            run(&mut program, function),
            "multi-param test-at-top loop must vectorize"
        );
        let data = program.func_data(function);
        assert_eq!(vector_load_count(&program, function), 1);
        assert_eq!(scalar_load_count(&program, function), 0);
        assert_eq!(
            vector_binaries(&program, function),
            vec![BinaryOp::Add],
            "the update becomes a lane-wise add"
        );
        // The header gained the materialized counter as its last parameter:
        // [passthrough_i, iv, t].
        let params = data.bb_data(header).params();
        assert_eq!(params.len(), 3, "counter appended after [passthrough_i, iv]");
        assert!(
            data.inst_data(params[2]).ty().is_i32(),
            "materialized counter stays scalar i32"
        );
        // The bound test is replaced by the counter test, and the exit edge
        // forwards the loop-invariant passthrough value.
        let term = data.layout().basicblock(header).terminator();
        let InstKind::Branch(branch) = data.inst_data(term).kind() else {
            panic!("header must end in a branch");
        };
        assert_eq!(
            branch.cond(),
            params[2],
            "bound test must become the counter test"
        );
        assert_eq!(branch.f_target(), exit);
        assert_eq!(
            branch.f_args().to_vec(),
            vec![seven],
            "passthrough value forwarded to the exit"
        );
        // The latch jump now carries [passthrough_i, iv', t'].
        let latch_term = data.layout().basicblock(latch).terminator();
        let InstKind::Jump(jump) = data.inst_data(latch_term).kind() else {
            panic!("test-at-top latch must end in a jump");
        };
        assert_eq!(
            jump.target(),
            header,
            "latch still jumps back to the header"
        );
        assert_eq!(jump.args().len(), 3, "latch forwards t' alongside iv'");
        let InstKind::Binary(t_sub) = data.inst_data(jump.args()[2]).kind() else {
            panic!("latch's third arg must be the counter step");
        };
        assert_eq!(t_sub.op(), BinaryOp::Sub);
        assert_eq!(t_sub.lhs(), params[2], "t' = t - 4");
        // The entry edge feeds 4Q = 16 into the new counter slot.
        let entry_bb = data.layout().entry_bb().expect("entry block").bb();
        let entry_edge = data.layout().basicblock(entry_bb).terminator();
        let InstKind::Jump(entry_jump) = data.inst_data(entry_edge).kind() else {
            panic!("entry must be a plain jump");
        };
        assert_eq!(entry_jump.args().len(), 3, "entry feeds the counter");
        let InstKind::Integer(init) = data.inst_data(entry_jump.args()[2]).kind() else {
            panic!("counter entry must be the constant 4Q");
        };
        assert_eq!(init.value(), 16, "counter enters at 4Q");
    }

    #[test]
    fn vectorizes_exit_iv_final() {
        // A4: rotated loop `header([passthrough_i, iv, t])` whose exit
        // takes `[passthrough_i, iv_final]` — the latch passes the IV's
        // update value (`iv' = add(iv, 1)`) as the final `j`, the
        // 01_mm1 kernel shape. The exit edge must feed the computed
        // `i0 + VF*q = 16` instead of the latch's per-iteration update,
        // and the back edge keeps stepping the IV by 4.
        let mut program = Program::new();
        let (function, header, latch, exit, seven) =
            build_rotated_exit_iv_final(&mut program, 16);
        assert!(
            run(&mut program, function),
            "exit-iv-final loop must vectorize"
        );
        let data = program.func_data(function);
        // 2x unroll: two vector loads, two lane-wise adds.
        assert_eq!(vector_load_count(&program, function), 2);
        assert_eq!(scalar_load_count(&program, function), 0);
        assert_eq!(
            vector_binaries(&program, function),
            vec![BinaryOp::Add, BinaryOp::Add],
            "2x unroll: the payload add becomes two lane-wise adds"
        );
        // Rotated header parameters are untouched: [passthrough_i, iv, t].
        let params = data.bb_data(header).params();
        assert_eq!(params.len(), 3, "rotated header keeps [passthrough_i, iv, t]");
        // r == 0 and no reduction: the latch branch still targets the exit
        // directly; the IV-final slot now carries the computed constant.
        let latch_term = data.layout().basicblock(latch).terminator();
        let InstKind::Branch(branch) = data.inst_data(latch_term).kind() else {
            panic!("latch must end in a branch");
        };
        assert_eq!(branch.f_target(), exit);
        let f_args = branch.f_args().to_vec();
        assert_eq!(f_args.len(), 2, "exit takes [passthrough_i, iv_final]");
        assert_eq!(
            f_args[0], seven,
            "the passthrough value is forwarded unchanged"
        );
        let InstKind::Integer(iv_final) = data.inst_data(f_args[1]).kind() else {
            panic!("the exit's iv-final arg must be the computed constant");
        };
        assert_eq!(
            iv_final.value(),
            16,
            "iv_final = i0 + step*q = 0 + 8*2"
        );
        // The back edge steps the IV by the unrolled vector width.
        let t_args = branch.t_args().to_vec();
        let InstKind::Binary(step) = data.inst_data(t_args[1]).kind() else {
            panic!("back-edge IV arg must be an add");
        };
        assert_eq!(step.op(), BinaryOp::Add);
        assert_eq!(step.lhs(), params[1], "iv' = iv + 8");
        let InstKind::Integer(step_c) = data.inst_data(step.rhs()).kind() else {
            panic!("IV step must be a constant");
        };
        assert_eq!(step_c.value(), 8, "IV steps by the unrolled vector width");
    }

    #[test]
    fn test_at_top_remainder_creates_epilogue() {
        // trip = 18 = 4 * 4 + 2: two scalar epilogue blocks are peeled.
        let mut program = Program::new();
        let function = build_test_at_top(&mut program, 18);
        assert!(
            run(&mut program, function),
            "test-at-top loop with remainder must vectorize with epilogue"
        );
        let data = program.func_data(function);
        // 4 original blocks (entry/header/latch/exit) + 2 peeled epilogue
        // blocks.
        let epi_count = data.layout().basicblocks().len() - 4;
        assert_eq!(epi_count, 2, "trip 18 = 4*4 + 2 peels two epilogue blocks");
    }

    #[test]
    fn rejects_test_at_top_nonconstant_bound() {
        // `lt iv, iv` — the bound is the IV itself, not a compile-time
        // constant: rejected (no versioning).
        let mut program = Program::new();
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr.clone());
            program.new_value().global_alloc(init)
        };
        let b = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_unit(), "tat_nc".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let cond = data.new_local_inst().binary(BinaryOp::Lt, iv, iv);
        let header_br = data.new_local_inst().branch(cond, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, cond);
        data.layout_mut().insert_inst(header, header_br);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let sum = lb.binary(BinaryOp::Add, load_a, one);
        let gep_b = lb.get_elem_ptr(b, vec![zero, iv]);
        let store = lb.store(sum, gep_b);
        drop(lb);
        let iv_next = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let latch_jump = data.new_local_inst().jump(header, vec![iv_next]);
        for inst in [
            one, gep_a, load_a, sum, gep_b, store, iv_next, latch_jump,
        ] {
            data.layout_mut().insert_inst(latch, inst);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);
        assert!(
            !run(&mut program, function),
            "non-constant bound must stay scalar (no versioning)"
        );
    }

    /// Build a test-at-top single-reduction loop (the B1 shape unlocked by
    /// commit 1): header `[passthrough_i, iv, acc]`, const bound, payload
    /// `b[i] = a[i] + passthrough_i; acc += a[i]`. The exit has no
    /// accumulator parameter and reads the header accumulator directly
    /// (SSA dominance — the real-case shape; the apply phase appends a
    /// scalar exit parameter and rewrites the read). When
    /// `exit_has_passthrough_param` the exit additionally takes the
    /// loop-invariant passthrough value as its (single) original
    /// parameter, so the appended accumulator parameter lands after it.
    #[allow(clippy::type_complexity)]
    fn build_test_at_top_reduction(
        program: &mut Program,
        bound: i32,
        exit_has_passthrough_param: bool,
    ) -> (Function, BasicBlock, BasicBlock, BasicBlock, Inst) {
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr.clone());
            program.new_value().global_alloc(init)
        };
        let b = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_i32(), "tat_red".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut *program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data.new_basic_block().basic_block(
            "header".into(),
            vec![i32.clone(), i32.clone(), i32.clone()],
        );
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = if exit_has_passthrough_param {
            data.new_basic_block()
                .basic_block("exit".into(), vec![i32.clone()])
        } else {
            data.new_basic_block().basic_block("exit".into(), vec![])
        };
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let seven = data.new_local_inst().integer(7);
        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![seven, zero, zero]);
        data.layout_mut().insert_inst(entry, seven);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, entry_jump);
        let passthrough_i = data.bb_data(header).params()[0];
        let iv = data.bb_data(header).params()[1];
        let acc = data.bb_data(header).params()[2];
        let bound_inst = data.new_local_inst().integer(bound);
        let cond = data.new_local_inst().binary(BinaryOp::Lt, iv, bound_inst);
        let header_br = if exit_has_passthrough_param {
            data.new_local_inst()
                .branch(cond, latch, vec![], exit, vec![seven])
        } else {
            data.new_local_inst()
                .branch(cond, latch, vec![], exit, vec![])
        };
        data.layout_mut().insert_inst(header, cond);
        data.layout_mut().insert_inst(header, header_br);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let sum = lb.binary(BinaryOp::Add, load_a, passthrough_i);
        let gep_b = lb.get_elem_ptr(b, vec![zero, iv]);
        let store = lb.store(sum, gep_b);
        drop(lb);
        let acc_next = data.new_local_inst().binary(BinaryOp::Add, acc, load_a);
        let iv_next = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        // The passthrough is forwarded unchanged; the IV and accumulator
        // update.
        let latch_jump = data
            .new_local_inst()
            .jump(header, vec![passthrough_i, iv_next, acc_next]);
        for inst in [
            one, gep_a, load_a, sum, gep_b, store, acc_next, iv_next, latch_jump,
        ] {
            data.layout_mut().insert_inst(latch, inst);
        }
        // The exit reads the header accumulator parameter directly (SSA
        // dominance) — the real-case shape.
        let ret = data.new_local_inst().ret(Some(acc));
        data.layout_mut().insert_inst(exit, ret);
        (function, header, latch, exit, seven)
    }

    #[test]
    fn vectorizes_test_at_top_with_invariant_constant_args() {
        // A3 extension: a test-at-top loop whose header carries the IV plus
        // loop-invariant *constant* back-edge arguments (not the parameter
        // itself — e.g. crypto's md5/sha1 state loops forward constants like
        // 256000 / 1 alongside the real IV). Those invariants must not count
        // as effective parameters, or `test_at_top_multi_param` rejects the
        // shape even though the payload is a plain elementwise loop.
        let mut program = Program::new();
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr.clone());
            program.new_value().global_alloc(init)
        };
        let b = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_unit(), "tat_inv".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        // header = [iv, const_k1, const_k2].
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone(), i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let k1 = data.new_local_inst().integer(7);
        let k2 = data.new_local_inst().integer(256000);
        let zero = data.new_local_inst().integer(0);
        let iv = data.bb_data(header).params()[0];
        let p1 = data.bb_data(header).params()[1];
        let p2 = data.bb_data(header).params()[2];
        let entry_jump = data.new_local_inst().jump(header, vec![zero, k1, k2]);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, k1);
        data.layout_mut().insert_inst(entry, k2);
        data.layout_mut().insert_inst(entry, entry_jump);
        let bound = data.new_local_inst().integer(16);
        let cond = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        let header_br = data
            .new_local_inst()
            .branch(cond, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, cond);
        data.layout_mut().insert_inst(header, header_br);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let sum = lb.binary(BinaryOp::Add, load_a, p1);
        let gep_b = lb.get_elem_ptr(b, vec![zero, iv]);
        let store = lb.store(sum, gep_b);
        drop(lb);
        let iv_next = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        // The two invariant constants are re-forwarded unchanged (their back
        // arguments are the constants themselves, not the header parameters
        // — the crypto md5/sha1 shape); only the IV steps by one.
        let latch_jump = data
            .new_local_inst()
            .jump(header, vec![iv_next, k1, k2]);
        for inst in [
            one, gep_a, load_a, sum, gep_b, store, iv_next, latch_jump,
        ] {
            data.layout_mut().insert_inst(latch, inst);
        }
        assert!(
            run(&mut program, function),
            "test-at-top with invariant constant args must vectorize"
        );
        let data = program.func_data(function);
        assert_eq!(vector_load_count(&program, function), 1);
        assert_eq!(vector_binaries(&program, function), vec![BinaryOp::Add]);
    }

    #[test]
    fn vectorizes_test_at_top_reduction_direct_exit_read() {
        // Const-bound test-at-top [passthrough_i, iv, acc] with a
        // param-less exit that reads the header accumulator directly: the
        // accumulator vectorizes, a reduce block delivers the scalar sum,
        // the apply phase appends a scalar exit parameter and rewrites the
        // read.
        let mut program = Program::new();
        let (function, header, _latch, exit, _seven) =
            build_test_at_top_reduction(&mut program, 16, false);
        assert!(
            run(&mut program, function),
            "param-less test-at-top exit reading acc must vectorize"
        );
        let data = program.func_data(function);
        assert_eq!(vector_load_count(&program, function), 1);
        assert_eq!(scalar_load_count(&program, function), 0);
        // Exactly one horizontal reduction (the accumulator's addv).
        let reduce_count = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .filter(|&inst| matches!(data.inst_data(inst).kind(), InstKind::VectorReduce(_)))
            .count();
        assert_eq!(reduce_count, 1, "one addv for the accumulator");
        // The exit gained a single scalar parameter; its terminator reads
        // that parameter instead of the (now vector) header accumulator.
        let exit_params = data.bb_data(exit).params();
        assert_eq!(exit_params.len(), 1, "one scalar param added to the exit");
        let exit_ret = data.layout().basicblock(exit).terminator();
        let InstKind::Return(ret) = data.inst_data(exit_ret).kind() else {
            panic!("exit must return");
        };
        assert_eq!(
            ret.value(),
            Some(exit_params[0]),
            "the exit's direct acc read is rewritten to the new param"
        );
        // The header false edge carries the vector accumulator to the
        // reduce block; the reduce block feeds the scalar sum to the exit.
        let term = data.layout().basicblock(header).terminator();
        let InstKind::Branch(branch) = data.inst_data(term).kind() else {
            panic!("header must end in a branch");
        };
        let f_target = branch.f_target();
        assert!(
            data.bb_data(f_target).name().starts_with("vec_reduce"),
            "exit edge enters the reduce block"
        );
        let f_args = branch.f_args().to_vec();
        assert_eq!(f_args.len(), 2, "reduce block takes [acc_vec, passthrough]");
        assert!(
            data.inst_data(f_args[0]).ty().is_vector(),
            "the reduce block receives the vector accumulator"
        );
        let reduce_jump = data.layout().basicblock(f_target).terminator();
        let InstKind::Jump(jump) = data.inst_data(reduce_jump).kind() else {
            panic!("reduce block must end in a jump");
        };
        assert_eq!(jump.target(), exit);
        assert_eq!(jump.args().len(), 1, "the sum feeds the added exit param");
        assert!(
            data.inst_data(jump.args()[0]).ty().is_i32(),
            "the added exit param receives the scalar sum"
        );
        // Idempotence: a second application changes nothing.
        assert!(!run(&mut program, function), "vectorized loop must not re-fire");
    }

    #[test]
    fn vectorizes_test_at_top_reduction_with_passthrough_exit_param() {
        // The exit already takes the loop-invariant passthrough value as
        // its single parameter; the accumulator parameter is appended
        // after it and fed the reduced sum.
        let mut program = Program::new();
        let (function, header, _latch, exit, seven) =
            build_test_at_top_reduction(&mut program, 16, true);
        assert!(
            run(&mut program, function),
            "test-at-top reduction with a passthrough exit param must vectorize"
        );
        let data = program.func_data(function);
        let exit_params = data.bb_data(exit).params();
        assert_eq!(
            exit_params.len(),
            2,
            "the accumulator param is appended after the passthrough"
        );
        let exit_ret = data.layout().basicblock(exit).terminator();
        let InstKind::Return(ret) = data.inst_data(exit_ret).kind() else {
            panic!("exit must return");
        };
        assert_eq!(
            ret.value(),
            Some(exit_params[1]),
            "the direct acc read is rewritten to the appended param"
        );
        // The reduce block jump feeds [passthrough, sum].
        let term = data.layout().basicblock(header).terminator();
        let InstKind::Branch(branch) = data.inst_data(term).kind() else {
            panic!("header must end in a branch");
        };
        let reduce_jump = data.layout().basicblock(branch.f_target()).terminator();
        let InstKind::Jump(jump) = data.inst_data(reduce_jump).kind() else {
            panic!("reduce block must end in a jump");
        };
        assert_eq!(jump.target(), exit);
        assert_eq!(jump.args().len(), 2, "exit receives [passthrough, sum]");
        assert_eq!(jump.args()[0], seven, "passthrough forwarded unchanged");
        assert!(
            data.inst_data(jump.args()[1]).ty().is_i32(),
            "the appended param receives the scalar sum"
        );
        assert!(!run(&mut program, function), "vectorized loop must not re-fire");
    }

    #[test]
    fn rejects_test_at_top_non_reducible_effective_2() {
        // A second effective parameter that is not a B1 accumulator must
        // stay rejected: `[iv, x]` with `x' = rem(x, 5)` (the get_random
        // shape). Either M42 refuses a Reducible verdict or the
        // accumulator checks reject it — in both cases the loop stays
        // scalar.
        let mut program = Program::new();
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr.clone());
            program.new_value().global_alloc(init)
        };
        let b = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_unit(), "tat_rem".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        let five = data.new_local_inst().integer(5);
        let entry_jump = data.new_local_inst().jump(header, vec![zero, zero]);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, five);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let x = data.bb_data(header).params()[1];
        let bound = data.new_local_inst().integer(16);
        let cond = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        let header_br = data.new_local_inst().branch(cond, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, cond);
        data.layout_mut().insert_inst(header, header_br);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let sum = lb.binary(BinaryOp::Add, load_a, one);
        let gep_b = lb.get_elem_ptr(b, vec![zero, iv]);
        let store = lb.store(sum, gep_b);
        drop(lb);
        // x' = rem(x, 5): a second effective parameter that is neither a
        // passthrough nor a reducible accumulator.
        let x_next = data.new_local_inst().binary(BinaryOp::Rem, x, five);
        let iv_next = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let latch_jump = data.new_local_inst().jump(header, vec![iv_next, x_next]);
        for inst in [
            one, gep_a, load_a, sum, gep_b, store, x_next, iv_next, latch_jump,
        ] {
            data.layout_mut().insert_inst(latch, inst);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);
        assert!(
            !run(&mut program, function),
            "non-reducible effective==2 test-at-top loop must stay scalar"
        );
    }

    /// Build a test-at-top elementwise loop with a *runtime* bound:
    /// `n` is a scalar global read in the entry block
    /// (`bound = load n`), and the loop is
    /// `while (iv < bound) { b[iv] = a[iv] + 1; iv++; }`. When
    /// `exit_reads_iv` the exit returns the header IV parameter directly
    /// (rewritten to the final value by the apply phase).
    fn build_runtime_bound_test_at_top(
        program: &mut Program,
        exit_reads_iv: bool,
    ) -> (Function, BasicBlock, BasicBlock, BasicBlock, Inst) {
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr.clone());
            program.new_value().global_alloc(init)
        };
        let b = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let n = {
            let init = program.new_value().zero_init(i32.clone());
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(
            if exit_reads_iv {
                Type::get_i32()
            } else {
                Type::get_unit()
            },
            "tat_rt".into(),
            vec![],
        );
        let mut data = ArenaContextMut {
            program: &mut *program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        // The bound is a global load: it must go through a
        // `dyn Arena` builder (FunctionData-rooted builders cannot touch
        // global operands).
        let bound = {
            let mut lb = LocalBuilder {
                arena: &mut data as &mut dyn Arena,
            };
            lb.load(n)
        };
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, bound);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let cond = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        let header_br = data.new_local_inst().branch(cond, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, cond);
        data.layout_mut().insert_inst(header, header_br);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let sum = lb.binary(BinaryOp::Add, load_a, one);
        let gep_b = lb.get_elem_ptr(b, vec![zero, iv]);
        let store = lb.store(sum, gep_b);
        drop(lb);
        let iv_next = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let latch_jump = data.new_local_inst().jump(header, vec![iv_next]);
        for inst in [
            one, gep_a, load_a, sum, gep_b, store, iv_next, latch_jump,
        ] {
            data.layout_mut().insert_inst(latch, inst);
        }
        let ret = if exit_reads_iv {
            data.new_local_inst().ret(Some(iv))
        } else {
            data.new_local_inst().ret(None)
        };
        data.layout_mut().insert_inst(exit, ret);
        (function, header, latch, exit, bound)
    }

    #[test]
    fn vectorizes_runtime_bound_test_at_top() {
        // A runtime bound (global load) unlocks the vector loop via a
        // runtime counter (`trip & -4`) plus a scalar tail loop for the
        // remainder. The tail must not be re-vectorized (idempotence).
        let mut program = Program::new();
        let (function, header, _latch, _exit, _bound) =
            build_runtime_bound_test_at_top(&mut program, false);
        assert!(
            run(&mut program, function),
            "runtime-bound test-at-top elementwise loop must vectorize"
        );
        let data = program.func_data(function);
        assert_eq!(vector_load_count(&program, function), 1);
        assert_eq!(
            scalar_load_count(&program, function),
            2,
            "the tail keeps one scalar clone of a[i] + the entry's bound load"
        );
        // The tail loop exists.
        let tail = data
            .layout()
            .basicblocks()
            .iter()
            .map(|l| l.bb())
            .find(|&bb| data.bb_data(bb).name().starts_with("vec_tail"))
            .expect("tail loop must exist");
        assert_eq!(
            data.bb_data(tail).params().len(),
            1,
            "tail header carries [iv_t]"
        );
        // The header's bound test became the runtime counter test
        // `gt(counter, 0)`.
        let term = data.layout().basicblock(header).terminator();
        let InstKind::Branch(branch) = data.inst_data(term).kind() else {
            panic!("header must end in a branch");
        };
        let InstKind::Binary(gt) = data.inst_data(branch.cond()).kind() else {
            panic!("runtime counter test must be a comparison");
        };
        assert_eq!(gt.op(), BinaryOp::Gt, "counter test is gt(counter, 0)");
        // The counter enters at `cnt0 = and(sub(bound, i0), -4)`.
        let entry_bb = data.layout().entry_bb().expect("entry block").bb();
        let entry_edge = data.layout().basicblock(entry_bb).terminator();
        let InstKind::Jump(entry_jump) = data.inst_data(entry_edge).kind() else {
            panic!("entry must be a plain jump");
        };
        let counter_arg = entry_jump.args()[1];
        let InstKind::Binary(and) = data.inst_data(counter_arg).kind() else {
            panic!("counter entry must be `and(trip, -4)`");
        };
        assert_eq!(and.op(), BinaryOp::And);
        let InstKind::Integer(neg4) = data.inst_data(and.rhs()).kind() else {
            panic!("mask must be the constant -4");
        };
        assert_eq!(neg4.value(), -4);
        let InstKind::Binary(sub) = data.inst_data(and.lhs()).kind() else {
            panic!("trip must be `sub(bound, i0)`");
        };
        assert_eq!(sub.op(), BinaryOp::Sub);
        // The tail entry carries `iv0 = i0 + select(gt(cnt0, 0), cnt0, 0)`
        // (the clamp is a scalar select — the backend has no scalar
        // min/max).
        let f_args = branch.f_args().to_vec();
        let InstKind::Binary(iv0) = data.inst_data(f_args[0]).kind() else {
            panic!("tail entry index must be an add");
        };
        assert_eq!(iv0.op(), BinaryOp::Add);
        let InstKind::Select(clamp) = data.inst_data(iv0.rhs()).kind() else {
            panic!("tail entry index must clamp cnt0 with a select");
        };
        let InstKind::Binary(gt) = data.inst_data(clamp.cond()).kind() else {
            panic!("the clamp's condition must be the sign test");
        };
        assert_eq!(gt.op(), BinaryOp::Gt);
        // The header's false edge targets the tail.
        assert!(
            data.bb_data(branch.f_target()).name().starts_with("vec_tail"),
            "counter-zero edge enters the tail"
        );
        // Idempotence: the tail (and the vector loop) must not re-fire.
        assert!(!run(&mut program, function), "tail must not be re-vectorized");
    }

    #[test]
    fn vectorizes_runtime_bound_tail_small_trip() {
        // A runtime bound whose value is 1..=3 (trip below one vector
        // iteration) still vectorizes structurally: `cnt0 = 0` skips the
        // vector loop and the tail runs the whole trip. The gate is
        // exercised through the same IR shape — the numeric trip is only
        // known at runtime, so the analysis cannot reject it.
        let mut program = Program::new();
        let (function, _header, _latch, _exit, _bound) =
            build_runtime_bound_test_at_top(&mut program, false);
        assert!(
            run(&mut program, function),
            "runtime trip below VF must still vectorize (tail covers it)"
        );
        let data = program.func_data(function);
        let tail = data
            .layout()
            .basicblocks()
            .iter()
            .map(|l| l.bb())
            .find(|&bb| data.bb_data(bb).name().starts_with("vec_tail"))
            .expect("tail loop must exist");
        // The tail's latch holds the scalar payload (one scalar load).
        let tail_latch = data
            .layout()
            .basicblocks()
            .iter()
            .map(|l| l.bb())
            .find(|&bb| data.bb_data(bb).name().starts_with("vec_tail_latch"))
            .expect("tail latch must exist");
        let scalar_loads = data
            .layout()
            .basicblock(tail_latch)
            .insts()
            .iter()
            .copied()
            .filter(|&inst| {
                matches!(data.inst_data(inst).kind(), InstKind::Load(_))
                    && !data.inst_data(inst).ty().is_vector()
            })
            .count();
        assert_eq!(scalar_loads, 1, "the tail's payload stays scalar");
        let _ = tail;
        assert!(!run(&mut program, function), "idempotent");
    }

    #[test]
    fn runtime_bound_rewrites_exit_iv_read() {
        // The exit returns the header IV parameter directly: the apply
        // phase rewrites the read to `select(gt(trip, 0), bound, i0)` —
        // the final index is the bound when the loop ran, `i0` when the
        // trip was non-positive.
        let mut program = Program::new();
        let (function, _header, _latch, exit, _bound) =
            build_runtime_bound_test_at_top(&mut program, true);
        assert!(
            run(&mut program, function),
            "runtime-bound loop with an iv-reading exit must vectorize"
        );
        let data = program.func_data(function);
        let exit_ret = data.layout().basicblock(exit).terminator();
        let InstKind::Return(ret) = data.inst_data(exit_ret).kind() else {
            panic!("exit must return");
        };
        let final_value = ret.value().expect("exit returns a value");
        let InstKind::Select(sel) = data.inst_data(final_value).kind() else {
            panic!("the rewritten final IV must be a select");
        };
        let InstKind::Binary(gt) = data.inst_data(sel.cond()).kind() else {
            panic!("the select's condition must be the trip sign test");
        };
        assert_eq!(gt.op(), BinaryOp::Gt);
        assert!(!run(&mut program, function), "idempotent");
    }

    /// Build a rotated (test-at-bottom) elementwise loop with a *runtime*
    /// trip counter — the shape of 01_mm1's matrix-multiply kernel:
    /// `header([passthrough, iv, counter])` entered from the preheader at
    /// `[p, i0, t0]` (t0 = a runtime value, `rotate_loops`'s
    /// `t0 = bound - i0`), payload `b[iv] = a[iv] + p`, and the exit
    /// receiving `[p, iv']` (a passthrough and the IV's final value).
    /// `guard_entry` selects the entry edge shape: a plain jump (default,
    /// like preheader_32 in 01_mm1) or a guard branch
    /// (`br gt(t0, 0), header, exit` — the rotate_loops negative-trip
    /// guard).
    fn build_rotated_runtime_elementwise(
        program: &mut Program,
        guard_entry: bool,
    ) -> (Function, BasicBlock, BasicBlock, BasicBlock, Inst) {
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr.clone());
            program.new_value().global_alloc(init)
        };
        let b = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let n = {
            let init = program.new_value().zero_init(i32.clone());
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_i32(), "rot_rt".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut *program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone(), i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data
            .new_basic_block()
            .basic_block("exit".into(), vec![i32.clone(), i32.clone()]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let seven = data.new_local_inst().integer(7);
        let zero = data.new_local_inst().integer(0);
        // The runtime trip: a global load (goes through `dyn Arena`).
        let t0 = {
            let mut lb = LocalBuilder {
                arena: &mut data as &mut dyn Arena,
            };
            lb.load(n)
        };
        let entry_edge = if guard_entry {
            let guard = data.new_local_inst().binary(BinaryOp::Gt, t0, zero);
            let br = data.new_local_inst().branch(
                guard,
                header,
                vec![seven, zero, t0],
                exit,
                vec![seven, zero],
            );
            data.layout_mut().insert_inst(entry, zero);
            data.layout_mut().insert_inst(entry, t0);
            data.layout_mut().insert_inst(entry, guard);
            data.layout_mut().insert_inst(entry, br);
            br
        } else {
            let jmp = data.new_local_inst().jump(header, vec![seven, zero, t0]);
            data.layout_mut().insert_inst(entry, zero);
            data.layout_mut().insert_inst(entry, t0);
            data.layout_mut().insert_inst(entry, jmp);
            jmp
        };
        let p = data.bb_data(header).params()[0];
        let iv = data.bb_data(header).params()[1];
        let counter = data.bb_data(header).params()[2];
        let header_jump = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, header_jump);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let sum = lb.binary(BinaryOp::Add, load_a, p);
        let gep_b = lb.get_elem_ptr(b, vec![zero, iv]);
        let store = lb.store(sum, gep_b);
        drop(lb);
        let iv_next = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let t_next = data.new_local_inst().binary(BinaryOp::Sub, counter, one);
        let back = data.new_local_inst().branch(
            t_next,
            header,
            vec![p, iv_next, t_next],
            exit,
            vec![p, iv_next],
        );
        for inst in [one, gep_a, load_a, sum, gep_b, store, iv_next, t_next, back] {
            data.layout_mut().insert_inst(latch, inst);
        }
        let exit_p = data.bb_data(exit).params()[0];
        let exit_f = data.bb_data(exit).params()[1];
        let use_params = data.new_local_inst().binary(BinaryOp::Add, exit_p, exit_f);
        let ret = data.new_local_inst().ret(Some(use_params));
        data.layout_mut().insert_inst(exit, use_params);
        data.layout_mut().insert_inst(exit, ret);
        (function, header, latch, exit, entry_edge)
    }

    #[test]
    fn vectorizes_rotated_runtime_elementwise() {
        // A rotated loop whose trip counter enters at a runtime value
        // (01_mm1's mm-kernel shape) vectorizes via the runtime counter
        // (`cnt0 = t0 & -4`) plus a scalar tail for the `t0 & 3`
        // remainder. The exit receives the passthrough and the IV's
        // *runtime* final value (`i0 + t0`, the rebuilt bound).
        let mut program = Program::new();
        let (function, header, latch, exit, _entry_edge) =
            build_rotated_runtime_elementwise(&mut program, false);
        assert!(
            run(&mut program, function),
            "rotated runtime-bound elementwise loop must vectorize"
        );
        let data = program.func_data(function);
        // 2x unroll: two vector loads.
        assert_eq!(vector_load_count(&program, function), 2);
        // The vectorized latch tests `gt(counter, 0)` and keeps looping on
        // the true edge; the counter's back-edge arg steps by 4.
        let latch_term = data.layout().basicblock(latch).terminator();
        let InstKind::Branch(branch) = data.inst_data(latch_term).kind() else {
            panic!("latch must end in the rewritten branch");
        };
        let InstKind::Binary(gt) = data.inst_data(branch.cond()).kind() else {
            panic!("runtime counter test must be a comparison");
        };
        assert_eq!(gt.op(), BinaryOp::Gt, "counter test is gt(counter, 0)");
        assert_eq!(
            branch.t_target(),
            header,
            "the true edge keeps looping"
        );
        // The false edge enters the scalar tail at `iv0 = i0 + max(cnt0,0)`
        // with the passthrough forwarded.
        let f_args = branch.f_args().to_vec();
        assert!(
            data.bb_data(branch.f_target()).name().starts_with("vec_tail"),
            "counter-zero edge enters the tail"
        );
        assert_eq!(f_args.len(), 2, "tail entry is [iv0, passthrough]");
        // The tail's own exit edge feeds the passthrough and the rebuilt
        // runtime bound `i0 + t0` as the IV final value.
        let tail = data
            .layout()
            .basicblocks()
            .iter()
            .map(|l| l.bb())
            .find(|&bb| data.bb_data(bb).name().starts_with("vec_tail"))
            .expect("tail loop must exist");
        assert_eq!(
            data.bb_data(tail).params().len(),
            2,
            "tail header carries [iv_t, passthrough]"
        );
        let tail_term = data.layout().basicblock(tail).terminator();
        let InstKind::Branch(tb) = data.inst_data(tail_term).kind() else {
            panic!("tail must end in a branch");
        };
        assert_eq!(tb.f_target(), exit, "tail exits to the loop exit");
        let exit_args = tb.f_args().to_vec();
        assert_eq!(exit_args.len(), 2);
        let InstKind::Binary(bound_add) = data.inst_data(exit_args[1]).kind() else {
            panic!("the IV final value must be the rebuilt bound");
        };
        assert_eq!(
            bound_add.op(),
            BinaryOp::Add,
            "rotated tail bound is `i0 + counter_entry`"
        );
        // The entry is a guarded branch: `gt(cnt0, 0)` — the vector loop
        // is a do-while, so a zero/negative `cnt0` (trip < 4) must skip
        // it entirely and enter the scalar tail directly. The true edge
        // carries `cnt0 = and(t0, -4)`.
        let entry_bb = data.layout().entry_bb().expect("entry block").bb();
        let entry_edge = data.layout().basicblock(entry_bb).terminator();
        let InstKind::Branch(entry_branch) = data.inst_data(entry_edge).kind() else {
            panic!("entry must be a guard branch");
        };
        let InstKind::Binary(guard_gt) = data.inst_data(entry_branch.cond()).kind() else {
            panic!("entry guard must be a comparison");
        };
        assert_eq!(guard_gt.op(), BinaryOp::Gt);
        let InstKind::Binary(and) = data.inst_data(entry_branch.t_args()[2]).kind() else {
            panic!("counter entry must be `and(t0, -4)`");
        };
        assert_eq!(and.op(), BinaryOp::And);
        let InstKind::Integer(neg8) = data.inst_data(and.rhs()).kind() else {
            panic!("mask must be the constant -8");
        };
        assert_eq!(neg8.value(), -8);
        assert!(!run(&mut program, function), "idempotent");
    }

    #[test]
    fn unrolls_elementwise_payload_2x_with_continuation_geps() {
        // A rotated runtime-bound elementwise loop (01_mm1's shape) is
        // vectorized AND 2x unrolled: each array's two loads/stores are
        // adjacent and share the first copy's base register via a constant
        // continuation GEP `getelemptr %addr, 4`. The counter and IV step by
        // 8 (the tail covers `trip & 7`).
        let mut program = Program::new();
        let (function, _header, latch, _exit, _entry_edge) =
            build_rotated_runtime_elementwise(&mut program, false);
        assert!(
            run(&mut program, function),
            "rotated runtime-bound elementwise loop must vectorize"
        );
        let data = program.func_data(function);
        // Two vector loads (a0/a1), two vector stores (b0/b1).
        assert_eq!(vector_load_count(&program, function), 2);
        // The latch payload is the unrolled body: two continuation GEPs
        // (`getelemptr %base, 4`) — one per distinct base address — plus two
        // vector loads and two vector stores.
        let latch_insts = latch_of(&program, function, latch);
        let cont_geps = latch_insts
            .iter()
            .filter(|&&inst| {
                matches!(
                    data.inst_data(inst).kind(),
                    InstKind::GetElemPtr(gep)
                        if gep.offsets().len() == 1
                            && matches!(data.inst_data(gep.offsets()[0]).kind(), InstKind::Integer(v) if v.value() == 4)
                )
            })
            .count();
        assert_eq!(cont_geps, 2, "one continuation GEP per base address");
        let vec_loads = latch_insts
            .iter()
            .filter(|&&inst| {
                matches!(data.inst_data(inst).kind(), InstKind::Load(_))
                    && data.inst_data(inst).ty().is_vector()
            })
            .count();
        let vec_stores = latch_insts
            .iter()
            .filter(|&&inst| matches!(data.inst_data(inst).kind(), InstKind::Store(_)))
            .count();
        assert_eq!(vec_loads, 2, "both copies' loads live in the latch");
        assert_eq!(vec_stores, 2, "both copies' stores live in the latch");
        // The latch branch's counter back-edge arg steps by 8.
        let latch_term = data.layout().basicblock(latch).terminator();
        let InstKind::Branch(branch) = data.inst_data(latch_term).kind() else {
            panic!("latch must end in the rewritten branch");
        };
        let t_args = branch.t_args().to_vec();
        let InstKind::Binary(sub) = data.inst_data(t_args[2]).kind() else {
            panic!("counter back-edge arg must be a sub");
        };
        assert_eq!(sub.op(), BinaryOp::Sub);
        let InstKind::Integer(step) = data.inst_data(sub.rhs()).kind() else {
            panic!("counter step must be a constant");
        };
        assert_eq!(step.value(), 8, "2x unroll: counter steps by 8");
    }

    #[test]
    fn vectorizes_rotated_runtime_guard_entry() {
        // The rotate_loops negative-trip guard (`br gt(t0, 0), header,
        // exit`) is a legal entry edge: the guard's own args are
        // preserved (they carry the "loop never ran" values), only the
        // header edge's counter slot is replaced by `cnt0`.
        let mut program = Program::new();
        let (function, _header, _latch, exit, _entry_edge) =
            build_rotated_runtime_elementwise(&mut program, true);
        assert!(
            run(&mut program, function),
            "rotated runtime loop with a guard-branch entry must vectorize"
        );
        let data = program.func_data(function);
        // The guard edge into the exit still feeds the original values
        // (the loop-skipped path).
        let entry_bb = data.layout().entry_bb().expect("entry block").bb();
        let entry_edge = data.layout().basicblock(entry_bb).terminator();
        let InstKind::Branch(guard) = data.inst_data(entry_edge).kind() else {
            panic!("guard entry must be a branch");
        };
        assert_eq!(guard.f_target(), exit, "guard false edge targets the exit");
        // The header edge's counter arg was replaced by `cnt0`.
        let InstKind::Binary(and) = data.inst_data(guard.t_args()[2]).kind() else {
            panic!("counter entry must be `and(t0, -4)`");
        };
        assert_eq!(and.op(), BinaryOp::And);
        // The loop-skipped path keeps the original args (no seed plumbing
        // for elementwise): the guard's false edge still feeds the exit's
        // original values `[passthrough, i0]`.
        assert_eq!(
            guard.f_args().to_vec(),
            vec![guard.f_args()[0], guard.f_args()[1]],
            "guard edge args are preserved"
        );
        assert!(!run(&mut program, function), "idempotent");
    }

    /// Build a rotated reduction loop with a *runtime* trip counter (the
    /// h-5-style shape): `header([acc, iv, counter])` entered at
    /// `[seed, i0, t0]` (t0 = a global load), payload
    /// `acc' = acc + load(a[iv])`, exit receiving `[acc']`.
    fn build_rotated_runtime_reduction(
        program: &mut Program,
    ) -> (Function, BasicBlock, BasicBlock, BasicBlock, Inst, Inst, Inst) {
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let n = {
            let init = program.new_value().zero_init(i32.clone());
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_i32(), "rot_rt_red".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut *program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone(), i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data
            .new_basic_block()
            .basic_block("exit".into(), vec![i32.clone()]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        // The runtime trip: a global load (goes through `dyn Arena`).
        let t0 = {
            let mut lb = LocalBuilder {
                arena: &mut data as &mut dyn Arena,
            };
            lb.load(n)
        };
        let entry_jump = data.new_local_inst().jump(header, vec![zero, zero, t0]);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, t0);
        data.layout_mut().insert_inst(entry, entry_jump);
        let acc = data.bb_data(header).params()[0];
        let iv = data.bb_data(header).params()[1];
        let counter = data.bb_data(header).params()[2];
        let header_jump = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, header_jump);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let sum = lb.binary(BinaryOp::Add, acc, load_a);
        let iv_next = lb.binary(BinaryOp::Add, iv, one);
        let t_next = lb.binary(BinaryOp::Sub, counter, one);
        let back = lb.branch(t_next, header, vec![sum, iv_next, t_next], exit, vec![sum]);
        drop(lb);
        for inst in [one, gep_a, load_a, sum, iv_next, t_next, back] {
            data.layout_mut().insert_inst(latch, inst);
        }
        let exit_acc = data.bb_data(exit).params()[0];
        let exit_ret = data.new_local_inst().ret(Some(exit_acc));
        data.layout_mut().insert_inst(exit, exit_ret);
        (function, header, latch, exit, acc, iv, counter)
    }

    #[test]
    fn vectorizes_rotated_runtime_reduction() {
        // A rotated single-reduction loop ([iv, acc]) with a runtime trip
        // counter: the vector loop accumulates lane-wise, the reduce block
        // produces `seed + Σc`, and the scalar tail continues the
        // reduction from there — `reduce -> tail([iv0, sum])`.
        let mut program = Program::new();
        let (function, header, latch, exit, acc, _iv, _counter) =
            build_rotated_runtime_reduction(&mut program);
        assert!(
            run(&mut program, function),
            "rotated runtime-bound reduction loop must vectorize"
        );
        let data = program.func_data(function);
        assert_eq!(vector_load_count(&program, function), 1);
        assert!(
            data.inst_data(acc).ty().is_vector(),
            "the accumulator is re-typed to a vector"
        );
        // The reduce block exists and feeds the scalar tail: its jump
        // target is the tail header with [iv0, sum, passthrough...].
        let reduce = data
            .layout()
            .basicblocks()
            .iter()
            .map(|l| l.bb())
            .find(|&bb| data.bb_data(bb).name().starts_with("vec_reduce"))
            .expect("reduce block must exist");
        let reduce_term = data.layout().basicblock(reduce).terminator();
        let InstKind::Jump(rj) = data.inst_data(reduce_term).kind() else {
            panic!("reduce block must end in a jump");
        };
        assert!(
            data.bb_data(rj.target()).name().starts_with("vec_tail"),
            "reduce must feed the scalar tail"
        );
        assert_eq!(
            rj.args().len(),
            2,
            "tail entry is [iv0, sum]"
        );
        let tail = data
            .layout()
            .basicblocks()
            .iter()
            .map(|l| l.bb())
            .find(|&bb| data.bb_data(bb).name().starts_with("vec_tail"))
            .expect("tail loop must exist");
        assert_eq!(
            data.bb_data(tail).params().len(),
            2,
            "tail header carries [iv_t, acc_t]"
        );
        // The vectorized latch still tests `gt(counter, 0)`; its false
        // edge carries the vector accumulator to the reduce block.
        let latch_term = data.layout().basicblock(latch).terminator();
        let InstKind::Branch(branch) = data.inst_data(latch_term).kind() else {
            panic!("latch must end in the rewritten branch");
        };
        let InstKind::Binary(gt) = data.inst_data(branch.cond()).kind() else {
            panic!("runtime counter test must be a comparison");
        };
        assert_eq!(gt.op(), BinaryOp::Gt, "counter test is gt(counter, 0)");
        assert!(
            data.bb_data(branch.f_target()).name().starts_with("vec_reduce"),
            "counter-zero edge carries the vector accumulator to the reduce block"
        );
        // The entry is a guarded branch: `gt(cnt0, 0)` — the vector loop
        // is a do-while, so a zero/negative `cnt0` (trip < 4) must skip
        // it entirely and enter the scalar tail directly. The true edge
        // feeds the splatted zero accumulator and `cnt0`.
        let entry_bb = data.layout().entry_bb().expect("entry block").bb();
        let entry_edge = data.layout().basicblock(entry_bb).terminator();
        let InstKind::Branch(entry_branch) = data.inst_data(entry_edge).kind() else {
            panic!("entry must be a guard branch");
        };
        let InstKind::Binary(guard_gt) = data.inst_data(entry_branch.cond()).kind() else {
            panic!("entry guard must be a comparison");
        };
        assert_eq!(guard_gt.op(), BinaryOp::Gt);
        assert!(
            matches!(
                data.inst_data(entry_branch.t_args()[0]).kind(),
                InstKind::VectorSplat(_)
            ),
            "the accumulator enters splatted"
        );
        let InstKind::Binary(and) = data.inst_data(entry_branch.t_args()[2]).kind() else {
            panic!("counter entry must be `and(t0, -4)`");
        };
        assert_eq!(and.op(), BinaryOp::And);
        assert!(
            data.bb_data(entry_branch.f_target())
                .name()
                .starts_with("vec_tail"),
            "the zero-trip edge enters the scalar tail"
        );
        // The exit receives the final accumulator through the tail.
        let tail_term = data.layout().basicblock(tail).terminator();
        let InstKind::Branch(tb) = data.inst_data(tail_term).kind() else {
            panic!("tail must end in a branch");
        };
        assert_eq!(tb.f_target(), exit, "tail exits to the loop exit");
        assert_eq!(tb.f_args().len(), 1, "exit receives [acc]");
        assert!(!run(&mut program, function), "idempotent");
    }

    /// Build a test-at-top single-reduction loop with a *runtime* bound
    /// (the shape that unlocks the 4 perf cases): header
    /// `[passthrough_i, iv, acc]`, `bound = load n` (scalar global),
    /// payload `b[i] = a[i] + passthrough_i; acc += a[i]`. The exit has
    /// no parameters and returns the header accumulator directly.
    fn build_runtime_reduction_test_at_top(
        program: &mut Program,
    ) -> (Function, BasicBlock, BasicBlock, BasicBlock, Inst) {
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr.clone());
            program.new_value().global_alloc(init)
        };
        let b = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let n = {
            let init = program.new_value().zero_init(i32.clone());
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_i32(), "tat_red_rt".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut *program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data.new_basic_block().basic_block(
            "header".into(),
            vec![i32.clone(), i32.clone(), i32.clone()],
        );
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let seven = data.new_local_inst().integer(7);
        let zero = data.new_local_inst().integer(0);
        let bound = {
            let mut lb = LocalBuilder {
                arena: &mut data as &mut dyn Arena,
            };
            lb.load(n)
        };
        let entry_jump = data.new_local_inst().jump(header, vec![seven, zero, zero]);
        data.layout_mut().insert_inst(entry, seven);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, bound);
        data.layout_mut().insert_inst(entry, entry_jump);
        let passthrough_i = data.bb_data(header).params()[0];
        let iv = data.bb_data(header).params()[1];
        let acc = data.bb_data(header).params()[2];
        let cond = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        let header_br = data.new_local_inst().branch(cond, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, cond);
        data.layout_mut().insert_inst(header, header_br);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let sum = lb.binary(BinaryOp::Add, load_a, passthrough_i);
        let gep_b = lb.get_elem_ptr(b, vec![zero, iv]);
        let store = lb.store(sum, gep_b);
        drop(lb);
        let acc_next = data.new_local_inst().binary(BinaryOp::Add, acc, load_a);
        let iv_next = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let latch_jump = data
            .new_local_inst()
            .jump(header, vec![passthrough_i, iv_next, acc_next]);
        for inst in [
            one, gep_a, load_a, sum, gep_b, store, acc_next, iv_next, latch_jump,
        ] {
            data.layout_mut().insert_inst(latch, inst);
        }
        let ret = data.new_local_inst().ret(Some(acc));
        data.layout_mut().insert_inst(exit, ret);
        (function, header, latch, exit, bound)
    }

    #[test]
    fn vectorizes_runtime_bound_reduction() {
        // Runtime-bound [passthrough_i, iv, acc]: the vector loop reduces
        // into a vector accumulator, the reduce block computes
        // `sum = seed + Σc`, the scalar tail continues the accumulation
        // for `trip & 3` iterations, and the exit receives the final
        // scalar through the freshly added parameter.
        let mut program = Program::new();
        let (function, header, _latch, exit, _bound) =
            build_runtime_reduction_test_at_top(&mut program);
        assert!(
            run(&mut program, function),
            "runtime-bound test-at-top reduction must vectorize"
        );
        let data = program.func_data(function);
        assert_eq!(vector_load_count(&program, function), 1);
        // The tail carries [iv_t, acc_t, passthrough].
        let tail = data
            .layout()
            .basicblocks()
            .iter()
            .map(|l| l.bb())
            .find(|&bb| data.bb_data(bb).name().starts_with("vec_tail"))
            .expect("tail loop must exist");
        let tail_params = data.bb_data(tail).params();
        assert_eq!(tail_params.len(), 3, "tail header carries [iv_t, acc_t, p]");
        // The reduce block jumps to the tail with [iv0, sum, p]; `sum` is
        // `add(seed, VectorReduce)`.
        let reduce = data
            .layout()
            .basicblocks()
            .iter()
            .map(|l| l.bb())
            .find(|&bb| data.bb_data(bb).name().starts_with("vec_reduce"))
            .expect("reduce block must exist");
        let reduce_jump = data.layout().basicblock(reduce).terminator();
        let InstKind::Jump(jump) = data.inst_data(reduce_jump).kind() else {
            panic!("reduce block must end in a jump");
        };
        assert_eq!(jump.target(), tail, "reduce block feeds the tail");
        let fwd = jump.args().to_vec();
        assert_eq!(fwd.len(), 3, "tail entry takes [iv0, sum, p]");
        let InstKind::Binary(iv0) = data.inst_data(fwd[0]).kind() else {
            panic!("tail entry index must be an add");
        };
        assert_eq!(iv0.op(), BinaryOp::Add);
        let InstKind::Binary(sum) = data.inst_data(fwd[1]).kind() else {
            panic!("the tail's acc seed must be the scalar sum");
        };
        assert_eq!(sum.op(), BinaryOp::Add, "sum = seed + VectorReduce");
        // The tail latch accumulates scalar: an `add(acc_t, delta)` where
        // the lhs is the tail's acc parameter.
        let tail_latch = data
            .layout()
            .basicblocks()
            .iter()
            .map(|l| l.bb())
            .find(|&bb| data.bb_data(bb).name().starts_with("vec_tail_latch"))
            .expect("tail latch must exist");
        let scalar_accumulates = data
            .layout()
            .basicblock(tail_latch)
            .insts()
            .iter()
            .copied()
            .filter(|&inst| {
                matches!(
                    data.inst_data(inst).kind(),
                    InstKind::Binary(b)
                        if b.op() == BinaryOp::Add && b.lhs() == tail_params[1]
                )
            })
            .count();
        assert_eq!(scalar_accumulates, 1, "the tail keeps the scalar acc update");
        // The header's counter-zero edge enters the reduce block with the
        // vector accumulator; the reduce block's own edge leaves the tail.
        let term = data.layout().basicblock(header).terminator();
        let InstKind::Branch(branch) = data.inst_data(term).kind() else {
            panic!("header must end in a branch");
        };
        assert_eq!(
            branch.f_target(),
            reduce,
            "counter-zero edge enters the reduce block"
        );
        // The exit received one added scalar parameter (its direct acc
        // read was rewritten) and the tail's final acc feeds it.
        let exit_params = data.bb_data(exit).params();
        assert_eq!(exit_params.len(), 1, "one scalar param added to the exit");
        let tail_br = data.layout().basicblock(tail).terminator();
        let InstKind::Branch(tb) = data.inst_data(tail_br).kind() else {
            panic!("tail header must end in a branch");
        };
        assert_eq!(tb.f_target(), exit);
        assert_eq!(tb.f_args().to_vec(), vec![tail_params[1]]);
        // Idempotence: neither the tail nor the vector loop re-fires.
        assert!(!run(&mut program, function), "vectorized loop must not re-fire");
    }

    /// Build a rotated elementwise loop with a payload select:
    /// `b[i] = cond ? a[i] : 0`. `lane_cond` makes the condition the
    /// lane-wise comparison `a[i] > 0` (rejected: v3); otherwise the
    /// condition is a loop-invariant flag defined in the entry block.
    fn build_select_loop(program: &mut Program, lane_cond: bool) -> Function {
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr.clone());
            program.new_value().global_alloc(init)
        };
        let b = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_unit(), "sel_loop".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut *program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        let trip_inst = data.new_local_inst().integer(16);
        let flag = data.new_local_inst().integer(1);
        let entry_jump = data.new_local_inst().jump(header, vec![zero, trip_inst]);
        for inst in [zero, trip_inst, flag, entry_jump] {
            data.layout_mut().insert_inst(entry, inst);
        }
        let iv = data.bb_data(header).params()[0];
        let counter = data.bb_data(header).params()[1];
        let header_jump = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, header_jump);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let cond = if lane_cond {
            lb.binary(BinaryOp::Gt, load_a, zero)
        } else {
            flag
        };
        let sel = lb.select(cond, load_a, zero);
        let gep_b = lb.get_elem_ptr(b, vec![zero, iv]);
        let store = lb.store(sel, gep_b);
        drop(lb);
        let iv_next = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let t_next = data.new_local_inst().binary(BinaryOp::Sub, counter, one);
        let back = data.new_local_inst().branch(t_next, header, vec![iv_next, t_next], exit, vec![]);
        let mut latch_insts = vec![
            one, gep_a, load_a, sel, gep_b, store, iv_next, t_next, back,
        ];
        if lane_cond {
            // The lane-wise comparison lives in the latch; the invariant
            // flag is already laid out in the entry block.
            latch_insts.insert(3, cond);
        }
        for inst in latch_insts {
            data.layout_mut().insert_inst(latch, inst);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);
        function
    }

    #[test]
    fn vectorizes_scalar_cond_select() {
        // `b[i] = flag ? a[i] : 0` with a loop-invariant condition: the
        // select is rewritten as a mask selection; no scalar Select remains.
        let mut program = Program::new();
        let function = build_select_loop(&mut program, false);
        assert!(
            run(&mut program, function),
            "invariant-condition select must vectorize"
        );
        let data = program.func_data(function);
        let select_left = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .filter(|&inst| matches!(data.inst_data(inst).kind(), InstKind::Select(_)))
            .count();
        assert_eq!(select_left, 0, "no scalar Select may survive vectorization");
        assert_eq!(
            vector_load_count(&program, function),
            2,
            "2x unroll: a[i] becomes two vector loads"
        );
    }

    /// Build a rotated loop with a single-arm if body (B1):
    /// `if ((a[i] & 1) == 0) { b[i] += a[i]; }` — body_br ends in
    /// `br cond, latch, arm`, the arm holds load b / load a / add /
    /// store b and jumps back to the latch.
    fn build_arm_loop(program: &mut Program) -> Function {
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr.clone());
            program.new_value().global_alloc(init)
        };
        let b = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_unit(), "arm_loop".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut *program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone()]);
        let body_br = data.new_basic_block().basic_block("body_br".into(), vec![]);
        let arm = data.new_basic_block().basic_block("arm".into(), vec![]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [header, body_br, arm, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        let trip_inst = data.new_local_inst().integer(16);
        let entry_jump = data.new_local_inst().jump(header, vec![zero, trip_inst]);
        for inst in [zero, trip_inst, entry_jump] {
            data.layout_mut().insert_inst(entry, inst);
        }
        let iv = data.bb_data(header).params()[0];
        let counter = data.bb_data(header).params()[1];
        let header_jump = data.new_local_inst().jump(body_br, vec![]);
        data.layout_mut().insert_inst(header, header_jump);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        // body_br: cond = (a[i] & 1); br cond, latch, arm
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let cond = lb.binary(BinaryOp::And, load_a, one);
        let br = lb.branch(cond, latch, vec![], arm, vec![]);
        drop(lb);
        for inst in [one, gep_a, load_a, cond, br] {
            data.layout_mut().insert_inst(body_br, inst);
        }
        // arm: b[i] += a[i]
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_b = lb.get_elem_ptr(b, vec![zero, iv]);
        let load_b = lb.load(gep_b);
        let sum = lb.binary(BinaryOp::Add, load_b, load_a);
        let store = lb.store(sum, gep_b);
        let arm_jump = lb.jump(latch, vec![]);
        drop(lb);
        for inst in [gep_b, load_b, sum, store, arm_jump] {
            data.layout_mut().insert_inst(arm, inst);
        }
        // latch: iv', t', back branch
        let iv_next = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let t_next = data.new_local_inst().binary(BinaryOp::Sub, counter, one);
        let back = data.new_local_inst().branch(t_next, header, vec![iv_next, t_next], exit, vec![]);
        for inst in [iv_next, t_next, back] {
            data.layout_mut().insert_inst(latch, inst);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);
        function
    }

    #[test]
    fn vectorizes_single_arm_if_body() {
        // B1: the arm-selecting branch disappears, the arm's store becomes
        // a masked store; loads vectorize.
        let mut program = Program::new();
        let function = build_arm_loop(&mut program);
        assert!(
            run(&mut program, function),
            "single-arm if body must vectorize via masked store"
        );
        let data = program.func_data(function);
        let select_left = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .filter(|&inst| matches!(data.inst_data(inst).kind(), InstKind::Select(_)))
            .count();
        assert_eq!(select_left, 0, "no scalar Select may survive");
        assert_eq!(
            vector_load_count(&program, function),
            2,
            "a[i] and b[i] become vector loads"
        );
        // The arm-selecting branch must be gone (now a plain jump).
        let body_br_insts = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .filter(|&inst| {
                matches!(data.inst_data(inst).kind(), InstKind::Branch(_))
            })
            .count();
        assert_eq!(body_br_insts, 1, "only the latch back-branch remains");
    }

    #[test]
    fn vectorizes_lane_cond_select() {
        // `b[i] = (a[i] > 0) ? a[i] : 0` — the condition is a lane-wise
        // payload value; the mask rewrite ((t & ~m) | (f & m)) needs only
        // whitelisted vector ops, so it vectorizes without bsl lowering.
        let mut program = Program::new();
        let function = build_select_loop(&mut program, true);
        assert!(
            run(&mut program, function),
            "lane-wise select condition must vectorize via mask rewrite"
        );
    }

    /// `sum += (a[i] & 1) ? b[i] : 0` — the matmul masked-kernel shape: a
    /// single-arm if whose arm is a *register reduction* (`acc' = acc + d`)
    /// rather than a memory store. The arm's accumulator update must be
    /// masked (`acc' = (acc & ~m) | (acc + d & m)`, `m = -(cond == 0)`) so
    /// the branch disappears and the accumulator vectorizes.
    fn build_masked_reduction_loop(program: &mut Program) -> Function {
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr.clone());
            program.new_value().global_alloc(init)
        };
        let b = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_i32(), "masked_reduce".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut *program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone(), i32.clone()]);
        let body_br = data.new_basic_block().basic_block("body_br".into(), vec![]);
        let arm = data.new_basic_block().basic_block("arm".into(), vec![]);
        // merge = the latch: it carries the (masked) accumulator phi, then
        // performs the iv/t updates and the back branch. Its parameter is
        // the *post-mask* accumulator value.
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![i32.clone()]);
        let exit = data
            .new_basic_block()
            .basic_block("exit".into(), vec![i32.clone()]);
        for bb in [header, body_br, arm, merge, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        let trip_inst = data.new_local_inst().integer(16);
        let entry_jump = data.new_local_inst().jump(header, vec![zero, zero, trip_inst]);
        for inst in [zero, trip_inst, entry_jump] {
            data.layout_mut().insert_inst(entry, inst);
        }
        let acc = data.bb_data(header).params()[0];
        let iv = data.bb_data(header).params()[1];
        let counter = data.bb_data(header).params()[2];
        let header_jump = data.new_local_inst().jump(body_br, vec![]);
        data.layout_mut().insert_inst(header, header_jump);
        let one = data.new_local_inst().integer(1);
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        // body_br: cond = (a[i] & 1); br cond, merge(acc), arm
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let cond = lb.binary(BinaryOp::And, load_a, one);
        let br = lb.branch(cond, merge, vec![acc], arm, vec![]);
        drop(lb);
        for inst in [one, gep_a, load_a, cond, br] {
            data.layout_mut().insert_inst(body_br, inst);
        }
        // arm: delta = acc + b[i]; jump merge(delta)
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let gep_b = lb.get_elem_ptr(b, vec![zero, iv]);
        let load_b = lb.load(gep_b);
        let delta = lb.binary(BinaryOp::Add, acc, load_b);
        let arm_jump = lb.jump(merge, vec![delta]);
        drop(lb);
        for inst in [gep_b, load_b, delta, arm_jump] {
            data.layout_mut().insert_inst(arm, inst);
        }
        // merge: phi acc (parameter), iv', t', back branch to header.
        let merge_acc = data.bb_data(merge).params()[0];
        let iv_next = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let t_next = data.new_local_inst().binary(BinaryOp::Sub, counter, one);
        let back = data.new_local_inst().branch(
            t_next,
            header,
            vec![merge_acc, iv_next, t_next],
            exit,
            vec![merge_acc],
        );
        for inst in [iv_next, t_next, back] {
            data.layout_mut().insert_inst(merge, inst);
        }
        let exit_acc = data.bb_data(exit).params()[0];
        let exit_ret = data.new_local_inst().ret(Some(exit_acc));
        data.layout_mut().insert_inst(exit, exit_ret);
        function
    }

    #[test]
    fn vectorizes_masked_reduction() {
        // B1 + register reduction: `sum += (a[i] & 1) ? b[i] : 0` on a
        // rotated `[acc, iv, t]` loop. The accumulator update lives in the
        // arm (single-exit, masked); it must vectorize with the accumulator
        // re-typed to <4 x i32> and a horizontal reduce at the exit.
        let mut program = Program::new();
        let function = build_masked_reduction_loop(&mut program);
        assert!(
            run(&mut program, function),
            "single-arm if with register reduction must vectorize"
        );
        let data = program.func_data(function);
        // The accumulator header parameter was re-typed to a vector.
        let acc_is_vector = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|l| data.bb_data(l.bb()).params().iter().copied())
            .any(|p| data.inst_data(p).ty().is_vector());
        assert!(acc_is_vector, "accumulator must be re-typed to <4 x i32>");
        let has_reduce = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .any(|inst| matches!(data.inst_data(inst).kind(), InstKind::VectorReduce(_)));
        assert!(has_reduce, "the vector loop must exit through VectorReduce");
        // The arm's mask expansion (`m`/`nm`/`tn`/`fo`/`sel`) must be
        // inserted *after* the lane-wise accumulator update it reads —
        // otherwise `tn = delta & m` references the update before it is
        // defined (use-before-def), which the backend SSA verifier rejects.
        for l in data.layout().basicblocks() {
            if data.bb_data(l.bb()).name() != "arm" {
                continue;
            }
            let insts: Vec<Inst> = l.insts().iter().copied().collect();
            for (idx, &inst) in insts.iter().enumerate() {
                let defined_before: FxHashSet<Inst> =
                    insts.iter().take(idx).copied().collect();
                for used in data.inst_data(inst).inst_usage() {
                    // Only enforce intra-block ordering: a use defined in
                    // another block is governed by dominance, not layout.
                    if !insts.contains(&used) {
                        continue;
                    }
                    assert!(
                        defined_before.contains(&used),
                        "arm inst {inst:?} uses {used:?} before its definition"
                    );
                }
            }
        }
    }

    /// Milestone 2: a synthetic 5-point convolution kernel. A test-at-top
    /// loop whose body is a chain of 5 masked multiply-accumulate units
    /// (`br mask, then, end(acc)`; `then: acc += In[row][cc] * K[k]`) —
    /// the conv2d 5×5 shape with a single row. `cc = iv + k - 2` with the
    /// mask `(cc >= 0) & (cc < bound)`. Returns (function, header).
    fn build_multi_arm_kernel(program: &mut Program) -> (Function, BasicBlock) {
        let i32 = Type::get_i32();
        let in_arr = Type::get_array(i32.clone(), 64);
        let k_arr = Type::get_array(i32.clone(), 5);
        let inp = {
            let init = program.new_value().zero_init(in_arr.clone());
            program.new_value().global_alloc(init)
        };
        let k = {
            let init = program.new_value().zero_init(k_arr);
            program.new_value().global_alloc(init)
        };
        // The accumulator output lives in its own array so the In reads and
        // the Out write do not carry a loop-carried conflict (the kernel
        // writes `Out[r][c]`, never `In`).
        let out = {
            let init = program.new_value().zero_init(in_arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_unit(), "multi_arm_kernel".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut *program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![i32.clone(), i32.clone()]);
        let n_units = 5;
        let mut body_brs = Vec::new();
        let mut thens = Vec::new();
        let mut ends = Vec::new();
        for i in 0..n_units {
            let body = data
                .new_basic_block()
                .basic_block(format!("body_br{i}").into(), vec![]);
            let then = data
                .new_basic_block()
                .basic_block(format!("then{i}").into(), vec![]);
            let end = data
                .new_basic_block()
                .basic_block(format!("end{i}").into(), vec![i32.clone()]);
            body_brs.push(body);
            thens.push(then);
            ends.push(end);
        }
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in std::iter::once(header)
            .chain(body_brs.iter().copied())
            .chain(thens.iter().copied())
            .chain(ends.iter().copied())
            .chain([latch, exit])
        {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        let bound = data.new_local_inst().integer(64);
        let two = data.new_local_inst().integer(2);
        // The shared row offset: a loop-outside constant (in the entry
        // block) so the M42 access-function classifier treats it as
        // invariant, matching the real kernel's outer-loop row offset.
        let row_off = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero, zero]);
        for inst in [zero, bound, two, row_off, entry_jump] {
            data.layout_mut().insert_inst(entry, inst);
        }
        let iv = data.bb_data(header).params()[0];
        let header_cond = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        let header_br = data.new_local_inst().branch(header_cond, body_brs[0], vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, header_cond);
        data.layout_mut().insert_inst(header, header_br);
        let mut prev_end_param: Option<Inst> = None;
        for i in 0..n_units {
            let body = body_brs[i];
            let then = thens[i];
            let end = ends[i];
            let i_const = data.new_local_inst().integer(i as i32);
            let k_idx = data.new_local_inst().integer(i as i32);
            let mut lb = LocalBuilder {
                arena: &mut data as &mut dyn Arena,
            };
            // mask = (cc >= 0) & (cc < bound), cc = sub(add(iv, i), 2).
            let iv_plus = lb.binary(BinaryOp::Add, iv, i_const);
            let cc = lb.binary(BinaryOp::Sub, iv_plus, two);
            let ge0 = lb.binary(BinaryOp::Ge, cc, zero);
            let lt_b = lb.binary(BinaryOp::Lt, cc, bound);
            let mask = lb.binary(BinaryOp::And, ge0, lt_b);
            let acc_in = prev_end_param.unwrap_or(zero);
            let br = lb.branch(mask, then, vec![], end, vec![acc_in]);
            // then: off = add(row_off, cc); ld_in = load In[off]; ld_k = load K[i];
            // mul = mul(ld_in, ld_k); acc' = add(acc_in, mul); jump end(acc').
            let off = lb.binary(BinaryOp::Add, row_off, cc);
            let gep_in = lb.get_elem_ptr(inp, vec![zero, off]);
            let ld_in = lb.load(gep_in);
            let gep_k = lb.get_elem_ptr(k, vec![zero, k_idx]);
            let ld_k = lb.load(gep_k);
            let mul = lb.binary(BinaryOp::Mul, ld_in, ld_k);
            let acc_new = lb.binary(BinaryOp::Add, acc_in, mul);
            let jmp = lb.jump(end, vec![acc_new]);
            drop(lb);
            for inst in [i_const, iv_plus, cc, ge0, lt_b, mask, br] {
                data.layout_mut().insert_inst(body, inst);
            }
            for inst in [k_idx, off, gep_in, ld_in, gep_k, ld_k, mul, acc_new, jmp] {
                data.layout_mut().insert_inst(then, inst);
            }
            // end: jump next (the next body_br, or the latch for the last).
            let end_jump = if i + 1 < n_units {
                data.new_local_inst().jump(body_brs[i + 1], vec![])
            } else {
                data.new_local_inst().jump(latch, vec![])
            };
            data.layout_mut().insert_inst(end, end_jump);
            prev_end_param = Some(data.bb_data(end).params()[0]);
        }
        // latch: store acc to a synthetic Out slot, then back-branch.
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        let out_gep = lb.get_elem_ptr(out, vec![zero, iv]);
        let store = lb.store(prev_end_param.unwrap(), out_gep);
        let one_const = lb.integer(1);
        let iv_next = lb.binary(BinaryOp::Add, iv, one_const);
        let back = lb.jump(header, vec![iv_next, prev_end_param.unwrap()]);
        drop(lb);
        for inst in [out_gep, store, one_const, iv_next, back] {
            data.layout_mut().insert_inst(latch, inst);
        }
        let exit_ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, exit_ret);
        (function, header)
    }

    #[test]
    fn recognizes_multi_arm_masked_kernel() {
        // Milestone 2 phase 1: the 5-unit masked multiply-accumulate chain
        // (conv2d kernel shape) must be recognized, with each unit's cc
        // offset (`iv + k - 2`), bound, and accumulator chain collected.
        let mut program = Program::new();
        let (function, header) = build_multi_arm_kernel(&mut program);
        let data = program.func_data(function);
        let (_, _, loops) = LoopAnalysis::new(data);
        let looop = loops
            .loops()
            .iter()
            .find(|l| l.header() == header)
            .expect("the kernel loop is analyzed");
        let latch = looop.latches()[0];
        let arena = ArenaContext {
            program: &program,
            curr_func: Some(function),
        };
        let plan = build_multi_arm_plan(&arena, data, looop, header, latch);
        let plan = plan.expect("the multi-arm kernel must be recognized");
        assert_eq!(plan.units.len(), 5, "all 5 units are recognized in order");
        for (i, unit) in plan.units.iter().enumerate() {
            // cc = iv + (i - 2).
            assert_eq!(unit.kc_off, i as i64 - 2, "unit {i} cc offset");
            // The mask's bound is the loop-invariant 64.
            assert!(matches!(
                arena.inst_data(unit.bound).kind(),
                InstKind::Integer(b) if b.value() == 64
            ));
            // Unit 0 seeds the accumulator from a constant; later units take
            // the previous unit's end parameter.
            if i == 0 {
                assert!(matches!(
                    arena.inst_data(unit.acc_in).kind(),
                    InstKind::Integer(_)
                ));
            } else {
                assert_eq!(
                    unit.acc_in,
                    data.bb_data(plan.units[i - 1].end).params()[0],
                    "unit {i} accumulator chains from the previous end"
                );
            }
            let InstKind::Binary(mul_b) = arena.inst_data(unit.mul).kind() else {
                panic!("unit {i} mul is a binary Mul");
            };
            assert_eq!(mul_b.op(), BinaryOp::Mul, "unit {i} mul op");
            assert_eq!(mul_b.rhs(), unit.load_k, "unit {i} K load feeds the mul");
            assert_eq!(mul_b.lhs(), unit.load_in, "unit {i} In load feeds the mul");
        }
    }

    #[test]
    fn vectorizes_multi_arm_masked_kernel() {
        // Milestone 2 phase 3: the synthetic 5-unit masked multiply-accumulate
        // kernel (build_multi_arm_kernel, const trip 64) must actually
        // vectorize: the accumulator chain re-typed to <4 x i32>, the In
        // loads contiguous vector loads, the boundary masks lane-wise, and a
        // horizontal reduce at the exit.
        let mut program = Program::new();
        let (function, _) = build_multi_arm_kernel(&mut program);
        let mut lv = LoopVectorize::new();
        assert!(lv.run(&mut program), "the multi-arm kernel must vectorize");
        let data = program.func_data(function);
        let has_vector_load = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .any(|inst| {
                matches!(data.inst_data(inst).kind(), InstKind::Load(_))
                    && data.inst_data(inst).ty().is_vector()
            });
        assert!(has_vector_load, "a unit's In load must become a vector load");
        let has_vector_add = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .any(|inst| {
                matches!(data.inst_data(inst).kind(), InstKind::Binary(b) if b.op() == BinaryOp::Add)
                    && data.inst_data(inst).ty().is_vector()
            });
        assert!(has_vector_add, "the accumulator add must be lane-wise");
        // The boundary masks are vector comparisons conjoined into the add.
        let has_vector_mask = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .any(|inst| {
                matches!(data.inst_data(inst).kind(), InstKind::Binary(b) if b.op() == BinaryOp::And)
                    && data.inst_data(inst).ty().is_vector()
            });
        assert!(has_vector_mask, "a lane-wise boundary mask must be built");
        let has_reduce = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .any(|inst| matches!(data.inst_data(inst).kind(), InstKind::VectorReduce(_)));
        assert!(has_reduce, "the vector accumulator must be horizontally reduced at the exit");
    }

    #[test]
    fn rejects_non_innermost_loop() {
        // A two-level nest: the outer loop contains the elementwise inner
        // loop. Only the inner one may be vectorized; the outer loop's latch
        // must keep its unit-step counter and index updates.
        let mut program = Program::new();
        let i32 = Type::get_i32();
        let arr = Type::get_array(i32.clone(), 64);
        let a = {
            let init = program.new_value().zero_init(arr.clone());
            program.new_value().global_alloc(init)
        };
        let b = {
            let init = program.new_value().zero_init(arr);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_unit(), "nested".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        // Outer loop: [iv_o, t_o] -> b1 -> inner loop -> b2 (latch) -> h_o.
        let h_o = data
            .new_basic_block()
            .basic_block("h_o".into(), vec![i32.clone(), i32.clone()]);
        let b1 = data.new_basic_block().basic_block("b1".into(), vec![]);
        let b2 = data.new_basic_block().basic_block("b2".into(), vec![]);
        let exit_o = data.new_basic_block().basic_block("exit_o".into(), vec![]);
        // Inner loop: [iv, t] -> latch; latch does the elementwise work.
        let h_i = data
            .new_basic_block()
            .basic_block("h_i".into(), vec![i32.clone(), i32.clone()]);
        let latch_i = data.new_basic_block().basic_block("latch_i".into(), vec![]);
        let exit_i = data.new_basic_block().basic_block("exit_i".into(), vec![]);
        for bb in [h_o, b1, b2, exit_o, h_i, latch_i, exit_i] {
            data.layout_mut().push_bb_back(bb);
        }
        let zero = data.new_local_inst().integer(0);
        let trip_o = data.new_local_inst().integer(32);
        let entry_jump = data.new_local_inst().jump(h_o, vec![zero, trip_o]);
        data.layout_mut().insert_inst(entry, zero);
        data.layout_mut().insert_inst(entry, trip_o);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv_o = data.bb_data(h_o).params()[0];
        let t_o = data.bb_data(h_o).params()[1];
        let h_o_jump = data.new_local_inst().jump(b1, vec![]);
        data.layout_mut().insert_inst(h_o, h_o_jump);
        let one = data.new_local_inst().integer(1);
        let trip_i = data.new_local_inst().integer(16);
        let iv = data.bb_data(h_i).params()[0];
        let t = data.bb_data(h_i).params()[1];
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        // b1: enter the inner loop.
        let inner_entry = lb.jump(h_i, vec![zero, trip_i]);
        // Inner header jump.
        let h_i_jump = lb.jump(latch_i, vec![]);
        let gep_a = lb.get_elem_ptr(a, vec![zero, iv]);
        let load_a = lb.load(gep_a);
        let gep_b = lb.get_elem_ptr(b, vec![zero, iv]);
        let store = lb.store(load_a, gep_b);
        let iv_next = lb.binary(BinaryOp::Add, iv, one);
        let t_next = lb.binary(BinaryOp::Sub, t, one);
        let inner_back = lb.branch(t_next, h_i, vec![iv_next, t_next], exit_i, vec![]);
        // b2 (outer latch): iv_o+1, t_o-1, test back.
        let iv_o_next = lb.binary(BinaryOp::Add, iv_o, one);
        let t_o_next = lb.binary(BinaryOp::Sub, t_o, one);
        let outer_back = lb.branch(t_o_next, h_o, vec![iv_o_next, t_o_next], exit_o, vec![]);
        drop(lb);
        data.layout_mut().insert_inst(b1, inner_entry);
        data.layout_mut().insert_inst(h_i, h_i_jump);
        for inst in [gep_a, load_a, gep_b, store, iv_next, t_next, inner_back] {
            data.layout_mut().insert_inst(latch_i, inst);
        }
        for inst in [iv_o_next, t_o_next, outer_back] {
            data.layout_mut().insert_inst(b2, inst);
        }
        let exit_o_ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit_o, exit_o_ret);
        let exit_i_jump = data.new_local_inst().jump(b2, vec![]);
        data.layout_mut().insert_inst(exit_i, exit_i_jump);

        assert!(run(&mut program, function), "the innermost loop is vectorized");
        let data = program.func_data(function);
        // The outer latch (b2) still steps by 1: it was not vectorized.
        let outer_steps_unit = data
            .layout()
            .basicblock(b2)
            .insts()
            .iter()
            .any(|&inst| {
                matches!(
                    data.inst_data(inst).kind(),
                    InstKind::Binary(b)
                        if b.op() == BinaryOp::Add
                            && b.lhs() == iv_o
                            && matches!(
                                data.inst_data(b.rhs()).kind(),
                                InstKind::Integer(v) if v.value() == 1
                            )
                )
            });
        assert!(outer_steps_unit, "outer loop must stay scalar (non-innermost)");
        assert_eq!(
            vector_load_count(&program, function),
            2,
            "only the inner loop vectorizes (2x unrolled)"
        );
    }
}
