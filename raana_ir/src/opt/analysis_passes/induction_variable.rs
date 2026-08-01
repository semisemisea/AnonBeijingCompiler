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
