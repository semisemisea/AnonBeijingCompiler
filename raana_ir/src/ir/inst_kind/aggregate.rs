use crate::ir::{
    inst_kind::InstKind,
    instruction::{Inst, InstData},
    types::Type,
};

#[derive(Debug, Clone)]
pub struct Aggregate {
    value: Vec<Inst>,
}

impl Aggregate {
    pub fn value(&self) -> &[Inst] {
        &self.value
    }

    pub fn new_data(ty: Type, value: Vec<Inst>) -> InstData {
        InstData::new(ty, InstKind::Aggregate(Aggregate { value }))
    }
}
