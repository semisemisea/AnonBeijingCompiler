use crate::ir::{
    instruction::{Inst, InstData, InstKind},
    types::Type,
};

#[derive(Debug, Clone)]
pub struct GetElemPtr {
    base: Inst,
    offset: Inst,
}

impl GetElemPtr {
    pub fn base(&self) -> Inst {
        self.base
    }

    pub fn offset(&self) -> Inst {
        self.offset
    }

    pub fn new_data(base: Inst, offset: Inst, ty: Type) -> InstData {
        InstData::new(ty, InstKind::GetElemPtr(GetElemPtr { base, offset }))
    }
}

#[derive(Debug, Clone)]
pub struct GetPtr {
    base: Inst,
    offset: Inst,
}

impl GetPtr {
    pub fn base(&self) -> Inst {
        self.base
    }

    pub fn offset(&self) -> Inst {
        self.offset
    }

    pub fn new_data(base: Inst, offset: Inst, ty: Type) -> InstData {
        InstData::new(ty, InstKind::GetPtr(GetPtr { base, offset }))
    }
}
