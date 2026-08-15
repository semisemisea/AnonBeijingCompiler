//! AArch64 汇编标签（Label）体系。
//!
//! [`Label`] 是发射阶段引用的符号，统一表示四类目标：
//! - [`Label::Block`]：MIR 基本块（跳转/分支目标）；
//! - [`Label::Function`]：HIR 函数入口；
//! - [`Label::GlobalValue`]：HIR 全局量（数据段符号）；
//! - [`Label::Embedded`]：编译器内嵌符号（见 [`crate::runtime::EmbeddedSymbol`]，
//!   如 memset/calloc 的本地别名）。
//!
//! 标签与 `emit_buffer::LabelKind` 对接，保证输出汇编里的符号名唯一且碰撞安全
//! （内嵌符号使用私有本地标签）。分支指令与跳转表都通过 [`Label`] 定位目标。

use taki_mir::{block_order::MirBlockIndex, prelude::*, vcode::EmitContext};

use crate::runtime::EmbeddedSymbol;

#[derive(Debug, Clone)]
pub enum Label {
    Block(MirBlockIndex),
    Function(HirFunction),
    GlobalValue(HirInst),
    Embedded(EmbeddedSymbol),
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

    /// The lowered block index, when this label names an intra-function block.
    pub fn block(&self) -> Option<MirBlockIndex> {
        match self {
            Self::Block(block) => Some(*block),
            _ => None,
        }
    }

    pub fn emit(&self, ctx: &mut dyn EmitContext) -> core::fmt::Result {
        match self {
            Self::Block(block) => ctx.write_label_ref(*block),
            Self::Function(function) => ctx.write_function_label(*function),
            Self::GlobalValue(value) => ctx.write_global_label(*value),
            Self::Embedded(symbol) => write!(ctx, "{}", symbol.symbol()),
        }
    }
}
