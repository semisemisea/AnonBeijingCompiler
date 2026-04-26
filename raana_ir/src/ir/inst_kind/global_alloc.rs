use crate::ir::{
    instruction::{Inst, InstData, InstKind},
    types::Type,
};

#[derive(Debug, Clone)]
pub struct GlobalAlloc {
    init: Inst,
}

impl GlobalAlloc {
    pub fn init(&self) -> Inst {
        self.init
    }

    pub fn new_data(init: Inst, ty: Type) -> InstData {
        InstData::new(ty, InstKind::GlobalAlloc(GlobalAlloc { init }))
    }
}
