use crate::opt::prelude::*;

pub struct StrengthReduction;

impl Pass for StrengthReduction {
    fn run_on(&self, _data: &mut ArenaContext<'_>) -> bool {
        false
    }
}

impl StrengthReduction {}
