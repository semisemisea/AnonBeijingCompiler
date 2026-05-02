use crate::ir::{inst_kind::InstKind, instruction::InstData, types::Type};

#[derive(Debug, Clone)]
pub struct BlockArgRef {
    index: usize,
}

impl BlockArgRef {
    pub fn index(&self) -> usize {
        panic!(
            "If the TODO in src/ir/builder.rs in method `add_param` have not been fixed yet. You should not rely on this method"
        );
        // self.index
    }

    pub fn new_data(index: usize, ty: Type) -> InstData {
        InstData::new(ty, InstKind::BlockArgRef(BlockArgRef { index }))
    }
}

#[derive(Debug, Clone)]
pub struct FuncArgRef {
    index: usize,
}

impl FuncArgRef {
    pub fn index(&self) -> usize {
        self.index
    }

    pub fn new_data(index: usize, ty: Type) -> InstData {
        InstData::new(ty, InstKind::FuncArgRef(FuncArgRef { index }))
    }
}
