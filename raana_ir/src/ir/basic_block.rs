use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
};

use crate::ir::instruction::Inst;

#[derive(Debug, Clone)]
pub struct BasicBlockData {
    name: String,
    params: Vec<Inst>,
    used_by: HashSet<Inst>,
}

impl BasicBlockData {
    pub fn new(name: String, params: Vec<Inst>) -> BasicBlockData {
        BasicBlockData {
            name,
            params,
            used_by: HashSet::new(),
        }
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

#[derive(Debug, Clone)]
pub struct BasicBlockArena {
    data: HashMap<BasicBlock, BasicBlockData>,
    next_id: u32,
}

impl BasicBlockArena {
    pub fn new() -> BasicBlockArena {
        BasicBlockArena {
            data: HashMap::new(),
            next_id: 1,
        }
    }

    pub fn data_of(&self, bb: BasicBlock) -> &BasicBlockData {
        self.data.get(&bb).unwrap()
    }

    pub fn mut_data_of(&mut self, bb: BasicBlock) -> &mut BasicBlockData {
        self.data.get_mut(&bb).unwrap()
    }

    pub fn alloc(&mut self, mut bb_data: BasicBlockData) -> BasicBlock {
        let id = BasicBlock(NonZeroU32::new(self.next_id).unwrap());
        self.next_id += 1;
        bb_data.set_name(format!("{}_{}", bb_data.name(), id.0.get()));
        self.data.insert(id, bb_data);
        id
    }
}
