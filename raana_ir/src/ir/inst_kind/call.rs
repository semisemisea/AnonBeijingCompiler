use crate::ir::{function::Function, instruction::Inst};

#[derive(Debug, Clone)]
pub struct Call {
    callee: Function,
    args: Vec<Inst>,
}

impl Call {
    pub fn callee(&self) -> std::num::NonZero<u32> {
        self.callee
    }

    pub fn args(&self) -> &[std::num::NonZero<u32>] {
        &self.args
    }
}
