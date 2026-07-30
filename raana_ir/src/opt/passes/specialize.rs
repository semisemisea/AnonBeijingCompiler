use crate::opt::prelude::*;

pub struct Specialize;

impl Pass for Specialize {
    fn run(&self, program: &mut Program) -> bool {
        false
    }
}
