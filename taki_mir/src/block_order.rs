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

fn outgoing_block_args<'a>(
    arena: ArenaContext<'a>,
    terminator: HirInst,
    succ_idx: usize,
    expected_succ: HirBasicBlock,
) -> &'a [HirInst] {
    match arena.inst_data(terminator).kind() {
        InstKind::Branch(branch) => match succ_idx {
            0 => {
                assert_eq!(branch.t_target(), expected_succ);
                branch.t_args()
            }
            1 => {
                assert_eq!(branch.f_target(), expected_succ);
                branch.f_args()
            }
            _ => unreachable!("branch has exactly two successors"),
        },
        InstKind::Jump(jump) => {
            assert_eq!(succ_idx, 0, "jump has exactly one successor");
            assert_eq!(jump.target(), expected_succ);
            jump.args()
        }
        _ => unreachable!("CFG successor must come from a branch or jump terminator"),
    }
}
