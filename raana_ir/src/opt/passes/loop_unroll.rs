use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    ir::remap::EntityMapper,
    opt::{
        analysis_passes::{
            induction_variable::{
                BasicInductionVariableAnalysis, constant_trip_count, normalize_strict_exit,
            },
            loop_analysis::{Loop, LoopAnalysis},
        },
        prelude::*,
        utils::{
            cfg::CFG,
            logical_edge::{LogicalEdge, incoming_edges, outgoing_edges},
        },
    },
};

const MAX_FULL_UNROLL_TRIPS: usize = 8;
const MAX_UNROLLED_NON_TERMINATORS: usize = 64;

pub struct LoopUnroll;

#[derive(Debug, Clone)]
struct UnrollCandidate {
    header: BasicBlock,
    body: BasicBlock,
    exit: BasicBlock,
    backedge: LogicalEdge,
    exit_edge: LogicalEdge,
    trip_count: usize,
    loop_values: FxHashSet<Inst>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloneError {
    MissingLoopValue(Inst),
}

impl Pass for LoopUnroll {
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
            let (cfg, _dom_tree, loops) = LoopAnalysis::from_cfg(cfg);
            let ivs = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
            let candidate = loops
                .loops()
                .iter()
                .find_map(|looop| find_candidate(data, &cfg, &loops, &ivs, looop));
            let Some(candidate) = candidate else {
                return changed;
            };
            apply_candidate(data, &candidate);
            changed = true;
        }
    }
}

fn find_candidate(
    data: &ArenaContextMut<'_>,
    cfg: &CFG,
    loops: &LoopAnalysis,
    ivs: &BasicInductionVariableAnalysis,
    looop: &Loop,
) -> Option<UnrollCandidate> {
    let header = looop.header();
    if Some(header) == data.layout().entry_bb().map(|block| block.bb())
        || looop.body().len() != 2
        || looop.latches().len() != 1
    {
        return None;
    }
    let body = looop.latches()[0];
    if body == header
        || !looop.contains(body)
        || !data.bb_data(body).params().is_empty()
        || loops
            .loops()
            .iter()
            .any(|nested| nested.header() != header && looop.contains(nested.header()))
    {
        return None;
    }

    let header_edges = outgoing_edges(data, header);
    if header_edges.len() != 2 {
        return None;
    }
    let continue_edges = header_edges
        .iter()
        .copied()
        .filter(|edge| looop.contains(edge.target(data)))
        .collect::<Vec<_>>();
    let exit_edges = header_edges
        .iter()
        .copied()
        .filter(|edge| !looop.contains(edge.target(data)))
        .collect::<Vec<_>>();
    let [continue_edge] = continue_edges.as_slice() else {
        return None;
    };
    let [exit_edge] = exit_edges.as_slice() else {
        return None;
    };
    if continue_edge.target(data) != body || !continue_edge.args(data).is_empty() {
        return None;
    }
    let exit = exit_edge.target(data);

    let body_edges = outgoing_edges(data, body);
    let [backedge] = body_edges.as_slice() else {
        return None;
    };
    if backedge.target(data) != header {
        return None;
    }

    let incoming = incoming_edges(data, cfg, header);
    if incoming.len() != 2 {
        return None;
    }
    let entries = incoming
        .iter()
        .copied()
        .filter(|edge| !looop.contains(edge.source()))
        .collect::<Vec<_>>();
    let backedges = incoming
        .iter()
        .copied()
        .filter(|edge| looop.contains(edge.source()))
        .collect::<Vec<_>>();
    let [entry_edge] = entries.as_slice() else {
        return None;
    };
    let [incoming_backedge] = backedges.as_slice() else {
        return None;
    };
    if incoming_backedge != backedge
        || looop.get_preheader(cfg) != Some(entry_edge.source())
        || entry_edge.args(data).len() != data.bb_data(header).params().len()
        || backedge.args(data).len() != data.bb_data(header).params().len()
    {
        return None;
    }

    let trip_count = ivs.for_loop(looop).iter().find_map(|iv| {
        let exit = normalize_strict_exit(data, looop, iv)?;
        constant_trip_count(data, iv, exit).map(|trip| trip.iterations())
    })?;
    if trip_count > MAX_FULL_UNROLL_TRIPS {
        return None;
    }

    let header_insts = non_terminators(data, header);
    let body_insts = non_terminators(data, body);
    let total = header_insts
        .len()
        .checked_mul(trip_count.checked_add(1)?)?
        .checked_add(body_insts.len().checked_mul(trip_count)?)?;
    if total > MAX_UNROLLED_NON_TERMINATORS
        || header_insts
            .iter()
            .chain(&body_insts)
            .any(|&inst| matches!(data.inst_data(inst).kind(), InstKind::Alloc))
    {
        return None;
    }

    let header_values = data
        .bb_data(header)
        .params()
        .iter()
        .copied()
        .chain(data.layout().basicblock(header).insts().iter().copied())
        .collect::<FxHashSet<_>>();
    let body_values = data
        .bb_data(body)
        .params()
        .iter()
        .copied()
        .chain(data.layout().basicblock(body).insts().iter().copied())
        .collect::<FxHashSet<_>>();
    if body_values.iter().any(|&value| {
        data.inst_data(value).used_by().iter().any(|&user| {
            data.layout()
                .parent_bb(user)
                .is_none_or(|block| !looop.contains(block))
        })
    }) {
        return None;
    }
    let loop_values = header_values.union(&body_values).copied().collect();

    Some(UnrollCandidate {
        header,
        body,
        exit,
        backedge: *backedge,
        exit_edge: *exit_edge,
        trip_count,
        loop_values,
    })
}

fn apply_candidate(data: &mut ArenaContextMut<'_>, candidate: &UnrollCandidate) {
    let header_source = non_terminators(data, candidate.header);
    let body_source = non_terminators(data, candidate.body);
    let header_params = data.bb_data(candidate.header).params().to_vec();
    let backedge_args = candidate.backedge.args(data).to_vec();
    let exit_args = candidate.exit_edge.args(data).to_vec();

    if candidate.trip_count == 0 {
        let values = header_params
            .iter()
            .copied()
            .map(|param| (param, param))
            .collect();
        let final_args = remap_values(&exit_args, &values, &candidate.loop_values)
            .expect("zero-trip exit arguments must be available in the original header");
        data.replace_inst_with(data.layout().basicblock(candidate.header).terminator())
            .jump(candidate.exit, final_args);
        return;
    }

    let mut current_body = candidate.body;
    let mut current_values = FxHashMap::default();
    for &param in &header_params {
        current_values.insert(param, param);
    }
    for &inst in &header_source {
        current_values.insert(inst, inst);
    }
    for &inst in &body_source {
        current_values.insert(inst, inst);
    }
    data.replace_inst_with(data.layout().basicblock(candidate.header).terminator())
        .jump(candidate.body, vec![]);

    let mut anchor = candidate.body;
    for iteration in 0..candidate.trip_count {
        let next_header = new_header(data, candidate.header, iteration + 1, &mut anchor);
        let next_params = data.bb_data(next_header).params().to_vec();
        let next_args = remap_values(&backedge_args, &current_values, &candidate.loop_values)
            .expect("validated backedge arguments must be remappable");
        data.replace_inst_with(data.layout().basicblock(current_body).terminator())
            .jump(next_header, next_args);

        let mut next_values = header_params
            .iter()
            .copied()
            .zip(next_params.iter().copied())
            .collect::<FxHashMap<_, _>>();
        clone_non_terminators(
            data,
            &header_source,
            next_header,
            &mut next_values,
            &candidate.loop_values,
        )
        .expect("validated header instructions must be remappable");

        if iteration + 1 == candidate.trip_count {
            let final_args = remap_values(&exit_args, &next_values, &candidate.loop_values)
                .expect("validated exit arguments must be remappable");
            remap_external_header_uses(
                data,
                candidate,
                &header_params,
                &header_source,
                &next_values,
            );
            let jump = data.new_local_value().jump(candidate.exit, final_args);
            data.layout_mut().insert_inst(next_header, jump);
            break;
        }

        let next_body = new_body(data, candidate.body, iteration + 1, &mut anchor);
        clone_non_terminators(
            data,
            &body_source,
            next_body,
            &mut next_values,
            &candidate.loop_values,
        )
        .expect("validated body instructions must be remappable");
        let enter_body = data.new_local_value().jump(next_body, vec![]);
        data.layout_mut().insert_inst(next_header, enter_body);
        let placeholder = data.new_local_value().jump(next_header, next_params);
        data.layout_mut().insert_inst(next_body, placeholder);

        current_body = next_body;
        current_values = next_values;
    }
}

fn remap_external_header_uses(
    data: &mut ArenaContextMut<'_>,
    candidate: &UnrollCandidate,
    header_params: &[Inst],
    header_source: &[Inst],
    final_values: &FxHashMap<Inst, Inst>,
) {
    let mut users = FxHashSet::default();
    for &value in header_params.iter().chain(header_source) {
        users.extend(
            data.inst_data(value)
                .used_by()
                .iter()
                .copied()
                .filter(|&user| {
                    data.layout()
                        .parent_bb(user)
                        .is_some_and(|block| block != candidate.header && block != candidate.body)
                }),
        );
    }
    let mut mapper = IterationMapper {
        values: final_values,
        loop_values: &candidate.loop_values,
    };
    let rewrites = users
        .into_iter()
        .map(|user| {
            let mapped = data
                .inst_data(user)
                .remap_refs(&mut mapper)
                .expect("validated external header use must be remappable");
            (user, mapped)
        })
        .collect::<Vec<_>>();
    for (user, mapped) in rewrites {
        data.replace_inst_with(user).raw(mapped);
    }
}

fn new_header(
    data: &mut ArenaContextMut<'_>,
    source: BasicBlock,
    iteration: usize,
    anchor: &mut BasicBlock,
) -> BasicBlock {
    let name = format!("{}_unroll_{iteration}", data.bb_data(source).name());
    let param_types = data
        .bb_data(source)
        .params()
        .iter()
        .map(|&param| data.inst_data(param).ty().clone())
        .collect();
    let block = data.new_basic_block().basic_block(name, param_types);
    data.layout_mut().insert_bb_after(*anchor, block);
    *anchor = block;
    block
}

fn new_body(
    data: &mut ArenaContextMut<'_>,
    source: BasicBlock,
    iteration: usize,
    anchor: &mut BasicBlock,
) -> BasicBlock {
    let name = format!("{}_unroll_{iteration}", data.bb_data(source).name());
    let block = data.new_basic_block().basic_block(name, vec![]);
    data.layout_mut().insert_bb_after(*anchor, block);
    *anchor = block;
    block
}

fn clone_non_terminators(
    data: &mut ArenaContextMut<'_>,
    source: &[Inst],
    destination: BasicBlock,
    values: &mut FxHashMap<Inst, Inst>,
    loop_values: &FxHashSet<Inst>,
) -> Result<(), CloneError> {
    for &inst in source {
        let ty = data.inst_data(inst).ty().clone();
        let shell = data.new_local_value().undef(ty);
        values.insert(inst, shell);
    }
    let mut mapper = IterationMapper {
        values,
        loop_values,
    };
    for &inst in source {
        let mapped = data.inst_data(inst).remap_refs(&mut mapper)?;
        let cloned = mapper.values[&inst];
        data.replace_inst_with(cloned).raw(mapped);
        data.layout_mut().insert_inst(destination, cloned);
    }
    Ok(())
}

fn remap_values(
    source: &[Inst],
    values: &FxHashMap<Inst, Inst>,
    loop_values: &FxHashSet<Inst>,
) -> Result<Vec<Inst>, CloneError> {
    source
        .iter()
        .map(|&value| map_value(value, values, loop_values))
        .collect()
}

fn map_value(
    value: Inst,
    values: &FxHashMap<Inst, Inst>,
    loop_values: &FxHashSet<Inst>,
) -> Result<Inst, CloneError> {
    if value.is_global() {
        return Ok(value);
    }
    if let Some(&mapped) = values.get(&value) {
        return Ok(mapped);
    }
    if loop_values.contains(&value) {
        return Err(CloneError::MissingLoopValue(value));
    }
    Ok(value)
}

struct IterationMapper<'a> {
    values: &'a FxHashMap<Inst, Inst>,
    loop_values: &'a FxHashSet<Inst>,
}

impl EntityMapper for IterationMapper<'_> {
    type Error = CloneError;

    fn map_inst(&mut self, inst: Inst) -> Result<Inst, Self::Error> {
        map_value(inst, self.values, self.loop_values)
    }

    fn map_block(&mut self, block: BasicBlock) -> Result<BasicBlock, Self::Error> {
        Ok(block)
    }
}

fn non_terminators(data: &FunctionData, block: BasicBlock) -> Vec<Inst> {
    let insts = data.layout().basicblock(block).insts();
    insts
        .iter()
        .copied()
        .take(insts.len().saturating_sub(1))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opt::utils::logical_edge::outgoing_edges;

    struct Fixture {
        program: Program,
        function: Function,
        header: BasicBlock,
        body: BasicBlock,
        exit: BasicBlock,
    }

    fn fixture(initial: i32, bound: i32, step: i32) -> Fixture {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "unroll".into(), vec![]);
        let (header, body, exit) = {
            let data = program.func_data_mut(function);
            let entry = data.add_entry_block();
            let header = data
                .new_basic_block()
                .basic_block("header".into(), vec![Type::get_i32(), Type::get_i32()]);
            let body = data.new_basic_block().basic_block("body".into(), vec![]);
            let exit = data
                .new_basic_block()
                .basic_block("exit".into(), vec![Type::get_i32()]);
            for block in [header, body, exit] {
                data.layout_mut().push_bb_back(block);
            }

            let initial = data.new_local_value().integer(initial);
            let zero = data.new_local_value().integer(0);
            let entry_jump = data.new_local_value().jump(header, vec![initial, zero]);
            data.layout_mut().insert_inst(entry, entry_jump);

            let params = data.bb_data(header).params().to_vec();
            let iv = params[0];
            let acc = params[1];
            let bound = data.new_local_value().integer(bound);
            let compare = if step > 0 {
                data.new_local_value().binary(BinaryOp::Lt, iv, bound)
            } else {
                data.new_local_value().binary(BinaryOp::Gt, iv, bound)
            };
            data.layout_mut().insert_inst(header, compare);
            let branch = data
                .new_local_value()
                .branch(compare, body, vec![], exit, vec![acc]);
            data.layout_mut().insert_inst(header, branch);

            let next_acc = data.new_local_value().binary(BinaryOp::Add, acc, iv);
            let step_value = data.new_local_value().integer(step.unsigned_abs() as i32);
            let next_iv = if step > 0 {
                data.new_local_value().binary(BinaryOp::Add, iv, step_value)
            } else {
                data.new_local_value().binary(BinaryOp::Sub, iv, step_value)
            };
            for inst in [next_acc, next_iv] {
                data.layout_mut().insert_inst(body, inst);
            }
            let backedge = data.new_local_value().jump(header, vec![next_iv, next_acc]);
            data.layout_mut().insert_inst(body, backedge);

            let result = data.bb_data(exit).params()[0];
            let ret = data.new_local_value().ret(Some(result));
            data.layout_mut().insert_inst(exit, ret);
            (header, body, exit)
        };
        Fixture {
            program,
            function,
            header,
            body,
            exit,
        }
    }

    fn run(fixture: &mut Fixture) -> bool {
        let mut data = ArenaContextMut {
            program: &mut fixture.program,
            curr_func: Some(fixture.function),
        };
        LoopUnroll.run_on(&mut data)
    }

    fn assert_edge_arguments_well_typed(data: &FunctionData) {
        let cfg = CFG::new(data).unwrap();
        for &source in cfg.blocks() {
            for edge in outgoing_edges(data, source) {
                let params = data.bb_data(edge.target(data)).params();
                let args = edge.args(data);
                assert_eq!(args.len(), params.len());
                for (&arg, &param) in args.iter().zip(params) {
                    assert_eq!(data.inst_data(arg).ty(), data.inst_data(param).ty());
                }
            }
        }
    }

    #[test]
    fn fully_unrolls_a_small_forward_loop_and_is_idempotent() {
        let mut fixture = fixture(0, 4, 1);
        assert!(run(&mut fixture));
        assert!(!run(&mut fixture));

        let data = fixture.program.func_data(fixture.function);
        let (_cfg, _dom, loops) = LoopAnalysis::new(data);
        assert!(loops.loops().is_empty());
        assert_edge_arguments_well_typed(data);
        let header_blocks = data
            .layout()
            .basicblocks()
            .iter()
            .filter(|block| data.bb_data(block.bb()).name().contains("header"))
            .count();
        let body_blocks = data
            .layout()
            .basicblocks()
            .iter()
            .filter(|block| data.bb_data(block.bb()).name().contains("body"))
            .count();
        assert_eq!(header_blocks, 5);
        assert_eq!(body_blocks, 4);

        let final_header = data
            .layout()
            .basicblocks()
            .iter()
            .find(|block| {
                let terminator = block.terminator();
                matches!(data.inst_data(terminator).kind(), InstKind::Jump(jump)
                    if jump.target() == fixture.exit)
            })
            .unwrap()
            .bb();
        let final_params = data.bb_data(final_header).params();
        let InstKind::Jump(exit_jump) = data
            .inst_data(data.layout().basicblock(final_header).terminator())
            .kind()
        else {
            unreachable!();
        };
        assert_eq!(exit_jump.args(), [final_params[1]]);
    }

    #[test]
    fn preserves_the_final_failed_header_visit_for_zero_trip_loops() {
        let mut fixture = fixture(0, 0, 1);
        assert!(run(&mut fixture));
        let data = fixture.program.func_data(fixture.function);
        let InstKind::Jump(jump) = data
            .inst_data(data.layout().basicblock(fixture.header).terminator())
            .kind()
        else {
            panic!("zero-trip header must jump directly to exit");
        };
        assert_eq!(jump.target(), fixture.exit);
        assert_eq!(jump.args().len(), 1);
        let (_cfg, _dom, loops) = LoopAnalysis::new(data);
        assert!(loops.loops().is_empty());
        assert_edge_arguments_well_typed(data);
    }

    #[test]
    fn unrolls_backward_and_non_unit_loops() {
        for (initial, bound, step, expected_headers) in [(7, 0, -2, 5), (0, 7, 2, 5)] {
            let mut fixture = fixture(initial, bound, step);
            assert!(run(&mut fixture));
            let data = fixture.program.func_data(fixture.function);
            let headers = data
                .layout()
                .basicblocks()
                .iter()
                .filter(|block| data.bb_data(block.bb()).name().contains("header"))
                .count();
            assert_eq!(headers, expected_headers);
            assert_edge_arguments_well_typed(data);
        }
    }

    #[test]
    fn rewrites_external_header_parameter_uses_to_the_final_iteration() {
        let mut fixture = fixture(0, 4, 1);
        let original_acc = fixture
            .program
            .func_data(fixture.function)
            .bb_data(fixture.header)
            .params()[1];
        let ret = fixture
            .program
            .func_data(fixture.function)
            .layout()
            .basicblock(fixture.exit)
            .terminator();
        fixture
            .program
            .func_data_mut(fixture.function)
            .replace_inst_with(ret)
            .ret(Some(original_acc));

        assert!(run(&mut fixture));
        let data = fixture.program.func_data(fixture.function);
        let InstKind::Return(ret) = data.inst_data(ret).kind() else {
            unreachable!();
        };
        let final_value = ret.value().unwrap();
        assert_ne!(final_value, original_acc);
        let final_header = data.layout().parent_bb(final_value).or_else(|| {
            data.layout().basicblocks().iter().find_map(|block| {
                data.bb_data(block.bb())
                    .params()
                    .contains(&final_value)
                    .then_some(block.bb())
            })
        });
        assert!(final_header.is_some());
        assert!(
            data.bb_data(final_header.unwrap())
                .name()
                .contains("unroll_4")
        );
    }

    #[test]
    fn rejects_large_and_dynamic_trip_counts() {
        let mut large = fixture(0, 9, 1);
        assert!(!run(&mut large));

        let mut dynamic = fixture(0, 4, 1);
        let function = dynamic.function;
        let header = dynamic.header;
        let body = dynamic.body;
        {
            let data = dynamic.program.func_data_mut(function);
            let parameter = data.new_basic_block().add_param(body, Type::get_i32());
            let terminator = data.layout().basicblock(header).terminator();
            let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
                unreachable!();
            };
            let cond = branch.cond();
            let false_target = branch.f_target();
            let false_args = branch.f_args().to_vec();
            data.replace_inst_with(terminator).branch(
                cond,
                body,
                vec![parameter],
                false_target,
                false_args,
            );
        }
        assert!(!run(&mut dynamic));
    }
}
