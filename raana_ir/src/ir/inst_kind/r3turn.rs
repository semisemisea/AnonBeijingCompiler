use crate::ir::instruction::Inst;

#[derive(Debug, Clone)]
pub struct Return {
    value: Option<Inst>,
}

impl Return {
    pub fn value(&self) -> Option<std::num::NonZero<u32>> {
        self.value
    }
}
