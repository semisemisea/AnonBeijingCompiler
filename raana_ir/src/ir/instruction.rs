use std::{
    collections::HashSet,
    num::NonZeroU32,
    sync::atomic::{AtomicU32, Ordering},
};

use crate::ir::{
    inst_kind::{
        Binary, BlockArgRef, Branch, Call, Float, FuncArgRef, GetElemPtr, GetPtr, GlobalAlloc,
        Integer, Jump, Load, Return, Store,
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
}

pub type Inst = NonZeroU32;

static LOCAL_INST_ID: AtomicU32 = AtomicU32::new(0x00000001);

static GLOBAL_INST_ID: AtomicU32 = AtomicU32::new(0x40000000);

pub fn next_local_inst_id() -> Inst {
    unsafe { NonZeroU32::new_unchecked(LOCAL_INST_ID.fetch_add(1, Ordering::Relaxed)) }
}

pub fn next_global_inst_id() -> Inst {
    unsafe { NonZeroU32::new_unchecked(GLOBAL_INST_ID.fetch_add(1, Ordering::Relaxed)) }
}
