use crate::ir::{basic_block::BasicBlock, instruction::Inst};

#[derive(Debug, Clone)]
pub struct Jump {
    target: BasicBlock,
    args: Vec<Inst>,
}

impl Jump {
    pub fn target(&self) -> std::num::NonZero<u32> {
        self.target
    }

    pub fn args(&self) -> &[std::num::NonZero<u32>] {
        &self.args
    }
}
