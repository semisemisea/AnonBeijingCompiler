use crate::ir::{
    basic_block::BasicBlock,
    inst_kind::InstKind,
    instruction::{Inst, InstData},
    types::Type,
};

#[derive(Debug, Clone)]
pub struct Branch {
    cond: Inst,
    t_target: BasicBlock,
    t_args: Vec<Inst>,
    f_target: BasicBlock,
    f_args: Vec<Inst>,
}

impl Branch {
    pub fn cond(&self) -> Inst {
        self.cond
    }

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

    #[deprecated]
    pub fn false_bb(&self) -> BasicBlock {
        self.f_target
    }

    #[deprecated]
    pub fn true_bb(&self) -> BasicBlock {
        self.t_target
    }
    #[deprecated]
    pub fn false_args(&self) -> &[Inst] {
        &self.f_args
    }

    #[deprecated]
    pub fn true_args(&self) -> &[Inst] {
        &self.t_args
    }

    pub fn new_data(
        cond: Inst,
        t_target: BasicBlock,
        t_args: Vec<Inst>,
        f_target: BasicBlock,
        f_args: Vec<Inst>,
    ) -> InstData {
        InstData::new(
            Type::get_unit(),
            InstKind::Branch(Branch {
                cond,
                t_target,
                t_args,
                f_target,
                f_args,
            }),
        )
    }
}
