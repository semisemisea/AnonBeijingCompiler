use crate::ir::{
    Program,
    arena::{Arena, ArenaMut},
    basic_block::{BasicBlock, BasicBlockData},
    function::Function,
    inst_kind::{
        Aggregate, Binary, BinaryOp, BlockArgRef, Branch, Call, Float, GetElemPtr, GetPtr,
        GlobalAlloc, InstKind, Integer, Jump, Load, Return, Store,
    },
    instruction::{Inst, InstData},
    types::Type,
};

pub trait InfoQuery {
    fn inst_type(&self, inst: Inst) -> Type;
    fn is_const(&self, inst: Inst) -> bool;
    fn bb_params(&self, bb: BasicBlock) -> &[Inst];
    fn func_type(&self, func: Function) -> Type;
}

pub trait InstInsert {
    fn insert_inst(&mut self, data: InstData) -> Inst;
}

pub trait ScalarInstBuilder: InstInsert + InfoQuery + Sized {
    fn raw(&mut self, data: InstData) -> Inst {
        self.insert_inst(data)
    }

    fn interger(&mut self, value: i32) -> Inst {
        self.insert_inst(Integer::new_data(value))
    }

    fn float(&mut self, value: f32) -> Inst {
        self.insert_inst(Float::new_data(value))
    }

    /// Undef have type unit here.
    fn undef(&mut self) -> Inst {
        self.insert_inst(InstData::new(Type::get_unit(), InstKind::Undef))
    }

    fn aggregate(&mut self, value: Vec<Inst>) -> Inst {
        assert!(!value.is_empty(), "aggregate elements cannot be empty");
        for (index, &elem) in value.iter().enumerate() {
            assert!(
                self.is_const(elem),
                "each elements should be constant but index {index}: {elem} is not"
            );
            assert_eq!(
                self.inst_type(elem),
                self.inst_type(value[0]),
                "find inconsistent elements type.\nfirst element {elem} is type of {}\nbut index {index}: {elem} is type of {}",
                self.inst_type(value[0]),
                self.inst_type(elem)
            );
        }
        self.insert_inst(Aggregate::new_data(self.inst_type(value[0]), value))
    }
}

pub trait LocalInstBuilder: ScalarInstBuilder {
    fn binary(&mut self, lhs: Inst, rhs: Inst, op: BinaryOp) -> Inst {
        let lhs_type = self.inst_type(lhs);
        let rhs_type = self.inst_type(rhs);
        assert!(lhs_type.is_scalar(), "lhs of binary is not scalar");
        assert!(rhs_type.is_scalar(), "rhs of binary is not scalar");
        assert!(
            lhs_type == rhs_type,
            "only the same type is supported currently\ntype of lhs: {lhs_type}\ntype of rhs: {rhs_type}"
        );
        // let ty = if lhs_type.is_i32() && rhs_type.is_i32() && !matches!(op ,BinaryOp::Div)
        self.insert_inst(Binary::new_data(lhs, rhs, op, lhs_type))
        // add_used_by?
    }

    fn branch(
        &mut self,
        cond: Inst,
        t_target: BasicBlock,
        t_args: Vec<Inst>,
        f_target: BasicBlock,
        f_args: Vec<Inst>,
    ) -> Inst {
        // TODO: Type check
        self.insert_inst(Branch::new_data(cond, t_target, t_args, f_target, f_args))
    }

    fn jump(&mut self, target: BasicBlock, args: Vec<Inst>) -> Inst {
        // TODO: Type check
        self.insert_inst(Jump::new_data(target, args))
    }

    fn call(&mut self, callee: Function, args: Vec<Inst>) -> Inst {
        self.insert_inst(Call::new_data(callee, args, self.func_type(callee)))
    }

    /// panic is base is not a pointer type.
    fn get_ptr(&mut self, base: Inst, offset: Inst) -> Inst {
        self.insert_inst(GetPtr::new_data(
            base,
            offset,
            self.inst_type(base).derefernce(),
        ))
    }

    /// panic if base is not a array type.
    fn get_elem_ptr(&mut self, base: Inst, offset: Inst) -> Inst {
        self.insert_inst(GetElemPtr::new_data(
            base,
            offset,
            self.inst_type(base).get_array_elem_ty(),
        ))
    }

    fn ret(&mut self, value: Option<Inst>) -> Inst {
        self.insert_inst(Return::new_data(value))
    }

    /// panic if you `ty` is unit type.
    fn alloc(&mut self, ty: Type) -> Inst {
        assert!(!ty.is_unit(), "Cannot allocate a unit type");
        self.insert_inst(InstData::new(Type::get_pointer(ty), InstKind::Alloc))
    }

    /// panic if `src` is not a pointer type.
    fn load(&mut self, src: Inst) -> Inst {
        self.insert_inst(Load::new_data(src, self.inst_type(src).derefernce()))
    }

    fn store(&mut self, src: Inst, dest: Inst) -> Inst {
        self.insert_inst(Store::new_data(src, dest))
    }
}

pub trait GlobalInstBuilder: ScalarInstBuilder {
    fn global_alloc(&mut self, init: Inst) -> Inst {
        self.insert_inst(GlobalAlloc::new_data(init, self.inst_type(init)))
    }
}

pub trait BasicBlockBuilder: Sized + InstInsert {
    fn insert_bb(&mut self, data: BasicBlockData) -> BasicBlock;

    /// return all the instruction of parameter. Give it name if you want.
    fn basic_block(&mut self, name: String, params_ty: Vec<Type>) -> BasicBlock {
        assert!(
            params_ty.iter().all(|p| !p.is_unit()),
            "parameter type must not be `unit`!"
        );
        let params: Vec<Inst> = params_ty
            .iter()
            .enumerate()
            .map(|(i, ty)| self.insert_inst(BlockArgRef::new_data(i, ty.clone())))
            .collect();
        self.insert_bb(BasicBlockData::new(name, params))
    }
}

pub trait ArenaQuery {
    fn arena(&self) -> Arena;
}

impl<T: ArenaQuery> InfoQuery for T {
    fn inst_type(&self, inst: Inst) -> Type {
        self.arena().inst_data(inst).ty().clone()
    }

    fn is_const(&self, inst: Inst) -> bool {
        self.arena().inst_data(inst).kind().is_const()
    }

    fn bb_params(&self, bb: BasicBlock) -> &[Inst] {
        self.arena().bb_data(bb).params()
    }

    fn func_type(&self, func: Function) -> Type {
        self.arena().func_data(func).ret_ty().clone()
    }
}

pub struct LocalBuilder<'a> {
    pub(crate) arena: ArenaMut<'a>,
}

impl ArenaQuery for LocalBuilder<'_> {
    fn arena(&self) -> Arena {
        self.arena.freeze()
    }
}

impl InstInsert for LocalBuilder<'_> {
    fn insert_inst(&mut self, data: InstData) -> Inst {
        self.arena.alloc_local_inst(data)
    }
}

impl ScalarInstBuilder for LocalBuilder<'_> {}

impl LocalInstBuilder for LocalBuilder<'_> {}

pub struct GlobalBuilder<'a> {
    pub(in crate::ir) program: &'a mut Program,
}

impl ArenaQuery for GlobalBuilder<'_> {
    fn arena(&self) -> Arena {
        Arena::new(None, Some(self.program.global_arena()))
    }
}

impl InstInsert for GlobalBuilder<'_> {
    fn insert_inst(&mut self, data: InstData) -> Inst {
        let id = ArenaMut::new(None, Some(self.program.global_arena_mut())).alloc_global_inst(data);
        self.program.inst_layout_push(id);
        id
    }
}

impl ScalarInstBuilder for GlobalBuilder<'_> {}

impl GlobalInstBuilder for GlobalBuilder<'_> {}

pub struct BasicBlockBuilders<'a> {
    pub(crate) arena: ArenaMut<'a>,
}

impl ArenaQuery for BasicBlockBuilders<'_> {
    fn arena(&self) -> Arena {
        self.arena.freeze()
    }
}

impl InstInsert for BasicBlockBuilders<'_> {
    fn insert_inst(&mut self, data: InstData) -> Inst {
        self.arena.alloc_local_inst(data)
    }
}

impl BasicBlockBuilder for BasicBlockBuilders<'_> {
    fn insert_bb(&mut self, data: BasicBlockData) -> BasicBlock {
        self.arena.alloc_basic_block(data)
    }
}
