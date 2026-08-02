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
mod tests {
    use super::*;
    use crate::reg_alloc::{
        index::{Block, Inst, InstRange},
        reg::{Allocation, Edit, Operand, PReg, PRegSet, RegClass, VReg},
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
        clobbers: Vec<PRegSet>,
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

        fn inst_clobbers(&self, inst: Inst) -> PRegSet {
            self.clobbers[inst.index()]
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
        // r2 is a reserved dedicated scratch: never allocatable, so it can
        // never appear as a parallel-move endpoint (mirrors AArch64/RISC-V).
        let r2 = PReg::new(2, RegClass::Int);
        MachineEnv {
            preferred_regs_by_class: [
                PRegSet::empty().with(r0).with(r1),
                PRegSet::empty(),
                PRegSet::empty(),
            ],
            non_preferred_regs_by_class: [PRegSet::empty(), PRegSet::empty(), PRegSet::empty()],
            scratch_by_class: [Some(r2), None, None],
            post_ra_scratch_by_class: [vec![r2], vec![], vec![]],
            fixed_stack_slots: vec![],
        }
    }

    fn wide_env() -> MachineEnv {
        // r0-r3 allocatable, all caller-saved: no register can survive a call
        // clobber, forcing real spills for values live across one.
        let mut preferred = PRegSet::empty();
        for index in 0..4 {
            preferred = preferred.with(PReg::new(index, RegClass::Int));
        }
        MachineEnv {
            preferred_regs_by_class: [preferred, PRegSet::empty(), PRegSet::empty()],
            non_preferred_regs_by_class: [PRegSet::empty(), PRegSet::empty(), PRegSet::empty()],
            scratch_by_class: [None, None, None],
            post_ra_scratch_by_class: [vec![], vec![], vec![]],
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
            clobbers: vec![PRegSet::empty(), PRegSet::empty()],
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
            clobbers: vec![PRegSet::empty(), PRegSet::empty()],
        };

        let output = run(&function, &machine_env()).expect("Ion allocation should succeed");
        assert_eq!(output.inst_alloc_offsets, vec![0, 1]);
        assert!(output.inst_allocs(0)[0].is_reg());
        assert!(output.inst_allocs(1)[0].is_reg());
    }

    // ─── M22: incoming fixed-def argument gates ────────────────────────────

    fn assert_no_edits(output: &Output) {
        assert!(
            output.edits.is_empty(),
            "expected no allocator edits, got {:#?}",
            output.edits
        );
    }

    fn assert_no_spills(output: &Output) {
        assert_eq!(output.num_spillslots, 0, "expected no stack spills");
    }

    #[test]
    fn incoming_fixed_def_reused_on_the_same_register_needs_no_move() {
        // Args fixed-def src -> r0; the only later use also wants r0.
        let r0 = PReg::new(0, RegClass::Int);
        let source = VReg::new(400, RegClass::Int);
        let function = TestFunction {
            operands: vec![
                vec![Operand::reg_fixed_def(source, r0)],
                vec![Operand::reg_fixed_use(source, r0)],
            ],
            blocks: vec![InstRange::new(Inst::new(0), Inst::new(2))],
            succs: vec![vec![]],
            preds: vec![vec![]],
            params: vec![vec![]],
            args: vec![vec![]],
            branches: vec![false, false],
            returns: vec![false, true],
            clobbers: vec![PRegSet::empty(), PRegSet::empty()],
        };

        let output = run(&function, &machine_env()).expect("Ion allocation should succeed");
        assert_eq!(
            output.inst_allocs(0),
            &[crate::reg_alloc::reg::Allocation::reg(r0)]
        );
        assert_eq!(
            output.inst_allocs(1),
            &[crate::reg_alloc::reg::Allocation::reg(r0)]
        );
        assert_no_edits(&output);
        assert_no_spills(&output);
    }

    #[test]
    fn incoming_fixed_def_reassigned_emits_a_register_move_not_a_spill() {
        // Args fixed-def src -> r0, but the call argument needs src in r1.
        // The value must move r0 -> r1 between the two instructions, never
        // round-tripping through a stack slot.
        let r0 = PReg::new(0, RegClass::Int);
        let r1 = PReg::new(1, RegClass::Int);
        let source = VReg::new(400, RegClass::Int);
        let function = TestFunction {
            operands: vec![
                vec![Operand::reg_fixed_def(source, r0)],
                vec![Operand::reg_fixed_use(source, r1)],
            ],
            blocks: vec![InstRange::new(Inst::new(0), Inst::new(2))],
            succs: vec![vec![]],
            preds: vec![vec![]],
            params: vec![vec![]],
            args: vec![vec![]],
            branches: vec![false, false],
            returns: vec![false, true],
            clobbers: vec![PRegSet::empty(), PRegSet::empty()],
        };

        let output = run(&function, &machine_env()).expect("Ion allocation should succeed");
        assert_eq!(
            output.inst_allocs(0),
            &[crate::reg_alloc::reg::Allocation::reg(r0)]
        );
        assert_eq!(
            output.inst_allocs(1),
            &[crate::reg_alloc::reg::Allocation::reg(r1)]
        );
        assert_no_spills(&output);
        let moves: Vec<_> = output
            .edits
            .iter()
            .filter_map(|(_, edit)| match edit {
                Edit::Move { from, to, .. } if from.is_reg() && to.is_reg() => Some((*from, *to)),
                _ => None,
            })
            .collect();
        assert!(
            moves.contains(&(
                crate::reg_alloc::reg::Allocation::reg(r0),
                crate::reg_alloc::reg::Allocation::reg(r1),
            )),
            "expected a r0 -> r1 register move, got {moves:?}"
        );
    }

    #[test]
    fn parallel_argument_swap_cycle_is_broken_without_a_spill() {
        // Args fixed-defs a -> r0, b -> r1. A later call needs a in r1 and b
        // in r0: a classic swap cycle that parallel-move resolution must break
        // without stack traffic.
        let r0 = PReg::new(0, RegClass::Int);
        let r1 = PReg::new(1, RegClass::Int);
        let a = VReg::new(400, RegClass::Int);
        let b = VReg::new(401, RegClass::Int);
        let function = TestFunction {
            operands: vec![
                vec![Operand::reg_fixed_def(a, r0), Operand::reg_fixed_def(b, r1)],
                vec![Operand::reg_fixed_use(a, r1), Operand::reg_fixed_use(b, r0)],
            ],
            blocks: vec![InstRange::new(Inst::new(0), Inst::new(2))],
            succs: vec![vec![]],
            preds: vec![vec![]],
            params: vec![vec![]],
            args: vec![vec![]],
            branches: vec![false, false],
            returns: vec![false, true],
            clobbers: vec![PRegSet::empty(), PRegSet::empty()],
        };

        let output = run(&function, &machine_env()).expect("Ion allocation should succeed");
        assert_no_spills(&output);
        assert_eq!(
            output.inst_allocs(0),
            &[
                crate::reg_alloc::reg::Allocation::reg(r0),
                crate::reg_alloc::reg::Allocation::reg(r1),
            ]
        );
        assert_eq!(
            output.inst_allocs(1),
            &[
                crate::reg_alloc::reg::Allocation::reg(r1),
                crate::reg_alloc::reg::Allocation::reg(r0),
            ]
        );
        assert!(
            !output.edits.is_empty(),
            "swap cycle must materialize parallel moves"
        );
    }

    #[test]
    fn high_pressure_incoming_arguments_spill_to_real_stack_slots() {
        // Four parameters arrive in r0-r3 and all stay live across a call
        // that clobbers every allocatable register. Since no callee-saved
        // register exists, Ion must emit real stack spills (`num_spillslots`)
        // rather than ABI home-slot round trips.
        let wide = wide_env();
        let mut clobber_all = PRegSet::empty();
        for index in 0..4 {
            clobber_all = clobber_all.with(PReg::new(index, RegClass::Int));
        }
        let regs: Vec<PReg> = (0..4).map(|i| PReg::new(i, RegClass::Int)).collect();
        let params: Vec<VReg> = (0..4)
            .map(|i| VReg::new(400 + i as usize, RegClass::Int))
            .collect();

        let function = TestFunction {
            operands: vec![
                params
                    .iter()
                    .zip(&regs)
                    .map(|(&vreg, &preg)| Operand::reg_fixed_def(vreg, preg))
                    .collect(),
                vec![],
                params.iter().map(|&vreg| Operand::reg_use(vreg)).collect(),
            ],
            blocks: vec![InstRange::new(Inst::new(0), Inst::new(3))],
            succs: vec![vec![]],
            preds: vec![vec![]],
            params: vec![vec![]],
            args: vec![vec![]],
            branches: vec![false, false, false],
            returns: vec![false, false, true],
            clobbers: vec![PRegSet::empty(), clobber_all, PRegSet::empty()],
        };

        let output = run(&function, &wide).expect("Ion allocation should succeed");
        assert_eq!(
            output.inst_allocs(0).len(),
            4,
            "every incoming fixed def must be allocated"
        );
        assert_eq!(
            output.inst_allocs(2).len(),
            4,
            "every post-call use must be allocated"
        );
        assert!(
            output.num_spillslots > 0,
            "values live across an all-register clobber must spill"
        );
        assert!(
            output.edits.iter().any(|(_, edit)| matches!(
                edit,
                Edit::Move { from, to, .. } if from.is_reg() && to.is_stack()
            )),
            "spills must be real allocator stack slots, not home slots"
        );
    }

    fn float_env() -> MachineEnv {
        let f0 = PReg::new(0, RegClass::Float);
        let f1 = PReg::new(1, RegClass::Float);
        // f2 is the reserved dedicated scratch for the float class.
        let f2 = PReg::new(2, RegClass::Float);
        MachineEnv {
            preferred_regs_by_class: [
                PRegSet::empty(),
                PRegSet::empty().with(f0).with(f1),
                PRegSet::empty(),
            ],
            non_preferred_regs_by_class: [PRegSet::empty(), PRegSet::empty(), PRegSet::empty()],
            scratch_by_class: [None, Some(f2), None],
            post_ra_scratch_by_class: [vec![], vec![f2], vec![]],
            fixed_stack_slots: vec![],
        }
    }

    #[test]
    fn float_parameter_swap_cycle_is_broken_without_a_spill() {
        let env = float_env();
        let f0 = PReg::new(0, RegClass::Float);
        let f1 = PReg::new(1, RegClass::Float);
        let a = VReg::new(500, RegClass::Float);
        let b = VReg::new(501, RegClass::Float);
        let function = TestFunction {
            operands: vec![
                vec![Operand::reg_fixed_def(a, f0), Operand::reg_fixed_def(b, f1)],
                vec![Operand::reg_fixed_use(a, f1), Operand::reg_fixed_use(b, f0)],
            ],
            blocks: vec![InstRange::new(Inst::new(0), Inst::new(2))],
            succs: vec![vec![]],
            preds: vec![vec![]],
            params: vec![vec![]],
            args: vec![vec![]],
            branches: vec![false, false],
            returns: vec![false, true],
            clobbers: vec![PRegSet::empty(), PRegSet::empty()],
        };

        let output = run(&function, &env).expect("Ion allocation should succeed");
        assert_no_spills(&output);
        assert_eq!(
            output.inst_allocs(0),
            &[Allocation::reg(f0), Allocation::reg(f1)]
        );
        assert_eq!(
            output.inst_allocs(1),
            &[Allocation::reg(f1), Allocation::reg(f0)]
        );
        assert!(
            !output.edits.is_empty(),
            "float swap cycle must materialize parallel moves"
        );
    }

    #[test]
    fn incoming_value_flows_through_a_normal_use_then_a_fixed_call_argument() {
        // A parameter is used by an ordinary instruction (any register), then
        // must land in the ABI argument register r1 for a call. No spill; the
        // value just moves between registers.
        let r0 = PReg::new(0, RegClass::Int);
        let r1 = PReg::new(1, RegClass::Int);
        let source = VReg::new(400, RegClass::Int);
        let function = TestFunction {
            operands: vec![
                vec![Operand::reg_fixed_def(source, r0)],
                vec![Operand::reg_use(source)],
                vec![Operand::reg_fixed_use(source, r1)],
            ],
            blocks: vec![InstRange::new(Inst::new(0), Inst::new(3))],
            succs: vec![vec![]],
            preds: vec![vec![]],
            params: vec![vec![]],
            args: vec![vec![]],
            branches: vec![false, false, false],
            returns: vec![false, false, true],
            clobbers: vec![PRegSet::empty(), PRegSet::empty(), PRegSet::empty()],
        };

        let output = run(&function, &machine_env()).expect("Ion allocation should succeed");
        assert_no_spills(&output);
        assert_eq!(
            output.inst_allocs(0),
            &[Allocation::reg(r0)],
            "incoming fixed def must stay in r0"
        );
        assert_eq!(
            output.inst_allocs(2),
            &[Allocation::reg(r1)],
            "call argument must land in r1"
        );
    }
}
