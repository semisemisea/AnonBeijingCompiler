//! Function-local signed i32 range analysis.
//!
//! The abstract domain is one closed interval with an optional hole at zero.
//! Arithmetic that may wrap for a non-singleton input is deliberately mapped
//! to `full`; singleton operations use RaanaIR's wrapping integer semantics.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    ir::{BasicBlock, BinaryOp, Inst, InstKind, arena::Arena},
    opt::{
        analysis_passes::{
            induction_variable::{
                BasicInductionVariable, BasicInductionVariableAnalysis, InductionDirection,
                InductionStep,
            },
            loop_analysis::{Loop, LoopAnalysis},
        },
        pass::ArenaContext,
        utils::{
            cfg::CFG,
            logical_edge::{LogicalEdge, LogicalEdgeArm, incoming_edges, outgoing_edges},
        },
    },
};

const WIDEN_AFTER: usize = 3;
const CONTEXT_DEPTH_LIMIT: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntRange {
    Empty,
    Bounded {
        min: i32,
        max: i32,
        contains_zero: bool,
    },
}

impl IntRange {
    pub const fn empty() -> Self {
        Self::Empty
    }

    pub const fn full() -> Self {
        Self::Bounded {
            min: i32::MIN,
            max: i32::MAX,
            contains_zero: true,
        }
    }

    pub const fn constant(value: i32) -> Self {
        Self::Bounded {
            min: value,
            max: value,
            contains_zero: value == 0,
        }
    }

    pub fn bounded(min: i32, max: i32) -> Self {
        Self::normalized(min, max, min <= 0 && max >= 0)
    }

    fn normalized(mut min: i32, mut max: i32, contains_zero: bool) -> Self {
        if min > max {
            return Self::Empty;
        }
        if !contains_zero {
            if min == 0 {
                min = 1;
            }
            if max == 0 {
                max = -1;
            }
            if min > max {
                return Self::Empty;
            }
        }
        Self::Bounded {
            min,
            max,
            contains_zero: contains_zero && min <= 0 && max >= 0,
        }
    }

    pub const fn min(self) -> Option<i32> {
        match self {
            Self::Empty => None,
            Self::Bounded { min, .. } => Some(min),
        }
    }

    pub const fn max(self) -> Option<i32> {
        match self {
            Self::Empty => None,
            Self::Bounded { max, .. } => Some(max),
        }
    }

    pub const fn singleton(self) -> Option<i32> {
        match self {
            Self::Bounded { min, max, .. } if min == max => Some(min),
            _ => None,
        }
    }

    pub const fn contains(self, value: i32) -> bool {
        match self {
            Self::Empty => false,
            Self::Bounded {
                min,
                max,
                contains_zero,
            } => min <= value && value <= max && (value != 0 || contains_zero),
        }
    }

    pub const fn contains_zero(self) -> bool {
        self.contains(0)
    }

    pub const fn excludes_zero(self) -> bool {
        !self.contains_zero()
    }

    pub fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Empty, range) | (range, Self::Empty) => range,
            (
                Self::Bounded {
                    min: lhs_min,
                    max: lhs_max,
                    contains_zero: lhs_zero,
                },
                Self::Bounded {
                    min: rhs_min,
                    max: rhs_max,
                    contains_zero: rhs_zero,
                },
            ) => Self::normalized(
                lhs_min.min(rhs_min),
                lhs_max.max(rhs_max),
                lhs_zero || rhs_zero,
            ),
        }
    }

    pub fn intersect(self, other: Self) -> Self {
        match (self, other) {
            (Self::Empty, _) | (_, Self::Empty) => Self::Empty,
            (
                Self::Bounded {
                    min: lhs_min,
                    max: lhs_max,
                    contains_zero: lhs_zero,
                },
                Self::Bounded {
                    min: rhs_min,
                    max: rhs_max,
                    contains_zero: rhs_zero,
                },
            ) => Self::normalized(
                lhs_min.max(rhs_min),
                lhs_max.min(rhs_max),
                lhs_zero && rhs_zero,
            ),
        }
    }

    pub fn is_subset_of(self, other: Self) -> bool {
        match (self, other) {
            (Self::Empty, _) => true,
            (_, Self::Empty) => false,
            (
                Self::Bounded {
                    min: lhs_min,
                    max: lhs_max,
                    contains_zero: lhs_zero,
                },
                Self::Bounded {
                    min: rhs_min,
                    max: rhs_max,
                    contains_zero: rhs_zero,
                },
            ) => {
                rhs_min <= lhs_min
                    && lhs_max <= rhs_max
                    && (!lhs_zero || rhs_zero)
                    && !(lhs_min == 0 && !lhs_zero && lhs_max == 0)
            }
        }
    }

    pub fn subset(self, other: Self) -> bool {
        self.is_subset_of(other)
    }

    pub fn assume_eq(self, other: Self) -> Self {
        self.intersect(other)
    }

    pub fn assume_ne(self, other: Self) -> Self {
        match other.singleton() {
            Some(0) => match self {
                Self::Bounded { min, max, .. } if min == 0 && max == 0 => Self::Empty,
                Self::Bounded { min, max, .. } => Self::normalized(min, max, false),
                Self::Empty => Self::Empty,
            },
            Some(value) if self.singleton() == Some(value) => Self::Empty,
            _ => self,
        }
    }

    pub fn assume_lt(self, other: Self) -> Self {
        let Some(other_max) = other.max() else {
            return Self::Empty;
        };
        let Some(max) = other_max.checked_sub(1) else {
            return Self::Empty;
        };
        self.intersect(Self::bounded(i32::MIN, max))
    }

    pub fn assume_le(self, other: Self) -> Self {
        let Some(other_max) = other.max() else {
            return Self::Empty;
        };
        self.intersect(Self::bounded(i32::MIN, other_max))
    }

    pub fn assume_gt(self, other: Self) -> Self {
        let Some(other_min) = other.min() else {
            return Self::Empty;
        };
        let Some(min) = other_min.checked_add(1) else {
            return Self::Empty;
        };
        self.intersect(Self::bounded(min, i32::MAX))
    }

    pub fn assume_ge(self, other: Self) -> Self {
        let Some(other_min) = other.min() else {
            return Self::Empty;
        };
        self.intersect(Self::bounded(other_min, i32::MAX))
    }

    pub fn widen(self, next: Self) -> Self {
        match (self, next) {
            (Self::Empty, range) => range,
            (range, Self::Empty) => range,
            (
                Self::Bounded {
                    min: old_min,
                    max: old_max,
                    contains_zero: old_zero,
                },
                Self::Bounded {
                    min: new_min,
                    max: new_max,
                    contains_zero: new_zero,
                },
            ) => Self::normalized(
                if new_min < old_min { i32::MIN } else { old_min },
                if new_max > old_max { i32::MAX } else { old_max },
                old_zero || new_zero,
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RangeContext {
    Function,
    BlockEntry(BasicBlock),
    Edge(LogicalEdge),
    Before(Inst),
}

type State = FxHashMap<Inst, IntRange>;

#[derive(Debug, Clone)]
enum ValueDef {
    Integer(i32),
    ZeroInit,
    Undef,
    Binary(BinaryOp, Inst, Inst),
    Select(Inst, Inst, Inst),
    Cast {
        src: Inst,
        source_float: Option<f32>,
    },
    BlockParameter,
    Unknown,
}

pub struct RangeAnalysis {
    definitions: FxHashMap<Inst, ValueDef>,
    ranges: State,
    block_entries: FxHashMap<BasicBlock, State>,
    edge_states: FxHashMap<LogicalEdge, State>,
    before: FxHashMap<Inst, State>,
    loop_caps: FxHashMap<Inst, IntRange>,
    loop_headers: FxHashSet<BasicBlock>,
}

impl RangeAnalysis {
    pub fn new(
        arena: &ArenaContext<'_>,
        cfg: &CFG,
        loops: &LoopAnalysis,
        induction: &BasicInductionVariableAnalysis,
    ) -> Self {
        let data = arena.curr_func_data();
        let mut definitions = FxHashMap::default();
        for (&inst, inst_data) in arena.inst_datas() {
            let definition = if !inst_data.ty().is_i32() {
                ValueDef::Unknown
            } else {
                match inst_data.kind() {
                    InstKind::Integer(value) => ValueDef::Integer(value.value()),
                    InstKind::ZeroInit => ValueDef::ZeroInit,
                    InstKind::Undef | InstKind::Load(_) | InstKind::Call(_) => ValueDef::Undef,
                    InstKind::Binary(binary) => {
                        ValueDef::Binary(binary.op(), binary.lhs(), binary.rhs())
                    }
                    InstKind::Select(select) => {
                        ValueDef::Select(select.cond(), select.if_true(), select.if_false())
                    }
                    InstKind::Cast(cast) => ValueDef::Cast {
                        src: cast.src(),
                        source_float: match arena.inst_data(cast.src()).kind() {
                            InstKind::Float(value) => Some(value.value()),
                            _ => None,
                        },
                    },
                    InstKind::BlockArgRef(_) => ValueDef::BlockParameter,
                    _ => ValueDef::Unknown,
                }
            };
            definitions.insert(inst, definition);
        }

        let loop_headers = loops.loops().iter().map(Loop::header).collect();
        let mut analysis = Self {
            definitions,
            ranges: State::default(),
            block_entries: FxHashMap::default(),
            edge_states: FxHashMap::default(),
            before: FxHashMap::default(),
            loop_caps: FxHashMap::default(),
            loop_headers,
        };
        analysis.solve(data, cfg);
        analysis.add_induction_caps(arena, loops, induction);
        if !analysis.loop_caps.is_empty() {
            analysis.ranges.clear();
            analysis.block_entries.clear();
            analysis.edge_states.clear();
            analysis.before.clear();
            analysis.solve(data, cfg);
        }
        analysis
    }

    pub fn range_of(&self, value: Inst) -> IntRange {
        self.range_in_context(value, RangeContext::Function)
    }

    pub fn range_at_block_entry(&self, block: BasicBlock, value: Inst) -> IntRange {
        self.range_in_context(value, RangeContext::BlockEntry(block))
    }

    pub fn range_on_edge(&self, edge: LogicalEdge, value: Inst) -> IntRange {
        self.range_in_context(value, RangeContext::Edge(edge))
    }

    pub fn range_before(&self, instruction: Inst, value: Inst) -> IntRange {
        self.range_in_context(value, RangeContext::Before(instruction))
    }

    pub fn loop_header_range(&self, header: BasicBlock, parameter: Inst) -> IntRange {
        self.range_at_block_entry(header, parameter)
    }

    pub fn range_in_context(&self, value: Inst, context: RangeContext) -> IntRange {
        let facts = match context {
            RangeContext::Function => &self.ranges,
            RangeContext::BlockEntry(block) => match self.block_entries.get(&block) {
                Some(state) => state,
                None => return self.base_range(value),
            },
            RangeContext::Edge(edge) => match self.edge_states.get(&edge) {
                Some(state) => state,
                None => return IntRange::empty(),
            },
            RangeContext::Before(inst) => match self.before.get(&inst) {
                Some(state) => state,
                None => return self.base_range(value),
            },
        };
        self.evaluate_contextual(value, facts, &mut FxHashSet::default(), 0)
    }

    pub fn proves_binary_no_signed_wrap(
        &self,
        op: BinaryOp,
        lhs: Inst,
        rhs: Inst,
        context: RangeContext,
    ) -> bool {
        if !matches!(
            op,
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Shl
        ) {
            return false;
        }
        let lhs = self.range_in_context(lhs, context);
        let rhs = self.range_in_context(rhs, context);
        mathematical_binary(op, lhs, rhs).is_some()
    }

    fn solve(&mut self, data: &crate::ir::FunctionData, cfg: &CFG) {
        let entry = cfg.entry();
        let mut initial = State::default();
        for &parameter in data.bb_data(entry).params() {
            if data.inst_data(parameter).ty().is_i32() {
                initial.insert(parameter, IntRange::full());
            }
        }
        self.block_entries.insert(entry, initial);

        let mut worklist = std::collections::VecDeque::from([entry]);
        let mut queued = FxHashSet::from_iter([entry]);
        let mut header_changes: FxHashMap<(BasicBlock, Inst), usize> = FxHashMap::default();
        while let Some(block) = worklist.pop_front() {
            queued.remove(&block);
            let Some(mut state) = self.block_entries.get(&block).cloned() else {
                continue;
            };
            for &inst in data.layout().basicblock(block).insts() {
                if data.inst_data(inst).ty().is_i32() {
                    // A value carried around a backedge is an old observation,
                    // not the result of this execution of its defining instruction.
                    state.remove(&inst);
                    self.before.insert(inst, state.clone());
                    let range = self.eval_with_state(inst, &state);
                    state.insert(inst, range);
                    self.ranges
                        .entry(inst)
                        .and_modify(|old| *old = old.join(range))
                        .or_insert(range);
                } else {
                    self.before.insert(inst, state.clone());
                }
            }

            for edge in outgoing_edges(data, block) {
                let mut edge_state = state.clone();
                if !self.refine_edge(data, edge, &mut edge_state) {
                    continue;
                }
                let changed_edge = match self.edge_states.get_mut(&edge) {
                    Some(old) => join_state(old, &edge_state),
                    None => {
                        self.edge_states.insert(edge, edge_state);
                        true
                    }
                };
                if !changed_edge {
                    continue;
                }
                let target = edge.target(data);
                let mut incoming = self.join_incoming(data, cfg, target);
                if self.loop_headers.contains(&target) {
                    for &parameter in data.bb_data(target).params() {
                        let Some(next) = incoming.get(&parameter).copied() else {
                            continue;
                        };
                        let old = self
                            .block_entries
                            .get(&target)
                            .and_then(|state| state.get(&parameter))
                            .copied()
                            .unwrap_or(IntRange::empty());
                        let changes = header_changes.entry((target, parameter)).or_default();
                        let mut value = if *changes >= WIDEN_AFTER {
                            old.widen(next)
                        } else {
                            next
                        };
                        if value != old {
                            *changes += 1;
                        }
                        if let Some(cap) = self.loop_caps.get(&parameter) {
                            value = value.intersect(*cap);
                        }
                        incoming.insert(parameter, value);
                    }
                }
                let changed = match self.block_entries.get_mut(&target) {
                    Some(old) => join_state(old, &incoming),
                    None => {
                        self.block_entries.insert(target, incoming);
                        true
                    }
                };
                if changed && queued.insert(target) {
                    worklist.push_back(target);
                }
            }
        }

        for state in self.block_entries.values().chain(self.edge_states.values()) {
            for (&value, &range) in state {
                self.ranges
                    .entry(value)
                    .and_modify(|old| *old = old.join(range))
                    .or_insert(range);
            }
        }
    }

    fn join_incoming(
        &self,
        data: &crate::ir::FunctionData,
        cfg: &CFG,
        target: BasicBlock,
    ) -> State {
        let mut result = State::default();
        let params = data.bb_data(target).params();
        for edge in incoming_edges(data, cfg, target) {
            let Some(state) = self.edge_states.get(&edge) else {
                continue;
            };
            for (&parameter, &argument) in params.iter().zip(edge.args(data)) {
                if !data.inst_data(parameter).ty().is_i32() {
                    continue;
                }
                let range = self.evaluate_contextual(argument, state, &mut FxHashSet::default(), 0);
                result
                    .entry(parameter)
                    .and_modify(|old| *old = old.join(range))
                    .or_insert(range);
            }
            for (&value, &range) in state {
                result
                    .entry(value)
                    .and_modify(|old| *old = old.join(range))
                    .or_insert(range);
            }
        }
        result
    }

    fn refine_edge(
        &self,
        data: &crate::ir::FunctionData,
        edge: LogicalEdge,
        state: &mut State,
    ) -> bool {
        if !matches!(edge.arm(), LogicalEdgeArm::True | LogicalEdgeArm::False) {
            return true;
        }
        let InstKind::Branch(branch) = data.inst_data(edge.terminator()).kind() else {
            return true;
        };
        let truth = edge.arm() == LogicalEdgeArm::True;
        let condition = branch.cond();
        let condition_range = self.eval_with_state(condition, state);
        if (truth && condition_range.singleton() == Some(0))
            || (!truth && condition_range.excludes_zero())
        {
            return false;
        }
        let refined_condition = if truth {
            condition_range.assume_ne(IntRange::constant(0))
        } else {
            condition_range.assume_eq(IntRange::constant(0))
        };
        if refined_condition == IntRange::empty() {
            return false;
        }
        state.insert(condition, refined_condition);

        let Some(ValueDef::Binary(mut op, lhs, rhs)) = self.definitions.get(&condition).cloned()
        else {
            return true;
        };
        if !op.is_compare() {
            return true;
        }
        if !data.inst_data(lhs).ty().is_i32() || !data.inst_data(rhs).ty().is_i32() {
            return true;
        }
        if !truth {
            op = op.complement_integer_compare().unwrap();
        }
        let lhs_range = self.eval_with_state(lhs, state);
        let rhs_range = self.eval_with_state(rhs, state);
        let (new_lhs, new_rhs) = refine_comparison(op, lhs_range, rhs_range);
        if new_lhs == IntRange::empty() || new_rhs == IntRange::empty() {
            return false;
        }
        state.insert(lhs, new_lhs);
        state.insert(rhs, new_rhs);
        true
    }

    fn add_induction_caps(
        &mut self,
        arena: &ArenaContext<'_>,
        loops: &LoopAnalysis,
        induction: &BasicInductionVariableAnalysis,
    ) {
        let data = arena.curr_func_data();
        for looop in loops.loops() {
            let terminator = data.layout().basicblock(looop.header()).terminator();
            let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
                continue;
            };
            let true_inside = looop.contains(branch.t_target());
            let false_inside = looop.contains(branch.f_target());
            if true_inside == false_inside {
                continue;
            }
            let InstKind::Binary(compare) = data.inst_data(branch.cond()).kind() else {
                continue;
            };
            for iv in induction.for_loop(looop) {
                let Some(step) = self.constant_step(iv.step()) else {
                    continue;
                };
                if step == 0 {
                    continue;
                }
                let mut op = compare.op();
                if !true_inside {
                    let Some(complement) = op.complement_integer_compare() else {
                        continue;
                    };
                    op = complement;
                }
                let bound_value = if compare.lhs() == iv.parameter() {
                    compare.rhs()
                } else if compare.rhs() == iv.parameter() {
                    let Some(swapped) = op.swap_compare_args() else {
                        continue;
                    };
                    op = swapped;
                    compare.lhs()
                } else {
                    continue;
                };
                let direction = if step > 0 {
                    InductionDirection::Forward
                } else {
                    InductionDirection::Backward
                };
                if !matches!(
                    (direction, op),
                    (InductionDirection::Forward, BinaryOp::Lt)
                        | (InductionDirection::Backward, BinaryOp::Gt)
                ) {
                    continue;
                }
                if step.unsigned_abs() != 1 {
                    let Some(bound) = self.constant_value(bound_value) else {
                        continue;
                    };
                    let no_wrap = match direction {
                        InductionDirection::Forward => {
                            i64::from(bound) + i64::from(step) - 1 <= i64::from(i32::MAX)
                        }
                        InductionDirection::Backward => {
                            i64::from(bound) + i64::from(step) + 1 >= i64::from(i32::MIN)
                        }
                    };
                    if !no_wrap {
                        continue;
                    }
                }
                let Some(cap) = self.constant_induction_cap(iv, direction, step, bound_value)
                else {
                    continue;
                };
                self.loop_caps
                    .entry(iv.parameter())
                    .and_modify(|old| *old = old.join(cap))
                    .or_insert(cap);
            }
        }
    }

    fn constant_induction_cap(
        &self,
        iv: &BasicInductionVariable,
        direction: InductionDirection,
        signed_step: i32,
        bound: Inst,
    ) -> Option<IntRange> {
        let bound = self.constant_value(bound)?;
        let initial_values = iv
            .initial_values()
            .iter()
            .map(|&value| self.constant_value(value))
            .collect::<Option<Vec<_>>>()?;
        let initial_min = initial_values.iter().copied().min()?;
        let initial_max = initial_values.iter().copied().max()?;

        match direction {
            InductionDirection::Forward => {
                let terminal = i64::from(bound)
                    .checked_sub(1)?
                    .checked_add(i64::from(signed_step))?;
                Some(IntRange::bounded(
                    initial_min,
                    initial_max.max(i32::try_from(terminal).ok()?),
                ))
            }
            InductionDirection::Backward => {
                let terminal = i64::from(bound)
                    .checked_add(1)?
                    .checked_add(i64::from(signed_step))?;
                Some(IntRange::bounded(
                    initial_min.min(i32::try_from(terminal).ok()?),
                    initial_max,
                ))
            }
        }
    }

    fn constant_step(&self, step: InductionStep) -> Option<i32> {
        match step {
            InductionStep::Add(value) => self.constant_value(value),
            InductionStep::Sub(value) => self.constant_value(value)?.checked_neg(),
        }
    }

    fn constant_value(&self, value: Inst) -> Option<i32> {
        match self.definitions.get(&value) {
            Some(ValueDef::Integer(value)) => Some(*value),
            Some(ValueDef::ZeroInit) => Some(0),
            _ => None,
        }
    }

    fn base_range(&self, value: Inst) -> IntRange {
        match self.definitions.get(&value) {
            Some(ValueDef::Integer(value)) => IntRange::constant(*value),
            Some(ValueDef::ZeroInit) => IntRange::constant(0),
            Some(ValueDef::Unknown) | Some(ValueDef::Undef) | None => IntRange::full(),
            _ => self.ranges.get(&value).copied().unwrap_or(IntRange::full()),
        }
    }

    fn eval_with_state(&self, value: Inst, state: &State) -> IntRange {
        self.evaluate_contextual(value, state, &mut FxHashSet::default(), 0)
    }

    fn evaluate_contextual(
        &self,
        value: Inst,
        facts: &State,
        visiting: &mut FxHashSet<Inst>,
        depth: usize,
    ) -> IntRange {
        if let Some(range) = facts.get(&value) {
            return *range;
        }
        if depth >= CONTEXT_DEPTH_LIMIT || !visiting.insert(value) {
            return self.base_range(value);
        }
        let range = match self.definitions.get(&value) {
            Some(ValueDef::Integer(value)) => IntRange::constant(*value),
            Some(ValueDef::ZeroInit) => IntRange::constant(0),
            Some(ValueDef::Binary(op, lhs, rhs)) => transfer_binary(
                *op,
                self.evaluate_contextual(*lhs, facts, visiting, depth + 1),
                self.evaluate_contextual(*rhs, facts, visiting, depth + 1),
            ),
            Some(ValueDef::Select(condition, if_true, if_false)) => {
                let condition = self.evaluate_contextual(*condition, facts, visiting, depth + 1);
                if condition.singleton() == Some(0) {
                    self.evaluate_contextual(*if_false, facts, visiting, depth + 1)
                } else if condition.excludes_zero() {
                    self.evaluate_contextual(*if_true, facts, visiting, depth + 1)
                } else {
                    self.evaluate_contextual(*if_true, facts, visiting, depth + 1)
                        .join(self.evaluate_contextual(*if_false, facts, visiting, depth + 1))
                }
            }
            Some(ValueDef::Cast { src, source_float }) => {
                if let Some(value) = source_float.and_then(fold_f32_to_i32) {
                    IntRange::constant(value)
                } else if matches!(self.definitions.get(src), Some(ValueDef::Integer(_))) {
                    self.evaluate_contextual(*src, facts, visiting, depth + 1)
                } else {
                    IntRange::full()
                }
            }
            Some(ValueDef::BlockParameter) => {
                self.ranges.get(&value).copied().unwrap_or(IntRange::full())
            }
            Some(ValueDef::Undef) | Some(ValueDef::Unknown) | None => IntRange::full(),
        };
        visiting.remove(&value);
        range
    }
}

fn join_state(target: &mut State, incoming: &State) -> bool {
    let mut changed = false;
    for (&value, &range) in incoming {
        match target.get_mut(&value) {
            Some(old) => {
                let joined = old.join(range);
                if joined != *old {
                    *old = joined;
                    changed = true;
                }
            }
            None => {
                target.insert(value, range);
                changed = true;
            }
        }
    }
    changed
}

fn refine_comparison(op: BinaryOp, lhs: IntRange, rhs: IntRange) -> (IntRange, IntRange) {
    match op {
        BinaryOp::Eq => (lhs.assume_eq(rhs), rhs.assume_eq(lhs)),
        BinaryOp::NotEq => (lhs.assume_ne(rhs), rhs.assume_ne(lhs)),
        BinaryOp::Lt => (lhs.assume_lt(rhs), rhs.assume_gt(lhs)),
        BinaryOp::Le => (lhs.assume_le(rhs), rhs.assume_ge(lhs)),
        BinaryOp::Gt => (lhs.assume_gt(rhs), rhs.assume_lt(lhs)),
        BinaryOp::Ge => (lhs.assume_ge(rhs), rhs.assume_le(lhs)),
        _ => (lhs, rhs),
    }
}

fn transfer_binary(op: BinaryOp, lhs: IntRange, rhs: IntRange) -> IntRange {
    if lhs == IntRange::empty() || rhs == IntRange::empty() {
        return IntRange::empty();
    }
    if let (Some(lhs), Some(rhs)) = (lhs.singleton(), rhs.singleton()) {
        if matches!(op, BinaryOp::Div | BinaryOp::Rem) && rhs == 0 {
            return IntRange::full();
        }
        return IntRange::constant(fold_binary(op, lhs, rhs));
    }
    if op.is_compare() {
        return comparison_range(op, lhs, rhs);
    }
    if let Some(range) = mathematical_binary(op, lhs, rhs) {
        return range;
    }
    match op {
        BinaryOp::And if rhs.singleton() == Some(0) || lhs.singleton() == Some(0) => {
            IntRange::constant(0)
        }
        BinaryOp::And => {
            let mask = lhs.singleton().or_else(|| rhs.singleton());
            match mask {
                Some(mask) if mask >= 0 => IntRange::bounded(0, mask),
                _ => IntRange::full(),
            }
        }
        BinaryOp::Or | BinaryOp::Xor if rhs.singleton() == Some(0) => lhs,
        BinaryOp::Or | BinaryOp::Xor if lhs.singleton() == Some(0) => rhs,
        BinaryOp::Div => transfer_div(lhs, rhs),
        BinaryOp::Rem => transfer_rem(lhs, rhs),
        BinaryOp::Shr => IntRange::bounded(0, i32::MAX),
        BinaryOp::Sar => match rhs.singleton().filter(|shift| (0..32).contains(shift)) {
            Some(shift) => {
                let shift = shift as u32;
                IntRange::bounded(lhs.min().unwrap() >> shift, lhs.max().unwrap() >> shift)
            }
            None => IntRange::full(),
        },
        _ => IntRange::full(),
    }
}

fn mathematical_binary(op: BinaryOp, lhs: IntRange, rhs: IntRange) -> Option<IntRange> {
    let (lhs_min, lhs_max, rhs_min, rhs_max) = (
        i64::from(lhs.min()?),
        i64::from(lhs.max()?),
        i64::from(rhs.min()?),
        i64::from(rhs.max()?),
    );
    let (min, max) = match op {
        BinaryOp::Add => (lhs_min + rhs_min, lhs_max + rhs_max),
        BinaryOp::Sub => (lhs_min - rhs_max, lhs_max - rhs_min),
        BinaryOp::Mul => {
            let values = [
                lhs_min * rhs_min,
                lhs_min * rhs_max,
                lhs_max * rhs_min,
                lhs_max * rhs_max,
            ];
            (*values.iter().min().unwrap(), *values.iter().max().unwrap())
        }
        BinaryOp::Shl if rhs_min == rhs_max && (0..32).contains(&rhs_min) => {
            let factor = 1_i64 << rhs_min;
            let values = [lhs_min * factor, lhs_max * factor];
            (values[0].min(values[1]), values[0].max(values[1]))
        }
        BinaryOp::Min => (lhs_min.min(rhs_min), lhs_max.min(rhs_max)),
        BinaryOp::Max => (lhs_min.max(rhs_min), lhs_max.max(rhs_max)),
        _ => return None,
    };
    if min < i64::from(i32::MIN) || max > i64::from(i32::MAX) {
        return None;
    }
    Some(IntRange::bounded(min as i32, max as i32))
}

fn comparison_range(op: BinaryOp, lhs: IntRange, rhs: IntRange) -> IntRange {
    let (lhs_min, lhs_max, rhs_min, rhs_max) = (
        lhs.min().unwrap(),
        lhs.max().unwrap(),
        rhs.min().unwrap(),
        rhs.max().unwrap(),
    );
    let proven_true = match op {
        BinaryOp::Eq => lhs.singleton().is_some() && lhs.singleton() == rhs.singleton(),
        BinaryOp::NotEq => lhs_max < rhs_min || rhs_max < lhs_min,
        BinaryOp::Lt => lhs_max < rhs_min,
        BinaryOp::Le => lhs_max <= rhs_min,
        BinaryOp::Gt => lhs_min > rhs_max,
        BinaryOp::Ge => lhs_min >= rhs_max,
        _ => false,
    };
    let proven_false = match op {
        BinaryOp::Eq => lhs_max < rhs_min || rhs_max < lhs_min,
        BinaryOp::NotEq => lhs.singleton().is_some() && lhs.singleton() == rhs.singleton(),
        BinaryOp::Lt => lhs_min >= rhs_max,
        BinaryOp::Le => lhs_min > rhs_max,
        BinaryOp::Gt => lhs_max <= rhs_min,
        BinaryOp::Ge => lhs_max < rhs_min,
        _ => false,
    };
    if proven_true {
        IntRange::constant(1)
    } else if proven_false {
        IntRange::constant(0)
    } else {
        IntRange::bounded(0, 1)
    }
}

fn transfer_div(lhs: IntRange, rhs: IntRange) -> IntRange {
    let Some(divisor) = rhs.singleton() else {
        return IntRange::full();
    };
    if divisor == 0 || (divisor == -1 && lhs.contains(i32::MIN)) {
        return IntRange::full();
    }
    let values = [lhs.min().unwrap() / divisor, lhs.max().unwrap() / divisor];
    IntRange::bounded(values[0].min(values[1]), values[0].max(values[1]))
}

fn transfer_rem(_lhs: IntRange, rhs: IntRange) -> IntRange {
    let Some(divisor) = rhs.singleton() else {
        return IntRange::full();
    };
    if divisor == 0 {
        return IntRange::full();
    }
    let magnitude = divisor
        .unsigned_abs()
        .saturating_sub(1)
        .min(i32::MAX as u32) as i32;
    IntRange::bounded(-magnitude, magnitude)
}

fn fold_binary(op: BinaryOp, lhs: i32, rhs: i32) -> i32 {
    match op {
        BinaryOp::Add => lhs.wrapping_add(rhs),
        BinaryOp::Sub => lhs.wrapping_sub(rhs),
        BinaryOp::Mul => lhs.wrapping_mul(rhs),
        BinaryOp::Div => lhs.wrapping_div(rhs),
        BinaryOp::Rem => lhs.wrapping_rem(rhs),
        BinaryOp::NotEq => (lhs != rhs) as i32,
        BinaryOp::Eq => (lhs == rhs) as i32,
        BinaryOp::Gt => (lhs > rhs) as i32,
        BinaryOp::Lt => (lhs < rhs) as i32,
        BinaryOp::Ge => (lhs >= rhs) as i32,
        BinaryOp::Le => (lhs <= rhs) as i32,
        BinaryOp::And => lhs & rhs,
        BinaryOp::Or => lhs | rhs,
        BinaryOp::Xor => lhs ^ rhs,
        BinaryOp::Shl => lhs.wrapping_shl(rhs as u32),
        BinaryOp::Shr => (lhs as u32).wrapping_shr(rhs as u32) as i32,
        BinaryOp::Sar => lhs.wrapping_shr(rhs as u32),
        BinaryOp::Min => lhs.min(rhs),
        BinaryOp::Max => lhs.max(rhs),
    }
}

fn fold_f32_to_i32(value: f32) -> Option<i32> {
    (value.is_finite() && value >= i32::MIN as f32 && value < i32::MAX as f32).then(|| value as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{Program, Type, builder_trait::*},
        opt::analysis_passes::{
            induction_variable::BasicInductionVariableAnalysis, loop_analysis::LoopAnalysis,
        },
    };

    fn analyze(program: &Program, function: crate::ir::Function) -> RangeAnalysis {
        let data = program.func_data(function);
        let (cfg, _, loops) = LoopAnalysis::new(data);
        let ivs = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        let arena = ArenaContext {
            program,
            curr_func: Some(function),
        };
        RangeAnalysis::new(&arena, &cfg, &loops, &ivs)
    }

    #[test]
    fn lattice_and_assumptions() {
        let a = IntRange::bounded(-4, 8);
        let b = IntRange::bounded(2, 12);
        assert_eq!(a.join(b), b.join(a));
        assert_eq!(a.intersect(b), b.intersect(a));
        assert!(a.intersect(b).is_subset_of(a));
        assert_eq!(a.assume_eq(b), IntRange::bounded(2, 8));
        assert!(a.assume_ne(IntRange::constant(0)).excludes_zero());
        assert_eq!(a.assume_lt(IntRange::constant(3)).max(), Some(2));
        assert_eq!(a.assume_gt(IntRange::constant(3)).min(), Some(4));
        assert_eq!(
            IntRange::bounded(0, 1)
                .assume_ne(IntRange::constant(0))
                .singleton(),
            Some(1)
        );
    }

    #[test]
    fn wrapping_singletons_and_interval_overflow() {
        assert_eq!(
            transfer_binary(
                BinaryOp::Add,
                IntRange::constant(i32::MAX),
                IntRange::constant(1),
            ),
            IntRange::constant(i32::MIN)
        );
        assert_eq!(
            transfer_binary(
                BinaryOp::Add,
                IntRange::bounded(0, i32::MAX),
                IntRange::constant(1)
            ),
            IntRange::full()
        );
        assert_eq!(
            transfer_binary(
                BinaryOp::Mul,
                IntRange::bounded(-2, 3),
                IntRange::constant(4)
            ),
            IntRange::bounded(-8, 12)
        );
    }

    #[test]
    fn diamond_refines_branch_condition_and_parameter_join() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "diamond".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        for block in [left, right, merge] {
            data.layout_mut().push_bb_back(block);
        }
        let x = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let compare = data.new_local_inst().binary(BinaryOp::Gt, x, zero);
        let branch = data
            .new_local_inst()
            .branch(compare, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(entry, compare);
        data.layout_mut().insert_inst(entry, branch);
        let one = data.new_local_inst().integer(1);
        let left_jump = data.new_local_inst().jump(merge, vec![one]);
        data.layout_mut().insert_inst(left, left_jump);
        let minus_one = data.new_local_inst().integer(-1);
        let right_jump = data.new_local_inst().jump(merge, vec![minus_one]);
        data.layout_mut().insert_inst(right, right_jump);
        let parameter = data.bb_data(merge).params()[0];
        let ret = data.new_local_inst().ret(Some(parameter));
        data.layout_mut().insert_inst(merge, ret);

        let analysis = analyze(&program, function);
        let data = program.func_data(function);
        let true_edge = outgoing_edges(data, entry)[0];
        let false_edge = outgoing_edges(data, entry)[1];
        assert_eq!(analysis.range_on_edge(true_edge, x).min(), Some(1));
        assert_eq!(analysis.range_on_edge(false_edge, x).max(), Some(0));
        assert_eq!(
            analysis.range_at_block_entry(merge, parameter),
            IntRange::bounded(-1, 1).assume_ne(IntRange::constant(0))
        );
    }

    #[test]
    fn same_target_arms_keep_distinct_arguments() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "arms".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        data.layout_mut().push_bb_back(merge);
        let condition = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let seven = data.new_local_inst().integer(7);
        let branch = data
            .new_local_inst()
            .branch(condition, merge, vec![zero], merge, vec![seven]);
        data.layout_mut().insert_inst(entry, branch);
        let parameter = data.bb_data(merge).params()[0];
        let ret = data.new_local_inst().ret(Some(parameter));
        data.layout_mut().insert_inst(merge, ret);
        let analysis = analyze(&program, function);
        let edges = outgoing_edges(program.func_data(function), entry);
        assert_eq!(
            analysis.range_on_edge(edges[0], condition).excludes_zero(),
            true
        );
        assert_eq!(
            analysis.range_on_edge(edges[1], condition),
            IntRange::constant(0)
        );
        assert_eq!(
            analysis.range_at_block_entry(merge, parameter),
            IntRange::bounded(0, 7)
        );
    }

    #[test]
    fn float_comparison_does_not_refine_float_operands_as_integers() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "float_branch".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let on_true = data.new_basic_block().basic_block("true".into(), vec![]);
        let on_false = data.new_basic_block().basic_block("false".into(), vec![]);
        data.layout_mut().push_bb_back(on_true);
        data.layout_mut().push_bb_back(on_false);

        let nan = data.new_local_inst().float(f32::NAN);
        let zero = data.new_local_inst().float(0.0);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, nan, zero);
        let branch = data
            .new_local_inst()
            .branch(compare, on_true, vec![], on_false, vec![]);
        data.layout_mut().insert_inst(entry, compare);
        data.layout_mut().insert_inst(entry, branch);
        let one = data.new_local_inst().integer(1);
        let true_ret = data.new_local_inst().ret(Some(one));
        data.layout_mut().insert_inst(on_true, true_ret);
        let zero_int = data.new_local_inst().integer(0);
        let false_ret = data.new_local_inst().ret(Some(zero_int));
        data.layout_mut().insert_inst(on_false, false_ret);

        let analysis = analyze(&program, function);
        let edges = outgoing_edges(program.func_data(function), entry);
        assert_eq!(analysis.range_on_edge(edges[0], nan), IntRange::full());
        assert_eq!(analysis.range_on_edge(edges[1], nan), IntRange::full());
    }

    #[test]
    fn joins_block_parameters_from_distinct_predecessors() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "join".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        for block in [left, right, merge] {
            data.layout_mut().push_bb_back(block);
        }
        let condition = data.params()[0];
        let branch = data
            .new_local_inst()
            .branch(condition, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(entry, branch);
        let three = data.new_local_inst().integer(3);
        let left_jump = data.new_local_inst().jump(merge, vec![three]);
        data.layout_mut().insert_inst(left, left_jump);
        let nine = data.new_local_inst().integer(9);
        let right_jump = data.new_local_inst().jump(merge, vec![nine]);
        data.layout_mut().insert_inst(right, right_jump);
        let parameter = data.bb_data(merge).params()[0];
        let ret = data.new_local_inst().ret(Some(parameter));
        data.layout_mut().insert_inst(merge, ret);

        let analysis = analyze(&program, function);
        assert_eq!(
            analysis.range_at_block_entry(merge, parameter),
            IntRange::bounded(3, 9)
        );
    }

    #[test]
    fn loop_cap_includes_failing_header_visit() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "loop".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }
        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let ten = data.new_local_inst().integer(10);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, ten);
        let branch = data
            .new_local_inst()
            .branch(compare, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, compare);
        data.layout_mut().insert_inst(header, branch);
        let one = data.new_local_inst().integer(1);
        let next = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let back = data.new_local_inst().jump(header, vec![next]);
        data.layout_mut().insert_inst(body, next);
        data.layout_mut().insert_inst(body, back);
        let ret = data.new_local_inst().ret(Some(iv));
        data.layout_mut().insert_inst(exit, ret);
        let analysis = analyze(&program, function);
        assert_eq!(
            analysis.loop_header_range(header, iv),
            IntRange::bounded(0, 10)
        );
    }

    #[test]
    fn unrecognized_loop_recurrence_widens_to_terminate() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "widen".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        data.layout_mut().push_bb_back(header);
        let one = data.new_local_inst().integer(1);
        let start = data.new_local_inst().jump(header, vec![one]);
        data.layout_mut().insert_inst(entry, start);
        let value = data.bb_data(header).params()[0];
        let two = data.new_local_inst().integer(2);
        let next = data.new_local_inst().binary(BinaryOp::Mul, value, two);
        let back = data.new_local_inst().jump(header, vec![next]);
        data.layout_mut().insert_inst(header, next);
        data.layout_mut().insert_inst(header, back);
        let analysis = analyze(&program, function);
        assert_eq!(
            analysis.loop_header_range(header, value).max(),
            Some(i32::MAX)
        );
    }

    #[test]
    fn proves_no_signed_wrap_in_context() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "proof".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let ten = data.new_local_inst().integer(10);
        let twenty = data.new_local_inst().integer(20);
        let max = data.new_local_inst().integer(i32::MAX);
        let add = data.new_local_inst().binary(BinaryOp::Add, ten, twenty);
        let ret = data.new_local_inst().ret(Some(add));
        data.layout_mut().insert_inst(entry, add);
        data.layout_mut().insert_inst(entry, ret);
        let analysis = analyze(&program, function);
        assert!(analysis.proves_binary_no_signed_wrap(
            BinaryOp::Add,
            ten,
            twenty,
            RangeContext::Before(add)
        ));
        assert!(!analysis.proves_binary_no_signed_wrap(
            BinaryOp::Add,
            max,
            twenty,
            RangeContext::Before(add)
        ));
    }
}
