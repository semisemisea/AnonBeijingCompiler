use crate::ir::{
    inst_kind::InstKind,
    instruction::{Inst, InstData},
    types::Type,
};

/// Splat a scalar across every lane of a vector: `dst[i] = src`.
#[derive(Debug, Clone)]
pub struct VectorSplat {
    src: Inst,
}

impl VectorSplat {
    pub fn src(&self) -> Inst {
        self.src
    }

    pub fn new_data(src: Inst, ty: Type) -> InstData {
        InstData::new(ty, InstKind::VectorSplat(VectorSplat { src }))
    }
}
