use crate::ir::{
    basic_block::{BasicBlock, BasicBlockArena, BasicBlockData},
    function::{next_function_id, Function, FunctionArena, FunctionData},
    instruction::{
        next_global_inst_id, next_local_inst_id, GlobalInstArena, Inst, InstData, LocalInstArena,
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

pub struct Arena<'a> {
    pub(crate) local: Option<&'a LocalArena>,
    pub(crate) global: Option<&'a GlobalArena>,
}

impl<'a> Arena<'a> {
    pub fn new(local: Option<&'a LocalArena>, global: Option<&'a GlobalArena>) -> Arena<'a> {
        Arena { local, global }
    }
    pub fn new_local(local: &'a LocalArena) -> Arena<'a> {
        Arena {
            local: Some(local),
            global: None,
        }
    }

    pub fn new_global(global: &'a GlobalArena) -> Arena<'a> {
        Arena {
            local: None,
            global: Some(global),
        }
    }

    pub fn inst_data(&self, inst: Inst) -> &'a InstData {
        if inst.is_global() {
            self.global.unwrap().inst_arena.data_of(inst)
        } else {
            self.local.unwrap().inst_arena.data_of(inst)
        }
    }

    pub fn bb_data(&self, bb: BasicBlock) -> &'a BasicBlockData {
        self.local.unwrap().bb_arena.data_of(bb)
    }

    pub fn func_data(&self, func: Function) -> &'a FunctionData {
        self.global.unwrap().func_arena.data_of(func)
    }
}

pub struct ArenaMut<'a> {
    pub(crate) local: Option<&'a mut LocalArena>,
    pub(crate) global: Option<&'a mut GlobalArena>,
}

impl<'a> ArenaMut<'a> {
    pub fn new(
        local: Option<&'a mut LocalArena>,
        global: Option<&'a mut GlobalArena>,
    ) -> ArenaMut<'a> {
        ArenaMut { local, global }
    }

    pub fn new_local(local: &'a mut LocalArena) -> ArenaMut<'a> {
        ArenaMut {
            local: Some(local),
            global: None,
        }
    }

    pub fn new_global(global: &'a mut GlobalArena) -> ArenaMut<'a> {
        ArenaMut {
            local: None,
            global: Some(global),
        }
    }

    pub fn alloc_local_inst(&mut self, data: InstData) -> Inst {
        let id = next_local_inst_id();
        for used in data.inst_usage() {
            self.inst_data_mut(used).used_by_mut().insert(id);
        }
        for bb in data.bb_usage() {
            self.bb_data_mut(bb).used_by_mut().insert(id);
        }
        self.local.as_mut().unwrap().inst_arena.alloc(data);
        id
    }

    pub fn inst_data_mut(&mut self, inst: Inst) -> &mut InstData {
        if inst.is_global() {
            self.global.as_mut().unwrap().inst_arena.mut_data_of(inst)
        } else {
            self.local.as_mut().unwrap().inst_arena.mut_data_of(inst)
        }
    }

    pub fn bb_data_mut(&mut self, bb: BasicBlock) -> &mut BasicBlockData {
        self.local.as_mut().unwrap().bb_arena.mut_data_of(bb)
    }

    pub fn alloc_global_inst(&mut self, data: InstData) -> Inst {
        let id = next_global_inst_id();
        for used in data.inst_usage() {
            self.inst_data_mut(used).used_by_mut().insert(id);
        }
        self.global.as_mut().unwrap().inst_arena.alloc(data);
        id
    }

    pub fn alloc_function(&mut self, data: FunctionData) {
        self.global.as_mut().unwrap().func_arena.alloc(data);
    }

    pub fn alloc_basic_block(&mut self, data: BasicBlockData) -> BasicBlock {
        self.local.as_mut().unwrap().bb_arena.alloc(data)
    }

    pub fn freeze(&self) -> Arena {
        Arena::new(self.local.as_deref(), self.global.as_deref())
    }

    pub fn func_data(&self, func: Function) -> &FunctionData {
        self.global.as_ref().unwrap().func_arena.data_of(func)
    }

    pub fn func_data_mut(&mut self, func: Function) -> &mut FunctionData {
        self.global.as_mut().unwrap().func_arena.mut_data_of(func)
    }
}
