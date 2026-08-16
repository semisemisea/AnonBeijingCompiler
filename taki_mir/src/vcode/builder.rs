//! Incremental VCode builder used by backend lowering.

use super::*;

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
