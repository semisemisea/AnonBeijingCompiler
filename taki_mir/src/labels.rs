use crate::{block_order::MirBlockIndex, prelude::*, vcode::EmitContext};

/// A symbol a backend can name but that no IR entity describes: a libc call, a
/// compiler-provided assembly blob, and so on.
///
/// Each target keeps its own symbol enum, because what the symbol *is* (an
/// external definition resolved by the linker, or a private label emitted into
/// this same unit) differs per target. This trait unifies only the one thing
/// [`Label`] needs from them: how to spell the symbol.
pub trait TargetSymbol: Copy {
    fn symbol(self) -> &'static str;
}

/// Anything a branch, call, or address materialization can name.
///
/// The IR-derived variants and the whole emission path are target-independent;
/// `S` carries whichever extra symbols a particular backend needs.
#[derive(Debug, Clone)]
pub enum Label<S> {
    Block(MirBlockIndex),
    Function(HirFunction),
    GlobalValue(HirInst),
    Symbol(S),
}

impl<S> Label<S> {
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
}

impl<S: TargetSymbol> Label<S> {
    pub fn emit(&self, ctx: &mut dyn EmitContext) -> core::fmt::Result {
        match self {
            Self::Block(block) => ctx.write_label_ref(*block),
            Self::Function(function) => ctx.write_function_label(*function),
            Self::GlobalValue(value) => ctx.write_global_label(*value),
            Self::Symbol(symbol) => write!(ctx, "{}", symbol.symbol()),
        }
    }
}
