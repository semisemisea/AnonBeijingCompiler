//! Base-object (GetBaseObject) and intra-procedural alias analysis for SysY IR.
//!
//! SysY has no pointer type, no address-of operator, no casts and no heap
//! allocation. Every pointer value therefore ultimately derives from one of
//! three base objects:
//!
//! - a function-local stack object (`InstKind::Alloc`),
//! - a global object (`InstKind::GlobalAlloc`),
//! - an array parameter (a pointer-typed entry-block parameter).
//!
//! The only indirection in the IR is the frontend's *pointer slot* pattern
//! (`alloc <**T>; store %param, %slot; ... load %slot`), which `BaseEnv`
//! resolves through a store-once slot map. Block parameters (phi values)
//! that carry pointers are resolved through their incoming edge arguments.
//!
//! The static alias rules follow the SysY memory model (see
//! `docs/memory_alias_analysis.md` §3.2): different local objects never
//! alias each other, locals never alias globals, and a local never aliases
//! an argument of the same function. Argument-vs-global and
//! argument-vs-argument pairs are conservatively `MayAlias` here; the
//! interprocedural points-to refinement lives in `analysis_passes/effects.rs`.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    ir::{BasicBlock, Function, FunctionData, Inst, InstKind, arena::Arena},
    opt::utils::gep::gep_index_stride,
};
/// The base object a pointer value ultimately derives from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemObject {
    /// A function-local stack object, identified by its `Alloc` instruction.
    Alloc(Inst),
    /// A global object, identified by its `GlobalAlloc` instruction.
    Global(Inst),
    /// The function's array parameter at the given index.
    Param(usize),
    /// Anything else (unknown provenance).
    Unknown,
}

impl MemObject {
    pub fn is_unknown(&self) -> bool {
        matches!(self, MemObject::Unknown)
    }
}

/// Alias classification between two memory accesses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasResult {
    /// The two accesses can never touch overlapping bytes.
    NoAlias,
    /// The two accesses may touch overlapping bytes (conservative).
    MayAlias,
    /// The two accesses provably touch the same bytes.
    MustAlias,
}

impl AliasResult {
    /// True when the pair must be treated as conflicting.
    pub fn may_alias(self) -> bool {
        self != AliasResult::NoAlias
    }
}

/// Per-function environment for base-object tracing.
///
/// Built once per function; all queries are read-only afterwards. The
/// base/offset tables are precomputed by [`BaseEnv::build_tables`] with a
/// fixed-point pass over the function's instructions and block parameters
/// (call it once after construction, with an arena that can reach global
/// instructions). Queries are then O(1) table lookups — no per-query
/// recursion.
pub struct BaseEnv {
    /// Position of each block parameter within its owning block, used to
    /// resolve pointer-valued block parameters through incoming edges.
    param_position: FxHashMap<Inst, (BasicBlock, usize)>,
    /// Pointer slots: an `Alloc` written exactly once (the frontend's
    /// `alloc <**T>; store %param, %slot` pattern) -> the stored value.
    slots: FxHashMap<Inst, Inst>,
    /// The function's entry parameters (`FunctionData::params()`).
    entry_params: Vec<Inst>,
    /// Precomputed base object of every address-shaped instruction.
    base_table: FxHashMap<Inst, MemObject>,
    /// Precomputed byte offset of every address-shaped instruction
    /// (`None` entry = offset not statically known).
    offset_table: FxHashMap<Inst, Option<i64>>,
}

impl BaseEnv {
    pub fn new(data: &FunctionData) -> BaseEnv {
        let mut param_position = FxHashMap::default();
        for bb_layout in data.layout().basicblocks() {
            let bb = bb_layout.bb();
            for (index, &param) in data.bb_data(bb).params().iter().enumerate() {
                param_position.insert(param, (bb, index));
            }
        }

        // Store-once pointer slots. A slot written more than once, or
        // touched by MemZero, is not resolvable. Global destinations are
        // skipped: only function-local Allocs can be pointer slots, and
        // `FunctionData` cannot inspect global instructions.
        let mut stored: FxHashMap<Inst, Inst> = FxHashMap::default();
        let mut ambiguous: FxHashSet<Inst> = FxHashSet::default();
        for bb_layout in data.layout().basicblocks() {
            for &inst in bb_layout.insts() {
                match data.inst_data(inst).kind() {
                    InstKind::Store(store) => {
                        let dest = store.dest();
                        if !dest.is_global()
                            && matches!(data.inst_data(dest).kind(), InstKind::Alloc)
                        {
                            if ambiguous.contains(&dest) {
                                continue;
                            }
                            if stored.insert(dest, store.src()).is_some() {
                                stored.remove(&dest);
                                ambiguous.insert(dest);
                            }
                        }
                    }
                    InstKind::MemZero(mem_zero) => {
                        let dest = mem_zero.dest();
                        if !dest.is_global()
                            && matches!(data.inst_data(dest).kind(), InstKind::Alloc)
                        {
                            stored.remove(&dest);
                            ambiguous.insert(dest);
                        }
                    }
                    _ => {}
                }
            }
        }

        BaseEnv {
            param_position,
            slots: stored,
            entry_params: data.params().to_vec(),
            base_table: FxHashMap::default(),
            offset_table: FxHashMap::default(),
        }
    }

    /// The base object `ptr` ultimately derives from.
    ///
    /// O(1) lookup into the table precomputed by [`Self::build_tables`];
    /// instructions missing from the table (or global objects, which are
    /// resolved directly) conservatively yield `Unknown`.
    pub fn base_of<A: Arena + ?Sized>(&self, arena: &A, ptr: Inst) -> MemObject {
        if matches!(arena.inst_data(ptr).kind(), InstKind::GlobalAlloc(..)) {
            return MemObject::Global(ptr);
        }
        self.base_table
            .get(&ptr)
            .copied()
            .unwrap_or(MemObject::Unknown)
    }

    /// Precompute the base and offset tables with a fixed-point pass.
    ///
    /// Must be called once after construction (with an arena that can reach
    /// global instructions, e.g. `ArenaContext`); afterwards all queries
    /// are O(1) lookups. The work table distinguishes "not yet resolved"
    /// (`None`) from "resolved to unknown" (`Some(Unknown)`), so cyclic
    /// phi chains (loop backedges) converge conservatively to `Unknown`
    /// instead of oscillating.
    pub fn build_tables<A: Arena + ?Sized>(&mut self, arena: &A, func: Function) {
        let data = arena.func_data(func);
        let mut insts: Vec<Inst> = Vec::new();
        for bb_layout in data.layout().basicblocks() {
            insts.extend(bb_layout.insts().iter().copied());
            insts.extend(arena.bb_data(bb_layout.bb()).params().iter().copied());
        }

        // ---- Base fixed point. ----
        let mut base_work: FxHashMap<Inst, Option<MemObject>> =
            insts.iter().map(|&i| (i, None)).collect();
        loop {
            let mut changed = false;
            for &ptr in &insts {
                let next = self.compute_base(arena, ptr, &base_work);
                let cur = base_work.get_mut(&ptr).unwrap();
                if *cur != next {
                    *cur = next;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        self.base_table = base_work
            .into_iter()
            .map(|(k, v)| (k, v.unwrap_or(MemObject::Unknown)))
            .collect();

        // ---- Offset fixed point. ----
        let mut off_work: FxHashMap<Inst, Option<Option<i64>>> =
            insts.iter().map(|&i| (i, None)).collect();
        loop {
            let mut changed = false;
            for &ptr in &insts {
                let next = self.compute_offset(arena, ptr, &off_work);
                let cur = off_work.get_mut(&ptr).unwrap();
                if *cur != next {
                    *cur = next;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        self.offset_table = off_work
            .into_iter()
            .map(|(k, v)| (k, v.flatten()))
            .collect();
    }

    /// Fixed-point step for the base of `ptr`.
    ///
    /// `work` maps every address-shaped instruction to its current base
    /// (`None` = not resolved yet). Dependencies that are not yet resolved
    /// keep the current entry pending; `Some(Unknown)` is a settled
    /// unknown (no further refinement).
    fn compute_base<A: Arena + ?Sized>(
        &self,
        arena: &A,
        ptr: Inst,
        work: &FxHashMap<Inst, Option<MemObject>>,
    ) -> Option<MemObject> {
        match arena.inst_data(ptr).kind() {
            InstKind::Alloc => Some(MemObject::Alloc(ptr)),
            InstKind::GlobalAlloc(..) => Some(MemObject::Global(ptr)),
            InstKind::GetElemPtr(gep) => {
                if matches!(
                    arena.inst_data(gep.base()).kind(),
                    InstKind::GlobalAlloc(..)
                ) {
                    return Some(MemObject::Global(gep.base()));
                }
                work.get(&gep.base()).copied().flatten()
            }
            InstKind::Load(load) => match self.slots.get(&load.src()) {
                Some(&stored) => {
                    if matches!(arena.inst_data(stored).kind(), InstKind::GlobalAlloc(..)) {
                        Some(MemObject::Global(stored))
                    } else {
                        work.get(&stored).copied().flatten()
                    }
                }
                None => Some(MemObject::Unknown),
            },
            InstKind::BlockArgRef(..) => {
                if let Some(index) = self.entry_params.iter().position(|&p| p == ptr) {
                    return Some(MemObject::Param(index));
                }
                let Some(&(block, index)) = self.param_position.get(&ptr) else {
                    return Some(MemObject::Unknown);
                };
                let mut result: Option<MemObject> = None;
                let mut all_set = true;
                for &user in arena.bb_data(block).used_by() {
                    let arg = match arena.inst_data(user).kind() {
                        InstKind::Jump(jump) if jump.target() == block => {
                            jump.args().get(index).copied()
                        }
                        InstKind::Branch(branch) => {
                            if branch.t_target() == block {
                                branch.t_args().get(index).copied()
                            } else if branch.f_target() == block {
                                branch.f_args().get(index).copied()
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                    let Some(arg) = arg else {
                        return Some(MemObject::Unknown);
                    };
                    match work.get(&arg) {
                        None => all_set = false,
                        Some(Some(MemObject::Unknown)) => return Some(MemObject::Unknown),
                        Some(Some(m)) => match result {
                            Some(prev) if prev != *m => return Some(MemObject::Unknown),
                            _ => result = Some(*m),
                        },
                        Some(None) => all_set = false,
                    }
                }
                if all_set {
                    result.or(Some(MemObject::Unknown))
                } else {
                    None
                }
            }
            _ => Some(MemObject::Unknown),
        }
    }

    /// Fixed-point step for the byte offset of `ptr`.
    ///
    /// The work value is `Option<Option<i64>>`: outer `None` = pending,
    /// `Some(None)` = settled non-constant, `Some(Some(off))` = constant.
    fn compute_offset<A: Arena + ?Sized>(
        &self,
        arena: &A,
        ptr: Inst,
        work: &FxHashMap<Inst, Option<Option<i64>>>,
    ) -> Option<Option<i64>> {
        match arena.inst_data(ptr).kind() {
            InstKind::Alloc | InstKind::GlobalAlloc(..) => Some(Some(0)),
            InstKind::BlockArgRef(..) => {
                // ABI parameters (the function's entry block) are bare
                // pointers: offset zero.
                if self.entry_params.iter().any(|&p| p == ptr) {
                    return Some(Some(0));
                }
                let Some(&(block, index)) = self.param_position.get(&ptr) else {
                    return Some(None);
                };
                let mut result: Option<i64> = None;
                let mut all_set = true;
                for &user in arena.bb_data(block).used_by() {
                    let arg = match arena.inst_data(user).kind() {
                        InstKind::Jump(jump) if jump.target() == block => {
                            jump.args().get(index).copied()
                        }
                        InstKind::Branch(branch) => {
                            if branch.t_target() == block {
                                branch.t_args().get(index).copied()
                            } else if branch.f_target() == block {
                                branch.f_args().get(index).copied()
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                    let Some(arg) = arg else {
                        return Some(None);
                    };
                    match work.get(&arg) {
                        None => all_set = false,
                        Some(Some(None)) => return Some(None),
                        Some(Some(Some(off))) => {
                            if result.is_none() {
                                result = Some(*off);
                            } else if result != Some(*off) {
                                return Some(None);
                            }
                        }
                        Some(None) => all_set = false,
                    }
                }
                if all_set { Some(result) } else { None }
            }
            InstKind::GetElemPtr(gep) => {
                let base_off = if matches!(
                    arena.inst_data(gep.base()).kind(),
                    InstKind::GlobalAlloc(..)
                ) {
                    Some(Some(0))
                } else {
                    work.get(&gep.base()).copied().flatten()
                };
                let mut off = match base_off {
                    None => return None, // base pending
                    Some(None) => return Some(None),
                    Some(Some(o)) => o,
                };
                for (pos, &idx) in gep.offsets().iter().enumerate() {
                    let Some(idx_value) = integer_constant(arena, idx) else {
                        return Some(None);
                    };
                    let Some(stride) = gep_index_stride(arena, ptr, pos) else {
                        return Some(None);
                    };
                    let Some(sum) = off.checked_add(idx_value as i64 * stride.byte_stride) else {
                        return Some(None);
                    };
                    off = sum;
                }
                Some(Some(off))
            }
            InstKind::Load(load) => match self.slots.get(&load.src()) {
                Some(&stored) => {
                    if matches!(arena.inst_data(stored).kind(), InstKind::GlobalAlloc(..)) {
                        Some(Some(0))
                    } else {
                        work.get(&stored).copied().flatten()
                    }
                }
                None => Some(None),
            },
            _ => Some(None),
        }
    }

    /// Byte offset of `ptr` relative to its base object, or `None` when any
    /// index along the GEP chain is not a compile-time constant.
    ///
    /// The base object itself (a bare Alloc/GlobalAlloc/parameter) has
    /// offset zero; block parameters resolve through their incoming edge
    /// arguments (precomputed by [`Self::build_tables`]).
    pub fn constant_offset<A: Arena + ?Sized>(&self, arena: &A, ptr: Inst) -> Option<i64> {
        if matches!(arena.inst_data(ptr).kind(), InstKind::GlobalAlloc(..)) {
            return Some(0);
        }
        self.offset_table.get(&ptr).copied().flatten()
    }

    /// Intra-procedural alias result between two addresses, refined by
    /// constant-offset interval disjointness on the same base.
    ///
    /// Argument-vs-argument and argument-vs-global pairs stay conservative
    /// `MayAlias`; the interprocedural refinement is `EffectAnalysis::alias`
    /// in `analysis_passes/effects.rs`.
    pub fn alias<A: Arena + ?Sized>(&self, arena: &A, a: Inst, b: Inst) -> AliasResult {
        if a == b {
            return AliasResult::MustAlias;
        }
        let base_a = self.base_of(arena, a);
        let base_b = self.base_of(arena, b);
        self.alias_with_bases(arena, a, b, base_a, base_b)
    }

    /// Same as [`Self::alias`] with precomputed bases.
    pub fn alias_with_bases<A: Arena + ?Sized>(
        &self,
        arena: &A,
        a: Inst,
        b: Inst,
        base_a: MemObject,
        base_b: MemObject,
    ) -> AliasResult {
        use MemObject::*;
        match (base_a, base_b) {
            (Unknown, _) | (_, Unknown) => AliasResult::MayAlias,
            (Alloc(x), Alloc(y)) if x != y => AliasResult::NoAlias,
            (Global(x), Global(y)) if x != y => AliasResult::NoAlias,
            (Alloc(..), Global(..)) | (Global(..), Alloc(..)) => AliasResult::NoAlias,
            (Alloc(..), Param(..)) | (Param(..), Alloc(..)) => AliasResult::NoAlias,
            // Argument pairs are conservatively MayAlias without points-to.
            (Param(..), Param(..)) | (Param(..), Global(..)) | (Global(..), Param(..)) => {
                AliasResult::MayAlias
            }
            // Same base: refine by constant offsets.
            _ => self.offset_alias(arena, a, b),
        }
    }

    /// Same-base refinement: constant offsets and access sizes decide
    /// NoAlias (disjoint byte intervals) or MustAlias (same start offset);
    /// anything else stays MayAlias.
    fn offset_alias<A: Arena + ?Sized>(&self, arena: &A, a: Inst, b: Inst) -> AliasResult {
        let (Some(off_a), Some(off_b)) = (
            self.constant_offset(arena, a),
            self.constant_offset(arena, b),
        ) else {
            return AliasResult::MayAlias;
        };
        let size_a = access_size(arena, a);
        let size_b = access_size(arena, b);
        if off_a + size_a <= off_b || off_b + size_b <= off_a {
            AliasResult::NoAlias
        } else if off_a == off_b {
            AliasResult::MustAlias
        } else {
            AliasResult::MayAlias
        }
    }
}

/// Size in bytes of the memory access through `addr` (the pointee size of
/// the address's pointer type).
pub fn access_size<A: Arena + ?Sized>(arena: &A, addr: Inst) -> i64 {
    arena.inst_data(addr).ty().derefernce().size() as i64
}

/// The value of `inst` when it is an `Integer` constant, else `None`.
pub fn integer_constant<A: Arena + ?Sized>(arena: &A, inst: Inst) -> Option<i32> {
    match arena.inst_data(inst).kind() {
        InstKind::Integer(value) => Some(value.value()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{Program, Type, arena::Arena, builder_trait::*},
        opt::pass::ArenaContext,
    };

    fn env_of(program: &Program, func: crate::ir::Function) -> (BaseEnv, ArenaContext<'_>) {
        let data = program.func_data(func);
        let mut env = BaseEnv::new(data);
        let ctx = ArenaContext {
            program,
            curr_func: Some(func),
        };
        env.build_tables(&ctx, func);
        (env, ctx)
    }

    /// A fresh zero-initialized global of the given pointee type.
    fn new_global(program: &mut Program, pointee: Type) -> Inst {
        let init = program.new_value().zero_init(pointee);
        program.new_value().global_alloc(init)
    }

    #[test]
    fn base_of_alloc_and_global() {
        let mut program = Program::new();
        let global = new_global(&mut program, Type::get_i32());
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let alloc = data.new_local_inst().alloc(Type::get_i32());
        data.layout_mut().insert_inst(entry, alloc);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.base_of(&ctx, alloc), MemObject::Alloc(alloc));
        assert_eq!(env.base_of(&ctx, global), MemObject::Global(global));
    }

    #[test]
    fn base_of_gep_walks_to_base() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let param = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let gep = data.new_local_inst().get_elem_ptr(param, vec![zero]);
        data.layout_mut().insert_inst(entry, gep);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.base_of(&ctx, gep), MemObject::Param(0));
    }

    #[test]
    fn base_of_resolves_frontend_pointer_slot() {
        // `%slot = alloc <**T>; store %param, %slot; %p = load %slot`
        // must trace back to the parameter (the frontend array-argument
        // lowering pattern).
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let param = data.params()[0];
        let slot = data
            .new_local_inst()
            .alloc(Type::get_i32().reference().reference());
        data.layout_mut().insert_inst(entry, slot);
        let store = data.new_local_inst().store(param, slot);
        data.layout_mut().insert_inst(entry, store);
        let load = data.new_local_inst().load(slot);
        data.layout_mut().insert_inst(entry, load);
        let zero = data.new_local_inst().integer(0);
        let gep = data.new_local_inst().get_elem_ptr(load, vec![zero]);
        data.layout_mut().insert_inst(entry, gep);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.base_of(&ctx, load), MemObject::Param(0));
        assert_eq!(env.base_of(&ctx, gep), MemObject::Param(0));
    }

    #[test]
    fn base_of_load_of_non_slot_is_unknown() {
        // A pointer load whose source is not a store-once slot (here: a
        // value loaded through a parameter-derived address) must yield
        // Unknown instead of a wrong base object.
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let param = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let addr = data.new_local_inst().get_elem_ptr(param, vec![zero]);
        data.layout_mut().insert_inst(entry, addr);
        let loaded = data.new_local_inst().load(addr);
        data.layout_mut().insert_inst(entry, loaded);
        let slot = data
            .new_local_inst()
            .alloc(Type::get_i32().reference().reference());
        data.layout_mut().insert_inst(entry, slot);
        // The slot receives a value whose provenance is unknown; it must not
        // be treated as a resolvable pointer slot.
        let store = data.new_local_inst().store(loaded, slot);
        data.layout_mut().insert_inst(entry, store);
        // The pointer load from the slot: the slot was written by a
        // parameter-derived address, so it must resolve to Unknown, not Param.
        let reload = data.new_local_inst().load(slot);
        data.layout_mut().insert_inst(entry, reload);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.base_of(&ctx, reload), MemObject::Unknown);
    }

    #[test]
    fn constant_offset_of_flat_and_multi_dim_geps() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();

        // Local `[i32; 5][i32; 5]` array. Following the frontend's
        // pointer-to-array convention the leading zero index selects the
        // array itself; then index 1 strides 20 bytes and index 2 strides 4.
        let arr_ty = Type::get_array(Type::get_array(Type::get_i32(), 5), 5);
        let alloc = data.new_local_inst().alloc(arr_ty);
        data.layout_mut().insert_inst(entry, alloc);
        let zero = data.new_local_inst().integer(0);
        let one = data.new_local_inst().integer(1);
        let two = data.new_local_inst().integer(2);
        let gep = data
            .new_local_inst()
            .get_elem_ptr(alloc, vec![zero, one, two]);
        data.layout_mut().insert_inst(entry, gep);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.constant_offset(&ctx, alloc), Some(0));
        assert_eq!(env.constant_offset(&ctx, gep), Some(1 * 20 + 2 * 4));
    }

    #[test]
    fn constant_offset_rejects_variable_index() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let param = data.params()[0];
        let index = data.params()[1];
        let zero = data.new_local_inst().integer(0);
        let gep = data.new_local_inst().get_elem_ptr(param, vec![zero]);
        data.layout_mut().insert_inst(entry, gep);
        let gep_var = data.new_local_inst().get_elem_ptr(param, vec![index]);
        data.layout_mut().insert_inst(entry, gep_var);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.constant_offset(&ctx, gep), Some(0));
        assert_eq!(env.constant_offset(&ctx, gep_var), None);
    }

    #[test]
    fn intra_alias_rules_matrix() {
        let mut program = Program::new();
        let global = new_global(&mut program, Type::get_i32());
        let global2 = new_global(&mut program, Type::get_i32());
        let function = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let param = data.params()[0];
        let alloc_a = data.new_local_inst().alloc(Type::get_i32());
        let alloc_b = data.new_local_inst().alloc(Type::get_i32());
        data.layout_mut().insert_inst(entry, alloc_a);
        data.layout_mut().insert_inst(entry, alloc_b);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.alias(&ctx, alloc_a, alloc_b), AliasResult::NoAlias);
        assert_eq!(env.alias(&ctx, alloc_a, global), AliasResult::NoAlias);
        assert_eq!(env.alias(&ctx, alloc_a, param), AliasResult::NoAlias);
        assert_eq!(env.alias(&ctx, global, global2), AliasResult::NoAlias);
        assert_eq!(env.alias(&ctx, global, param), AliasResult::MayAlias);
        assert_eq!(env.alias(&ctx, param, param), AliasResult::MustAlias);
    }

    #[test]
    fn same_base_constant_offsets_disentangle() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        // A local i32 array base; offsets 0, 4, 8 are pairwise disjoint.
        let alloc = data
            .new_local_inst()
            .alloc(Type::get_array(Type::get_i32(), 4));
        data.layout_mut().insert_inst(entry, alloc);
        let zero = data.new_local_inst().integer(0);
        let one = data.new_local_inst().integer(1);
        let two = data.new_local_inst().integer(2);
        let g0 = data.new_local_inst().get_elem_ptr(alloc, vec![zero]);
        let g1 = data.new_local_inst().get_elem_ptr(alloc, vec![one]);
        let g2 = data.new_local_inst().get_elem_ptr(alloc, vec![two]);
        data.layout_mut().insert_inst(entry, g0);
        data.layout_mut().insert_inst(entry, g1);
        data.layout_mut().insert_inst(entry, g2);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.alias(&ctx, g0, g0), AliasResult::MustAlias);
        assert_eq!(env.alias(&ctx, g0, g1), AliasResult::NoAlias);
        assert_eq!(env.alias(&ctx, g1, g2), AliasResult::NoAlias);
        assert_eq!(env.alias(&ctx, g0, g2), AliasResult::NoAlias);
    }

    #[test]
    fn block_param_resolves_through_uniform_incoming_edges() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let alloc = data.new_local_inst().alloc(Type::get_i32());
        data.layout_mut().insert_inst(entry, alloc);

        let head = data
            .new_basic_block()
            .basic_block("head".into(), vec![Type::get_i32().reference()]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        data.layout_mut().push_bb_back(head);
        data.layout_mut().push_bb_back(exit);

        let param = data.bb_data(head).params()[0];
        let jump = data.new_local_inst().jump(head, vec![alloc]);
        data.layout_mut().insert_inst(entry, jump);
        let ret_head = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(head, ret_head);
        let ret_exit = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret_exit);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.base_of(&ctx, param), MemObject::Alloc(alloc));
        assert_eq!(env.constant_offset(&ctx, param), Some(0));
    }

    #[test]
    fn conflicting_block_param_edges_yield_unknown() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let alloc_a = data.new_local_inst().alloc(Type::get_i32());
        let alloc_b = data.new_local_inst().alloc(Type::get_i32());
        data.layout_mut().insert_inst(entry, alloc_a);
        data.layout_mut().insert_inst(entry, alloc_b);

        let head = data
            .new_basic_block()
            .basic_block("head".into(), vec![Type::get_i32().reference()]);
        let mid = data.new_basic_block().basic_block("mid".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        data.layout_mut().push_bb_back(mid);
        data.layout_mut().push_bb_back(head);
        data.layout_mut().push_bb_back(exit);

        let param = data.bb_data(head).params()[0];
        let jump_a = data.new_local_inst().jump(head, vec![alloc_a]);
        data.layout_mut().insert_inst(entry, jump_a);
        let jump_b = data.new_local_inst().jump(head, vec![alloc_b]);
        data.layout_mut().insert_inst(mid, jump_b);
        let ret_head = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(head, ret_head);
        let ret_exit = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret_exit);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.base_of(&ctx, param), MemObject::Unknown);
    }
}
