use tomori_utils::{
    PrimaryMap, entity_impl,
    iter::{Iter, IterMut, Keys},
};

use crate::ir::{
    arena::{Arena, LocalArena},
    basic_block::BasicBlock,
    builder::{BasicBlockBuilders, LocalBuilder},
    inst_kind::FuncArgRef,
    instruction::Inst,
    layout::Layout,
    types::Type,
};

pub struct FunctionData {
    ret_ty: Type,
    name: String,
    params: Vec<Inst>,
    layout: Layout,
    local_arena: LocalArena,
}

impl Arena for FunctionData {
    fn local(&self) -> &LocalArena {
        self.local_arena()
    }

    fn global(&self) -> &super::arena::GlobalArena {
        unimplemented!()
    }

    fn local_mut(&mut self) -> &mut LocalArena {
        self.local_arena_mut()
    }

    fn global_mut(&mut self) -> &mut super::arena::GlobalArena {
        unimplemented!()
    }
}

impl FunctionData {
    pub fn new(ret_ty: Type, name: String, params_ty: Vec<Type>) -> FunctionData {
        let local_arena = LocalArena::new();
        let mut fd = FunctionData {
            ret_ty,
            name,
            params: vec![],
            layout: Layout::new(),
            local_arena,
        };
        let params = params_ty
            .iter()
            .enumerate()
            .map(|(i, ty)| fd.alloc_local_inst(FuncArgRef::new_data(i, ty.clone())))
            .collect();
        fd.params = params;
        fd
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn layout_mut(&mut self) -> &mut Layout {
        &mut self.layout
    }

    pub fn new_local_inst(&mut self) -> LocalBuilder<'_> {
        LocalBuilder { arena: self }
    }

    pub fn new_basic_block(&mut self) -> BasicBlockBuilders<'_> {
        BasicBlockBuilders { arena: self }
    }

    pub fn set_name(&mut self, name: String) {
        self.name = name;
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn ret_ty(&self) -> &Type {
        &self.ret_ty
    }

    pub fn params(&self) -> &[Inst] {
        &self.params
    }

    pub fn local_arena(&self) -> &LocalArena {
        &self.local_arena
    }

    pub fn local_arena_mut(&mut self) -> &mut LocalArena {
        &mut self.local_arena
    }

    pub fn remove_layout_inst(&mut self, bb: BasicBlock, inst: Inst) {
        self.detach_inst_usage(inst);
        self.layout_mut().remove_inst(bb, inst);
        self.remove_inst_data(inst);
    }

    pub fn detach_layout_inst(&mut self, bb: BasicBlock, inst: Inst) {
        self.detach_inst_usage(inst);
        self.layout_mut().remove_inst(bb, inst);
    }

    pub fn remove_orphan_inst(&mut self, inst: Inst) {
        self.detach_inst_usage(inst);
        self.remove_inst_data(inst);
    }

    pub fn remove_layout_basicblock(&mut self, bb: BasicBlock) {
        let insts = self
            .layout()
            .basicblock(bb)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for inst in insts {
            self.detach_inst_usage(inst);
            self.remove_inst_data(inst);
        }
        self.layout_mut().remove_basicblock(bb);
    }

    fn detach_inst_usage(&mut self, inst: Inst) {
        let inst_usage = self.inst_data(inst).inst_usage().collect::<Vec<_>>();
        for used in inst_usage {
            if self.has_inst_data(used) {
                self.inst_data_mut(used).used_by_mut().remove(&inst);
            }
        }

        let bb_usage = self.inst_data(inst).bb_usage().collect::<Vec<_>>();
        for used in bb_usage {
            if self.has_layout_bb(used) {
                self.bb_data_mut(used).used_by_mut().remove(&inst);
            }
        }
    }

    fn has_inst_data(&self, inst: Inst) -> bool {
        !inst.is_global()
            && self
                .local_arena()
                .inst_arena()
                .datas()
                .any(|(&key, _)| key == inst)
    }

    fn has_layout_bb(&self, bb: BasicBlock) -> bool {
        self.layout()
            .basicblocks()
            .iter()
            .any(|layout| layout.bb() == bb)
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct Function(u32);
entity_impl!(Function);

pub struct FunctionArena {
    data: PrimaryMap<Function, FunctionData>,
}

impl FunctionArena {
    pub fn new() -> FunctionArena {
        FunctionArena {
            data: PrimaryMap::new(),
        }
    }

    pub fn data_of(&self, func: Function) -> &FunctionData {
        &self.data[func]
    }

    pub fn mut_data_of(&mut self, func: Function) -> &mut FunctionData {
        &mut self.data[func]
    }

    pub fn alloc(&mut self, func_data: FunctionData) -> Function {
        self.data.push(func_data)
    }

    pub fn functions(&self) -> Iter<'_, Function, FunctionData> {
        self.data.iter()
    }

    pub fn functions_mut(&mut self) -> IterMut<'_, Function, FunctionData> {
        self.data.iter_mut()
    }

    pub fn funcs(&self) -> Keys<Function> {
        self.data.keys()
    }
}
