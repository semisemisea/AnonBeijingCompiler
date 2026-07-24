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
        reg::{MachineEnv, Output},
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
    ctx.output.edits.extend(edits.drain_edits());
    Ok(core::mem::take(&mut ctx.output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reg_alloc::{
        index::{Block, Inst, InstRange},
        reg::{Operand, PReg, PRegSet, RegClass, VReg},
    };

    struct TestFunction {
        operands: Vec<Vec<Operand>>,
        blocks: Vec<InstRange>,
        succs: Vec<Vec<Block>>,
        preds: Vec<Vec<Block>>,
        params: Vec<Vec<VReg>>,
        args: Vec<Vec<Vec<VReg>>>,
        branches: Vec<bool>,
        returns: Vec<bool>,
    }

    impl Function for TestFunction {
        fn num_insts(&self) -> usize {
            self.operands.len()
        }

        fn num_blocks(&self) -> usize {
            self.blocks.len()
        }

        fn entry_block(&self) -> Block {
            Block::new(0)
        }

        fn block_insns(&self, block: Block) -> InstRange {
            self.blocks[block.index()]
        }

        fn block_succs(&self, block: Block) -> &[Block] {
            &self.succs[block.index()]
        }

        fn block_preds(&self, block: Block) -> &[Block] {
            &self.preds[block.index()]
        }

        fn block_params(&self, block: Block) -> &[VReg] {
            &self.params[block.index()]
        }

        fn is_ret(&self, inst: Inst) -> bool {
            self.returns[inst.index()]
        }

        fn is_branch(&self, inst: Inst) -> bool {
            self.branches[inst.index()]
        }

        fn branch_blockparams(&self, block: Block, _inst: Inst, succ_idx: usize) -> &[VReg] {
            &self.args[block.index()][succ_idx]
        }

        fn inst_operands(&self, inst: Inst) -> &[Operand] {
            &self.operands[inst.index()]
        }

        fn inst_clobbers(&self, _inst: Inst) -> PRegSet {
            PRegSet::empty()
        }

        fn num_vregs(&self) -> usize {
            // Deliberately sparse source VRegs exercise DenseVRegFunction.
            0
        }

        fn spillslot_size(&self, _class: RegClass) -> usize {
            1
        }
    }

    fn machine_env() -> MachineEnv {
        let r0 = PReg::new(0, RegClass::Int);
        let r1 = PReg::new(1, RegClass::Int);
        MachineEnv {
            preferred_regs_by_class: [
                PRegSet::empty().with(r0).with(r1),
                PRegSet::empty(),
                PRegSet::empty(),
            ],
            non_preferred_regs_by_class: [PRegSet::empty(), PRegSet::empty(), PRegSet::empty()],
            scratch_by_class: [Some(r1), None, None],
            post_ra_scratch_by_class: [vec![r1], vec![], vec![]],
            fixed_stack_slots: vec![],
        }
    }

    #[test]
    fn run_allocates_fixed_and_reused_operands() {
        let r0 = PReg::new(0, RegClass::Int);
        let source = VReg::new(400, RegClass::Int);
        let result = VReg::new(900, RegClass::Int);
        let function = TestFunction {
            operands: vec![
                vec![Operand::reg_fixed_def(source, r0)],
                vec![
                    Operand::reg_fixed_use(source, r0),
                    Operand::reg_reuse_def(result, 0),
                ],
            ],
            blocks: vec![InstRange::new(Inst::new(0), Inst::new(2))],
            succs: vec![vec![]],
            preds: vec![vec![]],
            params: vec![vec![]],
            args: vec![vec![]],
            branches: vec![false, false],
            returns: vec![false, true],
        };

        let output = run(&function, &machine_env()).expect("Ion allocation should succeed");
        assert_eq!(output.inst_alloc_offsets, vec![0, 1]);
        assert_eq!(
            output.inst_allocs(0),
            &[crate::reg_alloc::reg::Allocation::reg(r0)]
        );
        assert_eq!(
            output.inst_allocs(1),
            &[
                crate::reg_alloc::reg::Allocation::reg(r0),
                crate::reg_alloc::reg::Allocation::reg(r0),
            ]
        );
    }

    #[test]
    fn run_propagates_block_parameter_cfg_liveness() {
        let source = VReg::new(400, RegClass::Int);
        let param = VReg::new(900, RegClass::Int);
        let function = TestFunction {
            operands: vec![
                vec![Operand::reg_def(source)],
                vec![Operand::reg_use(param)],
            ],
            blocks: vec![
                InstRange::new(Inst::new(0), Inst::new(1)),
                InstRange::new(Inst::new(1), Inst::new(2)),
            ],
            succs: vec![vec![Block::new(1)], vec![]],
            preds: vec![vec![], vec![Block::new(0)]],
            params: vec![vec![], vec![param]],
            args: vec![vec![vec![source]], vec![]],
            branches: vec![true, false],
            returns: vec![false, true],
        };

        let output = run(&function, &machine_env()).expect("Ion allocation should succeed");
        assert_eq!(output.inst_alloc_offsets, vec![0, 1]);
        assert!(output.inst_allocs(0)[0].is_reg());
        assert!(output.inst_allocs(1)[0].is_reg());
    }
}
