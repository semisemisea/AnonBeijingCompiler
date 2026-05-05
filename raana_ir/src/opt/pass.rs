use std::sync::OnceLock;

use itertools::Itertools;

use crate::{
    ir::{Function, FunctionData, Program, arena::Arena},
    opt::passes::*,
};

pub struct ArenaContext<'a> {
    pub program: &'a mut Program,
    pub curr_func: Option<Function>,
}
impl std::ops::Deref for ArenaContext<'_> {
    type Target = FunctionData;
    fn deref(&self) -> &Self::Target {
        self.program.func_data(self.curr_func.unwrap())
    }
}

impl std::ops::DerefMut for ArenaContext<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.program.func_data_mut(self.curr_func.unwrap())
    }
}

impl Arena for ArenaContext<'_> {
    fn local(&self) -> &crate::ir::arena::LocalArena {
        self.program
            .func_data(self.curr_func.unwrap())
            .local_arena()
    }

    fn local_mut(&mut self) -> &mut crate::ir::arena::LocalArena {
        self.program
            .func_data_mut(self.curr_func.unwrap())
            .local_arena_mut()
    }

    fn global(&self) -> &crate::ir::arena::GlobalArena {
        self.program.global_arena()
    }

    fn global_mut(&mut self) -> &mut crate::ir::arena::GlobalArena {
        self.program.global_arena_mut()
    }
}

pub trait Pass: Send + Sync {
    fn run(&self, program: &mut Program) {
        let funcs = program
            .global_arena()
            .func_arena()
            .functions()
            .map(|t| t.0)
            .collect_vec();
        let mut arena_context = ArenaContext {
            program,
            curr_func: None,
        };
        for func in funcs {
            arena_context.curr_func = Some(func);
            self.run_on(&mut arena_context);
        }
    }

    /// Compatibility for old code.
    /// Normally you should not !only! implement this function
    /// But you can implement both function at same time.
    fn run_on(&self, data: &mut ArenaContext<'_>) {
        unimplemented!()
    }
}

pub struct PassesManager {
    passes: Vec<Box<dyn Pass>>,
}

impl PassesManager {
    pub fn new() -> PassesManager {
        PassesManager { passes: Vec::new() }
    }

    pub fn register(&mut self, pass: Box<dyn Pass>) {
        self.passes.push(pass);
    }

    pub fn run_passes(&self, program: &mut Program) {
        self.passes.iter().for_each(|p| p.run(program));
    }

    pub fn default_ref() -> &'static PassesManager {
        DEFAULT_PASSES_LIST.get_or_init(|| {
            let mut p = PassesManager::new();
            let ssa = Box::new(ssa::SSATransform);
            p.register(ssa);

            let sccp = Box::new(const_prop::SparseConditionConstantPropagation);
            p.register(sccp);

            let gvn = Box::new(gvn::GlobalInstNumbering);
            p.register(gvn);
            p
        })
    }
}

static DEFAULT_PASSES_LIST: OnceLock<PassesManager> = OnceLock::new();
