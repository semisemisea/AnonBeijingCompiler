use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::opt::{
    analysis_passes::{
        dom_tree::v2::DominanceTree,
        induction_variable::{
            BasicInductionVariableAnalysis, DerivedInductionVariable,
            classify_derived_induction_variable, constant_induction_range, normalize_strict_exit,
        },
        loop_analysis::{Loop, LoopAnalysis},
    },
    prelude::*,
    utils::{
        cfg::CFG,
        gep::{gep_constant_offset_with_replacement_fits, gep_index_stride},
        logical_edge::{LogicalEdge, LogicalEdgeRewriter, incoming_edges, outgoing_edges},
        pointer_strength_reduction_cost::estimate_aarch64_pointer_strength_reduction,
        preheader::{EnsurePreheader, ensure_preheader},
    },
};

pub struct PointerStrengthReduction;

struct Candidate {
    gep: Inst,
    header_iv_position: usize,
    gep_iv_position: usize,
    backedge_groups: SmallVec<[BackedgeGroup; 2]>,
    pointer_step: i32,
    base: Inst,
    offsets: Vec<Inst>,
    pointer_ty: Type,
    derived_iv: Option<DerivedInductionVariable>,
}

struct BackedgeGroup {
    source: BasicBlock,
    edges: SmallVec<[LogicalEdge; 2]>,
}

enum ApplyResult {
    Unchanged,
    Changed,
}

impl PointerStrengthReduction {
    fn find_candidate(
        data: &ArenaContextMut<'_>,
        cfg: &CFG,
        dom_tree: &DominanceTree,
        loops: &LoopAnalysis,
        ivs: &BasicInductionVariableAnalysis,
        looop: &Loop,
        parameter_blocks: &FxHashMap<Inst, BasicBlock>,
    ) -> Option<Candidate> {
        let backedges = incoming_edges(data, cfg, looop.header())
            .into_iter()
            .filter(|edge| looop.contains(edge.source()))
            .collect::<Vec<_>>();
        if backedges.is_empty() {
            return None;
        }
        let mut backedge_groups = SmallVec::<[BackedgeGroup; 2]>::new();
        for &edge in &backedges {
            if let Some(group) = backedge_groups
                .iter_mut()
                .find(|group| group.source == edge.source())
            {
                group.edges.push(edge);
            } else {
                backedge_groups.push(BackedgeGroup {
                    source: edge.source(),
                    edges: SmallVec::from_slice(&[edge]),
                });
            }
        }

        for iv in ivs.for_loop(looop) {
            let Some(exit) = normalize_strict_exit(data, looop, iv) else {
                continue;
            };
            let signed_step = exit.signed_step();
            let constant_range = constant_induction_range(data, iv, exit);
            let Some(header_iv_position) = data
                .bb_data(looop.header())
                .params()
                .iter()
                .position(|&parameter| parameter == iv.parameter())
            else {
                continue;
            };
            if backedges.iter().any(|edge| {
                edge.args(data)
                    .get(header_iv_position)
                    .is_none_or(|value| !iv.update_values().contains(value))
            }) {
                continue;
            }

            for block_layout in data.layout().basicblocks() {
                let block = block_layout.bb();
                if !looop.contains(block)
                    || loops.min_loop_contain(block).map(Loop::header) != Some(looop.header())
                {
                    continue;
                }
                for &inst in block_layout.insts() {
                    let InstKind::GetElemPtr(gep) = data.inst_data(inst).kind() else {
                        continue;
                    };
                    let mut iv_offsets =
                        gep.offsets()
                            .iter()
                            .enumerate()
                            .filter_map(|(position, &offset)| {
                                if offset == iv.parameter() {
                                    return Some((position, None));
                                }
                                constant_range.and_then(|range| {
                                    classify_derived_induction_variable(
                                        data,
                                        looop,
                                        iv.parameter(),
                                        offset,
                                        range,
                                    )
                                    .map(|derived| (position, Some(derived)))
                                })
                            });
                    let Some((gep_iv_position, derived_iv)) = iv_offsets.next() else {
                        continue;
                    };
                    if iv_offsets.next().is_some() {
                        continue;
                    }
                    if !Self::available_at_header(
                        data,
                        dom_tree,
                        looop,
                        parameter_blocks,
                        gep.base(),
                    ) || !gep
                        .offsets()
                        .iter()
                        .enumerate()
                        .filter(|(position, _)| *position != gep_iv_position)
                        .all(|(_, &offset)| {
                            Self::available_at_header(
                                data,
                                dom_tree,
                                looop,
                                parameter_blocks,
                                offset,
                            )
                        })
                        || !Self::has_only_loop_memory_users(data, looop, inst)
                        || !backedge_groups
                            .iter()
                            .all(|group| dom_tree.dominates(block, group.source))
                    {
                        continue;
                    }
                    let Some(stride) = gep_index_stride(data, inst, gep_iv_position) else {
                        continue;
                    };
                    let coefficient = derived_iv
                        .as_ref()
                        .map(DerivedInductionVariable::coefficient)
                        .unwrap_or(1);
                    let Some(index_delta) = i64::from(signed_step).checked_mul(coefficient) else {
                        continue;
                    };
                    let Some(signed_pointer_step) = index_delta
                        .checked_mul(i64::from(stride.result_element_stride))
                        .and_then(|step| i32::try_from(step).ok())
                    else {
                        continue;
                    };
                    let Some(signed_byte_delta) = index_delta.checked_mul(stride.byte_stride)
                    else {
                        continue;
                    };
                    let removable_derived_insts = derived_iv
                        .as_ref()
                        .map(|derived| derived.removable_chain_cost(data, inst))
                        .unwrap_or(0);
                    let derived_setup_insts = derived_iv
                        .as_ref()
                        .map(|derived| {
                            usize::from(derived.coefficient() != 1)
                                + usize::from(derived.offset() != 0)
                        })
                        .unwrap_or(0);
                    let Some(cost) = estimate_aarch64_pointer_strength_reduction(
                        data,
                        cfg,
                        looop,
                        gep,
                        signed_byte_delta,
                        removable_derived_insts,
                        derived_setup_insts,
                    ) else {
                        continue;
                    };
                    if !cost.is_profitable() {
                        continue;
                    }
                    return Some(Candidate {
                        gep: inst,
                        header_iv_position,
                        gep_iv_position,
                        backedge_groups,
                        pointer_step: signed_pointer_step,
                        base: gep.base(),
                        offsets: gep.offsets().to_vec(),
                        pointer_ty: data.inst_data(inst).ty().clone(),
                        derived_iv,
                    });
                }
            }
        }
        None
    }

    fn available_at_header(
        data: &ArenaContextMut<'_>,
        dom_tree: &DominanceTree,
        looop: &Loop,
        parameter_blocks: &FxHashMap<Inst, BasicBlock>,
        value: Inst,
    ) -> bool {
        if value.is_global() || data.inst_data(value).kind().is_const() {
            return true;
        }
        match data.layout().parent_bb(value) {
            Some(block) => !looop.contains(block) && dom_tree.dominates(block, looop.header()),
            None if matches!(data.inst_data(value).kind(), InstKind::BlockArgRef(..)) => {
                parameter_blocks.get(&value).is_some_and(|&block| {
                    !looop.contains(block)
                        && dom_tree.contains(block)
                        && dom_tree.dominates(block, looop.header())
                })
            }
            None => false,
        }
    }

    fn has_only_loop_memory_users(data: &ArenaContextMut<'_>, looop: &Loop, gep: Inst) -> bool {
        let users = data.inst_data(gep).used_by();
        !users.is_empty()
            && users.iter().all(|&user| {
                data.layout()
                    .parent_bb(user)
                    .is_some_and(|block| looop.contains(block))
                    && match data.inst_data(user).kind() {
                        InstKind::Load(load) => load.src() == gep,
                        InstKind::Store(store) => store.dest() == gep,
                        _ => false,
                    }
            })
    }

    fn apply_candidate(
        data: &mut ArenaContextMut<'_>,
        cfg: &CFG,
        looop: &Loop,
        candidate: Candidate,
    ) -> ApplyResult {
        let preheader = match ensure_preheader(data, cfg, looop) {
            Some(EnsurePreheader::Existing(preheader) | EnsurePreheader::Created(preheader)) => {
                preheader
            }
            None => return ApplyResult::Unchanged,
        };

        let entry_edges = outgoing_edges(data, preheader)
            .into_iter()
            .filter(|edge| edge.target(data) == looop.header())
            .collect::<Vec<_>>();
        if entry_edges.is_empty()
            || entry_edges
                .iter()
                .any(|edge| edge.args(data).len() != data.bb_data(looop.header()).params().len())
        {
            return ApplyResult::Unchanged;
        }

        let mut initial_indices_by_edge = Vec::with_capacity(entry_edges.len());
        for edge in entry_edges {
            let initial_iv = edge.args(data)[candidate.header_iv_position];
            let derived_initial_index = match &candidate.derived_iv {
                Some(derived) => match data.inst_data(initial_iv).kind() {
                    InstKind::Integer(initial) => {
                        let Some(initial_index) = derived.evaluate(initial.value()) else {
                            return ApplyResult::Unchanged;
                        };
                        Some(initial_index)
                    }
                    _ => None,
                },
                None => {
                    if let InstKind::Integer(initial) = data.inst_data(initial_iv).kind() {
                        if !gep_constant_offset_with_replacement_fits(
                            data,
                            candidate.base,
                            &candidate.offsets,
                            candidate.gep_iv_position,
                            initial.value(),
                        ) {
                            return ApplyResult::Unchanged;
                        }
                    }
                    initial_indices_by_edge.push((edge, None, initial_iv));
                    continue;
                }
            };
            if let Some(initial_index) = derived_initial_index {
                if !gep_constant_offset_with_replacement_fits(
                    data,
                    candidate.base,
                    &candidate.offsets,
                    candidate.gep_iv_position,
                    initial_index,
                ) {
                    return ApplyResult::Unchanged;
                }
            }
            initial_indices_by_edge.push((edge, derived_initial_index, initial_iv));
        }

        let mut rewrites = LogicalEdgeRewriter::new();
        for (edge, derived_initial_index, initial_iv) in initial_indices_by_edge {
            let mut initial_offsets = candidate.offsets.clone();
            initial_offsets[candidate.gep_iv_position] =
                match (&candidate.derived_iv, derived_initial_index) {
                    (_, Some(initial_index)) => data.new_local_value().integer(initial_index),
                    (Some(derived), None) => {
                        let mut value = initial_iv;
                        if derived.coefficient() != 1 {
                            let coefficient = data
                                .new_local_value()
                                .integer(i32::try_from(derived.coefficient()).unwrap());
                            value =
                                data.new_local_value()
                                    .binary(BinaryOp::Mul, value, coefficient);
                            data.layout_mut().insert_before_terminator(preheader, value);
                        }
                        if derived.offset() != 0 {
                            let offset = data
                                .new_local_value()
                                .integer(i32::try_from(derived.offset()).unwrap());
                            value = data.new_local_value().binary(BinaryOp::Add, value, offset);
                            data.layout_mut().insert_before_terminator(preheader, value);
                        }
                        value
                    }
                    (None, None) => initial_iv,
                };
            let initial_pointer = data
                .new_local_value()
                .get_elem_ptr(candidate.base, initial_offsets);
            debug_assert_eq!(data.inst_data(initial_pointer).ty(), &candidate.pointer_ty);
            data.layout_mut()
                .insert_before_terminator(preheader, initial_pointer);
            rewrites.append_arg(data, edge, initial_pointer);
        }

        let pointer = data
            .new_basic_block()
            .add_param(looop.header(), candidate.pointer_ty.clone());
        let pointer_step = data.new_local_value().integer(candidate.pointer_step);
        for group in candidate.backedge_groups {
            let next_pointer = data
                .new_local_value()
                .get_elem_ptr(pointer, vec![pointer_step]);
            debug_assert_eq!(data.inst_data(next_pointer).ty(), &candidate.pointer_ty);
            data.layout_mut()
                .insert_before_terminator(group.source, next_pointer);
            for edge in group.edges {
                rewrites.append_arg(data, edge, next_pointer);
            }
        }
        assert!(rewrites.apply(data));

        let zero = data.new_local_value().integer(0);
        data.replace_inst_with(candidate.gep)
            .get_elem_ptr(pointer, vec![zero]);
        debug_assert_eq!(data.inst_data(candidate.gep).ty(), &candidate.pointer_ty);
        ApplyResult::Changed
    }
}

impl Pass for PointerStrengthReduction {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().is_decl() {
            return false;
        }
        let mut changed = false;
        loop {
            let Some(cfg) = CFG::new(data) else {
                return changed;
            };
            if cfg.is_acyclic() {
                return changed;
            }
            let parameter_blocks = cfg
                .blocks()
                .iter()
                .flat_map(|&block| {
                    data.bb_data(block)
                        .params()
                        .iter()
                        .copied()
                        .map(move |parameter| (parameter, block))
                })
                .collect::<FxHashMap<_, _>>();
            let (cfg, dom_tree, loops) = LoopAnalysis::from_cfg(cfg);
            let ivs = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
            let mut transformed = false;
            for looop in loops.loops() {
                let Some(candidate) = Self::find_candidate(
                    data,
                    &cfg,
                    &dom_tree,
                    &loops,
                    &ivs,
                    looop,
                    &parameter_blocks,
                ) else {
                    continue;
                };
                match Self::apply_candidate(data, &cfg, looop, candidate) {
                    ApplyResult::Unchanged => continue,
                    ApplyResult::Changed => {
                        changed = true;
                        transformed = true;
                        break;
                    }
                }
            }
            if !transformed {
                return changed;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Program, builder_trait::*};

    struct LoopFixture {
        function: Function,
        entry: BasicBlock,
        header: BasicBlock,
        latch: BasicBlock,
        gep: Inst,
        load: Inst,
        iv: Inst,
        next_iv: Inst,
        entry_jump: Inst,
        backedge: Inst,
    }

    fn build_loop_with_update(
        update_op: BinaryOp,
        step_value: i32,
        compare_op: BinaryOp,
        iv_last: bool,
    ) -> (Program, LoopFixture) {
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr".into(),
            vec![
                Type::get_pointer(array_ty),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let bound = data.params()[1];
        let outer_index = data.params()[2];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, latch, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let initial = data.new_local_inst().integer(3);
        let entry_jump = data.new_local_inst().jump(header, vec![initial]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let iv = data.bb_data(header).params()[0];
        let offsets = if iv_last {
            vec![outer_index, iv]
        } else {
            vec![iv, outer_index]
        };
        let gep = data.new_local_inst().get_elem_ptr(base, offsets);
        let load = data.new_local_inst().load(gep);
        let step = data.new_local_inst().integer(step_value);
        let next_iv = data.new_local_inst().binary(update_op, iv, step);
        for inst in [gep, load, next_iv] {
            data.layout_mut().insert_inst(header, inst);
        }
        let condition = data.new_local_inst().binary(compare_op, iv, bound);
        data.layout_mut().insert_inst(header, condition);
        let branch = data
            .new_local_inst()
            .branch(condition, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);

        let backedge = data.new_local_inst().jump(header, vec![next_iv]);
        data.layout_mut().insert_inst(latch, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        (
            program,
            LoopFixture {
                function,
                entry,
                header,
                latch,
                gep,
                load,
                iv,
                next_iv,
                entry_jump,
                backedge,
            },
        )
    }

    fn build_loop(compare_op: BinaryOp, iv_last: bool) -> (Program, LoopFixture) {
        build_loop_with_update(BinaryOp::Add, 1, compare_op, iv_last)
    }

    fn replace_bound_with_constant(program: &mut Program, fixture: &LoopFixture, bound_value: i32) {
        let data = program.func_data_mut(fixture.function);
        let terminator = data.layout().basicblock(fixture.header).terminator();
        let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
            panic!("header terminator must be a branch");
        };
        let condition = branch.cond();
        let InstKind::Binary(compare) = data.inst_data(condition).kind() else {
            panic!("header condition must be a comparison");
        };
        let (op, lhs, rhs) = (compare.op(), compare.lhs(), compare.rhs());
        let bound = data.new_local_value().integer(bound_value);
        let (lhs, rhs) = if lhs == fixture.iv {
            (lhs, bound)
        } else if rhs == fixture.iv {
            (bound, rhs)
        } else {
            panic!("header comparison must use the induction variable");
        };
        data.replace_inst_with(condition).binary(op, lhs, rhs);
    }

    fn replace_gep_index_with_affine(
        program: &mut Program,
        fixture: &LoopFixture,
        coefficient: i32,
        offset: i32,
        subtract_product: bool,
    ) -> Inst {
        let data = program.func_data_mut(fixture.function);
        let coefficient = data.new_local_value().integer(coefficient);
        let product = data
            .new_local_value()
            .binary(BinaryOp::Mul, fixture.iv, coefficient);
        let offset = data.new_local_value().integer(offset);
        let derived = if subtract_product {
            data.new_local_value()
                .binary(BinaryOp::Sub, offset, product)
        } else {
            data.new_local_value()
                .binary(BinaryOp::Add, product, offset)
        };
        data.layout_mut().insert_inst_before(fixture.gep, product);
        data.layout_mut().insert_inst_before(fixture.gep, derived);

        let InstKind::GetElemPtr(gep) = data.inst_data(fixture.gep).kind() else {
            panic!("fixture address must be a GEP");
        };
        let mut offsets = gep.offsets().to_vec();
        let position = offsets
            .iter()
            .position(|&value| value == fixture.iv)
            .expect("fixture GEP must use the induction variable");
        let base = gep.base();
        offsets[position] = derived;
        data.replace_inst_with(fixture.gep)
            .get_elem_ptr(base, offsets);
        derived
    }

    fn replace_gep_index_with_shift(
        program: &mut Program,
        fixture: &LoopFixture,
        shift: i32,
        offset: i32,
    ) {
        let data = program.func_data_mut(fixture.function);
        let shift = data.new_local_value().integer(shift);
        let shifted = data
            .new_local_value()
            .binary(BinaryOp::Shl, fixture.iv, shift);
        let offset = data.new_local_value().integer(offset);
        let derived = data
            .new_local_value()
            .binary(BinaryOp::Add, shifted, offset);
        data.layout_mut().insert_inst_before(fixture.gep, shifted);
        data.layout_mut().insert_inst_before(fixture.gep, derived);

        let InstKind::GetElemPtr(gep) = data.inst_data(fixture.gep).kind() else {
            panic!("fixture address must be a GEP");
        };
        let mut offsets = gep.offsets().to_vec();
        let position = offsets
            .iter()
            .position(|&value| value == fixture.iv)
            .expect("fixture GEP must use the induction variable");
        let base = gep.base();
        offsets[position] = derived;
        data.replace_inst_with(fixture.gep)
            .get_elem_ptr(base, offsets);
    }

    fn run(program: &mut Program, function: Function) -> bool {
        let mut context = ArenaContextMut {
            program,
            curr_func: Some(function),
        };
        PointerStrengthReduction.run_on(&mut context)
    }

    #[test]
    fn skips_an_acyclic_function() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "acyclic".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        data.layout_mut().push_bb_back(exit);
        let jump = data.new_local_inst().jump(exit, vec![]);
        data.layout_mut().insert_inst(entry, jump);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(!run(&mut program, function));
    }

    fn integer_constant(data: &FunctionData, inst: Inst) -> Option<i32> {
        match data.inst_data(inst).kind() {
            InstKind::Integer(integer) => Some(integer.value()),
            _ => None,
        }
    }

    #[test]
    fn carries_a_pointer_for_a_forward_unit_step_loop() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        assert!(run(&mut program, fixture.function));

        let data = program.func_data(fixture.function);
        let params = data.bb_data(fixture.header).params();
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], fixture.iv);
        let pointer = params[1];
        assert_eq!(
            data.inst_data(pointer).ty(),
            data.inst_data(fixture.gep).ty()
        );

        let InstKind::GetElemPtr(rewritten) = data.inst_data(fixture.gep).kind() else {
            panic!("the original address must remain a GEP");
        };
        assert_eq!(rewritten.base(), pointer);
        assert_eq!(rewritten.offsets().len(), 1);
        assert_eq!(integer_constant(data, rewritten.offsets()[0]), Some(0));
        assert_eq!(
            data.inst_data(fixture.load)
                .inst_usage()
                .collect::<Vec<_>>(),
            vec![fixture.gep]
        );

        let InstKind::Jump(entry_jump) = data.inst_data(fixture.entry_jump).kind() else {
            panic!("entry terminator must remain a jump");
        };
        assert_eq!(entry_jump.args().len(), 2);
        let initial_pointer = entry_jump.args()[1];
        assert_eq!(
            data.layout().parent_bb(initial_pointer),
            Some(fixture.entry)
        );
        let InstKind::GetElemPtr(initial) = data.inst_data(initial_pointer).kind() else {
            panic!("entry must compute the initial pointer");
        };
        assert_eq!(
            initial.offsets().last().copied(),
            Some(entry_jump.args()[0])
        );

        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        assert_eq!(backedge.args().len(), 2);
        assert_eq!(backedge.args()[0], fixture.next_iv);
        let next_pointer = backedge.args()[1];
        assert_eq!(data.layout().parent_bb(next_pointer), Some(fixture.latch));
        let InstKind::GetElemPtr(next) = data.inst_data(next_pointer).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(next.base(), pointer);
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(1));

        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let ivs = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        let looop = loops
            .loops()
            .iter()
            .find(|looop| looop.header() == fixture.header)
            .unwrap();
        assert!(ivs.find(looop, fixture.iv).is_some());
        assert!(!run(&mut program, fixture.function));
    }

    #[test]
    fn carries_a_pointer_for_a_backward_unit_step_loop() {
        let (mut program, fixture) = build_loop_with_update(BinaryOp::Sub, 1, BinaryOp::Gt, true);
        assert!(run(&mut program, fixture.function));

        let data = program.func_data(fixture.function);
        let pointer = data.bb_data(fixture.header).params()[1];
        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let next_pointer = backedge.args()[1];
        let InstKind::GetElemPtr(next) = data.inst_data(next_pointer).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(next.base(), pointer);
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(-1));
        assert!(!run(&mut program, fixture.function));
    }

    #[test]
    fn carries_a_pointer_for_a_forward_non_unit_step_loop() {
        let (mut program, fixture) = build_loop_with_update(BinaryOp::Add, 2, BinaryOp::Lt, true);
        replace_bound_with_constant(&mut program, &fixture, 100);
        assert!(run(&mut program, fixture.function));

        let data = program.func_data(fixture.function);
        let pointer = data.bb_data(fixture.header).params()[1];
        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let InstKind::GetElemPtr(next) = data.inst_data(backedge.args()[1]).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(next.base(), pointer);
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(2));
        assert!(!run(&mut program, fixture.function));
    }

    #[test]
    fn carries_a_pointer_for_a_backward_non_unit_step_loop() {
        let (mut program, fixture) = build_loop_with_update(BinaryOp::Sub, 2, BinaryOp::Gt, true);
        replace_bound_with_constant(&mut program, &fixture, -100);
        assert!(run(&mut program, fixture.function));

        let data = program.func_data(fixture.function);
        let pointer = data.bb_data(fixture.header).params()[1];
        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let InstKind::GetElemPtr(next) = data.inst_data(backedge.args()[1]).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(next.base(), pointer);
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(-2));
        assert!(!run(&mut program, fixture.function));
    }

    #[test]
    fn scales_a_non_unit_step_by_an_intermediate_gep_stride() {
        let (mut program, fixture) = build_loop_with_update(BinaryOp::Add, 2, BinaryOp::Lt, false);
        replace_bound_with_constant(&mut program, &fixture, 100);
        assert!(run(&mut program, fixture.function));

        let data = program.func_data(fixture.function);
        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let InstKind::GetElemPtr(next) = data.inst_data(backedge.args()[1]).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(64));
    }

    #[test]
    fn carries_a_pointer_for_a_constant_affine_index() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        replace_bound_with_constant(&mut program, &fixture, 10);
        let derived = replace_gep_index_with_affine(&mut program, &fixture, 2, 1, false);
        assert!(run(&mut program, fixture.function));

        let data = program.func_data(fixture.function);
        let pointer = data.bb_data(fixture.header).params()[1];
        let InstKind::Jump(entry) = data.inst_data(fixture.entry_jump).kind() else {
            panic!("entry terminator must remain a jump");
        };
        let InstKind::GetElemPtr(initial) = data.inst_data(entry.args()[1]).kind() else {
            panic!("entry must compute the initial pointer");
        };
        assert_eq!(
            integer_constant(data, *initial.offsets().last().unwrap()),
            Some(7)
        );

        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let InstKind::GetElemPtr(next) = data.inst_data(backedge.args()[1]).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(next.base(), pointer);
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(2));
        assert!(data.inst_data(derived).used_by().is_empty());
        assert!(!run(&mut program, fixture.function));
    }

    #[test]
    fn carries_a_pointer_for_a_negative_affine_coefficient() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        replace_bound_with_constant(&mut program, &fixture, 10);
        replace_gep_index_with_affine(&mut program, &fixture, 2, 20, true);
        assert!(run(&mut program, fixture.function));

        let data = program.func_data(fixture.function);
        let InstKind::Jump(entry) = data.inst_data(fixture.entry_jump).kind() else {
            panic!("entry terminator must remain a jump");
        };
        let InstKind::GetElemPtr(initial) = data.inst_data(entry.args()[1]).kind() else {
            panic!("entry must compute the initial pointer");
        };
        assert_eq!(
            integer_constant(data, *initial.offsets().last().unwrap()),
            Some(14)
        );
        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let InstKind::GetElemPtr(next) = data.inst_data(backedge.args()[1]).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(-2));
    }

    #[test]
    fn carries_a_pointer_for_a_shift_derived_index() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        replace_bound_with_constant(&mut program, &fixture, 10);
        replace_gep_index_with_shift(&mut program, &fixture, 2, 1);
        assert!(run(&mut program, fixture.function));

        let data = program.func_data(fixture.function);
        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let InstKind::GetElemPtr(next) = data.inst_data(backedge.args()[1]).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(4));
    }

    #[test]
    fn rejects_an_affine_index_without_a_constant_range_proof() {
        let (mut dynamic_bound_program, dynamic_bound) = build_loop(BinaryOp::Lt, true);
        replace_gep_index_with_affine(&mut dynamic_bound_program, &dynamic_bound, 2, 1, false);
        assert!(!run(&mut dynamic_bound_program, dynamic_bound.function));

        let (mut wrapping_program, wrapping) = build_loop(BinaryOp::Lt, true);
        replace_bound_with_constant(&mut wrapping_program, &wrapping, 10);
        replace_gep_index_with_affine(&mut wrapping_program, &wrapping, 1 << 30, 0, false);
        assert!(!run(&mut wrapping_program, wrapping.function));
    }

    #[test]
    fn rejects_affine_wrap_on_the_final_header_visit() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        replace_bound_with_constant(&mut program, &fixture, 2);
        {
            let data = program.func_data_mut(fixture.function);
            let zero = data.new_local_value().integer(0);
            data.replace_inst_with(fixture.entry_jump)
                .jump(fixture.header, vec![zero]);
        }
        let derived = replace_gep_index_with_affine(&mut program, &fixture, i32::MAX, 0, false);

        let data = program.func_data(fixture.function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let looop = loops
            .loops()
            .iter()
            .find(|looop| looop.header() == fixture.header)
            .unwrap();
        let ivs = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        let iv = ivs.find(looop, fixture.iv).unwrap();
        let context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(fixture.function),
        };
        let exit = normalize_strict_exit(&context, looop, iv).unwrap();
        let range = constant_induction_range(&context, iv, exit).unwrap();
        assert_eq!(range.min(), 0);
        assert_eq!(range.max(), 2);
        assert!(
            classify_derived_induction_variable(&context, looop, fixture.iv, derived, range,)
                .is_none()
        );
    }

    #[test]
    fn carries_a_pointer_across_two_latches() {
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_two_latches".into(),
            vec![
                Type::get_pointer(array_ty),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let bound = data.params()[1];
        let outer_index = data.params()[2];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let dispatch = data
            .new_basic_block()
            .basic_block("dispatch".into(), vec![]);
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, dispatch, left, right, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let gep = data
            .new_local_inst()
            .get_elem_ptr(base, vec![outer_index, iv]);
        let load = data.new_local_inst().load(gep);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        for inst in [gep, load, compare] {
            data.layout_mut().insert_inst(header, inst);
        }
        let header_branch = data
            .new_local_inst()
            .branch(compare, dispatch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, header_branch);

        let choose = data.new_local_inst().integer(1);
        let dispatch_branch = data
            .new_local_inst()
            .branch(choose, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(dispatch, dispatch_branch);

        let one_left = data.new_local_inst().integer(1);
        let left_update = data.new_local_inst().binary(BinaryOp::Add, iv, one_left);
        data.layout_mut().insert_inst(left, left_update);
        let left_backedge = data.new_local_inst().jump(header, vec![left_update]);
        data.layout_mut().insert_inst(left, left_backedge);

        let one_right = data.new_local_inst().integer(1);
        let right_update = data.new_local_inst().binary(BinaryOp::Add, one_right, iv);
        data.layout_mut().insert_inst(right, right_update);
        let right_backedge = data.new_local_inst().jump(header, vec![right_update]);
        data.layout_mut().insert_inst(right, right_backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        let pointer = data.bb_data(header).params()[1];
        for (latch, terminator, update) in [
            (left, left_backedge, left_update),
            (right, right_backedge, right_update),
        ] {
            let InstKind::Jump(backedge) = data.inst_data(terminator).kind() else {
                panic!("latch must remain a jump");
            };
            assert_eq!(backedge.args()[0], update);
            let next_pointer = backedge.args()[1];
            assert_eq!(data.layout().parent_bb(next_pointer), Some(latch));
            let InstKind::GetElemPtr(next) = data.inst_data(next_pointer).kind() else {
                panic!("latch must compute the next pointer");
            };
            assert_eq!(next.base(), pointer);
            assert_eq!(integer_constant(data, next.offsets()[0]), Some(1));
        }
        assert!(!run(&mut program, function));
    }

    #[test]
    fn shares_one_pointer_update_across_parallel_backedge_arms() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        let data = program.func_data_mut(fixture.function);
        let condition = data.new_local_value().integer(1);
        let one = data.new_local_value().integer(1);
        let alternate_update = data
            .new_local_value()
            .binary(BinaryOp::Add, one, fixture.iv);
        data.layout_mut()
            .insert_inst_before(fixture.backedge, alternate_update);
        data.replace_inst_with(fixture.backedge).branch(
            condition,
            fixture.header,
            vec![fixture.next_iv],
            fixture.header,
            vec![alternate_update],
        );

        assert!(run(&mut program, fixture.function));
        let data = program.func_data(fixture.function);
        let pointer = data.bb_data(fixture.header).params()[1];
        let InstKind::Branch(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a branch");
        };
        assert_eq!(backedge.t_args().len(), 2);
        assert_eq!(backedge.f_args().len(), 2);
        let next_pointer = backedge.t_args()[1];
        assert_eq!(backedge.f_args()[1], next_pointer);
        assert_eq!(data.layout().parent_bb(next_pointer), Some(fixture.latch));
        let InstKind::GetElemPtr(next) = data.inst_data(next_pointer).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(next.base(), pointer);
        assert!(!run(&mut program, fixture.function));
    }

    #[test]
    fn rejects_inconsistent_parallel_backedge_updates() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        let data = program.func_data_mut(fixture.function);
        let condition = data.new_local_value().integer(1);
        let two = data.new_local_value().integer(2);
        let alternate_update = data
            .new_local_value()
            .binary(BinaryOp::Add, fixture.iv, two);
        data.layout_mut()
            .insert_inst_before(fixture.backedge, alternate_update);
        data.replace_inst_with(fixture.backedge).branch(
            condition,
            fixture.header,
            vec![fixture.next_iv],
            fixture.header,
            vec![alternate_update],
        );

        assert!(!run(&mut program, fixture.function));
        assert_eq!(
            program
                .func_data(fixture.function)
                .bb_data(fixture.header)
                .params()
                .len(),
            1
        );
    }

    #[test]
    fn rejects_a_path_conditional_gep_with_multiple_latches() {
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_conditional_gep".into(),
            vec![
                Type::get_pointer(array_ty),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let bound = data.params()[1];
        let outer_index = data.params()[2];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let dispatch = data
            .new_basic_block()
            .basic_block("dispatch".into(), vec![]);
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, dispatch, left, right, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        data.layout_mut().insert_inst(header, compare);
        let header_branch = data
            .new_local_inst()
            .branch(compare, dispatch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, header_branch);

        let choose = data.new_local_inst().integer(1);
        let dispatch_branch = data
            .new_local_inst()
            .branch(choose, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(dispatch, dispatch_branch);

        let gep = data
            .new_local_inst()
            .get_elem_ptr(base, vec![outer_index, iv]);
        let load = data.new_local_inst().load(gep);
        let one_left = data.new_local_inst().integer(1);
        let left_update = data.new_local_inst().binary(BinaryOp::Add, iv, one_left);
        for inst in [gep, load, left_update] {
            data.layout_mut().insert_inst(left, inst);
        }
        let left_backedge = data.new_local_inst().jump(header, vec![left_update]);
        data.layout_mut().insert_inst(left, left_backedge);

        let one_right = data.new_local_inst().integer(1);
        let right_update = data.new_local_inst().binary(BinaryOp::Add, iv, one_right);
        data.layout_mut().insert_inst(right, right_update);
        let right_backedge = data.new_local_inst().jump(header, vec![right_update]);
        data.layout_mut().insert_inst(right, right_backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(!run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.bb_data(header).params().len(), 1);
        let InstKind::GetElemPtr(gep) = data.inst_data(gep).kind() else {
            unreachable!()
        };
        assert_eq!(gep.offsets().last().copied(), Some(iv));
    }

    #[test]
    fn carries_a_pointer_through_a_header_self_loop() {
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_self_loop".into(),
            vec![
                Type::get_pointer(array_ty),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let bound = data.params()[1];
        let outer_index = data.params()[2];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let gep = data
            .new_local_inst()
            .get_elem_ptr(base, vec![outer_index, iv]);
        let load = data.new_local_inst().load(gep);
        let one = data.new_local_inst().integer(1);
        let update = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        for inst in [gep, load, update, compare] {
            data.layout_mut().insert_inst(header, inst);
        }
        let backedge = data
            .new_local_inst()
            .branch(compare, header, vec![update], exit, vec![]);
        data.layout_mut().insert_inst(header, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        let pointer = data.bb_data(header).params()[1];
        let InstKind::Branch(backedge) = data.inst_data(backedge).kind() else {
            panic!("header terminator must remain a branch");
        };
        assert_eq!(backedge.t_args().len(), 2);
        assert!(backedge.f_args().is_empty());
        let next_pointer = backedge.t_args()[1];
        assert_eq!(data.layout().parent_bb(next_pointer), Some(header));
        let InstKind::GetElemPtr(next) = data.inst_data(next_pointer).kind() else {
            panic!("header must compute the next pointer");
        };
        assert_eq!(next.base(), pointer);
        assert!(!run(&mut program, function));
    }

    #[test]
    fn rejects_more_than_two_backedge_sources() {
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_three_latches".into(),
            vec![
                Type::get_pointer(array_ty),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let bound = data.params()[1];
        let outer_index = data.params()[2];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let dispatch_a = data
            .new_basic_block()
            .basic_block("dispatch_a".into(), vec![]);
        let dispatch_b = data
            .new_basic_block()
            .basic_block("dispatch_b".into(), vec![]);
        let latches = [
            data.new_basic_block().basic_block("left".into(), vec![]),
            data.new_basic_block().basic_block("middle".into(), vec![]),
            data.new_basic_block().basic_block("right".into(), vec![]),
        ];
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, dispatch_a, dispatch_b]
            .into_iter()
            .chain(latches)
            .chain([exit])
        {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let gep = data
            .new_local_inst()
            .get_elem_ptr(base, vec![outer_index, iv]);
        let load = data.new_local_inst().load(gep);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        for inst in [gep, load, compare] {
            data.layout_mut().insert_inst(header, inst);
        }
        let header_branch = data
            .new_local_inst()
            .branch(compare, dispatch_a, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, header_branch);

        let choose = data.new_local_inst().integer(1);
        let dispatch_a_branch =
            data.new_local_inst()
                .branch(choose, latches[0], vec![], dispatch_b, vec![]);
        data.layout_mut().insert_inst(dispatch_a, dispatch_a_branch);
        let dispatch_b_branch =
            data.new_local_inst()
                .branch(choose, latches[1], vec![], latches[2], vec![]);
        data.layout_mut().insert_inst(dispatch_b, dispatch_b_branch);

        for latch in latches {
            let one = data.new_local_inst().integer(1);
            let update = data.new_local_inst().binary(BinaryOp::Add, iv, one);
            data.layout_mut().insert_inst(latch, update);
            let backedge = data.new_local_inst().jump(header, vec![update]);
            data.layout_mut().insert_inst(latch, backedge);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(!run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.bb_data(header).params().as_slice(), [iv]);
        let InstKind::GetElemPtr(gep) = data.inst_data(gep).kind() else {
            unreachable!()
        };
        assert_eq!(gep.offsets().last().copied(), Some(iv));
    }

    #[test]
    fn rejects_a_non_unit_step_without_a_no_wrap_proof() {
        let (mut program, fixture) = build_loop_with_update(BinaryOp::Add, 2, BinaryOp::Lt, true);
        assert!(!run(&mut program, fixture.function));
        let data = program.func_data(fixture.function);
        assert_eq!(data.bb_data(fixture.header).params().len(), 1);
        let InstKind::GetElemPtr(gep) = data.inst_data(fixture.gep).kind() else {
            unreachable!()
        };
        assert_eq!(gep.offsets().last().copied(), Some(fixture.iv));
    }

    #[test]
    fn rejects_a_non_unit_step_with_a_wrapping_bound() {
        for (update_op, step, compare_op, bound) in [
            (BinaryOp::Add, 2, BinaryOp::Lt, i32::MAX),
            (BinaryOp::Sub, 2, BinaryOp::Gt, i32::MIN),
        ] {
            let (mut program, fixture) = build_loop_with_update(update_op, step, compare_op, true);
            replace_bound_with_constant(&mut program, &fixture, bound);
            assert!(!run(&mut program, fixture.function));
            let data = program.func_data(fixture.function);
            assert_eq!(data.bb_data(fixture.header).params().len(), 1);
            let InstKind::GetElemPtr(gep) = data.inst_data(fixture.gep).kind() else {
                unreachable!()
            };
            assert_eq!(gep.offsets().last().copied(), Some(fixture.iv));
        }
    }

    #[test]
    fn rejects_a_non_strict_bound_that_can_wrap() {
        let (mut program, fixture) = build_loop(BinaryOp::Le, true);
        let param_count = program
            .func_data(fixture.function)
            .bb_data(fixture.header)
            .params()
            .len();
        assert!(!run(&mut program, fixture.function));
        let data = program.func_data(fixture.function);
        assert_eq!(data.bb_data(fixture.header).params().len(), param_count);
        let InstKind::GetElemPtr(gep) = data.inst_data(fixture.gep).kind() else {
            unreachable!()
        };
        assert_eq!(gep.offsets().last().copied(), Some(fixture.iv));
    }

    #[test]
    fn rejects_a_single_cheap_aarch64_address_term() {
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_cheap_gep".into(),
            vec![Type::get_pointer(array_ty), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let bound = data.params()[1];
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
        let gep = data.new_local_inst().get_elem_ptr(base, vec![zero, iv]);
        let load = data.new_local_inst().load(gep);
        let one = data.new_local_inst().integer(1);
        let next_iv = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        for inst in [gep, load, next_iv, compare] {
            data.layout_mut().insert_inst(header, inst);
        }
        let branch = data
            .new_local_inst()
            .branch(compare, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);
        let backedge = data.new_local_inst().jump(header, vec![next_iv]);
        data.layout_mut().insert_inst(latch, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(!run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.bb_data(header).params().len(), 1);
        let InstKind::GetElemPtr(gep) = data.inst_data(gep).kind() else {
            unreachable!()
        };
        assert_eq!(gep.offsets().last().copied(), Some(iv));
    }

    #[test]
    fn carries_a_pointer_for_an_induction_index_before_the_final_gep_index() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, false);
        assert!(run(&mut program, fixture.function));
        let data = program.func_data(fixture.function);
        let pointer = data.bb_data(fixture.header).params()[1];
        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let InstKind::GetElemPtr(next) = data.inst_data(backedge.args()[1]).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(next.base(), pointer);
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(32));
        let InstKind::Jump(entry_jump) = data.inst_data(fixture.entry_jump).kind() else {
            panic!("entry terminator must remain a jump");
        };
        let InstKind::GetElemPtr(initial) = data.inst_data(entry_jump.args()[1]).kind() else {
            panic!("entry must compute the initial pointer");
        };
        assert_eq!(initial.offsets()[0], entry_jump.args()[0]);
        assert!(!run(&mut program, fixture.function));
    }

    #[test]
    fn queries_the_stride_of_a_global_array_base() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let initializer = program.new_value().zero_init(array_ty);
        let base = program.new_value().global_alloc(initializer);
        {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(fixture.function),
            };
            let zero = data.new_local_value().integer(0);
            data.replace_inst_with(fixture.gep)
                .get_elem_ptr(base, vec![zero, fixture.iv]);
        }

        let data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(fixture.function),
        };
        assert_eq!(
            gep_index_stride(&data, fixture.gep, 1),
            Some(crate::opt::utils::gep::GepIndexStride {
                byte_stride: 4,
                result_element_stride: 1,
            })
        );
    }

    #[test]
    fn carries_a_backward_pointer_through_an_invariant_suffix() {
        let row_ty = Type::get_array(Type::get_i32(), 32);
        let matrix_ty = Type::get_array(row_ty, 4);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_middle".into(),
            vec![
                Type::get_pointer(matrix_ty),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let bound = data.params()[1];
        let suffix = data.params()[2];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, latch, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let initial = data.new_local_inst().integer(3);
        let entry_jump = data.new_local_inst().jump(header, vec![initial]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let zero = data.new_local_inst().integer(0);
        let gep = data
            .new_local_inst()
            .get_elem_ptr(base, vec![zero, iv, suffix]);
        let load = data.new_local_inst().load(gep);
        let one = data.new_local_inst().integer(1);
        let next_iv = data.new_local_inst().binary(BinaryOp::Sub, iv, one);
        let compare = data.new_local_inst().binary(BinaryOp::Gt, iv, bound);
        for inst in [gep, load, next_iv, compare] {
            data.layout_mut().insert_inst(header, inst);
        }
        let branch = data
            .new_local_inst()
            .branch(compare, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);
        let backedge = data.new_local_inst().jump(header, vec![next_iv]);
        data.layout_mut().insert_inst(latch, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        let pointer = data.bb_data(header).params()[1];
        let InstKind::Jump(backedge) = data.inst_data(backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let InstKind::GetElemPtr(next) = data.inst_data(backedge.args()[1]).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(next.base(), pointer);
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(-32));
        let InstKind::Jump(entry_jump) = data.inst_data(entry_jump).kind() else {
            panic!("entry terminator must remain a jump");
        };
        let InstKind::GetElemPtr(initial_pointer) = data.inst_data(entry_jump.args()[1]).kind()
        else {
            panic!("entry must compute the initial pointer");
        };
        assert_eq!(initial_pointer.offsets(), [zero, initial, suffix]);
        assert!(!run(&mut program, function));
    }

    #[test]
    fn rejects_a_gep_that_uses_the_same_induction_variable_twice() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, false);
        let data = program.func_data_mut(fixture.function);
        let base = data.params()[0];
        data.replace_inst_with(fixture.gep)
            .get_elem_ptr(base, vec![fixture.iv, fixture.iv]);

        assert!(!run(&mut program, fixture.function));
        assert_eq!(
            program
                .func_data(fixture.function)
                .bb_data(fixture.header)
                .params()
                .len(),
            1
        );
    }

    #[test]
    fn rejects_a_loop_variant_suffix_after_the_induction_index() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, false);
        let data = program.func_data_mut(fixture.function);
        let base = data.params()[0];
        data.replace_inst_with(fixture.gep)
            .get_elem_ptr(base, vec![fixture.iv, fixture.next_iv]);

        assert!(!run(&mut program, fixture.function));
        assert_eq!(
            program
                .func_data(fixture.function)
                .bb_data(fixture.header)
                .params()
                .len(),
            1
        );
    }

    #[test]
    fn carries_an_affine_pointer_through_a_created_preheader() {
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_created_preheader".into(),
            vec![Type::get_pointer(array_ty), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let outer_index = data.params()[1];
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [left, right, header, latch, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let choose = data.new_local_inst().integer(1);
        let entry_branch = data
            .new_local_inst()
            .branch(choose, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(entry, entry_branch);

        let zero = data.new_local_inst().integer(0);
        let left_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(left, left_jump);
        let three = data.new_local_inst().integer(3);
        let right_jump = data.new_local_inst().jump(header, vec![three]);
        data.layout_mut().insert_inst(right, right_jump);

        let iv = data.bb_data(header).params()[0];
        let two = data.new_local_inst().integer(2);
        let product = data.new_local_inst().binary(BinaryOp::Mul, iv, two);
        let one = data.new_local_inst().integer(1);
        let derived = data.new_local_inst().binary(BinaryOp::Add, product, one);
        let gep = data
            .new_local_inst()
            .get_elem_ptr(base, vec![outer_index, derived]);
        let load = data.new_local_inst().load(gep);
        let next_iv = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let bound = data.new_local_inst().integer(10);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        for inst in [product, derived, gep, load, next_iv, compare] {
            data.layout_mut().insert_inst(header, inst);
        }
        let header_branch = data
            .new_local_inst()
            .branch(compare, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, header_branch);

        let backedge = data.new_local_inst().jump(header, vec![next_iv]);
        data.layout_mut().insert_inst(latch, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let looop = loops
            .loops()
            .iter()
            .find(|looop| looop.header() == header)
            .unwrap();
        let preheader = looop.get_preheader(&cfg).unwrap();
        assert!(![entry, left, right].contains(&preheader));

        for (jump_inst, initial) in [(left_jump, zero), (right_jump, three)] {
            let InstKind::Jump(jump) = data.inst_data(jump_inst).kind() else {
                panic!("outside edge must remain a jump");
            };
            assert_eq!(jump.target(), preheader);
            assert_eq!(jump.args(), [initial]);
        }

        let preheader_parameter = data.bb_data(preheader).params()[0];
        let preheader_terminator = data.layout().basicblock(preheader).terminator();
        let InstKind::Jump(preheader_jump) = data.inst_data(preheader_terminator).kind() else {
            panic!("created preheader must end in a jump");
        };
        assert_eq!(preheader_jump.target(), header);
        assert_eq!(preheader_jump.args().len(), 2);
        assert_eq!(preheader_jump.args()[0], preheader_parameter);

        let initial_pointer = preheader_jump.args()[1];
        assert_eq!(data.layout().parent_bb(initial_pointer), Some(preheader));
        let InstKind::GetElemPtr(initial_gep) = data.inst_data(initial_pointer).kind() else {
            panic!("preheader must compute the initial pointer");
        };
        let initial_index = *initial_gep.offsets().last().unwrap();
        let InstKind::Binary(initial_add) = data.inst_data(initial_index).kind() else {
            panic!("affine initial index must end in an add");
        };
        assert_eq!(initial_add.op(), BinaryOp::Add);
        assert_eq!(integer_constant(data, initial_add.rhs()), Some(1));
        let InstKind::Binary(initial_product) = data.inst_data(initial_add.lhs()).kind() else {
            panic!("affine initial index must contain a product");
        };
        assert_eq!(initial_product.op(), BinaryOp::Mul);
        assert_eq!(initial_product.lhs(), preheader_parameter);
        assert_eq!(integer_constant(data, initial_product.rhs()), Some(2));

        let pointer = data.bb_data(header).params()[1];
        let InstKind::Jump(backedge) = data.inst_data(backedge).kind() else {
            panic!("latch must remain a jump");
        };
        assert_eq!(backedge.args()[0], next_iv);
        let next_pointer = backedge.args()[1];
        let InstKind::GetElemPtr(next_gep) = data.inst_data(next_pointer).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(next_gep.base(), pointer);
        assert_eq!(integer_constant(data, next_gep.offsets()[0]), Some(2));

        let InstKind::GetElemPtr(rewritten_gep) = data.inst_data(gep).kind() else {
            unreachable!()
        };
        assert_eq!(rewritten_gep.base(), pointer);
        assert_eq!(integer_constant(data, rewritten_gep.offsets()[0]), Some(0));
        assert!(!run(&mut program, function));
    }

    #[test]
    fn preserves_distinct_same_target_initial_values() {
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_preheader".into(),
            vec![
                Type::get_pointer(array_ty),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let bound = data.params()[1];
        let outer_index = data.params()[2];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, latch, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let condition = data.new_local_inst().integer(1);
        let zero = data.new_local_inst().integer(0);
        let one = data.new_local_inst().integer(1);
        let entry_branch =
            data.new_local_inst()
                .branch(condition, header, vec![zero], header, vec![one]);
        data.layout_mut().insert_inst(entry, entry_branch);

        let iv = data.bb_data(header).params()[0];
        let gep = data
            .new_local_inst()
            .get_elem_ptr(base, vec![outer_index, iv]);
        let load = data.new_local_inst().load(gep);
        let next_iv = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        for inst in [gep, load, next_iv, compare] {
            data.layout_mut().insert_inst(header, inst);
        }
        let header_branch = data
            .new_local_inst()
            .branch(compare, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, header_branch);
        let backedge = data.new_local_inst().jump(header, vec![next_iv]);
        data.layout_mut().insert_inst(latch, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let looop = loops
            .loops()
            .iter()
            .find(|looop| looop.header() == header)
            .unwrap();
        let preheader = looop.get_preheader(&cfg).unwrap();
        assert_eq!(preheader, entry);
        assert_eq!(data.bb_data(header).params().len(), 2);

        let InstKind::Branch(entry_branch) = data.inst_data(entry_branch).kind() else {
            unreachable!()
        };
        assert_eq!(entry_branch.t_target(), header);
        assert_eq!(entry_branch.t_args()[0], zero);
        assert_eq!(entry_branch.f_target(), header);
        assert_eq!(entry_branch.f_args()[0], one);
        assert_eq!(entry_branch.t_args().len(), 2);
        assert_eq!(entry_branch.f_args().len(), 2);
        let true_pointer = entry_branch.t_args()[1];
        let false_pointer = entry_branch.f_args()[1];
        assert_ne!(true_pointer, false_pointer);
        let InstKind::GetElemPtr(true_initial) = data.inst_data(true_pointer).kind() else {
            unreachable!()
        };
        let InstKind::GetElemPtr(false_initial) = data.inst_data(false_pointer).kind() else {
            unreachable!()
        };
        assert_eq!(true_initial.offsets().last().copied(), Some(zero));
        assert_eq!(false_initial.offsets().last().copied(), Some(one));
        assert!(!run(&mut program, function));
    }
}
