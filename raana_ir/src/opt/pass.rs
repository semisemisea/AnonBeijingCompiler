use crate::ir::Program;

pub trait Pass {
    fn run_on(&mut self, program: &mut Program);
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
        self.passes.iter_mut().for_each(|p| p.run_on(program));
    }
}
