use crate::ir::{BasicBlock, Function, Inst};

pub(crate) trait EntityMapper {
    type Error;

    fn map_inst(&mut self, inst: Inst) -> Result<Inst, Self::Error>;

    fn map_block(&mut self, block: BasicBlock) -> Result<BasicBlock, Self::Error>;

    fn map_function(&mut self, function: Function) -> Result<Function, Self::Error> {
        Ok(function)
    }
}
