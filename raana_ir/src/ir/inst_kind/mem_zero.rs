use crate::ir::{
    inst_kind::InstKind,
    instruction::{Inst, InstData},
    types::Type,
};

/// The byte length of a `MemZero`. Either a compile-time constant (array
/// zero-initialization) or a runtime value (a zeroed loop region whose trip
/// count is not statically known).
#[derive(Debug, Clone)]
pub enum MemZeroLen {
    Const(usize),
    Value(Inst),
}

#[derive(Debug, Clone)]
pub struct MemZero {
    dest: Inst,
    byte_len: MemZeroLen,
}

impl MemZero {
    pub fn dest(&self) -> Inst {
        self.dest
    }

    pub fn byte_len(&self) -> usize {
        match self.byte_len {
            MemZeroLen::Const(byte_len) => byte_len,
            MemZeroLen::Value(_) => panic!("dynamic mem zero has no constant byte length"),
        }
    }

    pub fn byte_len_len(&self) -> &MemZeroLen {
        &self.byte_len
    }

    pub fn new_data(dest: Inst, byte_len: usize) -> InstData {
        InstData::new(
            Type::get_unit(),
            InstKind::MemZero(MemZero {
                dest,
                byte_len: MemZeroLen::Const(byte_len),
            }),
        )
    }

    pub fn new_dynamic_data(dest: Inst, byte_len: Inst) -> InstData {
        InstData::new(
            Type::get_unit(),
            InstKind::MemZero(MemZero {
                dest,
                byte_len: MemZeroLen::Value(byte_len),
            }),
        )
    }
}
