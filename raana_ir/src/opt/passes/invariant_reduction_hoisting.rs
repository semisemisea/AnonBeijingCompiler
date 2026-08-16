//! Hoist a whole invariant reduction nest out of an outer trip loop.
//!
//! Recognizes an outer loop whose body is a *pure, invariant reduction nest*:
//!
//! ```text
//! L_out(r, ..., acc, ...):      br r < R, entry, exit
//! entry:                        jump i_header(...)
//! i_header(...):                ... nested loops that only read memory and
//!                               accumulate `acc = acc + D_i` ...
//! latch:                        r = r + 1; jump L_out(..., acc, r, ...)
//! exit:                         ... uses acc ...
//! ```
//!
//! The nest is invariant with respect to `r`: it contains no stores / calls and
//! does not read `r`, and `acc` is only used additively (`acc ± E`) inside it.
//! Therefore running the nest once with `acc = 0` yields the per-iteration
//! increment `D_total`, and the whole nest can be *degraded* to a single
//! `acc = acc + D_total` per outer iteration.
//!
//! `many_mat_cal-1/2/3` are the only corpus programs whose outer loop has this
//! shape: the `R`-trip loop re-computes the `T×T` square-sum every iteration
//! (≈ 1.5×10¹⁰ element ops); after degradation it runs once (≈ 10⁶) plus `R`
//! trivial adds. This mirrors what gcc does for the same benchmark.
//!
//! Soundness:
//! - The nest is pure (no store / call / memzero) and reads only loop-invariant
//!   memory, so cloning it to run once duplicates no side effects.
//! - `acc` is only accumulated additively inside the nest, so
//!   `nest(acc) = acc + D` for a constant `D` independent of `acc`; `R`
//!   successive applications equal `acc_init + D_total * R` (mod 2³²).
//! - The nest never reads the outer trip counter `r`, so `D` does not depend on
//!   the iteration.
//!
//! This is a structure-only optimization (per `docs/Illegal_optimization.md`
//! rule two; see TODO.md §2.6): it never matches names, strings, or
//! benchmark-specific bounds.

use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;

use crate::ir::remap::EntityMapper;
use crate::opt::{
    analysis_passes::loop_analysis::{Loop, LoopAnalysis},
    prelude::*,
    utils::{
        cfg::CFG,
        preheader::{EnsurePreheader, ensure_preheader},
    },
};

pub struct InvariantReductionHoisting;

/// Everything needed to hoist one outer reduction nest.
struct OuterCandidate {
    header: BasicBlock,
    exit: BasicBlock,
    latch: BasicBlock,
    entry: BasicBlock,
    r_idx: usize,
    acc_idx: usize,
    /// The value the nest produces for the accumulator slot (latch back edge).
    final_acc: Inst,
    /// The latch's back-edge arguments to the header.
    orig_back_args: Vec<Inst>,
    /// `L_out.body` minus the header: the invariant nest blocks.
    region: FxHashSet<BasicBlock>,
}

impl InvariantReductionHoisting {
    fn find_outer(data: &ArenaContextMut<'_>, looop: &Loop) -> Option<OuterCandidate> {
        let header = looop.header();
        let params = data.bb_data(header).params().to_vec();

        // (1) The header branches into the nest and out to the exit.
        let terminator = data.layout().basicblock(header).terminator();
        let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
            return None;
        };
        let (entry, exit) =
            if looop.contains(branch.t_target()) && !looop.contains(branch.f_target()) {
                (branch.t_target(), branch.f_target())
            } else if !looop.contains(branch.t_target()) && looop.contains(branch.f_target()) {
                (branch.f_target(), branch.t_target())
            } else {
                return None;
            };
        // The nest entry has no parameters: the clone starts with an empty jump.
        if !data.bb_data(entry).params().is_empty() {
            return None;
        }

        // (2) The header's exit test compares a header parameter (the trip
        // counter `r`) against a bound. The general BIV analysis cannot see `r`
        // here because the counter is threaded through nested loop headers
        // before its latch update; the structural test is what matters: the
        // nest must not depend on `r` (checked by `trace_invariant`).
        let InstKind::Binary(compare) = data.inst_data(branch.cond()).kind() else {
            return None;
        };
        let r = match (compare.op(), compare.lhs(), compare.rhs()) {
            (BinaryOp::Lt, lhs, _) if params.contains(&lhs) => lhs,
            (BinaryOp::Gt, _, rhs) if params.contains(&rhs) => rhs,
            _ => return None,
        };
        let r_idx = params.iter().position(|&p| p == r)?;

        // (3) A single latch and a non-trivial nest.
        if looop.latches().len() != 1 {
            return None;
        }
        let latch = looop.latches()[0];
        let region = looop
            .body()
            .iter()
            .copied()
            .filter(|&block| block != header)
            .collect::<FxHashSet<_>>();
        if region.len() < 2 || !region.contains(&latch) {
            return None;
        }

        // (4) The nest is pure and references no blocks outside the nest except
        // the header via the latch's back edge. Invariance with respect to the
        // trip counter is checked separately by `trace_invariant`.
        for &block in &region {
            for &inst in data.layout().basicblock(block).insts() {
                let kind = data.inst_data(inst).kind();
                if matches!(
                    kind,
                    InstKind::Store(..)
                        | InstKind::Call(..)
                        | InstKind::TailCall(..)
                        | InstKind::MemZero(..)
                        | InstKind::GlobalAlloc(..)
                ) {
                    return None;
                }
                for target in data.inst_data(inst).bb_usage() {
                    if target != header && !region.contains(&target) {
                        return None;
                    }
                }
            }
        }
        // The nest must not depend on the trip counter: `r` may only be
        // threaded through nested headers and updated in the latch, never used
        // in a computation that contributes to `D`.
        if !trace_invariant(data, &region, header, latch, r) {
            return None;
        }

        // (5) The latch jumps back to the header.
        let orig_back_args = match data
            .inst_data(data.layout().basicblock(latch).terminator())
            .kind()
        {
            InstKind::Jump(jump) if jump.target() == header => jump.args().to_vec(),
            _ => return None,
        };
        if orig_back_args.len() != params.len() {
            return None;
        }

        // (6) Find the single accumulator: a non-trip header parameter that is
        // accumulated additively through the nest, ending at the latch's back
        // edge for its own slot.
        let mut acc_candidate = None;
        for (idx, &p) in params.iter().enumerate() {
            if idx == r_idx || !data.inst_data(p).ty().is_i32() {
                continue;
            }
            let Some(final_acc) = trace_accumulator(data, &region, header, exit, p) else {
                continue;
            };
            if orig_back_args[idx] == final_acc
                && acc_candidate.replace((p, idx, final_acc)).is_some()
            {
                return None;
            }
        }
        let (_, acc_idx, final_acc) = match acc_candidate {
            Some(value) => value,
            None => {
                eprintln!(
                    "IRH: header {} rejected: no additive accumulator",
                    data.bb_data(header).name()
                );
                return None;
            }
        };

        // (7) The nest must not reference header-local computed values (only
        // header parameters, which the clone substitutes with their entering
        // values; a header-local value would not dominate the clone).
        let param_set = params.iter().copied().collect::<FxHashSet<_>>();
        for &block in &region {
            for &inst in data.layout().basicblock(block).insts() {
                if data.inst_data(inst).kind().is_terminator() {
                    continue;
                }
                for used in data.inst_data(inst).inst_usage() {
                    if used.is_global() || data.inst_data(used).kind().is_const() {
                        continue;
                    }
                    if data.layout().parent_bb(used) == Some(header) && !param_set.contains(&used) {
                        return None;
                    }
                }
            }
        }

        Some(OuterCandidate {
            header,
            exit,
            latch,
            entry,
            r_idx,
            acc_idx,
            final_acc,
            orig_back_args,
            region,
        })
    }

    /// Rewrite one outer loop. Returns true when the function changed.
    fn apply(
        data: &mut ArenaContextMut<'_>,
        cfg: &CFG,
        looop: &Loop,
        cand: &OuterCandidate,
    ) -> bool {
        let Some(preheader) = ensure_preheader(data, cfg, looop) else {
            return false;
        };
        let preheader = match preheader {
            EnsurePreheader::Existing(preheader) => preheader,
            EnsurePreheader::Created(..) => return true,
        };
        let InstKind::Jump(jump) = data
            .inst_data(data.layout().basicblock(preheader).terminator())
            .kind()
        else {
            return false;
        };
        let orig_args = jump.args().to_vec();
        let params = data.bb_data(cand.header).params().to_vec();
        if orig_args.len() != params.len() {
            return false;
        }

        // Seed the nest with `acc = 0`; the trip counter and every other header
        // parameter take their entering values (which dominate the clone).
        let zero = data.new_local_value().integer(0);
        let mut header_param_map = FxHashMap::default();
        for (i, &p) in params.iter().enumerate() {
            let replacement = if i == cand.acc_idx || i == cand.r_idx {
                zero
            } else {
                orig_args[i]
            };
            header_param_map.insert(p, replacement);
        }

        // Fresh blocks for the cloned nest, in original layout order.
        let region_order = data
            .layout()
            .basicblocks()
            .iter()
            .map(|layout| layout.bb())
            .filter(|&block| cand.region.contains(&block))
            .collect::<Vec<_>>();
        let i32_ty = Type::get_i32();
        let compute_done = data
            .new_basic_block()
            .basic_block("irh_done".into(), vec![i32_ty.clone()]);
        let mut block_map = FxHashMap::default();
        let mut value_map = FxHashMap::default();
        let mut anchor = preheader;
        for &block in &region_order {
            let param_types = data
                .bb_data(block)
                .params()
                .iter()
                .map(|&p| data.inst_data(p).ty().clone())
                .collect::<Vec<_>>();
            let name = format!("irh_{}", data.bb_data(block).name());
            let cloned = data.new_basic_block().basic_block(name, param_types);
            data.layout_mut().insert_bb_after(anchor, cloned);
            anchor = cloned;
            block_map.insert(block, cloned);
            let cloned_params = data.bb_data(cloned).params().to_vec();
            for (&src, &dst) in data.bb_data(block).params().iter().zip(&cloned_params) {
                value_map.insert(src, dst);
            }
        }
        data.layout_mut().insert_bb_after(anchor, compute_done);

        // Clone the nest's instructions and rewire the latch to `compute_done`.
        let mut mapper = RegionMapper {
            data: &mut *data,
            region: &cand.region,
            block_map: &block_map,
            value_map: &mut value_map,
            header_param_map: &header_param_map,
        };
        let latch_clone = block_map[&cand.latch];
        for &block in &region_order {
            let is_latch = block == cand.latch;
            let insts = mapper
                .data
                .layout()
                .basicblock(block)
                .insts()
                .iter()
                .copied()
                .collect::<Vec<_>>();
            for (index, &inst) in insts.iter().enumerate() {
                let is_terminator = index + 1 == insts.len();
                if is_latch && is_terminator {
                    continue;
                }
                mapper.clone_inst(inst);
            }
        }
        // The cloned nest's exit accumulator becomes `D_total`.
        let final_clone = mapper.clone_inst(cand.final_acc);
        let done_jump = mapper
            .data
            .new_local_value()
            .jump(compute_done, vec![final_clone]);
        mapper.data.layout_mut().insert_inst(latch_clone, done_jump);

        // `compute_done`: carry `D_total` back into the (degraded) loop via a
        // fresh loop-carried header parameter. Add the carrier first so the
        // jump argument count matches the header parameter count.
        let carrier = data
            .new_basic_block()
            .add_param(cand.header, i32_ty.clone());
        let done_param = data.bb_data(compute_done).params()[0];
        let mut done_args = orig_args.clone();
        done_args.push(done_param);
        let done_jump = data.new_local_value().jump(cand.header, done_args);
        data.layout_mut().insert_inst(compute_done, done_jump);

        // The original latch (and any other terminator still targeting the
        // header) is now unreachable — the header branches to `degraded_latch`
        // instead — but its `used_by` reverse link to the header survives until
        // DCE removes the block. Every consumer that walks `used_by` (e.g.
        // DeadPhiElimination) sees that edge, so its argument list must still
        // match the header's (now carrier-extended) parameter list. Top up the
        // stale edge with a dummy value; it is never executed.
        let header_params_len = data.bb_data(cand.header).params().len();
        let stale_edges = data
            .bb_data(cand.header)
            .used_by()
            .iter()
            .copied()
            .filter(|&inst| data.layout().parent_bb(inst).is_some())
            .collect::<Vec<_>>();
        for inst in stale_edges {
            let dummy = data.new_local_value().integer(0);
            match data.inst_data(inst).kind() {
                InstKind::Jump(jump) if jump.target() == cand.header => {
                    let t = jump.target();
                    let mut args = jump.args().to_vec();
                    while args.len() < header_params_len {
                        args.push(dummy);
                    }
                    data.replace_inst_with(inst).jump(t, args);
                }
                InstKind::Branch(branch) => {
                    let c = branch.cond();
                    let tt = branch.t_target();
                    let ft = branch.f_target();
                    let mut ta = branch.t_args().to_vec();
                    let mut fa = branch.f_args().to_vec();
                    if tt == cand.header {
                        while ta.len() < header_params_len {
                            ta.push(dummy);
                        }
                    }
                    if ft == cand.header {
                        while fa.len() < header_params_len {
                            fa.push(dummy);
                        }
                    }
                    data.replace_inst_with(inst).branch(c, tt, ta, ft, fa);
                }
                _ => {}
            }
        }

        // The original latch references values inside the (now unreachable)
        // nest, so it cannot serve as the degraded loop's latch. Build a fresh
        // latch that touches only reachable values: the header's own parameters
        // and the `D_total` carrier.
        let r_back = cand.orig_back_args[cand.r_idx];
        let InstKind::Binary(r_update) = data.inst_data(r_back).kind() else {
            return false;
        };
        if !matches!(r_update.op(), BinaryOp::Add | BinaryOp::Sub) {
            return false;
        }
        let lhs_const = data.inst_data(r_update.lhs()).kind().is_const();
        let rhs_const = data.inst_data(r_update.rhs()).kind().is_const();
        let (step, is_sub) = if lhs_const && !rhs_const {
            (r_update.lhs(), r_update.op() == BinaryOp::Sub)
        } else if rhs_const && !lhs_const {
            (r_update.rhs(), r_update.op() == BinaryOp::Sub)
        } else {
            return false;
        };

        let degraded_latch = data
            .new_basic_block()
            .basic_block("irh_latch".into(), vec![]);
        data.layout_mut()
            .insert_bb_after(compute_done, degraded_latch);
        let acc2 = data
            .new_local_value()
            .binary(BinaryOp::Add, params[cand.acc_idx], carrier);
        let step_op = if is_sub { BinaryOp::Sub } else { BinaryOp::Add };
        let r2 = data
            .new_local_value()
            .binary(step_op, params[cand.r_idx], step);
        let mut back_args = params.clone();
        back_args[cand.acc_idx] = acc2;
        back_args[cand.r_idx] = r2;
        back_args.push(carrier);
        let back = data.new_local_value().jump(cand.header, back_args);
        for inst in [acc2, r2, back] {
            data.layout_mut().insert_inst(degraded_latch, inst);
        }

        // Degrade the outer loop: skip the nest, branch straight to the latch.
        let header_term = data.layout().basicblock(cand.header).terminator();
        let InstKind::Branch(branch) = data.inst_data(header_term).kind() else {
            return false;
        };
        let cond = branch.cond();
        let exit_args = branch.f_args().to_vec();
        data.replace_inst_with(header_term).branch(
            cond,
            degraded_latch,
            vec![],
            cand.exit,
            exit_args,
        );

        // Redirect the preheader into the cloned nest.
        let entry_clone = block_map[&cand.entry];
        data.replace_inst_with(data.layout().basicblock(preheader).terminator())
            .jump(entry_clone, vec![]);

        true
    }
}

impl Pass for InvariantReductionHoisting {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().is_decl() {
            return false;
        }
        let Some(cfg) = CFG::new(data) else {
            return false;
        };
        if cfg.is_acyclic() {
            return false;
        }
        let (cfg, _dom_tree, loop_analysis) = LoopAnalysis::from_cfg(cfg);
        for looop in loop_analysis.loops() {
            let Some(candidate) = Self::find_outer(data, looop) else {
                continue;
            };
            if Self::apply(data, &cfg, looop, &candidate) {
                return true;
            }
        }
        false
    }
}

/// Follows `acc` through the nest, verifying that every use inside the nest is
/// additive (`acc ± E`) or a parameter pass-through into a nested loop header.
/// Returns the final value (the nest's accumulator output) if the trace ends at
/// the outer loop's back edge, or `None` if any non-additive use is found.
fn trace_accumulator(
    data: &ArenaContextMut<'_>,
    region: &FxHashSet<BasicBlock>,
    header: BasicBlock,
    exit: BasicBlock,
    seed: Inst,
) -> Option<Inst> {
    let mut worklist = VecDeque::from([seed]);
    let mut visited = FxHashSet::default();
    let mut accumulated = false;
    let mut final_value = None;
    while let Some(cur) = worklist.pop_front() {
        if !visited.insert(cur) {
            continue;
        }
        let users = data
            .inst_data(cur)
            .used_by()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for user in users {
            let parent = data.layout().parent_bb(user);
            let in_region = parent.is_some_and(|block| region.contains(&block));
            match data.inst_data(user).kind() {
                InstKind::Binary(binary) if in_region => {
                    let is_accumulate = matches!(binary.op(), BinaryOp::Add | BinaryOp::Sub)
                        && (binary.lhs() == cur || binary.rhs() == cur);
                    let other = if binary.lhs() == cur {
                        binary.rhs()
                    } else {
                        binary.lhs()
                    };
                    if !is_accumulate || other == cur {
                        return None;
                    }
                    accumulated = true;
                    worklist.push_back(user);
                }
                InstKind::Jump(jump) if in_region => {
                    let target = jump.target();
                    if target == header {
                        if !jump.args().contains(&cur) {
                            return None;
                        }
                        if final_value.replace(cur).is_some() {
                            return None;
                        }
                    } else if region.contains(&target) {
                        push_param_targets(data, region, &mut worklist, target, jump.args(), cur);
                    } else {
                        return None;
                    }
                }
                InstKind::Branch(branch) if in_region => {
                    if branch.cond() == cur {
                        return None;
                    }
                    push_param_targets(
                        data,
                        region,
                        &mut worklist,
                        branch.t_target(),
                        branch.t_args(),
                        cur,
                    );
                    push_param_targets(
                        data,
                        region,
                        &mut worklist,
                        branch.f_target(),
                        branch.f_args(),
                        cur,
                    );
                }
                InstKind::Binary(..) | InstKind::Select(..) | InstKind::Cast(..) => {
                    // non-additive pure use of the accumulator
                    return None;
                }
                _ => {
                    // The original accumulator parameter may be consumed by the
                    // exit block (which observes the final value); nested
                    // accumulator values must have no outside uses.
                    if cur == seed && parent == Some(exit) {
                        continue;
                    }
                    return None;
                }
            }
        }
    }
    if accumulated { final_value } else { None }
}

fn push_param_targets(
    data: &ArenaContextMut<'_>,
    region: &FxHashSet<BasicBlock>,
    worklist: &mut VecDeque<Inst>,
    target: BasicBlock,
    args: &[Inst],
    cur: Inst,
) {
    if !region.contains(&target) {
        return;
    }
    for (arg, &param) in args.iter().zip(data.bb_data(target).params()) {
        if *arg == cur {
            worklist.push_back(param);
        }
    }
}

/// Verifies the nest does not depend on the outer trip counter `r`. `r` may be
/// threaded through nested loop headers (as a pass-through parameter) and may
/// be updated in the latch (`r + 1`, fed to the back edge), but any other use —
/// in a comparison, a GEP, a load address, a condition — means the nest's
/// output depends on `r` and the transformation is rejected.
fn trace_invariant(
    data: &ArenaContextMut<'_>,
    region: &FxHashSet<BasicBlock>,
    header: BasicBlock,
    latch: BasicBlock,
    r: Inst,
) -> bool {
    let mut worklist = VecDeque::from([r]);
    let mut visited = FxHashSet::default();
    while let Some(cur) = worklist.pop_front() {
        if !visited.insert(cur) {
            continue;
        }
        let users = data
            .inst_data(cur)
            .used_by()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for user in users {
            let parent = data.layout().parent_bb(user);
            // The header's own `r < bound` test is the loop's trip test.
            if parent == Some(header) {
                continue;
            }
            let Some(parent) = parent else {
                return false;
            };
            if !region.contains(&parent) {
                return false;
            }
            if parent == latch {
                // Trip update or the back-edge argument: both are discarded in
                // the compute-once clone.
                match data.inst_data(user).kind() {
                    InstKind::Binary(binary)
                        if matches!(binary.op(), BinaryOp::Add | BinaryOp::Sub)
                            && (binary.lhs() == cur || binary.rhs() == cur) =>
                    {
                        worklist.push_back(user);
                    }
                    InstKind::Jump(jump)
                        if jump.target() == header && jump.args().contains(&cur) => {}
                    _ => return false,
                }
            } else {
                // Non-latch blocks may only thread `cur` into a nested header.
                match data.inst_data(user).kind() {
                    InstKind::Jump(jump) => {
                        push_param_targets(
                            data,
                            region,
                            &mut worklist,
                            jump.target(),
                            jump.args(),
                            cur,
                        );
                    }
                    InstKind::Branch(branch) => {
                        if branch.cond() == cur {
                            return false;
                        }
                        push_param_targets(
                            data,
                            region,
                            &mut worklist,
                            branch.t_target(),
                            branch.t_args(),
                            cur,
                        );
                        push_param_targets(
                            data,
                            region,
                            &mut worklist,
                            branch.f_target(),
                            branch.f_args(),
                            cur,
                        );
                    }
                    _ => return false,
                }
            }
        }
    }
    true
}

/// Clones one nest instruction into its fresh block, substituting header
/// parameters (accumulator/trip → zero, others → entering values), cloning
/// nest-internal values, and sharing loop-invariant values defined outside the
/// nest.
struct RegionMapper<'a, 'b> {
    data: &'a mut ArenaContextMut<'b>,
    region: &'a FxHashSet<BasicBlock>,
    block_map: &'a FxHashMap<BasicBlock, BasicBlock>,
    value_map: &'a mut FxHashMap<Inst, Inst>,
    header_param_map: &'a FxHashMap<Inst, Inst>,
}

impl RegionMapper<'_, '_> {
    fn clone_inst(&mut self, inst: Inst) -> Inst {
        if inst.is_global() {
            return inst;
        }
        if let Some(&replacement) = self.header_param_map.get(&inst) {
            return replacement;
        }
        if let Some(&cloned) = self.value_map.get(&inst) {
            return cloned;
        }
        let Some(parent) = self.data.layout().parent_bb(inst) else {
            // Constants and block parameters of blocks outside the nest.
            return inst;
        };
        if !self.region.contains(&parent) {
            // Loop-invariant value defined outside the nest (dominates the
            // header, hence the clone).
            return inst;
        }
        let inst_data = self.data.inst_data(inst).clone();
        let shell = self.data.new_local_value().undef(inst_data.ty().clone());
        self.value_map.insert(inst, shell);
        let mapped = inst_data
            .remap_refs(self)
            .expect("invariant nest has no block references outside itself");
        self.data.replace_inst_with(shell).raw(mapped);
        let dest = self.block_map[&parent];
        self.data.layout_mut().insert_inst(dest, shell);
        shell
    }
}

impl EntityMapper for RegionMapper<'_, '_> {
    type Error = ();

    fn map_inst(&mut self, inst: Inst) -> Result<Inst, Self::Error> {
        Ok(self.clone_inst(inst))
    }

    fn map_block(&mut self, block: BasicBlock) -> Result<BasicBlock, Self::Error> {
        self.block_map.get(&block).copied().ok_or(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        Program, Type,
        builder_trait::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
    };

    /// Build a `for r in 0..R { for i in 0..T { acc += a[i]; } }` nest. The
    /// outer loop carries `[r, acc]` and the inner loop carries `[i, acc]`.
    fn build_nest(program: &mut Program, symbolic_bound: bool) -> Function {
        let function = program.new_function(
            Type::get_i32(),
            "nest".into(),
            vec![
                Type::get_i32(),
                Type::get_i32(),
                Type::get_pointer(Type::get_array(Type::get_i32(), 16)),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let (r_bound, t_bound) = if symbolic_bound {
            (data.params()[0], data.params()[1])
        } else {
            (
                data.new_local_inst().integer(2),
                data.new_local_inst().integer(8),
            )
        };
        let base = data.params()[2];

        let outer = data
            .new_basic_block()
            .basic_block("outer".into(), vec![Type::get_i32(), Type::get_i32()]);
        let outer_body = data
            .new_basic_block()
            .basic_block("outer_body".into(), vec![]);
        let inner = data
            .new_basic_block()
            .basic_block("inner".into(), vec![Type::get_i32(), Type::get_i32()]);
        let inner_body = data
            .new_basic_block()
            .basic_block("inner_body".into(), vec![]);
        let inner_exit = data
            .new_basic_block()
            .basic_block("inner_exit".into(), vec![]);
        let outer_latch = data
            .new_basic_block()
            .basic_block("outer_latch".into(), vec![]);
        let outer_exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [
            outer,
            outer_body,
            inner,
            inner_body,
            inner_exit,
            outer_latch,
            outer_exit,
        ] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let one = data.new_local_inst().integer(1);
        let entry_jump = data.new_local_inst().jump(outer, vec![zero, zero]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let r = data.bb_data(outer).params()[0];
        let acc = data.bb_data(outer).params()[1];
        let r_test = data.new_local_inst().binary(BinaryOp::Lt, r, r_bound);
        let outer_branch =
            data.new_local_inst()
                .branch(r_test, outer_body, vec![], outer_exit, vec![]);
        data.layout_mut().insert_inst(outer, r_test);
        data.layout_mut().insert_inst(outer, outer_branch);

        let enter_inner = data.new_local_inst().jump(inner, vec![zero, acc]);
        data.layout_mut().insert_inst(outer_body, enter_inner);

        let i = data.bb_data(inner).params()[0];
        let inner_acc = data.bb_data(inner).params()[1];
        let i_test = data.new_local_inst().binary(BinaryOp::Lt, i, t_bound);
        let inner_branch =
            data.new_local_inst()
                .branch(i_test, inner_body, vec![], inner_exit, vec![]);
        data.layout_mut().insert_inst(inner, i_test);
        data.layout_mut().insert_inst(inner, inner_branch);

        let gep = data.new_local_inst().get_elem_ptr(base, vec![zero, i]);
        let load = data.new_local_inst().load(gep);
        let acc2 = data.new_local_inst().binary(BinaryOp::Add, inner_acc, load);
        let i2 = data.new_local_inst().binary(BinaryOp::Add, i, one);
        for inst in [gep, load, acc2, i2] {
            data.layout_mut().insert_inst(inner_body, inst);
        }
        let inner_back = data.new_local_inst().jump(inner, vec![i2, acc2]);
        data.layout_mut().insert_inst(inner_body, inner_back);

        let r2 = data.new_local_inst().binary(BinaryOp::Add, r, one);
        data.layout_mut().insert_inst(inner_exit, r2);
        let outer_back = data.new_local_inst().jump(outer, vec![r2, inner_acc]);
        data.layout_mut().insert_inst(inner_exit, outer_back);

        let ret = data.new_local_inst().ret(Some(acc));
        data.layout_mut().insert_inst(outer_exit, ret);
        function
    }

    fn run(program: &mut Program, function: Function) -> bool {
        let mut context = ArenaContextMut {
            program,
            curr_func: Some(function),
        };
        InvariantReductionHoisting.run_on(&mut context)
    }

    #[test]
    fn degrades_an_invariant_outer_reduction_nest() {
        let mut program = Program::new();
        let function = build_nest(&mut program, true);
        let data = program.func_data(function);
        let original_blocks = data.layout().basicblocks().len();
        let changed = run(&mut program, function);
        assert!(changed);

        let data = program.func_data(function);
        // A compute-once clone of the nest plus the `compute_done` block were
        // added (4 nest blocks + done).
        let new_blocks = data.layout().basicblocks().len();
        assert!(new_blocks >= original_blocks + 5);

        // `compute_done` receives `D_total` and jumps into the degraded loop.
        let done = data
            .layout()
            .basicblocks()
            .iter()
            .map(|layout| layout.bb())
            .find(|&b| data.bb_data(b).name().starts_with("irh_done"))
            .expect("compute_done must exist");
        let done_insts = data.layout().basicblock(done).insts();
        let InstKind::Jump(done_jump) = data.inst_data(*done_insts.iter().last().unwrap()).kind()
        else {
            panic!("compute_done must end in a jump");
        };
        let outer = done_jump.target();
        // The degraded outer header carries an extra `D_total` carrier param.
        assert_eq!(data.bb_data(outer).params().len(), 3);

        // The degraded header branches straight to the latch, skipping the nest.
        let outer_insts = data.layout().basicblock(outer).insts();
        let InstKind::Branch(branch) = data.inst_data(*outer_insts.iter().last().unwrap()).kind()
        else {
            panic!("outer header must branch");
        };
        assert_eq!(
            data.bb_data(branch.f_target())
                .name()
                .trim_end_matches(|c: char| c.is_ascii_digit() || c == '_'),
            "exit"
        );
        let latch = branch.t_target();
        let latch_insts = data.layout().basicblock(latch).insts();
        let InstKind::Jump(latch_jump) = data.inst_data(*latch_insts.iter().last().unwrap()).kind()
        else {
            panic!("latch must end in a jump");
        };
        assert_eq!(latch_jump.target(), outer);
        assert_eq!(latch_jump.args().len(), 3);

        // The second application must be stable (the degraded loop is no longer
        // a non-trivial nest).
        assert!(!run(&mut program, function));
        assert_eq!(
            program.func_data(function).layout().basicblocks().len(),
            new_blocks
        );

        // Every terminator that still targets the degraded header must supply
        // the carrier argument, including the original (now unreachable)
        // latch edge: `used_by` reverse links survive until DCE, and
        // DeadPhiElimination walks them to trim arguments, so a stale edge
        // with a short argument list would panic it (many_mat_cal regression).
        let data = program.func_data(function);
        let stale_edges = data
            .bb_data(outer)
            .used_by()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        assert!(
            stale_edges.len() >= 3,
            "compute_done + degraded latch + original latch must target the header"
        );
        for &inst in &stale_edges {
            match data.inst_data(inst).kind() {
                InstKind::Jump(jump) => {
                    assert_eq!(
                        jump.args().len(),
                        3,
                        "stale jump edge must carry the carrier"
                    );
                }
                InstKind::Branch(branch) => {
                    if branch.t_target() == outer {
                        assert_eq!(branch.t_args().len(), 3);
                    }
                    if branch.f_target() == outer {
                        assert_eq!(branch.f_args().len(), 3);
                    }
                }
                _ => panic!("a terminator targeting the header must be a jump or branch"),
            }
        }
    }

    #[test]
    fn skips_a_nest_with_a_store_in_the_outer_body() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "store_nest".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let bound = data.params()[0];
        let outer = data
            .new_basic_block()
            .basic_block("outer".into(), vec![Type::get_i32(), Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [outer, body, exit] {
            data.layout_mut().push_bb_back(block);
        }
        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(outer, vec![zero, zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let r = data.bb_data(outer).params()[0];
        let acc = data.bb_data(outer).params()[1];
        let one = data.new_local_inst().integer(1);
        let test = data.new_local_inst().binary(BinaryOp::Lt, r, bound);
        let branch = data
            .new_local_inst()
            .branch(test, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(outer, test);
        data.layout_mut().insert_inst(outer, branch);
        let slot = data.new_local_inst().alloc(Type::get_i32());
        let store = data.new_local_inst().store(zero, slot);
        let acc2 = data.new_local_inst().binary(BinaryOp::Add, acc, one);
        let r2 = data.new_local_inst().binary(BinaryOp::Add, r, one);
        for inst in [slot, store, acc2, r2] {
            data.layout_mut().insert_inst(body, inst);
        }
        let back = data.new_local_inst().jump(outer, vec![r2, acc2]);
        data.layout_mut().insert_inst(body, back);
        let ret = data.new_local_inst().ret(Some(acc));
        data.layout_mut().insert_inst(exit, ret);

        let blocks = program.func_data(function).layout().basicblocks().len();
        assert!(
            !run(&mut program, function),
            "a store in the body must veto"
        );
        assert_eq!(
            program.func_data(function).layout().basicblocks().len(),
            blocks
        );
    }
}
