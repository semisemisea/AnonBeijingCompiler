use std::fmt::Debug;

use rustc_hash::FxHashMap;
use smallvec::SmallVec;
pub use tomori_utils::Ranges;

use crate::{
    abi::{ABIMachineSpec, CalleeABI},
    block_order::{BlockLoweringOrder, MirBlockIndex},
    reg_alloc::{
        function::Function,
        index::{Block, Inst, InstRange},
        reg::{
            AllocationKind, Edit, InstOrEdit, Operand, OperandCollector, OperandConstraint,
            OperandKind, OperandVisitor, OperandWriter, Output, PRegSet, RegClass, VReg,
        },
    },
    register::{Reg, VRegAllocator, Writable},
    types::LoweredType,
};

#[derive(Debug, PartialEq, Eq)]
pub enum MachTerminator {
    /// Not a terminator
    None,
    /// Return
    Return,
    /// Tail call
    TailReturn,
    /// Branch
    Branch,
}

pub trait MachInst: Clone + Debug {
    type ABISpec: ABIMachineSpec<I = Self>;

    fn get_operands(&mut self, collector: &mut impl OperandVisitor);

    // TODO: return a pair of register
    fn is_move(&self) -> Option<(Writable<Reg>, Reg)>;

    fn is_term(&self) -> MachTerminator;

    /// Whether the emitter must restore the current frame before this
    /// instruction. Backends may override this for return-like instructions
    /// that stay within the current invocation.
    fn needs_epilogue(&self) -> bool {
        matches!(self.is_term(), MachTerminator::Return)
    }

    fn rc_for_type(ty: LoweredType) -> (&'static [RegClass], &'static [LoweredType]);

    fn gen_jump(target: MirBlockIndex) -> Self;

    /// Target-specific encoding/form validation. Targets without additional
    /// invariants inherit the no-op verifier.
    fn verify(&self) -> Result<(), String> {
        Ok(())
    }
}

pub trait EmitContext: core::fmt::Write {
    fn write_reg(&mut self, reg: &Reg) -> core::fmt::Result;
    fn write_label_ref(&mut self, idx: MirBlockIndex) -> core::fmt::Result;
    fn write_function_label(&mut self, func: crate::prelude::HirFunction) -> core::fmt::Result;
    fn write_global_label(&mut self, gv: crate::prelude::HirInst) -> core::fmt::Result;
    fn write_external_symbol(&mut self, symbol: &str) -> core::fmt::Result;

    /// End the current instruction: flush the accumulated instruction text as
    /// one slot. No-op when nothing was written since the last flush.
    fn end_inst(&mut self) -> core::fmt::Result {
        Ok(())
    }

    /// Take the text accumulated since the last flush (the current
    /// instruction's text) for structured use, e.g. composing a branch
    /// prefix that includes a rendered register.
    fn take_inst_text(&mut self) -> String {
        String::new()
    }

    /// Emit an optimizable conditional branch whose target remains symbolic.
    /// `prefix` is the text before the target label; `inv_prefix` is the
    /// inverted-encoding equivalent used when branch optimization flips the
    /// condition.
    fn put_branch(
        &mut self,
        prefix: &str,
        inv_prefix: Option<&str>,
        target: MirBlockIndex,
        kind: crate::emit_buffer::LabelKind,
    ) -> core::fmt::Result {
        let _ = (prefix, inv_prefix, target, kind);
        Ok(())
    }

    /// Emit an optimizable unconditional branch.
    fn put_uncond_branch(
        &mut self,
        prefix: &str,
        target: MirBlockIndex,
        kind: crate::emit_buffer::LabelKind,
    ) -> core::fmt::Result {
        let _ = (prefix, target, kind);
        Ok(())
    }
}

pub trait MachInstEmit {
    fn emit(&self, ctx: &mut dyn EmitContext) -> core::fmt::Result;
}

pub trait VCodeInst: MachInst + MachInstEmit {}
impl<T: MachInst + MachInstEmit> VCodeInst for T {}

pub struct VCodeContainer<I>
where
    I: VCodeInst,
{
    vreg_types: Vec<LoweredType>,
    insts: Vec<I>,

    /// ABI of the function
    pub abi: CalleeABI<I::ABISpec>,

    /// clobber meanings some hidden interruption on liveness range of a variable.
    clobbers: FxHashMap<u32, PRegSet>,

    /// Array of operands.
    operands: Vec<Operand>,

    /// Reverse Post-order of dominate tree.
    block_order: BlockLoweringOrder,

    /// A ranges map the instruction to operands.
    operands_range: Ranges,

    /// A range map the block to instructions
    block_range: Ranges,

    /// Array of each block's successors.
    block_succ: Vec<Block>,
    block_succ_range: Ranges,

    /// Array of each block's predecessors.
    block_pred: Vec<Block>,
    block_pred_range: Ranges,

    block_params: Vec<VReg>,
    block_params_range: Ranges,

    branch_block_args: Vec<VReg>,
    branch_block_arg_range: Ranges,
    branch_block_arg_succ_range: Ranges,

    inst_is_branch: Vec<bool>,
    inst_is_ret: Vec<bool>,
}

impl<I: VCodeInst> VCodeContainer<I> {
    pub fn new(abi: CalleeABI<I::ABISpec>, block_order: BlockLoweringOrder) -> VCodeContainer<I> {
        VCodeContainer {
            insts: Vec::new(),
            vreg_types: Vec::new(),
            abi,
            block_order,
            clobbers: FxHashMap::default(),
            operands: Vec::new(),
            operands_range: Ranges::default(),
            block_range: Ranges::default(),
            block_succ: Vec::new(),
            block_succ_range: Ranges::default(),
            block_pred: Vec::new(),
            block_pred_range: Ranges::default(),
            block_params: Vec::new(),
            block_params_range: Ranges::default(),
            branch_block_args: Vec::new(),
            branch_block_arg_range: Ranges::default(),
            branch_block_arg_succ_range: Ranges::default(),
            inst_is_branch: Vec::new(),
            inst_is_ret: Vec::new(),
        }
    }

    pub fn write_back_allocs(&mut self, output: &Output) {
        for i in 0..self.insts.len() {
            let allocs = output.inst_allocs(i as u32);
            let mut writer = OperandWriter::new(allocs);
            self.insts[i].get_operands(&mut writer);
        }
    }

    /// Materialize allocator edit-moves into the instruction stream and legalize
    /// every frame-dependent pseudo-addressing mode in-place.
    ///
    /// After this call:
    /// - Every instruction in `self.insts` has a concrete, encodable addressing
    ///   mode (no `FrameSlot`, `SpOffset`, `IncomingArg`, or `OutgoingArg`).
    /// - Allocator `Edit::Move`s from `output.edits` have been expanded into real
    ///   move/spill/reload instructions interleaved at their program points.
    /// - The `output` is fully consumed; the emitter no longer needs it.
    /// - The `operands`, `operands_range`, and `clobbers` tables are cleared
    ///   (they were only needed for register allocation).
    ///
    /// Block structure (block ranges, successors, predecessors, params) is
    /// preserved; only the per-block instruction count may grow.
    pub fn finalize_for_emission(&mut self, output: &Output) {
        use crate::types::{F32, I32, I64, V4I32};

        let frame = self.abi.frame_layout().clone();
        let spill_unit_bytes = self.abi.spill_unit_bytes();
        let num_blocks = self.block_range.len();

        let mut new_insts: Vec<I> = Vec::new();
        let mut new_block_range = Ranges::default();

        // Resolve the width of an allocator edit's value from the vreg type
        // table. Vector-class vregs are normally 128-bit vector values, but
        // on AArch64 f32 scalars also allocate from the Vector bank (sN ≡ vN),
        // so the recorded type must win: an F32-typed vreg yields a 32-bit
        // move/spill rather than a 128-bit one. Fall back to a 128-bit vector
        // default so edits never panic even if the type was not recorded.
        let vector_ty = |vreg: Option<u32>| {
            vreg.and_then(|v| self.vreg_types.get(v as usize))
                .copied()
                .unwrap_or(V4I32)
        };

        for block_index in 0..num_blocks {
            let block_start = new_insts.len();

            for item in output.block_insts_and_edits(self, Block::new(block_index)) {
                match item {
                    InstOrEdit::Inst(inst_idx) => {
                        let inst = self.insts[inst_idx.index()].clone();
                        let legalized = I::ABISpec::legalize_inst(&frame, inst);
                        for inst in legalized {
                            new_insts.push(inst);
                        }
                    }
                    InstOrEdit::Edit(edit) => {
                        let Edit::Move {
                            from,
                            to,
                            class,
                            vreg,
                        } = edit;
                        match (from.as_reg(), to.as_reg()) {
                            (Some(from_reg), Some(to_reg)) => {
                                let ty = match class {
                                    RegClass::Float => F32,
                                    RegClass::Int => {
                                        // Size register moves from the value's
                                        // actual type: an i32 copy is a
                                        // `mov w, w` (clears the upper half).
                                        let vreg_ty =
                                            vreg.and_then(|v| self.vreg_types.get(v as usize));
                                        match vreg_ty {
                                            Some(&I32) => I32,
                                            _ => I64,
                                        }
                                    }
                                    RegClass::Vector => vector_ty(*vreg),
                                };
                                let mv = I::ABISpec::gen_move(
                                    Reg::from_physical_reg(from_reg),
                                    Reg::from_physical_reg(to_reg),
                                    ty,
                                );
                                for inst in I::ABISpec::legalize_inst(&frame, mv) {
                                    new_insts.push(inst);
                                }
                            }
                            (Some(from_reg), None) => {
                                let slot = to.as_stack().unwrap();
                                let offset = frame.spill_slot_offset(slot, spill_unit_bytes);
                                let ty = match class {
                                    RegClass::Float => F32,
                                    RegClass::Vector => vector_ty(*vreg),
                                    _ => I64,
                                };
                                for inst in I::ABISpec::gen_spill_store_at_sp(
                                    Reg::from_physical_reg(from_reg),
                                    offset,
                                    ty,
                                ) {
                                    for inst in I::ABISpec::legalize_inst(&frame, inst) {
                                        new_insts.push(inst);
                                    }
                                }
                            }
                            (None, Some(to_reg)) => {
                                let slot = from.as_stack().unwrap();
                                let offset = frame.spill_slot_offset(slot, spill_unit_bytes);
                                let ty = match class {
                                    RegClass::Float => F32,
                                    RegClass::Vector => vector_ty(*vreg),
                                    _ => I64,
                                };
                                for inst in I::ABISpec::gen_spill_load_at_sp(
                                    offset,
                                    Writable::from_reg(Reg::from_physical_reg(to_reg)),
                                    ty,
                                ) {
                                    for inst in I::ABISpec::legalize_inst(&frame, inst) {
                                        new_insts.push(inst);
                                    }
                                }
                            }
                            (None, None) => {
                                let from_slot = from.as_stack().unwrap();
                                let to_slot = to.as_stack().unwrap();
                                let from_offset =
                                    frame.spill_slot_offset(from_slot, spill_unit_bytes);
                                let to_offset = frame.spill_slot_offset(to_slot, spill_unit_bytes);
                                // A 128-bit vector spill occupies two 8-byte
                                // units; copy both halves when the target's
                                // stack-to-stack move is word-width only.
                                let moves: SmallVec<[I; 8]> = if *class == RegClass::Vector {
                                    I::ABISpec::gen_stack_to_stack_move(from_offset, to_offset)
                                        .into_iter()
                                        .chain(I::ABISpec::gen_stack_to_stack_move(
                                            from_offset + 8,
                                            to_offset + 8,
                                        ))
                                        .collect()
                                } else {
                                    I::ABISpec::gen_stack_to_stack_move(from_offset, to_offset)
                                        .into_iter()
                                        .collect()
                                };
                                for inst in moves {
                                    for inst in I::ABISpec::legalize_inst(&frame, inst) {
                                        new_insts.push(inst);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            new_block_range.push_end(new_insts.len());
            let _ = block_start;
        }

        let mut new_inst_is_branch = Vec::with_capacity(new_insts.len());
        let mut new_inst_is_ret = Vec::with_capacity(new_insts.len());
        for inst in &new_insts {
            let term = inst.is_term();
            new_inst_is_branch.push(matches!(
                term,
                MachTerminator::Branch | MachTerminator::TailReturn
            ));
            new_inst_is_ret.push(matches!(term, MachTerminator::Return));
        }

        self.insts = new_insts;
        self.block_range = new_block_range;
        self.inst_is_branch = new_inst_is_branch;
        self.inst_is_ret = new_inst_is_ret;

        // The operand tables and clobber map were only needed for register
        // allocation. Clear them so stale data can't confuse later passes.
        self.operands.clear();
        self.operands_range = Ranges::default();
        self.clobbers.clear();
    }

    /// Check allocator output before applying it to mutable machine instructions.
    pub fn verify_alloc_output(&self, output: &Output) -> Result<(), String> {
        if output.inst_alloc_offsets.len() != self.insts.len() {
            return Err(format!(
                "allocator output has {} instruction offsets for {} instructions",
                output.inst_alloc_offsets.len(),
                self.insts.len()
            ));
        }

        let mut previous = 0;
        for (inst_index, &offset) in output.inst_alloc_offsets.iter().enumerate() {
            let offset = offset as usize;
            if offset < previous || offset > output.allocs.len() {
                return Err(format!(
                    "allocator output offset {offset} for instruction {inst_index} is outside the allocation table"
                ));
            }
            previous = offset;
        }

        for inst_index in 0..self.insts.len() {
            let allocations = output.inst_allocs(inst_index as u32);
            let operands = self.inst_operands(Inst::new(inst_index));
            if allocations.len() != operands.len() {
                return Err(format!(
                    "allocator output has {} locations for instruction {inst_index}, expected {}",
                    allocations.len(),
                    operands.len()
                ));
            }

            for (operand_index, (&operand, &allocation)) in
                operands.iter().zip(allocations).enumerate()
            {
                if operand.as_fixed_nonallocatable().is_some() {
                    if !allocation.is_none() {
                        return Err(format!(
                            "allocator assigned {allocation} to fixed non-allocatable operand {operand_index} ({operand}) of instruction {inst_index}"
                        ));
                    }
                    continue;
                }

                if allocation.is_none() {
                    return Err(format!(
                        "allocator left operand {operand_index} ({operand}) of instruction {inst_index} unresolved"
                    ));
                }
                if let Some(preg) = allocation.as_reg() {
                    if preg.class() != operand.class() {
                        return Err(format!(
                            "allocator assigned register {preg} to operand {operand_index} ({operand}) of instruction {inst_index}"
                        ));
                    }
                }
                if allocation.kind() == AllocationKind::Stack
                    && allocation.index() >= output.num_spillslots
                {
                    return Err(format!(
                        "allocator assigned out-of-range spill slot {} to operand {operand_index} ({operand}) of instruction {inst_index}; spill-slot count is {}",
                        allocation.index(),
                        output.num_spillslots
                    ));
                }

                match operand.constraint() {
                    OperandConstraint::Any => {}
                    OperandConstraint::Reg if !allocation.is_reg() => {
                        return Err(format!(
                            "allocator assigned {allocation} to register operand {operand_index} ({operand}) of instruction {inst_index}"
                        ));
                    }
                    OperandConstraint::Stack if !allocation.is_stack() => {
                        return Err(format!(
                            "allocator assigned {allocation} to stack operand {operand_index} ({operand}) of instruction {inst_index}"
                        ));
                    }
                    OperandConstraint::FixedReg(preg) if allocation.as_reg() != Some(preg) => {
                        return Err(format!(
                            "allocator assigned {allocation} to fixed register operand {operand_index} ({operand}) of instruction {inst_index}"
                        ));
                    }
                    OperandConstraint::Limit(limit)
                        if !allocation.is_reg()
                            || allocation.as_reg().unwrap().hw_enc() >= limit =>
                    {
                        return Err(format!(
                            "allocator assigned {allocation} outside the register limit to operand {operand_index} ({operand}) of instruction {inst_index}"
                        ));
                    }
                    OperandConstraint::Reuse(reuse_index) => {
                        let Some(reused) = allocations.get(reuse_index).copied() else {
                            return Err(format!(
                                "allocator reuse operand {operand_index} ({operand}) references missing operand {reuse_index} of instruction {inst_index}"
                            ));
                        };
                        if !allocation.is_reg() || allocation != reused {
                            return Err(format!(
                                "allocator assigned {allocation} to reuse operand {operand_index} ({operand}) of instruction {inst_index}; expected register {reused}"
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }

        let mut previous_point = None;
        for (edit_index, (point, edit)) in output.edits.iter().enumerate() {
            if point.inst() as usize >= self.insts.len() {
                return Err(format!(
                    "allocator edit {edit_index} references instruction {} outside the function",
                    point.inst()
                ));
            }
            if previous_point.is_some_and(|previous| previous > *point) {
                return Err(format!(
                    "allocator edit {edit_index} at {point:?} is out of program-point order"
                ));
            }
            previous_point = Some(*point);

            let Edit::Move {
                from, to, class, ..
            } = edit;
            if from.is_none() || to.is_none() {
                return Err(format!(
                    "allocator edit {edit_index} at {point:?} has an unresolved endpoint: {from} -> {to}"
                ));
            }
            for allocation in [*from, *to] {
                if allocation.is_stack() && allocation.index() >= output.num_spillslots {
                    return Err(format!(
                        "allocator edit {edit_index} at {point:?} references out-of-range spill slot {} in {from} -> {to}; spill-slot count is {}",
                        allocation.index(),
                        output.num_spillslots
                    ));
                }
            }
            if let (Some(from_reg), Some(to_reg)) = (from.as_reg(), to.as_reg()) {
                if from_reg.class() != *class || to_reg.class() != *class {
                    return Err(format!(
                        "allocator edit {edit_index} at {point:?} crosses register classes: {from} -> {to}"
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn inst(&self, i: usize) -> &I {
        &self.insts[i]
    }

    /// Mutable access to a single instruction. Used by MIR passes that rewrite
    /// instructions in place (e.g. peephole combine).
    pub fn inst_mut(&mut self, i: usize) -> &mut I {
        &mut self.insts[i]
    }

    /// Mutable access to the whole flattened instruction stream. Used by MIR
    /// passes that rewrite many instructions in place (e.g. dead code
    /// elimination). Passes that change any instruction's operand structure
    /// must let the pipeline trigger `rebuild_operand_tables` afterwards.
    pub fn insts_mut(&mut self) -> &mut [I] {
        &mut self.insts
    }

    /// Replace the whole instruction stream and block ranges together. Used by
    /// MIR passes that move instructions between blocks (e.g. hoisting
    /// loop-invariant constants to a preheader): the two tables must stay
    /// consistent, so they are updated atomically. Callers must follow up with
    /// `rebuild_operand_tables` so the per-instruction operand/clobber tables
    /// match the new indices.
    pub fn set_insts_and_block_range(&mut self, insts: Vec<I>, block_range: Ranges) {
        debug_assert_eq!(insts.len(), block_range.get(block_range.len() - 1).end);
        self.insts = insts;
        self.block_range = block_range;
    }

    /// Rewrite every branch block argument equal to `from` to `to`. Branch
    /// arguments are vreg uses that live in the CFG side table rather than in
    /// any instruction's operand list, so passes that redirect a vreg (e.g.
    /// constant CSE) must update them too.
    pub fn rewrite_branch_block_args(&mut self, from: Reg, to: Reg) {
        let from_vreg = VReg::from(from);
        let to_vreg = VReg::from(to);
        for arg in self.branch_block_args.iter_mut() {
            if *arg == from_vreg {
                *arg = to_vreg;
            }
        }
    }

    /// All branch block arguments of the function. These are virtual-register
    /// uses that live in the CFG side tables and never appear in any
    /// instruction's operand list; MIR passes computing liveness or use
    /// counts must treat them as uses.
    pub fn branch_block_args(&self) -> &[VReg] {
        &self.branch_block_args
    }

    /// Update the per-instruction terminator flags after reordering or
    /// rewriting instructions. Used by post-RA passes (e.g. list scheduler).
    pub fn set_inst_terminator(&mut self, i: usize, term: MachTerminator) {
        self.inst_is_branch[i] =
            matches!(term, MachTerminator::Branch | MachTerminator::TailReturn);
        self.inst_is_ret[i] = matches!(term, MachTerminator::Return);
    }

    /// Returns the instruction slice for a block, indexed in lowered-order.
    pub fn block_insts(&self, block_index: usize) -> &[I] {
        let range = self.block_range.get(block_index);
        &self.insts[range]
    }

    /// Number of basic blocks in lowered order.
    pub fn num_blocks(&self) -> usize {
        self.block_range.len()
    }

    /// Instruction index range `[start..end)` for a given block.
    pub fn block_inst_range(&self, block_index: usize) -> core::ops::Range<usize> {
        self.block_range.get(block_index)
    }

    /// Total instruction count.
    pub fn num_insts(&self) -> usize {
        self.insts.len()
    }

    /// Rebuild operand tables, terminator metadata, and clobber maps from the
    /// current instruction stream. Pre-RA passes that mutate instruction
    /// operand structures (e.g. peephole combine) must call this before
    /// register allocation so that `operands`/`operands_range`/`clobbers`
    /// reflect the post-pass instruction stream.
    pub fn rebuild_operand_tables(&mut self) {
        self.operands.clear();
        self.operands_range = Ranges::default();
        self.clobbers.clear();

        let allocatable = PRegSet::from(self.abi.machine_env());
        let num_insts = self.insts.len();
        self.inst_is_branch = vec![false; num_insts];
        self.inst_is_ret = vec![false; num_insts];

        for (i, inst) in self.insts.iter_mut().enumerate() {
            match inst.is_term() {
                MachTerminator::Branch | MachTerminator::TailReturn => {
                    self.inst_is_branch[i] = true;
                }
                MachTerminator::Return => {
                    self.inst_is_ret[i] = true;
                }
                MachTerminator::None => {}
            }
            let mut op_collector =
                OperandCollector::new(&mut self.operands, allocatable, |vreg| vreg);
            inst.get_operands(&mut op_collector);
            let (ops, clobbers) = op_collector.finish();
            self.operands_range.push_end(ops);
            if clobbers != PRegSet::default() {
                self.clobbers.insert(i as u32, clobbers);
            }
        }
    }

    pub fn block_order(&self) -> &BlockLoweringOrder {
        &self.block_order
    }

    /// Validate VCode metadata and target instruction invariants at a pipeline
    /// boundary. Errors name both the stage and the affected VCode entity.
    pub fn verify(&self, stage: &str) -> Result<(), String> {
        let blocks = self.block_range.len();
        let fail = |detail: String| Err(format!("VCode verification failed at {stage}: {detail}"));
        if self.block_succ_range.len() != blocks
            || self.block_pred_range.len() != blocks
            || self.block_params_range.len() != blocks
            || self.branch_block_arg_succ_range.len() != blocks
        {
            return fail(format!(
                "block metadata lengths differ (blocks={blocks}, succ={}, pred={}, params={}, branch-args={})",
                self.block_succ_range.len(),
                self.block_pred_range.len(),
                self.block_params_range.len(),
                self.branch_block_arg_succ_range.len()
            ));
        }
        if self.branch_block_arg_range.len() != self.block_succ.len() {
            return fail(format!(
                "edge argument metadata differs from successor count (argument-ranges={}, successors={})",
                self.branch_block_arg_range.len(),
                self.block_succ.len()
            ));
        }
        // After finalize_for_emission, the operand tables are intentionally
        // cleared (they were only needed for register allocation). Accept that
        // state as long as both operands and operands_range are empty.
        let operands_finalized = self.operands.is_empty() && self.operands_range.is_empty();
        if (!operands_finalized && self.operands_range.len() != self.insts.len())
            || self.inst_is_branch.len() != self.insts.len()
            || self.inst_is_ret.len() != self.insts.len()
        {
            return fail(format!(
                "instruction metadata lengths differ (insts={}, operands={}, branches={}, returns={})",
                self.insts.len(),
                self.operands_range.len(),
                self.inst_is_branch.len(),
                self.inst_is_ret.len()
            ));
        }

        for (inst_index, inst) in self.insts.iter().enumerate() {
            if let Err(error) = inst.verify() {
                return fail(format!("instruction {inst_index} {inst:?}: {error}"));
            }
            let term = inst.is_term();
            if self.inst_is_branch[inst_index]
                != matches!(term, MachTerminator::Branch | MachTerminator::TailReturn)
                || self.inst_is_ret[inst_index] != matches!(term, MachTerminator::Return)
            {
                return fail(format!(
                    "instruction {inst_index} terminator metadata disagrees with {term:?}"
                ));
            }
            if !operands_finalized {
                log::trace!(
                    target: "taki_mir::verify",
                    "stage={stage} inst={inst_index} operands={:?} instruction={inst:?} clobbers={:?}",
                    &self.operands[self.operands_range.get(inst_index)],
                    self.clobbers.get(&(inst_index as u32))
                );
            } else {
                log::trace!(
                    target: "taki_mir::verify",
                    "stage={stage} inst={inst_index} instruction={inst:?}",
                );
            }
        }

        for block_index in 0..blocks {
            let block = Block::new(block_index);
            let insts = self.block_range.get(block_index);
            let succs = &self.block_succ[self.block_succ_range.get(block_index)];
            let preds = &self.block_pred[self.block_pred_range.get(block_index)];
            let params = &self.block_params[self.block_params_range.get(block_index)];
            for &succ in succs {
                if !succ.is_valid() || succ.index() >= blocks {
                    return fail(format!(
                        "block {block_index} has invalid successor {succ:?}"
                    ));
                }
                if !self.block_preds(succ).contains(&block) {
                    return fail(format!(
                        "block {block_index} -> {} is absent from successor predecessors",
                        succ.index()
                    ));
                }
            }
            for &pred in preds {
                if !pred.is_valid()
                    || pred.index() >= blocks
                    || !self.block_succs(pred).contains(&block)
                {
                    return fail(format!(
                        "block {block_index} has inconsistent predecessor {pred:?}"
                    ));
                }
            }
            let arg_entries = self.branch_block_arg_succ_range.get(block_index);
            if arg_entries.len() != succs.len() {
                return fail(format!(
                    "block {block_index} has {} successors but {} argument lists",
                    succs.len(),
                    arg_entries.len()
                ));
            }
            for (succ_index, &succ) in succs.iter().enumerate() {
                let args = &self.branch_block_args[self
                    .branch_block_arg_range
                    .get(arg_entries.start + succ_index)];
                let target_params = self.block_params(succ);
                if args.len() != target_params.len() {
                    return fail(format!(
                        "block {block_index} edge {succ_index} -> {} has {} args for {} params",
                        succ.index(),
                        args.len(),
                        target_params.len()
                    ));
                }
                if let Some((arg, param)) = args
                    .iter()
                    .zip(target_params)
                    .find(|(arg, param)| arg.class() != param.class())
                {
                    return fail(format!(
                        "block {block_index} edge {succ_index} -> {} class mismatch: {arg:?} -> {param:?}",
                        succ.index()
                    ));
                }
            }
            let terminator = insts.end.checked_sub(1).map(|index| &self.insts[index]);
            for inst_index in insts.start..insts.end.saturating_sub(1) {
                if self.insts[inst_index].is_term() != MachTerminator::None {
                    return fail(format!(
                        "block {block_index} has a non-final terminator at instruction {inst_index}"
                    ));
                }
            }
            match (succs.is_empty(), terminator.map(|inst| inst.is_term())) {
                (false, Some(MachTerminator::Branch)) => {}
                (true, Some(MachTerminator::Return | MachTerminator::TailReturn)) => {}
                (false, term) => {
                    return fail(format!(
                        "block {block_index} has successors but terminal instruction is {term:?}"
                    ));
                }
                (true, term) => {
                    return fail(format!(
                        "block {block_index} has no successors but terminal instruction is {term:?}"
                    ));
                }
            }
            log::debug!(target: "taki_mir::verify", "stage={stage} block={block_index} insts={insts:?} succs={succs:?} params={params:?}");
        }
        // SSA verification requires operand tables, which are cleared after
        // finalize_for_emission. Skip it in the post-finalize state.
        if !operands_finalized {
            self.verify_strict_ssa(stage)?;
        }
        log::debug!(target: "taki_mir::verify", "stage={stage} passed: blocks={blocks}, insts={}, vregs={}, operands={}", self.insts.len(), self.vreg_types.len(), self.operands.len());
        Ok(())
    }

    /// Validate the SSA form required by Ion. Block parameters define values at
    /// block entry; all other definitions are instruction operands.
    fn verify_strict_ssa(&self, stage: &str) -> Result<(), String> {
        #[derive(Clone, Copy, Debug)]
        enum DefLoc {
            BlockEntry(Block),
            Inst(Block, usize),
        }

        let blocks = self.num_blocks();
        let fail = |detail: String| {
            Err(format!(
                "VCode SSA verification failed at {stage}: {detail}"
            ))
        };
        if blocks == 0 {
            return fail("function has no entry block".to_owned());
        }

        let mut inst_blocks = vec![Block::invalid(); self.insts.len()];
        for block_index in 0..blocks {
            let block = Block::new(block_index);
            for inst_index in self.block_range.get(block_index) {
                if !inst_blocks[inst_index].is_valid() {
                    inst_blocks[inst_index] = block;
                } else {
                    return fail(format!(
                        "instruction {inst_index} belongs to multiple blocks"
                    ));
                }
            }
        }
        if let Some((inst_index, _)) = inst_blocks
            .iter()
            .enumerate()
            .find(|(_, block)| !block.is_valid())
        {
            return fail(format!("instruction {inst_index} belongs to no block"));
        }

        // Compute dominators over the VCode CFG. Multiple successor entries to
        // the same block remain distinct elsewhere; dominance only needs sets.
        let all_blocks: Vec<bool> = vec![true; blocks];
        let mut dominates = vec![all_blocks; blocks];
        dominates[0] = (0..blocks).map(|index| index == 0).collect();
        let mut changed = true;
        while changed {
            changed = false;
            for block_index in 1..blocks {
                let preds = self.block_preds(Block::new(block_index));
                if preds.is_empty() {
                    return fail(format!("non-entry block {block_index} has no predecessors"));
                }
                let mut next = vec![true; blocks];
                for pred in preds {
                    for (index, value) in next.iter_mut().enumerate() {
                        *value &= dominates[pred.index()][index];
                    }
                }
                next[block_index] = true;
                if next != dominates[block_index] {
                    dominates[block_index] = next;
                    changed = true;
                }
            }
        }

        let mut defs: FxHashMap<VReg, DefLoc> = FxHashMap::default();
        for block_index in 0..blocks {
            let block = Block::new(block_index);
            for &vreg in self.block_params(block) {
                if block_index == 0 {
                    return fail(format!("entry block has live-in block parameter {vreg}"));
                }
                if let Some(previous) = defs.insert(vreg, DefLoc::BlockEntry(block)) {
                    return fail(format!(
                        "VReg {vreg} has more than one definition: {previous:?} and block entry {block_index}"
                    ));
                }
            }
        }
        for (inst_index, operands) in
            (0..self.insts.len()).map(|index| (index, self.inst_operands(Inst::new(index))))
        {
            let block = inst_blocks[inst_index];
            for &operand in operands {
                if operand.kind() == OperandKind::Def {
                    if let Some(previous) =
                        defs.insert(operand.vreg(), DefLoc::Inst(block, inst_index))
                    {
                        return fail(format!(
                            "VReg {} has more than one definition: {previous:?} and instruction {inst_index} in block {} ({:?})",
                            operand.vreg(),
                            block.index(),
                            self.insts[inst_index]
                        ));
                    }
                }
            }
        }

        let dominates_use = |vreg: VReg, use_block: Block, use_inst: usize| match defs.get(&vreg) {
            Some(DefLoc::BlockEntry(def_block)) => dominates[use_block.index()][def_block.index()],
            Some(DefLoc::Inst(def_block, def_inst)) if *def_block == use_block => {
                *def_inst < use_inst
            }
            Some(DefLoc::Inst(def_block, _)) => dominates[use_block.index()][def_block.index()],
            None => false,
        };
        for (inst_index, operands) in
            (0..self.insts.len()).map(|index| (index, self.inst_operands(Inst::new(index))))
        {
            let block = inst_blocks[inst_index];
            for &operand in operands {
                if operand.kind() == OperandKind::Use
                    && !dominates_use(operand.vreg(), block, inst_index)
                {
                    return fail(format!(
                        "use of {} by instruction {inst_index} in block {} has no dominating definition",
                        operand.vreg(),
                        block.index()
                    ));
                }
            }
        }
        for block_index in 0..blocks {
            let block = Block::new(block_index);
            let terminator = self.block_range.get(block_index).end - 1;
            for succ_index in 0..self.block_succs(block).len() {
                let args = self.branch_blockparams(block, Inst::new(terminator), succ_index);
                for &vreg in args {
                    if !dominates_use(vreg, block, terminator) {
                        let inst_range = self.block_range.get(block_index);
                        return fail(format!(
                            "edge {block_index}:{succ_index} arguments {args:?} use {vreg} without a definition dominating its terminator; definition is {:?}; block instructions are {:?}",
                            defs.get(&vreg),
                            &self.insts[inst_range]
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

impl<I: VCodeInst> Function for VCodeContainer<I> {
    fn num_insts(&self) -> usize {
        self.insts.len()
    }

    fn num_blocks(&self) -> usize {
        self.block_range.len()
    }

    fn entry_block(&self) -> Block {
        Block::new(0)
    }

    fn block_insns(&self, block: Block) -> InstRange {
        let range = self.block_range.get(block.index());
        InstRange::new(Inst::new(range.start), Inst::new(range.end))
    }

    fn block_succs(&self, block: Block) -> &[Block] {
        let range = self.block_succ_range.get(block.index());
        &self.block_succ[range]
    }

    fn block_preds(&self, block: Block) -> &[Block] {
        let range = self.block_pred_range.get(block.index());
        &self.block_pred[range]
    }

    fn block_params(&self, block: Block) -> &[VReg] {
        let range = self.block_params_range.get(block.index());
        &self.block_params[range]
    }

    fn is_ret(&self, insn: Inst) -> bool {
        self.inst_is_ret.get(insn.index()).copied().unwrap_or(false)
    }

    fn is_branch(&self, insn: Inst) -> bool {
        self.inst_is_branch
            .get(insn.index())
            .copied()
            .unwrap_or(false)
    }

    fn branch_blockparams(&self, block: Block, _insn: Inst, succ_idx: usize) -> &[VReg] {
        let succ_range = self.branch_block_arg_succ_range.get(block.index());
        let succ_entry = succ_range.start + succ_idx;
        if succ_entry >= succ_range.end {
            return &[];
        }
        let arg_range = self.branch_block_arg_range.get(succ_entry);
        &self.branch_block_args[arg_range]
    }

    fn inst_operands(&self, insn: Inst) -> &[Operand] {
        let range = self.operands_range.get(insn.index());
        &self.operands[range]
    }

    fn inst_clobbers(&self, insn: Inst) -> PRegSet {
        self.clobbers
            .get(&(insn.index() as u32))
            .copied()
            .unwrap_or_default()
    }

    fn num_vregs(&self) -> usize {
        self.vreg_types.len()
    }

    fn spillslot_size(&self, regclass: RegClass) -> usize {
        self.abi.spillslot_size(regclass) as usize
    }
}

pub struct VCodeBuilder<I>
where
    I: VCodeInst,
{
    pub(crate) vcode: VCodeContainer<I>,
}

impl<I: VCodeInst> VCodeBuilder<I> {
    pub fn new(abi: CalleeABI<I::ABISpec>, block_order: BlockLoweringOrder) -> VCodeBuilder<I> {
        VCodeBuilder {
            vcode: VCodeContainer::new(abi, block_order),
        }
    }

    pub fn push(&mut self, inst: I) {
        self.vcode.insts.push(inst);
    }

    pub fn add_block_param(&mut self, param_reg: VReg) {
        self.vcode.block_params.push(param_reg);
    }

    pub fn block_order(&self) -> &BlockLoweringOrder {
        &self.vcode.block_order
    }

    pub fn add_succ(&mut self, block: MirBlockIndex, regs: &[Reg]) {
        self.vcode.block_succ.push(block);
        self.add_block_args_for_succ(regs);
    }

    pub fn add_block_args_for_succ(&mut self, regs: &[Reg]) {
        self.vcode
            .branch_block_args
            .extend(regs.iter().map(|reg| reg.to_virtual_reg().unwrap()));
        let end = self.vcode.branch_block_args.len();
        self.vcode.branch_block_arg_range.push_end(end);
    }

    pub fn end_bb(&mut self) {
        let inst_end = self.vcode.insts.len();
        self.vcode.block_range.push_end(inst_end);

        let succ_end = self.vcode.block_succ.len();
        self.vcode.block_succ_range.push_end(succ_end);

        let block_param_end = self.vcode.block_params.len();
        self.vcode.block_params_range.push_end(block_param_end);

        let branch_block_arg_range_end = self.vcode.branch_block_arg_range.len();
        self.vcode
            .branch_block_arg_succ_range
            .push_end(branch_block_arg_range_end);
    }

    pub fn build(mut self, mut vregs: VRegAllocator<I>) -> VCodeContainer<I> {
        self.vcode.vreg_types = std::mem::take(&mut vregs.vreg_types);

        self.reverse_and_finalize();

        self.collect_operands_and_terminators(&vregs);

        self.compute_preds_from_succs();

        // At this point, nothing in the vcode should mention any
        // VReg which has been aliased. All the appropriate rewriting
        // should have happened above. Just to be sure, let's
        // double-check each field which has vregs.
        // Note: can't easily check vcode.insts, resolved in collect_operands.
        // Operands are resolved in collect_operands.
        vregs.assert_no_vreg_aliases(self.vcode.operands.iter().map(|op| op.vreg()));
        vregs.assert_no_vreg_aliases(self.vcode.block_params.iter().copied());
        // Branch block args are resolved in collect_operands.
        vregs.assert_no_vreg_aliases(self.vcode.branch_block_args.iter().copied());

        self.vcode
    }

    pub fn collect_operands_and_terminators(&mut self, vregs: &VRegAllocator<I>) {
        let allocatable = PRegSet::from(self.vcode.abi.machine_env());
        let num_insts = self.vcode.insts.len();
        self.vcode.inst_is_branch = vec![false; num_insts];
        self.vcode.inst_is_ret = vec![false; num_insts];
        for (i, inst) in self.vcode.insts.iter_mut().enumerate() {
            match inst.is_term() {
                MachTerminator::Branch | MachTerminator::TailReturn => {
                    self.vcode.inst_is_branch[i] = true;
                }
                MachTerminator::Return => {
                    self.vcode.inst_is_ret[i] = true;
                }
                MachTerminator::None => {}
            }
            let mut op_collector =
                OperandCollector::new(&mut self.vcode.operands, allocatable, |vreg| {
                    vregs.resolve_alias(vreg)
                });
            inst.get_operands(&mut op_collector);
            let (ops, clobbers) = op_collector.finish();
            self.vcode.operands_range.push_end(ops);

            if clobbers != PRegSet::default() {
                self.vcode.clobbers.insert(i as u32, clobbers);
            }

            if let Some((dst, src)) = inst.is_move() {
                assert!(src.is_virtual());
                assert!(dst.to_reg().is_virtual());
            }
        }

        for arg in self.vcode.branch_block_args.iter_mut() {
            let new_arg = vregs.resolve_alias(*arg);
            *arg = new_arg;
        }
        for param in self.vcode.block_params.iter_mut() {
            let new_param = vregs.resolve_alias(*param);
            *param = new_param;
        }
    }

    fn compute_preds_from_succs(&mut self) {
        let mut starts = vec![0u32; self.vcode.block_range.len()];

        for succ in self.vcode.block_succ.iter() {
            starts[succ.index()] += 1;
        }

        self.vcode.block_pred_range.reserve(starts.len());
        let mut end = 0;
        for count in starts.iter_mut() {
            let start = end;
            end += *count;
            *count = start;
            self.vcode.block_pred_range.push_end(end as usize);
        }
        let end = end as usize;
        assert_eq!(end, self.vcode.block_succ.len());

        self.vcode.block_pred.resize(end, Block::invalid());
        for (pred, range) in self.vcode.block_succ_range.iter() {
            let pred = Block::new(pred);
            for succ in self.vcode.block_succ[range].iter() {
                let pos = &mut starts[succ.index()];
                self.vcode.block_pred[*pos as usize] = pred;
                *pos += 1;
            }
        }

        assert!(self.vcode.block_pred.iter().all(|pred| pred.is_valid()));
    }

    pub fn reverse_and_finalize(&mut self) {
        let n_insts = self.vcode.insts.len();
        if n_insts == 0 {
            return;
        }

        self.vcode.block_range.reverse_index();
        self.vcode.block_range.reverse_target(n_insts);

        self.vcode.block_params_range.reverse_index();
        self.vcode.block_succ_range.reverse_index();
        self.vcode.insts.reverse();
        self.vcode.branch_block_arg_succ_range.reverse_index();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        abi::{ABIMachineSpec, ArgPair, ArgSlot, CalleeABI, FrameLayout, StackAMode},
        prelude::{ArenaContext, HirBasicBlock, HirFunctionData, HirInst, HirType},
        reg_alloc::reg::{MachineEnv, OperandVisitorImpl, PReg},
        register::{Reg, VRegAllocator, Writable},
        types::{I64, LoweredType, V4I32},
    };
    use raana_ir::{
        ir::Program,
        opt::prelude::{BasicBlockBuilder, LocalInstBuilder},
    };

    #[derive(Clone, Debug)]
    enum TestInst {
        Ret,
        LoadImm {
            rd: Writable<Reg>,
        },
        Mov {
            src: Reg,
            dst: Writable<Reg>,
        },
        /// Fake sink: keeps every listed register live through the block end.
        UseAll {
            regs: Vec<Reg>,
        },
        /// Fake call: clobbers the given physical registers, forcing values
        /// live across it to spill (mirrors a real call site).
        Call {
            clobbers: PRegSet,
        },
        Jump,
        Nop,
    }

    struct TestABI;

    impl MachInst for TestInst {
        type ABISpec = TestABI;

        fn get_operands(&mut self, collector: &mut impl OperandVisitor) {
            match self {
                Self::LoadImm { rd } => collector.reg_def(rd),
                Self::Mov { src, dst } => {
                    collector.reg_use(src);
                    collector.reg_def(dst);
                }
                Self::UseAll { regs } => {
                    for reg in regs {
                        collector.reg_use(reg);
                    }
                }
                Self::Call { clobbers } => collector.reg_clobbers(*clobbers),
                Self::Ret | Self::Jump | Self::Nop => {}
            }
        }

        fn is_move(&self) -> Option<(Writable<Reg>, Reg)> {
            match self {
                Self::Mov { src, dst } => Some((*dst, *src)),
                _ => None,
            }
        }

        fn is_term(&self) -> MachTerminator {
            match self {
                Self::Ret => MachTerminator::Return,
                Self::Jump => MachTerminator::Branch,
                _ => MachTerminator::None,
            }
        }

        fn rc_for_type(ty: LoweredType) -> (&'static [RegClass], &'static [LoweredType]) {
            match ty {
                I64 => (&[RegClass::Int], &[I64]),
                V4I32 => (&[RegClass::Vector], &[V4I32]),
                _ => unreachable!(),
            }
        }

        fn gen_jump(_target: MirBlockIndex) -> Self {
            Self::Jump
        }
    }

    impl MachInstEmit for TestInst {
        fn emit(&self, _ctx: &mut dyn EmitContext) -> core::fmt::Result {
            Ok(())
        }
    }

    impl ABIMachineSpec for TestABI {
        type I = TestInst;

        fn stack_align() -> u32 {
            16
        }

        fn spillslot_size(regclass: RegClass) -> u32 {
            match regclass {
                // Vectors occupy two 8-byte spill units, mirroring AArch64.
                RegClass::Vector => 2,
                _ => 1,
            }
        }

        fn spill_unit_bytes() -> u32 {
            8
        }

        fn is_callee_saved(_preg: PReg) -> bool {
            false
        }

        fn gen_load_stack(_mem: StackAMode, dst: Writable<Reg>, _ty: LoweredType) -> Self::I {
            TestInst::LoadImm { rd: dst }
        }

        fn gen_load_imm(dst: Writable<Reg>, _value: u64, _ty: LoweredType) -> Self::I {
            TestInst::LoadImm { rd: dst }
        }

        fn gen_load_addr(dst: Writable<Reg>, _label: HirInst) -> Self::I {
            TestInst::LoadImm { rd: dst }
        }

        fn gen_get_stack_addr(_mem: StackAMode, dst: Writable<Reg>) -> Self::I {
            TestInst::LoadImm { rd: dst }
        }

        fn gen_args(_args: Vec<ArgPair>) -> Self::I {
            TestInst::Nop
        }

        fn gen_store_stack(_src: Reg, _mem: StackAMode, _ty: LoweredType) -> Self::I {
            TestInst::Nop
        }

        fn gen_move(src: Reg, dst: Reg, _ty: LoweredType) -> Self::I {
            TestInst::Mov {
                src,
                dst: Writable::from_reg(dst),
            }
        }

        fn compute_arg_loc(_arena: ArenaContext<'_>) -> (Vec<ArgSlot>, u32) {
            (vec![], 0)
        }

        fn compute_call_arg_loc(_types: &[HirType]) -> (Vec<ArgSlot>, u32) {
            (vec![], 0)
        }

        fn get_machine_env() -> &'static MachineEnv {
            static ENV: std::sync::LazyLock<MachineEnv> = std::sync::LazyLock::new(|| MachineEnv {
                preferred_regs_by_class: [
                    PRegSet::empty().with(PReg::new(0, RegClass::Int)),
                    PRegSet::empty().with(PReg::new(0, RegClass::Float)),
                    PRegSet::empty(),
                ],
                non_preferred_regs_by_class: [PRegSet::empty(); 3],
                scratch_by_class: [None; 3],
                post_ra_scratch_by_class: [vec![], vec![], vec![]],
                fixed_stack_slots: vec![],
            });
            &ENV
        }

        fn gen_prologue_frame_setup(_frame: &FrameLayout) -> smallvec::SmallVec<[Self::I; 16]> {
            smallvec::SmallVec::new()
        }

        fn gen_epilogue_frame_restore(_frame: &FrameLayout) -> smallvec::SmallVec<[Self::I; 16]> {
            smallvec::SmallVec::new()
        }

        fn gen_clobber_save(_frame: &FrameLayout) -> smallvec::SmallVec<[Self::I; 16]> {
            smallvec::SmallVec::new()
        }

        fn gen_clobber_restore(_frame: &FrameLayout) -> smallvec::SmallVec<[Self::I; 16]> {
            smallvec::SmallVec::new()
        }
    }

    fn add_block(data: &mut HirFunctionData, name: &str) {
        let block = data.new_basic_block().basic_block(name.to_owned(), vec![]);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().push_bb_back(block);
        data.layout_mut().insert_inst(block, ret);
    }

    fn empty_vcode() -> VCodeContainer<TestInst> {
        let mut program = Program::new();
        let func = program.new_function(HirType::get_unit(), "verify".to_owned(), vec![]);
        add_block(program.func_data_mut(func), "entry");
        let arena = ArenaContext {
            program: &program,
            curr_func: Some(func),
        };
        let abi = CalleeABI::<TestABI>::new(arena);
        let order = BlockLoweringOrder::new(arena);
        let mut builder = VCodeBuilder::new(abi, order);
        builder.push(TestInst::Ret);
        builder.end_bb();
        builder.build(VRegAllocator::with_capaticy(0))
    }

    fn vcode_with_integer_def() -> VCodeContainer<TestInst> {
        let mut program = Program::new();
        let func = program.new_function(HirType::get_unit(), "verify".to_owned(), vec![]);
        add_block(program.func_data_mut(func), "entry");
        let arena = ArenaContext {
            program: &program,
            curr_func: Some(func),
        };
        let abi = CalleeABI::<TestABI>::new(arena);
        let order = BlockLoweringOrder::new(arena);
        let mut builder = VCodeBuilder::new(abi, order);
        let mut vregs = VRegAllocator::with_capaticy(1);
        let dst = Writable::from_reg(vregs.alloc(I64));
        builder.push(TestInst::Ret);
        builder.push(TestInst::LoadImm { rd: dst });
        builder.end_bb();
        builder.build(vregs)
    }

    #[test]
    fn ion_allocation_output_verifies_for_basic_vcode() {
        let vcode = vcode_with_integer_def();
        let output = crate::reg_alloc::ion::run(&vcode, vcode.abi.machine_env())
            .expect("Ion allocation should succeed");

        assert!(vcode.verify_alloc_output(&output).is_ok());
    }

    #[test]
    fn take_args_returns_none_when_no_register_arguments_bound() {
        let mut program = Program::new();
        let func = program.new_function(HirType::get_unit(), "args".to_owned(), vec![]);
        add_block(program.func_data_mut(func), "entry");
        let arena = ArenaContext {
            program: &program,
            curr_func: Some(func),
        };
        let mut abi = CalleeABI::<TestABI>::new(arena);
        assert!(abi.take_args().is_none());
    }

    #[test]
    fn strict_ssa_verifier_rejects_duplicate_definitions() {
        let mut program = Program::new();
        let func = program.new_function(HirType::get_unit(), "verify".to_owned(), vec![]);
        add_block(program.func_data_mut(func), "entry");
        let arena = ArenaContext {
            program: &program,
            curr_func: Some(func),
        };
        let abi = CalleeABI::<TestABI>::new(arena);
        let order = BlockLoweringOrder::new(arena);
        let mut builder = VCodeBuilder::new(abi, order);
        let mut vregs = VRegAllocator::with_capaticy(1);
        let dst = Writable::from_reg(vregs.alloc(I64));
        builder.push(TestInst::Ret);
        builder.push(TestInst::LoadImm { rd: dst });
        builder.push(TestInst::LoadImm { rd: dst });
        builder.end_bb();

        let error = builder.build(vregs).verify("test").unwrap_err();
        assert!(error.contains("more than one definition"), "{error}");
    }

    #[test]
    fn strict_ssa_verifier_rejects_undefined_instruction_use() {
        let mut program = Program::new();
        let func = program.new_function(HirType::get_unit(), "verify".to_owned(), vec![]);
        add_block(program.func_data_mut(func), "entry");
        let arena = ArenaContext {
            program: &program,
            curr_func: Some(func),
        };
        let abi = CalleeABI::<TestABI>::new(arena);
        let order = BlockLoweringOrder::new(arena);
        let mut builder = VCodeBuilder::new(abi, order);
        let mut vregs = VRegAllocator::with_capaticy(2);
        let undefined = vregs.alloc(I64);
        let dst = Writable::from_reg(vregs.alloc(I64));
        builder.push(TestInst::Ret);
        builder.push(TestInst::Mov {
            src: undefined,
            dst,
        });
        builder.end_bb();

        let error = builder.build(vregs).verify("test").unwrap_err();
        assert!(error.contains("has no dominating definition"), "{error}");
    }

    #[test]
    fn strict_ssa_verifier_rejects_entry_block_parameters() {
        let mut program = Program::new();
        let func = program.new_function(HirType::get_unit(), "verify".to_owned(), vec![]);
        add_block(program.func_data_mut(func), "entry");
        let arena = ArenaContext {
            program: &program,
            curr_func: Some(func),
        };
        let abi = CalleeABI::<TestABI>::new(arena);
        let order = BlockLoweringOrder::new(arena);
        let mut builder = VCodeBuilder::new(abi, order);
        let mut vregs = VRegAllocator::with_capaticy(1);
        builder.push(TestInst::Ret);
        builder.add_block_param(vregs.alloc(I64).into());
        builder.end_bb();

        let error = builder.build(vregs).verify("test").unwrap_err();
        assert!(
            error.contains("entry block has live-in block parameter"),
            "{error}"
        );
    }

    #[test]
    fn strict_ssa_verifier_rejects_undefined_edge_argument() {
        let mut program = Program::new();
        let func = program.new_function(HirType::get_unit(), "verify".to_owned(), vec![]);
        add_block(program.func_data_mut(func), "entry");
        let arena = ArenaContext {
            program: &program,
            curr_func: Some(func),
        };
        let abi = CalleeABI::<TestABI>::new(arena);
        let order = BlockLoweringOrder::new(arena);
        let mut builder = VCodeBuilder::new(abi, order);
        let mut vregs = VRegAllocator::with_capaticy(2);
        let param = vregs.alloc(I64);
        let undefined = vregs.alloc(I64);
        builder.push(TestInst::Ret);
        builder.add_block_param(param.into());
        builder.end_bb();
        builder.push(TestInst::gen_jump(Block::new(1)));
        builder.add_succ(Block::new(1), &[undefined]);
        builder.end_bb();

        let error = builder.build(vregs).verify("test").unwrap_err();
        assert!(
            error.contains("arguments") && error.contains("without a definition"),
            "{error}"
        );
    }

    #[test]
    fn vcode_verifier_rejects_non_final_terminator() {
        let mut vcode = empty_vcode();
        vcode.insts.insert(0, TestInst::Ret);
        vcode.operands_range = Ranges::default();
        vcode.operands_range.push_end(0);
        vcode.operands_range.push_end(0);
        vcode.inst_is_branch.insert(0, false);
        vcode.inst_is_ret.insert(0, true);
        vcode.block_range = Ranges::default();
        vcode.block_range.push_end(2);

        let error = vcode.verify("test").unwrap_err();
        assert!(error.contains("non-final terminator"), "{error}");
    }

    #[test]
    fn allocation_output_verifier_accepts_empty_return() {
        let vcode = empty_vcode();
        let output = Output {
            inst_alloc_offsets: vec![0],
            ..Output::default()
        };

        assert!(vcode.verify_alloc_output(&output).is_ok());
    }

    #[test]
    fn allocation_output_verifier_rejects_out_of_range_edit_slot() {
        let vcode = empty_vcode();
        let output = Output {
            inst_alloc_offsets: vec![0],
            edits: vec![(
                crate::reg_alloc::reg::ProgPoint::before(0),
                Edit::Move {
                    from: crate::reg_alloc::reg::Allocation::stack(
                        crate::reg_alloc::reg::SpillSlot::new(0),
                    ),
                    to: crate::reg_alloc::reg::Allocation::reg(PReg::new(5, RegClass::Int)),
                    class: RegClass::Int,
                    vreg: None,
                },
            )],
            ..Output::default()
        };

        let error = vcode.verify_alloc_output(&output).unwrap_err();
        assert!(error.contains("out-of-range spill slot"), "{error}");
    }

    #[test]
    fn allocation_output_verifier_accepts_vector_edit() {
        let vcode = empty_vcode();
        let output = Output {
            inst_alloc_offsets: vec![0],
            edits: vec![(
                crate::reg_alloc::reg::ProgPoint::before(0),
                Edit::Move {
                    from: crate::reg_alloc::reg::Allocation::stack(
                        crate::reg_alloc::reg::SpillSlot::new(0),
                    ),
                    to: crate::reg_alloc::reg::Allocation::stack(
                        crate::reg_alloc::reg::SpillSlot::new(0),
                    ),
                    class: RegClass::Vector,
                    vreg: None,
                },
            )],
            num_spillslots: 2,
            ..Output::default()
        };

        assert!(
            vcode.verify_alloc_output(&output).is_ok(),
            "vector spill edits must pass allocation-output verification"
        );
    }

    #[test]
    fn allocation_output_verifier_rejects_instruction_arity_mismatch() {
        let vcode = vcode_with_integer_def();
        let output = Output {
            inst_alloc_offsets: vec![0, 0],
            ..Output::default()
        };

        let error = vcode.verify_alloc_output(&output).unwrap_err();
        assert!(error.contains("expected 1"), "{error}");
    }

    #[test]
    fn allocation_output_verifier_rejects_register_class_mismatch() {
        let vcode = vcode_with_integer_def();
        let output = Output {
            inst_alloc_offsets: vec![0, 1],
            allocs: vec![crate::reg_alloc::reg::Allocation::reg(PReg::new(
                0,
                RegClass::Float,
            ))],
            ..Output::default()
        };

        let error = vcode.verify_alloc_output(&output).unwrap_err();
        assert!(error.contains("assigned register"), "{error}");
    }

    /// Build a single-block VCode with two vector values copied through
    /// register-register moves and then kept live across a call that clobbers
    /// every vector register. Mirrors the AArch64 machine-layer acceptance:
    /// vector values must complete register allocation (register/stack moves
    /// and spills) without panicking.
    fn vector_chain_vcode() -> VCodeContainer<TestInst> {
        let mut program = Program::new();
        let func = program.new_function(HirType::get_unit(), "vector".to_owned(), vec![]);
        add_block(program.func_data_mut(func), "entry");
        let arena = ArenaContext {
            program: &program,
            curr_func: Some(func),
        };
        let abi = CalleeABI::<TestABI>::new(arena);
        let order = BlockLoweringOrder::new(arena);
        let mut builder = VCodeBuilder::new(abi, order);
        let mut vregs = VRegAllocator::with_capaticy(4);

        let d0 = vregs.alloc(V4I32);
        let d1 = vregs.alloc(V4I32);
        let o0 = vregs.alloc(V4I32);
        let o1 = vregs.alloc(V4I32);
        let mut clobber_all = PRegSet::empty();
        for index in 0..4 {
            clobber_all = clobber_all.with(PReg::new(index, RegClass::Vector));
        }
        // Instructions are pushed in reverse of their final order (the block
        // terminator goes first). Final order: defs, moves, call, sink, ret.
        builder.push(TestInst::Ret);
        builder.push(TestInst::UseAll { regs: vec![o0, o1] });
        builder.push(TestInst::Call {
            clobbers: clobber_all,
        });
        for (src, dst) in [(d0, o0), (d1, o1)] {
            builder.push(TestInst::Mov {
                src,
                dst: Writable::from_reg(dst),
            });
        }
        builder.push(TestInst::LoadImm {
            rd: Writable::from_reg(d1),
        });
        builder.push(TestInst::LoadImm {
            rd: Writable::from_reg(d0),
        });
        builder.end_bb();
        builder.build(vregs)
    }

    #[test]
    fn vector_values_allocate_through_moves_and_spills() {
        let mut vcode = vector_chain_vcode();
        let mut preferred = PRegSet::empty();
        for index in 0..4 {
            preferred = preferred.with(PReg::new(index, RegClass::Vector));
        }
        let env = MachineEnv {
            preferred_regs_by_class: [
                PRegSet::empty().with(PReg::new(0, RegClass::Int)),
                PRegSet::empty().with(PReg::new(0, RegClass::Float)),
                preferred,
            ],
            non_preferred_regs_by_class: [PRegSet::empty(); 3],
            scratch_by_class: [None; 3],
            post_ra_scratch_by_class: [vec![], vec![], vec![]],
            fixed_stack_slots: vec![],
        };

        let output = crate::reg_alloc::ion::run(&vcode, &env)
            .expect("Ion allocation should succeed for vector values");
        assert!(
            output.num_spillslots >= 2,
            "vector values live across an all-vector clobber must spill, got {} slots",
            output.num_spillslots
        );
        assert!(
            output.edits.iter().any(|(_, edit)| matches!(
                edit,
                Edit::Move {
                    class: RegClass::Vector,
                    ..
                }
            )),
            "vector moves and spills must materialize as Vector-class edits"
        );
        assert!(vcode.verify_alloc_output(&output).is_ok());

        let spill_size =
            u32::try_from(output.num_spillslots).unwrap() * vcode.abi.spill_unit_bytes();
        vcode
            .abi
            .compute_frame_layout(spill_size, &output)
            .expect("frame layout should accept vector spill units");
        vcode.write_back_allocs(&output);
        vcode.finalize_for_emission(&output);
        // The finalized stream contains spill moves plus the original
        // instructions; it must be non-empty and never panic on Vector class.
        assert!(vcode.num_insts() > 0);
    }
}
