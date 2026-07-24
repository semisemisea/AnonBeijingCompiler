use crate::ir::{
    inst_kind::InstKind,
    instruction::{Inst, InstData},
    types::Type,
};

#[derive(Debug, Clone)]
pub struct MemZero {
    dest: Inst,
    byte_len: usize,
}

impl MemZero {
    pub fn dest(&self) -> Inst {
        self.dest
    }

    pub fn byte_len(&self) -> usize {
        self.byte_len
    }

    pub fn new_data(dest: Inst, byte_len: usize) -> InstData {
        InstData::new(
            Type::get_unit(),
            InstKind::MemZero(MemZero { dest, byte_len }),
        )
    }
}
