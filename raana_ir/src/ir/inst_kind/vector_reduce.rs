use crate::ir::{
    inst_kind::InstKind,
    instruction::{Inst, InstData},
    types::Type,
};

/// Horizontal reduction of a vector into a scalar: `dst = op(src[0..lanes])`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorReduceOp {
    /// Lane-wise integer sum, e.g. AArch64 `addv` on `.4s`.
    Add,
}

#[derive(Debug, Clone)]
pub struct VectorReduce {
    op: VectorReduceOp,
    src: Inst,
}

impl VectorReduce {
    pub fn op(&self) -> VectorReduceOp {
        self.op
    }

    pub fn src(&self) -> Inst {
        self.src
    }

    pub fn new_data(op: VectorReduceOp, src: Inst, ty: Type) -> InstData {
        InstData::new(ty, InstKind::VectorReduce(VectorReduce { op, src }))
    }
}
