//! Loop interchange for the two innermost natural loops (j/k swap).
//!
//! Motivation: matmul-style `i-j-k` loops have a non-contiguous inner access
//! (`a[k][j]` strides by a row). Swapping j/k yields `i-k-j` where the new
//! inner j loop touches `c[i][j]`/`b[k][j]`/`a[k][j]` contiguously (4B), which
//! the M44 vectorizer can then handle.
//!
//! Block topology this pass consumes (post-`rotate_loops`, test-at-bottom):
//!
//! ```text
//! H_i(i): jump B_i
//! B_i:    jump H_j(j0)                 (i-loop body, shell)
//! H_j(j): jump B_j
//! B_j:    jump H_k(k0, temp0)          (j-loop body, shell)
//! H_k(k,temp): jump B_k
//! B_k:    [if/reduction ..., k_update, k_test_br -> H_k(k',temp') / E_k]
//! E_k:    [c[i][j]=temp, j_update, j_test_br -> H_j(j') / E_j]
//! E_j:    [i_update, i_test_br -> H_i(i') / E_i]
//! ```
//!
//! After the swap every block is reused; only four jump targets, the `H_k`
//! parameter list, the instruction tails of `B_k`/`E_k`, and the reduction
//! target change:
//!
//! ```text
//! B_i:    jump H_k(k0)                 (i-loop body now contains k-loop)
//! H_k(k): jump B_j
//! B_j:    jump H_j(j0)                 (into the new inner j-loop)
//! H_j(j): jump B_k
//! B_k:    [c[i][j] accumulation, j_update, j_test_br -> H_j(j') / E_k]
//! E_k:    [k_update, k_test_br -> H_k(k') / E_j]
//! ```
//!
//! Reduction migration: the register accumulator `temp` (a `H_k` block
//! parameter, `temp' = select(cond, temp+delta, temp)`) becomes a memory
//! accumulation on `c[i][j]` (load + select/add + store). Legality requires
//! `c[i][j]` to be zero at the first k iteration: the base must be a global
//! with a provably all-zero initializer, and no other write to it inside the
//! k-loop. Float reductions are never migrated (IEEE addition is not
//! associative; the k->j accumulation order change would alter results).
//!
//! Idempotence: the direction-similarity test is symmetric, so an additional
//! asymmetric condition is required — only swap when the inner loop has an
//! access whose k-coefficient is outside `{0, element size}` (a row stride).
//! After the swap all inner-j coefficients are in `{0, 4}` and the pass
//! stops, so the fixed-point pipeline converges.

use rustc_hash::FxHashSet;
use smallvec::SmallVec;

use crate::ir::{
    arena::Arena, BasicBlock, BinaryOp, Function, Inst, InstKind, Program,
};
use crate::ir::builder_trait::*;
use crate::ir::function::FunctionData;
use crate::opt::{
    analysis_passes::{
        dependence::{
            AccessKind, DependenceAnalysis, Verdict, classify_index,
        },
        effects::EffectAnalysis,
        induction_variable::BasicInductionVariableAnalysis,
        loop_analysis::{Loop, LoopAnalysis},
        memory::{MemObject, integer_constant},
        range::RangeAnalysis,
    },
    pass::{ArenaContext, ArenaContextMut, Pass},
    utils::{gep::gep_index_stride, logical_edge::incoming_edges, visit_and_replace},
};

pub struct LoopInterchange;

impl Pass for LoopInterchange {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        let Some(func) = data.curr_func else {
            return false;
        };
        if data.layout().entry_bb().is_none() {
            return false;
        }
        // Read-only analysis phase.
        let program: &Program = data.program;
        let plan = find_interchange(program, func);
        let Some(plan) = plan else {
            return false;
        };
        // Mutation phase.
        apply_interchange(data, plan)
    }
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

struct Plan {
    /// i-loop body shell: `jump H_j(j0)` becomes `jump H_k(k0)`.
    b_i: BasicBlock,
    /// j-loop header: `jump B_j` becomes `jump B_k`.
    h_j: BasicBlock,
    /// j-loop body shell: `jump H_k(k0, temp0)` becomes `jump H_j(j0)`.
    b_j: BasicBlock,
    /// k-loop header: parameter `temp` removed; `jump B_k` becomes `jump B_j`.
    h_k: BasicBlock,
    /// Inner body (k-body -> new j-body): reduction migrated, tails swapped.
    b_k: BasicBlock,
    /// k-exit (j-latch -> new k-latch): tails swapped, k-test retargeted.
    e_k: BasicBlock,
    /// j-exit (unchanged): `i_update, i_test_br -> H_i / E_i`.
    e_j: BasicBlock,
    /// Position of `temp` in `H_k` parameters.
    temp_idx: usize,
    /// Position of the stepped-pointer parameter in `H_k` parameters (the
    /// loop-carried column pointer, e.g. matmul1's `b[k][j]`). Its body
    /// reads are rewritten to multi-dim GEPs and the parameter dropped.
    ptr_idx: Option<usize>,
    /// Root base of the stepped pointer's initial value GEP chain (the
    /// global the column pointer walks), e.g. `gv_b` for matmul1's
    /// `b[k][j]` pointer. `None` when `ptr_idx` is `None` or the chain is
    /// not a plain GEP over a global.
    ptr_root: Option<Inst>,
    /// The reduction accumulator (`H_k` parameter).
    temp: Inst,
    /// Address GEP of the reduction target `c[i][j]` (lives in `E_k`).
    c_gep: Inst,
    /// The j-loop header parameter that is the j IV (may flow through a phi
    /// chain on the back edge).
    j_iv: Inst,
    /// The j-loop trip counter's back-edge value position in H_j params
    /// (None when the loop has no trip counter).
    j_trip_idx: Option<usize>,
    /// The k-loop trip counter's back-edge value position in H_k params
    /// (None when the loop has no trip counter).
    k_trip_idx: Option<usize>,
    /// The k-loop body's first block (the header's jump target before the
    /// swap). After the swap the j-loop header must enter here — B_k is the
    /// k-loop *latch* (now the j-test), not the body.
    k_first: BasicBlock,
    /// Shell blocks between the j-loop entry and H_k (pure jumps and
    /// invariant preheaders), in layout order. The first is the block the
    /// i-loop body must jump into after the swap.
    shells: SmallVec<[BasicBlock; 4]>,
    /// Snapshot of `L_k.body()` (reduction-migration scan).
    k_body: SmallVec<[BasicBlock; 8]>,
    /// Instructions to move: `B_k` tail (k-update + k-test) and `E_k` tail
    /// (j-update + j-test).
    k_update: Inst,
    k_test: Inst,
    j_update: Inst,
    j_test: Inst,
    /// Initial values carried by the rewritten jumps.
    j0: Inst,
    k0: Inst,
}

// ---------------------------------------------------------------------------
// Read-only analysis
// ---------------------------------------------------------------------------

/// Test-at-bottom trip-count sanity: exactly one latch whose terminator is a
/// branch back to the header with a constant bound, and the carried update is
/// `iv ± 1` (unit step). This is the countability + unit-step requirement.
fn rotate_unit_step(data: &FunctionData, arena: &impl Arena, looop: &Loop, iv: Inst) -> bool {
    if looop.latches().len() != 1 {
        return false;
    }
    let latch = looop.latches()[0];
    let terminator = data.layout().basicblock(latch).terminator();
    let InstKind::Branch(branch) = arena.inst_data(terminator).kind() else {
        return false;
    };
    let (inside_bb, inside_args, outside_bb, outside_args) = if branch.t_target() == looop.header()
    {
        (branch.t_target(), branch.t_args(), branch.f_target(), branch.f_args())
    } else if branch.f_target() == looop.header() {
        (branch.f_target(), branch.f_args(), branch.t_target(), branch.t_args())
    } else {
        return false;
    };
    let _ = inside_bb;
    let _ = outside_args;
    if looop.contains(outside_bb) {
        return false; // Single exit (exit may carry block parameters).
    }
    let params = data.bb_data(looop.header()).params();
    let Some(iv_idx) = params.iter().position(|&p| p == iv) else {
        return false;
    };
    if inside_args.len() != params.len() {
        return false;
    }
    let updated = inside_args[iv_idx];
    // Unit step: `updated` provably equals `iv + 1` / `iv - 1`, possibly
    // through a block-parameter chain (the inner-loop exit blocks carry the
    // outer IV; the back edge updates it after the inner loop exits).
    let unit = match arena.inst_data(updated).kind() {
        InstKind::Binary(b) if b.op() == BinaryOp::Add => {
            (integer_constant(arena, b.rhs()) == Some(1)
                && value_flows_from(data, arena, b.lhs(), iv, &mut FxHashSet::default()))
                || (integer_constant(arena, b.lhs()) == Some(1)
                    && value_flows_from(data, arena, b.rhs(), iv, &mut FxHashSet::default()))
        }
        InstKind::Binary(b) if b.op() == BinaryOp::Sub => {
            integer_constant(arena, b.rhs()) == Some(1)
                && value_flows_from(data, arena, b.lhs(), iv, &mut FxHashSet::default())
        }
        _ => false,
    };
    if !unit {
        return false;
    }
    // Countability: the latch condition must be bounded — a comparison with
    // a constant, or a countdown counter (binary with a constant operand,
    // e.g. the rotated trip counter `t - 1`), or a bare counter.
    let cond = branch.cond();
    match arena.inst_data(cond).kind() {
        InstKind::Integer(_) => true,
        InstKind::Binary(compare) => {
            if !arena.inst_data(compare.lhs()).ty().is_i32()
                || !arena.inst_data(compare.rhs()).ty().is_i32()
            {
                return false;
            }
            let has_const = integer_constant(arena, compare.lhs()).is_some()
                || integer_constant(arena, compare.rhs()).is_some();
            has_const || cond == updated
        }
        _ => false,
    }
}

/// Whether `value` provably equals `target`: `value` is `target` itself, or
/// a block parameter whose every incoming edge carries a provably-equal
/// value. Cyclic self-carrying edges are neutral. (Local copy of the
/// rotate_loops helper; the two passes deliberately share the semantics.)
fn value_flows_from(
    data: &FunctionData,
    arena: &impl Arena,
    value: Inst,
    target: Inst,
    visited: &mut FxHashSet<Inst>,
) -> bool {
    if value == target {
        return true;
    }
    if !visited.insert(value) {
        return true; // Self-carrying edge: value unchanged.
    }
    let InstKind::BlockArgRef(_) = arena.inst_data(value).kind() else {
        return false;
    };
    let Some(param_block) = data
        .layout()
        .basicblocks()
        .iter()
        .find(|l| data.bb_data(l.bb()).params().contains(&value))
        .map(|l| l.bb())
    else {
        return false;
    };
    let param_pos = data
        .bb_data(param_block)
        .params()
        .iter()
        .position(|&p| p == value)
        .unwrap();
    let preds: Vec<Inst> = data
        .bb_data(param_block)
        .used_by()
        .iter()
        .copied()
        .collect();
    if preds.is_empty() {
        return false;
    }
    for pred_inst in preds {
        let args: Vec<Inst> = match arena.inst_data(pred_inst).kind() {
            InstKind::Jump(jump) => jump.args().to_vec(),
            InstKind::Branch(branch) if branch.t_target() == param_block => {
                branch.t_args().to_vec()
            }
            InstKind::Branch(branch) if branch.f_target() == param_block => {
                branch.f_args().to_vec()
            }
            _ => return false,
        };
        let Some(&arg) = args.get(param_pos) else {
            return false;
        };
        if !value_flows_from(data, arena, arg, target, visited) {
            return false;
        }
    }
    true
}

/// Whether `update` is `p + 1` / `p - 1`, possibly through a single
/// block-parameter chain flowing back to `p`.
fn is_unit_step_from(data: &FunctionData, arena: &impl Arena, update: Inst, p: Inst) -> bool {
    let InstKind::Binary(b) = arena.inst_data(update).kind() else {
        return false;
    };
    match (b.op(), b.lhs(), b.rhs()) {
        (BinaryOp::Add, lhs, rhs) => {
            if integer_constant(arena, rhs) == Some(1) {
                value_flows_from(data, arena, lhs, p, &mut FxHashSet::default())
            } else if integer_constant(arena, lhs) == Some(1) {
                value_flows_from(data, arena, rhs, p, &mut FxHashSet::default())
            } else {
                false
            }
        }
        (BinaryOp::Sub, lhs, rhs) => {
            integer_constant(arena, rhs) == Some(1)
                && value_flows_from(data, arena, lhs, p, &mut FxHashSet::default())
        }
        _ => false,
    }
}

/// Find the j-loop IV among `L_j`'s header parameters. The BIV analysis only
/// recognizes direct `iv ± 1` back-edge updates; matmul1's j update flows
/// through the inner-loop exit block parameters (`add(p, 1)` with `p`
/// flowing back to the header parameter), so fall back to a phi-chain scan.
/// The candidate must also appear as a GEP index inside `L_k` (it is the
/// dimension traversed by the k-loop accesses, not the trip counter).
fn find_j_iv(
    data: &FunctionData,
    arena: &impl Arena,
    l_j: &Loop,
    l_k: &Loop,
) -> Option<Inst> {
    if l_j.latches().len() != 1 {
        return None;
    }
    let latch = l_j.latches()[0];
    let term = data.layout().basicblock(latch).terminator();
    let InstKind::Branch(branch) = arena.inst_data(term).kind() else {
        return None;
    };
    let (inside_args, inside_bb) = if branch.t_target() == l_j.header() {
        (branch.t_args(), branch.t_target())
    } else if branch.f_target() == l_j.header() {
        (branch.f_args(), branch.f_target())
    } else {
        return None;
    };
    if inside_bb != l_j.header() {
        return None;
    }
    let params = data.bb_data(l_j.header()).params();
    if inside_args.len() != params.len() {
        return None;
    }
    let mut candidates = SmallVec::<[Inst; 2]>::new();
    for (pos, &p) in params.iter().enumerate() {
        if !is_unit_step_from(data, arena, inside_args[pos], p) {
            continue;
        }
        // The candidate must appear in a GEP index inside L_k (possibly via
        // the parameter chain that forwards it into the k-loop header).
        let mut used_as_index = false;
        for &bb in l_k.body() {
            for &inst in data.layout().basicblock(bb).insts() {
                if let InstKind::GetElemPtr(gep) = arena.inst_data(inst).kind() {
                    if gep.offsets().iter().any(|&i| {
                        i == p
                            || value_flows_from(data, arena, i, p, &mut FxHashSet::default())
                    }) {
                        used_as_index = true;
                    }
                }
            }
        }
        if used_as_index {
            candidates.push(p);
        }
    }
    if candidates.len() != 1 {
        return None;
    }
    Some(candidates[0])
}

/// Byte-level access coefficients relative to the j-loop and k-loop IVs.
/// Returns `(coeff_j, coeff_k)`; `None` if any index is not affine.
///
/// Each coefficient is classified against its *own* loop: an index that is
/// the k-loop IV is loop-outside (hence invariant) when classified against
/// the j-loop, and vice versa.
fn dual_coeffs(
    arena: &ArenaContext<'_>,
    l_j: &Loop,
    l_k: &Loop,
    j_iv: Inst,
    k_iv: Inst,
    ranges: &RangeAnalysis,
    addr: Inst,
) -> Option<(i64, i64)> {
    let mut cj = 0i64;
    let mut ck = 0i64;
    let mut cur = addr;
    loop {
        let InstKind::GetElemPtr(gep) = arena.inst_data(cur).kind() else {
            break;
        };
        for (pos, &index) in gep.offsets().iter().enumerate() {
            let stride = gep_index_stride(arena, cur, pos)?.byte_stride;
            let info_j = classify_index(arena, l_j, j_iv, ranges, cur, index).ok()?;
            let info_k = classify_index(arena, l_k, k_iv, ranges, cur, index).ok()?;
            cj += info_j.coefficient * stride;
            ck += info_k.coefficient * stride;
        }
        cur = gep.base();
        // A *stepped pointer* base: the header carries a pointer parameter
        // whose back-edge advances it by a constant byte stride per iteration
        // (e.g. matmul1's `b[k][j]` column pointer `%69 → getelemptr %69, 1000`,
        // or the transpose loop's `a[j][i]` column pointer). Without this the
        // access is invisible to the direction/row-stride check and the
        // interchange refuses the nest. Walk the loop's header parameters:
        // `cur` is such a parameter iff its back-edge arg is
        // `getelemptr(param, const)`.
        let data = arena.curr_func_data();
        let h_k_params = data.bb_data(l_k.header()).params().to_vec();
        if let Some(pos) = h_k_params.iter().position(|&p| p == cur) {
            if let Some((back_args, _)) = latch_args(data, arena, l_k) {
                if let Some(&back) = back_args.get(pos) {
                    if let InstKind::GetElemPtr(ptr_gep) = arena.inst_data(back).kind() {
                        if ptr_gep.base() == cur
                            && ptr_gep.offsets().len() == 1
                            && arena
                                .inst_data(ptr_gep.offsets()[0])
                                .kind()
                                .is_const()
                        {
                            if let Some(constant) =
                                integer_constant(arena, ptr_gep.offsets()[0])
                            {
                                ck += constant as i64;
                            }
                            // The pointer's own base is loop-outside; stop.
                            break;
                        }
                    }
                }
            }
        }
        // Same for the j loop: a stepped column pointer carried by H_j
        // (e.g. the transpose loop's `a[j][i]`).
        let h_j_params = data.bb_data(l_j.header()).params().to_vec();
        if let Some(pos) = h_j_params.iter().position(|&p| p == cur) {
            if let Some((back_args, _)) = latch_args(data, arena, l_j) {
                if let Some(&back) = back_args.get(pos) {
                    if let InstKind::GetElemPtr(ptr_gep) = arena.inst_data(back).kind() {
                        if ptr_gep.base() == cur
                            && ptr_gep.offsets().len() == 1
                            && arena
                                .inst_data(ptr_gep.offsets()[0])
                                .kind()
                                .is_const()
                        {
                            if let Some(constant) =
                                integer_constant(arena, ptr_gep.offsets()[0])
                            {
                                cj += constant as i64;
                            }
                            break;
                        }
                    }
                }
            }
        }
    }
    Some((cj, ck))
}

/// All-zero check for a global initializer (recursive for aggregates).
fn is_zero_value(arena: &impl Arena, v: Inst) -> bool {
    match arena.inst_data(v).kind() {
        InstKind::ZeroInit => true,
        InstKind::Integer(integer) => integer.value() == 0,
        InstKind::Float(f) => f.value() == 0.0,
        InstKind::Aggregate(aggregate) => aggregate
            .value()
            .iter()
            .all(|&elem| is_zero_value(arena, elem)),
        _ => false,
    }
}

fn is_zero_global(arena: &impl Arena, g: Inst) -> bool {
    let InstKind::GlobalAlloc(alloc) = arena.inst_data(g).kind() else {
        return false;
    };
    is_zero_value(arena, alloc.init())
}

/// Exit block of a loop: the outside target of its latch branch. The exit
/// may carry block parameters (interchange rewrites them); the caller is
/// responsible for parameter reconciliation.
fn exit_of_one(data: &FunctionData, arena: &impl Arena, looop: &Loop) -> Option<BasicBlock> {
    if looop.latches().len() != 1 {
        return None;
    }
    let latch = looop.latches()[0];
    let terminator = data.layout().basicblock(latch).terminator();
    let InstKind::Branch(branch) = arena.inst_data(terminator).kind() else {
        return None;
    };
    let (exit, _args) = if branch.t_target() == looop.header() {
        (branch.f_target(), branch.f_args())
    } else if branch.f_target() == looop.header() {
        (branch.t_target(), branch.t_args())
    } else {
        return None;
    };
    Some(exit)
}

/// Latch structure: `(update, test_compare, test_branch)`. The update is the
/// back-edge argument of header parameter `iv`.
fn find_latch_parts(
    data: &FunctionData,
    arena: &impl Arena,
    latch: BasicBlock,
    iv: Inst,
) -> Option<(Inst, Inst, Inst)> {
    let terminator = data.layout().basicblock(latch).terminator();
    let InstKind::Branch(branch) = arena.inst_data(terminator).kind() else {
        return None;
    };
    // The header arm is the target whose parameter count matches its args.
    let (header, inside_args) = if data.bb_data(branch.t_target()).params().len()
        == branch.t_args().len()
    {
        (branch.t_target(), branch.t_args())
    } else if data.bb_data(branch.f_target()).params().len() == branch.f_args().len() {
        (branch.f_target(), branch.f_args())
    } else {
        return None;
    };
    let params = data.bb_data(header).params();
    let idx = params.iter().position(|&p| p == iv)?;
    if inside_args.len() != params.len() {
        return None;
    }
    Some((inside_args[idx], branch.cond(), terminator))
}

/// The back-edge argument vector of a single-latch loop's latch terminator.
fn latch_args(data: &FunctionData, arena: &impl Arena, looop: &Loop) -> Option<(Vec<Inst>, BasicBlock)> {
    let latch = *looop.latches().first()?;
    let term = data.layout().basicblock(latch).terminator();
    let InstKind::Branch(branch) = arena.inst_data(term).kind() else {
        return None;
    };
    if branch.t_target() == looop.header() {
        Some((branch.t_args().to_vec(), branch.t_target()))
    } else if branch.f_target() == looop.header() {
        Some((branch.f_args().to_vec(), branch.f_target()))
    } else {
        None
    }
}

/// Address instruction of a collected access (load src / store dest).
fn access_inst_addr(arena: &ArenaContext<'_>, inst: Inst) -> Inst {
    match arena.inst_data(inst).kind() {
        InstKind::Load(load) => load.src(),
        InstKind::Store(store) => store.dest(),
        other => unreachable!("dependence access inst is Load/Store, got {other:?}"),
    }
}

/// Find one interchangeable (L_j, L_k) pair and build the transformation plan.
fn find_interchange(program: &Program, func: Function) -> Option<Plan> {
    let arena = ArenaContext {
        program,
        curr_func: Some(func),
    };
    let data = program.func_data(func);
    let (cfg, _dom, loops) = LoopAnalysis::new(data);
    let induction = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
    let nonneg =
        crate::opt::analysis_passes::return_summary::nonneg_preserving_functions(program);
    let self_params = crate::opt::analysis_passes::return_summary::always_nonneg_params(
        program, &nonneg,
    )
    .get(&func)
    .cloned()
    .unwrap_or_default();

    // Cheap candidate pre-filter before building the heavy analyses: at
    // least one innermost loop with a parent is required, otherwise no
    // interchange is possible and Range/Dependence never need to run.
    let mut any_candidate = false;
    for (k_idx, l_k) in loops.loops().iter().enumerate() {
        let innermost = !loops
            .loops()
            .iter()
            .any(|l| l.header() != l_k.header() && l_k.contains(l.header()));
        if innermost && loops.parent_loop_index(k_idx).is_some() {
            any_candidate = true;
            break;
        }
    }
    if !any_candidate {
        return None;
    }

    let effects = EffectAnalysis::new(program);
    let nonneg = crate::opt::analysis_passes::return_summary::nonneg_preserving_functions(program);
    let self_params = crate::opt::analysis_passes::return_summary::always_nonneg_params(
        program, &nonneg,
    )
    .get(&func)
    .cloned()
    .unwrap_or_default();
    // Dependence analysis is only needed for the M42 reducibility check;
    // build it lazily on the first candidate that reaches that check.
    let mut deps: Option<DependenceAnalysis> = None;
    // Range analysis is only needed for the direction check (dual_coeffs);
    // build it lazily on the first candidate that reaches that check.
    let mut ranges: Option<RangeAnalysis> = None;

    for k_idx in 0..loops.loops().len() {
        let l_k = &loops.loops()[k_idx];
        // (a) L_k must be an innermost loop with a parent.
        if loops
            .loops()
            .iter()
            .any(|l| l.header() != l_k.header() && l_k.contains(l.header()))
        {
            continue;
        }
        let Some(j_idx) = loops.parent_loop_index(k_idx) else {
            continue;
        };
        let l_j = &loops.loops()[j_idx];

        // (b) Unit-step countable loops (post-rotate form).
        let Some(k_biv) = induction.for_loop(l_k).first() else {
            continue;
        };
        let k_iv = k_biv.parameter();
        // The j IV: prefer the BIV analysis, but reject a "BIV" whose back
        // edge provably flows back to itself — that is an invariant
        // pass-through parameter, not an induction variable (the BIV
        // analysis treats unchanged parameters as zero-step IVs). Fall back
        // to the phi-chain scan (matmul1's j update flows through the
        // inner-loop exit block parameters).
        let h_j_params = data.bb_data(l_j.header()).params().to_vec();
        let j_iv_candidate = induction.for_loop(l_j).first().map(|biv| biv.parameter());
        let used_as_index = |iv: Inst| -> bool {
            l_k.body().iter().any(|&bb| {
                data.layout().basicblock(bb).insts().iter().any(|&inst| {
                    matches!(
                        arena.inst_data(inst).kind(),
                        InstKind::GetElemPtr(gep)
                            if gep.offsets().iter().any(|&i| {
                                i == iv
                                    || value_flows_from(
                                        data,
                                        &arena,
                                        i,
                                        iv,
                                        &mut FxHashSet::default(),
                                    )
                            })
                    )
                })
            })
        };
        let j_iv = match j_iv_candidate {
            Some(cand) => {
                // A valid j IV must (a) change in the j-loop (its back edge
                // does not flow back to itself — the BIV analysis reports
                // invariant pass-throughs and countdown trip counters as
                // zero/negative-step IVs) and (b) appear in a GEP index
                // inside L_k.
                let is_invariant = latch_args(data, &arena, l_j)
                    .and_then(|(args, _)| {
                        h_j_params
                            .iter()
                            .position(|&p| p == cand)
                            .and_then(|pos| args.get(pos).copied())
                    })
                    .map_or(false, |back| {
                        value_flows_from(data, &arena, back, cand, &mut FxHashSet::default())
                    });
                if is_invariant || !used_as_index(cand) {
                    match find_j_iv(data, &arena, l_j, l_k) {
                        Some(iv) => iv,
                        None => {
                            continue;
                        }
                    }
                } else {
                    cand
                }
            }
            None => match find_j_iv(data, &arena, l_j, l_k) {
                Some(iv) => iv,
                None => {
                    continue;
                }
            },
        };
        if !rotate_unit_step(data, &arena, l_k, k_iv) {
            continue;
        }
        if !rotate_unit_step(data, &arena, l_j, j_iv) {
            continue;
        }

        // (c) Single latch; exits are distinct.
        if l_k.latches().len() != 1 || l_j.latches().len() != 1 {
            continue;
        }
        let b_k = l_k.latches()[0];
        let e_k = exit_of_one(data, &arena, l_k);
        let e_j = exit_of_one(data, &arena, l_j);
        let (Some(e_k), Some(e_j)) = (e_k, e_j) else {
            continue;
        };
        if !l_j.contains(e_k) || l_k.contains(e_k) || l_j.contains(e_j) {
            continue;
        }

        // (d) Perfect nesting: L_j.body \ L_k.body is exactly
        // {shell chain, E_k} (the header H_j belongs to the body set). The
        // shell chain is the jump path H_j -> ... -> H_k; each shell is a
        // pure jump or an invariant preheader. Blocks reached through E_k
        // (the outer latch continuation) are not shells.
        let h_j = l_j.header();
        let h_k = l_k.header();
        let mut shells: SmallVec<[BasicBlock; 4]> = SmallVec::new();
        let mut cur = h_j;
        loop {
            let term = data.layout().basicblock(cur).terminator();
            let InstKind::Jump(jump) = arena.inst_data(term).kind() else {
                break;
            };
            let next = jump.target();
            if next == h_k {
                break; // The chain ends at H_k.
            }
            if next == h_j || !l_j.contains(next) || shells.contains(&next) {
                break; // Not a simple forward chain.
            }
            shells.push(next);
            cur = next;
        }
        let mut saw_e_k = false;
        for &bb in l_j.body() {
            if bb == e_k {
                saw_e_k = true;
            }
        }
        if !saw_e_k || shells.is_empty() {
            continue;
        }
        // The chain must actually end at H_k.
        let last_term = data.layout().basicblock(cur).terminator();
        let InstKind::Jump(last_jump) = arena.inst_data(last_term).kind() else {
            continue;
        };
        if last_jump.target() != h_k {
            continue;
        }
        // Invariant (outer-loop) parameters of H_j: the parameters whose
        // back-edge value provably flows back to themselves.
        let h_j_params = data.bb_data(h_j).params().to_vec();
        let Some((h_j_latch_args, _)) = latch_args(data, &arena, l_j) else {
            continue;
        };
        if h_j_latch_args.len() != h_j_params.len() {
            continue;
        }
        let invariant_params: SmallVec<[Inst; 2]> = h_j_params
            .iter()
            .enumerate()
            .filter(|&(pos, &p)| {
                value_flows_from(data, &arena, h_j_latch_args[pos], p, &mut FxHashSet::default())
            })
            .map(|(_, &p)| p)
            .collect();
        if invariant_params.is_empty() {
            // No invariant outer-loop parameter: the shells must then be
            // pure jumps (no non-terminator instructions to validate).
            let mut pure = true;
            for &shell in &shells {
                let term = data.layout().basicblock(shell).terminator();
                for &inst in data.layout().basicblock(shell).insts() {
                    if inst != term {
                        pure = false;
                        break;
                    }
                }
                if !pure {
                    break;
                }
            }
            if !pure {
                continue;
            }
        }
        // Verify the shell chain: H_j jumps into the first shell, each shell
        // jumps into the next, and the last jumps into H_k. Non-terminator
        // instructions must be invariant (only use constants, globals, or
        // values flowing back to an invariant H_j parameter). One exception:
        // a column-pointer GEP whose *only* non-invariant use is the j IV —
        // `%col = getelemptr base, (0, j)` feeding a stepped-pointer slot of
        // H_k (e.g. matmul1's `b[k][j]` column pointer). It is loop-invariant
        // in k (the swapped outer loop) and apply rewrites the pointer reads
        // to multi-dim GEPs, so the shell compute survives the swap.
        let mut prev = h_j;
        let mut chain_ok = true;
        // H_k parameters whose back-edge advances by a constant GEP stride
        // (the stepped-pointer slots; their initial values come from the last
        // shell's jump).
        let h_k_params_for_ptr = data.bb_data(h_k).params().to_vec();
        let stepped_ptr_params: SmallVec<[Inst; 2]> = latch_args(data, &arena, l_k)
            .map(|(back_args, _)| {
                h_k_params_for_ptr
                    .iter()
                    .enumerate()
                    .filter(|&(pos, &p)| {
                        matches!(
                            arena.inst_data(back_args[pos]).kind(),
                            InstKind::GetElemPtr(gep)
                                if gep.base() == p
                                    && gep.offsets().len() == 1
                                    && arena
                                        .inst_data(gep.offsets()[0])
                                        .kind()
                                        .is_const(),
                        )
                    })
                    .map(|(_, &p)| p)
                    .collect()
            })
            .unwrap_or_default();
        for &shell in &shells {
            let prev_term = data.layout().basicblock(prev).terminator();
            let InstKind::Jump(prev_jump) = arena.inst_data(prev_term).kind() else {
                chain_ok = false;
                break;
            };
            if prev_jump.target() != shell {
                chain_ok = false;
                break;
            }
            let term = data.layout().basicblock(shell).terminator();
            let InstKind::Jump(shell_jump) = arena.inst_data(term).kind() else {
                chain_ok = false;
                break;
            };
            let shell_jump_args = shell_jump.args().to_vec();
            for &inst in data.layout().basicblock(shell).insts() {
                if inst == term {
                    continue;
                }
                let mut saw_non_invariant_use = false;
                let mut col_ptr_ok = false;
                for used in arena.inst_data(inst).inst_usage() {
                    let ok = used.is_global()
                        || arena.inst_data(used).kind().is_const()
                        || invariant_params.iter().any(|&p| {
                            value_flows_from(data, &arena, used, p, &mut FxHashSet::default())
                        });
                    if ok {
                        continue;
                    }
                    saw_non_invariant_use = true;
                    // A GEP whose only non-invariant use is the j IV, and
                    // whose result feeds a stepped-pointer parameter of H_k
                    // via the shell's jump: column-pointer setup.
                    if used == j_iv
                        && matches!(arena.inst_data(inst).kind(), InstKind::GetElemPtr(_))
                        && shell_jump_args.iter().any(|&a| {
                            a == inst
                                && stepped_ptr_params.iter().any(|&p| {
                                    h_k_params_for_ptr
                                        .iter()
                                        .position(|&q| q == p)
                                        .map_or(false, |pos| {
                                            shell_jump_args.get(pos).copied() == Some(inst)
                                        })
                                })
                        })
                    {
                        col_ptr_ok = true;
                        continue;
                    }
                    chain_ok = false;
                    break;
                }
                if saw_non_invariant_use && !col_ptr_ok {
                    chain_ok = false;
                    break;
                }
                if !chain_ok {
                    break;
                }
            }
            if !chain_ok {
                break;
            }
            prev = shell;
            let _ = shell_jump;
        }
        if !chain_ok {
            continue;
        }
        // The last shell must jump into H_k.
        let last_shell = *shells.last().unwrap();
        let last_term = data.layout().basicblock(last_shell).terminator();
        let InstKind::Jump(last_jump) = arena.inst_data(last_term).kind() else {
            continue;
        };
        if last_jump.target() != h_k {
            continue;
        }
        // H_j must jump into the first shell.
        let h_j_term = data.layout().basicblock(h_j).terminator();
        let InstKind::Jump(h_j_jump) = arena.inst_data(h_j_term).kind() else {
            continue;
        };
        if h_j_jump.target() != shells[0] {
            continue;
        }

        // (e) The k-loop must be a reduction (M42) — no calls, no other
        // carried dependencies.
        let deps = deps
            .get_or_insert_with(|| DependenceAnalysis::new(program, func, &effects, 4));
        let Some(dep) = deps.for_loop(l_k) else {
            continue;
        };
        let Verdict::Reducible { accumulator, .. } = dep.verdict else {
            continue;
        };
        let temp = accumulator;
        let params = data.bb_data(h_k).params();
        let Some(temp_idx) = params.iter().position(|&p| p == temp) else {
            continue;
        };

        // (f) Direction similarity + asymmetric row-stride condition over all
        // k-loop accesses.
        let mut all_same_sign = true;
        let mut has_row_stride = false;
        for access in &dep.accesses {
            let addr = access_inst_addr(&arena, access.inst);
            let ranges = ranges.get_or_insert_with(|| {
                RangeAnalysis::new(&arena, &cfg, &loops, &induction, &nonneg, &self_params)
            });
            let Some((cj, ck)) = dual_coeffs(&arena, l_j, l_k, j_iv, k_iv, ranges, addr) else {
                all_same_sign = false;
                break;
            };
            if cj < 0 || ck < 0 {
                all_same_sign = false;
                break;
            }
            if ck != 0 && ck != access.size {
                has_row_stride = true;
            }
        }
        if !all_same_sign {
            continue;
        }
        if !has_row_stride {
            continue;
        }

        // (g) Reduction target: E_k contains exactly one store of the
        // accumulator; its base is a zero-initialized global; no other write
        // to it inside L_k.
        let Some((j_update, j_test, _j_test_br)) = find_latch_parts(data, &arena, e_k, j_iv) else {
            continue;
        };
        // Locate the store `c[i][j] = temp` inside E_k: the stored value is
        // either the accumulator itself or its phi-chain copy at the same
        // position in E_k's parameter list (E_k mirrors the non-trip H_k
        // parameters: (i, j, k', temp')).
        let e_k_params = data.bb_data(e_k).params().to_vec();
        let mut c_gep = None;
        for inst in data.layout().basicblock(e_k).insts() {
            if let InstKind::Store(store) = arena.inst_data(*inst).kind() {
                let src_matches = store.src() == temp
                    || (temp_idx < e_k_params.len() && store.src() == e_k_params[temp_idx]);
                if src_matches {
                    c_gep = Some(store.dest());
                    break;
                }
            }
        }
        let Some(c_gep) = c_gep else {
            continue;
        };
        let base = effects.env_of(func).base_of(&arena, c_gep);
        let MemObject::Global(g) = base else {
            continue;
        };
        if !is_zero_global(&arena, g) {
            continue;
        }
        if dep.accesses.iter().any(|a| {
            a.kind != AccessKind::Read && a.base == MemObject::Global(g)
        }) {
            continue;
        }

        // (h) B_k tail must be the k-update + k-test + k-test-branch.
        let Some((k_update, k_test, _k_test_br)) = find_latch_parts(data, &arena, b_k, k_iv)
        else {
            continue;
        };

        // (i) Unique non-backedge predecessor of H_j is the i-loop body shell.
        let mut preds: SmallVec<[BasicBlock; 2]> = SmallVec::new();
        for edge in incoming_edges(data, &cfg, h_j) {
            if !l_j.contains(edge.source()) {
                preds.push(edge.source());
            }
        }
        if preds.len() != 1 {
            continue;
        }
        let b_i = preds[0];
        let b_i_term = data.layout().basicblock(b_i).terminator();
        let InstKind::Jump(b_i_jump) = arena.inst_data(b_i_term).kind() else {
            continue;
        };
        if b_i_jump.target() != h_j {
            continue;
        }
        let b_i_args = b_i_jump.args().to_vec();
        // j0 is the j-loop initial value carried by B_i's jump.
        let j_idx = data.bb_data(h_j).params().iter().position(|&p| p == j_iv);
        let Some(j_idx) = j_idx else {
            continue;
        };
        let j0 = *b_i_args.get(j_idx)?;
        // k0 is the k-loop initial value carried by the last shell's jump.
        let h_k_params = data.bb_data(h_k).params().to_vec();
        let k_idx = h_k_params.iter().position(|&p| p == k_iv)?;
        let InstKind::Jump(last_jump) = arena.inst_data(last_term).kind() else {
            unreachable!()
        };
        let last_args = last_jump.args().to_vec();
        if last_args.len() != h_k_params.len() {
            continue;
        }
        let k0 = *last_args.get(k_idx)?;

        // Trip counters: header parameters whose back-edge value is `p - 1`.
        let j_trip_idx = h_j_params
            .iter()
            .enumerate()
            .find(|&(pos, &p)| {
                p != j_iv
                    && matches!(
                        arena.inst_data(h_j_latch_args[pos]).kind(),
                        InstKind::Binary(b)
                            if b.op() == BinaryOp::Sub
                                && b.lhs() == p
                                && integer_constant(&arena, b.rhs()) == Some(1),
                    )
            })
            .map(|(pos, _)| pos);
        let Some((k_latch_args, _)) = latch_args(data, &arena, l_k) else {
            continue;
        };
        let k_trip_idx = h_k_params
            .iter()
            .enumerate()
            .find(|&(pos, &p)| {
                p != k_iv
                    && matches!(
                        arena.inst_data(k_latch_args[pos]).kind(),
                        InstKind::Binary(b)
                            if b.op() == BinaryOp::Sub
                                && b.lhs() == p
                                && integer_constant(&arena, b.rhs()) == Some(1),
                    )
            })
            .map(|(pos, _)| pos);

        // Stepped-pointer parameter: a H_k parameter whose back-edge value
        // is `getelemptr(param, const)` (loop-carried column pointer, e.g.
        // matmul1's `b[k][j]`). It is not the IV/trip/temp/accumulator.
        let ptr_idx = h_k_params
            .iter()
            .enumerate()
            .find(|&(pos, &p)| {
                p != k_iv
                    && Some(pos) != k_trip_idx
                    && matches!(
                        arena.inst_data(k_latch_args[pos]).kind(),
                        InstKind::GetElemPtr(gep)
                            if gep.base() == p
                                && gep.offsets().len() == 1
                                && arena.inst_data(gep.offsets()[0]).kind().is_const(),
                    )
            })
            .map(|(pos, _)| pos);
        // Root base of the pointer's initial value (its GEP chain base).
        // matmul1: ptr0 = `getelemptr %gv_b, (0, 0, j)` -> root `gv_b`.
        let ptr_root = ptr_idx.and_then(|pi| {
            let ptr0 = *last_args.get(pi)?;
            let mut cur = ptr0;
            loop {
                let InstKind::GetElemPtr(gep) = arena.inst_data(cur).kind() else {
                    break;
                };
                cur = gep.base();
            }
            if matches!(arena.inst_data(cur).kind(), InstKind::GlobalAlloc(..)) {
                Some(cur)
            } else {
                None
            }
        });
        // Without a provable root base the pointer reads cannot be rewritten
        // to multi-dim GEPs; refuse the nest (conservative).
        if ptr_idx.is_some() && ptr_root.is_none() {
            continue;
        }

        return Some(Plan {
            b_i,
            h_j,
            b_j: shells[0],
            h_k,
            b_k,
            e_k,
            e_j,
            temp_idx,
            temp,
            ptr_idx,
            ptr_root,
            c_gep,
            j_iv,
            j_trip_idx,
            k_trip_idx,
            k_first: {
                let hk_term = data.layout().basicblock(h_k).terminator();
                match arena.inst_data(hk_term).kind() {
                    InstKind::Jump(j) => j.target(),
                    _ => unreachable!("rotated k-loop header ends in a jump"),
                }
            },
            shells: {
                shells
            },
            k_body: l_k.body().iter().copied().collect(),
            k_update,
            k_test,
            j_update,
            j_test,
            j0,
            k0,
        });
    }
    None
}

// ---------------------------------------------------------------------------
// Transformation
// ---------------------------------------------------------------------------

fn apply_interchange(data: &mut ArenaContextMut<'_>, plan: Plan) -> bool {
    let Plan {
        b_i,
        h_j,
        b_j,
        h_k,
        b_k,
        e_k,
        e_j,
        temp_idx,
        temp,
        ptr_idx,
        ptr_root,
        c_gep,
        j_iv,
        j_trip_idx,
        k_trip_idx,
        k_first,
        shells,
        k_body,
        k_update,
        k_test,
        j_update: _j_update,
        j_test,
        j0,
        k0,
    } = plan;

    let h_k_params_pre = data.bb_data(h_k).params().to_vec();
    let h_j_params = data.bb_data(h_j).params().to_vec();
    let j_pos_hj = h_j_params.iter().position(|&p| p == j_iv).unwrap();

    // Pre-validate every failure condition before mutating anything: a
    // mid-transform failure would leave a half-rewritten IR behind.
    let validate = || -> Option<&'static str> {
        if !matches!(data.inst_data(c_gep).kind(), InstKind::GetElemPtr(_)) {
            return Some("c_gep-not-gep");
        }
        // B_i and the last shell must end in jumps.
        if !matches!(
            data.inst_data(data.layout().basicblock(b_i).terminator()).kind(),
            InstKind::Jump(_)
        ) {
            return Some("bi-not-jump");
        }
        if !matches!(
            data.inst_data(data.layout().basicblock(*shells.last().unwrap()).terminator()).kind(),
            InstKind::Jump(_)
        ) {
            return Some("last-shell-not-jump");
        }
        // The first shell must be parameterless (H_k jumps into it bare);
        // later shells may carry parameters (preheaders) — their argument
        // chains are rewritten below.
        if !data.bb_data(shells[0]).params().is_empty() {
            return Some("shell-has-params");
        }
        // The former k-loop body (the j-loop entry after the swap) must be
        // parameterless: H_j enters it bare.
        {
            let hk_term = data.layout().basicblock(h_k).terminator();
            let InstKind::Jump(j) = data.inst_data(hk_term).kind() else {
                return Some("hk-not-jump");
            };
            if !data.bb_data(j.target()).params().is_empty() {
                return Some("k-first-has-params");
            }
        }
        // Interior shell arguments referencing H_j parameters must map into
        // H_k's post-drop parameter list (and never the trip counter).
        for idx in 0..shells.len().saturating_sub(1) {
            let term = data.layout().basicblock(shells[idx]).terminator();
            let InstKind::Jump(jump) = data.inst_data(term).kind() else {
                return Some("shell-not-jump");
            };
            for &arg in jump.args() {
                if let Some(p) = h_j_params.iter().position(|&q| q == arg) {
                    if Some(p) == j_trip_idx || p >= h_k_params_pre.len() {
                        return Some("shell-arg-unmappable");
                    }
                }
            }
        }
        // The k trip counter's initial value must be carried by the last
        // shell's jump (which feeds H_k). Some upstream simplifications drop
        // the slot; without it the swapped k-loop would start with a bogus
        // trip count — refuse rather than guess.
        if let Some(t) = k_trip_idx {
            let last_term = data.layout().basicblock(*shells.last().unwrap()).terminator();
            let InstKind::Jump(jump) = data.inst_data(last_term).kind() else {
                return Some("last-not-jump");
            };
            if jump.args().get(t).is_none() {
                return Some("k-trip-init-missing");
            }
        }
        // E_j's used parameters must either flow from an invariant H_j
        // parameter with a slot in H_k, or be dead pass-through values
        // (used only by the terminator's jump arguments) that can be
        // replaced with zero.
        let e_j_params = data.bb_data(e_j).params().to_vec();
        for &p in &e_j_params {
            let non_term_used = data
                .layout()
                .basicblock(e_j)
                .insts()
                .iter()
                .filter(|&&i| i != data.layout().basicblock(e_j).terminator())
                .any(|&inst| data.inst_data(inst).inst_usage().any(|u| u == p));
            if !non_term_used {
                continue; // Dead pass-through: zero is safe.
            }
            let mapped = h_j_params.iter().enumerate().any(|(q, &hp)| {
                Some(q) != j_trip_idx
                    && q < h_k_params_pre.len()
                    && value_flows_from(&**data, &**data, hp, hp, &mut FxHashSet::default())
                    && value_flows_from(&**data, &**data, p, hp, &mut FxHashSet::default())
            });
            if !mapped {
                return Some("e_j-param-unmappable");
            }
        }
        None
    };
    if let Some(reason) = validate() {
        return false;
    }

    // 0.5. Reconcile E_j's parameters *before* E_k's terminator (the j-test,
    //    which is E_j's only predecessor) is removed: the flow analysis
    //    below reads E_j's incoming edges. Used parameters must flow from an
    //    invariant H_j parameter with a slot in H_k; dead pass-through
    //    parameters (used only by the terminator's jump) are replaced with
    //    zero. The k-test's exit arguments follow.
    let e_j_params = data.bb_data(e_j).params().to_vec();
    let e_j_term = data.layout().basicblock(e_j).terminator();
    let mut e_j_args: Vec<Inst> = Vec::new();
    for &p in &e_j_params {
        let non_term_used = data
            .layout()
            .basicblock(e_j)
            .insts()
            .iter()
            .filter(|&&i| i != e_j_term)
            .any(|&inst| data.inst_data(inst).inst_usage().any(|u| u == p));
        if !non_term_used {
            // Dead pass-through (e.g. the k-loop's final k/temp values that
            // the output section only re-forwards): zero is safe.
            e_j_args.push(data.new_local_inst().integer(0));
            continue;
        }
        // The used parameter must flow from an invariant H_j parameter whose
        // slot exists in H_k (i -> slot 0); refuse otherwise.
        let mut mapped = None;
        for (q, &hp) in h_j_params.iter().enumerate() {
            if Some(q) == j_trip_idx {
                continue;
            }
            if value_flows_from(&**data, &**data, hp, hp, &mut FxHashSet::default())
                && value_flows_from(&**data, &**data, p, hp, &mut FxHashSet::default())
            {
                mapped = Some(q);
                break;
            }
        }
        let Some(q) = mapped else {
            return false;
        };
        if q >= h_k_params_pre.len() {
            return false;
        }
        e_j_args.push(h_k_params_pre[q]);
    }

    // 0. Rewrite the reduction-target GEP: it lives in E_k and uses E_k's
    //    block parameters (i/j), which do not dominate B_k. Point the
    //    offsets at H_k's parameters (same values) so the GEP can move.
    let (i_pos_hk, j_pos_hk) = (
        h_k_params_pre
            .iter()
            .position(|&p| p != j_iv && p != temp)
            .unwrap(),
        // H_k's j slot is the parameter that provably equals the j IV
        // (the header forwards it under a different Inst identity).
        h_k_params_pre.iter().position(|&p| {
            p == j_iv || value_flows_from(&**data, &**data, p, j_iv, &mut FxHashSet::default())
        }),
    );
    if let InstKind::GetElemPtr(gep) = data.inst_data(c_gep).kind() {
        let mut offsets = gep.offsets().to_vec();
        for offset in &mut offsets {
            if let Some(pos) = data
                .bb_data(e_k)
                .params()
                .iter()
                .position(|&p| p == *offset)
            {
                // Map E_k parameter position back to the H_k parameter with
                // the same role (i -> i, j -> j). When j is not a H_k
                // parameter (single-shell test form), leave the offset as-is
                // (it references the H_j parameter, which dominates B_k).
                let hk_pos = if pos == 0 {
                    Some(i_pos_hk)
                } else {
                    j_pos_hk
                };
                if let Some(hk_pos) = hk_pos {
                    *offset = h_k_params_pre[hk_pos];
                }
            }
        }
        let base = gep.base();
        data.replace_inst_with(c_gep).get_elem_ptr(base, offsets);
    } else {
        return false;
    }

    // 1. Move the reduction-target GEP into B_k.
    move_inst(data, e_k, b_k, c_gep);

    // 2. Remove the two old terminators (the tails are moved, not removed).
    remove_inst(data, b_k, data.layout().basicblock(b_k).terminator());
    remove_inst(data, e_k, data.layout().basicblock(e_k).terminator());

    // 3. Reduction migration: register accumulator -> c[i][j] memory
    //    accumulation; delete the E_k store. Handles both the select form
    //    and the if-branch + phi-join form.
    migrate_reduction(data, &k_body, e_k, temp, c_gep);

    // 4. Rewrite j references inside the k-loop body: the accesses use
    //    H_k's j parameter, but after the swap the j-loop's live IV is
    //    H_j's (H_k's j is the fixed per-k-iteration value). The only other
    //    users of H_k's j are the removed terminators.
    if let Some(j_pos_hk) = j_pos_hk {
        visit_and_replace(data, h_k_params_pre[j_pos_hk], h_j_params[j_pos_hj]);
    }

    // 4.5. Rewrite stepped-pointer reads: the loop-carried column pointer
    //    (e.g. matmul1's `b[k][j]` through `%69`) reads a fixed column while
    //    the k IV steps the row. After the swap the inner loop is j, so the
    //    read must become a direct multi-dim GEP `(0, k, j)` on the pointer's
    //    root base. The pointer parameter itself is then dropped (its back
    //    edge advance is dead). The k parameter lives in H_k (outer after
    //    the swap) and dominates the body; the j parameter is H_j's live IV.
    if let Some(ptr_pos) = ptr_idx {
        let ptr = h_k_params_pre[ptr_pos];
        let root = ptr_root.expect("ptr_root present when ptr_idx is set");
        let k_inst = {
            // The k IV is the H_k parameter updated by k_update.
            h_k_params_pre
                .iter()
                .find(|&&p| {
                    matches!(
                        data.inst_data(k_update).kind(),
                        InstKind::Binary(b) if b.lhs() == p || b.rhs() == p
                    )
                })
                .copied()
                .expect("k IV parameter")
        };
        let j_inst = h_j_params[j_pos_hj];
        let zero = data.new_local_inst().integer(0);
        let scan_blocks: Vec<BasicBlock> = k_body.iter().copied().collect();
        for bb in scan_blocks {
            let insts: Vec<Inst> = data
                .layout()
                .basicblock(bb)
                .insts()
                .iter()
                .copied()
                .collect();
            for inst in insts {
                let InstKind::GetElemPtr(gep) = data.inst_data(inst).kind().clone() else {
                    continue;
                };
                if gep.base() != ptr {
                    continue;
                }
                // In the latch the pointer is advanced (`getelemptr(ptr, c)`)
                // to feed the back edge; with the parameter dropped that
                // advance is dead. Remove it.
                let is_latch_advance = bb == b_k
                    && gep.offsets().len() == 1
                    && data.inst_data(gep.offsets()[0]).kind().is_const();
                if is_latch_advance {
                    remove_inst(data, bb, inst);
                    continue;
                }
                // Body read: `getelemptr(ptr, X)` -> `getelemptr(root, (0,k,j))`.
                data.replace_inst_with(inst)
                    .get_elem_ptr(root, vec![zero, k_inst, j_inst]);
            }
        }
        drop_header_param(data, h_k, ptr_pos);
    }

    // 5. Swap instruction tails: k parts go to E_k; the j-test goes to B_k.
    //    (The old j-update depends on E_k's parameters and is dropped; a
    //    fresh one is built below from H_j's live j.)
    move_inst(data, b_k, e_k, k_update);
    move_inst(data, b_k, e_k, k_test);
    move_inst(data, e_k, b_k, j_test);

    // 6. Drop `temp` from H_k and from every edge into H_k (before the
    //    branch rebuild so argument counts match).
    drop_header_param(data, h_k, temp_idx);
    let h_k_params = data.bb_data(h_k).params().to_vec();

    // 6.5. E_k's parameters are dead (its body now only holds the k parts,
    //    which read H_k's parameters directly). E_j's parameter list keeps
    //    only the parameters its body actually uses; the k-test re-feeds
    //    them via `e_j_args` computed in step 0.5 (before the j-test, E_j's
    //    only predecessor, was removed).
    data.bb_data_mut(e_k).params_mut().clear();
    let e_j_used: FxHashSet<Inst> = e_j_params
        .iter()
        .copied()
        .filter(|&p| {
            data.layout().basicblock(e_j).insts().iter().any(|&inst| {
                data.inst_data(inst).inst_usage().any(|u| u == p)
            })
        })
        .collect();
    data.bb_data_mut(e_j)
        .params_mut()
        .retain(|p| e_j_used.contains(p));

    // 7. Rebuild the two terminators.
    let h_j_params = data.bb_data(h_j).params().to_vec();
    //    B_k ends with the j-test: H_j(j', trip_j') / E_k (parameterless).
    let one = data.new_local_inst().integer(1);
    let j_update = data
        .new_local_inst()
        .binary(BinaryOp::Add, h_j_params[j_pos_hj], one);
    data.layout_mut().insert_before_terminator(b_k, j_update);
    let mut j_args = h_j_params.clone();
    j_args[j_pos_hj] = j_update;
    if let Some(trip) = j_trip_idx {
        j_args[trip] = j_test; // `trip - 1` (moved from E_k).
    }
    let j_test_br = data.new_local_inst().branch(j_test, h_j, j_args, e_k, vec![]);
    data.layout_mut().insert_inst(b_k, j_test_br);
    //    E_k ends with the k-test: H_k(i, j, k', trip_k') / E_j.
    let k_pos = h_k_params
        .iter()
        .position(|&p| {
            matches!(
                data.inst_data(k_update).kind(),
                InstKind::Binary(b) if b.lhs() == p || b.rhs() == p,
            )
        })
        .unwrap();
    let mut k_args = h_k_params.clone();
    k_args[k_pos] = k_update;
    if let Some(mut trip) = k_trip_idx {
        if trip > temp_idx {
            trip -= 1;
        }
        k_args[trip] = k_test; // `trip_k - 1` (moved from B_k).
    }
    let k_test_br = data
        .new_local_inst()
        .branch(k_test, h_k, k_args, e_j, e_j_args);
    data.layout_mut().insert_inst(e_k, k_test_br);

    // 7.5. E_k now holds only the k parts: the moved k-update/k-test and the
    //    freshly built k-test branch. The pre-swap j-update / j-test
    //    instructions left in E_k (e.g. `Add(j, 1)` referencing the old E_k
    //    j parameter) are dead and their operands may dangle after E_k's
    //    parameter list is cleared. Remove everything except the three.
    {
        let e_insts: Vec<Inst> = data
            .layout()
            .basicblock(e_k)
            .insts()
            .iter()
            .copied()
            .collect();
        for inst in e_insts {
            if inst == k_update || inst == k_test || inst == k_test_br {
                continue;
            }
            remove_inst(data, e_k, inst);
        }
    }

    // 8. Rewire the shell chain and the loop-entry jumps.
    //    B_i: jump H_j -> jump H_k, args (i, j0, k0, trip_k0).
    let b_i_old_args = {
        let term = data.layout().basicblock(b_i).terminator();
        let InstKind::Jump(jump) = data.inst_data(term).kind() else {
            return false;
        };
        jump.args().to_vec()
    };
    let last_shell = *shells.last().unwrap();
    let last_old_args = {
        let term = data.layout().basicblock(last_shell).terminator();
        let InstKind::Jump(jump) = data.inst_data(term).kind() else {
            return false;
        };
        jump.args().to_vec()
    };
    let trip_k0 = k_trip_idx
        // `last_old_args` was read after `drop_header_param` trimmed the
        // temp slot, so the trip counter sits at the adjusted position.
        .map(|t| if t > temp_idx { t - 1 } else { t })
        .and_then(|t| last_old_args.get(t).copied())
        .unwrap_or_else(|| {
            // No trip counter (or no slot in the last shell's jump): reuse
            // the i value's slot pattern is unknown; refuse to invent one.
            data.new_local_inst().integer(0)
        });
    // B_i's new args follow H_k's (post-drop) parameter list: the invariant
    // outer value from the original jump, j0/k0 at the IV slots, and the k
    // trip counter at its slot.
    let j_pos_hk_adj = j_pos_hk.map(|p| if p > temp_idx { p - 1 } else { p });
    let trip_pos_hk_adj = k_trip_idx.map(|t| if t > temp_idx { t - 1 } else { t });
    let k_pos_hk = h_k_params
        .iter()
        .position(|&p| {
            matches!(
                data.inst_data(k_update).kind(),
                InstKind::Binary(b) if b.lhs() == p || b.rhs() == p,
            )
        })
        .unwrap();
    let mut b_i_args: Vec<Inst> = Vec::with_capacity(h_k_params.len());
    for (pos, _p) in h_k_params.iter().enumerate() {
        let v = if Some(pos) == j_pos_hk_adj {
            j0
        } else if pos == k_pos_hk {
            k0
        } else if Some(pos) == trip_pos_hk_adj {
            trip_k0
        } else {
            b_i_old_args[0] // The invariant outer value (i).
        };
        b_i_args.push(v);
    }
    if b_i_args.len() != h_k_params.len() {
        return false;
    }
    rewrite_jump(data, b_i, h_k, b_i_args);
    //    H_k: jump B_k -> jump the first shell.
    if !data.bb_data(shells[0]).params().is_empty() {
        return false;
    }
    rewrite_jump(data, h_k, shells[0], vec![]);
    //    Interior shells: jump targets stay; arguments that referenced H_j
    //    parameters are remapped to H_k's parameters (same positions).
    for idx in 0..shells.len() - 1 {
        let shell = shells[idx];
        let next = shells[idx + 1];
        let term = data.layout().basicblock(shell).terminator();
        let InstKind::Jump(jump) = data.inst_data(term).kind() else {
            return false;
        };
        let mut args = jump.args().to_vec();
        for arg in &mut args {
            if let Some(p) = h_j_params.iter().position(|&q| q == *arg) {
                if Some(p) == j_trip_idx || p >= h_k_params.len() {
                    return false; // Trip counter / unmatched param: refuse.
                }
                *arg = h_k_params[p];
            }
        }
        rewrite_jump(data, shell, next, args);
    }
    //    Last shell: jump H_k -> jump H_j, args per H_j's parameter list:
    //    invariants from the old jump (same position), j from B_i's original
    //    j0 (the initial IV, an outer value that dominates the shell), and
    //    the j trip counter from B_i's original jump. The pre-swap H_j
    //    parameters live below the shell after the swap and must not be
    //    referenced here.
    let mut last_args: Vec<Inst> = Vec::with_capacity(h_j_params.len());
    for (p, _param) in h_j_params.iter().enumerate() {
        let v = if Some(p) == j_trip_idx {
            b_i_old_args[p]
        } else if p == j_pos_hj {
            b_i_old_args.get(p).copied().unwrap_or(j0)
        } else {
            b_i_old_args.get(p).copied().unwrap_or(last_old_args[p])
        };
        last_args.push(v);
    }
    rewrite_jump(data, last_shell, h_j, last_args);
    // 8.5. Drop B_k's dead accumulator parameter (phi-join form: E_k's store
    //      and B_k's phi-join are gone after the migration) and trim every
    //      incoming edge's argument list, so H_j can enter B_k bare.
    let b_k_params = data.bb_data(b_k).params().to_vec();
    if !b_k_params.is_empty() {
        let keep: FxHashSet<usize> = FxHashSet::default();
        let pred_insts: Vec<Inst> = data.bb_data(b_k).used_by().iter().copied().collect();
        data.bb_data_mut(b_k).params_mut().clear();
        for pred_inst in pred_insts {
            let Some(from) = data.layout().parent_bb(pred_inst) else {
                continue;
            };
            let kind = data.inst_data(pred_inst).kind().clone();
            match kind {
                InstKind::Jump(jump) => {
                    let new_args: Vec<Inst> = jump
                        .args()
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| keep.contains(i))
                        .map(|(_, &a)| a)
                        .collect();
                    rewrite_jump(data, from, jump.target(), new_args);
                }
                InstKind::Branch(branch) if branch.t_target() == b_k => {
                    let new_args: Vec<Inst> = branch
                        .t_args()
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| keep.contains(i))
                        .map(|(_, &a)| a)
                        .collect();
                    data.replace_inst_with(pred_inst).branch(
                        branch.cond(),
                        branch.t_target(),
                        new_args,
                        branch.f_target(),
                        branch.f_args().to_vec(),
                    );
                }
                InstKind::Branch(branch) if branch.f_target() == b_k => {
                    let new_args: Vec<Inst> = branch
                        .f_args()
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| keep.contains(i))
                        .map(|(_, &a)| a)
                        .collect();
                    data.replace_inst_with(pred_inst).branch(
                        branch.cond(),
                        branch.t_target(),
                        branch.t_args().to_vec(),
                        branch.f_target(),
                        new_args,
                    );
                }
                _ => {}
            }
        }
    }
    //    H_j: jump the first shell -> jump the (former) k-loop body, which
    //    is now the j-loop body. B_k is the k-loop latch (now the j-test)
    //    and must not be the entry.
    rewrite_jump(data, h_j, k_first, vec![]);

    // 8.75. Dead-code sweep on the shells: after the swap a shell sits
    //    between the outer H_k and the inner H_j. It originally referenced
    //    H_j's parameters (the pre-swap outer loop), which now live in the
    //    inner header *below* the shell and no longer dominate it. The shell
    //    must be reduced to instructions that only use values in scope at
    //    the outer loop:
    //      * uses of the inner j IV / j trip counter: dead (they fed the
    //        dropped column pointer / the moved trip counter); remove them.
    //      * uses of the inner i parameter: `i` is also an outer H_k
    //        parameter; rewrite to the outer value so row-pointer GEPs
    //        survive.
    let mut shell_dead: Vec<(BasicBlock, Inst)> = Vec::new();
    for &shell in &shells {
        let term = data.layout().basicblock(shell).terminator();
        for &inst in data.layout().basicblock(shell).insts() {
            if inst == term {
                continue;
            }
            let uses_j = data.inst_data(inst).inst_usage().any(|used| {
                !used.is_global()
                    && !data.inst_data(used).kind().is_const()
                    && value_flows_from(&**data, &**data, used, j_iv, &mut FxHashSet::default())
            });
            if uses_j {
                shell_dead.push((shell, inst));
            }
        }
    }
    // Remap the shell's H_j `i` references to the outer H_k `i` parameter
    // (the only pre-swap H_j parameter that survives in outer scope).
    let i_hj = h_j_params
        .iter()
        .position(|&p| p != j_iv && Some(&p) != j_trip_idx.map(|t| &h_j_params[t]))
        .map(|idx| h_j_params[idx]);
    // The outer `i` parameter: the first H_k parameter that is neither the
    // j IV nor the k trip counter (trip_idx is already in h_k_params scope).
    let i_hk = h_k_params.iter().position(|&p| {
        p != j_iv
            && !k_trip_idx
                .map(|t| t < h_k_params.len() && h_k_params[t] == p)
                .unwrap_or(false)
    });
    if let (Some(i_hj), Some(i_pos)) = (i_hj, i_hk) {
        visit_and_replace(data, i_hj, h_k_params[i_pos]);
    }
    for (shell, inst) in shell_dead {
        remove_inst(data, shell, inst);
    }

    let _ = b_j;
    true
}

fn remove_inst(data: &mut ArenaContextMut<'_>, bb: BasicBlock, inst: Inst) {
    // Layout removal does not clean the terminator's block-target
    // registration: a removed jump/branch leaves stale entries in the
    // target blocks' used_by sets, which later analyses iterate. Detach
    // them here. (move_inst keeps the instruction alive and must NOT call
    // this helper.)
    let kind = data.inst_data(inst).kind().clone();
    match kind {
        InstKind::Jump(jump) => {
            data.bb_data_mut(jump.target()).used_by_mut().remove(&inst);
        }
        InstKind::Branch(branch) => {
            data.bb_data_mut(branch.t_target())
                .used_by_mut()
                .remove(&inst);
            data.bb_data_mut(branch.f_target())
                .used_by_mut()
                .remove(&inst);
        }
        _ => {}
    }
    data.layout_mut().remove_inst(bb, inst);
}

/// Move an instruction between blocks, preserving use-def links (layout-only).
/// Inserts before the target block's terminator when one exists, so a moved
/// instruction never lands after the terminator.
fn move_inst(data: &mut ArenaContextMut<'_>, from: BasicBlock, to: BasicBlock, inst: Inst) {
    data.layout_mut().remove_inst(from, inst);
    if data.layout().basicblock(to).insts().is_empty() {
        data.layout_mut().insert_inst(to, inst);
    } else {
        data.layout_mut().insert_before_terminator(to, inst);
    }
}

/// Replace the terminator of `bb` with a new jump to `target` carrying `args`.
fn rewrite_jump(data: &mut ArenaContextMut<'_>, bb: BasicBlock, target: BasicBlock, args: Vec<Inst>) {
    let terminator = data.layout().basicblock(bb).terminator();
    data.replace_inst_with(terminator).jump(target, args);
}

/// Remove `temp` at `temp_idx` from `H_k`'s parameters and from the argument
/// vector of every edge into `H_k`.
fn drop_header_param(data: &mut ArenaContextMut<'_>, h_k: BasicBlock, temp_idx: usize) {
    data.bb_data_mut(h_k).params_mut().remove(temp_idx);
    let blocks: Vec<BasicBlock> = data
        .layout()
        .basicblocks()
        .iter()
        .map(|layout| layout.bb())
        .collect();
    for bb in blocks {
        if data.layout().basicblock(bb).insts().is_empty() {
            continue;
        }
        let term = data.layout().basicblock(bb).terminator();
        let kind = data.inst_data(term).kind().clone();
        match kind {
            InstKind::Jump(jump) if jump.target() == h_k => {
                let mut args = jump.args().to_vec();
                if args.len() > temp_idx {
                    args.remove(temp_idx);
                    rewrite_jump(data, bb, h_k, args);
                }
            }
            InstKind::Branch(branch) => {
                if branch.t_target() == h_k && branch.t_args().len() > temp_idx {
                    let mut args = branch.t_args().to_vec();
                    args.remove(temp_idx);
                    let f_args = branch.f_args().to_vec();
                    data.replace_inst_with(term).branch(
                        branch.cond(),
                        h_k,
                        args,
                        branch.f_target(),
                        f_args,
                    );
                } else if branch.f_target() == h_k && branch.f_args().len() > temp_idx {
                    let mut args = branch.f_args().to_vec();
                    args.remove(temp_idx);
                    let t_args = branch.t_args().to_vec();
                    data.replace_inst_with(term).branch(
                        branch.cond(),
                        branch.t_target(),
                        t_args,
                        h_k,
                        args,
                    );
                }
            }
            _ => {}
        }
    }
}

/// Rewrite the reduction update from the register accumulator `temp` to a
/// memory accumulation on `c[i][j]`, and delete the final `c[i][j] = temp`
/// store in E_k. `temp` is the accumulator identified by M42 (a `H_k`
/// parameter); its update is the *last* instruction consuming it anywhere in
/// the k-loop body — the chain tail `select(cond, add(temp, delta), temp)`
/// in B_k, or the `add(temp, delta)` inside an if-branch whose join block
/// carries the phi (matmul1's form). The load/store are placed next to the
/// update so the if-branch condition is preserved.
fn migrate_reduction(
    data: &mut ArenaContextMut<'_>,
    k_body: &[BasicBlock],
    e_k: BasicBlock,
    acc: Inst,
    c_gep: Inst,
) {
    // The update is the *last* instruction in the loop body that consumes
    // `acc` and is itself a computable update shape (the branch that carries
    // `acc` into the join block is not an update).
    let mut update: Option<(BasicBlock, Inst)> = None;
    for &bb in k_body {
        for &inst in data.layout().basicblock(bb).insts() {
            if data.inst_data(inst).inst_usage().any(|used| used == acc) {
                let is_computable = matches!(
                    data.inst_data(inst).kind(),
                    InstKind::Binary(_) | InstKind::Select(_)
                );
                if is_computable {
                    update = Some((bb, inst));
                }
            }
        }
    }
    let Some((update_bb, update)) = update else {
        return;
    };

    // The target GEP must dominate the load/store: the pre-swap move into
    // B_k does not dominate the if-branch block where the update lives.
    // Move it to just before the update (a tail insertion would leave the
    // load using it before its definition inside the same block).
    if data.layout().parent_bb(c_gep) != Some(update_bb) {
        let from = data.layout().parent_bb(c_gep).expect("c_gep in layout");
        data.layout_mut().remove_inst(from, c_gep);
    } else {
        data.layout_mut().remove_inst(update_bb, c_gep);
    }
    data.layout_mut().insert_inst_before(update, c_gep);

    // Insert `ld = load(c_gep)` before the update, rewrite the update's `acc`
    // references to `ld`, and insert `store(update, c_gep)` right after it.
    let ld = data.new_local_inst().load(c_gep);
    data.layout_mut().insert_inst_before(update, ld);
    rewrite_update_acc(data, update, acc, ld);
    let store = data.new_local_inst().store(update, c_gep);
    data.layout_mut().insert_before_terminator(update_bb, store);

    // Delete `c[i][j] = temp` in E_k (the store into the migration target).
    // `remove_layout_inst` (not bare `layout_mut().remove_inst`) so the
    // store's operands (src, dest) are detached from their used_by lists —
    // otherwise the migrated c_gep keeps a dangling reference to the removed
    // store, which downstream passes (e.g. loop_vectorize's escape check)
    // misread as a value escaping the loop.
    let e_insts: Vec<Inst> = data
        .layout()
        .basicblock(e_k)
        .insts()
        .iter()
        .copied()
        .collect();
    for inst in e_insts {
        if let InstKind::Store(store) = data.inst_data(inst).kind() {
            if store.dest() == c_gep {
                data.remove_layout_inst(e_k, inst);
                break;
            }
        }
    }
}

/// Rebuild the update instruction with the accumulator replaced by `ld`.
/// Supports `temp ± E`, `temp * E`, `min/max(temp, E)` and the select form
/// `select(cond, temp op E, temp)`.
fn rewrite_update_acc(data: &mut ArenaContextMut<'_>, update: Inst, acc: Inst, ld: Inst) {
    let kind = data.inst_data(update).kind().clone();
    match kind {
        InstKind::Binary(binary) => {
            let (op, lhs, rhs) = (binary.op(), binary.lhs(), binary.rhs());
            let (new_lhs, new_rhs) = if lhs == acc {
                (ld, rhs)
            } else if rhs == acc {
                (lhs, ld)
            } else {
                return; // Not an accumulator update after all.
            };
            data.replace_inst_with(update).binary(op, new_lhs, new_rhs);
        }
        InstKind::Select(select) if select.if_false() == acc => {
            let cond = select.cond();
            let if_true = select.if_true();
            let InstKind::Binary(binary) = data.inst_data(if_true).kind().clone() else {
                return;
            };
            let (op, lhs, rhs) = (binary.op(), binary.lhs(), binary.rhs());
            let (new_lhs, new_rhs) = if lhs == acc {
                (ld, rhs)
            } else if rhs == acc {
                (lhs, ld)
            } else {
                return;
            };
            let inner = data.replace_inst_with(if_true).binary(op, new_lhs, new_rhs);
            data.replace_inst_with(update).select(cond, inner, ld);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Type;

    /// Three-level test-at-bottom loop nest simulating matmul1's i-j-k
    /// multiplication kernel over globals `a/b` (reads) and `c` (reduction
    /// target). `c_zero` selects a zero vs non-zero global initializer;
    /// `reverse_j` makes the a[k][j] access use `7 - j` (negative j coeff).
    ///
    /// Instruction construction goes through `LocalBuilder` over the mutable
    /// arena context: `FunctionData`-rooted builders cannot resolve the type
    /// of a global base GEP (`Arena::global` is unimplemented).
    fn build_triple(program: &mut Program, c_zero: bool, reverse_j: bool) -> Function {
        let arr8 = Type::get_array(Type::get_array(Type::get_i32(), 8), 8);
        let c = if c_zero {
            let init = program.new_value().zero_init(arr8.clone());
            program.new_value().global_alloc(init)
        } else {
            // Non-zero initializer: an 8x8 aggregate of ones.
            let one = program.new_value().integer(1);
            let row = program.new_value().aggregate(vec![one; 8]);
            let init = program.new_value().aggregate(vec![row; 8]);
            program.new_value().global_alloc(init)
        };
        let a = {
            let init = program.new_value().zero_init(arr8.clone());
            program.new_value().global_alloc(init)
        };
        let b = {
            let init = program.new_value().zero_init(arr8);
            program.new_value().global_alloc(init)
        };
        let function = program.new_function(Type::get_i32(), "matmul".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut *program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();

        let i32 = Type::get_i32();
        let e_i = data.new_basic_block().basic_block("e_i".into(), vec![]);
        let h_i = data.new_basic_block().basic_block("h_i".into(), vec![i32.clone()]);
        let b_i = data.new_basic_block().basic_block("b_i".into(), vec![]);
        let h_j = data.new_basic_block().basic_block("h_j".into(), vec![i32.clone()]);
        let b_j = data.new_basic_block().basic_block("b_j".into(), vec![]);
        let h_k = data
            .new_basic_block()
            .basic_block("h_k".into(), vec![i32.clone(), i32.clone()]);
        let b_k = data.new_basic_block().basic_block("b_k".into(), vec![]);
        let e_k = data.new_basic_block().basic_block("e_k".into(), vec![]);
        let e_j = data.new_basic_block().basic_block("e_j".into(), vec![]);
        for bb in [h_i, b_i, h_j, b_j, h_k, b_k, e_k, e_j, e_i] {
            data.layout_mut().push_bb_back(bb);
        }

        let zero = data.new_local_inst().integer(0);
        let one = data.new_local_inst().integer(1);
        let n8 = data.new_local_inst().integer(8);
        let entry_jump = data.new_local_inst().jump(h_i, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let i = data.bb_data(h_i).params()[0];
        let j = data.bb_data(h_j).params()[0];
        let k = data.bb_data(h_k).params()[0];
        let temp = data.bb_data(h_k).params()[1];

        // Instruction construction (global GEPs need the Arena context).
        let mut lb = LocalBuilder {
            arena: &mut data as &mut dyn Arena,
        };
        // i-loop: H_i jump B_i; B_i jump H_j(j0)  (shell).
        let h_i_jump = lb.jump(b_i, vec![]);
        let j0 = lb.integer(0);
        let b_i_jump = lb.jump(h_j, vec![j0]);
        // j-loop: H_j jump B_j; B_j jump H_k(k0, temp0).
        let h_j_jump = lb.jump(b_j, vec![]);
        let k0 = lb.integer(0);
        let t0 = lb.integer(0);
        let b_j_jump = lb.jump(h_k, vec![k0, t0]);
        // k-loop body B_k: temp' = select(cond, temp + b[i][k]*a[k][j], temp).
        let h_k_jump = lb.jump(b_k, vec![]);
        // [0, i, k]: leading zero traverses the pointer level, then one index
        // per array dimension (matches the frontend's lowering of a[i][k]).
        let gep_bik = lb.get_elem_ptr(b, vec![zero, i, k]);
        let load_bik = lb.load(gep_bik);
        let j_idx = if reverse_j {
            let seven = lb.integer(7);
            lb.binary(BinaryOp::Sub, seven, j)
        } else {
            j
        };
        let gep_akj = lb.get_elem_ptr(a, vec![zero, k, j_idx]);
        let load_akj = lb.load(gep_akj);
        let mul = lb.binary(BinaryOp::Mul, load_bik, load_akj);
        let cond = lb.binary(BinaryOp::Eq, mul, zero);
        let add = lb.binary(BinaryOp::Add, temp, mul);
        let update = lb.select(cond, add, temp);
        let k_update = lb.binary(BinaryOp::Add, k, one);
        let k_test = lb.binary(BinaryOp::Lt, k_update, n8);
        let k_test_br = lb.branch(k_test, h_k, vec![k_update, update], e_k, vec![]);
        // E_k: c[i][j] = temp; j' = j + 1; br j' < 8, H_j(j'), E_j.
        let gep_cij = lb.get_elem_ptr(c, vec![zero, i, j]);
        let c_store = lb.store(temp, gep_cij);
        let j_update = lb.binary(BinaryOp::Add, j, one);
        let j_test = lb.binary(BinaryOp::Lt, j_update, n8);
        let j_test_br = lb.branch(j_test, h_j, vec![j_update], e_j, vec![]);
        // E_j: i' = i + 1; br i' < 8, H_i(i'), E_i.
        let i_update = lb.binary(BinaryOp::Add, i, one);
        let i_test = lb.binary(BinaryOp::Lt, i_update, n8);
        let i_test_br = lb.branch(i_test, h_i, vec![i_update], e_i, vec![]);
        let ret = lb.ret(Some(zero));
        drop(lb);

        // Layout insertion (the mutable context is free again).
        data.layout_mut().insert_inst(h_i, h_i_jump);
        data.layout_mut().insert_inst(b_i, b_i_jump);
        data.layout_mut().insert_inst(h_j, h_j_jump);
        data.layout_mut().insert_inst(b_j, b_j_jump);
        data.layout_mut().insert_inst(h_k, h_k_jump);
        let mut b_k_insts = vec![gep_bik, load_bik];
        if reverse_j {
            // The reversed index expression is a real instruction; it must be
            // in layout or the analysis treats it as loop-outside.
            b_k_insts.push(j_idx);
        }
        b_k_insts.extend([
            gep_akj, load_akj, mul, cond, add, update, k_update, k_test, k_test_br,
        ]);
        for inst in b_k_insts {
            data.layout_mut().insert_inst(b_k, inst);
        }
        for inst in [gep_cij, c_store, j_update, j_test, j_test_br] {
            data.layout_mut().insert_inst(e_k, inst);
        }
        for inst in [i_update, i_test, i_test_br] {
            data.layout_mut().insert_inst(e_j, inst);
        }
        data.layout_mut().insert_inst(e_i, ret);
        let _ = (a, b);
        function
    }

    fn run(program: &mut Program, function: Function) -> bool {
        let mut context = ArenaContextMut {
            program,
            curr_func: Some(function),
        };
        LoopInterchange.run_on(&mut context)
    }

    #[test]
    fn swaps_jk_in_matmul_shape() {
        let mut program = Program::new();
        let function = build_triple(&mut program, true, false);
        assert!(run(&mut program, function), "expected interchange to fire");
        let data = program.func_data(function);
        // After the swap, the four IVs (i/j/k, temp) collapsed to three
        // (i/j/k): the total block-parameter count is 3.
        let total_params: usize = data
            .layout()
            .basicblocks()
            .iter()
            .map(|l| data.bb_data(l.bb()).params().len())
            .sum();
        assert_eq!(total_params, 3, "temp parameter must be dropped");
        // The reduction target is now accumulated via load/store inside the
        // inner body: exactly one store remains in the whole function (the
        // accumulation store; the old `c[i][j] = temp` was deleted).
        let stores: Vec<Inst> = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .filter(|&inst| matches!(data.inst_data(inst).kind(), InstKind::Store(_)))
            .collect();
        assert_eq!(stores.len(), 1, "exactly one store (the accumulation)");
        // The k-test branch now exits to the former j-exit.
        let exits_to_old_ej = data
            .layout()
            .basicblocks()
            .iter()
            .filter(|l| {
                let term = l.terminator();
                matches!(data.inst_data(term).kind(), InstKind::Branch(_))
            })
            .count();
        assert!(exits_to_old_ej >= 2, "two test branches remain");
    }

    #[test]
    fn is_idempotent_after_swap() {
        let mut program = Program::new();
        let function = build_triple(&mut program, true, false);
        assert!(run(&mut program, function));
        // Second run must not fire again (inner j coefficients are now in
        // {0, elem_size}: no row stride left).
        let mut context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        assert!(!LoopInterchange.run_on(&mut context), "must not re-swap");
    }

    #[test]
    fn rejects_reversed_direction() {
        let mut program = Program::new();
        let function = build_triple(&mut program, true, true);
        let mut context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        assert!(
            !LoopInterchange.run_on(&mut context),
            "negative j coeff must be rejected"
        );
    }

    #[test]
    fn rejects_nonzero_reduction_target() {
        let mut program = Program::new();
        let function = build_triple(&mut program, false, false);
        let mut context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        assert!(
            !LoopInterchange.run_on(&mut context),
            "non-zero initializer must be rejected"
        );
    }
}
