use crate::ir::{
    inst_kind::InstKind,
    instruction::{Inst, InstData},
    types::Type,
};

#[derive(Debug, Clone)]
pub struct GetElemPtr {
    base: Inst,
    offsets: Vec<Inst>,
}

impl GetElemPtr {
    pub fn base(&self) -> Inst {
        self.base
    }

    pub fn offsets(&self) -> &[Inst] {
        &self.offsets
    }

    pub fn new_data(base: Inst, offsets: Vec<Inst>, ty: Type) -> InstData {
        InstData::new(ty, InstKind::GetElemPtr(GetElemPtr { base, offsets }))
    }
}
