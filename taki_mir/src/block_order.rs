//! The order of we traversing a series of basicblock should be RPO of dominance tree.
use std::ops::Range;

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
    fn new(arena: ArenaContext<'_>, rpo: &[HirBasicBlock]) -> BlockLoweringOrder {
        assert!(arena.curr_func.is_some());
        let mut in_degree: FxHashMap<HirBasicBlock, u32> = FxHashMap::default();
        let mut out_degree: FxHashMap<HirBasicBlock, u32> = FxHashMap::default();
        let mut lowered_order = Vec::new();
        let mut block_succ = Vec::new();
        let mut block_succ_range: FxHashMap<HirBasicBlock, Range<usize>> = FxHashMap::default();

        // Count in-degree and out-degree
        for bb_layout in arena.f().layout().basicblocks() {
            let succ_start_index = block_succ.len();
            let bb = bb_layout.bb();
            let terminator = *bb_layout.insts().get_last().unwrap();
            let term_data = arena.inst_data(terminator);

            for succ in term_data.bb_usage() {
                *out_degree.get_mut(&bb).unwrap() += 1;
                *in_degree.get_mut(&succ).unwrap() += 1;
                block_succ.push(LoweredBlock::Orig { block: succ });
            }

            let succ_end_index = block_succ.len();
            block_succ_range.insert(bb, succ_start_index..succ_end_index);
        }

        let mut hlir_block_map = FxHashMap::default();
        for &bb in rpo {
            let idx = MirBlockIndex::new(lowered_order.len());
            lowered_order.push(LoweredBlock::Orig { block: bb });
            hlir_block_map.insert(bb, idx);

            if out_degree[&bb] > 1 {
                let range = block_succ_range[&bb].clone();
                let succs = block_succ[range].iter_mut().enumerate();
                for (succ_idx, lowered_block) in succs {
                    let orig = lowered_block.orig_block().unwrap();
                    if in_degree[&orig] > 1 {
                        *lowered_block = LoweredBlock::Edge {
                            pred: bb,
                            succ: orig,
                            succ_idx: succ_idx as u32,
                        }
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
