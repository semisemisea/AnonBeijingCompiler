use crate::ir::{
    inst_kind::InstKind,
    instruction::{Inst, InstData},
    types::Type,
};

#[derive(Debug, Clone)]
pub struct Cast {
    src: Inst,
}

impl Cast {
    pub fn src(&self) -> Inst {
        self.src
    }

    pub fn new_data(src: Inst, ty: Type) -> InstData {
        InstData::new(ty, InstKind::Cast(Cast { src }))
    }
}
