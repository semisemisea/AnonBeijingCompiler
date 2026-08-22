use crate::{
    fmt::writer::Writer,
    ir::{
        arena::{Arena, GlobalArena},
        builder::GlobalBuilder,
        function::{Function, FunctionData},
        instruction::Inst,
        types::Type,
    },
};

pub struct Program {
    global_arena: GlobalArena,
    function_layout: Vec<Function>,
    global_inst_layout: Vec<Inst>,
    main_function: Option<Function>,
}

impl Arena for Program {
    fn local(&self) -> &super::arena::LocalArena {
        unimplemented!()
    }

    fn global(&self) -> &GlobalArena {
        self.global_arena()
    }

    fn local_mut(&mut self) -> &mut super::arena::LocalArena {
        unimplemented!()
    }

    fn global_mut(&mut self) -> &mut GlobalArena {
        self.global_arena_mut()
    }
}

impl std::fmt::Display for Program {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut w = Writer::new(self);
        w.write().unwrap();
        write!(f, "{}", w.finish())
    }
}

impl Program {
    pub fn new() -> Program {
        Program {
            global_arena: GlobalArena::new(),
            function_layout: Vec::new(),
            global_inst_layout: Vec::new(),
            main_function: None,
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
        let set_flag = name.eq("main");
        let id = self
            .global_arena
            .func_arena
            .alloc(FunctionData::new(ret_ty, name, params_ty));
        self.func_layout_push(id);
        if set_flag {
            self.main_function = Some(id);
        }
        id
    }

    pub fn get_main_function(&self) -> Function {
        self.main_function.unwrap()
    }

    /// Whether a `main` function was declared. Passes that require an entry
    /// point (e.g. IPSCCP's ICFG worklist) call [`Self::get_main_function`]
    /// which panics without one; empty compilation units (empty source,
    /// declarations only) must be rejected before reaching them.
    pub fn has_main(&self) -> bool {
        self.main_function.is_some()
    }

    pub fn function_layout(&self) -> &[Function] {
        &self.function_layout
    }

    /// Remove `func` from the program's function layout. The `FunctionData`
    /// stays in the arena (function handles are index-based and removing
    /// from the arena would invalidate every later handle), so a removed
    /// function is unreachable to layout-walking passes but its handle
    /// remains queryable.
    pub fn remove_function(&mut self, func: Function) {
        self.function_layout.retain(|&f| f != func);
    }
}
