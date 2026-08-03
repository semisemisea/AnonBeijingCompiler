use crate::ir::{
    Program,
    arena::Arena,
    basic_block::{BasicBlock, BasicBlockData},
    function::Function,
    inst_kind::{
        Aggregate, Binary, BinaryOp, BlockArgRef, Branch, Call, Cast, Float, Fma, GetElemPtr,
        GlobalAlloc, InstKind, Integer, Jump, Load, MemZero, Return, Select, Store, TailCall,
        VectorExtractElement, VectorInsertElement, VectorReduce, VectorReduceOp, VectorSplat,
    },
    instruction::{Inst, InstData},
    types::{Type, TypeKind},
};

pub trait InfoQuery {
    fn inst_type(&self, inst: Inst) -> Type;
    fn is_const(&self, inst: Inst) -> bool;
    fn inst_kind(&self, inst: Inst) -> &InstKind;
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
        assert!(
            lhs_type.is_scalar() || lhs_type.is_vector(),
            "lhs of binary is neither scalar nor vector: {lhs_type}"
        );
        assert!(
            rhs_type.is_scalar() || rhs_type.is_vector(),
            "rhs of binary is neither scalar nor vector: {rhs_type}"
        );
        assert!(
            lhs_type == rhs_type,
            "only the same type is supported currently\ntype of lhs: {lhs_type}\ntype of rhs: {rhs_type}"
        );
        self.insert_inst(Binary::new_data(lhs, rhs, op, lhs_type))
    }

    fn select(&mut self, cond: Inst, if_true: Inst, if_false: Inst) -> Inst {
        let cond_ty = self.inst_type(cond);
        let true_ty = self.inst_type(if_true);
        let false_ty = self.inst_type(if_false);
        assert!(
            cond_ty.is_i32() || cond_ty.is_vector(),
            "select condition must be i32 or a vector mask, got {cond_ty}"
        );
        assert_eq!(
            true_ty, false_ty,
            "select alternatives must have the same type"
        );
        assert!(!true_ty.is_unit(), "select cannot produce a unit value");
        if cond_ty.is_vector() {
            assert_eq!(
                cond_ty, true_ty,
                "vector select condition must match the alternative vector type"
            );
        }
        self.insert_inst(Select::new_data(cond, if_true, if_false, true_ty))
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
        let t_param = self.bb_params(t_target);
        assert_eq!(t_param.len(), t_args.len());
        for (&param, &arg) in t_param.iter().zip(t_args.iter()) {
            assert_eq!(self.inst_type(param), self.inst_type(arg));
        }
        let f_param = self.bb_params(f_target);
        assert_eq!(f_param.len(), f_args.len());
        for (&param, &arg) in f_param.iter().zip(f_args.iter()) {
            assert_eq!(self.inst_type(param), self.inst_type(arg));
        }
        self.insert_inst(Branch::new_data(cond, t_target, t_args, f_target, f_args))
    }

    fn jump(&mut self, target: BasicBlock, args: Vec<Inst>) -> Inst {
        // TODO: Type check
        let params = self.bb_params(target);
        assert_eq!(params.len(), args.len());
        for (&param, &arg) in params.iter().zip(args.iter()) {
            assert_eq!(self.inst_type(param), self.inst_type(arg))
        }
        self.insert_inst(Jump::new_data(target, args))
    }

    fn call(&mut self, callee: Function, args: Vec<Inst>) -> Inst {
        self.insert_inst(Call::new_data(callee, args, self.func_type(callee)))
    }

    /// Constructs a cross-function call when the local builder cannot inspect
    /// the Program-owned callee arena.
    fn call_with_type(&mut self, callee: Function, args: Vec<Inst>, ret_ty: Type) -> Inst {
        self.insert_inst(Call::new_data(callee, args, ret_ty))
    }

    /// A tail call: reuses the current frame and transfers the return value of
    /// `callee(args)` straight to this function's caller. Must end a block.
    fn tail_call(&mut self, callee: Function, args: Vec<Inst>) -> Inst {
        self.insert_inst(TailCall::new_data(callee, args))
    }

    fn cast(&mut self, src: Inst, ty: Type) -> Inst {
        let src_ty = self.inst_type(src);
        assert!(
            src_ty.is_scalar() || src_ty.is_vector(),
            "cast source is neither scalar nor vector: {src_ty}"
        );
        assert!(
            ty.is_scalar() || ty.is_vector(),
            "cast target is neither scalar nor vector: {ty}"
        );
        self.insert_inst(Cast::new_data(src, ty))
    }

    /// panic if base is not a array type.
    fn get_elem_ptr(&mut self, base: Inst, offsets: Vec<Inst>) -> Inst {
        offsets.iter().enumerate().for_each(|(i, &inst)| {
            assert!(
                self.inst_type(inst).is_i32(),
                "`offsets[{i}]`: {inst} should all be integer but receive {}",
                self.inst_type(inst)
            )
        });
        // Each offset traverses one level in the type tree:
        // pointer → deref, array → get elem. After all offsets, wrap in pointer.
        let result_ty = (0..offsets.len())
            .fold(self.inst_type(base), |ty, _i| {
                if ty.is_pointer() {
                    ty.derefernce()
                } else {
                    ty.get_array_info().0
                }
            })
            .reference();
        self.insert_inst(GetElemPtr::new_data(base, offsets, result_ty))
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

    fn mem_zero(&mut self, dest: Inst, byte_len: usize) -> Inst {
        assert!(
            self.inst_type(dest).is_pointer(),
            "memzero destination must be a pointer"
        );
        assert!(byte_len > 0, "memzero byte length must be nonzero");
        self.insert_inst(MemZero::new_data(dest, byte_len))
    }

    /// Fused multiply-add over vectors: `result = acc + lhs * rhs`.
    fn fma(&mut self, acc: Inst, lhs: Inst, rhs: Inst) -> Inst {
        let acc_ty = self.inst_type(acc);
        let lhs_ty = self.inst_type(lhs);
        let rhs_ty = self.inst_type(rhs);
        assert!(
            acc_ty.is_vector(),
            "fma accumulator must be a vector, got {acc_ty}"
        );
        assert_eq!(
            acc_ty, lhs_ty,
            "fma operands must share the accumulator's type"
        );
        assert_eq!(
            acc_ty, rhs_ty,
            "fma operands must share the accumulator's type"
        );
        self.insert_inst(Fma::new_data(acc, lhs, rhs, acc_ty))
    }

    /// Splat a scalar across every lane of `ty`.
    fn vector_splat(&mut self, src: Inst, ty: Type) -> Inst {
        let src_ty = self.inst_type(src);
        assert!(
            src_ty.is_scalar(),
            "vector_splat source must be scalar, got {src_ty}"
        );
        assert!(
            ty.is_vector(),
            "vector_splat target must be a vector, got {ty}"
        );
        let TypeKind::Vector(elem, _lanes) = ty.kind() else {
            unreachable!("vector_splat target is a vector");
        };
        assert_eq!(
            elem, &src_ty,
            "vector_splat element type {elem} must match source type {src_ty}"
        );
        self.insert_inst(VectorSplat::new_data(src, ty))
    }

    /// Extract the lane at constant `index` of a vector into a scalar.
    fn vector_extract_element(&mut self, src: Inst, index: Inst) -> Inst {
        let src_ty = self.inst_type(src);
        assert!(
            src_ty.is_vector(),
            "vector_extract_element source must be a vector, got {src_ty}"
        );
        let TypeKind::Vector(elem, lanes) = src_ty.kind() else {
            unreachable!("vector_extract_element source is a vector");
        };
        assert!(
            self.inst_type(index).is_i32(),
            "vector_extract_element index must be i32, got {}",
            self.inst_type(index)
        );
        assert_lane_constant(self, index, *lanes, "vector_extract_element");
        self.insert_inst(VectorExtractElement::new_data(src, index, elem.clone()))
    }

    /// Insert scalar `element` into lane at constant `index` of `vector`.
    fn vector_insert_element(&mut self, vector: Inst, element: Inst, index: Inst) -> Inst {
        let vector_ty = self.inst_type(vector);
        assert!(
            vector_ty.is_vector(),
            "vector_insert_element vector operand must be a vector, got {vector_ty}"
        );
        let TypeKind::Vector(elem, lanes) = vector_ty.kind() else {
            unreachable!("vector_insert_element vector operand is a vector");
        };
        assert_eq!(
            elem,
            &self.inst_type(element),
            "vector_insert_element element type must match the vector element type"
        );
        assert!(
            self.inst_type(index).is_i32(),
            "vector_insert_element index must be i32, got {}",
            self.inst_type(index)
        );
        assert_lane_constant(self, index, *lanes, "vector_insert_element");
        self.insert_inst(VectorInsertElement::new_data(
            vector, element, index, vector_ty,
        ))
    }

    /// Horizontally reduce `src` with `op`, producing the vector's element type.
    fn vector_reduce(&mut self, op: VectorReduceOp, src: Inst) -> Inst {
        let src_ty = self.inst_type(src);
        assert!(
            src_ty.is_vector(),
            "vector_reduce source must be a vector, got {src_ty}"
        );
        let TypeKind::Vector(elem, _lanes) = src_ty.kind() else {
            unreachable!("vector_reduce source is a vector");
        };
        self.insert_inst(VectorReduce::new_data(op, src, elem.clone()))
    }
}

/// Assert `index` is a constant `Integer` instruction in `[0, lanes)`.
fn assert_lane_constant(builder: &dyn InfoQuery, index: Inst, lanes: usize, what: &str) {
    let InstKind::Integer(idx) = builder.inst_kind(index) else {
        panic!("{what} index must be a constant integer");
    };
    let lane = idx.value();
    assert!(
        (0..lanes as i32).contains(&lane),
        "{what} lane index {lane} out of range [0, {lanes})"
    );
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
        match self.arena().inst_data(inst).kind() {
            InstKind::Aggregate(agg) => agg.value().iter().all(|&elem| self.is_const(elem)),
            kind => kind.is_const(),
        }
    }

    fn inst_kind(&self, inst: Inst) -> &InstKind {
        self.arena().inst_data(inst).kind()
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

#[cfg(test)]
mod tests {
    use super::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder};
    use crate::ir::{Program, Type, arena::Arena, inst_kind::InstKind};

    #[test]
    fn mem_zero_has_unit_type_and_registers_its_destination_use() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "clear".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let alloc = data
            .new_local_inst()
            .alloc(Type::get_array(Type::get_i32(), 4));
        let clear = data.new_local_inst().mem_zero(alloc, 16);

        assert!(data.inst_data(clear).ty().is_unit());
        assert!(matches!(
            data.inst_data(clear).kind(),
            InstKind::MemZero(..)
        ));
        assert_eq!(
            data.inst_data(clear).inst_usage().collect::<Vec<_>>(),
            vec![alloc]
        );
        assert!(data.inst_data(alloc).used_by().contains(&clear));
    }

    #[test]
    #[should_panic(expected = "memzero destination must be a pointer")]
    fn mem_zero_rejects_non_pointer_destinations() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "clear".into(), vec![]);
        let data = program.func_data_mut(function);
        let integer = data.new_local_inst().integer(0);
        data.new_local_inst().mem_zero(integer, 4);
    }

    #[test]
    #[should_panic(expected = "memzero byte length must be nonzero")]
    fn mem_zero_rejects_zero_length() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "clear".into(), vec![]);
        let data = program.func_data_mut(function);
        let alloc = data.new_local_inst().alloc(Type::get_i32());
        data.new_local_inst().mem_zero(alloc, 0);
    }

    #[test]
    fn vector_binary_keeps_the_vector_result_type() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "vec_bin".into(), vec![]);
        let data = program.func_data_mut(function);
        let v4i32 = Type::get_vector(Type::get_i32(), 4);
        let a = data.new_local_inst().undef(v4i32.clone());
        let b = data.new_local_inst().undef(v4i32.clone());
        let add = data.new_local_inst().binary(crate::ir::BinaryOp::Add, a, b);
        assert_eq!(data.inst_data(add).ty().kind(), v4i32.kind());
        // Vector comparisons produce a mask of the operand's vector type, not i32.
        let eq = data.new_local_inst().binary(crate::ir::BinaryOp::Eq, a, b);
        assert_eq!(data.inst_data(eq).ty().kind(), v4i32.kind());
    }

    #[test]
    fn scalar_binary_results_still_force_i32_for_comparisons() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "scalar_cmp".into(), vec![]);
        let data = program.func_data_mut(function);
        let a = data.new_local_inst().integer(1);
        let b = data.new_local_inst().integer(2);
        let eq = data.new_local_inst().binary(crate::ir::BinaryOp::Eq, a, b);
        assert!(data.inst_data(eq).ty().is_i32());
    }

    #[test]
    fn vector_splat_validates_source_element_and_lane_indexes() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "vec_splat".into(), vec![]);
        let data = program.func_data_mut(function);
        let v4i32 = Type::get_vector(Type::get_i32(), 4);

        let src = data.new_local_inst().integer(7);
        let splat = data.new_local_inst().vector_splat(src, v4i32.clone());
        assert!(matches!(
            data.inst_data(splat).kind(),
            InstKind::VectorSplat(..)
        ));
        assert_eq!(
            data.inst_data(splat).inst_usage().collect::<Vec<_>>(),
            vec![src]
        );

        // Extract/insert accept an in-range constant lane.
        let zero = data.new_local_inst().integer(0);
        let extracted = data.new_local_inst().vector_extract_element(splat, zero);
        assert!(data.inst_data(extracted).ty().is_i32());
        let inserted = data
            .new_local_inst()
            .vector_insert_element(splat, src, zero);
        assert_eq!(data.inst_data(inserted).ty().kind(), v4i32.kind());
    }

    #[test]
    #[should_panic(expected = "lane index 4 out of range [0, 4)")]
    fn vector_lane_index_out_of_range_is_rejected() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "vec_lane_oob".into(), vec![]);
        let data = program.func_data_mut(function);
        let v4i32 = Type::get_vector(Type::get_i32(), 4);
        let a = data.new_local_inst().undef(v4i32.clone());
        let four = data.new_local_inst().integer(4);
        data.new_local_inst().vector_extract_element(a, four);
    }

    #[test]
    #[should_panic(expected = "index must be a constant integer")]
    fn vector_lane_index_must_be_constant() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "vec_lane_dyn".into(), vec![]);
        let data = program.func_data_mut(function);
        let v4i32 = Type::get_vector(Type::get_i32(), 4);
        let a = data.new_local_inst().undef(v4i32.clone());
        let index = data.new_local_inst().undef(Type::get_i32());
        data.new_local_inst().vector_insert_element(a, index, index);
    }

    #[test]
    fn fma_and_reduce_validate_their_operand_types() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "vec_fma_reduce".into(), vec![]);
        let data = program.func_data_mut(function);
        let v4f32 = Type::get_vector(Type::get_f32(), 4);
        let acc = data.new_local_inst().undef(v4f32.clone());
        let lhs = data.new_local_inst().undef(v4f32.clone());
        let rhs = data.new_local_inst().undef(v4f32.clone());
        let fma = data.new_local_inst().fma(acc, lhs, rhs);
        assert!(matches!(data.inst_data(fma).kind(), InstKind::Fma(..)));
        assert_eq!(
            data.inst_data(fma).inst_usage().collect::<Vec<_>>(),
            vec![acc, lhs, rhs]
        );

        let reduce = data
            .new_local_inst()
            .vector_reduce(crate::ir::VectorReduceOp::Add, fma);
        assert!(matches!(
            data.inst_data(reduce).kind(),
            InstKind::VectorReduce(..)
        ));
        assert!(data.inst_data(reduce).ty().is_f32());
    }

    #[test]
    #[should_panic(expected = "fma accumulator must be a vector")]
    fn fma_rejects_scalar_accumulators() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "fma_scalar".into(), vec![]);
        let data = program.func_data_mut(function);
        let acc = data.new_local_inst().integer(1);
        let v4i32 = Type::get_vector(Type::get_i32(), 4);
        let lhs = data.new_local_inst().undef(v4i32.clone());
        let rhs = data.new_local_inst().undef(v4i32);
        data.new_local_inst().fma(acc, lhs, rhs);
    }

    #[test]
    fn vector_kind_remap_round_trips_all_operands() {
        use crate::ir::{BasicBlock, Function, Inst, remap::EntityMapper};

        struct IdentityMapper;
        impl EntityMapper for IdentityMapper {
            type Error = ();
            fn map_inst(&mut self, inst: Inst) -> Result<Inst, ()> {
                Ok(inst)
            }
            fn map_block(&mut self, block: BasicBlock) -> Result<BasicBlock, ()> {
                Ok(block)
            }
            fn map_function(&mut self, function: Function) -> Result<Function, ()> {
                Ok(function)
            }
        }

        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "vec_remap".into(), vec![]);
        let data = program.func_data_mut(function);
        let v4i32 = Type::get_vector(Type::get_i32(), 4);
        let a = data.new_local_inst().undef(v4i32.clone());
        let b = data.new_local_inst().undef(v4i32.clone());
        let c = data.new_local_inst().undef(v4i32.clone());
        let src = data.new_local_inst().integer(7);
        let zero = data.new_local_inst().integer(0);

        let fma = data.new_local_inst().fma(a, b, c);
        let splat = data.new_local_inst().vector_splat(src, v4i32.clone());
        let extract = data.new_local_inst().vector_extract_element(splat, zero);
        let insert = data
            .new_local_inst()
            .vector_insert_element(splat, src, zero);
        let reduce = data
            .new_local_inst()
            .vector_reduce(crate::ir::VectorReduceOp::Add, splat);

        for inst in [fma, splat, extract, insert, reduce] {
            let original = data.inst_data(inst);
            let remapped = original.remap_refs(&mut IdentityMapper).unwrap();
            assert_eq!(remapped.ty().kind(), original.ty().kind());
            assert_eq!(
                remapped.inst_usage().collect::<Vec<_>>(),
                original.inst_usage().collect::<Vec<_>>()
            );
        }
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
            self.arena.global_mut().inst_arena.insert(self.inst, data);
        } else {
            self.arena.local_mut().inst_arena.insert(self.inst, data);
        }
        self.inst
    }
}

impl ScalarInstBuilder for ReplaceBuilder<'_> {}
impl LocalInstBuilder for ReplaceBuilder<'_> {}
