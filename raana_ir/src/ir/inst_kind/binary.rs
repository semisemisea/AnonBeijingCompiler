use crate::ir::instruction::Inst;

#[derive(Debug, Clone)]
pub struct Binary {
    op: BinaryOp,
    lhs: Inst,
    rhs: Inst,
}

impl Binary {
    pub fn op(&self) -> &BinaryOp {
        &self.op
    }

    pub fn lhs(&self) -> std::num::NonZero<u32> {
        self.lhs
    }

    pub fn rhs(&self) -> std::num::NonZero<u32> {
        self.rhs
    }
}

#[derive(Debug, Clone)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}
