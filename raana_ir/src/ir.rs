pub mod arena;
pub(crate) mod basic_block;
pub(crate) mod builder;
pub(crate) mod function;
pub(crate) mod inst_kind;
pub(crate) mod instruction;
pub(crate) mod layout;
pub(crate) mod program;
pub(crate) mod types;

pub mod builder_trait {
    pub use super::builder::{
        BasicBlockBuilder, BasicBlockBuilders, GlobalBuilder, GlobalInstBuilder, InstInsert,
        LocalBuilder, LocalInstBuilder, ScalarInstBuilder,
    };
}

pub use basic_block::BasicBlock;
pub use function::{Function, FunctionData};
pub use inst_kind::{
    Aggregate, Binary, BinaryOp, BlockArgRef, Branch, Call, GetElemPtr, GetPtr, InstKind, Integer,
    Jump, Load, Return, Store,
};
pub use instruction::{Inst, InstData};
pub use program::Program;
pub use types::{Type, TypeKind};
