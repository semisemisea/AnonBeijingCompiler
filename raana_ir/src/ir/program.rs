use crate::ir::{
    arena::{Arena, ArenaMut, GlobalArena},
    builder::GlobalBuilder,
    function::{Function, FunctionData, next_function_id},
    instruction::Inst,
    types::Type,
};

pub struct Program {
    global_arena: GlobalArena,
    function_layout: Vec<Function>,
    global_inst_layout: Vec<Inst>,
}

impl Program {
    pub fn new() -> Program {
        Program {
            global_arena: GlobalArena::new(),
            function_layout: Vec::new(),
            global_inst_layout: Vec::new(),
        }
    }

    pub fn new_value(&mut self) -> GlobalBuilder<'_> {
        GlobalBuilder { program: self }
    }

    pub fn global_arena(&self) -> &GlobalArena {
        &self.global_arena
    }

    pub fn global_arena_mut(&mut self) -> &mut GlobalArena {
        &mut self.global_arena
    }

    pub fn arena_mut(&mut self) -> ArenaMut<'_> {
        ArenaMut::new_global(&mut self.global_arena)
    }

    pub fn arena(&self) -> Arena<'_> {
        Arena::new_global(&self.global_arena)
    }

    pub fn func_data(&self, func: Function) -> &FunctionData {
        self.global_arena.func_arena.data_of(func)
    }

    pub fn func_data_mut(&mut self, func: Function) -> &mut FunctionData {
        self.global_arena.func_arena.mut_data_of(func)
    }

    pub fn global_inst_layout(&self) -> &[Inst] {
        &self.global_inst_layout
    }

    pub fn inst_layout_push(&mut self, inst: Inst) {
        self.global_inst_layout.push(inst);
    }

    pub fn func_layout_push(&mut self, func: Function) {
        self.function_layout.push(func);
    }

    pub fn new_function(&mut self, ret_ty: Type, name: String, params_ty: Vec<Type>) -> Function {
        self.arena_mut()
            .alloc_function(FunctionData::new(ret_ty, name, params_ty));
        let id = next_function_id();
        self.func_layout_push(id);
        id
    }

    pub fn function_layout(&self) -> &[Function] {
        &self.function_layout
    }
}
