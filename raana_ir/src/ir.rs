mod arena;
mod basic_block;
mod builder;
mod function;
mod inst_kind;
mod instruction;
mod layout;
mod program;
mod types;

pub mod builder_trait {
    pub use super::builder::{
        BasicBlockBuilder, GlobalInstBuilder, LocalInstBuilder, ScalarInstBuilder,
    };
}

pub use inst_kind::BinaryOp;
// pub use inst_kind::InstKind;
pub use program::Program;
pub use types::Type;
