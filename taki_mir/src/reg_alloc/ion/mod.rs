/*
 * Portions of this module are adapted from regalloc2 0.15.1,
 * https://github.com/bytecodealliance/regalloc2/tree/v0.15.1.
 *
 * Released under the Apache License 2.0 with LLVM Exception. See the
 * repository LICENSE for the full license text. Local modifications adapt
 * regalloc2's interfaces and allocation utilities to taki_mir.
 */

//! Backtracking register allocator adapted from regalloc2's Ion allocator.
//!
//! The public entry point is deliberately separate from the production
//! allocator while this port is validated.  In particular, it normalizes the
//! client's VRegs into allocator-local dense VRegs before constructing Ion's
//! VReg-indexed state.

mod cfg;
mod data_structures;
mod domtree;
mod function;
mod indexset;
mod liveranges;
mod merge;
mod moves;
mod postorder;
mod process;
// Kept crate-visible while `spill.rs` remains on the upstream module layout.
pub(crate) use process::AllocRegResult;
mod redundant_moves;
mod reg_traversal;
mod requirement;
mod spill;

pub use cfg::{CFGInfo, CFGInfoCtx};
pub use data_structures::{
    BlockparamIn as BlockParamIn, BlockparamOut as BlockParamOut, CodeRange, Ctx, Env,
    FixedRegFixupLevel, InsertMovePrio, InsertedMove, InsertedMoves, LiveBundle, LiveBundleIndex,
    LiveBundleVec, LiveRange, LiveRangeFlag, LiveRangeIndex, LiveRangeKey, LiveRangeList,
    LiveRangeListEntry, PRegIndex, SpillSetIndex, SpillSlotData, SpillSlotIndex, Use, UseList,
    VRegIndex,
};
pub use function::DenseVRegFunction;
pub use indexset::IndexSet;
pub use liveranges::{Liveness, SpillWeight, spill_weight_from_constraint};
pub use redundant_moves::{RedundantMoveAction, RedundantMoveEliminator, RedundantMoveState};
pub use reg_traversal::RegTraversalIter;
pub use requirement::{Requirement, RequirementConflict, RequirementConflictAt};

use crate::{
    VecExt,
    reg_alloc::{
        function::Function,
        reg::{Edit, MachineEnv, Output, RegClass, VReg},
    },
};

impl<'a, F: Function> Env<'a, F> {
    /// Initialize an Ion allocation context for one dense function view.
    ///
    /// The local port has no annotations or allocator statistics in `Output`;
    /// those upstream-only facilities are intentionally reset and omitted here.
    fn new(func: &'a F, env: &'a MachineEnv, ctx: &'a mut Ctx) -> Self {
        let ninstrs = func.num_insts();
        let nblocks = func.num_blocks();

        ctx.liveins.preallocate(nblocks);
        ctx.liveouts.preallocate(nblocks);
        ctx.ranges.preallocate(4 * ninstrs);
        ctx.bundles.preallocate(ninstrs);
        ctx.spillsets.preallocate(ninstrs);
        ctx.vregs.preallocate(func.num_vregs());
        ctx.output.allocs.preallocate(4 * ninstrs);

        // A context is reusable between invocations. Clear every allocation
        // product; retain capacities to avoid churn on repeated compilations.
        ctx.liveins.clear();
        ctx.liveouts.clear();
        ctx.blockparam_ins.clear();
        ctx.blockparam_outs.clear();
        ctx.ranges.storage.clear();
        ctx.bundles.storage.clear();
        ctx.spillsets.storage.clear();
        ctx.vregs.storage.clear();
        ctx.allocation_queue.heap.clear();
        ctx.spilled_bundles.clear();
        ctx.scratch_spillset_pool
            .extend(ctx.spillslots.drain(..).map(|mut slot| {
                slot.ranges.btree.clear();
                slot.ranges
            }));
        ctx.slots_by_class = Default::default();
        ctx.extra_spillslots_by_class = Default::default();
        ctx.preferred_victim_by_class = [crate::reg_alloc::reg::PReg::invalid(); 3];
        ctx.multi_fixed_reg_fixups.clear();
        ctx.allocated_bundle_count = 0;
        ctx.debug_annotations.clear();
        ctx.conflict_set.clear();
        ctx.scratch_conflicts.clear();
        ctx.scratch_bundle.clear();
        ctx.scratch_vreg_ranges.clear();
        ctx.scratch_workqueue.clear();
        ctx.scratch_operand_rewrites.clear();
        ctx.scratch_removed_lrs.clear();
        ctx.scratch_removed_lrs_vregs.clear();
        ctx.scratch_workqueue_set.clear();
        ctx.output = Output::default();

        for preg in &mut ctx.pregs {
            preg.is_stack = false;
            preg.allocations.btree.clear();
        }

        Self { func, env, ctx }
    }

    fn init(&mut self) -> Result<(), String> {
        self.create_pregs_and_vregs();
        self.compute_liveness()?;
        self.build_liveranges()?;
        self.fixup_multi_fixed_vregs();
        self.merge_vreg_bundles();
        self.queue_bundles();
        Ok(())
    }

    fn run(&mut self) -> Result<data_structures::Edits, String> {
        self.process_bundles()?;
        self.try_allocating_regs_for_spilled_bundles();
        self.allocate_spillslots();
        let moves = self.apply_allocations_and_insert_moves();
        Ok(self.resolve_inserted_moves(moves))
    }
}

/// Allocate registers with the Ion backtracking allocator.
///
/// Ion indexes its analysis state directly by VReg number. `DenseVRegFunction`
/// therefore provides a private dense view even when the source function uses
/// sparse VRegs (as VCode does for pinned physical-register values).
pub fn run<F: Function>(func: &F, mach_env: &MachineEnv) -> Result<Output, String> {
    let dense_func = DenseVRegFunction::new(func);
    let mut ctx = Ctx::default();
    ctx.cfginfo.init(&dense_func, &mut ctx.cfginfo_ctx)?;

    let mut edits = {
        let mut env = Env::new(&dense_func, mach_env, &mut ctx);
        env.init()?;
        env.run()?
    };
    // Map allocator-local vreg indices in the edits back to the original
    // function's vregs so the emitter can size moves from real value types.
    let remapped = edits.drain_edits().map(|(point, edit)| {
        let edit = match edit {
            Edit::Move {
                from,
                to,
                class,
                vreg: Some(idx),
            } => {
                let local = VReg::new(idx as usize, RegClass::Int);
                let original = dense_func
                    .original_vreg(local)
                    .map(|v| v.vreg() as u32)
                    .unwrap_or(idx);
                Edit::Move {
                    from,
                    to,
                    class,
                    vreg: Some(original),
                }
            }
            other => other,
        };
        (point, edit)
    });
    ctx.output.edits.extend(remapped);
    Ok(core::mem::take(&mut ctx.output))
}

#[cfg(test)]
mod tests;


