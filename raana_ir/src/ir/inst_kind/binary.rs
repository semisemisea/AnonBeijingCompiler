use crate::ir::{
    inst_kind::InstKind,
    instruction::{Inst, InstData},
    types::Type,
};

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

    pub fn lhs(&self) -> Inst {
        self.lhs
    }

    pub fn rhs(&self) -> Inst {
        self.rhs
    }

    pub fn new_data(lhs: Inst, rhs: Inst, op: BinaryOp, ty: Type) -> InstData {
        InstData::new(ty, InstKind::Binary(Binary { lhs, rhs, op }))
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
