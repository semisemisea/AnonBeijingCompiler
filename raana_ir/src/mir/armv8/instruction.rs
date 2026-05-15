use crate::mir::armv8::operand::Inst;

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
    pub global: Vec<Inst>,
    pub funcs: Vec<Function>,
}
