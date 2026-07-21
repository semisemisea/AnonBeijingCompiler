use std::fmt::Debug;

use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use tomori_utils::Ranges;

use crate::{
    abi::{ABIMachineSpec, CalleeABI},
    block_order::{BlockLoweringOrder, MirBlockIndex},
    reg_alloc::{
        function::Function,
        index::{Block, Inst, InstRange},
        reg::{
            Operand, OperandCollector, OperandVisitor, OperandWriter, Output, PRegSet, RegClass,
            VReg,
        },
    },
    register::{Reg, VRegAllocator, Writable},
    types::LoweredType,
};

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

    pub fn inst(&self, i: usize) -> &I {
        &self.insts[i]
    }

    pub fn block_order(&self) -> &BlockLoweringOrder {
        &self.block_order
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
        // Currently block params are never aliased to another vreg.
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
