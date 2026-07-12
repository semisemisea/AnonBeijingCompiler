use crate::{
    abi::CalleeABI,
    lower::{LowerBackend, LowerContext},
};

pub mod abi;
pub mod armv8;
pub mod block_order;
pub mod inst_predicate;
pub mod lower;
pub mod reg_alloc;
pub mod register;
pub mod types;
pub mod vcode;

pub mod prelude {
    use std::collections::HashSet;

    pub use raana_ir::ir::Program as HirProgram;
    pub use raana_ir::ir::Type as HirType;
    pub use raana_ir::ir::arena::Arena;
    pub use raana_ir::ir::builder_trait::*;
    pub use raana_ir::ir::inst_kind::*;
    pub use raana_ir::ir::{
        BasicBlock as HirBasicBlock, basic_block::BasicBlockData as HirBasicBlockData,
        layout::BasicBlockLayout as HirBasicBlockLayout,
    };
    pub use raana_ir::ir::{
        BinaryOp, Inst as HirInst, InstData as HirInstData, InstKind as HirInstKind,
    };
    pub use raana_ir::ir::{Function as HirFunction, FunctionData as HirFunctionData};

    use rustc_hash::FxBuildHasher;

    pub struct ArenaContext<'a> {
        pub program: &'a HirProgram,
        pub curr_func: Option<HirFunction>,
    }

    pub type FxHashSet<K> = HashSet<K, FxBuildHasher>;

    impl ArenaContext<'_> {
        /// Get current function data
        pub fn f(&self) -> &HirFunctionData {
            self.program.func_data(self.curr_func.unwrap())
        }

        pub fn set_current_function(&mut self, f: HirFunction) {
            self.curr_func.replace(f);
        }
    }

    impl Arena for ArenaContext<'_> {
        fn local(&self) -> &raana_ir::ir::arena::LocalArena {
            self.program
                .func_data(self.curr_func.unwrap())
                .local_arena()
        }

        fn local_mut(&mut self) -> &mut raana_ir::ir::arena::LocalArena {
            unimplemented!()
        }

        fn global(&self) -> &raana_ir::ir::arena::GlobalArena {
            self.program.global_arena()
        }

        fn global_mut(&mut self) -> &mut raana_ir::ir::arena::GlobalArena {
            unimplemented!()
        }
    }
}

use prelude::*;

// fn compile<B: LowerBackend>(p: &HirProgram, b: B) {
//     // TODO: Lower the global part
//
//     // TODO: Lower the local part, function by function
//
//     // TODO: Before calling this function, make sure all the orphan instruction is removed from the
//     // layout and !arena!.
//     let abi = CalleeABI::new();
//     let lower = LowerContext::new(p, abi);
// }
