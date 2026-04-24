use crate::ir::instruction::Inst;

#[derive(Debug, Clone)]
pub struct Store {
    src: Inst,
    dest: Inst,
}

impl Store {
    pub fn src(&self) -> std::num::NonZero<u32> {
        self.src
    }

    pub fn dest(&self) -> std::num::NonZero<u32> {
        self.dest
    }
}

#[derive(Debug, Clone)]
pub struct Load {
    src: Inst,
    dest: Inst,
}

impl Load {
    pub fn src(&self) -> std::num::NonZero<u32> {
        self.src
    }

    pub fn dest(&self) -> std::num::NonZero<u32> {
        self.dest
    }
}
