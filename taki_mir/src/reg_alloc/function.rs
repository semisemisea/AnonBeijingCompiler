use crate::reg_alloc::index::{Block, Inst, InstRange};
use crate::reg_alloc::reg::{Operand, PRegSet, RegClass, VReg};

pub trait Function {
    fn num_insts(&self) -> usize;
    fn num_blocks(&self) -> usize;
    fn entry_block(&self) -> Block;
    fn block_insns(&self, block: Block) -> InstRange;
    fn block_succs(&self, block: Block) -> &[Block];
    fn block_preds(&self, block: Block) -> &[Block];
    fn block_params(&self, block: Block) -> &[VReg];
    fn is_ret(&self, insn: Inst) -> bool;
    fn is_branch(&self, insn: Inst) -> bool;
    fn branch_blockparams(&self, block: Block, insn: Inst, succ_idx: usize) -> &[VReg];
    fn inst_operands(&self, insn: Inst) -> &[Operand];
    fn inst_clobbers(&self, insn: Inst) -> PRegSet;
    fn num_vregs(&self) -> usize;
    fn spillslot_size(&self, regclass: RegClass) -> usize;
    fn multi_spillslot_named_by_last_slot(&self) -> bool {
        false
    }
    fn allow_multiple_vreg_defs(&self) -> bool {
        false
    }
}
