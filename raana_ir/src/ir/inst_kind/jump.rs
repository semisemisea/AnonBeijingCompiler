use crate::ir::{
    basic_block::BasicBlock,
    inst_kind::InstKind,
    instruction::{Inst, InstData},
    types::Type,
};

#[derive(Debug, Clone)]
pub struct Jump {
    target: BasicBlock,
    args: Vec<Inst>,
}

impl Jump {
    pub fn target(&self) -> BasicBlock {
        self.target
    }

    pub fn args(&self) -> &[Inst] {
        &self.args
    }

    pub fn new_data(target: BasicBlock, args: Vec<Inst>) -> InstData {
        InstData::new(Type::get_unit(), InstKind::Jump(Jump { target, args }))
    }
}
