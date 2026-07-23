use crate::ir::{
    InstKind, Type,
    instruction::{Inst, InstData},
};

/// Selects one of two values without changing control flow.
///
/// A zero condition selects `if_false`; any non-zero condition selects
/// `if_true`.
#[derive(Debug, Clone)]
pub struct Select {
    cond: Inst,
    if_true: Inst,
    if_false: Inst,
}

impl Select {
    pub fn cond(&self) -> Inst {
        self.cond
    }

    pub fn if_true(&self) -> Inst {
        self.if_true
    }

    pub fn if_false(&self) -> Inst {
        self.if_false
    }

    pub fn new_data(cond: Inst, if_true: Inst, if_false: Inst, ty: Type) -> InstData {
        InstData::new(
            ty,
            InstKind::Select(Select {
                cond,
                if_true,
                if_false,
            }),
        )
    }
}
