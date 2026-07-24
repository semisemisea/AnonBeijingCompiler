use crate::{block_order::MirBlockIndex, prelude::*, vcode::EmitContext};

#[derive(Debug, Clone)]
pub enum Label {
    Block(MirBlockIndex),
    Function(HirFunction),
    GlobalValue(HirInst),
    ExternalSymbol(&'static str),
}

impl Label {
    pub fn from_global_inst(val: HirInst) -> Self {
        assert!(val.is_global());
        Label::GlobalValue(val)
    }

    pub fn from_function(f: HirFunction) -> Self {
        Label::Function(f)
    }

    pub fn from_block(block: MirBlockIndex) -> Self {
        Label::Block(block)
    }

    pub fn libcall(libcall: crate::libcall::LibCall) -> Self {
        Self::ExternalSymbol(libcall.symbol())
    }

    pub fn emit(&self, ctx: &mut dyn EmitContext) -> core::fmt::Result {
        match self {
            Label::Block(b) => ctx.write_label_ref(*b),
            Label::Function(f) => ctx.write_function_label(*f),
            Label::GlobalValue(gv) => ctx.write_global_label(*gv),
            Label::ExternalSymbol(symbol) => ctx.write_external_symbol(symbol),
        }
    }
}
