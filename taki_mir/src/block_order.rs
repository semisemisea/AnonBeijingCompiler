//! The order of traversing basic blocks uses RPO of the dominance tree.
use std::ops::Range;

use raana_ir::opt::prelude::{IDAllocator, cfg, dom_tree};
use rustc_hash::FxHashMap;

pub type MirBlockIndex = crate::reg_alloc::index::Block;

use crate::prelude::*;

pub struct BlockLoweringOrder {
    lowered_order: Vec<LoweredBlock>,
    lowered_succ_indices: Vec<MirBlockIndex>,
    lowered_succ_ranges: Vec<(Option<HirInst>, Range<usize>)>,
    hlir_block_map: FxHashMap<HirBasicBlock, MirBlockIndex>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LoweredBlock {
    Orig {
        block: HirBasicBlock,
    },
    Edge {
        pred: HirBasicBlock,
        succ: HirBasicBlock,
        succ_idx: u32,
    },
}

impl LoweredBlock {
    pub fn orig_block(&self) -> Option<HirBasicBlock> {
        match self {
            LoweredBlock::Orig { block } => Some(*block),
            LoweredBlock::Edge { .. } => None,
        }
    }

    pub fn pred_block(&self) -> Option<HirBasicBlock> {
        match self {
            LoweredBlock::Edge { pred, .. } => Some(*pred),
            LoweredBlock::Orig { .. } => None,
        }
    }

    pub fn succ_block(&self) -> Option<HirBasicBlock> {
        match self {
            LoweredBlock::Edge { succ, .. } => Some(*succ),
            LoweredBlock::Orig { .. } => None,
        }
    }
}

impl BlockLoweringOrder {
    pub fn new(arena: ArenaContext<'_>) -> BlockLoweringOrder {
        assert!(arena.curr_func.is_some());

        let func_data = arena.f();
        let mut bb_id = IDAllocator::default();
        let (graph, prece) = cfg::build_cfg_both(func_data, &mut bb_id);
        let plain_rpo = cfg::rpo_path(&graph);

        let idom_map = dom_tree::idom(&prece, &plain_rpo);
        let dom_children = dom_tree::build_dominance_tree(&idom_map, plain_rpo.len());

        let mut rpo_index = vec![0usize; plain_rpo.len()];
        for (i, &id) in plain_rpo.iter().enumerate() {
            rpo_index[id] = i;
        }

        // Visit the dominator tree in pre-order, sorting children by the
        // original CFG RPO so that sibling branches are processed in a
        // predictable order.
        let mut domtree_rpo = Vec::with_capacity(plain_rpo.len());
        fn domtree_dfs(
            node: usize,
            children: &[Vec<usize>],
            rpo_idx: &[usize],
            result: &mut Vec<usize>,
        ) {
            result.push(node);
            let mut child_list = children[node].clone();
            child_list.sort_by_key(|&c| rpo_idx[c]);
            for child in child_list {
                domtree_dfs(child, children, rpo_idx, result);
            }
        }
        domtree_dfs(0, &dom_children, &rpo_index, &mut domtree_rpo);

        let rpo: Vec<HirBasicBlock> = domtree_rpo.iter().map(|&id| bb_id.search_id(id)).collect();

        let mut out_degree: FxHashMap<HirBasicBlock, u32> = FxHashMap::default();
        let mut lowered_order = Vec::new();
        let mut block_succ = Vec::new();
        let mut block_succ_range: FxHashMap<HirBasicBlock, Range<usize>> = FxHashMap::default();

        // Record each original CFG successor in terminator successor order.
        for bb_layout in arena.f().layout().basicblocks() {
            let succ_start_index = block_succ.len();
            let bb = bb_layout.bb();
            let terminator = *bb_layout.insts().get_last().unwrap();
            let term_data = arena.inst_data(terminator);

            out_degree.entry(bb).or_insert(0);
            for succ in term_data.bb_usage() {
                *out_degree.get_mut(&bb).unwrap() += 1;
                block_succ.push(LoweredBlock::Orig { block: succ });
            }

            let succ_end_index = block_succ.len();
            block_succ_range.insert(bb, succ_start_index..succ_end_index);
        }

        let mut hlir_block_map = FxHashMap::default();
        for &bb in &rpo {
            let idx = MirBlockIndex::new(lowered_order.len());
            lowered_order.push(LoweredBlock::Orig { block: bb });
            hlir_block_map.insert(bb, idx);

            if out_degree[&bb] > 1 {
                let range = block_succ_range[&bb].clone();
                let succs = block_succ[range].iter_mut().enumerate();
                for (succ_idx, lowered_block) in succs {
                    let orig = lowered_block.orig_block().unwrap();
                    let terminator = *arena
                        .f()
                        .layout()
                        .basicblock(bb)
                        .insts()
                        .get_last()
                        .unwrap();
                    let args = outgoing_block_args(arena, terminator, succ_idx, orig);
                    let params = arena.f().bb_data(orig).params();
                    assert_eq!(
                        args.len(),
                        params.len(),
                        "edge arguments must match successor block parameters"
                    );
                    for (&arg, &param) in args.iter().zip(params) {
                        assert_eq!(
                            arena.inst_data(arg).ty(),
                            arena.inst_data(param).ty(),
                            "edge arguments must have the type of their successor block parameter"
                        );
                    }

                    // A multi-way terminator cannot own a parallel copy for just one
                    // outgoing edge. Materialize every value-carrying edge, even if
                    // the destination has only one predecessor.
                    if !args.is_empty() || !params.is_empty() {
                        let edge = LoweredBlock::Edge {
                            pred: bb,
                            succ: orig,
                            succ_idx: succ_idx as u32,
                        };
                        assert!(
                            !lowered_order.contains(&edge),
                            "each source successor index must produce a distinct edge block"
                        );
                        *lowered_block = edge;
                        lowered_order.push(edge);
                    }
                }
            }
        }

        let lb_index_map = FxHashMap::from_iter(
            lowered_order
                .iter()
                .enumerate()
                .map(|(idx, lb)| (lb, MirBlockIndex::new(idx))),
        );

        let mut lowered_succ_indices = Vec::new();
        let lowered_succ_ranges = Vec::from_iter(lowered_order.iter().map(|lb| {
            let start = lowered_succ_indices.len();
            let opt_inst = match lb {
                &LoweredBlock::Orig { block } => {
                    let range = block_succ_range[&block].clone();
                    lowered_succ_indices
                        .extend(block_succ[range].iter().map(|lb| lb_index_map[lb]));
                    let last = *arena
                        .f()
                        .layout()
                        .basicblock(block)
                        .insts()
                        .get_last()
                        .unwrap();

                    arena.is_branch(last).then_some(last)
                }
                &LoweredBlock::Edge { succ, .. } => {
                    let succ_index = lb_index_map[&LoweredBlock::Orig { block: succ }];
                    lowered_succ_indices.push(succ_index);
                    None
                }
            };
            let end = lowered_succ_indices.len();
            (opt_inst, start..end)
        }));

        BlockLoweringOrder {
            lowered_order,
            lowered_succ_indices,
            lowered_succ_ranges,
            hlir_block_map,
        }
    }

    pub fn lowered_order(&self) -> &[LoweredBlock] {
        &self.lowered_order[..]
    }

    pub fn lowered_index_for_block(&self, bb: HirBasicBlock) -> Option<MirBlockIndex> {
        self.hlir_block_map
            .get(&bb)
            .filter(|block| block.is_valid())
            .copied()
    }

    pub fn succ_indices(&self, block: MirBlockIndex) -> (Option<HirInst>, &[MirBlockIndex]) {
        let (opt_inst, range) = &self.lowered_succ_ranges[block.index()];
        (*opt_inst, &self.lowered_succ_indices[range.clone()])
    }
}

fn outgoing_block_args(
    arena: ArenaContext<'_>,
    terminator: HirInst,
    succ_idx: usize,
    expected_succ: HirBasicBlock,
) -> Vec<HirInst> {
    match arena.inst_data(terminator).kind() {
        InstKind::Branch(branch) => match succ_idx {
            0 => {
                assert_eq!(branch.t_target(), expected_succ);
                branch.t_args().to_vec()
            }
            1 => {
                assert_eq!(branch.f_target(), expected_succ);
                branch.f_args().to_vec()
            }
            _ => unreachable!("branch has exactly two successors"),
        },
        InstKind::Jump(jump) => {
            assert_eq!(succ_idx, 0, "jump has exactly one successor");
            assert_eq!(jump.target(), expected_succ);
            jump.args().to_vec()
        }
        _ => unreachable!("CFG successor must come from a branch or jump terminator"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use raana_ir::ir::{Program, arena::Arena};
    use raana_ir::opt::prelude::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder};

    fn add_block(data: &mut HirFunctionData, name: &str, params: Vec<HirType>) -> HirBasicBlock {
        let block = data.new_basic_block().basic_block(name.to_owned(), params);
        data.layout_mut().push_bb_back(block);
        block
    }

    fn order_for(program: &HirProgram, func: HirFunction) -> BlockLoweringOrder {
        BlockLoweringOrder::new(ArenaContext {
            program,
            curr_func: Some(func),
        })
    }

    #[test]
    fn keeps_distinct_branch_edges_to_the_same_parameterized_target() {
        let mut program = Program::new();
        let func = program.new_function(HirType::get_i32(), "diamond".to_owned(), vec![]);
        let (entry, merge, branch) = {
            let data = program.func_data_mut(func);
            let entry = add_block(data, "entry", vec![]);
            let merge = add_block(data, "merge", vec![HirType::get_i32()]);
            let (cond, true_value, false_value, branch) = {
                let mut builder = data.new_local_inst();
                let cond = builder.integer(1);
                let true_value = builder.integer(10);
                let false_value = builder.integer(20);
                let branch =
                    builder.branch(cond, merge, vec![true_value], merge, vec![false_value]);
                (cond, true_value, false_value, branch)
            };
            let _ = (cond, true_value, false_value);
            data.layout_mut().insert_inst(entry, branch);
            let param = data.bb_data(merge).params()[0];
            let ret = data.new_local_inst().ret(Some(param));
            data.layout_mut().insert_inst(merge, ret);
            (entry, merge, branch)
        };

        let order = order_for(&program, func);
        let entry_index = order.lowered_index_for_block(entry).unwrap();
        let (terminator, successors) = order.succ_indices(entry_index);

        assert_eq!(terminator, Some(branch));
        assert_eq!(successors.len(), 2);
        assert_ne!(successors[0], successors[1]);
        assert_eq!(
            order.lowered_order()[successors[0].index()],
            LoweredBlock::Edge {
                pred: entry,
                succ: merge,
                succ_idx: 0,
            }
        );
        assert_eq!(
            order.lowered_order()[successors[1].index()],
            LoweredBlock::Edge {
                pred: entry,
                succ: merge,
                succ_idx: 1,
            }
        );
    }

    #[test]
    fn keeps_return_out_of_branch_metadata() {
        let mut program = Program::new();
        let func = program.new_function(HirType::get_i32(), "returning".to_owned(), vec![]);
        let entry = {
            let data = program.func_data_mut(func);
            let entry = add_block(data, "entry", vec![]);
            let value = data.new_local_inst().integer(42);
            let ret = data.new_local_inst().ret(Some(value));
            data.layout_mut().insert_inst(entry, ret);
            entry
        };

        let order = order_for(&program, func);
        let entry_index = order.lowered_index_for_block(entry).unwrap();
        let (branch, successors) = order.succ_indices(entry_index);

        assert_eq!(branch, None, "return is not a CFG branch");
        assert!(successors.is_empty(), "return has no CFG successors");
    }

    #[test]
    fn preserves_loop_continue_break_and_critical_edges() {
        let mut program = Program::new();
        let func = program.new_function(HirType::get_i32(), "loop".to_owned(), vec![]);
        let (entry, header, body, exit) = {
            let data = program.func_data_mut(func);
            let entry = add_block(data, "entry", vec![]);
            let header = add_block(data, "header", vec![HirType::get_i32()]);
            let body = add_block(data, "body", vec![]);
            let exit = add_block(data, "exit", vec![]);

            let initial = data.new_local_inst().integer(0);
            let entry_jump = data.new_local_inst().jump(header, vec![initial]);
            data.layout_mut().insert_inst(entry, entry_jump);

            let cond = data.new_local_inst().integer(1);
            let header_branch = data
                .new_local_inst()
                .branch(cond, body, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, header_branch);

            let next = data.new_local_inst().integer(1);
            let body_cond = data.new_local_inst().integer(1);
            let body_branch =
                data.new_local_inst()
                    .branch(body_cond, header, vec![next], exit, vec![]);
            data.layout_mut().insert_inst(body, body_branch);

            let result = data.new_local_inst().integer(0);
            let ret = data.new_local_inst().ret(Some(result));
            data.layout_mut().insert_inst(exit, ret);
            (entry, header, body, exit)
        };

        let order = order_for(&program, func);
        let body_index = order.lowered_index_for_block(body).unwrap();
        let header_index = order.lowered_index_for_block(header).unwrap();
        let (_, body_successors) = order.succ_indices(body_index);
        let (_, header_successors) = order.succ_indices(header_index);

        assert_eq!(body_successors.len(), 2);
        assert_eq!(
            order.lowered_order()[body_successors[0].index()],
            LoweredBlock::Edge {
                pred: body,
                succ: header,
                succ_idx: 0,
            },
            "continue carries the loop parameter through an edge-owned transfer"
        );
        assert_eq!(
            order.lowered_order()[body_successors[1].index()],
            LoweredBlock::Orig { block: exit },
            "break remains a direct edge when it has no block arguments"
        );
        assert_eq!(header_successors.len(), 2);
        assert!(
            header_successors
                .iter()
                .any(|successor| order.lowered_order()[successor.index()]
                    == LoweredBlock::Orig { block: exit }),
            "the header-to-exit critical edge remains represented independently"
        );
        assert!(order.lowered_index_for_block(entry).is_some());
    }
}
