use crate::ir::{
    basic_block::BasicBlock,
    instruction::{Inst, InstData, InstKind},
    types::Type,
};

#[derive(Debug, Clone)]
pub struct Branch {
    t_target: BasicBlock,
    t_args: Vec<Inst>,
    f_target: BasicBlock,
    f_args: Vec<Inst>,
}

impl Branch {
    pub fn t_target(&self) -> BasicBlock {
        self.t_target
    }

    pub fn t_args(&self) -> &[Inst] {
        &self.t_args
    }

    pub fn f_target(&self) -> BasicBlock {
        self.f_target
    }

    pub fn f_args(&self) -> &[Inst] {
        &self.f_args
    }

    pub fn new_data(
        t_target: BasicBlock,
        t_args: Vec<Inst>,
        f_target: BasicBlock,
        f_args: Vec<Inst>,
    ) -> InstData {
        InstData::new(
            Type::get_unit(),
            InstKind::Branch(Branch {
                t_target,
                t_args,
                f_target,
                f_args,
            }),
        )
    }
}
