use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use crate::opt::{
    analysis_passes::loop_analysis::{Loop, LoopAnalysis},
    prelude::*,
    utils::{cfg::CFG, logical_edge::incoming_edges},
};

/// The normalized update of a basic induction variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InductionStep {
    Add(Inst),
    Sub(Inst),
}

impl InductionStep {
    pub fn value(self) -> Inst {
        match self {
            Self::Add(value) | Self::Sub(value) => value,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BasicInductionVariable {
    parameter: Inst,
    initial_values: SmallVec<[Inst; 2]>,
    update_values: SmallVec<[Inst; 2]>,
    step: InductionStep,
}

impl BasicInductionVariable {
    pub fn parameter(&self) -> Inst {
        self.parameter
    }

    pub fn initial_values(&self) -> &[Inst] {
        &self.initial_values
    }

    pub fn update_values(&self) -> &[Inst] {
        &self.update_values
    }

    pub fn step(&self) -> InductionStep {
        self.step
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InductionDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NormalizedInductionExit {
    direction: InductionDirection,
    signed_step: i32,
    bound: Inst,
}

impl NormalizedInductionExit {
    pub fn direction(self) -> InductionDirection {
        self.direction
    }

    pub fn signed_step(self) -> i32 {
        self.signed_step
    }

    pub fn bound(self) -> Inst {
        self.bound
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TripCountEstimate {
    Exact(u64),
    UpperBound(u64),
}

impl TripCountEstimate {
    pub fn exact(self) -> Option<u64> {
        match self {
            Self::Exact(iterations) => Some(iterations),
            Self::UpperBound(_) => None,
        }
    }

    pub fn upper_bound(self) -> u64 {
        match self {
            Self::Exact(iterations) | Self::UpperBound(iterations) => iterations,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConstantInductionRange {
    min: i32,
    max: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConstantTripCount {
    iterations: usize,
    initial: i32,
    bound: i32,
    signed_step: i32,
}

impl ConstantTripCount {
    pub fn iterations(self) -> usize {
        self.iterations
    }

    pub fn initial(self) -> i32 {
        self.initial
    }

    pub fn bound(self) -> i32 {
        self.bound
    }

    pub fn signed_step(self) -> i32 {
        self.signed_step
    }
}

impl ConstantInductionRange {
    pub fn min(self) -> i32 {
        self.min
    }

    pub fn max(self) -> i32 {
        self.max
    }
}

pub struct BasicInductionVariableAnalysis {
    by_header: FxHashMap<BasicBlock, Vec<BasicInductionVariable>>,
}

struct HeaderIncoming {
    source: BasicBlock,
    args: Vec<Inst>,
}

impl BasicInductionVariableAnalysis {
    pub fn new(data: &FunctionData, cfg: &CFG, loops: &LoopAnalysis) -> Self {
        let mut by_header = FxHashMap::default();
        for looop in loops.loops() {
            let header = looop.header();
            let header_params = data.bb_data(header).params();
            let incoming = header_incoming(data, cfg, looop, header_params.len());
            let loop_params = looop
                .body()
                .iter()
                .flat_map(|&block| data.bb_data(block).params().iter().copied())
                .collect::<FxHashSet<_>>();

            let variables = header_params
                .iter()
                .copied()
                .enumerate()
                .filter_map(|(position, parameter)| {
                    analyze_parameter(data, looop, &loop_params, &incoming, position, parameter)
                })
                .collect();
            by_header.insert(header, variables);
        }
        Self { by_header }
    }

    pub fn for_loop(&self, looop: &Loop) -> &[BasicInductionVariable] {
        self.by_header
            .get(&looop.header())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn find(&self, looop: &Loop, parameter: Inst) -> Option<&BasicInductionVariable> {
        self.for_loop(looop)
            .iter()
            .find(|variable| variable.parameter == parameter)
    }
}

/// Normalize a constant-step header exit to `iv < bound` or `iv > bound` while
/// the selected branch arm remains inside the loop. Non-unit steps require a
/// constant bound that proves the continuing update cannot wrap `i32`.
pub fn normalize_strict_exit(
    data: &ArenaContextMut<'_>,
    looop: &Loop,
    iv: &BasicInductionVariable,
) -> Option<NormalizedInductionExit> {
    let signed_step = match iv.step() {
        InductionStep::Add(step) => integer_constant(data, step)?,
        InductionStep::Sub(step) => integer_constant(data, step)?.checked_neg()?,
    };
    let direction = match signed_step.cmp(&0) {
        std::cmp::Ordering::Greater => InductionDirection::Forward,
        std::cmp::Ordering::Less => InductionDirection::Backward,
        std::cmp::Ordering::Equal => return None,
    };

    let terminator = data.layout().basicblock(looop.header()).terminator();
    let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
        return None;
    };
    let true_inside = looop.contains(branch.t_target());
    let false_inside = looop.contains(branch.f_target());
    if true_inside == false_inside {
        return None;
    }

    let InstKind::Binary(compare) = data.inst_data(branch.cond()).kind() else {
        return None;
    };
    if !data.inst_data(compare.lhs()).ty().is_i32() || !data.inst_data(compare.rhs()).ty().is_i32()
    {
        return None;
    }

    let mut op = compare.op();
    if !true_inside {
        op = op.complement_integer_compare()?;
    }
    let bound = if compare.lhs() == iv.parameter() {
        compare.rhs()
    } else if compare.rhs() == iv.parameter() {
        op = op.swap_compare_args()?;
        compare.lhs()
    } else {
        return None;
    };

    match (direction, op) {
        (InductionDirection::Forward, BinaryOp::Lt)
        | (InductionDirection::Backward, BinaryOp::Gt) => {}
        _ => return None,
    };

    if signed_step.unsigned_abs() != 1 {
        let bound = i64::from(integer_constant(data, bound)?);
        let signed_step = i64::from(signed_step);
        let no_wrap = match direction {
            InductionDirection::Forward => bound + signed_step - 1 <= i64::from(i32::MAX),
            InductionDirection::Backward => bound + signed_step + 1 >= i64::from(i32::MIN),
        };
        if !no_wrap {
            return None;
        }
    }

    Some(NormalizedInductionExit {
        direction,
        signed_step,
        bound,
    })
}

pub fn constant_induction_range(
    data: &ArenaContextMut<'_>,
    iv: &BasicInductionVariable,
    exit: NormalizedInductionExit,
) -> Option<ConstantInductionRange> {
    let bound = integer_constant(data, exit.bound())?;
    let initial_values = iv
        .initial_values()
        .iter()
        .map(|&value| integer_constant(data, value))
        .collect::<Option<Vec<_>>>()?;
    let initial_min = initial_values.iter().copied().min()?;
    let initial_max = initial_values.iter().copied().max()?;

    match exit.direction() {
        InductionDirection::Forward => {
            let terminal = i64::from(bound)
                .checked_sub(1)?
                .checked_add(i64::from(exit.signed_step()))?;
            Some(ConstantInductionRange {
                min: initial_min,
                max: initial_max.max(i32::try_from(terminal).ok()?),
            })
        }
        InductionDirection::Backward => {
            let terminal = i64::from(bound)
                .checked_add(1)?
                .checked_add(i64::from(exit.signed_step()))?;
            Some(ConstantInductionRange {
                min: initial_min.min(i32::try_from(terminal).ok()?),
                max: initial_max,
            })
        }
    }
}

/// Compute a constant trip count for a normalized strict induction exit.
/// Multiple entry values produce an upper bound unless they all have the same
/// trip count.
pub fn induction_trip_count(
    data: &ArenaContextMut<'_>,
    iv: &BasicInductionVariable,
    exit: NormalizedInductionExit,
) -> Option<TripCountEstimate> {
    let bound = integer_constant(data, exit.bound())?;
    let mut counts = iv.initial_values().iter().map(|&initial| {
        estimated_trip_count_values(
            integer_constant(data, initial)?,
            bound,
            exit.signed_step(),
            exit.direction(),
        )
    });
    let first = counts.next()??;
    let mut upper_bound = first;
    let mut exact = true;
    for count in counts {
        let count = count?;
        exact &= count == first;
        upper_bound = upper_bound.max(count);
    }
    Some(if exact {
        TripCountEstimate::Exact(first)
    } else {
        TripCountEstimate::UpperBound(upper_bound)
    })
}

fn estimated_trip_count_values(
    initial: i32,
    bound: i32,
    signed_step: i32,
    direction: InductionDirection,
) -> Option<u64> {
    let (distance, step) = match direction {
        InductionDirection::Forward => {
            if signed_step <= 0 {
                return None;
            }
            if initial >= bound {
                return Some(0);
            }
            (
                i64::from(bound).checked_sub(i64::from(initial))?,
                i64::from(signed_step),
            )
        }
        InductionDirection::Backward => {
            if signed_step >= 0 {
                return None;
            }
            if initial <= bound {
                return Some(0);
            }
            (
                i64::from(initial).checked_sub(i64::from(bound))?,
                i64::from(signed_step).checked_neg()?,
            )
        }
    };
    let distance = u64::try_from(distance).ok()?;
    let step = u64::try_from(step).ok()?;
    Some(distance.div_ceil(step))
}

pub fn constant_trip_count(
    data: &ArenaContextMut<'_>,
    iv: &BasicInductionVariable,
    exit: NormalizedInductionExit,
) -> Option<ConstantTripCount> {
    let [initial] = iv.initial_values() else {
        return None;
    };
    let initial = integer_constant(data, *initial)?;
    let bound = integer_constant(data, exit.bound())?;
    let iterations =
        constant_trip_count_values(initial, bound, exit.signed_step(), exit.direction())?;
    Some(ConstantTripCount {
        iterations,
        initial,
        bound,
        signed_step: exit.signed_step(),
    })
}

fn constant_trip_count_values(
    initial: i32,
    bound: i32,
    signed_step: i32,
    direction: InductionDirection,
) -> Option<usize> {
    let (distance, step) = match direction {
        InductionDirection::Forward => {
            if signed_step <= 0 {
                return None;
            }
            if initial >= bound {
                return Some(0);
            }
            (
                i64::from(bound).checked_sub(i64::from(initial))?,
                i64::from(signed_step),
            )
        }
        InductionDirection::Backward => {
            if signed_step >= 0 {
                return None;
            }
            if initial <= bound {
                return Some(0);
            }
            (
                i64::from(initial).checked_sub(i64::from(bound))?,
                i64::from(signed_step).checked_neg()?,
            )
        }
    };
    let rounded = distance.checked_add(step.checked_sub(1)?)?;
    usize::try_from(rounded.checked_div(step)?).ok()
}

fn header_incoming(
    data: &FunctionData,
    cfg: &CFG,
    looop: &Loop,
    parameter_count: usize,
) -> Vec<HeaderIncoming> {
    let header = looop.header();
    let mut incoming = Vec::new();
    let mut push = |source: BasicBlock, args: &[Inst]| {
        assert_eq!(
            args.len(),
            parameter_count,
            "header edge arguments must match header parameters"
        );
        incoming.push(HeaderIncoming {
            source,
            args: args.to_vec(),
        });
    };

    for edge in incoming_edges(data, cfg, header) {
        push(edge.source(), edge.args(data));
    }
    incoming
}

fn analyze_parameter(
    data: &FunctionData,
    looop: &Loop,
    loop_params: &FxHashSet<Inst>,
    incoming: &[HeaderIncoming],
    position: usize,
    parameter: Inst,
) -> Option<BasicInductionVariable> {
    if !data.inst_data(parameter).ty().is_i32() {
        return None;
    }

    let mut initial_values = SmallVec::new();
    let mut update_values = SmallVec::new();
    let mut common_step = None;

    for edge in incoming {
        let value = edge.args[position];
        if !looop.contains(edge.source) {
            initial_values.push(value);
            continue;
        }

        let step = match_update(data, parameter, value)?;
        if is_literal_zero(data, step.value())
            || !is_loop_invariant_step(data, looop, loop_params, step.value())
        {
            return None;
        }
        if let Some(existing) = common_step {
            if !same_step(data, existing, step) {
                return None;
            }
        } else {
            common_step = Some(step);
        }
        update_values.push(value);
    }

    if initial_values.is_empty() || update_values.is_empty() {
        return None;
    }
    Some(BasicInductionVariable {
        parameter,
        initial_values,
        update_values,
        step: common_step?,
    })
}

fn match_update(data: &FunctionData, parameter: Inst, update: Inst) -> Option<InductionStep> {
    let InstKind::Binary(binary) = data.inst_data(update).kind() else {
        return None;
    };
    match binary.op() {
        BinaryOp::Add if binary.lhs() == parameter => Some(InductionStep::Add(binary.rhs())),
        BinaryOp::Add if binary.rhs() == parameter => Some(InductionStep::Add(binary.lhs())),
        BinaryOp::Sub if binary.lhs() == parameter => Some(InductionStep::Sub(binary.rhs())),
        _ => None,
    }
}

fn is_loop_invariant_step(
    data: &FunctionData,
    looop: &Loop,
    loop_params: &FxHashSet<Inst>,
    step: Inst,
) -> bool {
    if step.is_global() {
        return true;
    }
    if data.inst_data(step).kind().is_const() {
        return true;
    }
    if loop_params.contains(&step) {
        return false;
    }
    match data.layout().parent_bb(step) {
        Some(block) => !looop.contains(block),
        None => matches!(data.inst_data(step).kind(), InstKind::BlockArgRef(..)),
    }
}

fn is_literal_zero(data: &FunctionData, value: Inst) -> bool {
    !value.is_global()
        && matches!(data.inst_data(value).kind(), InstKind::Integer(integer) if integer.value() == 0)
}

fn integer_constant(data: &ArenaContextMut<'_>, value: Inst) -> Option<i32> {
    match data.inst_data(value).kind() {
        InstKind::Integer(integer) => Some(integer.value()),
        _ => None,
    }
}

fn same_step(data: &FunctionData, lhs: InductionStep, rhs: InductionStep) -> bool {
    match (lhs, rhs) {
        (InductionStep::Add(lhs), InductionStep::Add(rhs))
        | (InductionStep::Sub(lhs), InductionStep::Sub(rhs)) => same_step_value(data, lhs, rhs),
        _ => false,
    }
}

fn same_step_value(data: &FunctionData, lhs: Inst, rhs: Inst) -> bool {
    if lhs == rhs {
        return true;
    }
    if lhs.is_global() || rhs.is_global() {
        return false;
    }
    matches!(
        (data.inst_data(lhs).kind(), data.inst_data(rhs).kind()),
        (InstKind::Integer(lhs), InstKind::Integer(rhs)) if lhs.value() == rhs.value()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        Program, Type,
        builder_trait::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
    };

    fn only_loop(loops: &LoopAnalysis) -> &Loop {
        assert_eq!(loops.loops().len(), 1);
        &loops.loops()[0]
    }

    #[test]
    fn computes_exact_constant_trip_counts() {
        for (initial, bound, step, direction, expected) in [
            (0, 0, 1, InductionDirection::Forward, Some(0)),
            (0, 1, 1, InductionDirection::Forward, Some(1)),
            (0, 7, 2, InductionDirection::Forward, Some(4)),
            (5, 0, -1, InductionDirection::Backward, Some(5)),
            (7, 0, -2, InductionDirection::Backward, Some(4)),
            (
                i32::MIN,
                i32::MIN + 1,
                1,
                InductionDirection::Forward,
                Some(1),
            ),
            (
                i32::MAX,
                i32::MAX - 1,
                -1,
                InductionDirection::Backward,
                Some(1),
            ),
            (0, 4, -1, InductionDirection::Forward, None),
            (4, 0, 1, InductionDirection::Backward, None),
        ] {
            assert_eq!(
                constant_trip_count_values(initial, bound, step, direction),
                expected
            );
        }
    }

    fn trip_count(
        initial_values: &[i32],
        bound: i32,
        signed_step: i32,
        direction: InductionDirection,
    ) -> Option<TripCountEstimate> {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "trip_count".into(), vec![]);
        let data = program.func_data_mut(function);
        let initial_values = initial_values
            .iter()
            .map(|&value| data.new_local_inst().integer(value))
            .collect();
        let bound = data.new_local_inst().integer(bound);
        let step = data.new_local_inst().integer(signed_step);
        let iv = BasicInductionVariable {
            parameter: bound,
            initial_values,
            update_values: SmallVec::new(),
            step: InductionStep::Add(step),
        };
        let exit = NormalizedInductionExit {
            direction,
            signed_step,
            bound,
        };
        let context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        induction_trip_count(&context, &iv, exit)
    }

    #[test]
    fn computes_exact_forward_backward_non_unit_and_zero_trip_counts() {
        assert_eq!(
            trip_count(&[0], 7, 2, InductionDirection::Forward),
            Some(TripCountEstimate::Exact(4))
        );
        assert_eq!(
            trip_count(&[7], 0, -2, InductionDirection::Backward),
            Some(TripCountEstimate::Exact(4))
        );
        assert_eq!(
            trip_count(&[7], 7, 1, InductionDirection::Forward),
            Some(TripCountEstimate::Exact(0))
        );
    }

    #[test]
    fn estimates_multiple_initial_values_conservatively() {
        let estimate = trip_count(&[0, 3, 8], 8, 2, InductionDirection::Forward).unwrap();
        assert_eq!(estimate, TripCountEstimate::UpperBound(4));
        assert_eq!(estimate.exact(), None);
        assert_eq!(estimate.upper_bound(), 4);

        let exact = trip_count(&[0, -1], 7, 2, InductionDirection::Forward).unwrap();
        assert_eq!(exact, TripCountEstimate::Exact(4));
        assert_eq!(exact.exact(), Some(4));
        assert_eq!(exact.upper_bound(), 4);
    }

    fn assert_single_iv(
        program: &Program,
        function: Function,
        parameter: Inst,
    ) -> BasicInductionVariable {
        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let analysis = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        let variables = analysis.for_loop(only_loop(&loops));
        assert_eq!(variables.len(), 1);
        assert_eq!(variables[0].parameter(), parameter);
        variables[0].clone()
    }

    fn normalized_step(
        update_op: BinaryOp,
        step_value: i32,
        compare_op: BinaryOp,
        iv_on_left: bool,
        continue_on_true: bool,
        constant_bound: Option<i32>,
    ) -> Option<(InductionDirection, i32)> {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "normalized_exit".into(),
            if constant_bound.is_some() {
                vec![]
            } else {
                vec![Type::get_i32()]
            },
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let bound = match constant_bound {
            Some(value) => data.new_local_inst().integer(value),
            None => data.params()[0],
        };
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, latch, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let step = data.new_local_inst().integer(step_value);
        let update = data.new_local_inst().binary(update_op, iv, step);
        data.layout_mut().insert_inst(header, update);
        let (lhs, rhs) = if iv_on_left { (iv, bound) } else { (bound, iv) };
        let compare = data.new_local_inst().binary(compare_op, lhs, rhs);
        data.layout_mut().insert_inst(header, compare);
        let (true_target, false_target) = if continue_on_true {
            (latch, exit)
        } else {
            (exit, latch)
        };
        let branch =
            data.new_local_inst()
                .branch(compare, true_target, vec![], false_target, vec![]);
        data.layout_mut().insert_inst(header, branch);
        let backedge = data.new_local_inst().jump(header, vec![update]);
        data.layout_mut().insert_inst(latch, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let looop = only_loop(&loops);
        let analysis = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        let iv = analysis.find(looop, iv)?;
        let context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        normalize_strict_exit(&context, looop, iv)
            .map(|exit| (exit.direction(), exit.signed_step()))
    }

    #[test]
    fn normalizes_forward_and_backward_strict_unit_exits() {
        assert_eq!(
            normalized_step(BinaryOp::Add, 1, BinaryOp::Lt, true, true, None),
            Some((InductionDirection::Forward, 1))
        );
        assert_eq!(
            normalized_step(BinaryOp::Sub, -1, BinaryOp::Ge, true, false, None),
            Some((InductionDirection::Forward, 1))
        );
        assert_eq!(
            normalized_step(BinaryOp::Sub, 1, BinaryOp::Gt, true, true, None),
            Some((InductionDirection::Backward, -1))
        );
        assert_eq!(
            normalized_step(BinaryOp::Add, -1, BinaryOp::Le, true, false, None),
            Some((InductionDirection::Backward, -1))
        );
    }

    #[test]
    fn normalizes_non_unit_steps_with_a_constant_no_wrap_bound() {
        assert_eq!(
            normalized_step(BinaryOp::Add, 2, BinaryOp::Lt, true, true, Some(100)),
            Some((InductionDirection::Forward, 2))
        );
        assert_eq!(
            normalized_step(BinaryOp::Sub, 2, BinaryOp::Gt, true, true, Some(-100)),
            Some((InductionDirection::Backward, -2))
        );
        assert_eq!(
            normalized_step(BinaryOp::Add, -3, BinaryOp::Gt, true, true, Some(-100)),
            Some((InductionDirection::Backward, -3))
        );
        assert_eq!(
            normalized_step(BinaryOp::Sub, -3, BinaryOp::Lt, true, true, Some(100)),
            Some((InductionDirection::Forward, 3))
        );
        assert_eq!(
            normalized_step(BinaryOp::Add, 2, BinaryOp::Gt, false, true, Some(100)),
            Some((InductionDirection::Forward, 2))
        );
        assert_eq!(
            normalized_step(BinaryOp::Add, 2, BinaryOp::Ge, true, false, Some(100)),
            Some((InductionDirection::Forward, 2))
        );
        assert_eq!(
            normalized_step(BinaryOp::Add, 2, BinaryOp::Le, false, false, Some(100)),
            Some((InductionDirection::Forward, 2))
        );
        assert_eq!(
            normalized_step(
                BinaryOp::Add,
                2,
                BinaryOp::Lt,
                true,
                true,
                Some(i32::MAX - 1),
            ),
            Some((InductionDirection::Forward, 2))
        );
        assert_eq!(
            normalized_step(
                BinaryOp::Sub,
                2,
                BinaryOp::Gt,
                true,
                true,
                Some(i32::MIN + 1),
            ),
            Some((InductionDirection::Backward, -2))
        );
    }

    #[test]
    fn rejects_non_unit_steps_without_a_no_wrap_proof() {
        assert_eq!(
            normalized_step(BinaryOp::Add, 2, BinaryOp::Lt, true, true, None),
            None
        );
        assert_eq!(
            normalized_step(BinaryOp::Add, 2, BinaryOp::Lt, true, true, Some(i32::MAX),),
            None
        );
        assert_eq!(
            normalized_step(BinaryOp::Sub, 2, BinaryOp::Gt, true, true, Some(i32::MIN),),
            None
        );
        assert_eq!(
            normalized_step(BinaryOp::Sub, i32::MIN, BinaryOp::Lt, true, true, Some(0),),
            None
        );
    }

    #[test]
    fn rejects_non_strict_mismatched_and_non_unit_exits() {
        assert_eq!(
            normalized_step(BinaryOp::Add, 1, BinaryOp::Le, true, true, None),
            None
        );
        assert_eq!(
            normalized_step(BinaryOp::Add, 1, BinaryOp::Gt, true, true, None),
            None
        );
    }

    #[test]
    fn normalizes_an_exit_with_a_global_unit_step() {
        let mut program = Program::new();
        let step = program.new_value().integer(1);
        let function = program.new_function(
            Type::get_unit(),
            "global_step_exit".into(),
            vec![Type::get_i32()],
        );
        let (header, iv) = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(function),
            };
            let entry = data.add_entry_block();
            let bound = data.params()[0];
            let header = data
                .new_basic_block()
                .basic_block("header".into(), vec![Type::get_i32()]);
            let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
            let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
            for block in [header, latch, exit] {
                data.layout_mut().push_bb_back(block);
            }

            let zero = data.new_local_value().integer(0);
            let entry_jump = data.new_local_value().jump(header, vec![zero]);
            data.layout_mut().insert_inst(entry, entry_jump);
            let iv = data.bb_data(header).params()[0];
            let update = data.new_local_value().binary(BinaryOp::Add, iv, step);
            let compare = data.new_local_value().binary(BinaryOp::Lt, iv, bound);
            for inst in [update, compare] {
                data.layout_mut().insert_inst(header, inst);
            }
            let branch = data
                .new_local_value()
                .branch(compare, latch, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, branch);
            let backedge = data.new_local_value().jump(header, vec![update]);
            data.layout_mut().insert_inst(latch, backedge);
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(exit, ret);
            (header, iv)
        };

        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let looop = only_loop(&loops);
        assert_eq!(looop.header(), header);
        let analysis = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        let iv = analysis.find(looop, iv).unwrap();
        let context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        assert_eq!(
            normalize_strict_exit(&context, looop, iv)
                .map(|exit| (exit.direction(), exit.signed_step())),
            Some((InductionDirection::Forward, 1))
        );
    }

    #[test]
    fn recognizes_add_sub_and_commuted_add() {
        for (op, parameter_on_left, subtract) in [
            (BinaryOp::Add, true, false),
            (BinaryOp::Add, false, false),
            (BinaryOp::Sub, true, true),
        ] {
            let mut program = Program::new();
            let function = program.new_function(Type::get_unit(), "iv".into(), vec![]);
            let data = program.func_data_mut(function);
            let entry = data.add_entry_block();
            let header = data
                .new_basic_block()
                .basic_block("header".into(), vec![Type::get_i32()]);
            let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
            let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
            for block in [header, latch, exit] {
                data.layout_mut().push_bb_back(block);
            }

            let zero = data.new_local_inst().integer(0);
            let entry_jump = data.new_local_inst().jump(header, vec![zero]);
            data.layout_mut().insert_inst(entry, entry_jump);
            let parameter = data.bb_data(header).params()[0];
            let one = data.new_local_inst().integer(1);
            let (lhs, rhs) = if parameter_on_left {
                (parameter, one)
            } else {
                (one, parameter)
            };
            let update = data.new_local_inst().binary(op, lhs, rhs);
            data.layout_mut().insert_inst(header, update);
            let condition = data.new_local_inst().integer(1);
            let branch = data
                .new_local_inst()
                .branch(condition, latch, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, branch);
            let backedge = data.new_local_inst().jump(header, vec![update]);
            data.layout_mut().insert_inst(latch, backedge);
            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(exit, ret);

            let variable = assert_single_iv(&program, function, parameter);
            assert_eq!(variable.initial_values(), &[zero]);
            assert_eq!(variable.update_values(), &[update]);
            assert_eq!(
                variable.step(),
                if subtract {
                    InductionStep::Sub(one)
                } else {
                    InductionStep::Add(one)
                }
            );
        }
    }

    #[test]
    fn accepts_symbolic_outer_value_as_inner_loop_step() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "nested_iv".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let outer_header = data
            .new_basic_block()
            .basic_block("outer_header".into(), vec![Type::get_i32()]);
        let inner_header = data
            .new_basic_block()
            .basic_block("inner_header".into(), vec![Type::get_i32()]);
        let inner_latch = data
            .new_basic_block()
            .basic_block("inner_latch".into(), vec![]);
        let outer_latch = data
            .new_basic_block()
            .basic_block("outer_latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [outer_header, inner_header, inner_latch, outer_latch, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(outer_header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let outer = data.bb_data(outer_header).params()[0];
        let enter_inner = data.new_local_inst().jump(inner_header, vec![zero]);
        data.layout_mut().insert_inst(outer_header, enter_inner);
        let inner = data.bb_data(inner_header).params()[0];
        let inner_update = data.new_local_inst().binary(BinaryOp::Add, inner, outer);
        data.layout_mut().insert_inst(inner_header, inner_update);
        let condition = data.new_local_inst().integer(1);
        let inner_branch =
            data.new_local_inst()
                .branch(condition, inner_latch, vec![], outer_latch, vec![]);
        data.layout_mut().insert_inst(inner_header, inner_branch);
        let inner_backedge = data.new_local_inst().jump(inner_header, vec![inner_update]);
        data.layout_mut().insert_inst(inner_latch, inner_backedge);
        let one = data.new_local_inst().integer(1);
        let outer_update = data.new_local_inst().binary(BinaryOp::Add, outer, one);
        data.layout_mut().insert_inst(outer_latch, outer_update);
        let outer_branch =
            data.new_local_inst()
                .branch(condition, outer_header, vec![outer_update], exit, vec![]);
        data.layout_mut().insert_inst(outer_latch, outer_branch);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        assert_eq!(loops.loops().len(), 2);
        let analysis = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        let inner_loop = loops
            .loops()
            .iter()
            .find(|looop| looop.header() == inner_header)
            .unwrap();
        let variable = analysis.find(inner_loop, inner).unwrap();
        assert_eq!(variable.step(), InductionStep::Add(outer));
    }

    #[test]
    fn preserves_multiple_entry_and_same_target_backedge_arms() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "parallel_iv".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        for block in [header, latch] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let five = data.new_local_inst().integer(5);
        let condition = data.new_local_inst().integer(1);
        let entry_branch =
            data.new_local_inst()
                .branch(condition, header, vec![zero], header, vec![five]);
        data.layout_mut().insert_inst(entry, entry_branch);
        let parameter = data.bb_data(header).params()[0];
        let one_a = data.new_local_inst().integer(1);
        let one_b = data.new_local_inst().integer(1);
        let update_a = data
            .new_local_inst()
            .binary(BinaryOp::Add, parameter, one_a);
        let update_b = data
            .new_local_inst()
            .binary(BinaryOp::Add, parameter, one_b);
        for update in [update_a, update_b] {
            data.layout_mut().insert_inst(header, update);
        }
        let to_latch = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, to_latch);
        let backedge =
            data.new_local_inst()
                .branch(condition, header, vec![update_a], header, vec![update_b]);
        data.layout_mut().insert_inst(latch, backedge);

        let variable = assert_single_iv(&program, function, parameter);
        assert_eq!(
            variable
                .initial_values()
                .iter()
                .copied()
                .collect::<FxHashSet<_>>(),
            FxHashSet::from_iter([zero, five])
        );
        assert_eq!(
            variable
                .update_values()
                .iter()
                .copied()
                .collect::<FxHashSet<_>>(),
            FxHashSet::from_iter([update_a, update_b])
        );
    }

    #[test]
    fn accepts_consistent_multiple_latches() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "multi_latch_iv".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let left_latch = data
            .new_basic_block()
            .basic_block("left_latch".into(), vec![]);
        let right_latch = data
            .new_basic_block()
            .basic_block("right_latch".into(), vec![]);
        for block in [header, left_latch, right_latch] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let parameter = data.bb_data(header).params()[0];
        let condition = data.new_local_inst().integer(1);
        let choose_latch =
            data.new_local_inst()
                .branch(condition, left_latch, vec![], right_latch, vec![]);
        data.layout_mut().insert_inst(header, choose_latch);

        let one_a = data.new_local_inst().integer(1);
        let left_update = data
            .new_local_inst()
            .binary(BinaryOp::Add, parameter, one_a);
        data.layout_mut().insert_inst(left_latch, left_update);
        let left_backedge = data.new_local_inst().jump(header, vec![left_update]);
        data.layout_mut().insert_inst(left_latch, left_backedge);

        let one_b = data.new_local_inst().integer(1);
        let right_update = data
            .new_local_inst()
            .binary(BinaryOp::Add, one_b, parameter);
        data.layout_mut().insert_inst(right_latch, right_update);
        let right_backedge = data.new_local_inst().jump(header, vec![right_update]);
        data.layout_mut().insert_inst(right_latch, right_backedge);

        let variable = assert_single_iv(&program, function, parameter);
        assert_eq!(variable.update_values().len(), 2);
        assert_eq!(variable.step(), InductionStep::Add(one_a));
    }

    #[test]
    fn rejects_passthrough_zero_variant_and_non_affine_updates() {
        enum UpdateKind {
            Passthrough,
            AddZero,
            ReverseSub,
            Mul,
            VariantStep,
        }

        for update_kind in [
            UpdateKind::Passthrough,
            UpdateKind::AddZero,
            UpdateKind::ReverseSub,
            UpdateKind::Mul,
            UpdateKind::VariantStep,
        ] {
            let mut program = Program::new();
            let function = program.new_function(Type::get_unit(), "not_iv".into(), vec![]);
            let data = program.func_data_mut(function);
            let entry = data.add_entry_block();
            let header = data
                .new_basic_block()
                .basic_block("header".into(), vec![Type::get_i32()]);
            let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
            for block in [header, latch] {
                data.layout_mut().push_bb_back(block);
            }
            let zero = data.new_local_inst().integer(0);
            let entry_jump = data.new_local_inst().jump(header, vec![zero]);
            data.layout_mut().insert_inst(entry, entry_jump);
            let parameter = data.bb_data(header).params()[0];
            let one = data.new_local_inst().integer(1);
            let update = match update_kind {
                UpdateKind::Passthrough => parameter,
                UpdateKind::AddZero => data.new_local_inst().binary(BinaryOp::Add, parameter, zero),
                UpdateKind::ReverseSub => {
                    data.new_local_inst().binary(BinaryOp::Sub, one, parameter)
                }
                UpdateKind::Mul => data.new_local_inst().binary(BinaryOp::Mul, parameter, one),
                UpdateKind::VariantStep => {
                    let step = data.new_local_inst().binary(BinaryOp::Add, one, one);
                    data.layout_mut().insert_inst(header, step);
                    data.new_local_inst().binary(BinaryOp::Add, parameter, step)
                }
            };
            if update != parameter {
                data.layout_mut().insert_inst(header, update);
            }
            let to_latch = data.new_local_inst().jump(latch, vec![]);
            data.layout_mut().insert_inst(header, to_latch);
            let backedge = data.new_local_inst().jump(header, vec![update]);
            data.layout_mut().insert_inst(latch, backedge);

            let data = program.func_data(function);
            let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
            let analysis = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
            assert!(analysis.for_loop(only_loop(&loops)).is_empty());
        }
    }

    #[test]
    fn rejects_inconsistent_same_target_backedge_arms() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "mixed_iv".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        for block in [header, latch] {
            data.layout_mut().push_bb_back(block);
        }
        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let parameter = data.bb_data(header).params()[0];
        let one = data.new_local_inst().integer(1);
        let update = data.new_local_inst().binary(BinaryOp::Add, parameter, one);
        data.layout_mut().insert_inst(header, update);
        let to_latch = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, to_latch);
        let condition = data.new_local_inst().integer(1);
        let backedge =
            data.new_local_inst()
                .branch(condition, header, vec![update], header, vec![parameter]);
        data.layout_mut().insert_inst(latch, backedge);

        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let analysis = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        assert!(analysis.for_loop(only_loop(&loops)).is_empty());
    }

    #[test]
    fn rejects_float_recurrence() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "float_iv".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_f32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        for block in [header, latch] {
            data.layout_mut().push_bb_back(block);
        }
        let zero = data.new_local_inst().float(0.0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let parameter = data.bb_data(header).params()[0];
        let one = data.new_local_inst().float(1.0);
        let update = data.new_local_inst().binary(BinaryOp::Add, parameter, one);
        data.layout_mut().insert_inst(header, update);
        let to_latch = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, to_latch);
        let backedge = data.new_local_inst().jump(header, vec![update]);
        data.layout_mut().insert_inst(latch, backedge);

        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let analysis = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        assert!(analysis.for_loop(only_loop(&loops)).is_empty());
    }

    #[test]
    fn rejects_entry_header_without_an_outside_initial_value() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "entry_header".into(),
            vec![Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let header = data.add_entry_block();
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        data.layout_mut().push_bb_back(exit);

        let parameter = data.params()[0];
        let one = data.new_local_inst().integer(1);
        let update = data.new_local_inst().binary(BinaryOp::Add, parameter, one);
        data.layout_mut().insert_inst(header, update);
        let condition = data.new_local_inst().integer(1);
        let backedge = data
            .new_local_inst()
            .branch(condition, header, vec![update], exit, vec![]);
        data.layout_mut().insert_inst(header, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let analysis = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        assert!(analysis.for_loop(only_loop(&loops)).is_empty());
    }
}
