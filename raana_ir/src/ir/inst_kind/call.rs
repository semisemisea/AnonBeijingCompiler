use crate::ir::{
    function::Function,
    instruction::{Inst, InstData, InstKind},
    types::Type,
};

#[derive(Debug, Clone)]
pub struct Call {
    callee: Function,
    args: Vec<Inst>,
}

impl Call {
    pub fn callee(&self) -> Function {
        self.callee
    }

    pub fn args(&self) -> &[Inst] {
        &self.args
    }

    pub fn new_data(callee: Function, args: Vec<Inst>, ty: Type) -> InstData {
        InstData::new(ty, InstKind::Call(Call { callee, args }))
    }
}
