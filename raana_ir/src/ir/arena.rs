use crate::ir::{
    BasicBlockBuilders, LocalBuilder, Type,
    basic_block::{BasicBlock, BasicBlockArena, BasicBlockData},
    builder::ReplaceBuilder,
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

    #[must_use]
    #[inline]
    fn inst_data(&self, inst: Inst) -> &InstData {
        if inst.is_global() {
            self.global().inst_arena.data_of(inst)
        } else {
            self.local().inst_arena.data_of(inst)
        }
    }

    #[must_use]
    #[inline]
    fn bb_data(&self, bb: BasicBlock) -> &BasicBlockData {
        self.local().bb_arena.data_of(bb)
    }

    #[must_use]
    #[inline]
    fn func_data(&self, func: Function) -> &FunctionData {
        self.global().func_arena.data_of(func)
    }

    #[must_use]
    #[inline]
    fn inst_data_mut(&mut self, inst: Inst) -> &mut InstData {
        if inst.is_global() {
            self.global_mut().inst_arena.mut_data_of(inst)
        } else {
            self.local_mut().inst_arena.mut_data_of(inst)
        }
    }

    #[inline]
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

    #[must_use]
    #[inline]
    fn replace_inst_with(&mut self, inst: Inst) -> ReplaceBuilder<'_>
    where
        Self: std::marker::Sized,
    {
        ReplaceBuilder { arena: self, inst }
    }

    #[must_use]
    #[inline]
    fn new_local_value(&mut self) -> LocalBuilder<'_>
    where
        Self: std::marker::Sized,
    {
        LocalBuilder { arena: self }
    }

    #[must_use]
    #[inline]
    fn new_basic_block(&mut self) -> BasicBlockBuilders<'_>
    where
        Self: std::marker::Sized,
    {
        BasicBlockBuilders { arena: self }
    }

    #[inline]
    fn alloc_global_inst(&mut self, data: InstData) -> Inst {
        let id = next_global_inst_id();
        for used in data.inst_usage() {
            self.inst_data_mut(used).used_by_mut().insert(id);
        }
        self.global_mut().inst_arena.alloc(data);
        id
    }

    fn remove_inst(&mut self, inst: Inst) -> InstData {
        self.local_mut().inst_arena.remove(inst)
    }

    #[inline]
    fn alloc_function(&mut self, data: FunctionData) {
        self.global_mut().func_arena.alloc(data);
    }

    #[inline]
    fn alloc_basic_block(&mut self, data: BasicBlockData) -> BasicBlock {
        self.local_mut().bb_arena.alloc(data)
    }

    #[inline]
    fn func_data_mut(&mut self, func: Function) -> &mut FunctionData {
        self.global_mut().func_arena.mut_data_of(func)
    }
}
