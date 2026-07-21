use crate::ir::{
    arena::Arena,
    inst_kind::InstKind,
    instruction::{Inst, InstData},
    types::Type,
};

#[derive(Debug, Clone)]
pub struct Aggregate {
    value: Vec<Inst>,
}

impl Aggregate {
    pub fn value(&self) -> &[Inst] {
        &self.value
    }

    pub fn new_data(ty: Type, value: Vec<Inst>) -> InstData {
        InstData::new(ty, InstKind::Aggregate(Aggregate { value }))
    }

    pub fn flatten(&self, arena: &dyn Arena) -> Vec<Inst> {
        let mut v = vec![];
        for &val in self.value.iter() {
            let data = arena.inst_data(val);
            match data.kind() {
                InstKind::Aggregate(agg) => v.extend(agg.flatten(arena)),
                _ => v.push(val),
            }
        }
        v
    }
}
