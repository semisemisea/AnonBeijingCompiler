#[derive(Debug, Clone)]
pub struct BlockArgRef {
    index: usize,
}

impl BlockArgRef {
    pub fn index(&self) -> usize {
        self.index
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
}
