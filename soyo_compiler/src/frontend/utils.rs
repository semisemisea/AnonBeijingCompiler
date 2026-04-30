/// Define how a AST node should convert to Koopa IR.
///
/// Required method: `fn convert(&self, ctx: &mut AstGenContext);`
///
/// @param `ctx`: Context that store everything needed to convert.
pub trait ToRaanaIR {
    fn convert(&self, ctx: &mut AstGenContext);

    fn global_convert(&self, ctx: &mut AstGenContext);
}

use super::items::*;
use raana_ir::ir::{
    arena::{Arena, ArenaMut},
    builder_trait::*,
    *,
};
use std::collections::{
    HashMap,
    hash_map::Entry::{Occupied, Vacant},
};

pub type Ident = std::rc::Rc<str>;

#[derive(Debug, Clone, Copy)]
pub enum Symbol {
    Constant(Inst),
    Variable(Inst),
    Callable(Function),
}

pub type SymbolTable = HashMap<Ident, Symbol>;

pub struct AstGenContext {
    pub program: Program,
    func_stack: Vec<Function>,
    val_stack: Vec<Inst>,
    curr_bb: Option<BasicBlock>,
    symbol_table: Vec<SymbolTable>,
    def_type: Option<Type>,
    loop_stack: Vec<(BasicBlock, BasicBlock)>,
}

impl AstGenContext {
    pub fn new() -> AstGenContext {
        AstGenContext {
            program: Program::new(),
            func_stack: Vec::new(),
            val_stack: Vec::new(),
            curr_bb: None,
            symbol_table: vec![SymbolTable::new()],
            def_type: None,
            loop_stack: Vec::new(),
        }
    }

    pub fn get_global_val(&self, inst: Inst) -> Option<Number> {
        if let InstKind::Integer(int) = self.global_arena().inst_data(inst).kind() {
            return Some(int.value());
        }
        None
    }

    pub fn _get_val(&self, ident: &Ident) -> Option<Number> {
        let sym = self.global_scope().get(ident);
        sym.map(|&x| match x {
            Symbol::Constant(int) => {
                let InstKind::Integer(int) = self.global_arena().inst_data(int).kind() else {
                    unreachable!();
                };
                int.value()
            }
            Symbol::Variable(var) => {
                let InstKind::GlobalAlloc(glob_alloc) = self.global_arena().inst_data(var).kind()
                else {
                    unreachable!();
                };
                match self.global_arena().inst_data(glob_alloc.init()).kind() {
                    InstKind::Integer(int) => int.value(),
                    InstKind::ZeroInit => 0,
                    _ => unreachable!(),
                }
            }
            Symbol::Callable(_) => unreachable!(),
        })
    }

    pub fn push_loop(&mut self, entry_bb: BasicBlock, end_bb: BasicBlock) {
        self.loop_stack.push((entry_bb, end_bb));
    }

    pub fn pop_loop(&mut self) {
        self.loop_stack.pop();
    }

    pub fn curr_loop(&self) -> Option<(BasicBlock, BasicBlock)> {
        self.loop_stack.last().copied()
    }

    pub fn add_entry_bb(&mut self) -> BasicBlock {
        let func_data = self.curr_func_data_mut();
        let entry_bb = func_data
            .new_basic_block()
            .basic_block("%entry".into(), vec![]);
        func_data.layout_mut().push_bb_back(entry_bb);
        entry_bb
    }

    pub fn add_scope(&mut self) {
        self.symbol_table.push(HashMap::new());
    }

    pub fn del_scope(&mut self) {
        self.symbol_table.pop();
    }

    pub fn curr_scope(&self) -> &SymbolTable {
        self.symbol_table.last().unwrap()
    }

    pub fn curr_scope_mut(&mut self) -> &mut SymbolTable {
        self.symbol_table.last_mut().unwrap()
    }

    pub fn global_scope(&self) -> &SymbolTable {
        self.symbol_table.first().unwrap()
    }

    pub fn new_global_value(&mut self) -> GlobalBuilder<'_> {
        self.program.new_value()
    }

    #[inline]
    pub fn insert_const(&mut self, ident: Ident, val: Inst) {
        assert!(
            self.curr_scope().get(&ident).is_none(),
            "Redefine the constant {}",
            &*ident
        );
        self.curr_scope_mut().insert(ident, Symbol::Constant(val));
    }

    #[inline]
    pub fn insert_var(&mut self, ident: Ident, val: Inst) {
        assert!(
            // self.global_scope().get(&ident).is_none()
            self.curr_scope().get(&ident).is_none(),
            "Redefine the variable {}",
            &*ident
        );
        self.curr_scope_mut().insert(ident, Symbol::Variable(val));
    }

    #[inline]
    pub fn insert_func(&mut self, ident: Ident, func: Function) {
        debug_assert!(self.symbol_table.len() == 1);
        match self.curr_scope_mut().entry(ident.clone()) {
            Occupied(_) => panic!("Redefine the function {}", &*ident),
            Vacant(e) => {
                e.insert(Symbol::Callable(func));
            }
        }
    }

    #[inline]
    pub fn get_symbol(&self, ident: &Ident) -> Option<Symbol> {
        self.symbol_table
            .iter()
            .rev()
            .find_map(|symbol_table| symbol_table.get(ident).copied())
    }

    #[inline]
    /// cheap version of get_symbol when you want global
    pub fn get_global(&self, ident: &Ident) -> Option<Symbol> {
        self.symbol_table.first().unwrap().get(ident).copied()
    }

    #[inline]
    pub fn push_func(&mut self, func: Function) {
        self.func_stack.push(func);
    }

    #[inline]
    pub fn pop_func(&mut self) -> Option<Function> {
        self.func_stack.pop()
    }

    #[inline]
    pub fn end(self) -> Program {
        self.program
    }

    #[inline]
    pub fn curr_func_data_mut(&mut self) -> &mut FunctionData {
        self.program.func_data_mut(*self.func_stack.last().unwrap())
    }

    #[inline]
    pub fn curr_func_data(&self) -> &FunctionData {
        self.program.func_data(*self.func_stack.last().unwrap())
    }

    pub fn full_arena(&self) -> Arena<'_> {
        Arena::new(
            Some(self.curr_func_data().local_arena()),
            Some(self.program.global_arena()),
        )
    }

    pub fn local_arena(&self) -> Arena<'_> {
        self.curr_func_data().arena()
    }

    pub fn local_arena_mut(&mut self) -> ArenaMut<'_> {
        self.curr_func_data_mut().arena_mut()
    }

    pub fn global_arena(&self) -> Arena<'_> {
        self.program.arena()
    }

    pub fn global_arena_mut(&mut self) -> ArenaMut<'_> {
        self.program.arena_mut()
    }

    #[inline]
    /// A completed basic block means it has end with one of the instruction below.
    /// `br`, `jump`, `ret`
    pub fn is_complete_bb(&self) -> bool {
        let curr_bb = self.curr_bb.unwrap();
        self.curr_func_data()
            .layout()
            .basicblock(curr_bb)
            .insts()
            .get_last()
            .is_some_and(|&inst| {
                matches!(
                    self.local_arena().inst_data(inst).kind(),
                    InstKind::Branch(_) | InstKind::Jump(_) | InstKind::Return(_)
                )
            })
    }

    #[inline]
    /// No effect when a basic block is completed (a.k.a have `br`, `jump` or `ret` at the end)
    pub fn push_inst(&mut self, inst: Inst) {
        let curr_bb = self.curr_bb.unwrap();
        if !self.is_complete_bb() {
            self.curr_func_data_mut()
                .layout_mut()
                .insert_inst(curr_bb, inst);
        }
    }

    pub fn remove_inst(&mut self, inst: Inst) {
        let curr_basic_blcok = self.curr_bb.unwrap();
        let _ = self
            .curr_func_data_mut()
            .layout_mut()
            .remove_inst(curr_basic_blcok, inst);
    }

    #[inline]
    pub fn push_val(&mut self, val: Inst) {
        self.val_stack.push(val);
    }

    #[inline]
    pub fn pop_val(&mut self) -> Option<Inst> {
        self.val_stack.pop()
    }

    // #[inline]
    // fn peek_val(&self) -> Option<&Inst> {
    //     self.val_stack.last()
    // }

    #[must_use]
    #[inline]
    pub fn new_bb(&mut self) -> BasicBlockBuilders<'_> {
        self.curr_func_data_mut().new_basic_block()
    }

    pub fn register_bb(&mut self, bb: BasicBlock) {
        self.curr_func_data_mut().layout_mut().push_bb_back(bb);
    }

    // TODO: pending for layout.
    pub fn remove_bb(&mut self, bb: BasicBlock) {
        todo!()
        // let _ = self.curr_func_data_mut().layout_mut().bbs_mut().remove(&bb);
    }

    #[must_use]
    #[inline]
    pub fn new_local_value(&mut self) -> LocalBuilder<'_> {
        self.curr_func_data_mut().new_local_inst()
    }

    #[inline]
    /// Return the original basic_block handle
    pub fn set_curr_bb(&mut self, bb: BasicBlock) -> Option<BasicBlock> {
        if self.curr_bb.is_some() && !self.is_complete_bb() {
            let ret = self.new_local_value().ret(None);
            self.push_inst(ret);
        }
        self.curr_bb.replace(bb)
    }

    #[inline]
    pub fn reset_curr_bb(&mut self) {
        self.curr_bb = None
    }

    #[inline]
    pub fn bb_params(&self, bb: BasicBlock) -> &[Inst] {
        self.curr_func_data().arena().bb_data(bb).params()
    }

    #[inline]
    pub fn set_def_type(&mut self, ty: Type) -> Option<Type> {
        self.def_type.replace(ty)
    }

    #[inline]
    pub fn curr_def_type(&self) -> Option<Type> {
        self.def_type.clone()
    }

    #[inline]
    pub fn is_constant(&self, l_val: &LVal) -> bool {
        matches!(
            self.curr_scope().get(&l_val.ident),
            Some(Symbol::Constant(_))
        )
    }

    pub fn decl_library_functions(&mut self) {
        let getint = self
            .program
            .new_function(Type::get_i32(), "getint".into(), vec![]);
        self.insert_func(std::rc::Rc::from("getint"), getint);
        let getch = self
            .program
            .new_function(Type::get_i32(), "@getch".into(), vec![]);
        self.insert_func(std::rc::Rc::from("getch"), getch);
        let getarray = self.program.new_function(
            Type::get_i32(),
            "@getarray".into(),
            vec![Type::get_pointer(Type::get_i32())],
        );
        self.insert_func(std::rc::Rc::from("getarray"), getarray);
        let putint =
            self.program
                .new_function(Type::get_unit(), "@putint".into(), vec![Type::get_i32()]);
        self.insert_func(std::rc::Rc::from("putint"), putint);
        let putch =
            self.program
                .new_function(Type::get_unit(), "@putch".into(), vec![Type::get_i32()]);
        self.insert_func(std::rc::Rc::from("putch"), putch);
        let putarray = self.program.new_function(
            Type::get_unit(),
            "@putarray".into(),
            vec![Type::get_i32(), Type::get_pointer(Type::get_i32())],
        );
        self.insert_func(std::rc::Rc::from("putarray"), putarray);
        let starttime = self
            .program
            .new_function(Type::get_unit(), "@starttime".into(), vec![]);
        self.insert_func(std::rc::Rc::from("starttime"), starttime);
        let stoptime = self
            .program
            .new_function(Type::get_unit(), "@stoptime".into(), vec![]);
        self.insert_func(std::rc::Rc::from("stoptime"), stoptime);
    }

    #[inline]
    fn local_val_as_i32(&self, inst: Inst) -> Option<i32> {
        debug_assert!(!inst.is_global());
        match self.local_arena().inst_data(inst).kind() {
            InstKind::Integer(int) => Some(int.value()),
            _ => None,
        }
    }

    #[inline]
    fn global_val_as_i32(&self, inst: Inst) -> Option<i32> {
        debug_assert!(inst.is_global());
        match self.global_arena().inst_data(inst).kind() {
            InstKind::Integer(int) => Some(int.value()),
            _ => None,
        }
    }

    pub fn as_i32(&self, val: Inst) -> Option<i32> {
        if val.is_global() {
            self.global_val_as_i32(val)
        } else {
            self.local_val_as_i32(val)
        }
    }

    #[inline]
    fn global_val_as_i32_val(&mut self, inst: Inst) -> Inst {
        assert!(inst.is_global());
        let int = match self.global_arena().inst_data(inst).kind() {
            InstKind::Integer(int) => int.value(),
            _ => unreachable!(),
        };
        self.curr_func_data_mut().new_local_inst().integer(int)
    }

    pub fn as_i32_val(&mut self, val: Inst) -> Inst {
        if val.is_global() {
            self.global_val_as_i32_val(val)
        } else {
            val
        }
    }

    pub fn pop_i32(&mut self) -> i32 {
        let val = self.pop_val().expect("Value stack is empty");
        self.as_i32(val)
            .unwrap_or_else(|| panic!("Not an integer {:?}", val))
    }

    pub fn set_value_name(&mut self, val: Inst, ident: Ident) {
        if val.is_global() {
            self.global_arena_mut()
                .inst_data_mut(val)
                .set_name(format!("gv_{}", ident.clone()));
        } else {
            self.local_arena_mut()
                .inst_data_mut(inst)
                .set_name(format!("%v_{}", ident.clone()));
        }
    }

    pub fn is_pointer_to_array(&self, inst: Inst) -> bool {
        if inst.is_global() {
            match self.global_arena().inst_data(inst).ty().kind() {
                TypeKind::Pointer(point_to) => matches!(point_to.kind(), TypeKind::Array(..)),
                _ => false,
            }
        } else {
            match self.local_arena().inst_data(inst).ty().kind() {
                TypeKind::Pointer(point_to) => matches!(point_to.kind(), TypeKind::Array(..)),
                _ => false,
            }
        }
    }
}
