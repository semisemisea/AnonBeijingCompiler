use crate::ir::{basic_block::BasicBlock, instruction::Inst};

#[derive(Debug, Clone)]
pub struct Branch {
    t_target: BasicBlock,
    t_args: Vec<Inst>,
    f_target: BasicBlock,
    f_args: Vec<Inst>,
}

impl Branch {
    pub fn t_target(&self) -> std::num::NonZero<u32> {
        self.t_target
    }

    pub fn t_args(&self) -> &[std::num::NonZero<u32>] {
        &self.t_args
    }

    pub fn f_target(&self) -> std::num::NonZero<u32> {
        self.f_target
    }

    pub fn f_args(&self) -> &[std::num::NonZero<u32>] {
        &self.f_args
    }
}
