use crate::ir::{
    function::Function,
    inst_kind::InstKind,
    instruction::{Inst, InstData},
    types::Type,
};

/// A tail call: transfer control to `callee(args)` and return its result
/// directly to the current function's caller, reusing the current stack frame.
///
/// `TailCall` is a terminator: it does not fall through. It is what
/// tail-call optimization produces from a self-recursive `ret call self(args)`,
/// replacing the entry-block loop trick entirely. Because the frame is reused,
/// recursion compiled this way runs in constant stack space.
#[derive(Debug, Clone)]
pub struct TailCall {
    callee: Function,
    args: Vec<Inst>,
}

impl TailCall {
    pub fn callee(&self) -> Function {
        self.callee
    }

    pub fn args(&self) -> &[Inst] {
        &self.args
    }

    pub fn new_data(callee: Function, args: Vec<Inst>) -> InstData {
        InstData::new(
            Type::get_unit(),
            InstKind::TailCall(TailCall { callee, args }),
        )
    }
}
