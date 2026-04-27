use crate::ir::{inst_kind::InstKind, instruction::InstData, types::Type};

#[derive(Debug, Clone)]
pub struct Integer {
    value: i32,
}

impl Integer {
    pub fn value(&self) -> i32 {
        self.value
    }

    pub(crate) fn new_data(value: i32) -> InstData {
        InstData::new(Type::get_i32(), InstKind::Integer(Integer { value }))
    }
}

#[derive(Debug, Clone)]
pub struct Float {
    value: f32,
}

impl Float {
    pub fn value(&self) -> f32 {
        self.value
    }

    pub(crate) fn new_data(value: f32) -> InstData {
        InstData::new(Type::get_f32(), InstKind::Float(Float { value }))
    }
}
