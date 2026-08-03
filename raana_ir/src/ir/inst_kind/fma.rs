use crate::ir::{
    inst_kind::InstKind,
    instruction::{Inst, InstData},
    types::Type,
};

/// Fused multiply-add: `dst = fma(acc, lhs, rhs) = acc + lhs * rhs`.
///
/// The accumulator is a read-write operand: the selected machine instruction
/// (AArch64 `fmla`) writes its result into the accumulator register.
#[derive(Debug, Clone)]
pub struct Fma {
    acc: Inst,
    lhs: Inst,
    rhs: Inst,
}

impl Fma {
    pub fn acc(&self) -> Inst {
        self.acc
    }

    pub fn lhs(&self) -> Inst {
        self.lhs
    }

    pub fn rhs(&self) -> Inst {
        self.rhs
    }

    pub fn new_data(acc: Inst, lhs: Inst, rhs: Inst, ty: Type) -> InstData {
        InstData::new(ty, InstKind::Fma(Fma { acc, lhs, rhs }))
    }
}
