use std::fmt::Debug;

use rustc_hash::FxHashMap;
use tomori_utils::Ranges;

use crate::{
    abi::{ABIMachineSpec, CalleeABI},
    block_order::{BlockLoweringOrder, MirBlockIndex},
    prelude::*,
    reg_alloc::reg::{Operand, OperandCollector, OperandVisitor, PReg, PRegSet, RegClass, VReg},
    register::{Reg, VRegAllocator, Writable},
    types::Type,
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

    fn rc_for_type(ty: Type) -> (&'static [RegClass], &'static [Type]);

    fn gen_jump(target: MirBlockIndex) -> Self;
}

pub trait MachInstEmit {}

pub trait VCodeInst: MachInst + MachInstEmit {}
impl<T: MachInst + MachInstEmit> VCodeInst for T {}

use crate::reg_alloc::index::Inst as InstIndex;

pub struct VCodeContainer<I>
where
    I: VCodeInst,
{
    vreg_types: Vec<Type>,
    /// Array of instructions
    /// `[i_0, i_1, i_2, i_3,...]`
    insts: Vec<I>,

    /// ABI of the function
    pub abi: CalleeABI<I::ABISpec>,

    /// clobber meanings some hidden interruption on liveness range of a variable.
    clobbers: FxHashMap<InstIndex, PRegSet>,

    /// Array of operands.
    /// `[o_0, o_1, o_2, o_3, ...]`
    operands: Vec<Operand>,

    /// Reverse Post-order of dominate tree.
    block_order: BlockLoweringOrder,

    /// A ranges map the instruction to operands.
    /// `[0..1, 2..4, ...]` indicates the instruction `i_0` use the operand `o_0` and `o_1`
    operands_range: Ranges,

    /// A range map the block to instructions
    /// `[0..1, 2..4, ...]` indicates the block 0 owns the instruction `i_0` and `i_1`
    block_range: Ranges,

    /// Array of each block's successors.
    block_succ: Vec<MirBlockIndex>,
    block_succ_range: Ranges,

    /// Array of each block's predecessors.
    block_pred: Vec<MirBlockIndex>,
    block_pred_range: Ranges,

    block_params: Vec<VReg>,
    block_params_range: Ranges,

    branch_block_args: Vec<VReg>,
    branch_block_args_range: Ranges,
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
            branch_block_args_range: Ranges::default(),
        }
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
        self.vcode.branch_block_args_range.push_end(end);
    }

    pub fn end_bb(&mut self) {
        let inst_end = self.vcode.insts.len();
        self.vcode.block_range.push_end(inst_end);

        let succ_end = self.vcode.block_succ.len();
        self.vcode.block_succ_range.push_end(succ_end);

        let block_param_end = self.vcode.block_params.len();
        self.vcode.block_params_range.push_end(block_param_end);

        let branch_block_arg_end = self.vcode.branch_block_args.len();
        self.vcode
            .branch_block_args_range
            .push_end(branch_block_arg_end);
    }

    pub fn build(mut self, mut vregs: VRegAllocator<I>) -> VCodeContainer<I> {
        self.vcode.vreg_types = std::mem::take(&mut vregs.vreg_types);

        self.reverse_and_finalize();

        self.collect_operands(&vregs);

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

    pub fn collect_operands(&mut self, vregs: &VRegAllocator<I>) {
        let allocatable = PRegSet::from(self.vcode.abi.machine_env());
        for (i, inst) in self.vcode.insts.iter_mut().enumerate() {
            let mut op_collector =
                OperandCollector::new(&mut self.vcode.operands, allocatable, |vreg| {
                    vregs.resolve_alias(vreg)
                });
            inst.get_operands(&mut op_collector);
            let (ops, clobbers) = op_collector.finish();
            self.vcode.operands_range.push_end(ops);

            if clobbers != PRegSet::default() {
                self.vcode.clobbers.insert(InstIndex::new(i), clobbers);
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

        self.vcode.block_pred.resize(end, MirBlockIndex::invalid());
        for (pred, range) in self.vcode.block_succ_range.iter() {
            let pred = MirBlockIndex::new(pred);
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
        self.vcode.branch_block_args_range.reverse_index();
    }
}
