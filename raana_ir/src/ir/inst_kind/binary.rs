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

#[derive(Debug, Clone, Copy)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    NotEq,
    Eq,
    Gt,
    Lt,
    Ge,
    Le,
    And,
    Or,
    Xor,
    Shl,
    Shr,
    Sar,
}

impl std::fmt::Display for BinaryOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                BinaryOp::Add => "add",
                BinaryOp::Sub => "sub",
                BinaryOp::Mul => "mul",
                BinaryOp::Div => "div",
                BinaryOp::Rem => "rem",
                BinaryOp::NotEq => "neq",
                BinaryOp::Eq => "eq",
                BinaryOp::Gt => "gt",
                BinaryOp::Lt => "lt",
                BinaryOp::Le => "le",
                BinaryOp::Ge => "ge",
                BinaryOp::And => "and",
                BinaryOp::Or => "or",
                BinaryOp::Xor => "xor",
                BinaryOp::Shl => "shl",
                BinaryOp::Shr => "shr",
                BinaryOp::Sar => "sar",
            }
        )
    }
}
