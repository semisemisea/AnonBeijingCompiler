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
