use crate::opt::{
    analysis_passes::{
        dom_tree::v2::DominanceTree,
        induction_variable::{
            BasicInductionVariable, BasicInductionVariableAnalysis, InductionStep,
        },
        loop_analysis::{Loop, LoopAnalysis},
    },
    prelude::*,
    utils::{
        cfg::CFG,
        logical_edge::{LogicalEdge, LogicalEdgeRewriter, incoming_edges, outgoing_edges},
        pointer_strength_reduction_cost::estimate_aarch64_pointer_strength_reduction,
        preheader::{EnsurePreheader, ensure_preheader},
    },
};

pub struct PointerStrengthReduction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Forward,
}

struct Candidate {
    gep: Inst,
    iv_position: usize,
    latch: BasicBlock,
    backedge: LogicalEdge,
    direction: Direction,
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
            let Some(direction) = Self::safe_unit_direction(data, looop, iv) else {
                continue;
            };
            let Some(iv_position) = data
                .bb_data(looop.header())
                .params()
                .iter()
                .position(|&parameter| parameter == iv.parameter())
            else {
                continue;
            };
            if backedge.args(data).get(iv_position).copied() != iv.update_values().first().copied()
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
                    if gep.offsets().len() < 2
                        || gep.offsets().last().copied() != Some(iv.parameter())
                        || !Self::available_at_header(data, dom_tree, looop, gep.base())
                        || !gep.offsets()[..gep.offsets().len() - 1]
                            .iter()
                            .all(|&offset| Self::available_at_header(data, dom_tree, looop, offset))
                        || !Self::has_only_loop_memory_users(data, looop, inst)
                    {
                        continue;
                    }
                    let Some(cost) =
                        estimate_aarch64_pointer_strength_reduction(data, cfg, looop, gep)
                    else {
                        continue;
                    };
                    if !cost.is_profitable() {
                        continue;
                    }
                    return Some(Candidate {
                        gep: inst,
                        iv_position,
                        latch,
                        backedge,
                        direction,
                        base: gep.base(),
                        offsets: gep.offsets().to_vec(),
                        pointer_ty: data.inst_data(inst).ty().clone(),
                    });
                }
            }
        }
        None
    }

    fn safe_unit_direction(
        data: &ArenaContextMut<'_>,
        looop: &Loop,
        iv: &BasicInductionVariable,
    ) -> Option<Direction> {
        let step = match iv.step() {
            InductionStep::Add(step) if Self::integer_constant(data, step) == Some(1) => {
                Direction::Forward
            }
            _ => return None,
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
        if !data.inst_data(compare.lhs()).ty().is_i32()
            || !data.inst_data(compare.rhs()).ty().is_i32()
        {
            return None;
        }

        let mut op = compare.op();
        if !true_inside {
            op = op.complement_integer_compare()?;
        }
        if compare.rhs() == iv.parameter() {
            op = op.swap_compare_args()?;
        } else if compare.lhs() != iv.parameter() {
            return None;
        }
        match (step, op) {
            (Direction::Forward, BinaryOp::Lt) => Some(Direction::Forward),
            _ => None,
        }
    }

    fn integer_constant(data: &ArenaContextMut<'_>, inst: Inst) -> Option<i32> {
        match data.inst_data(inst).kind() {
            InstKind::Integer(integer) => Some(integer.value()),
            _ => None,
        }
    }

    fn available_at_header(
        data: &ArenaContextMut<'_>,
        dom_tree: &DominanceTree,
        looop: &Loop,
        value: Inst,
    ) -> bool {
        if value.is_global() || data.inst_data(value).kind().is_const() {
            return true;
        }
        match data.layout().parent_bb(value) {
            Some(block) => !looop.contains(block) && dom_tree.dominates(block, looop.header()),
            None if matches!(data.inst_data(value).kind(), InstKind::BlockArgRef(..)) => data
                .layout()
                .basicblocks()
                .iter()
                .map(|layout| layout.bb())
                .find(|&block| data.bb_data(block).params().contains(&value))
                .is_some_and(|block| {
                    !looop.contains(block)
                        && dom_tree.contains(block)
                        && dom_tree.dominates(block, looop.header())
                }),
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
            let initial_iv = edge.args(data)[candidate.iv_position];
            let mut initial_offsets = candidate.offsets.clone();
            *initial_offsets.last_mut().unwrap() = initial_iv;
            let initial_pointer = data
                .new_local_value()
                .get_elem_ptr(candidate.base, initial_offsets);
            assert_eq!(data.inst_data(initial_pointer).ty(), &candidate.pointer_ty);
            data.layout_mut()
                .insert_before_terminator(preheader, initial_pointer);
            rewrites.append_arg(data, edge, initial_pointer);
        }

        let pointer = data
            .new_basic_block()
            .add_param(looop.header(), candidate.pointer_ty.clone());
        let pointer_step = data.new_local_value().integer(match candidate.direction {
            Direction::Forward => 1,
        });
        let next_pointer = data
            .new_local_value()
            .get_elem_ptr(pointer, vec![pointer_step]);
        assert_eq!(data.inst_data(next_pointer).ty(), &candidate.pointer_ty);
        data.layout_mut()
            .insert_before_terminator(candidate.latch, next_pointer);
        rewrites.append_arg(data, candidate.backedge, next_pointer);
        assert!(rewrites.apply(data));

        let zero = data.new_local_value().integer(0);
        data.replace_inst_with(candidate.gep)
            .get_elem_ptr(pointer, vec![zero]);
        assert_eq!(data.inst_data(candidate.gep).ty(), &candidate.pointer_ty);
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
            let (cfg, dom_tree, loops) = LoopAnalysis::new(data);
            let ivs = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
            let mut transformed = false;
            for looop in loops.loops() {
                let Some(candidate) =
                    Self::find_candidate(data, &cfg, &dom_tree, &loops, &ivs, looop)
                else {
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

    fn build_loop(compare_op: BinaryOp, iv_last: bool) -> (Program, LoopFixture) {
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
        let one = data.new_local_inst().integer(1);
        let next_iv = data.new_local_inst().binary(BinaryOp::Add, iv, one);
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

    fn run(program: &mut Program, function: Function) -> bool {
        let mut context = ArenaContextMut {
            program,
            curr_func: Some(function),
        };
        PointerStrengthReduction.run_on(&mut context)
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
    fn rejects_an_induction_index_before_the_final_gep_index() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, false);
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
