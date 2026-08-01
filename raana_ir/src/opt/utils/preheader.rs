//! Dedicated loop-preheader canonicalization.

use crate::opt::{
    analysis_passes::loop_analysis::Loop,
    pass::ArenaContextMut,
    prelude::{Arena, BasicBlock, BasicBlockBuilder, LocalInstBuilder},
    utils::{
        cfg::CFG,
        logical_edge::{LogicalEdgeRewriter, incoming_edges},
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsurePreheader {
    Existing(BasicBlock),
    Created(BasicBlock),
}

/// Return or create a dedicated preheader for `looop`.
///
/// `Created` invalidates CFG, dominance, loop, and induction-variable analysis
/// snapshots. Entry-header loops are rejected because changing the function
/// entry also requires migrating its parameter values.
pub fn ensure_preheader(
    data: &mut ArenaContextMut<'_>,
    cfg: &CFG,
    looop: &Loop,
) -> Option<EnsurePreheader> {
    let header = looop.header();
    if header == cfg.entry() {
        return None;
    }
    if let Some(preheader) = looop.get_preheader(cfg) {
        return Some(EnsurePreheader::Existing(preheader));
    }

    let outside_edges = incoming_edges(data, cfg, header)
        .into_iter()
        .filter(|edge| !looop.contains(edge.source()))
        .map(|edge| (edge, edge.args(data).to_vec()))
        .collect::<Vec<_>>();
    if outside_edges.is_empty() {
        return None;
    }

    let parameter_types = data
        .bb_data(header)
        .params()
        .iter()
        .map(|&parameter| data.inst_data(parameter).ty().clone())
        .collect();
    let preheader = data
        .new_basic_block()
        .basic_block("preheader".into(), parameter_types);
    let preheader_parameters = data.bb_data(preheader).params().to_vec();
    data.layout_mut().insert_bb_before(header, preheader);

    let jump = data.new_local_inst().jump(header, preheader_parameters);
    data.layout_mut().insert_inst(preheader, jump);

    let mut rewrites = LogicalEdgeRewriter::new();
    for (edge, arguments) in outside_edges {
        rewrites.retarget(data, edge, preheader, arguments);
    }
    assert!(
        rewrites.apply(data),
        "preheader creation must redirect at least one outside edge"
    );

    Some(EnsurePreheader::Created(preheader))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{
            InstKind, Program, Type,
            builder_trait::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
        },
        opt::{
            analysis_passes::loop_analysis::LoopAnalysis,
            utils::logical_edge::{LogicalEdgeArm, incoming_edges},
        },
    };

    fn loop_with_header(loops: &LoopAnalysis, header: BasicBlock) -> &Loop {
        loops
            .loops()
            .iter()
            .find(|looop| looop.header() == header)
            .expect("expected loop header")
    }

    #[test]
    fn reuses_an_existing_preheader() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "existing".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data.new_basic_block().basic_block("header".into(), vec![]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, latch, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let entry_jump = data.new_local_inst().jump(header, vec![]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let condition = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(condition, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);
        let backedge = data.new_local_inst().jump(header, vec![]);
        data.layout_mut().insert_inst(latch, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let block_count = data.layout().basicblocks().len();
        let mut context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        assert_eq!(
            ensure_preheader(&mut context, &cfg, loop_with_header(&loops, header)),
            Some(EnsurePreheader::Existing(entry))
        );
        assert_eq!(context.layout().basicblocks().len(), block_count);
        assert_eq!(context.layout().basicblock(entry).terminator(), entry_jump);
    }

    #[test]
    fn rejects_an_entry_header_loop() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "entry_loop".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let condition = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(entry, branch);
        let backedge = data.new_local_inst().jump(entry, vec![]);
        data.layout_mut().insert_inst(body, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let block_count = data.layout().basicblocks().len();
        let mut context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        assert_eq!(
            ensure_preheader(&mut context, &cfg, loop_with_header(&loops, entry)),
            None
        );
        assert_eq!(context.layout().basicblocks().len(), block_count);
        assert_eq!(context.layout().entry_bb().unwrap().bb(), entry);
        assert!(CFG::new(&context).is_some());
    }

    #[test]
    fn preserves_all_logical_entries_phi_values_and_backedges() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "create".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let latch_a = data.new_basic_block().basic_block("latch_a".into(), vec![]);
        let latch_b = data.new_basic_block().basic_block("latch_b".into(), vec![]);
        for block in [left, right, header, latch_a, latch_b] {
            data.layout_mut().push_bb_back(block);
        }

        let condition = data.new_local_inst().integer(1);
        let entry_branch = data
            .new_local_inst()
            .branch(condition, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(entry, entry_branch);

        let ten = data.new_local_inst().integer(10);
        let eleven = data.new_local_inst().integer(11);
        let left_branch =
            data.new_local_inst()
                .branch(condition, header, vec![ten], header, vec![eleven]);
        data.layout_mut().insert_inst(left, left_branch);
        let twenty = data.new_local_inst().integer(20);
        let right_jump = data.new_local_inst().jump(header, vec![twenty]);
        data.layout_mut().insert_inst(right, right_jump);

        let header_branch =
            data.new_local_inst()
                .branch(condition, latch_a, vec![], latch_b, vec![]);
        data.layout_mut().insert_inst(header, header_branch);
        let thirty = data.new_local_inst().integer(30);
        let backedge_a = data.new_local_inst().jump(header, vec![thirty]);
        data.layout_mut().insert_inst(latch_a, backedge_a);
        let forty = data.new_local_inst().integer(40);
        let backedge_b = data.new_local_inst().jump(header, vec![forty]);
        data.layout_mut().insert_inst(latch_b, backedge_b);

        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let mut context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        let Some(EnsurePreheader::Created(preheader)) =
            ensure_preheader(&mut context, &cfg, loop_with_header(&loops, header))
        else {
            panic!("expected a new preheader");
        };

        assert_eq!(
            context
                .bb_data(preheader)
                .params()
                .iter()
                .map(|&parameter| context.inst_data(parameter).ty().clone())
                .collect::<Vec<_>>(),
            vec![Type::get_i32()]
        );
        let InstKind::Jump(forward) = context
            .inst_data(context.layout().basicblock(preheader).terminator())
            .kind()
        else {
            panic!("preheader must end in a jump");
        };
        assert_eq!(forward.target(), header);
        assert_eq!(forward.args(), context.bb_data(preheader).params());
        let blocks = context
            .layout()
            .basicblocks()
            .iter()
            .map(|layout| layout.bb())
            .collect::<Vec<_>>();
        let header_position = blocks.iter().position(|&block| block == header).unwrap();
        assert_eq!(blocks[header_position - 1], preheader);

        let InstKind::Branch(left_rewritten) = context.inst_data(left_branch).kind() else {
            panic!("left terminator must remain a branch");
        };
        assert_eq!(left_rewritten.t_target(), preheader);
        assert_eq!(left_rewritten.t_args(), [ten]);
        assert_eq!(left_rewritten.f_target(), preheader);
        assert_eq!(left_rewritten.f_args(), [eleven]);
        let InstKind::Jump(right_rewritten) = context.inst_data(right_jump).kind() else {
            panic!("right terminator must remain a jump");
        };
        assert_eq!(right_rewritten.target(), preheader);
        assert_eq!(right_rewritten.args(), [twenty]);

        for (backedge, argument) in [(backedge_a, thirty), (backedge_b, forty)] {
            let InstKind::Jump(jump) = context.inst_data(backedge).kind() else {
                panic!("backedge must remain a jump");
            };
            assert_eq!(jump.target(), header);
            assert_eq!(jump.args(), [argument]);
        }

        let (rebuilt_cfg, _dom_tree, rebuilt_loops) = LoopAnalysis::new(&context);
        let rebuilt_loop = loop_with_header(&rebuilt_loops, header);
        assert_eq!(rebuilt_loop.get_preheader(&rebuilt_cfg), Some(preheader));
        let incoming = incoming_edges(&context, &rebuilt_cfg, preheader);
        assert_eq!(incoming.len(), 3);
        assert_eq!(
            incoming
                .iter()
                .filter(|edge| edge.arm() != LogicalEdgeArm::Jump)
                .count(),
            2
        );

        let block_count = context.layout().basicblocks().len();
        assert_eq!(
            ensure_preheader(&mut context, &rebuilt_cfg, rebuilt_loop),
            Some(EnsurePreheader::Existing(preheader))
        );
        assert_eq!(context.layout().basicblocks().len(), block_count);
    }
}
