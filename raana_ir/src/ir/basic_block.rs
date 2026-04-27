use std::{
    collections::HashSet,
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
}

#[derive(Debug, Clone, Copy)]
pub struct BasicBlock(NonZeroU32);

static BBID: AtomicU32 = AtomicU32::new(1);

fn next_bbid() -> BasicBlock {
    BasicBlock(unsafe { NonZeroU32::new_unchecked(BBID.fetch_add(1, Ordering::Relaxed)) })
}

#[derive(Debug, Clone)]
pub struct BasicBlockArena {
    data: Vec<BasicBlockData>,
}

impl BasicBlockArena {
    pub fn new() -> BasicBlockArena {
        BasicBlockArena { data: Vec::new() }
    }

    pub fn data_of(&self, bb: BasicBlock) -> &BasicBlockData {
        &self.data[(bb.0.get() - 1) as usize]
    }

    pub fn mut_data_of(&mut self, bb: BasicBlock) -> &mut BasicBlockData {
        &mut self.data[(bb.0.get() - 1) as usize]
    }

    pub fn alloc(&mut self, bb_data: BasicBlockData) -> BasicBlock {
        self.data.push(bb_data);
        next_bbid()
    }
}
