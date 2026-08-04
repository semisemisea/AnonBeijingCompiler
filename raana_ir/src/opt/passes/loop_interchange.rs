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
    utils::{gep::gep_index_stride, logical_edge::incoming_edges},
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
    /// The reduction accumulator (`H_k` parameter).
    temp: Inst,
    /// Address GEP of the reduction target `c[i][j]` (lives in `E_k`).
    c_gep: Inst,
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
    if looop.contains(outside_bb) || !outside_args.is_empty() {
        return false; // Single exit, no exit block parameters.
    }
    let params = data.bb_data(looop.header()).params();
    let Some(iv_idx) = params.iter().position(|&p| p == iv) else {
        return false;
    };
    if inside_args.len() != params.len() {
        return false;
    }
    let updated = inside_args[iv_idx];
    // Unit step: updated == iv ± 1.
    let unit = match arena.inst_data(updated).kind() {
        InstKind::Binary(b) if b.op() == BinaryOp::Add => {
            (b.lhs() == iv && integer_constant(arena, b.rhs()) == Some(1))
                || (b.rhs() == iv && integer_constant(arena, b.lhs()) == Some(1))
        }
        InstKind::Binary(b) if b.op() == BinaryOp::Sub => {
            b.lhs() == iv && integer_constant(arena, b.rhs()) == Some(1)
        }
        _ => false,
    };
    if !unit {
        return false;
    }
    // Constant bound on the updated value.
    let InstKind::Binary(compare) = arena.inst_data(branch.cond()).kind() else {
        return false;
    };
    if !arena.inst_data(compare.lhs()).ty().is_i32() || !arena.inst_data(compare.rhs()).ty().is_i32()
    {
        return false;
    }
    integer_constant(arena, compare.lhs())
        .or_else(|| integer_constant(arena, compare.rhs()))
        .is_some()
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

/// Exit block of a loop: the parameterless outside target of its latch branch.
fn exit_of_one(data: &FunctionData, arena: &impl Arena, looop: &Loop) -> Option<BasicBlock> {
    if looop.latches().len() != 1 {
        return None;
    }
    let latch = looop.latches()[0];
    let terminator = data.layout().basicblock(latch).terminator();
    let InstKind::Branch(branch) = arena.inst_data(terminator).kind() else {
        return None;
    };
    let (exit, args) = if branch.t_target() == looop.header() {
        (branch.f_target(), branch.f_args())
    } else if branch.f_target() == looop.header() {
        (branch.t_target(), branch.t_args())
    } else {
        return None;
    };
    if !args.is_empty() {
        return None;
    }
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
    let ranges = RangeAnalysis::new(&arena, &cfg, &loops, &induction);
    let effects = EffectAnalysis::new(program);
    let deps = DependenceAnalysis::new(program, func, &effects, 4);

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
        let Some(j_biv) = induction.for_loop(l_j).first() else {
            continue;
        };
        let k_iv = k_biv.parameter();
        let j_iv = j_biv.parameter();
        if !rotate_unit_step(data, &arena, l_k, k_iv) || !rotate_unit_step(data, &arena, l_j, j_iv)
        {
            continue;
        }

        // (c) Single latch; exits are parameterless and distinct.
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
        // {B_j shell, E_k} (the header H_j belongs to the body set).
        let h_j = l_j.header();
        let mut shells: SmallVec<[BasicBlock; 4]> = SmallVec::new();
        for &bb in l_j.body() {
            if l_k.contains(bb) || bb == e_k || bb == h_j {
                continue;
            }
            shells.push(bb);
        }
        if shells.len() != 1 {
            continue;
        }
        let b_j = shells[0];
        let h_k = l_k.header();
        // B_j must be a pure jump shell into H_k.
        let shell_term = data.layout().basicblock(b_j).terminator();
        let InstKind::Jump(shell_jump) = arena.inst_data(shell_term).kind() else {
            continue;
        };
        if shell_jump.target() != h_k {
            continue;
        }
        let shell_args = shell_jump.args().to_vec();
        // H_j must jump into B_j.
        let h_j_term = data.layout().basicblock(h_j).terminator();
        let InstKind::Jump(h_j_jump) = arena.inst_data(h_j_term).kind() else {
            continue;
        };
        if h_j_jump.target() != b_j {
            continue;
        }

        // (e) The k-loop must be a reduction (M42) — no calls, no other
        // carried dependencies.
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
            let Some((cj, ck)) = dual_coeffs(&arena, l_j, l_k, j_iv, k_iv, &ranges, addr) else {
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
        // Locate the store `c[i][j] = temp` inside E_k.
        let mut c_gep = None;
        for inst in data.layout().basicblock(e_k).insts() {
            if let InstKind::Store(store) = arena.inst_data(*inst).kind() {
                if store.src() == temp {
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
        // k0 is the k-loop initial value carried by B_j's jump.
        let k_idx = params.iter().position(|&p| p == k_iv)?;
        let k0 = *shell_args.get(k_idx)?;

        return Some(Plan {
            b_i,
            h_j,
            b_j,
            h_k,
            b_k,
            e_k,
            e_j,
            temp_idx,
            temp,
            c_gep,
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
        c_gep,
        k_update,
        k_test,
        j_update,
        j_test,
        j0,
        k0,
    } = plan;

    // 1. Move the reduction-target GEP into B_k (it currently lives in E_k).
    //    The GEP depends only on values dominating B_k (j-loop params etc.).
    move_inst(data, e_k, b_k, c_gep);

    // 2. Remove the two old terminators; the tail instructions (k parts in
    //    B_k, j parts in E_k) are moved by step 4, not removed here.
    remove_inst(data, b_k, data.layout().basicblock(b_k).terminator());
    remove_inst(data, e_k, data.layout().basicblock(e_k).terminator());

    // 3. Reduction migration in B_k: register accumulator -> memory
    //    accumulation, and delete the `c[i][j] = temp` store in E_k.
    migrate_reduction(data, b_k, e_k, temp, c_gep);

    // 4. Swap instruction tails: k parts go to E_k, j parts go to B_k.
    move_inst(data, b_k, e_k, k_update);
    move_inst(data, b_k, e_k, k_test);
    move_inst(data, e_k, b_k, j_update);
    move_inst(data, e_k, b_k, j_test);

    // 5. Drop the `temp` parameter from H_k and from every edge into H_k.
    //    This must precede the k-test rebuild below (the branch asserts the
    //    argument count matches H_k's parameters).
    drop_header_param(data, h_k, temp_idx);

    // 6. Rebuild the two terminators.
    //    B_k ends with the j-test: H_j(j') / E_k (targets unchanged).
    let j_test_br = data.new_local_inst().branch(j_test, h_j, vec![j_update], e_k, vec![]);
    data.layout_mut().insert_inst(b_k, j_test_br);
    //    E_k ends with the k-test: H_k(k') / E_j (exit retargeted from E_k).
    let k_test_br = data.new_local_inst().branch(k_test, h_k, vec![k_update], e_j, vec![]);
    data.layout_mut().insert_inst(e_k, k_test_br);

    // 7. Rewrite the four control-flow jumps.
    //    B_i: jump H_j(j0) -> jump H_k(k0).
    rewrite_jump(data, b_i, h_k, vec![k0]);
    //    B_j: jump H_k(k0, temp0) -> jump H_j(j0).
    rewrite_jump(data, b_j, h_j, vec![j0]);
    //    H_j: jump B_j -> jump B_k.
    rewrite_jump(data, h_j, b_k, vec![]);
    //    H_k: jump B_k -> jump B_j.
    rewrite_jump(data, h_k, b_j, vec![]);

    true
}

fn remove_inst(data: &mut ArenaContextMut<'_>, bb: BasicBlock, inst: Inst) {
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

/// Rewrite the reduction update in B_k from the register accumulator `temp`
/// to a memory accumulation on `c[i][j]`, and delete the final
/// `c[i][j] = temp` store in E_k. `temp` is the accumulator identified by
/// M42 (a `H_k` parameter); its update is the *last* instruction in B_k that
/// consumes it (the chain tail: `select(cond, add(temp, delta), temp)`).
fn migrate_reduction(
    data: &mut ArenaContextMut<'_>,
    b_k: BasicBlock,
    e_k: BasicBlock,
    acc: Inst,
    c_gep: Inst,
) {
    let b_insts: Vec<Inst> = data
        .layout()
        .basicblock(b_k)
        .insts()
        .iter()
        .copied()
        .collect();
    // The update is the *last* instruction in B_k that consumes `acc` (the
    // chain tail: `select(cond, add(acc, delta), acc)` — `add` consumes `acc`
    // first, `select` consumes it again via `if_false`; the select is the
    // carried value and must be the one rewritten).
    let mut update = None;
    for &inst in &b_insts {
        if data.inst_data(inst).inst_usage().any(|used| used == acc) {
            update = Some(inst);
        }
    }
    let Some(update) = update else {
        return;
    };

    // Insert `ld = load(c_gep)` before the update, rewrite the update's `acc`
    // references to `ld`, and append `store(update, c_gep)` (the old
    // terminator was already removed, so appending is safe).
    let ld = data.new_local_inst().load(c_gep);
    data.layout_mut().insert_inst_before(update, ld);
    rewrite_update_acc(data, update, acc, ld);
    let store = data.new_local_inst().store(update, c_gep);
    data.layout_mut().insert_inst(b_k, store);

    // Delete `c[i][j] = temp` in E_k (the store into the migration target).
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
                data.layout_mut().remove_inst(e_k, inst);
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
