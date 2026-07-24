use std::fmt::Debug;

use rustc_hash::FxHashMap;
use tomori_utils::Ranges;

use crate::{
    abi::{ABIMachineSpec, CalleeABI},
    block_order::{BlockLoweringOrder, MirBlockIndex},
    reg_alloc::{
        function::Function,
        index::{Block, Inst, InstRange},
        reg::{
            AllocationKind, Edit, Operand, OperandCollector, OperandConstraint, OperandKind,
            OperandVisitor, OperandWriter, Output, PRegSet, RegClass, VReg,
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

#[derive(Debug, PartialEq, Eq)]
pub enum CallType {
    /// Not a call
    None,
    /// Normal call
    Call,
    /// Tail Call
    TailCall,
}

pub trait MachInst: Clone + Debug {
    type ABISpec: ABIMachineSpec<I = Self>;

    fn get_operands(&mut self, collector: &mut impl OperandVisitor);

    // TODO: return a pair of register
    fn is_move(&self) -> Option<(Writable<Reg>, Reg)>;

    fn is_term(&self) -> MachTerminator;

    fn call_type(&self) -> CallType;

    fn is_mem_access(&self) -> bool;

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

            let Edit::Move { from, to, class } = edit;
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
            if *class == RegClass::Vector {
                return Err(format!(
                    "allocator edit {edit_index} at {point:?} uses unsupported vector spill semantics: {from} -> {to}"
                ));
            }
        }
        Ok(())
    }

    pub fn inst(&self, i: usize) -> &I {
        &self.insts[i]
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
        if self.operands_range.len() != self.insts.len()
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
            log::trace!(
                target: "taki_mir::verify",
                "stage={stage} inst={inst_index} operands={:?} instruction={inst:?} clobbers={:?}",
                &self.operands[self.operands_range.get(inst_index)],
                self.clobbers.get(&(inst_index as u32))
            );
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
        self.verify_strict_ssa(stage)?;
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
    pub vcode: VCodeContainer<I>,
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
        abi::CalleeABI,
        prelude::{ArenaContext, HirFunctionData, HirType},
        register::{VRegAllocator, Writable},
        riscv64::{abi::Riscv64ABI, instructions::MInst},
        types::I64,
    };
    use raana_ir::{
        ir::Program,
        opt::prelude::{BasicBlockBuilder, LocalInstBuilder},
    };

    fn add_block(data: &mut HirFunctionData, name: &str) {
        let block = data.new_basic_block().basic_block(name.to_owned(), vec![]);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().push_bb_back(block);
        data.layout_mut().insert_inst(block, ret);
    }

    fn empty_vcode() -> VCodeContainer<MInst> {
        let mut program = Program::new();
        let func = program.new_function(HirType::get_unit(), "verify".to_owned(), vec![]);
        add_block(program.func_data_mut(func), "entry");
        let arena = ArenaContext {
            program: &program,
            curr_func: Some(func),
        };
        let abi = CalleeABI::<Riscv64ABI>::new(arena);
        let order = BlockLoweringOrder::new(arena);
        let mut builder = VCodeBuilder::new(abi, order);
        builder.push(MInst::Ret);
        builder.end_bb();
        builder.build(VRegAllocator::with_capaticy(0))
    }

    fn vcode_with_integer_def() -> VCodeContainer<MInst> {
        let mut program = Program::new();
        let func = program.new_function(HirType::get_unit(), "verify".to_owned(), vec![]);
        add_block(program.func_data_mut(func), "entry");
        let arena = ArenaContext {
            program: &program,
            curr_func: Some(func),
        };
        let abi = CalleeABI::<Riscv64ABI>::new(arena);
        let order = BlockLoweringOrder::new(arena);
        let mut builder = VCodeBuilder::new(abi, order);
        let mut vregs = VRegAllocator::with_capaticy(1);
        let dst = Writable::from_reg(vregs.alloc(I64));
        builder.push(MInst::Ret);
        builder.push(MInst::LoadImm { rd: dst, value: 1 });
        builder.end_bb();
        builder.build(vregs)
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
        let abi = CalleeABI::<Riscv64ABI>::new(arena);
        let order = BlockLoweringOrder::new(arena);
        let mut builder = VCodeBuilder::new(abi, order);
        let mut vregs = VRegAllocator::with_capaticy(1);
        let dst = Writable::from_reg(vregs.alloc(I64));
        builder.push(MInst::Ret);
        builder.push(MInst::LoadImm { rd: dst, value: 2 });
        builder.push(MInst::LoadImm { rd: dst, value: 1 });
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
        let abi = CalleeABI::<Riscv64ABI>::new(arena);
        let order = BlockLoweringOrder::new(arena);
        let mut builder = VCodeBuilder::new(abi, order);
        let mut vregs = VRegAllocator::with_capaticy(2);
        let undefined = vregs.alloc(I64);
        let dst = Writable::from_reg(vregs.alloc(I64));
        builder.push(MInst::Ret);
        builder.push(MInst::Mov {
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
        let abi = CalleeABI::<Riscv64ABI>::new(arena);
        let order = BlockLoweringOrder::new(arena);
        let mut builder = VCodeBuilder::new(abi, order);
        let mut vregs = VRegAllocator::with_capaticy(1);
        builder.push(MInst::Ret);
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
        let abi = CalleeABI::<Riscv64ABI>::new(arena);
        let order = BlockLoweringOrder::new(arena);
        let mut builder = VCodeBuilder::new(abi, order);
        let mut vregs = VRegAllocator::with_capaticy(2);
        let param = vregs.alloc(I64);
        let undefined = vregs.alloc(I64);
        builder.push(MInst::Ret);
        builder.add_block_param(param.into());
        builder.end_bb();
        builder.push(MInst::gen_jump(Block::new(1)));
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
        vcode.insts.insert(0, MInst::Ret);
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
                    to: crate::reg_alloc::reg::Allocation::reg(crate::riscv64::regs::px_reg(5)),
                    class: RegClass::Int,
                },
            )],
            ..Output::default()
        };

        let error = vcode.verify_alloc_output(&output).unwrap_err();
        assert!(error.contains("out-of-range spill slot"), "{error}");
    }

    #[test]
    fn allocation_output_verifier_rejects_vector_edit() {
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
                },
            )],
            num_spillslots: 1,
            ..Output::default()
        };

        let error = vcode.verify_alloc_output(&output).unwrap_err();
        assert!(
            error.contains("unsupported vector spill semantics"),
            "{error}"
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
            allocs: vec![crate::reg_alloc::reg::Allocation::reg(
                crate::riscv64::regs::pf_reg(0),
            )],
            ..Output::default()
        };

        let error = vcode.verify_alloc_output(&output).unwrap_err();
        assert!(error.contains("assigned register"), "{error}");
    }
}
