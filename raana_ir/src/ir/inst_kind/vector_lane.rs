use crate::ir::{
    inst_kind::InstKind,
    instruction::{Inst, InstData},
    types::Type,
};

/// Extract one lane of a vector into a scalar: `dst = src[index]`.
///
/// `index` must be a constant `Integer` instruction in `[0, lanes)`; machine
/// lane-insert instructions require an immediate lane.
#[derive(Debug, Clone)]
pub struct VectorExtractElement {
    src: Inst,
    index: Inst,
}

impl VectorExtractElement {
    pub fn src(&self) -> Inst {
        self.src
    }

    pub fn index(&self) -> Inst {
        self.index
    }

    pub fn new_data(src: Inst, index: Inst, ty: Type) -> InstData {
        InstData::new(
            ty,
            InstKind::VectorExtractElement(VectorExtractElement { src, index }),
        )
    }
}

/// Insert a scalar into one lane of a vector: `dst = vector` with
/// `dst[index] = element`. `index` must be a constant `Integer` instruction
/// in `[0, lanes)`.
#[derive(Debug, Clone)]
pub struct VectorInsertElement {
    vector: Inst,
    element: Inst,
    index: Inst,
}

impl VectorInsertElement {
    pub fn vector(&self) -> Inst {
        self.vector
    }

    pub fn element(&self) -> Inst {
        self.element
    }

    pub fn index(&self) -> Inst {
        self.index
    }

    pub fn new_data(vector: Inst, element: Inst, index: Inst, ty: Type) -> InstData {
        InstData::new(
            ty,
            InstKind::VectorInsertElement(VectorInsertElement {
                vector,
                element,
                index,
            }),
        )
    }
}
