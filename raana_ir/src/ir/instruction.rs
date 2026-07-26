use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
};

use crate::ir::{
    inst_kind::{BasicBlockUsage, InstKind, InstUsage},
    types::Type,
};

#[derive(Debug)]
pub struct InstData {
    ty: Type,
    name: Option<String>,
    kind: InstKind,
    pub(crate) used_by: HashSet<Inst>,
}

impl Clone for InstData {
    /// With empty used_by hashset.
    fn clone(&self) -> Self {
        InstData {
            ty: self.ty.clone(),
            name: self.name.clone(),
            kind: self.kind.clone(),
            used_by: HashSet::new(),
        }
    }
}

impl InstData {
    /// With no name and empty used_by set.
    pub fn new(ty: Type, kind: InstKind) -> InstData {
        InstData {
            ty,
            name: None,
            kind,
            used_by: HashSet::new(),
        }
    }

    pub fn set_name(&mut self, name: String) {
        self.name = Some(name);
    }

    pub fn used_by(&self) -> &HashSet<Inst> {
        &self.used_by
    }

    pub(in crate::ir) fn used_by_mut(&mut self) -> &mut HashSet<Inst> {
        &mut self.used_by
    }

    pub fn ty(&self) -> &Type {
        &self.ty
    }

    pub fn kind(&self) -> &InstKind {
        &self.kind
    }

    pub fn is_const(&self) -> bool {
        self.kind().is_const()
    }

    pub fn inst_usage(&self) -> InstUsage<'_> {
        InstUsage {
            data: self.kind(),
            index: 0,
        }
    }

    pub fn bb_usage(&self) -> BasicBlockUsage<'_> {
        BasicBlockUsage {
            data: self.kind(),
            index: 0,
        }
    }

    pub fn name(&self) -> Option<&String> {
        self.name.as_ref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Inst(NonZeroU32);

impl Inst {
    pub fn is_global(&self) -> bool {
        self.0.get() >= GLOBAL_ID_START_FROM
    }
}

impl std::fmt::Display for Inst {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Inst({})", self.0)
    }
}

const LOCAL_ID_START_FROM: u32 = 0x00000001;

const GLOBAL_ID_START_FROM: u32 = 0x40000000;

/// Instruction storage handing out ids counting up from `START_ID`.
///
/// Local and global instructions live in arenas seeded from non-overlapping id
/// ranges, so that [`Inst::is_global`] alone tells which arena owns an id. The
/// seed is a const parameter so that the two aliases below stay distinct types.
#[derive(Debug, Clone)]
pub struct InstArena<const START_ID: u32> {
    data: HashMap<Inst, InstData>,
    next_id: u32,
}

pub type LocalInstArena = InstArena<LOCAL_ID_START_FROM>;

pub type GlobalInstArena = InstArena<GLOBAL_ID_START_FROM>;

impl<const START_ID: u32> InstArena<START_ID> {
    pub fn new() -> Self {
        Self {
            data: HashMap::new(),
            next_id: START_ID,
        }
    }

    pub fn alloc(&mut self, data: InstData) -> Inst {
        let inst = Inst(NonZeroU32::new(self.next_id).unwrap());
        self.next_id += 1;
        self.data.insert(inst, data);
        inst
    }

    pub fn data_of(&self, inst: Inst) -> &InstData {
        self.data.get(&inst).unwrap()
    }

    pub fn mut_data_of(&mut self, inst: Inst) -> &mut InstData {
        self.data.get_mut(&inst).unwrap()
    }

    pub fn remove(&mut self, inst: Inst) -> InstData {
        self.data.remove(&inst).unwrap()
    }

    pub fn insert(&mut self, inst: Inst, new_data: InstData) {
        self.data.insert(inst, new_data);
    }

    pub fn datas(&self) -> std::collections::hash_map::Iter<'_, Inst, InstData> {
        self.data.iter()
    }
}
