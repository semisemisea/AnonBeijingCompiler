use crate::armv8::operand::{Inst, InstKind};

pub struct BasicBlock {
    pub name: String,
    pub insts: Vec<Inst>,
}

pub struct Function {
    pub name: String,
    pub blocks: Vec<BasicBlock>,
    pub stack_size: u32,
}

pub struct Program {
    pub global: Vec<InstKind>,
    pub funcs: Vec<Function>,
}
