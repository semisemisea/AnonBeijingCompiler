use log::info;

use crate::ir::{
    Program,
    arena::Arena,
    basic_block::{BasicBlock, BasicBlockData},
    function::Function,
    inst_kind::{
        Aggregate, Binary, BinaryOp, BlockArgRef, Branch, Call, Cast, Float, GetElemPtr, GetPtr,
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

    fn integer(&mut self, value: i32) -> Inst {
        self.insert_inst(Integer::new_data(value))
    }

    fn float(&mut self, value: f32) -> Inst {
        self.insert_inst(Float::new_data(value))
    }

    /// Undef have type unit here.
    fn undef(&mut self, ty: Type) -> Inst {
        self.insert_inst(InstData::new(ty, InstKind::Undef))
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
        self.insert_inst(Aggregate::new_data(
            Type::get_array(self.inst_type(value[0]), value.len()),
            value,
        ))
    }
}

pub trait LocalInstBuilder: ScalarInstBuilder {
    fn binary(&mut self, op: BinaryOp, lhs: Inst, rhs: Inst) -> Inst {
        let lhs_type = self.inst_type(lhs);
        let rhs_type = self.inst_type(rhs);
        assert!(lhs_type.is_scalar(), "lhs of binary is not scalar");
        assert!(rhs_type.is_scalar(), "rhs of binary is not scalar");
        assert!(
            lhs_type == rhs_type,
            "only the same type is supported currently\ntype of lhs: {lhs_type}\ntype of rhs: {rhs_type}"
        );
        self.insert_inst(Binary::new_data(lhs, rhs, op, lhs_type))
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

    fn cast(&mut self, src: Inst, ty: Type) -> Inst {
        let src_ty = self.inst_type(src);
        assert!(src_ty.is_scalar(), "cast source is not scalar");
        assert!(ty.is_scalar(), "cast target is not scalar");
        self.insert_inst(Cast::new_data(src, ty))
    }

    /// panic is base is not a pointer type.
    fn get_ptr(&mut self, base: Inst, offset: Inst) -> Inst {
        self.insert_inst(GetPtr::new_data(base, offset, self.inst_type(base)))
    }

    /// panic if base is not a array type.
    fn get_elem_ptr(&mut self, base: Inst, offset: Inst) -> Inst {
        self.insert_inst(GetElemPtr::new_data(
            base,
            offset,
            // must be a pointer to an array.
            Type::get_pointer(self.inst_type(base).derefernce().get_array_elem_ty()),
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
        self.insert_inst(GlobalAlloc::new_data(
            init,
            Type::get_pointer(self.inst_type(init)),
        ))
    }

    fn zero_init(&mut self, ty: Type) -> Inst {
        // TODO: type check.
        self.insert_inst(InstData::new(ty, InstKind::ZeroInit))
    }
}

pub trait BasicBlockBuilder: Sized + InstInsert + ArenaQuery {
    fn insert_bb(&mut self, data: BasicBlockData) -> BasicBlock;
    fn bb_data_mut(&mut self, bb: BasicBlock) -> &mut BasicBlockData;

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

    fn add_param(&mut self, bb: BasicBlock, ty: Type) -> Inst {
        // TODO: This will work for now if you don't rely on method `BlockArgRef::index(&self) -> usize`
        // The previous code didn't use the index.
        let bar = self.insert_inst(BlockArgRef::new_data(self.bb_params(bb).len() + 1, ty));
        self.bb_data_mut(bb).params_mut().push(bar);
        bar
    }

    fn remove_param(&mut self, bb: BasicBlock, index: usize) {
        let data = self.bb_data_mut(bb);
        data.params_mut().remove(index);
    }
}

pub trait ArenaQuery {
    fn arena(&self) -> &dyn Arena;
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
    pub arena: &'a mut dyn Arena,
}

impl ArenaQuery for LocalBuilder<'_> {
    fn arena(&self) -> &dyn Arena {
        self.arena
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
    fn arena(&self) -> &dyn Arena {
        self.program
    }
}

impl InstInsert for GlobalBuilder<'_> {
    fn insert_inst(&mut self, data: InstData) -> Inst {
        let is_global_alloc = matches!(data.kind(), InstKind::GlobalAlloc(..));
        let id = self.program.alloc_global_inst(data);
        if is_global_alloc {
            self.program.inst_layout_push(id);
        }
        id
    }
}

impl ScalarInstBuilder for GlobalBuilder<'_> {}

impl GlobalInstBuilder for GlobalBuilder<'_> {}

pub struct BasicBlockBuilders<'a> {
    pub(crate) arena: &'a mut dyn Arena,
}

impl ArenaQuery for BasicBlockBuilders<'_> {
    fn arena(&self) -> &dyn Arena {
        self.arena
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

    fn bb_data_mut(&mut self, bb: BasicBlock) -> &mut BasicBlockData {
        self.arena.bb_data_mut(bb)
    }
}

pub struct ReplaceBuilder<'a> {
    pub(crate) arena: &'a mut dyn Arena,
    pub(crate) inst: Inst,
}

impl ArenaQuery for ReplaceBuilder<'_> {
    fn arena(&self) -> &dyn Arena {
        self.arena
    }
}

impl InstInsert for ReplaceBuilder<'_> {
    fn insert_inst(&mut self, mut data: InstData) -> Inst {
        let old_data = if self.inst.is_global() {
            self.arena.global_mut().inst_arena.remove(self.inst)
        } else {
            self.arena.local_mut().inst_arena.remove(self.inst)
        };
        for used in old_data.inst_usage() {
            self.arena
                .inst_data_mut(used)
                .used_by_mut()
                .remove(&self.inst);
        }
        for bb in old_data.bb_usage() {
            self.arena.bb_data_mut(bb).used_by_mut().remove(&self.inst);
        }
        for used in data.inst_usage() {
            self.arena
                .inst_data_mut(used)
                .used_by_mut()
                .insert(self.inst);
        }
        for bb in data.bb_usage() {
            self.arena.bb_data_mut(bb).used_by_mut().insert(self.inst);
        }
        data.used_by = old_data.used_by;
        if self.inst.is_global() {
            self.arena.global_mut().inst_arena.alloc(self.inst, data);
        } else {
            self.arena.local_mut().inst_arena.alloc(self.inst, data);
        }
        self.inst
    }
}

impl ScalarInstBuilder for ReplaceBuilder<'_> {}
impl LocalInstBuilder for ReplaceBuilder<'_> {}
