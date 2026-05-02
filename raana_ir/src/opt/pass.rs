use std::sync::OnceLock;

use crate::ir::{Function, FunctionData, Program};

pub trait Pass: Send + Sync {
    fn run(&self, program: &mut Program) {
        for (func, data) in program.global_arena_mut().func_arena_mut().functions_mut() {
            self.run_on(func, data);
        }
    }

    /// Compatibility for old code.
    /// Normally you should not !only! implement this function
    /// But you can implement both function at same time.
    fn run_on(&self, func: Function, data: &mut FunctionData) {
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
            let p = PassesManager::new();
            p
        })
    }
}

static DEFAULT_PASSES_LIST: OnceLock<PassesManager> = OnceLock::new();
