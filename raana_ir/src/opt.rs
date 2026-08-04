mod analysis_passes;
pub mod config;
pub mod pass;
mod passes;
pub mod stats;
pub mod utils;

// Re-exported so out-of-tree emitters (e.g. the LLVM writer) can recognize the
// compiler-provided `soyo_mulmod` modmul builtin without depending on the
// (private) pass module layout.
pub use passes::mulmod_recognize::MULMOD_HELPER;

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
    pub use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
    pub use std::collections::VecDeque;

    pub use log::{debug, error, info, trace, warn};

    // Analysis pass
    pub use super::analysis_passes::*;
    // Pass trait object
    pub use super::pass::{ArenaContext, ArenaContextMut, Pass};
    // Pass
    pub use super::passes::*;
    // Type alias
    pub use super::utils::type_alias::*;
    // IDAllocator
    pub use super::utils::IDAllocator;
    // utils
    pub use super::utils;

    pub use utils::call::*;
    pub use utils::global_handle::*;
}
