use crate::opt::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallSite {
    pub call: Inst,
    pub caller: Function,
    pub callee: Function,
    /// The instruction right next to the call instruction.
    pub continuation: Inst,
}

#[derive(Debug, Default)]
pub struct CallSiteTable {
    pub calls: Vec<Inst>,
    pub callers: Vec<Function>,
    pub callees: Vec<Function>,
    pub continuations: Vec<Inst>,
}

impl CallSiteTable {
    pub fn push(&mut self, call: Inst, caller: Function, callee: Function, continuation: Inst) {
        self.calls.push(call);
        self.callers.push(caller);
        self.callees.push(callee);
        self.continuations.push(continuation);
    }

    pub fn len(&self) -> usize {
        self.calls.len()
    }

    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
}
