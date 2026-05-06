mod analysis_passes;
pub mod pass;
mod passes;
pub mod utils;

/// Opt crate prelude
pub mod prelude {
    // IR object.
    pub use crate::ir::Program;
    pub use crate::ir::Type;
    pub use crate::ir::arena::Arena;
    pub use crate::ir::builder_trait::*;
    pub use crate::ir::{BasicBlock, basic_block::BasicBlockData};
    pub use crate::ir::{BinaryOp, Inst, InstData, InstKind};
    pub use crate::ir::{Function, FunctionData};

    // Common Data Structure.
    pub use std::collections::{HashMap, HashSet, VecDeque};

    pub use log::{debug, error, info, trace, warn};

    // Analysis pass
    pub use super::analysis_passes::*;
    // Pass trait object
    pub use super::pass::{ArenaContext, Pass};
    // Pass
    pub use super::passes::*;
    // Type alias
    pub use super::utils::type_alias::*;
    // IDAllocator
    pub use super::utils::IDAllocator;
    // utils
    pub use super::utils;
}
