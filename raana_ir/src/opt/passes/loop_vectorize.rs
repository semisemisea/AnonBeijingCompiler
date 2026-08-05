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

use rustc_hash::FxHashMap;

use crate::ir::{
    arena::Arena,
    builder_trait::*,
    inst_kind::{
        Binary, BlockArgRef, Float, GetElemPtr, InstKind, Integer, Load, Select, Store,
        VectorReduce, VectorReduceOp, VectorSplat,
    },
    types::{Type, TypeKind},
    BasicBlock, BinaryOp, Function, Inst, Program,
};
use crate::ir::function::FunctionData;
use crate::opt::{
    analysis_passes::{
        dependence::{AccessKind, DependenceAnalysis, ReductionOp, Verdict},
        effects::EffectAnalysis,
        loop_analysis::{Loop, LoopAnalysis},
        memory::MemObject,
    },
    pass::{ArenaContext, ArenaContextMut, Pass},
    utils::cfg::CFG,
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
        if self.analysis.is_none() {
            // Direct invocations (unit tests) have no analysis yet.
            self.analysis = Some(EffectAnalysis::new(data.program));
        }
        // Read-only analysis phase.
        let program: &Program = data.program;
        let effects = self.analysis.as_ref().expect("analysis built above");
        let plan = find_vectorizable(program, func, effects);
        let Some(plan) = plan else {
            return false;
        };
        // Mutation phase.
        apply_vectorize(data, plan)
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
    /// Latch instructions to rewrite / clone, in layout order (excludes the
    /// latch machinery: terminator, condition, back-edge args).
    payload: Vec<Inst>,
    /// Rewrite class per payload instruction.
    classes: FxHashMap<Inst, Class>,
    /// `<4 x T>` result type (T = the loop's element type).
    vector_ty: Type,
    /// B1: register-reduction plan, or None for plain elementwise loops.
    reduction: Option<ReductionPlan>,
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
        eprintln!("[M44] func={name} header={:?} reject={reason}", looop.header());
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
    let (cfg, _dom, loops) = LoopAnalysis::new(data);
    let dependence = DependenceAnalysis::new(program, func, effects, VF as u32);
    for looop in loops.loops() {
        if let Some(plan) =
            analyze_loop(&arena, data, &cfg, &loops, &dependence, looop)
        {
            return Some(plan);
        }
    }
    None
}

fn analyze_loop(
    arena: &ArenaContext<'_>,
    data: &FunctionData,
    cfg: &CFG,
    loops: &LoopAnalysis,
    dependence: &DependenceAnalysis,
    looop: &Loop,
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
    if looop.body().len() != 2 {
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
                    });
                    break;
                }
            }
        }
        if arm_plan.is_none() {
            trace(data, looop, "shape_body_not_2_blocks");
            return None;
        }
    }
    let header_insts = data.layout().basicblock(header).insts();
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
        if branch.t_target() != latch {
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
    // Passthrough slots: the back-edge argument is the parameter itself.
    // The trip counter is the last parameter (its back-edge arg is the
    // `t' = sub(t, 1)` condition, never the parameter itself).
    let passthrough: Vec<usize> = (0..n_params)
        .filter(|&i| back_args[i] == params[i])
        .collect();
    let counter_slot = n_params - 1;
    let effective: Vec<usize> = if test_at_top {
        // No counter parameter yet (materialized in the mutation phase), so
        // nothing is excluded beyond the passthroughs.
        (0..n_params)
            .filter(|&i| !passthrough.contains(&i))
            .collect()
    } else {
        (0..n_params)
            .filter(|&i| i != counter_slot && !passthrough.contains(&i))
            .collect()
    };
    // Test-at-top loops carry exactly one real induction parameter (the
    // IV); any additional non-passthrough parameter is not a recognized
    // shape (rotated loops may carry a second one: the B1 accumulator).
    if test_at_top && effective.len() != 1 {
        trace(data, looop, "test_at_top_multi_param");
        return None;
    }
    let acc_info: Option<(Inst, usize, BinaryOp)> = match effective.len() {
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
            if passthrough.contains(&acc_slot) || acc_slot == counter_slot {
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
            // The single effective (non-passthrough) parameter is the IV;
            // `iv' = add(iv, 1)` is its plain-jump argument in the latch.
            // The remaining parameters are loop-invariant passthroughs.
            let iv_slot = effective[0];
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
            let InstKind::Binary(binary) = arena.inst_data(update).kind() else {
                trace(data, looop, "b1_acc_update_not_binary");
                return None;
            };
            if binary.op() != bop || binary.lhs() != acc {
                trace(data, looop, "b1_acc_update_shape");
                return None;
            }
            let delta = binary.rhs();
            // The accumulator must be consumed only by its update chain
            // (and the exit argument). An exit that reads the header
            // parameter directly is not the rotated form and stays scalar.
            for &user in arena.inst_data(acc).used_by() {
                if user != update && data.layout().parent_bb(user) != Some(latch) {
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
    for (pos, &arg) in exit_args.iter().enumerate() {
        let spec = if acc_info.is_some() && pos == 0 {
            let (update, _) = acc_update
                .as_ref()
                .expect("acc_update is set alongside acc_info");
            if arg != *update {
                trace(data, looop, "exit_arg_not_acc");
                return None;
            }
            ExitArgSpec::Acc
        } else if arg == iv_next {
            ExitArgSpec::IvFinal
        } else if let Some(slot) = passthrough.iter().copied().find(|&slot| {
            arg == params[slot] || arg == entry_args[slot]
        }) {
            ExitArgSpec::Passthrough(slot)
        } else {
            trace(data, looop, "exit_has_params");
            return None;
        };
        exit_specs.push(spec);
    }

    // 7. Exact trip: the initial index must be a compile-time constant, and
    //    the trip must fit at least one vector iteration. Rotated loops
    //    carry the trip as a constant counter entry; test-at-top loops
    //    derive it from a constant bound (`trip = bound - i0`, no
    //    versioning). When the trip leaves a remainder the exit must not
    //    read the IV (its final value after `q` vector iterations differs
    //    from the scalar `bound`); with R == 0 the values coincide.
    let Some(entry_i0) = constant_i64(arena, data, entry_args[iv_slot]) else {
        trace(data, looop, "entry_i0_not_const");
        return None;
    };
    let trip: i64 = if test_at_top {
        let Some(bound) = constant_i64(arena, data, bound_inst) else {
            trace(data, looop, "test_at_top_bound_not_const");
            return None;
        };
        bound - entry_i0
    } else {
        let Some(t) = constant_i64(arena, data, entry_args[counter_slot]) else {
            trace(data, looop, "entry_trip_not_const");
            return None;
        };
        t
    };
    if trip < VF {
        trace(data, looop, "trip_below_vf");
        return None;
    }
    if test_at_top && trip % VF != 0 {
        // The vectorized loop runs `q` iterations; the scalar epilogue
        // substitutes constant IVs, so the header parameter's final value
        // is `i0 + 4q`, not `bound`. An exit that reads the IV is only
        // correct when R == 0 (then `i0 + 4q == bound`).
        for &user in arena.inst_data(iv).used_by() {
            let bb = data.layout().parent_bb(user);
            if bb != Some(header) && bb != Some(latch) {
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
        }
        set
    };
    let payload: Vec<Inst> = if let Some(arm_plan) = &arm_plan {
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

    // First pass: loads and stores (their classes feed the binary checks).
    for &inst in &payload {
        let kind = arena.inst_data(inst).kind();
        match kind {
            InstKind::GetElemPtr(gep) => {
                // GEP offsets may reference only the index IV or loop
                // invariants; the base must be loop-invariant (a global,
                // stack alloc, or a dominating value).
                if !is_loop_invariant(arena, gep.base(), header, latch) {
        trace(data, looop, "gep_base_loop_variant");
        return None;
                }
                for &offset in gep.offsets() {
                    if offset != iv && !is_loop_invariant(arena, offset, header, latch) {
        trace(data, looop, "gep_offset_loop_variant");
        return None;
                    }
                }
                classes.insert(inst, Class::Keep);
            }
            InstKind::Load(load) => {
                let access = dep.accesses.iter().find(|a| a.inst == inst)?;
                let Some((class, elem)) =
                    classify_load(arena, load.src(), access, &payload, header, latch)
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
                if !base_is_16b_aligned(arena, access.base) {
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
                // Classified in the second pass (needs the load classes).
                classes.insert(inst, Class::VecBinary);
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
        // (Div vectorizes only for f32 — NEON has no integer vector divide,
        // `sdiv` is scalar-only; constant-divisor i32 loops are rewritten to
        // shift chains by StrengthReduction before the vectorizer's next look.
        // Min/Max vectorize for both i32 and f32: smin/smax/fmin/fmax.)
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
        if binary.op() == BinaryOp::Div && ty.is_i32() {
            trace(data, looop, "binary_int_div_no_neon");
            return None;
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
                    if !is_loop_invariant(arena, operand, header, latch) {
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
                if !is_loop_invariant(arena, store.src(), header, latch) {
                                        trace(data, looop, "mixed_elem_types");
        trace(data, looop, "store_src_loop_variant");
        return None;
                }
            }
        }
    }

    // 9. Every payload value must stay inside the loop (no uses in the exit
    //    region), and at least one real vector operation must be produced.
    //    With a B1 single-arm body the uses may sit in any body block
    //    (body_br / arm / merge), not just the latch.
    for &inst in &payload {
        for &user in arena.inst_data(inst).used_by() {
            let user_bb = data.layout().parent_bb(user);
            if !user_bb.is_some_and(|bb| looop.contains(bb)) {
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
    let reduction = match acc_info {
        None => None,
        Some((acc, acc_slot, bop)) => {
            let (update, delta) = acc_update.expect("acc_update set alongside acc_info");
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
                exit_acc_param: if exit_params.is_empty() {
                    None
                } else {
                    Some(exit_params[0])
                },
            })
        }
    };

    // B1 single-arm bodies: only exact trips are supported for now (the
    // epilogue chain for multi-block bodies is not implemented).
    if arm_plan.is_some() && trip % VF != 0 {
        trace(data, looop, "arm_remainder_not_supported");
        return None;
    }

    Some(VecPlan {
        header,
        latch,
        exit,
        entry_edge,
        iv,
        counter,
        entry_i0,
        trip,
        latch_branch,
        t_next,
        iv_next,
        payload,
        classes,
        vector_ty: Type::get_vector(elem_ty, VF as usize),
        reduction,
        passthrough_args,
        exit_specs,
        test_at_top,
        arm: arm_plan,
    })
}

/// Classify one load against its access function.
fn classify_load(
    arena: &ArenaContext<'_>,
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
    if access.size != VF || !base_is_16b_aligned(arena, access.base) {
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
            | BinaryOp::Min
            | BinaryOp::Max
            | BinaryOp::Eq
            | BinaryOp::Gt
    )
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
        // A passthrough parameter: the back-edge argument is the parameter
        // itself. Rotated loops branch back to the header; test-at-top
        // loops jump back to it.
        let term = arena.curr_func_data().layout().basicblock(latch).terminator();
        return match arena.inst_data(term).kind() {
            InstKind::Branch(branch) => branch.t_args().get(slot) == Some(&inst),
            InstKind::Jump(jump) => jump.args().get(slot) == Some(&inst),
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

/// v1 alignment gate: the base object must be provably 16B-aligned (globals
/// with size >= 16 are `.p2align 4`; AArch64 array stack slots round up to
/// 16). Array parameters have unknown caller alignment and are rejected
/// (that case needs M43 versioning).
fn base_is_16b_aligned(arena: &ArenaContext<'_>, base: MemObject) -> bool {
    match base {
        MemObject::Global(global) => arena.inst_data(global).ty().derefernce().size() >= 16,
        MemObject::Alloc(alloc) => {
            let ty = arena.inst_data(alloc).ty().derefernce();
            matches!(ty.kind(), TypeKind::Array(_, _)) && ty.size() >= 16
        }
        MemObject::Param(_) | MemObject::Unknown => false,
    }
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
    let VecPlan {
        header,
        latch,
        exit,
        entry_edge,
        iv,
        counter,
        entry_i0,
        trip,
        latch_branch,
        t_next,
        iv_next,
        payload,
        classes,
        vector_ty,
        reduction,
        passthrough_args,
        exit_specs,
        test_at_top,
        arm,
    } = plan;
    let q = trip / VF;
    let r = trip % VF;
    let i32 = Type::get_i32();
    let n_passthrough = passthrough_args.len();
    // A4: does any exit parameter carry the IV's final value? Those slots
    // must be fed the computed `i0 + VF*q` (+ the peeled remainder through
    // the epilogue chain) instead of the latch's per-iteration update.
    let has_iv_final = exit_specs.iter().any(|spec| *spec == ExitArgSpec::IvFinal);
    // The trip counter is the last header parameter; passthrough params
    // precede it ([.. passthroughs .., iv (, acc), t]). Test-at-top loops
    // have no counter parameter: one is materialized here as a new header
    // parameter (fed `4Q` from the entry edge, stepped by 4 in the latch,
    // and tested at the header in place of the bound).
    let four = data.new_local_inst().integer(VF as i32);
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
        2 + usize::from(reduction.is_some()) + n_passthrough
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

    // 1b. Reduction exit block: `acc_final = VectorReduce(Add, acc_vec)`,
    //     then jump into the epilogue chain (or straight to the exit when
    //     r == 0). Placed right after the latch. Passthrough values are
    //     forwarded alongside the reduced accumulator.
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
        let (target, args) = match epi_blocks.first().copied() {
            Some(first) => {
                let mut fwd = vec![acc_final];
                fwd.extend(data.bb_data(rb).params()[1..].iter().copied());
                (first, fwd)
            }
            None => {
                // r == 0: the exit edge carries the reduced accumulator,
                // the passthrough values, and — when declared — the
                // computed IV final value `i0 + VF*q` (the loop ran exactly
                // `q` vector iterations, so `i0 + 4q == i0 + trip`).
                let iv_final_const = has_iv_final
                    .then(|| data.new_local_inst().integer((entry_i0 + VF * q) as i32));
                let acc_value = red.exit_acc_param.map(|_| acc_final);
                (
                    exit,
                    build_exit_args(&exit_specs, &passthrough_args, acc_value, iv_final_const),
                )
            }
        };
        let jump = data.new_local_inst().jump(target, args);
        data.layout_mut().insert_inst(rb, jump);
        Some(rb)
    } else {
        None
    };

    if r > 0 {
        for (idx, &block) in epi_blocks.iter().enumerate() {
            let subst_iv = data
                .new_local_inst()
                .integer((entry_i0 + VF * q + idx as i64) as i32);
            let mut map = FxHashMap::<Inst, Inst>::default();
            let mut insts: Vec<Inst> = Vec::with_capacity(payload.len() + 2);
            for &orig in &payload {
                insts.push(clone_payload_inst(data, orig, &mut map, iv, subst_iv));
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
                    // exit's arguments from its parameter classes. The IV
                    // final value is the computed `i0 + VF*q + r`: the
                    // peeled scalar iterations ran with `iv = i0 + 4q + k`,
                    // so the scalar exit value `i0 + trip` == `i0 + 4q + r`.
                    let iv_final_const = has_iv_final
                        .then(|| data.new_local_inst().integer((entry_i0 + VF * q + r) as i32));
                    (
                        exit,
                        build_exit_args(&exit_specs, &passthrough_args, acc_value, iv_final_const),
                    )
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
    let mut splats = FxHashMap::<Inst, Inst>::default();
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
                        // load already inside the arm.
                        let old = payload.iter().copied().find(|&p| {
                            matches!(
                                data.inst_data(p).kind(),
                                InstKind::Load(l) if l.src() == dest
                            )
                        });
                        let Some(old) = old else {
                            unreachable!("arm store without a same-address load");
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
                        let m = alloc_inst(
                            data,
                            Binary::new_data(vzero, eq0, BinaryOp::Sub, vector_ty.clone()),
                        );
                        data.layout_mut().insert_inst_before(inst, m);
                        let nm = alloc_inst(
                            data,
                            Binary::new_data(m, vneg, BinaryOp::Xor, vector_ty.clone()),
                        );
                        data.layout_mut().insert_inst_before(inst, nm);
                        let tn = alloc_inst(
                            data,
                            Binary::new_data(vnew, m, BinaryOp::And, vector_ty.clone()),
                        );
                        data.layout_mut().insert_inst_before(inst, tn);
                        let fo = alloc_inst(
                            data,
                            Binary::new_data(vold, nm, BinaryOp::And, vector_ty.clone()),
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
                let m = alloc_inst(
                    data,
                    Binary::new_data(vzero, eq0, BinaryOp::Sub, vector_ty.clone()),
                );
                data.layout_mut().insert_inst_before(inst, m);
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

    // 2b. B1: re-type the accumulator parameter to `<4 x T>` and rewrite its
    //     update chain into a lane-wise accumulation. The delta resolves
    //     through the same machinery as any payload value (vector load /
    //     vector binary) or is splatted when loop-invariant.
    if let Some(red) = &reduction {
        data.inst_data_mut(red.acc).set_type(vector_ty.clone());
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
    if test_at_top || reduction.is_some() || r > 0 || has_iv_final {
        let (cond, t_target, t_args) = if test_at_top {
            (eff_counter, latch, vec![])
        } else {
            match data.inst_data(latch_branch).kind() {
                InstKind::Branch(b) => (b.cond(), b.t_target(), b.t_args().to_vec()),
                _ => unreachable!(),
            }
        };
        let iv_final_const = has_iv_final
            .then(|| data.new_local_inst().integer((entry_i0 + VF * q) as i32));
        let (f_target, f_args) = if test_at_top {
            // The epilogue chain (r > 0) and the exit both carry the
            // loop-invariant passthrough values: their header parameters
            // stay live and unchanged across the vectorized loop, so the
            // entry-edge values are forwarded as-is.
            if r > 0 {
                (epi_blocks[0], passthrough_args.clone())
            } else {
                (
                    exit,
                    build_exit_args(&exit_specs, &passthrough_args, None, iv_final_const),
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
                None if r > 0 => (epi_blocks[0], passthrough_args.clone()),
                None => {
                    // r == 0: the exit is reached directly; IV-final
                    // parameters are replaced by the computed constant.
                    (
                        exit,
                        build_exit_args(&exit_specs, &passthrough_args, None, iv_final_const),
                    )
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
    let four_q = data.new_local_inst().integer((VF * q) as i32);
    let acc_splat: Option<Inst> = reduction.as_ref().map(|red| {
        let s = alloc_inst(data, VectorSplat::new_data(red.acc_init, vector_ty.clone()));
        data.layout_mut().insert_inst_before(entry_edge, s);
        s
    });
    let entry_rewrite = match data.inst_data(entry_edge).kind() {
        InstKind::Branch(b) => {
            let mut t_args = b.t_args().to_vec();
            if test_at_top {
                // The new counter parameter is appended after the IV.
                t_args.push(four_q);
            } else {
                t_args[entry_arg_count - 1] = four_q;
            }
            if let (Some(red), Some(splat)) = (&reduction, &acc_splat) {
                t_args[red.acc_slot] = *splat;
            }
            EntryRewrite::Branch {
                cond: b.cond(),
                f_target: b.f_target(),
                f_args: b.f_args().to_vec(),
                t_args,
            }
        }
        InstKind::Jump(j) => {
            let mut args = j.args().to_vec();
            if test_at_top {
                args.push(four_q);
            } else {
                args[entry_arg_count - 1] = four_q;
            }
            if let (Some(red), Some(splat)) = (&reduction, &acc_splat) {
                args[red.acc_slot] = *splat;
            }
            EntryRewrite::Jump {
                target: j.target(),
                args,
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
    if let Some(&splat) = splats.get(&operand) {
        return splat;
    }
    let splat = alloc_inst(data, VectorSplat::new_data(operand, vector_ty.clone()));
    splats.insert(operand, splat);
    data.layout_mut().insert_inst_before(anchor, splat);
    splat
}

/// Clone one payload instruction into an epilogue block, substituting the
/// index IV with a constant. In-loop operands resolve through `map` (the
/// payload is cloned in def-before-use order); constants are re-created per
/// block; globals and dominating values are reused.
fn clone_payload_inst(
    data: &mut ArenaContextMut<'_>,
    orig: Inst,
    map: &mut FxHashMap<Inst, Inst>,
    iv: Inst,
    subst_iv: Inst,
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
        InstKind::Store(store) => Store::new_data(
            map_operand(data, store.src(), map, iv, subst_iv),
            map_operand(data, store.dest(), map, iv, subst_iv),
        ),
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
        // One vector load (a), one vector store (b), no scalar loads left.
        assert_eq!(vector_load_count(&program, function), 1);
        assert_eq!(scalar_load_count(&program, function), 0);
        // R == 0: no epilogue, the latch still exits to `exit`.
        let insts = latch_of(&program, function, latch);
        let terminator = *insts.last().unwrap();
        let InstKind::Branch(branch) = program.func_data(function).inst_data(terminator).kind()
        else {
            panic!("latch must end in a branch");
        };
        assert_eq!(branch.f_target(), exit, "no epilogue for trip % 4 == 0");
        // The counter now steps by 4 and the index by 4.
        let step_is = |inst: &Inst, op: BinaryOp| {
            matches!(
                program.func_data(function).inst_data(*inst).kind(),
                InstKind::Binary(b) if b.op() == op && matches!(
                    program.func_data(function).inst_data(b.rhs()).kind(),
                    InstKind::Integer(v) if v.value() == 4
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
    fn rejects_non_exact_trip() {
        // The trip counter comes from a function parameter: no versioning in
        // v1, so the loop must be left scalar.
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
            !run(&mut program, function),
            "runtime trip count must be rejected (no versioning in v1)"
        );
    }

    #[test]
    fn rejects_array_param_base() {
        // dst[j] = src[j] over array parameters: caller alignment is
        // unknown, so v1 rejects (versioning is M43).
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
        assert_eq!(vector_load_count(&program, function), 1);
        assert_eq!(scalar_load_count(&program, function), 0);
        assert_eq!(
            vector_binaries(&program, function),
            vec![BinaryOp::Shl, BinaryOp::Xor],
            "payload binaries become lane-wise vector ops in order"
        );
    }

    #[test]
    fn rejects_i32_div() {
        // NEON has no integer vector divide (`sdiv` is scalar-only), so i32
        // Div stays scalar; constant-divisor loops are rewritten to shift
        // chains by StrengthReduction before the vectorizer's next look.
        let mut program = Program::new();
        let (function, _header, _latch, _exit) =
            build_op_chain(&mut program, Type::get_i32(), 16, &[BinaryOp::Div], &[2]);
        assert!(!run(&mut program, function), "i32 div must be rejected");
    }

    #[test]
    fn vectorizes_f32_div_loop() {
        // b[i] = a[i] / 2.0: float vector divide (fdiv v.4s).
        let mut program = Program::new();
        let (function, _header, _latch, _exit) =
            build_op_chain(&mut program, Type::get_f32(), 16, &[BinaryOp::Div], &[2]);
        assert!(run(&mut program, function), "f32 div loop must vectorize");
        assert_eq!(vector_binaries(&program, function), vec![BinaryOp::Div]);
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
            2,
            "both b[i] and a[i] become vector loads"
        );
        assert_eq!(scalar_load_count(&program, function), 0);
        assert_eq!(
            vector_binaries(&program, function),
            vec![BinaryOp::Add],
            "the update becomes a lane-wise add"
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
        assert_eq!(vector_load_count(&program, function), 1);
        assert_eq!(scalar_load_count(&program, function), 0);
        assert_eq!(
            vector_binaries(&program, function),
            vec![BinaryOp::Add],
            "the payload add becomes a lane-wise add"
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
            "iv_final = i0 + VF*q = 0 + 4*4"
        );
        // The back edge steps the IV by the vector width.
        let t_args = branch.t_args().to_vec();
        let InstKind::Binary(step) = data.inst_data(t_args[1]).kind() else {
            panic!("back-edge IV arg must be an add");
        };
        assert_eq!(step.op(), BinaryOp::Add);
        assert_eq!(step.lhs(), params[1], "iv' = iv + 4");
        let InstKind::Integer(step_c) = data.inst_data(step.rhs()).kind() else {
            panic!("IV step must be a constant");
        };
        assert_eq!(step_c.value(), 4, "IV steps by the vector width");
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
            1,
            "a[i] becomes a vector load"
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
        assert_eq!(vector_load_count(&program, function), 1, "only the inner loop vectorizes");
    }
}
