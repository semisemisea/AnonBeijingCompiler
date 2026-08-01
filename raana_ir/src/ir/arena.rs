use itertools::Itertools;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::ir::{
    BasicBlockBuilders, LocalBuilder,
    basic_block::{BasicBlock, BasicBlockArena, BasicBlockData},
    builder::ReplaceBuilder,
    function::{Function, FunctionArena, FunctionData},
    inst_kind::InstKind,
    instruction::{GlobalInstArena, Inst, InstData, LocalInstArena},
};

const INST_EQUIV_DEPTH_LIMIT: usize = 32;

#[derive(Clone, Copy)]
enum InstEquivState {
    Visiting,
    Equivalent(bool),
}

struct InstEquivContext<'a, A: Arena + ?Sized> {
    arena: &'a A,
    cache: FxHashMap<(Inst, Inst), InstEquivState>,
}

impl<'a, A: Arena + ?Sized> InstEquivContext<'a, A> {
    fn new(arena: &'a A) -> Self {
        Self {
            arena,
            cache: FxHashMap::default(),
        }
    }

    fn insts_equiv(&mut self, lhs: &[Inst], rhs: &[Inst]) -> bool {
        insts_equiv_at(self.arena, &mut self.cache, lhs, rhs, 0)
    }

    fn inst_equiv(&mut self, lhs: Inst, rhs: Inst, depth: usize) -> bool {
        inst_equiv(self.arena, &mut self.cache, lhs, rhs, depth)
    }
}

fn insts_equiv_at<A: Arena + ?Sized>(
    arena: &A,
    cache: &mut FxHashMap<(Inst, Inst), InstEquivState>,
    lhs: &[Inst],
    rhs: &[Inst],
    depth: usize,
) -> bool {
    lhs.len() == rhs.len()
        && lhs
            .iter()
            .zip(rhs)
            .all(|(&lhs, &rhs)| inst_equiv(arena, cache, lhs, rhs, depth))
}

fn inst_equiv<A: Arena + ?Sized>(
    arena: &A,
    cache: &mut FxHashMap<(Inst, Inst), InstEquivState>,
    lhs: Inst,
    rhs: Inst,
    depth: usize,
) -> bool {
    if lhs == rhs {
        return true;
    }
    if depth >= INST_EQUIV_DEPTH_LIMIT {
        return false;
    }
    if let Some(state) = cache.get(&(lhs, rhs)) {
        return matches!(state, InstEquivState::Equivalent(true));
    }

    let lhs_data = arena.inst_data(lhs);
    let rhs_data = arena.inst_data(rhs);
    if lhs_data.ty() != rhs_data.ty()
        || std::mem::discriminant(lhs_data.kind()) != std::mem::discriminant(rhs_data.kind())
    {
        return false;
    }

    // Cycles are not valid value-expression trees. Reject them conservatively.
    cache.insert((lhs, rhs), InstEquivState::Visiting);
    cache.insert((rhs, lhs), InstEquivState::Visiting);
    let equivalent = match (lhs_data.kind(), rhs_data.kind()) {
        (InstKind::Integer(lhs), InstKind::Integer(rhs)) => lhs.value() == rhs.value(),
        (InstKind::Float(lhs), InstKind::Float(rhs)) => {
            lhs.value().to_bits() == rhs.value().to_bits()
        }
        (InstKind::ZeroInit, InstKind::ZeroInit) => true,
        (InstKind::Binary(lhs), InstKind::Binary(rhs)) if lhs.op() == rhs.op() => {
            let direct = inst_equiv(arena, cache, lhs.lhs(), rhs.lhs(), depth + 1)
                && inst_equiv(arena, cache, lhs.rhs(), rhs.rhs(), depth + 1);
            direct
                || (lhs.op().is_commutative_for(arena.inst_data(lhs.lhs()).ty())
                    && inst_equiv(arena, cache, lhs.lhs(), rhs.rhs(), depth + 1)
                    && inst_equiv(arena, cache, lhs.rhs(), rhs.lhs(), depth + 1))
        }
        (InstKind::Cast(lhs), InstKind::Cast(rhs)) => {
            inst_equiv(arena, cache, lhs.src(), rhs.src(), depth + 1)
        }
        (InstKind::GetElemPtr(lhs), InstKind::GetElemPtr(rhs)) => {
            inst_equiv(arena, cache, lhs.base(), rhs.base(), depth + 1)
                && insts_equiv_at(arena, cache, lhs.offsets(), rhs.offsets(), depth + 1)
        }
        (InstKind::Aggregate(lhs), InstKind::Aggregate(rhs)) => {
            insts_equiv_at(arena, cache, lhs.value(), rhs.value(), depth + 1)
        }
        _ => false,
    };
    cache.insert((lhs, rhs), InstEquivState::Equivalent(equivalent));
    cache.insert((rhs, lhs), InstEquivState::Equivalent(equivalent));
    equivalent
}

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

    /// Conservatively checks whether two values represent the same expression.
    fn equal(&self, lhs: Inst, rhs: Inst) -> bool {
        InstEquivContext::new(self).inst_equiv(lhs, rhs, 0)
    }

    /// Checks value lists with a shared cache for recursive expression comparisons.
    fn insts_equal(&self, lhs: &[Inst], rhs: &[Inst]) -> bool {
        InstEquivContext::new(self).insts_equiv(lhs, rhs)
    }

    #[must_use]
    #[inline]
    fn inst_data(&self, inst: Inst) -> &InstData {
        if inst.is_global() {
            self.global().inst_arena.data_of(inst)
        } else {
            self.local().inst_arena.data_of(inst)
        }
    }

    fn inst_datas(
        &self,
    ) -> std::iter::Chain<
        std::collections::hash_map::Iter<'_, Inst, InstData>,
        std::collections::hash_map::Iter<'_, Inst, InstData>,
    > {
        self.global()
            .inst_arena
            .datas()
            .chain(self.local().inst_arena.datas())
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
    fn alloc_local_inst(&mut self, mut data: InstData) -> Inst {
        let inst_usage = data.inst_usage().collect::<SmallVec<[Inst; 4]>>();
        let bb_usage = data.bb_usage().collect::<SmallVec<[BasicBlock; 2]>>();
        debug_assert!(data.used_by().is_empty());
        data.used_by_mut().clear();

        let id = self.local_mut().inst_arena.alloc(data);
        for used in inst_usage {
            self.inst_data_mut(used).used_by_mut().insert(id);
        }
        for bb in bb_usage {
            self.bb_data_mut(bb).used_by_mut().insert(id);
        }
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
    fn alloc_global_inst(&mut self, mut data: InstData) -> Inst {
        let inst_usage = data.inst_usage().collect::<SmallVec<[Inst; 4]>>();
        debug_assert!(data.used_by().is_empty());
        data.used_by_mut().clear();

        let id = self.global_mut().inst_arena.alloc(data);
        for used in inst_usage {
            self.inst_data_mut(used).used_by_mut().insert(id);
        }
        id
    }

    fn remove_inst(&mut self, inst: Inst) -> InstData {
        // INFO: Or you can choose to dfs wipe everything out.
        assert!(self.inst_data(inst).used_by().is_empty());
        for used in self.inst_data(inst).inst_usage().collect_vec() {
            self.inst_data_mut(used).used_by_mut().remove(&inst);
        }
        for bb in self.inst_data(inst).bb_usage().collect_vec() {
            self.bb_data_mut(bb).used_by_mut().remove(&inst);
        }
        self.remove_inst_data(inst)
    }

    fn remove_inst_data(&mut self, inst: Inst) -> InstData {
        if inst.is_global() {
            self.global_mut().inst_arena.remove(inst)
        } else {
            self.local_mut().inst_arena.remove(inst)
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        BinaryOp, Type,
        builder_trait::{LocalInstBuilder, ScalarInstBuilder},
    };

    #[test]
    fn compares_structurally_equivalent_values() {
        let mut function = FunctionData::new(Type::get_unit(), "equiv".into(), vec![]);
        let one_a = function.new_local_inst().integer(1);
        let one_b = function.new_local_inst().integer(1);
        let two_a = function.new_local_inst().integer(2);
        let two_b = function.new_local_inst().integer(2);
        let add_a = function
            .new_local_inst()
            .binary(BinaryOp::Add, one_a, two_a);
        let add_b = function
            .new_local_inst()
            .binary(BinaryOp::Add, two_b, one_b);

        assert!(function.equal(one_a, one_b));
        assert!(function.equal(add_a, add_b));
        assert!(function.insts_equal(&[one_a, add_a], &[one_b, add_b]));
    }

    #[test]
    fn keeps_order_for_non_commutative_values() {
        let mut function = FunctionData::new(Type::get_unit(), "ordered".into(), vec![]);
        let one_a = function.new_local_inst().integer(1);
        let one_b = function.new_local_inst().integer(1);
        let two_a = function.new_local_inst().integer(2);
        let two_b = function.new_local_inst().integer(2);
        let sub_a = function
            .new_local_inst()
            .binary(BinaryOp::Sub, one_a, two_a);
        let sub_b = function
            .new_local_inst()
            .binary(BinaryOp::Sub, two_b, one_b);

        assert!(!function.equal(sub_a, sub_b));
    }

    #[test]
    fn keeps_float_arithmetic_operand_order() {
        let mut function = FunctionData::new(Type::get_unit(), "float_ordered".into(), vec![]);
        let one_a = function.new_local_inst().float(1.0);
        let one_b = function.new_local_inst().float(1.0);
        let two_a = function.new_local_inst().float(2.0);
        let two_b = function.new_local_inst().float(2.0);
        let direct_a = function
            .new_local_inst()
            .binary(BinaryOp::Add, one_a, two_a);
        let direct_b = function
            .new_local_inst()
            .binary(BinaryOp::Add, one_b, two_b);
        let swapped = function
            .new_local_inst()
            .binary(BinaryOp::Add, two_b, one_b);

        assert!(function.equal(direct_a, direct_b));
        assert!(!function.equal(direct_a, swapped));
    }

    #[test]
    fn conservatively_rejects_distinct_memory_values() {
        let mut function = FunctionData::new(Type::get_unit(), "memory".into(), vec![]);
        let alloc_a = function.new_local_inst().alloc(Type::get_i32());
        let alloc_b = function.new_local_inst().alloc(Type::get_i32());
        let load_a = function.new_local_inst().load(alloc_a);
        let load_b = function.new_local_inst().load(alloc_a);

        assert!(!function.equal(alloc_a, alloc_b));
        assert!(!function.equal(load_a, load_b));
        assert!(function.equal(load_a, load_a));
    }
}
