use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
    sync::atomic::{AtomicU32, Ordering},
};

use index_list::IndexList;

use crate::ir::instruction::Inst;

#[derive(Debug, Clone)]
pub struct BasicBlockData {
    name: String,
    params: Vec<Inst>,
    insts: IndexList<Inst>,
    used_by: HashSet<Inst>,
}

impl BasicBlockData {
    pub fn new(name: String, params: Vec<Inst>) -> BasicBlockData {
        BasicBlockData {
            name,
            params,
            insts: IndexList::new(),
            used_by: HashSet::new(),
        }
    }

    pub fn insts(&self) -> &IndexList<Inst> {
        &self.insts
    }

    pub fn insts_mut(&mut self) -> &mut IndexList<Inst> {
        &mut self.insts
    }

    pub fn params(&self) -> &Vec<Inst> {
        &self.params
    }

    pub fn used_by(&self) -> &HashSet<Inst> {
        &self.used_by
    }

    pub fn used_by_mut(&mut self) -> &mut HashSet<Inst> {
        &mut self.used_by
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn set_name(&mut self, name: String) {
        self.name = name;
    }

    pub fn params_mut(&mut self) -> &mut Vec<Inst> {
        &mut self.params
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BasicBlock(NonZeroU32);

static BBID: AtomicU32 = AtomicU32::new(1);

pub(crate) fn reset() {
    BBID.store(1, Ordering::Relaxed);
}

fn next_bbid() -> BasicBlock {
    BasicBlock(unsafe { NonZeroU32::new_unchecked(BBID.fetch_add(1, Ordering::Relaxed)) })
}

#[derive(Debug, Clone)]
pub struct BasicBlockArena {
    data: HashMap<BasicBlock, BasicBlockData>,
}

impl BasicBlockArena {
    pub fn new() -> BasicBlockArena {
        BasicBlockArena {
            data: HashMap::new(),
        }
    }

    pub fn data_of(&self, bb: BasicBlock) -> &BasicBlockData {
        self.data.get(&bb).unwrap()
    }

    pub fn mut_data_of(&mut self, bb: BasicBlock) -> &mut BasicBlockData {
        self.data.get_mut(&bb).unwrap()
    }

    pub fn alloc(&mut self, bb_data: BasicBlockData) -> BasicBlock {
        let id = next_bbid();
        self.data.insert(id, bb_data);
        id
    }
}
