pub mod arena;
pub mod basic_block;
pub(crate) mod builder;
pub(crate) mod function;
pub mod inst_kind;
pub(crate) mod instruction;
pub mod layout;
pub(crate) mod program;
pub(crate) mod types;

pub mod builder_trait {
    pub use super::builder::{
        BasicBlockBuilder, BasicBlockBuilders, GlobalBuilder, GlobalInstBuilder, InfoQuery,
        InstInsert, LocalBuilder, LocalInstBuilder, ScalarInstBuilder,
    };
}

pub use basic_block::BasicBlock;
pub use builder::{BasicBlockBuilders, GlobalBuilder, LocalBuilder};
pub use function::{Function, FunctionData};
pub use inst_kind::{
    Aggregate, Binary, BinaryOp, BlockArgRef, Branch, Call, Cast, GetElemPtr, InstKind, Integer,
    Jump, Load, Return, Select, Store,
};
pub use instruction::{Inst, InstData};
pub use program::Program;
pub use types::{Type, TypeKind};
