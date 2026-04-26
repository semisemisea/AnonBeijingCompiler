use crate::ir::{
    instruction::{InstData, InstKind},
    types::Type,
};

#[derive(Debug, Clone)]
pub struct BlockArgRef {
    index: usize,
}

impl BlockArgRef {
    pub fn index(&self) -> usize {
        self.index
    }

    pub fn new_data(index: usize, ty: Type) -> InstData {
        InstData::new(ty, InstKind::BlockArgRef(BlockArgRef { index }))
    }
}

#[derive(Debug, Clone)]
pub struct FuncArgRef {
    index: usize,
}
