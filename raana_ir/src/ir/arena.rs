use crate::ir::{
    basic_block::{BasicBlock, BasicBlockArena, BasicBlockData},
    function::{Function, FunctionArena, FunctionData},
    instruction::{
        GlobalInstArena, Inst, InstData, LocalInstArena, next_global_inst_id, next_local_inst_id,
    },
};

pub struct LocalArena {
    pub(in crate::ir) bb_arena: BasicBlockArena,
    pub(in crate::ir) inst_arena: LocalInstArena,
}

pub struct GlobalArena {
    pub(in crate::ir) func_arena: FunctionArena,
    pub(in crate::ir) inst_arena: GlobalInstArena,
}

impl GlobalArena {
    pub(in crate::ir) fn new() -> Self {
        Self {
            func_arena: FunctionArena::new(),
            inst_arena: GlobalInstArena::new(),
        }
    }

    pub fn inst_arena(&self) -> &GlobalInstArena {
        &self.inst_arena
    }

    pub fn func_arena(&self) -> &FunctionArena {
        &self.func_arena
    }

    pub fn func_arena_mut(&mut self) -> &mut FunctionArena {
        &mut self.func_arena
    }

    pub fn inst_arena_mut(&mut self) -> &mut GlobalInstArena {
        &mut self.inst_arena
    }
}

impl LocalArena {
    pub(in crate::ir) fn new() -> LocalArena {
        LocalArena {
            bb_arena: BasicBlockArena::new(),
            inst_arena: LocalInstArena::new(),
        }
    }

    pub fn bb_arena(&self) -> &BasicBlockArena {
        &self.bb_arena
    }

    pub fn inst_arena(&self) -> &LocalInstArena {
        &self.inst_arena
    }
}

pub trait Arena {
    fn local(&self) -> &LocalArena;
    fn global(&self) -> &GlobalArena;
    fn local_mut(&mut self) -> &mut LocalArena;
    fn global_mut(&mut self) -> &mut GlobalArena;

    fn inst_data(&self, inst: Inst) -> &InstData {
        if inst.is_global() {
            self.global().inst_arena.data_of(inst)
        } else {
            self.local().inst_arena.data_of(inst)
        }
    }

    fn bb_data(&self, bb: BasicBlock) -> &BasicBlockData {
        self.local().bb_arena.data_of(bb)
    }

    fn func_data(&self, func: Function) -> &FunctionData {
        self.global().func_arena.data_of(func)
    }

    fn inst_data_mut(&mut self, inst: Inst) -> &mut InstData {
        if inst.is_global() {
            self.global_mut().inst_arena.mut_data_of(inst)
        } else {
            self.local_mut().inst_arena.mut_data_of(inst)
        }
    }

    fn alloc_local_inst(&mut self, data: InstData) -> Inst {
        let id = next_local_inst_id();
        for used in data.inst_usage() {
            self.inst_data_mut(used).used_by_mut().insert(id);
        }
        for bb in data.bb_usage() {
            self.bb_data_mut(bb).used_by_mut().insert(id);
        }
        self.local_mut().inst_arena.alloc(id, data);
        id
    }

    fn bb_data_mut(&mut self, bb: BasicBlock) -> &mut BasicBlockData {
        self.local_mut().bb_arena.mut_data_of(bb)
    }

    fn alloc_global_inst(&mut self, data: InstData) -> Inst {
        let id = next_global_inst_id();
        for used in data.inst_usage() {
            self.inst_data_mut(used).used_by_mut().insert(id);
        }
        self.global_mut().inst_arena.alloc(data);
        id
    }

    fn alloc_function(&mut self, data: FunctionData) {
        self.global_mut().func_arena.alloc(data);
    }

    fn alloc_basic_block(&mut self, data: BasicBlockData) -> BasicBlock {
        self.local_mut().bb_arena.alloc(data)
    }

    fn func_data_mut(&mut self, func: Function) -> &mut FunctionData {
        self.global_mut().func_arena.mut_data_of(func)
    }
}
