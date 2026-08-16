use rustc_hash::{FxHashMap, FxHashSet};

use crate::opt::{
    analysis_passes::effects::{AbstractObject, EffectAnalysis, WriteRoot},
    prelude::*,
};

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
    Call {
        callee: Function,
        result_ty: Type,
        args: Vec<ValueNumber>,
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

struct ValueNumbering<'a> {
    next: u32,
    by_inst: FxHashMap<Inst, NumberedValue>,
    by_key: FxHashMap<ValueKey, ValueNumber>,
    analysis: &'a EffectAnalysis,
}

impl<'a> ValueNumbering<'a> {
    fn new(analysis: &'a EffectAnalysis) -> Self {
        Self {
            next: 0,
            by_inst: FxHashMap::default(),
            by_key: FxHashMap::default(),
            analysis,
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
            InstKind::Call(call) => {
                // Calls are only mergeable when the callee is read-only (no
                // I/O, no timer, no external write); the caller must also
                // prove no intervening write, which the leader tracking in
                // run_on handles.
                //
                // Side-effecting calls (e.g. getint) must NOT share a value
                // number: two calls to the same callee with the same
                // arguments read different inputs and produce different
                // results. Sharing a number would make consumers of the two
                // results (e.g. `lt a, n` vs `lt b, n`) look equivalent and
                // get merged. Such calls still number their arguments (for
                // consistency) but get a per-instruction identity key.
                let eliminable = self.analysis.is_removable(call.callee());
                if eliminable {
                    (
                        ValueKey::Call {
                            callee: call.callee(),
                            result_ty: ty,
                            args: call
                                .args()
                                .iter()
                                .map(|&arg| self.number(data, arg).number)
                                .collect(),
                        },
                        true,
                    )
                } else {
                    (ValueKey::Identity { ty, value }, false)
                }
            }
            InstKind::Load(..)
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
            | InstKind::TailCall(..)
            | InstKind::Fma(..)
            | InstKind::VectorSplat(..)
            | InstKind::VectorExtractElement(..)
            | InstKind::VectorInsertElement(..)
            | InstKind::VectorReduce(..) => (ValueKey::Identity { ty, value }, false),
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

/// Scoped leaders for read-only calls keyed by value number, invalidated by
/// writes that may hit what the callee reads. Unlike loads (where any store
/// conservatively invalidates every leader), invalidation is precise: a
/// store through a resolved base only kills leaders whose callee may read
/// that base, and a sibling call only kills leaders whose callee's read set
/// intersects the sibling's write set.
struct ScopedCallLeaders {
    leaders: FxHashMap<ValueNumber, (Inst, Function)>,
    scopes: Vec<Vec<ValueNumber>>,
}

impl ScopedCallLeaders {
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
        self.leaders.get(&number).map(|(leader, _)| *leader)
    }

    fn insert(&mut self, number: ValueNumber, leader: Inst, callee: Function) {
        self.leaders.insert(number, (leader, callee));
        self.scopes.last_mut().unwrap().push(number);
    }

    /// Drop every leader whose callee satisfies `predicate`.
    fn invalidate_matching(&mut self, predicate: impl Fn(Function) -> bool) {
        let stale = self
            .leaders
            .iter()
            .filter(|(_, (_, callee))| predicate(*callee))
            .map(|(&number, _)| number)
            .collect::<Vec<_>>();
        for number in stale {
            self.leaders.remove(&number);
        }
    }

    fn exit_scope(&mut self) {
        for number in self.scopes.pop().unwrap() {
            self.leaders.remove(&number);
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
        // RPO position per block: a predecessor that comes *later* in RPO
        // is a backedge, i.e. this block is a loop header.
        let rpo_pos: FxHashMap<usize, usize> =
            rpo.iter().enumerate().map(|(i, &b)| (b, i)).collect();
        // Function-level effects do not change while this pass runs (GVN
        // only replaces values), so analyze once per invocation.
        let analysis = EffectAnalysis::new(data.program);
        let mut numbers = ValueNumbering::new(&analysis);
        let mut leaders = ScopedLeaders::new();

        enum Visit {
            Enter(BId),
            Exit,
        }

        let mut changed = false;
        let mut load_leaders = ScopedLoadLeaders::new();
        let mut call_leaders = ScopedCallLeaders::new();
        let mut visits = vec![Visit::Enter(0)];
        while let Some(visit) = visits.pop() {
            let bb_id = match visit {
                Visit::Enter(bb_id) => bb_id,
                Visit::Exit => {
                    load_leaders.exit_scope();
                    call_leaders.exit_scope();
                    leaders.exit_scope();
                    continue;
                }
            };
            leaders.enter_scope();
            load_leaders.enter_scope();
            call_leaders.enter_scope();
            let bb = bb_alloc.search_id(bb_id);
            // Loop headers (any predecessor is later in RPO = backedge):
            // the loop body may write any address, so a load CSE'd from a
            // pre-header or an earlier iteration would read a stale value
            // on the backedge. Invalidate every load leader on entry.
            let is_loop_header = predecessors.get(&bb_id).is_some_and(|preds| {
                preds
                    .iter()
                    .any(|&p| rpo_pos.get(&p).is_some_and(|&pi| pi > rpo_pos[&bb_id]))
            });
            if is_loop_header {
                load_leaders.record_store();
            }
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
                    InstKind::Store(store) => {
                        load_leaders.record_store();
                        // Precisely invalidate call leaders whose callee may
                        // read the stored location.
                        let func = data.curr_func.unwrap();
                        let targets = analysis.targets_of(data, func, store.dest());
                        call_leaders
                            .invalidate_matching(|c| analysis.call_may_read(c, targets.as_ref()));
                    }
                    InstKind::MemZero(mem_zero) => {
                        load_leaders.record_store();
                        let func = data.curr_func.unwrap();
                        let targets = analysis.targets_of(data, func, mem_zero.dest());
                        call_leaders
                            .invalidate_matching(|c| analysis.call_may_read(c, targets.as_ref()));
                    }
                    InstKind::Call(call) => {
                        // A call that writes no external memory (a read-only
                        // or pure-I/O callee) cannot clobber a loaded value,
                        // so load leaders stay valid across it.
                        if analysis.effects_of(call.callee()).may_write_memory() {
                            load_leaders.record_store();
                        }
                        // A sibling call may write what a call leader reads.
                        let func = data.curr_func.unwrap();
                        let sibling = call.callee();
                        let has_conflict = match analysis.call_read_roots(sibling, func) {
                            Some(leader_reads) => {
                                let reads = leader_reads
                                    .iter()
                                    .map(|r| match r {
                                        WriteRoot::Global(g) => AbstractObject::Global(*g),
                                        WriteRoot::Local(f, a) => AbstractObject::Alloc(*f, *a),
                                    })
                                    .collect::<FxHashSet<_>>();
                                analysis.call_may_write(sibling, Some(&reads))
                            }
                            // The sibling may read anything: any leader may
                            // be clobbered if the sibling writes anything at
                            // all.
                            None => analysis.effects_of(sibling).may_write_memory(),
                        };
                        if has_conflict {
                            // Conservatively: only the leaders the sibling
                            // may actually write get dropped; recompute the
                            // per-leader check below.
                            call_leaders.invalidate_matching(|c| {
                                let reads = match analysis.call_read_roots(c, func) {
                                    Some(reads) => reads,
                                    None => return analysis.effects_of(sibling).may_write_memory(),
                                };
                                let reads = reads
                                    .iter()
                                    .map(|r| match r {
                                        WriteRoot::Global(g) => AbstractObject::Global(*g),
                                        WriteRoot::Local(f, a) => AbstractObject::Alloc(*f, *a),
                                    })
                                    .collect::<FxHashSet<_>>();
                                analysis.call_may_write(sibling, Some(&reads))
                            });
                        }
                    }
                    _ => {}
                }
                let numbered = numbers.number(data, value);
                if !numbered.eliminable {
                    continue;
                }
                // Call leaders are tracked separately so a write can
                // invalidate exactly the calls whose callee reads what it
                // writes; the generic leaders map has no such invalidation.
                let leader = if matches!(data.inst_data(value).kind(), InstKind::Call(..)) {
                    call_leaders.get(numbered.number)
                } else {
                    leaders.get(numbered.number)
                };
                if let Some(leader) = leader {
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
                    // Read-only calls are managed exclusively by
                    // call_leaders; everything else goes through the generic
                    // leaders map.
                    if let InstKind::Call(call) = data.inst_data(value).kind() {
                        call_leaders.insert(numbered.number, value, call.callee());
                    } else {
                        leaders.insert(numbered.number, value);
                    }
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
        // A side-effecting callee: getint performs I/O.
        let callee = program.new_function(Type::get_i32(), "getint".into(), vec![]);
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
        // Loads of the same address are CSE'd unless a store intervenes;
        // side-effecting calls are never CSE'd.
        assert_eq!(binary_operands(data, loads), (load_a, load_a));
        assert_eq!(binary_operands(data, calls), (call_a, call_b));
    }

    #[test]
    fn cses_identical_pure_calls() {
        let mut program = Program::new();
        // A strictly pure callee returning its argument.
        let callee = program.new_function(Type::get_i32(), "pure_fn".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(callee);
        let entry = data.add_entry_block();
        let p = data.params()[0];
        let ret = data.new_local_inst().ret(Some(p));
        data.layout_mut().insert_inst(entry, ret);

        let function =
            program.new_function(Type::get_i32(), "cse_calls".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let x = data.params()[0];
        let call_a = data
            .new_local_inst()
            .call_with_type(callee, vec![x], Type::get_i32());
        let call_b = data
            .new_local_inst()
            .call_with_type(callee, vec![x], Type::get_i32());
        let sum = data.new_local_inst().binary(BinaryOp::Add, call_a, call_b);
        let ret = data.new_local_inst().ret(Some(sum));
        for value in [call_a, call_b, sum, ret] {
            data.layout_mut().insert_inst(entry, value);
        }

        assert!(GlobalInstNumbering.run(&mut program));
        let data = program.func_data(function);
        assert_eq!(binary_operands(data, sum), (call_a, call_a));
        assert!(data.inst_data(call_b).used_by().is_empty());
    }

    #[test]
    fn store_to_read_global_blocks_call_cse() {
        let mut program = Program::new();
        let zero_init = program.new_value().integer(0);
        let gv = program.new_value().global_alloc(zero_init);
        // A read-only callee reading gv[0].
        let callee = program.new_function(Type::get_i32(), "reads_global".into(), vec![]);
        program.func_data_mut(callee).add_entry_block();
        {
            let mut ctx = ArenaContextMut {
                program: &mut program,
                curr_func: Some(callee),
            };
            let entry = ctx.layout().entry_bb().unwrap().bb();
            let zero = ctx.new_local_value().integer(0);
            let gep = ctx.new_local_value().get_elem_ptr(gv, vec![zero]);
            let load = ctx.new_local_value().load(gep);
            let ret = ctx.new_local_value().ret(Some(load));
            ctx.layout_mut().insert_inst(entry, zero);
            ctx.layout_mut().insert_inst(entry, gep);
            ctx.layout_mut().insert_inst(entry, load);
            ctx.layout_mut().insert_inst(entry, ret);
        }
        let function = program.new_function(Type::get_i32(), "conflicted".into(), vec![]);
        program.func_data_mut(function).add_entry_block();
        let call_a;
        let call_b;
        let sum;
        {
            let mut ctx = ArenaContextMut {
                program: &mut program,
                curr_func: Some(function),
            };
            let entry = ctx.layout().entry_bb().unwrap().bb();
            call_a = ctx
                .new_local_value()
                .call_with_type(callee, vec![], Type::get_i32());
            ctx.layout_mut().insert_inst(entry, call_a);
            let zero = ctx.new_local_value().integer(0);
            let gep = ctx.new_local_value().get_elem_ptr(gv, vec![zero]);
            let one = ctx.new_local_value().integer(1);
            let store = ctx.new_local_value().store(one, gep);
            ctx.layout_mut().insert_inst(entry, gep);
            ctx.layout_mut().insert_inst(entry, store);
            call_b = ctx
                .new_local_value()
                .call_with_type(callee, vec![], Type::get_i32());
            ctx.layout_mut().insert_inst(entry, call_b);
            sum = ctx.new_local_value().binary(BinaryOp::Add, call_a, call_b);
            ctx.layout_mut().insert_inst(entry, sum);
            let ret = ctx.new_local_value().ret(Some(sum));
            ctx.layout_mut().insert_inst(entry, ret);
        }

        // Nothing else in this function is CSE-able, so the pass may report
        // no change; the guarantee is that the call was not merged.
        GlobalInstNumbering.run(&mut program);
        let data = program.func_data(function);
        // The store to a global the callee reads must invalidate the leader.
        assert_eq!(binary_operands(data, sum), (call_a, call_b));
    }

    #[test]
    fn store_to_local_does_not_block_call_cse() {
        let mut program = Program::new();
        let zero_init = program.new_value().integer(0);
        let gv = program.new_value().global_alloc(zero_init);
        let callee = program.new_function(Type::get_i32(), "reads_global".into(), vec![]);
        program.func_data_mut(callee).add_entry_block();
        {
            let mut ctx = ArenaContextMut {
                program: &mut program,
                curr_func: Some(callee),
            };
            let entry = ctx.layout().entry_bb().unwrap().bb();
            let zero = ctx.new_local_value().integer(0);
            let gep = ctx.new_local_value().get_elem_ptr(gv, vec![zero]);
            let load = ctx.new_local_value().load(gep);
            let ret = ctx.new_local_value().ret(Some(load));
            ctx.layout_mut().insert_inst(entry, zero);
            ctx.layout_mut().insert_inst(entry, gep);
            ctx.layout_mut().insert_inst(entry, load);
            ctx.layout_mut().insert_inst(entry, ret);
        }
        let function = program.new_function(Type::get_i32(), "unconflicted".into(), vec![]);
        program.func_data_mut(function).add_entry_block();
        let call_a;
        let call_b;
        let sum;
        {
            let mut ctx = ArenaContextMut {
                program: &mut program,
                curr_func: Some(function),
            };
            let entry = ctx.layout().entry_bb().unwrap().bb();
            call_a = ctx
                .new_local_value()
                .call_with_type(callee, vec![], Type::get_i32());
            ctx.layout_mut().insert_inst(entry, call_a);
            let slot = ctx.new_local_value().alloc(Type::get_i32());
            let one = ctx.new_local_value().integer(1);
            let store = ctx.new_local_value().store(one, slot);
            ctx.layout_mut().insert_inst(entry, slot);
            ctx.layout_mut().insert_inst(entry, store);
            call_b = ctx
                .new_local_value()
                .call_with_type(callee, vec![], Type::get_i32());
            ctx.layout_mut().insert_inst(entry, call_b);
            sum = ctx.new_local_value().binary(BinaryOp::Add, call_a, call_b);
            ctx.layout_mut().insert_inst(entry, sum);
            let ret = ctx.new_local_value().ret(Some(sum));
            ctx.layout_mut().insert_inst(entry, ret);
        }

        assert!(GlobalInstNumbering.run(&mut program));
        let data = program.func_data(function);
        // A local store cannot alias gv, so the second call merges.
        assert_eq!(binary_operands(data, sum), (call_a, call_a));
    }

    #[test]
    fn read_only_call_does_not_invalidate_load_leaders() {
        let mut program = Program::new();
        // A pure void callee: no I/O, no writes.
        let callee = program.new_function(Type::get_unit(), "pure_void".into(), vec![]);
        let data = program.func_data_mut(callee);
        let entry = data.add_entry_block();
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let function = program.new_function(Type::get_i32(), "loads_across_call".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let slot = data.new_local_inst().alloc(Type::get_i32());
        let load_a = data.new_local_inst().load(slot);
        let call = data
            .new_local_inst()
            .call_with_type(callee, vec![], Type::get_unit());
        let load_b = data.new_local_inst().load(slot);
        let sum = data.new_local_inst().binary(BinaryOp::Add, load_a, load_b);
        let ret = data.new_local_inst().ret(Some(sum));
        for value in [slot, load_a, call, load_b, sum, ret] {
            data.layout_mut().insert_inst(entry, value);
        }

        assert!(GlobalInstNumbering.run(&mut program));
        let data = program.func_data(function);
        // A read-only call cannot clobber the slot, so the second load merges.
        assert_eq!(binary_operands(data, sum), (load_a, load_a));
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
