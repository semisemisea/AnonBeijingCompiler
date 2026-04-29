use crate::ir::{Function, FunctionData, Program};

pub trait Pass {
    fn run(&mut self, program: &mut Program) {
        for (func, data) in program.global_arena_mut().func_arena_mut().functions_mut() {
            self.run_on(func, data);
        }
    }

    /// Compatibility for old code.
    /// Normally you should not !only! implement this function
    /// But you can implement both function at same time.
    fn run_on(&mut self, func: Function, data: &mut FunctionData) {
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

    pub fn run_passes(&mut self, program: &mut Program) {
        self.passes.iter_mut().for_each(|p| p.run(program));
    }
}
