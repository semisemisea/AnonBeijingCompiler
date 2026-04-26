use std::{
    collections::HashSet,
    num::NonZeroU32,
    sync::atomic::{AtomicU32, Ordering},
};

use crate::ir::{
    inst_kind::{
        Aggregate, Binary, BlockArgRef, Branch, Call, Float, FuncArgRef, GetElemPtr, GetPtr,
        GlobalAlloc, Integer, Jump, Load, Return, Store,
    },
    types::Type,
};

#[derive(Debug, Clone)]
pub struct InstData {
    ty: Type,
    name: Option<String>,
    kind: InstKind,
    used_by: HashSet<Inst>,
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

    pub fn used_by_mut(&mut self) -> &mut HashSet<Inst> {
        &mut self.used_by
    }
}

#[derive(Debug, Clone)]
pub enum InstKind {
    Undef,
    ZeroInit,
    Integer(Integer),
    Float(Float),
    Binary(Binary),
    Jump(Jump),
    Branch(Branch),
    Return(Return),
    GetElemPtr(GetElemPtr),
    GetPtr(GetPtr),
    Alloc,
    GlobalAlloc(GlobalAlloc),
    Store(Store),
    Load(Load),
    Call(Call),
    BlockArgRef(BlockArgRef),
    FunkArgRef(FuncArgRef),
    Aggregate(Aggregate),
}

#[derive(Debug, Clone, Copy)]
pub struct Inst(NonZeroU32);

impl std::fmt::Display for Inst {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Inst({})", self.0)
    }
}

static LOCAL_INST_ID: AtomicU32 = AtomicU32::new(0x00000001);

static GLOBAL_INST_ID: AtomicU32 = AtomicU32::new(0x40000000);

pub fn next_local_inst_id() -> Inst {
    Inst(unsafe { NonZeroU32::new_unchecked(LOCAL_INST_ID.fetch_add(1, Ordering::Relaxed)) })
}

pub fn next_global_inst_id() -> Inst {
    Inst(unsafe { NonZeroU32::new_unchecked(GLOBAL_INST_ID.fetch_add(1, Ordering::Relaxed)) })
}

pub struct InstArena {
    data: Vec<InstData>,
}

impl InstArena {
    fn new() -> InstArena {
        InstArena { data: Vec::new() }
    }
}
