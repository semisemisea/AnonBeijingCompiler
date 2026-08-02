use rustc_hash::FxHashMap;

use crate::opt::{
    analysis_passes::{
        dom_tree::v2::DominanceTree,
        induction_variable::{
            BasicInductionVariableAnalysis, InductionDirection, normalize_strict_unit_exit,
        },
        loop_analysis::{Loop, LoopAnalysis},
    },
    prelude::*,
    utils::{
        cfg::CFG,
        gep::gep_index_stride,
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
    latch: BasicBlock,
    backedge: LogicalEdge,
    pointer_step: i32,
    base: Inst,
    offsets: Vec<Inst>,
    pointer_ty: Type,
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
        let &[latch] = looop.latches() else {
            return None;
        };
        if latch == looop.header() {
            return None;
        }

        let backedges = incoming_edges(data, cfg, looop.header())
            .into_iter()
            .filter(|edge| looop.contains(edge.source()))
            .collect::<Vec<_>>();
        let &[backedge] = backedges.as_slice() else {
            return None;
        };
        if backedge.source() != latch {
            return None;
        }

        for iv in ivs.for_loop(looop) {
            let Some(direction) = normalize_strict_unit_exit(data, looop, iv) else {
                continue;
            };
            let Some(header_iv_position) = data
                .bb_data(looop.header())
                .params()
                .iter()
                .position(|&parameter| parameter == iv.parameter())
            else {
                continue;
            };
            if backedge.args(data).get(header_iv_position).copied()
                != iv.update_values().first().copied()
                || iv.update_values().len() != 1
            {
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
                    let mut iv_positions =
                        gep.offsets()
                            .iter()
                            .enumerate()
                            .filter_map(|(position, &offset)| {
                                (offset == iv.parameter()).then_some(position)
                            });
                    let Some(gep_iv_position) = iv_positions.next() else {
                        continue;
                    };
                    if iv_positions.next().is_some() {
                        continue;
                    }
                    if gep.offsets().len() < 2
                        || !Self::available_at_header(
                            data,
                            dom_tree,
                            looop,
                            parameter_blocks,
                            gep.base(),
                        )
                        || !gep
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
                    {
                        continue;
                    }
                    let Some(stride) = gep_index_stride(data, inst, gep_iv_position) else {
                        continue;
                    };
                    let signed_pointer_step = match direction {
                        InductionDirection::Forward => stride.result_element_stride,
                        InductionDirection::Backward => {
                            let Some(step) = stride.result_element_stride.checked_neg() else {
                                continue;
                            };
                            step
                        }
                    };
                    let signed_byte_delta = match direction {
                        InductionDirection::Forward => stride.byte_stride,
                        InductionDirection::Backward => {
                            let Some(delta) = stride.byte_stride.checked_neg() else {
                                continue;
                            };
                            delta
                        }
                    };
                    let Some(cost) = estimate_aarch64_pointer_strength_reduction(
                        data,
                        cfg,
                        looop,
                        gep,
                        signed_byte_delta,
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
                        latch,
                        backedge,
                        pointer_step: signed_pointer_step,
                        base: gep.base(),
                        offsets: gep.offsets().to_vec(),
                        pointer_ty: data.inst_data(inst).ty().clone(),
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
            Some(EnsurePreheader::Existing(preheader)) => preheader,
            Some(EnsurePreheader::Created(..)) => return ApplyResult::Changed,
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

        let mut rewrites = LogicalEdgeRewriter::new();
        for edge in entry_edges {
            let initial_iv = edge.args(data)[candidate.header_iv_position];
            let mut initial_offsets = candidate.offsets.clone();
            initial_offsets[candidate.gep_iv_position] = initial_iv;
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
        let next_pointer = data
            .new_local_value()
            .get_elem_ptr(pointer, vec![pointer_step]);
        debug_assert_eq!(data.inst_data(next_pointer).ty(), &candidate.pointer_ty);
        data.layout_mut()
            .insert_before_terminator(candidate.latch, next_pointer);
        rewrites.append_arg(data, candidate.backedge, next_pointer);
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
