/*
 * Adapted for the taki_mir Ion port from the Function normalization required
 * by regalloc2 0.15.1's Ion allocator.
 *
 * Released under the Apache License 2.0 with LLVM Exception. See the
 * repository LICENSE for the full license text. This is a local VCode view,
 * not a regalloc2 compatibility wrapper.
 */

//! Dense allocator-local VReg numbering for Ion.

use rustc_hash::FxHashMap;

use crate::{
    reg_alloc::{
        function::Function,
        index::{Block, Inst, InstRange},
        reg::{Operand, PRegSet, RegClass, VReg},
    },
    register::pinned_vreg_to_preg,
};

/// An owned `Function` view whose VRegs are dense from zero.
///
/// VCode reserves low VReg indices for pinned physical registers. Ion uses a
/// compact VReg-indexed data model, so this view maps both ordinary and pinned
/// VRegs into a private dense space. Physical-register constraints remain on
/// operands; pinned VRegs are only dataflow values here.
pub struct DenseVRegFunction<'a, F> {
    function: &'a F,
    local_by_original: FxHashMap<VReg, VReg>,
    original_by_local: Vec<VReg>,
    operands: Vec<Vec<Operand>>,
    block_params: Vec<Vec<VReg>>,
    branch_args: Vec<Vec<Vec<VReg>>>,
}

impl<'a, F: Function> DenseVRegFunction<'a, F> {
    pub fn new(function: &'a F) -> Self {
        let mut view = Self {
            function,
            local_by_original: FxHashMap::default(),
            original_by_local: Vec::new(),
            operands: Vec::with_capacity(function.num_insts()),
            block_params: Vec::with_capacity(function.num_blocks()),
            branch_args: Vec::with_capacity(function.num_blocks()),
        };

        for index in 0..function.num_insts() {
            let operands = function
                .inst_operands(Inst::new(index))
                .iter()
                .copied()
                .map(|operand| view.normalize_operand(operand))
                .collect();
            view.operands.push(operands);
        }
        for index in 0..function.num_blocks() {
            let block = Block::new(index);
            let params = function
                .block_params(block)
                .iter()
                .copied()
                .map(|vreg| view.normalize_vreg(vreg))
                .collect();
            view.block_params.push(params);

            let args = function
                .block_succs(block)
                .iter()
                .enumerate()
                .map(|(succ_index, _)| {
                    let inst = function.block_insns(block).last();
                    function
                        .branch_blockparams(block, inst, succ_index)
                        .iter()
                        .copied()
                        .map(|vreg| view.normalize_vreg(vreg))
                        .collect()
                })
                .collect();
            view.branch_args.push(args);
        }
        view
    }

    fn normalize_operand(&mut self, operand: Operand) -> Operand {
        if operand.as_fixed_nonallocatable().is_some() {
            return operand;
        }
        Operand::new(
            self.normalize_vreg(operand.vreg()),
            operand.constraint(),
            operand.kind(),
            operand.pos(),
        )
    }

    fn normalize_vreg(&mut self, original: VReg) -> VReg {
        if let Some(&local) = self.local_by_original.get(&original) {
            return local;
        }
        let local = VReg::new(self.original_by_local.len(), original.class());
        self.local_by_original.insert(original, local);
        self.original_by_local.push(original);
        local
    }

    /// Returns the source VReg for an Ion-local VReg.
    pub fn original_vreg(&self, local: VReg) -> Option<VReg> {
        self.original_by_local.get(local.vreg()).copied()
    }

    /// Returns the Ion-local VReg for a source VReg, if the source appears in
    /// an operand, block parameter, or edge argument.
    pub fn local_vreg(&self, original: VReg) -> Option<VReg> {
        self.local_by_original.get(&original).copied()
    }

    /// Returns the pinned physical register represented by a local VReg.
    pub fn pinned_preg(&self, local: VReg) -> Option<crate::reg_alloc::reg::PReg> {
        self.original_vreg(local).and_then(pinned_vreg_to_preg)
    }
}

impl<F: Function> Function for DenseVRegFunction<'_, F> {
    fn num_insts(&self) -> usize {
        self.function.num_insts()
    }

    fn num_blocks(&self) -> usize {
        self.function.num_blocks()
    }

    fn entry_block(&self) -> Block {
        self.function.entry_block()
    }

    fn block_insns(&self, block: Block) -> InstRange {
        self.function.block_insns(block)
    }

    fn block_succs(&self, block: Block) -> &[Block] {
        self.function.block_succs(block)
    }

    fn block_preds(&self, block: Block) -> &[Block] {
        self.function.block_preds(block)
    }

    fn block_params(&self, block: Block) -> &[VReg] {
        &self.block_params[block.index()]
    }

    fn is_ret(&self, inst: Inst) -> bool {
        self.function.is_ret(inst)
    }

    fn is_branch(&self, inst: Inst) -> bool {
        self.function.is_branch(inst)
    }

    fn branch_blockparams(&self, block: Block, _inst: Inst, succ_idx: usize) -> &[VReg] {
        &self.branch_args[block.index()][succ_idx]
    }

    fn inst_operands(&self, inst: Inst) -> &[Operand] {
        &self.operands[inst.index()]
    }

    fn inst_clobbers(&self, inst: Inst) -> PRegSet {
        self.function.inst_clobbers(inst)
    }

    fn num_vregs(&self) -> usize {
        self.original_by_local.len()
    }

    fn spillslot_size(&self, regclass: RegClass) -> usize {
        self.function.spillslot_size(regclass)
    }

    fn multi_spillslot_named_by_last_slot(&self) -> bool {
        self.function.multi_spillslot_named_by_last_slot()
    }

    fn allow_multiple_vreg_defs(&self) -> bool {
        self.function.allow_multiple_vreg_defs()
    }
}

#[cfg(test)]
mod tests;
