use rustc_hash::FxHashMap;

use crate::opt::prelude::*;

pub struct GlobalInstNumbering;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
struct ValueNumber(u32);

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum ValueKey {
    Integer {
        ty: Type,
        value: i32,
    },
    Float {
        ty: Type,
        bits: u32,
    },
    Binary {
        operand_ty: Type,
        result_ty: Type,
        op: BinaryOp,
        lhs: ValueNumber,
        rhs: ValueNumber,
    },
    Cast {
        src_ty: Type,
        result_ty: Type,
        src: ValueNumber,
    },
    Select {
        result_ty: Type,
        cond: ValueNumber,
        if_true: ValueNumber,
        if_false: ValueNumber,
    },
    GetElemPtr {
        base_ty: Type,
        result_ty: Type,
        base: ValueNumber,
        offsets: Vec<ValueNumber>,
    },
    Identity {
        ty: Type,
        value: Inst,
    },
}

#[derive(Clone, Copy)]
struct NumberedValue {
    number: ValueNumber,
    eliminable: bool,
}

struct ValueNumbering {
    next: u32,
    by_inst: FxHashMap<Inst, NumberedValue>,
    by_key: FxHashMap<ValueKey, ValueNumber>,
}

impl ValueNumbering {
    fn new() -> Self {
        Self {
            next: 0,
            by_inst: FxHashMap::default(),
            by_key: FxHashMap::default(),
        }
    }

    fn number(&mut self, data: &ArenaContextMut<'_>, value: Inst) -> NumberedValue {
        if let Some(&numbered) = self.by_inst.get(&value) {
            return numbered;
        }

        let ty = data.inst_data(value).ty().clone();
        let (key, eliminable) = match data.inst_data(value).kind() {
            InstKind::Integer(integer) => (
                ValueKey::Integer {
                    ty,
                    value: integer.value(),
                },
                true,
            ),
            InstKind::Float(float) => (
                ValueKey::Float {
                    ty,
                    bits: float.value().to_bits(),
                },
                true,
            ),
            InstKind::Binary(binary) => {
                let operand_ty = data.inst_data(binary.lhs()).ty().clone();
                let mut op = binary.op();
                let mut lhs = self.number(data, binary.lhs()).number;
                let mut rhs = self.number(data, binary.rhs()).number;
                if lhs > rhs {
                    if op.is_commutative_for(&operand_ty) {
                        std::mem::swap(&mut lhs, &mut rhs);
                    } else if let Some(swapped) = op.swap_compare_args() {
                        op = swapped;
                        std::mem::swap(&mut lhs, &mut rhs);
                    }
                }
                (
                    ValueKey::Binary {
                        operand_ty,
                        result_ty: ty,
                        op,
                        lhs,
                        rhs,
                    },
                    true,
                )
            }
            InstKind::Cast(cast) => (
                ValueKey::Cast {
                    src_ty: data.inst_data(cast.src()).ty().clone(),
                    result_ty: ty,
                    src: self.number(data, cast.src()).number,
                },
                true,
            ),
            InstKind::Select(select) => (
                ValueKey::Select {
                    result_ty: ty,
                    cond: self.number(data, select.cond()).number,
                    if_true: self.number(data, select.if_true()).number,
                    if_false: self.number(data, select.if_false()).number,
                },
                true,
            ),
            InstKind::GetElemPtr(gep) => (
                ValueKey::GetElemPtr {
                    base_ty: data.inst_data(gep.base()).ty().clone(),
                    result_ty: ty,
                    base: self.number(data, gep.base()).number,
                    offsets: gep
                        .offsets()
                        .iter()
                        .map(|&offset| self.number(data, offset).number)
                        .collect(),
                },
                true,
            ),
            InstKind::Load(..)
            | InstKind::Call(..)
            | InstKind::Alloc
            | InstKind::GlobalAlloc(..)
            | InstKind::BlockArgRef(..)
            | InstKind::Aggregate(..)
            | InstKind::Undef
            | InstKind::ZeroInit
            | InstKind::Store(..)
            | InstKind::MemZero(..)
            | InstKind::Return(..)
            | InstKind::Jump(..)
            | InstKind::Branch(..)
            | InstKind::TailCall(..) => (ValueKey::Identity { ty, value }, false),
        };

        let number = *self.by_key.entry(key).or_insert_with(|| {
            let number = ValueNumber(self.next);
            self.next += 1;
            number
        });
        let numbered = NumberedValue { number, eliminable };
        self.by_inst.insert(value, numbered);
        numbered
    }
}

struct ScopedLeaders {
    leaders: FxHashMap<ValueNumber, Inst>,
    scopes: Vec<Vec<ValueNumber>>,
}

impl ScopedLeaders {
    fn new() -> Self {
        Self {
            leaders: FxHashMap::default(),
            scopes: Vec::new(),
        }
    }

    fn enter_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    fn get(&self, number: ValueNumber) -> Option<Inst> {
        self.leaders.get(&number).copied()
    }

    fn insert(&mut self, number: ValueNumber, leader: Inst) {
        assert!(self.leaders.insert(number, leader).is_none());
        self.scopes.last_mut().unwrap().push(number);
    }

    fn exit_scope(&mut self) {
        for number in self.scopes.pop().unwrap() {
            self.leaders.remove(&number);
        }
    }
}

/// Scoped leaders for loads keyed by their address instruction, invalidated
/// by any store or call (conservative may-alias: any store may hit any
/// address). A global store counter records whether a memory writer
/// intervened between the leader and the candidate load.
struct ScopedLoadLeaders {
    leaders: FxHashMap<Inst, (Inst, u64)>,
    scopes: Vec<Vec<Inst>>,
    store_counter: u64,
}

impl ScopedLoadLeaders {
    fn new() -> Self {
        Self {
            leaders: FxHashMap::default(),
            scopes: Vec::new(),
            store_counter: 0,
        }
    }

    fn enter_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    fn record_store(&mut self) {
        self.store_counter += 1;
    }

    /// The leader for `addr` that no store/call has invalidated.
    fn get(&self, addr: Inst) -> Option<Inst> {
        self.leaders
            .get(&addr)
            .filter(|(_, counter)| *counter == self.store_counter)
            .map(|(leader, _)| *leader)
    }

    fn insert(&mut self, addr: Inst, leader: Inst) {
        self.leaders.insert(addr, (leader, self.store_counter));
        self.scopes.last_mut().unwrap().push(addr);
    }

    fn exit_scope(&mut self) {
        for addr in self.scopes.pop().unwrap() {
            self.leaders.remove(&addr);
        }
    }
}

impl Pass for GlobalInstNumbering {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().entry_bb().is_none() {
            return false;
        }
        debug!("----------------------------------------------------");
        debug!("gvn start: {:?}", data.name());

        let mut bb_alloc = IDAllocator::new(1);
        let (graph, predecessors) = cfg::build_cfg_both(data, &mut bb_alloc);
        assert_eq!(bb_alloc.get_id(&data.layout().entry_bb().unwrap().bb()), 0);
        let rpo = cfg::rpo_path(&graph);
        let idom = dom_tree::idom(&predecessors, &rpo);
        let dominance_tree = dom_tree::build_dominance_tree(&idom, rpo.len());
        let mut numbers = ValueNumbering::new();
        let mut leaders = ScopedLeaders::new();

        enum Visit {
            Enter(BId),
            Exit,
        }

        let mut changed = false;
        let mut load_leaders = ScopedLoadLeaders::new();
        let mut visits = vec![Visit::Enter(0)];
        while let Some(visit) = visits.pop() {
            let bb_id = match visit {
                Visit::Enter(bb_id) => bb_id,
                Visit::Exit => {
                    load_leaders.exit_scope();
                    leaders.exit_scope();
                    continue;
                }
            };
            leaders.enter_scope();
            load_leaders.enter_scope();
            let bb = bb_alloc.search_id(bb_id);
            let values = data
                .bb_data(bb)
                .params()
                .iter()
                .chain(data.layout().basicblock(bb).insts().iter())
                .copied()
                .collect::<Vec<_>>();
            for value in values {
                match data.inst_data(value).kind() {
                    InstKind::Load(load) => {
                        let addr = load.src();
                        // Any store or call invalidates load leaders.
                        if let Some(leader) = load_leaders.get(addr) {
                            if value != leader && !data.inst_data(value).used_by().is_empty() {
                                utils::visit_and_replace(data, value, leader);
                                changed = true;
                            }
                        } else {
                            load_leaders.insert(addr, value);
                        }
                        let numbered = numbers.number(data, value);
                        let _ = numbered;
                        continue;
                    }
                    InstKind::Store(..) | InstKind::Call(..) | InstKind::MemZero(..) => {
                        load_leaders.record_store();
                    }
                    _ => {}
                }
                let numbered = numbers.number(data, value);
                if !numbered.eliminable {
                    continue;
                }
                if let Some(leader) = leaders.get(numbered.number) {
                    assert_eq!(data.inst_data(value).ty(), data.inst_data(leader).ty());
                    if value != leader && !data.inst_data(value).used_by().is_empty() {
                        trace!(
                            "gvn replace function={} value={value:?} leader={leader:?} number={:?}",
                            data.name(),
                            numbered.number
                        );
                        utils::visit_and_replace(data, value, leader);
                        changed = true;
                    }
                } else {
                    leaders.insert(numbered.number, value);
                }
            }

            visits.push(Visit::Exit);
            for &child in dominance_tree[bb_id].iter().rev() {
                visits.push(Visit::Enter(child));
            }
        }
        debug!("----------------------------------------------------");
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Program, arena::Arena, builder_trait::*};

    fn return_value(data: &FunctionData, bb: BasicBlock) -> Inst {
        let terminator = data.layout().basicblock(bb).terminator();
        let InstKind::Return(ret) = data.inst_data(terminator).kind() else {
            panic!("expected return")
        };
        ret.value().unwrap()
    }

    fn binary_operands(data: &FunctionData, value: Inst) -> (Inst, Inst) {
        let InstKind::Binary(binary) = data.inst_data(value).kind() else {
            panic!("expected binary")
        };
        (binary.lhs(), binary.rhs())
    }

    #[test]
    fn eliminates_transitive_congruence_in_one_run() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "transitive".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let (x, y) = (data.params()[0], data.params()[1]);
        let one_a = data.new_local_inst().integer(1);
        let one_b = data.new_local_inst().integer(1);
        let add_a = data.new_local_inst().binary(BinaryOp::Add, x, one_a);
        let add_b = data.new_local_inst().binary(BinaryOp::Add, x, one_b);
        let mul_a = data.new_local_inst().binary(BinaryOp::Mul, add_a, y);
        let mul_b = data.new_local_inst().binary(BinaryOp::Mul, add_b, y);
        let difference = data.new_local_inst().binary(BinaryOp::Sub, mul_a, mul_b);
        let ret = data.new_local_inst().ret(Some(difference));
        for value in [one_a, one_b, add_a, add_b, mul_a, mul_b, difference, ret] {
            data.layout_mut().insert_inst(entry, value);
        }

        assert!(GlobalInstNumbering.run(&mut program));
        assert!(!GlobalInstNumbering.run(&mut program));
        let data = program.func_data(function);
        assert_eq!(binary_operands(data, difference), (mul_a, mul_a));
        assert!(data.inst_data(mul_b).used_by().is_empty());
        assert!(data.inst_data(mul_a).used_by().contains(&difference));
    }

    #[test]
    fn keeps_cast_result_types_separate() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "typed_cast".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let source = data.params()[0];
        let cast_i32 = data.new_local_inst().cast(source, Type::get_i32());
        let cast_f32 = data.new_local_inst().cast(source, Type::get_f32());
        let duplicate_i32 = data.new_local_inst().cast(source, Type::get_i32());
        let zero_i32 = data.new_local_inst().integer(0);
        let zero_f32 = data.new_local_inst().float(0.0);
        let int_use = data
            .new_local_inst()
            .binary(BinaryOp::Add, duplicate_i32, zero_i32);
        let float_use = data
            .new_local_inst()
            .binary(BinaryOp::Add, cast_f32, zero_f32);
        let ret = data.new_local_inst().ret(Some(int_use));
        for value in [
            cast_i32,
            cast_f32,
            duplicate_i32,
            zero_i32,
            zero_f32,
            int_use,
            float_use,
            ret,
        ] {
            data.layout_mut().insert_inst(entry, value);
        }

        assert!(GlobalInstNumbering.run(&mut program));
        let data = program.func_data(function);
        assert_eq!(binary_operands(data, int_use).0, cast_i32);
        assert_eq!(binary_operands(data, float_use).0, cast_f32);
        assert!(data.inst_data(cast_f32).ty().is_f32());
    }

    #[test]
    fn commutes_integer_but_not_float_arithmetic() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "commutativity".into(),
            vec![
                Type::get_i32(),
                Type::get_i32(),
                Type::get_f32(),
                Type::get_f32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let (x, y, a, b) = (
            data.params()[0],
            data.params()[1],
            data.params()[2],
            data.params()[3],
        );
        let int_xy = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let int_yx = data.new_local_inst().binary(BinaryOp::Add, y, x);
        let float_ab = data.new_local_inst().binary(BinaryOp::Add, a, b);
        let float_ba = data.new_local_inst().binary(BinaryOp::Add, b, a);
        let float_difference = data
            .new_local_inst()
            .binary(BinaryOp::Sub, float_ab, float_ba);
        let result = data.new_local_inst().binary(BinaryOp::Sub, int_xy, int_yx);
        let ret = data.new_local_inst().ret(Some(result));
        for value in [
            int_xy,
            int_yx,
            float_ab,
            float_ba,
            float_difference,
            result,
            ret,
        ] {
            data.layout_mut().insert_inst(entry, value);
        }

        assert!(GlobalInstNumbering.run(&mut program));
        let data = program.func_data(function);
        assert_eq!(binary_operands(data, result), (int_xy, int_xy));
        assert_eq!(
            binary_operands(data, float_difference),
            (float_ab, float_ba)
        );
    }

    #[test]
    fn uses_dominating_leaders_without_crossing_siblings() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "dominance".into(),
            vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        for bb in [left, right] {
            data.layout_mut().push_bb_back(bb);
        }
        let (cond, x, y) = (data.params()[0], data.params()[1], data.params()[2]);
        let branch = data
            .new_local_inst()
            .branch(cond, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(entry, branch);

        let left_first = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let left_second = data.new_local_inst().binary(BinaryOp::Add, y, x);
        let left_ret = data.new_local_inst().ret(Some(left_second));
        for value in [left_first, left_second, left_ret] {
            data.layout_mut().insert_inst(left, value);
        }

        let right_first = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let right_second = data.new_local_inst().binary(BinaryOp::Add, y, x);
        let right_ret = data.new_local_inst().ret(Some(right_second));
        for value in [right_first, right_second, right_ret] {
            data.layout_mut().insert_inst(right, value);
        }

        assert!(GlobalInstNumbering.run(&mut program));
        let data = program.func_data(function);
        assert_eq!(return_value(data, left), left_first);
        assert_eq!(return_value(data, right), right_first);
        assert_ne!(return_value(data, right), left_first);
    }

    #[test]
    fn reuses_a_parent_block_leader() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "parent".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let child = data.new_basic_block().basic_block("child".into(), vec![]);
        data.layout_mut().push_bb_back(child);
        let (x, y) = (data.params()[0], data.params()[1]);
        let leader = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let jump = data.new_local_inst().jump(child, vec![]);
        data.layout_mut().insert_inst(entry, leader);
        data.layout_mut().insert_inst(entry, jump);
        let duplicate = data.new_local_inst().binary(BinaryOp::Add, y, x);
        let ret = data.new_local_inst().ret(Some(duplicate));
        data.layout_mut().insert_inst(child, duplicate);
        data.layout_mut().insert_inst(child, ret);

        assert!(GlobalInstNumbering.run(&mut program));
        assert_eq!(return_value(program.func_data(function), child), leader);
    }

    #[test]
    fn does_not_cse_memory_or_calls() {
        let mut program = Program::new();
        let callee = program.new_function(Type::get_i32(), "callee".into(), vec![]);
        let function = program.new_function(Type::get_i32(), "effects".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let alloc = data.new_local_inst().alloc(Type::get_i32());
        let load_a = data.new_local_inst().load(alloc);
        let load_b = data.new_local_inst().load(alloc);
        let call_a = data
            .new_local_inst()
            .call_with_type(callee, vec![], Type::get_i32());
        let call_b = data
            .new_local_inst()
            .call_with_type(callee, vec![], Type::get_i32());
        let loads = data.new_local_inst().binary(BinaryOp::Add, load_a, load_b);
        let calls = data.new_local_inst().binary(BinaryOp::Add, call_a, call_b);
        let result = data.new_local_inst().binary(BinaryOp::Add, loads, calls);
        let ret = data.new_local_inst().ret(Some(result));
        for value in [
            alloc, load_a, load_b, call_a, call_b, loads, calls, result, ret,
        ] {
            data.layout_mut().insert_inst(entry, value);
        }

        assert!(GlobalInstNumbering.run(&mut program));
        let data = program.func_data(function);
        // Calls are never CSE'd; loads of the same address are, unless a
        // store intervenes.
        assert_eq!(binary_operands(data, loads), (load_a, load_a));
        assert_eq!(binary_operands(data, calls), (call_a, call_b));
    }

    #[test]
    fn store_invalidates_load_leader() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "store".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let alloc = data.new_local_inst().alloc(Type::get_i32());
        let load_a = data.new_local_inst().load(alloc);
        let zero = data.new_local_inst().integer(0);
        let store = data.new_local_inst().store(zero, alloc);
        let load_b = data.new_local_inst().load(alloc);
        let sum = data.new_local_inst().binary(BinaryOp::Add, load_a, load_b);
        let ret = data.new_local_inst().ret(Some(sum));
        for value in [alloc, load_a, zero, store, load_b, sum, ret] {
            data.layout_mut().insert_inst(entry, value);
        }

        assert!(!GlobalInstNumbering.run(&mut program));
        let data = program.func_data(function);
        assert_eq!(
            binary_operands(data, sum),
            (load_a, load_b),
            "the store between the loads must prevent CSE"
        );
    }

    #[test]
    fn keeps_tail_calls_unique_and_rewrites_their_arguments() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "tail_call".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let one_a = data.new_local_inst().integer(1);
        let one_b = data.new_local_inst().integer(1);
        let tail_call = data.new_local_inst().tail_call(function, vec![one_b]);
        for value in [one_a, one_b, tail_call] {
            data.layout_mut().insert_inst(entry, value);
        }

        assert!(GlobalInstNumbering.run(&mut program));
        assert!(!GlobalInstNumbering.run(&mut program));
        let data = program.func_data(function);
        let InstKind::TailCall(tail_call_data) = data.inst_data(tail_call).kind() else {
            panic!("expected tail call")
        };
        assert_eq!(tail_call_data.args(), &[one_a]);
    }

    #[test]
    fn canonicalizes_swapped_comparisons() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "compare".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let (x, y) = (data.params()[0], data.params()[1]);
        let less = data.new_local_inst().binary(BinaryOp::Lt, x, y);
        let greater = data.new_local_inst().binary(BinaryOp::Gt, y, x);
        let result = data.new_local_inst().binary(BinaryOp::Sub, less, greater);
        let ret = data.new_local_inst().ret(Some(result));
        for value in [less, greater, result, ret] {
            data.layout_mut().insert_inst(entry, value);
        }

        assert!(GlobalInstNumbering.run(&mut program));
        assert_eq!(
            binary_operands(program.func_data(function), result),
            (less, less)
        );
    }
}
