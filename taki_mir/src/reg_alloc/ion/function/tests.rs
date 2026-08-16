use super::*;
use crate::{
    reg_alloc::reg::{Operand, PReg},
    register::preg_to_pinned_vreg,
};

struct TestFunction {
    operands: Vec<Vec<Operand>>,
    params: Vec<Vec<VReg>>,
    args: Vec<Vec<Vec<VReg>>>,
    succs: Vec<Vec<Block>>,
    preds: Vec<Vec<Block>>,
}

impl Function for TestFunction {
    fn num_insts(&self) -> usize {
        self.operands.len()
    }

    fn num_blocks(&self) -> usize {
        self.params.len()
    }

    fn entry_block(&self) -> Block {
        Block::new(0)
    }

    fn block_insns(&self, block: Block) -> InstRange {
        let inst = if block.index() == 0 { 0 } else { 1 };
        InstRange::new(Inst::new(inst), Inst::new(inst + 1))
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

    fn is_ret(&self, _inst: Inst) -> bool {
        false
    }

    fn is_branch(&self, _inst: Inst) -> bool {
        false
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
        0
    }

    fn spillslot_size(&self, _regclass: RegClass) -> usize {
        1
    }
}

#[test]
fn dense_vreg_mapping_preserves_pinned_pregs_and_edge_dataflow() {
    let source = VReg::new(400, RegClass::Int);
    let pinned = preg_to_pinned_vreg(PReg::new(5, RegClass::Int));
    let param = VReg::new(700, RegClass::Int);
    let function = TestFunction {
        operands: vec![
            vec![
                Operand::reg_def(source),
                Operand::reg_fixed_use(pinned, PReg::new(5, RegClass::Int)),
            ],
            vec![Operand::reg_use(param)],
        ],
        params: vec![vec![], vec![param]],
        args: vec![vec![vec![source]], vec![]],
        succs: vec![vec![Block::new(1)], vec![]],
        preds: vec![vec![], vec![Block::new(0)]],
    };

    let dense = DenseVRegFunction::new(&function);
    let dense_source = dense.local_vreg(source).unwrap();
    let dense_pinned = dense.local_vreg(pinned).unwrap();
    let dense_param = dense.local_vreg(param).unwrap();

    assert_eq!(dense_source.vreg(), 0);
    assert_eq!(dense_pinned.vreg(), 1);
    assert_eq!(dense_param.vreg(), 2);
    assert_eq!(dense.num_vregs(), 3);
    assert_eq!(dense.original_vreg(dense_pinned), Some(pinned));
    assert_eq!(
        dense.pinned_preg(dense_pinned),
        Some(PReg::new(5, RegClass::Int))
    );
    assert_eq!(dense.inst_operands(Inst::new(0))[1].vreg(), dense_pinned);
    assert_eq!(dense.block_params(Block::new(1)), &[dense_param]);
    assert_eq!(
        dense.branch_blockparams(Block::new(0), Inst::new(0), 0),
        &[dense_source]
    );
}
