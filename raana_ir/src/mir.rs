mod armv8;

pub mod prelude {
    pub use crate::ir::Program as HirProgram;
    pub use crate::ir::Type as HirType;
    pub use crate::ir::arena::Arena;
    pub use crate::ir::builder_trait::*;
    pub use crate::ir::inst_kind::*;
    pub use crate::ir::{
        BasicBlock as HirBasicBlock, basic_block::BasicBlockData as HirBasicBlockData,
        layout::BasicBlockLayout as HirBasicBlockLayout,
    };
    pub use crate::ir::{
        BinaryOp, Inst as HirInst, InstData as HirInstData, InstKind as HirInstKind,
    };
    pub use crate::ir::{Function as HirFunction, FunctionData as HirFunctionData};
}
