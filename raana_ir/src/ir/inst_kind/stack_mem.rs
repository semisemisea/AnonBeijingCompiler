use crate::ir::{
    instruction::{Inst, InstData, InstKind},
    types::Type,
};

#[derive(Debug, Clone)]
pub struct Store {
    src: Inst,
    dest: Inst,
}

impl Store {
    pub fn src(&self) -> Inst {
        self.src
    }

    pub fn dest(&self) -> Inst {
        self.dest
    }

    pub fn new_data(src: Inst, dest: Inst) -> InstData {
        InstData::new(Type::get_unit(), InstKind::Store(Store { src, dest }))
    }
}

#[derive(Debug, Clone)]
pub struct Load {
    src: Inst,
}

impl Load {
    pub fn src(&self) -> Inst {
        self.src
    }

    pub fn new_data(src: Inst, ty: Type) -> InstData {
        InstData::new(ty, InstKind::Load(Load { src }))
    }
}
