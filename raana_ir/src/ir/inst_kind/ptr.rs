use crate::ir::instruction::Inst;

#[derive(Debug, Clone)]
pub struct GetElemPtr {
    base: Inst,
    offset: Inst,
}

impl GetElemPtr {
    pub fn base(&self) -> std::num::NonZero<u32> {
        self.base
    }

    pub fn offset(&self) -> std::num::NonZero<u32> {
        self.offset
    }
}

#[derive(Debug, Clone)]
pub struct GetPtr {
    base: Inst,
    offset: Inst,
}

impl GetPtr {
    pub fn base(&self) -> std::num::NonZero<u32> {
        self.base
    }

    pub fn offset(&self) -> std::num::NonZero<u32> {
        self.offset
    }
}
