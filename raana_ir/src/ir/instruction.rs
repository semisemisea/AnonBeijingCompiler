use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
    sync::atomic::{AtomicU32, Ordering},
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

static LOCAL_INST_ID: AtomicU32 = AtomicU32::new(LOCAL_ID_START_FROM);

static GLOBAL_INST_ID: AtomicU32 = AtomicU32::new(GLOBAL_ID_START_FROM);

pub(crate) fn reset() {
    LOCAL_INST_ID.store(LOCAL_ID_START_FROM, Ordering::Relaxed);
    GLOBAL_INST_ID.store(GLOBAL_ID_START_FROM, Ordering::Relaxed);
}

pub(in crate::ir) fn next_local_inst_id() -> Inst {
    Inst(unsafe { NonZeroU32::new_unchecked(LOCAL_INST_ID.fetch_add(1, Ordering::Relaxed)) })
}

pub(in crate::ir) fn next_global_inst_id() -> Inst {
    Inst(unsafe { NonZeroU32::new_unchecked(GLOBAL_INST_ID.fetch_add(1, Ordering::Relaxed)) })
}

#[derive(Debug, Clone)]
pub struct LocalInstArena {
    data: HashMap<Inst, InstData>,
}

#[derive(Debug, Clone)]
pub struct GlobalInstArena {
    data: HashMap<Inst, InstData>,
}

impl LocalInstArena {
    pub fn new() -> LocalInstArena {
        LocalInstArena {
            data: HashMap::new(),
        }
    }

    pub fn alloc(&mut self, inst: Inst, data: InstData) {
        self.data.insert(inst, data);
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

impl GlobalInstArena {
    pub fn new() -> GlobalInstArena {
        GlobalInstArena {
            data: HashMap::new(),
        }
    }

    pub fn alloc(&mut self, inst: Inst, data: InstData) {
        self.data.insert(inst, data);
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

    pub fn datas(&self) -> std::collections::hash_map::Iter<'_, Inst, InstData> {
        self.data.iter()
    }
}
