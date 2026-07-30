use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
};

use crate::ir::{
    inst_kind::{
        Aggregate, BasicBlockUsage, Binary, Branch, Call, Cast, GetElemPtr, GlobalAlloc, InstKind,
        InstUsage, Jump, Load, MemZero, Return, Select, Store, TailCall,
    },
    remap::EntityMapper,
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

    pub(crate) fn remap_refs<M: EntityMapper>(&self, mapper: &mut M) -> Result<Self, M::Error> {
        let mapped_kind = match self.kind() {
            InstKind::Undef
            | InstKind::ZeroInit
            | InstKind::Integer(..)
            | InstKind::Float(..)
            | InstKind::Alloc
            | InstKind::BlockArgRef(..) => self.kind().clone(),
            InstKind::Binary(binary) => {
                Binary::new_data(
                    mapper.map_inst(binary.lhs())?,
                    mapper.map_inst(binary.rhs())?,
                    binary.op(),
                    self.ty().clone(),
                )
                .kind
            }
            InstKind::Select(select) => {
                Select::new_data(
                    mapper.map_inst(select.cond())?,
                    mapper.map_inst(select.if_true())?,
                    mapper.map_inst(select.if_false())?,
                    self.ty().clone(),
                )
                .kind
            }
            InstKind::Jump(jump) => {
                Jump::new_data(
                    mapper.map_block(jump.target())?,
                    jump.args()
                        .iter()
                        .map(|&arg| mapper.map_inst(arg))
                        .collect::<Result<Vec<_>, _>>()?,
                )
                .kind
            }
            InstKind::Branch(branch) => {
                Branch::new_data(
                    mapper.map_inst(branch.cond())?,
                    mapper.map_block(branch.t_target())?,
                    branch
                        .t_args()
                        .iter()
                        .map(|&arg| mapper.map_inst(arg))
                        .collect::<Result<Vec<_>, _>>()?,
                    mapper.map_block(branch.f_target())?,
                    branch
                        .f_args()
                        .iter()
                        .map(|&arg| mapper.map_inst(arg))
                        .collect::<Result<Vec<_>, _>>()?,
                )
                .kind
            }
            InstKind::Cast(cast) => {
                Cast::new_data(mapper.map_inst(cast.src())?, self.ty().clone()).kind
            }
            InstKind::Return(ret) => {
                Return::new_data(
                    ret.value()
                        .map(|value| mapper.map_inst(value))
                        .transpose()?,
                )
                .kind
            }
            InstKind::GetElemPtr(gep) => {
                GetElemPtr::new_data(
                    mapper.map_inst(gep.base())?,
                    gep.offsets()
                        .iter()
                        .map(|&offset| mapper.map_inst(offset))
                        .collect::<Result<Vec<_>, _>>()?,
                    self.ty().clone(),
                )
                .kind
            }
            InstKind::GlobalAlloc(alloc) => {
                GlobalAlloc::new_data(mapper.map_inst(alloc.init())?, self.ty().clone()).kind
            }
            InstKind::Store(store) => {
                Store::new_data(
                    mapper.map_inst(store.src())?,
                    mapper.map_inst(store.dest())?,
                )
                .kind
            }
            InstKind::MemZero(mem_zero) => {
                MemZero::new_data(mapper.map_inst(mem_zero.dest())?, mem_zero.byte_len()).kind
            }
            InstKind::Load(load) => {
                Load::new_data(mapper.map_inst(load.src())?, self.ty().clone()).kind
            }
            InstKind::Call(call) => {
                Call::new_data(
                    mapper.map_function(call.callee())?,
                    call.args()
                        .iter()
                        .map(|&arg| mapper.map_inst(arg))
                        .collect::<Result<Vec<_>, _>>()?,
                    self.ty().clone(),
                )
                .kind
            }
            InstKind::TailCall(call) => {
                TailCall::new_data(
                    mapper.map_function(call.callee())?,
                    call.args()
                        .iter()
                        .map(|&arg| mapper.map_inst(arg))
                        .collect::<Result<Vec<_>, _>>()?,
                )
                .kind
            }
            InstKind::Aggregate(aggregate) => {
                Aggregate::new_data(
                    self.ty().clone(),
                    aggregate
                        .value()
                        .iter()
                        .map(|&value| mapper.map_inst(value))
                        .collect::<Result<Vec<_>, _>>()?,
                )
                .kind
            }
        };

        Ok(Self {
            ty: self.ty.clone(),
            name: self.name.clone(),
            kind: mapped_kind,
            used_by: HashSet::new(),
        })
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

#[derive(Debug, Clone)]
pub struct LocalInstArena {
    data: HashMap<Inst, InstData>,
    next_id: u32,
}

#[derive(Debug, Clone)]
pub struct GlobalInstArena {
    data: HashMap<Inst, InstData>,
    next_id: u32,
}

impl LocalInstArena {
    pub fn new() -> LocalInstArena {
        LocalInstArena {
            data: HashMap::new(),
            next_id: LOCAL_ID_START_FROM,
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

impl GlobalInstArena {
    pub fn new() -> GlobalInstArena {
        GlobalInstArena {
            data: HashMap::new(),
            next_id: GLOBAL_ID_START_FROM,
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

    pub fn insert(&mut self, inst: Inst, data: InstData) {
        self.data.insert(inst, data);
    }

    pub fn datas(&self) -> std::collections::hash_map::Iter<'_, Inst, InstData> {
        self.data.iter()
    }
}
