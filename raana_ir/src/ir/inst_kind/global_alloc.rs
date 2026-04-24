use crate::ir::instruction::Inst;

#[derive(Debug, Clone)]
pub struct GlobalAlloc {
    init: Inst,
}

impl GlobalAlloc {
    pub fn init(&self) -> std::num::NonZero<u32> {
        self.init
    }
}
