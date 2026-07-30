use crate::opt::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Node {
    pub func: Function,
    pub inst: Inst,
}

impl Node {
    pub fn new(func: Function, inst: Inst) -> Self {
        Self { func, inst }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Block {
    pub func: Function,
    pub block: BasicBlock,
}
impl Block {
    pub fn new(func: Function, block: BasicBlock) -> Block {
        Block { func, block }
    }
}
