use crate::ir::{
    instruction::{Inst, InstData, InstKind},
    types::Type,
};

#[derive(Debug, Clone)]
pub struct Return {
    value: Option<Inst>,
}

impl Return {
    pub fn value(&self) -> Option<Inst> {
        self.value
    }

    pub fn new_data(value: Option<Inst>) -> InstData {
        InstData::new(Type::get_unit(), InstKind::Return(Return { value }))
    }
}
