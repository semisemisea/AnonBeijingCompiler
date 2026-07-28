use std::collections::{HashMap, HashSet};

use crate::opt::prelude::*;

// GVN-PRE implementation status and roadmap:
//
// Implemented:
// - Typed value numbering for safe i32 binary expressions.
// - Full-redundancy elimination at joins using block parameters.
// - Partial-redundancy elimination when exactly one incoming edge is missing.
// - Phi translation through existing block parameters.
// - Logical edge identity for jump/true/false edges, including parallel edges.
// - Critical-edge splitting and insertion on the split edge.
// - Conservative dominance checks and rejection of loop headers with backedges.
// - At most one structural rewrite per function invocation to avoid stale analyses.
// - Trace-level diagnostics for each committed transformation.
//
// Current safety boundary:
// - Candidates: i32 Add/Sub/Mul/And/Or/Xor and integer comparisons.
// - Excluded: shifts, Div/Rem, float operations, loads, calls, casts, GEPs,
//   Undef, memory operations, multiple missing edges, and loop-header insertion.
//
// Next implementation steps, in recommended order:
// 1. Replace the local CFG/dominator implementation with reusable edge-aware
//    CFG and dominance analyses, plus an IR verifier for block argument arity,
//    types, use-def consistency, and dominance after structural edits.
// 2. Share typed value numbering and expression canonicalization with the
//    ordinary GVN pass. Keep congruence classes separate from available leaders.
// 3. Add a profitability model using loop depth, dynamic edge estimates, code
//    growth, new block parameters, and expected register pressure.
// 4. Permit multiple missing edges only when the profitability model approves;
//    retain exact logical-edge splitting and transactional terminator rewrites.
// 5. Add carefully tested shift operations after confirming RaanaIR and every
//    backend agree on out-of-range shift semantics.
// 6. Handle loops only after natural-loop/preheader analysis exists. Avoid
//    backedge insertion initially; implement LICM as a separate pass.
// 7. Add memory value numbering separately: exact-address load CSE and local
//    store forwarding first, invalidating on uncertain stores, MemZero, or calls.
//    Do not add load PRE until aliasing and speculation safety are modeled.
// 8. Add function effect summaries (ReadNone/ReadOnly/WriteOrUnknown) before
//    considering call CSE, call PRE, or movement of calls.
//
// Required validation for each extension:
// - Focused unit tests for diamonds, critical edges, parallel edges, phi
//   translation, loops, unsafe operations, and pass idempotence.
// - Full workspace tests, release build, all functional tests under AArch64/QEMU,
//   perf output checks, and before/after instruction, spill, and runtime metrics.
pub struct GVNPRE;

type ValueNumber = u32;
type Available = HashMap<Expr, Inst>;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Expr {
    operand_ty: Type,
    result_ty: Type,
    op: BinaryOp,
    lhs: ValueNumber,
    rhs: ValueNumber,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum ValueKey {
    Integer(Type, i32),
    Expr(Expr),
    Identity(Type, Inst),
}

#[derive(Clone, Copy)]
enum EdgeArm {
    Jump,
    True,
    False,
}

#[derive(Clone, Copy)]
struct Edge {
    from: BasicBlock,
    terminator: Inst,
    arm: EdgeArm,
}

enum Rewrite {
    Jump(BasicBlock, Vec<Inst>),
    Branch(Inst, BasicBlock, Vec<Inst>, BasicBlock, Vec<Inst>),
}

struct ValueNumbers {
    next: ValueNumber,
    values: HashMap<Inst, ValueNumber>,
    keys: HashMap<ValueKey, ValueNumber>,
}

impl ValueNumbers {
    fn new() -> Self {
        Self {
            next: 0,
            values: HashMap::new(),
            keys: HashMap::new(),
        }
    }

    fn number(&mut self, data: &ArenaContextMut<'_>, value: Inst) -> ValueNumber {
        if let Some(&number) = self.values.get(&value) {
            return number;
        }
        let ty = data.inst_data(value).ty().clone();
        let key = match data.inst_data(value).kind() {
            InstKind::Integer(integer) => ValueKey::Integer(ty, integer.value()),
            InstKind::Binary(binary) => self
                .expr(data, binary.op(), binary.lhs(), binary.rhs())
                .map(ValueKey::Expr)
                .unwrap_or(ValueKey::Identity(ty, value)),
            _ => ValueKey::Identity(ty, value),
        };
        let number = *self.keys.entry(key).or_insert_with(|| {
            let number = self.next;
            self.next += 1;
            number
        });
        self.values.insert(value, number);
        number
    }

    fn expr(
        &mut self,
        data: &ArenaContextMut<'_>,
        op: BinaryOp,
        lhs: Inst,
        rhs: Inst,
    ) -> Option<Expr> {
        let operand_ty = data.inst_data(lhs).ty().clone();
        if !operand_ty.is_i32() || data.inst_data(rhs).ty() != &operand_ty {
            return None;
        }
        if !matches!(
            op,
            BinaryOp::Add
                | BinaryOp::Sub
                | BinaryOp::Mul
                | BinaryOp::And
                | BinaryOp::Or
                | BinaryOp::Xor
                | BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Gt
                | BinaryOp::Lt
                | BinaryOp::Ge
                | BinaryOp::Le
        ) {
            return None;
        }

        let mut op = op;
        let mut lhs = self.number(data, lhs);
        let mut rhs = self.number(data, rhs);
        if lhs > rhs {
            if matches!(
                op,
                BinaryOp::Add
                    | BinaryOp::Mul
                    | BinaryOp::And
                    | BinaryOp::Or
                    | BinaryOp::Xor
                    | BinaryOp::Eq
                    | BinaryOp::NotEq
            ) {
                std::mem::swap(&mut lhs, &mut rhs);
            } else if let Some(swapped) = op.swap_compare_args() {
                op = swapped;
                std::mem::swap(&mut lhs, &mut rhs);
            }
        }
        Some(Expr {
            operand_ty,
            result_ty: Type::get_i32(),
            op,
            lhs,
            rhs,
        })
    }
}

impl Edge {
    fn args(self, data: &ArenaContextMut<'_>) -> Vec<Inst> {
        match (self.arm, data.inst_data(self.terminator).kind()) {
            (EdgeArm::Jump, InstKind::Jump(jump)) => jump.args().to_vec(),
            (EdgeArm::True, InstKind::Branch(branch)) => branch.t_args().to_vec(),
            (EdgeArm::False, InstKind::Branch(branch)) => branch.f_args().to_vec(),
            _ => unreachable!(),
        }
    }
}

impl GVNPRE {
    fn reachable_edges(
        &self,
        data: &ArenaContextMut<'_>,
    ) -> (Vec<BasicBlock>, HashMap<BasicBlock, Vec<Edge>>) {
        let Some(entry) = data.layout().entry_bb().map(|layout| layout.bb()) else {
            return (Vec::new(), HashMap::new());
        };
        let mut order = Vec::new();
        let mut incoming: HashMap<BasicBlock, Vec<Edge>> = HashMap::new();
        let mut seen = HashSet::new();
        let mut work = vec![entry];
        while let Some(bb) = work.pop() {
            if !seen.insert(bb) {
                continue;
            }
            order.push(bb);
            let terminator = data.layout().basicblock(bb).terminator();
            match data.inst_data(terminator).kind() {
                InstKind::Jump(jump) => {
                    incoming.entry(jump.target()).or_default().push(Edge {
                        from: bb,
                        terminator,
                        arm: EdgeArm::Jump,
                    });
                    work.push(jump.target());
                }
                InstKind::Branch(branch) => {
                    incoming.entry(branch.t_target()).or_default().push(Edge {
                        from: bb,
                        terminator,
                        arm: EdgeArm::True,
                    });
                    incoming.entry(branch.f_target()).or_default().push(Edge {
                        from: bb,
                        terminator,
                        arm: EdgeArm::False,
                    });
                    work.push(branch.f_target());
                    work.push(branch.t_target());
                }
                InstKind::Return(..) => {}
                _ => return (Vec::new(), HashMap::new()),
            }
        }
        (order, incoming)
    }

    fn availability(
        &self,
        data: &ArenaContextMut<'_>,
        blocks: &[BasicBlock],
        incoming: &HashMap<BasicBlock, Vec<Edge>>,
        numbers: &mut ValueNumbers,
    ) -> HashMap<BasicBlock, Available> {
        let mut output: HashMap<BasicBlock, Available> = HashMap::new();
        let mut changed = true;
        while changed {
            changed = false;
            for &bb in blocks {
                let mut available = incoming
                    .get(&bb)
                    .and_then(|edges| {
                        let first = output.get(&edges.first()?.from)?.clone();
                        Some(first)
                    })
                    .unwrap_or_default();
                if let Some(edges) = incoming.get(&bb) {
                    for edge in edges.iter().skip(1) {
                        let Some(other) = output.get(&edge.from) else {
                            available.clear();
                            break;
                        };
                        available.retain(|expr, leader| other.get(expr) == Some(leader));
                    }
                }
                for &inst in data.layout().basicblock(bb).insts() {
                    let InstKind::Binary(binary) = data.inst_data(inst).kind() else {
                        continue;
                    };
                    if let Some(expr) = numbers.expr(data, binary.op(), binary.lhs(), binary.rhs())
                    {
                        available.insert(expr, inst);
                    }
                }
                if output.get(&bb) != Some(&available) {
                    output.insert(bb, available);
                    changed = true;
                }
            }
        }
        output
    }

    fn translated_operand(
        &self,
        data: &ArenaContextMut<'_>,
        bb: BasicBlock,
        edge: Edge,
        operand: Inst,
    ) -> Option<Inst> {
        let params = data.bb_data(bb).params();
        let Some(index) = params.iter().position(|&param| param == operand) else {
            return Some(operand);
        };
        edge.args(data).get(index).copied()
    }

    fn dominators(
        &self,
        blocks: &[BasicBlock],
        incoming: &HashMap<BasicBlock, Vec<Edge>>,
    ) -> HashMap<BasicBlock, HashSet<BasicBlock>> {
        let all = blocks.iter().copied().collect::<HashSet<_>>();
        let mut dominators = HashMap::new();
        let Some(&entry) = blocks.first() else {
            return dominators;
        };
        for &bb in blocks {
            dominators.insert(
                bb,
                if bb == entry {
                    HashSet::from([entry])
                } else {
                    all.clone()
                },
            );
        }

        let mut changed = true;
        while changed {
            changed = false;
            for &bb in blocks.iter().skip(1) {
                let Some(edges) = incoming.get(&bb) else {
                    continue;
                };
                let mut predecessors = edges.iter().map(|edge| edge.from);
                let Some(first) = predecessors.next() else {
                    continue;
                };
                let mut next = dominators[&first].clone();
                for predecessor in predecessors {
                    next.retain(|dominator| dominators[&predecessor].contains(dominator));
                }
                next.insert(bb);
                if dominators.get(&bb) != Some(&next) {
                    dominators.insert(bb, next);
                    changed = true;
                }
            }
        }
        dominators
    }

    fn value_available_before(
        &self,
        data: &ArenaContextMut<'_>,
        dominators: &HashMap<BasicBlock, HashSet<BasicBlock>>,
        bb: BasicBlock,
        terminator: Inst,
        value: Inst,
    ) -> bool {
        if value.is_global()
            || data.params().contains(&value)
            || matches!(data.inst_data(value).kind(), InstKind::Integer(..))
        {
            return true;
        }

        if let Some(def_bb) = data.layout().parent_bb(value) {
            if def_bb != bb {
                return dominators.get(&bb).is_some_and(|set| set.contains(&def_bb));
            }
            for &inst in data.layout().basicblock(bb).insts() {
                if inst == terminator {
                    return false;
                }
                if inst == value {
                    return true;
                }
            }
            return false;
        }

        data.layout().basicblocks().iter().any(|layout| {
            data.bb_data(layout.bb()).params().contains(&value)
                && dominators
                    .get(&bb)
                    .is_some_and(|set| set.contains(&layout.bb()))
        })
    }

    fn edge_is_valid(&self, data: &ArenaContextMut<'_>, bb: BasicBlock, edge: Edge) -> bool {
        let args = edge.args(data);
        let params = data.bb_data(bb).params();
        args.len() == params.len()
            && args
                .iter()
                .zip(params)
                .all(|(&arg, &param)| data.inst_data(arg).ty() == data.inst_data(param).ty())
    }

    fn rewrite_for(data: &ArenaContextMut<'_>, terminator: Inst) -> Rewrite {
        match data.inst_data(terminator).kind() {
            InstKind::Jump(jump) => Rewrite::Jump(jump.target(), jump.args().to_vec()),
            InstKind::Branch(branch) => Rewrite::Branch(
                branch.cond(),
                branch.t_target(),
                branch.t_args().to_vec(),
                branch.f_target(),
                branch.f_args().to_vec(),
            ),
            _ => unreachable!(),
        }
    }

    fn append_edge_value(rewrite: &mut Rewrite, arm: EdgeArm, value: Inst) {
        match (arm, rewrite) {
            (EdgeArm::Jump, Rewrite::Jump(_, args)) => args.push(value),
            (EdgeArm::True, Rewrite::Branch(_, _, args, _, _)) => args.push(value),
            (EdgeArm::False, Rewrite::Branch(_, _, _, _, args)) => args.push(value),
            _ => unreachable!(),
        }
    }

    fn retarget_edge_to_split(rewrite: &mut Rewrite, arm: EdgeArm, split: BasicBlock) {
        match (arm, rewrite) {
            (EdgeArm::True, Rewrite::Branch(_, target, args, _, _)) => {
                *target = split;
                args.clear();
            }
            (EdgeArm::False, Rewrite::Branch(_, _, _, target, args)) => {
                *target = split;
                args.clear();
            }
            _ => unreachable!(),
        }
    }

    fn apply_rewrites(data: &mut ArenaContextMut<'_>, rewrites: HashMap<Inst, Rewrite>) {
        for (terminator, rewrite) in rewrites {
            match rewrite {
                Rewrite::Jump(target, args) => {
                    data.replace_inst_with(terminator).jump(target, args);
                }
                Rewrite::Branch(cond, t_target, t_args, f_target, f_args) => {
                    data.replace_inst_with(terminator)
                        .branch(cond, t_target, t_args, f_target, f_args);
                }
            }
        }
    }

    fn apply(
        &self,
        data: &mut ArenaContextMut<'_>,
        bb: BasicBlock,
        recomputation: Inst,
        edges: &[Edge],
        leaders: &[Inst],
    ) {
        let mut rewrites = HashMap::new();
        for (&edge, &leader) in edges.iter().zip(leaders) {
            let rewrite = rewrites
                .entry(edge.terminator)
                .or_insert_with(|| Self::rewrite_for(data, edge.terminator));
            Self::append_edge_value(rewrite, edge.arm, leader);
        }

        let ty = data.inst_data(recomputation).ty().clone();
        let param = data.new_basic_block().add_param(bb, ty);
        Self::apply_rewrites(data, rewrites);
        utils::visit_and_replace(data, recomputation, param);
        data.remove_layout_inst(bb, recomputation);
    }

    fn apply_insertion(
        &self,
        data: &mut ArenaContextMut<'_>,
        bb: BasicBlock,
        recomputation: Inst,
        edges: &[Edge],
        leaders: &[Option<Inst>],
        missing_index: usize,
        op: BinaryOp,
        lhs: Inst,
        rhs: Inst,
    ) {
        let missing = edges[missing_index];
        let mut rewrites = HashMap::new();
        for &edge in edges {
            rewrites
                .entry(edge.terminator)
                .or_insert_with(|| Self::rewrite_for(data, edge.terminator));
        }

        let inserted = data.new_local_inst().binary(op, lhs, rhs);
        let effective_missing = if matches!(missing.arm, EdgeArm::Jump) {
            data.layout_mut()
                .insert_inst_before(missing.terminator, inserted);
            missing
        } else {
            let old_args = missing.args(data);
            let split = data
                .new_basic_block()
                .basic_block("gvn_pre_split".into(), vec![]);
            data.layout_mut().push_bb_back(split);
            data.layout_mut().insert_inst(split, inserted);
            Self::retarget_edge_to_split(
                rewrites.get_mut(&missing.terminator).unwrap(),
                missing.arm,
                split,
            );
            let split_jump = data.new_local_inst().jump(bb, old_args);
            data.layout_mut().insert_inst(split, split_jump);
            Edge {
                from: split,
                terminator: split_jump,
                arm: EdgeArm::Jump,
            }
        };

        let ty = data.inst_data(recomputation).ty().clone();
        let param = data.new_basic_block().add_param(bb, ty);
        for (index, &edge) in edges.iter().enumerate() {
            if index == missing_index && !matches!(missing.arm, EdgeArm::Jump) {
                continue;
            }
            let value = leaders[index].unwrap_or(inserted);
            Self::append_edge_value(rewrites.get_mut(&edge.terminator).unwrap(), edge.arm, value);
        }
        if effective_missing.terminator != missing.terminator {
            let mut split_rewrite = Self::rewrite_for(data, effective_missing.terminator);
            Self::append_edge_value(&mut split_rewrite, EdgeArm::Jump, inserted);
            rewrites.insert(effective_missing.terminator, split_rewrite);
        }

        Self::apply_rewrites(data, rewrites);
        utils::visit_and_replace(data, recomputation, param);
        data.remove_layout_inst(bb, recomputation);
    }
}

impl Pass for GVNPRE {
    fn run_on(&self, data: &mut ArenaContextMut<'_>) -> bool {
        let (blocks, incoming) = self.reachable_edges(data);
        if blocks.is_empty() {
            return false;
        }
        let mut numbers = ValueNumbers::new();
        let output = self.availability(data, &blocks, &incoming, &mut numbers);
        let dominators = self.dominators(&blocks, &incoming);

        for &bb in &blocks {
            let Some(edges) = incoming.get(&bb).filter(|edges| edges.len() >= 2) else {
                continue;
            };
            if edges.iter().any(|edge| {
                dominators
                    .get(&edge.from)
                    .is_some_and(|set| set.contains(&bb))
            }) || edges
                .iter()
                .any(|&edge| !self.edge_is_valid(data, bb, edge))
            {
                continue;
            }
            let insts = data
                .layout()
                .basicblock(bb)
                .insts()
                .iter()
                .copied()
                .collect::<Vec<_>>();
            for recomputation in insts {
                let InstKind::Binary(binary) = data.inst_data(recomputation).kind() else {
                    continue;
                };
                let (op, lhs, rhs) = (binary.op(), binary.lhs(), binary.rhs());
                let Some(candidate_expr) = numbers.expr(data, op, lhs, rhs) else {
                    continue;
                };
                if data.inst_data(recomputation).ty() != &candidate_expr.result_ty {
                    continue;
                }
                let mut leaders = Vec::with_capacity(edges.len());
                let mut translated = Vec::with_capacity(edges.len());
                for &edge in edges {
                    let Some(lhs) = self.translated_operand(data, bb, edge, lhs) else {
                        leaders.clear();
                        break;
                    };
                    let Some(rhs) = self.translated_operand(data, bb, edge, rhs) else {
                        leaders.clear();
                        break;
                    };
                    let Some(expr) = numbers.expr(data, op, lhs, rhs) else {
                        leaders.clear();
                        break;
                    };
                    if expr.result_ty != candidate_expr.result_ty
                        || !self.value_available_before(
                            data,
                            &dominators,
                            edge.from,
                            edge.terminator,
                            lhs,
                        )
                        || !self.value_available_before(
                            data,
                            &dominators,
                            edge.from,
                            edge.terminator,
                            rhs,
                        )
                    {
                        leaders.clear();
                        break;
                    }
                    let leader = output
                        .get(&edge.from)
                        .and_then(|map| map.get(&expr))
                        .copied()
                        .filter(|&leader| {
                            data.inst_data(leader).ty() == data.inst_data(recomputation).ty()
                                && self.value_available_before(
                                    data,
                                    &dominators,
                                    edge.from,
                                    edge.terminator,
                                    leader,
                                )
                        });
                    translated.push((lhs, rhs));
                    leaders.push(leader);
                }
                if leaders.len() != edges.len() {
                    continue;
                }

                let missing = leaders
                    .iter()
                    .enumerate()
                    .filter_map(|(index, leader)| leader.is_none().then_some(index))
                    .collect::<Vec<_>>();
                if missing.is_empty() {
                    let leaders = leaders.into_iter().map(Option::unwrap).collect::<Vec<_>>();
                    if leaders.windows(2).all(|pair| pair[0] == pair[1]) {
                        continue;
                    }

                    // All edge arguments and leaders have been validated. Mutate once,
                    // then return so no analysis result can be reused after CFG changes.
                    self.apply(data, bb, recomputation, edges, &leaders);
                    trace!(
                        "gvn-pre fully_available function={} block={bb:?} expression={candidate_expr:?} block_parameters_added=1",
                        data.name()
                    );
                    return true;
                }
                if missing.len() == 1 {
                    let missing_index = missing[0];
                    let (lhs, rhs) = translated[missing_index];
                    self.apply_insertion(
                        data,
                        bb,
                        recomputation,
                        edges,
                        &leaders,
                        missing_index,
                        op,
                        lhs,
                        rhs,
                    );
                    trace!(
                        "gvn-pre partial_insertion function={} block={bb:?} expression={candidate_expr:?} edge_split={} block_parameters_added=1 expressions_inserted=1",
                        data.name(),
                        !matches!(edges[missing_index].arm, EdgeArm::Jump)
                    );
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Program, arena::Arena, builder_trait::*};

    fn returned_value(data: &FunctionData, bb: BasicBlock) -> Inst {
        let ret = data.layout().basicblock(bb).terminator();
        let InstKind::Return(ret) = data.inst_data(ret).kind() else {
            panic!("expected return")
        };
        ret.value().unwrap()
    }

    #[test]
    fn eliminates_diamond_recomputation_with_commuted_arm() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "diamond".into(),
            vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let head = data.add_entry_block();
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let merge = data.new_basic_block().basic_block("merge".into(), vec![]);
        for bb in [left, right, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let (cond, x, y) = (data.params()[0], data.params()[1], data.params()[2]);
        let branch = data
            .new_local_inst()
            .branch(cond, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(head, branch);
        let left_add = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let left_jump = data.new_local_inst().jump(merge, vec![]);
        data.layout_mut().insert_inst(left, left_add);
        data.layout_mut().insert_inst(left, left_jump);
        let right_add = data.new_local_inst().binary(BinaryOp::Add, y, x);
        let right_jump = data.new_local_inst().jump(merge, vec![]);
        data.layout_mut().insert_inst(right, right_add);
        data.layout_mut().insert_inst(right, right_jump);
        let recomputation = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let ret = data.new_local_inst().ret(Some(recomputation));
        data.layout_mut().insert_inst(merge, recomputation);
        data.layout_mut().insert_inst(merge, ret);

        assert!(GVNPRE.run(&mut program));
        let data = program.func_data(function);
        let param = data.bb_data(merge).params()[0];
        assert_eq!(returned_value(data, merge), param);
        assert_eq!(data.layout().parent_bb(recomputation), None);
        assert!(matches!(
            data.inst_data(left_jump).kind(),
            InstKind::Jump(jump) if jump.args() == [left_add]
        ));
        assert!(matches!(
            data.inst_data(right_jump).kind(),
            InstKind::Jump(jump) if jump.args() == [right_add]
        ));
    }

    #[test]
    fn inserts_on_non_critical_missing_edge() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "negative".into(),
            vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let head = data.add_entry_block();
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let merge = data.new_basic_block().basic_block("merge".into(), vec![]);
        for bb in [left, right, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let (cond, x, y) = (data.params()[0], data.params()[1], data.params()[2]);
        let branch = data
            .new_local_inst()
            .branch(cond, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(head, branch);
        let left_add = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let left_jump = data.new_local_inst().jump(merge, vec![]);
        data.layout_mut().insert_inst(left, left_add);
        data.layout_mut().insert_inst(left, left_jump);
        let right_jump = data.new_local_inst().jump(merge, vec![]);
        data.layout_mut().insert_inst(right, right_jump);
        let recomputation = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let ret = data.new_local_inst().ret(Some(recomputation));
        data.layout_mut().insert_inst(merge, recomputation);
        data.layout_mut().insert_inst(merge, ret);

        let pass = GVNPRE;
        assert!(pass.run(&mut program));
        assert!(!pass.run(&mut program));
        let data = program.func_data(function);
        let param = data.bb_data(merge).params()[0];
        assert_eq!(returned_value(data, merge), param);
        assert_eq!(data.layout().parent_bb(recomputation), None);
        assert!(matches!(
            data.inst_data(left_jump).kind(),
            InstKind::Jump(jump) if jump.args() == [left_add]
        ));
        let right_insts = data
            .layout()
            .basicblock(right)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(right_insts.len(), 2);
        let inserted = right_insts[0];
        assert!(matches!(
            data.inst_data(inserted).kind(),
            InstKind::Binary(binary)
                if binary.op() == BinaryOp::Add && binary.lhs() == x && binary.rhs() == y
        ));
        assert!(matches!(
            data.inst_data(right_jump).kind(),
            InstKind::Jump(jump) if jump.args() == [inserted]
        ));
    }

    #[test]
    fn splits_critical_missing_edge() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "critical".into(),
            vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let head = data.add_entry_block();
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let merge = data.new_basic_block().basic_block("merge".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [left, right, merge, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let (cond, x, y) = (data.params()[0], data.params()[1], data.params()[2]);
        let head_branch = data
            .new_local_inst()
            .branch(cond, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(head, head_branch);
        let left_add = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let left_jump = data.new_local_inst().jump(merge, vec![]);
        data.layout_mut().insert_inst(left, left_add);
        data.layout_mut().insert_inst(left, left_jump);
        let right_branch = data
            .new_local_inst()
            .branch(cond, merge, vec![], exit, vec![]);
        data.layout_mut().insert_inst(right, right_branch);
        let recomputation = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let merge_ret = data.new_local_inst().ret(Some(recomputation));
        data.layout_mut().insert_inst(merge, recomputation);
        data.layout_mut().insert_inst(merge, merge_ret);
        let zero = data.new_local_inst().integer(0);
        let exit_ret = data.new_local_inst().ret(Some(zero));
        data.layout_mut().insert_inst(exit, exit_ret);

        let pass = GVNPRE;
        assert!(pass.run(&mut program));
        assert!(!pass.run(&mut program));
        let data = program.func_data(function);
        assert_eq!(data.layout().basicblocks().len(), 6);
        let InstKind::Branch(branch) = data.inst_data(right_branch).kind() else {
            panic!("expected branch")
        };
        let split = branch.t_target();
        assert_ne!(split, merge);
        assert_eq!(branch.f_target(), exit);
        assert!(branch.t_args().is_empty());
        let split_insts = data
            .layout()
            .basicblock(split)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(split_insts.len(), 2);
        let inserted = split_insts[0];
        assert!(matches!(
            data.inst_data(inserted).kind(),
            InstKind::Binary(binary) if binary.op() == BinaryOp::Add
        ));
        assert!(matches!(
            data.inst_data(split_insts[1]).kind(),
            InstKind::Jump(jump) if jump.target() == merge && jump.args() == [inserted]
        ));
        assert!(matches!(
            data.inst_data(left_jump).kind(),
            InstKind::Jump(jump) if jump.args() == [left_add]
        ));
        assert_eq!(returned_value(data, merge), data.bb_data(merge).params()[0]);
    }

    #[test]
    fn splits_only_missing_same_target_parallel_arm() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "parallel_missing".into(),
            vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let head = data.add_entry_block();
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32(), Type::get_i32()]);
        data.layout_mut().push_bb_back(merge);
        let (cond, x, y) = (data.params()[0], data.params()[1], data.params()[2]);
        let xy = data.new_local_inst().binary(BinaryOp::Sub, x, y);
        let branch = data
            .new_local_inst()
            .branch(cond, merge, vec![x, y], merge, vec![y, x]);
        data.layout_mut().insert_inst(head, xy);
        data.layout_mut().insert_inst(head, branch);
        let params = data.bb_data(merge).params().clone();
        let recomputation = data
            .new_local_inst()
            .binary(BinaryOp::Sub, params[0], params[1]);
        let ret = data.new_local_inst().ret(Some(recomputation));
        data.layout_mut().insert_inst(merge, recomputation);
        data.layout_mut().insert_inst(merge, ret);

        let pass = GVNPRE;
        assert!(pass.run(&mut program));
        assert!(!pass.run(&mut program));
        let data = program.func_data(function);
        let InstKind::Branch(branch_data) = data.inst_data(branch).kind() else {
            panic!("expected branch")
        };
        assert_eq!(branch_data.t_target(), merge);
        assert_eq!(branch_data.t_args(), [x, y, xy]);
        let split = branch_data.f_target();
        assert_ne!(split, merge);
        assert!(branch_data.f_args().is_empty());
        let split_insts = data
            .layout()
            .basicblock(split)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let inserted = split_insts[0];
        assert!(matches!(
            data.inst_data(inserted).kind(),
            InstKind::Binary(binary)
                if binary.op() == BinaryOp::Sub && binary.lhs() == y && binary.rhs() == x
        ));
        assert!(matches!(
            data.inst_data(split_insts[1]).kind(),
            InstKind::Jump(jump) if jump.target() == merge && jump.args() == [y, x, inserted]
        ));
    }

    #[test]
    fn translates_phi_operands_for_inserted_expression() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "phi_insert".into(),
            vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let head = data.add_entry_block();
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32(), Type::get_i32()]);
        for bb in [left, right, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let (cond, x, y) = (data.params()[0], data.params()[1], data.params()[2]);
        let head_branch = data
            .new_local_inst()
            .branch(cond, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(head, head_branch);
        let left_add = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let left_jump = data.new_local_inst().jump(merge, vec![x, y]);
        data.layout_mut().insert_inst(left, left_add);
        data.layout_mut().insert_inst(left, left_jump);
        let right_jump = data.new_local_inst().jump(merge, vec![y, x]);
        data.layout_mut().insert_inst(right, right_jump);
        let params = data.bb_data(merge).params().clone();
        let recomputation = data
            .new_local_inst()
            .binary(BinaryOp::Add, params[0], params[1]);
        let ret = data.new_local_inst().ret(Some(recomputation));
        data.layout_mut().insert_inst(merge, recomputation);
        data.layout_mut().insert_inst(merge, ret);

        assert!(GVNPRE.run(&mut program));
        let data = program.func_data(function);
        let right_insts = data
            .layout()
            .basicblock(right)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let inserted = right_insts[0];
        assert!(matches!(
            data.inst_data(inserted).kind(),
            InstKind::Binary(binary)
                if binary.op() == BinaryOp::Add && binary.lhs() == y && binary.rhs() == x
        ));
        assert!(matches!(
            data.inst_data(right_jump).kind(),
            InstKind::Jump(jump) if jump.args() == [y, x, inserted]
        ));
        assert!(matches!(
            data.inst_data(left_jump).kind(),
            InstKind::Jump(jump) if jump.args() == [x, y, left_add]
        ));
    }

    #[test]
    fn rejects_loop_header_with_backedge() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "loop".into(),
            vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data.new_basic_block().basic_block("header".into(), vec![]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let (cond, x, y) = (data.params()[0], data.params()[1], data.params()[2]);
        let entry_add = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let entry_jump = data.new_local_inst().jump(header, vec![]);
        data.layout_mut().insert_inst(entry, entry_add);
        data.layout_mut().insert_inst(entry, entry_jump);
        let recomputation = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let header_branch = data
            .new_local_inst()
            .branch(cond, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, recomputation);
        data.layout_mut().insert_inst(header, header_branch);
        let latch_jump = data.new_local_inst().jump(header, vec![]);
        data.layout_mut().insert_inst(latch, latch_jump);
        let ret = data.new_local_inst().ret(Some(recomputation));
        data.layout_mut().insert_inst(exit, ret);

        assert!(!GVNPRE.run(&mut program));
        let data = program.func_data(function);
        assert!(data.bb_data(header).params().is_empty());
        assert_eq!(data.layout().parent_bb(recomputation), Some(header));
    }

    #[test]
    fn rejects_unsafe_binary_operation() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "unsafe".into(),
            vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let head = data.add_entry_block();
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let merge = data.new_basic_block().basic_block("merge".into(), vec![]);
        for bb in [left, right, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let (cond, x, y) = (data.params()[0], data.params()[1], data.params()[2]);
        let branch = data
            .new_local_inst()
            .branch(cond, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(head, branch);
        let left_div = data.new_local_inst().binary(BinaryOp::Div, x, y);
        let left_jump = data.new_local_inst().jump(merge, vec![]);
        data.layout_mut().insert_inst(left, left_div);
        data.layout_mut().insert_inst(left, left_jump);
        let right_jump = data.new_local_inst().jump(merge, vec![]);
        data.layout_mut().insert_inst(right, right_jump);
        let recomputation = data.new_local_inst().binary(BinaryOp::Div, x, y);
        let ret = data.new_local_inst().ret(Some(recomputation));
        data.layout_mut().insert_inst(merge, recomputation);
        data.layout_mut().insert_inst(merge, ret);

        assert!(!GVNPRE.run(&mut program));
        let data = program.func_data(function);
        assert!(data.bb_data(merge).params().is_empty());
        assert_eq!(data.layout().parent_bb(recomputation), Some(merge));
    }

    #[test]
    fn preserves_same_target_true_and_false_edges() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "parallel".into(),
            vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let head = data.add_entry_block();
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32(), Type::get_i32()]);
        data.layout_mut().push_bb_back(merge);
        let (cond, x, y) = (data.params()[0], data.params()[1], data.params()[2]);
        let xy = data.new_local_inst().binary(BinaryOp::Sub, x, y);
        let yx = data.new_local_inst().binary(BinaryOp::Sub, y, x);
        let branch = data
            .new_local_inst()
            .branch(cond, merge, vec![x, y], merge, vec![y, x]);
        for inst in [xy, yx, branch] {
            data.layout_mut().insert_inst(head, inst);
        }
        let params = data.bb_data(merge).params().clone();
        let recomputation = data
            .new_local_inst()
            .binary(BinaryOp::Sub, params[0], params[1]);
        let ret = data.new_local_inst().ret(Some(recomputation));
        data.layout_mut().insert_inst(merge, recomputation);
        data.layout_mut().insert_inst(merge, ret);

        assert!(GVNPRE.run(&mut program));
        let data = program.func_data(function);
        let result = data.bb_data(merge).params()[2];
        assert_eq!(returned_value(data, merge), result);
        let InstKind::Branch(branch) = data.inst_data(branch).kind() else {
            panic!("expected branch")
        };
        assert_eq!(branch.t_args(), [x, y, xy]);
        assert_eq!(branch.f_args(), [y, x, yx]);
    }
}
