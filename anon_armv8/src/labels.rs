use taki_mir::{block_order::MirBlockIndex, prelude::*, vcode::EmitContext};

#[derive(Debug, Clone)]
pub enum Label {
    Block(MirBlockIndex),
    Function(HirFunction),
    GlobalValue(HirInst),
}

impl Label {
    pub fn from_global_inst(value: HirInst) -> Self {
        assert!(value.is_global());
        Self::GlobalValue(value)
    }

    pub fn from_function(function: HirFunction) -> Self {
        Self::Function(function)
    }

    pub fn from_block(block: MirBlockIndex) -> Self {
        Self::Block(block)
    }

    pub fn emit(&self, ctx: &mut dyn EmitContext) -> core::fmt::Result {
        match self {
            Self::Block(block) => ctx.write_label_ref(*block),
            Self::Function(function) => ctx.write_function_label(*function),
            Self::GlobalValue(value) => ctx.write_global_label(*value),
        }
    }
}
