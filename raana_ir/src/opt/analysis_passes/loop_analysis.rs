use itertools::Itertools;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use crate::opt::{analysis_passes::dom_tree::v2::DominanceTree, prelude::*, utils::cfg::CFG};

pub struct Loop {
    header: BasicBlock,
    body: FxHashSet<BasicBlock>,
    latches: SmallVec<[BasicBlock; 2]>,
}

impl Loop {
    pub fn header(&self) -> BasicBlock {
        self.header
    }

    pub fn body(&self) -> &FxHashSet<BasicBlock> {
        &self.body
    }

    pub fn latches(&self) -> &[BasicBlock] {
        &self.latches
    }

    pub fn contains(&self, block: BasicBlock) -> bool {
        self.body.contains(&block)
    }

    pub fn get_preheader(&self, cfg: &utils::cfg::CFG) -> Option<BasicBlock> {
        let candidate = *cfg
            .predecessors_of(self.header)
            .iter()
            .filter(|&&b| !self.contains(b))
            .exactly_one()
            .ok()?;
        let _preds_succ = *cfg.successors_of(candidate).iter().exactly_one().ok()?;
        Some(candidate)
    }
}

pub struct LoopAnalysis {
    /// Storage place.
    /// The order of loops is sorted by its size, from small to big.
    loops: Vec<Loop>,

    /// `direct_parent[i] == j`, means `loops[i]`'s direct parent is `loops[j]`
    /// if `i == j`, means `loops[i]` is one of the root in the loop forest.
    direct_parent: Vec<usize>,

    /// Each loop owns a unique header, so we use it to find the index of `loops`.
    loop_index: FxHashMap<BasicBlock, usize>,

    /// `block_to_inner_loop[&bb] == i`, means the smallest loop contains the `bb` is `loops[i]`
    block_to_inner_loop: FxHashMap<BasicBlock, usize>,
}

impl LoopAnalysis {
    pub fn loops(&self) -> &[Loop] {
        &self.loops
    }

    pub fn min_loop_contain(&self, block: BasicBlock) -> Option<&Loop> {
        self.min_loop_contain_index(block)
            .map(|index| &self.loops[index])
    }

    /// Returns the index of the innermost loop containing `block`.
    pub fn min_loop_contain_index(&self, block: BasicBlock) -> Option<usize> {
        self.block_to_inner_loop.get(&block).copied()
    }

    /// Returns the analysis-local index of the loop with `header`.
    pub fn loop_index(&self, header: BasicBlock) -> Option<usize> {
        self.loop_index.get(&header).copied()
    }

    /// Returns the direct parent index, or `None` when `index` is a root loop.
    pub fn parent_loop_index(&self, index: usize) -> Option<usize> {
        let parent = self.direct_parent[index];
        (parent != index).then_some(parent)
    }

    /// Returns the direct parent, or `None` when `index` is a root loop.
    pub fn parent_loop(&self, index: usize) -> Option<&Loop> {
        self.parent_loop_index(index)
            .map(|parent| &self.loops[parent])
    }

    /// Iterates loop indices containing `block`, from innermost to outermost.
    pub fn containing_loop_indices(&self, block: BasicBlock) -> impl Iterator<Item = usize> + '_ {
        std::iter::successors(self.min_loop_contain_index(block), |&index| {
            self.parent_loop_index(index)
        })
    }

    /// Iterates loops containing `block`, from innermost to outermost.
    pub fn containing_loops(&self, block: BasicBlock) -> impl Iterator<Item = &Loop> + '_ {
        self.containing_loop_indices(block)
            .map(|index| &self.loops[index])
    }

    pub fn new(data: &FunctionData) -> (CFG, DominanceTree, LoopAnalysis) {
        let cfg = utils::cfg::CFG::new(data).unwrap();
        Self::from_cfg(cfg)
    }

    pub fn from_cfg(cfg: CFG) -> (CFG, DominanceTree, LoopAnalysis) {
        let dom_tree = dom_tree::v2::DominanceTree::from_cfg(&cfg);
        let mut back_edges: FxHashMap<BasicBlock, SmallVec<[BasicBlock; 2]>> = FxHashMap::default();
        for edge in cfg.edges() {
            if dom_tree.dominates(edge.dst, edge.src) {
                back_edges.entry(edge.dst).or_default().push(edge.src);
            }
        }
        #[cfg(debug_assertions)]
        assert_reducible(&cfg, &dom_tree);

        let mut loops = vec![];
        for (header, latches) in back_edges {
            for &latch in &latches {
                debug_assert!(
                    cfg.successors_of(latch).contains(&header),
                    "loop latch must have an edge to its header"
                );
                debug_assert!(
                    dom_tree.dominates(header, latch),
                    "loop header must dominate every latch"
                );
            }

            // A self-loop has the header itself as its latch and needs no
            // predecessor walk beyond the header.
            let mut worklist =
                VecDeque::from_iter(latches.iter().filter(|&&block| block != header).copied());
            let mut body = FxHashSet::from_iter(latches.iter().copied());
            body.insert(header);
            while let Some(block) = worklist.pop_front() {
                for &pred in cfg.predecessors_of(block) {
                    // prevent re-explore and over-explore (beyond header block)
                    if body.insert(pred) {
                        debug_assert!(
                            dom_tree.dominates(header, pred),
                            "natural loop contains a block not dominated by its header"
                        );
                        worklist.push_back(pred);
                    }
                }
            }
            debug_assert!(body.contains(&header));
            debug_assert!(latches.iter().all(|latch| body.contains(latch)));
            debug_assert!(
                body.iter().all(|&block| dom_tree.dominates(header, block)),
                "loop header must dominate every block in its natural loop"
            );
            loops.push(Loop {
                header,
                body,
                latches,
            });
        }

        // sort the loops from small to big.
        loops.sort_unstable_by_key(|l| l.body.len());
        #[cfg(debug_assertions)]
        assert_laminar(&loops);

        let mut block_to_inner_loop = FxHashMap::default();
        block_to_inner_loop.reserve(cfg.block_count());
        let mut direct_parent = Vec::with_capacity(loops.len());
        for i in 0..loops.len() {
            direct_parent.push(i);
        }
        for (i, l1) in loops.iter().enumerate() {
            l1.body.iter().for_each(|&bb| {
                block_to_inner_loop.entry(bb).or_insert(i);
            });
            if let Some((j, _)) = loops
                .iter()
                .enumerate()
                .skip(i + 1)
                .find(|&(_j, l2)| l2.contains(l1.header))
            {
                direct_parent[i] = j;
            }
        }
        let loop_index = FxHashMap::from_iter(loops.iter().enumerate().map(|(i, l)| (l.header, i)));
        #[cfg(debug_assertions)]
        debug_assert_eq!(
            loop_index.len(),
            loops.len(),
            "each natural loop must have a unique header"
        );

        let analysis = LoopAnalysis {
            loops,
            direct_parent,
            loop_index,
            block_to_inner_loop,
        };
        #[cfg(debug_assertions)]
        analysis.verify(&cfg, &dom_tree);
        (cfg, dom_tree, analysis)
    }

    #[cfg(debug_assertions)]
    fn verify(&self, cfg: &utils::cfg::CFG, dom_tree: &dom_tree::v2::DominanceTree) {
        debug_assert_eq!(self.direct_parent.len(), self.loops.len());
        debug_assert_eq!(self.loop_index.len(), self.loops.len());

        for (index, looop) in self.loops.iter().enumerate() {
            debug_assert_eq!(self.loop_index.get(&looop.header), Some(&index));
            debug_assert!(looop.body.contains(&looop.header));
            debug_assert!(!looop.latches.is_empty());
            for &latch in &looop.latches {
                debug_assert!(looop.body.contains(&latch));
                debug_assert!(cfg.successors_of(latch).contains(&looop.header));
            }
            debug_assert!(
                looop
                    .body
                    .iter()
                    .all(|&block| dom_tree.dominates(looop.header, block))
            );

            let parent = self.direct_parent[index];
            if parent == index {
                debug_assert!(
                    self.loops
                        .iter()
                        .enumerate()
                        .all(|(other, candidate)| other == index
                            || !looop
                                .body
                                .iter()
                                .all(|block| candidate.body.contains(block))),
                    "a root loop must not be contained in another loop"
                );
            } else {
                debug_assert!(parent < self.loops.len());
                let parent_loop = &self.loops[parent];
                debug_assert!(looop.body.len() < parent_loop.body.len());
                debug_assert!(
                    looop
                        .body
                        .iter()
                        .all(|block| parent_loop.body.contains(block))
                );
                debug_assert!(
                    self.loops
                        .iter()
                        .enumerate()
                        .all(|(candidate_index, candidate)| {
                            candidate_index == index
                                || candidate_index == parent
                                || !looop
                                    .body
                                    .iter()
                                    .all(|block| candidate.body.contains(block))
                                || parent_loop.body.len() <= candidate.body.len()
                        }),
                    "direct parent must be the smallest loop containing its child"
                );
            }
        }

        for (&block, &index) in &self.block_to_inner_loop {
            debug_assert!(index < self.loops.len());
            debug_assert!(self.loops[index].contains(block));
            debug_assert!(
                self.loops.iter().enumerate().all(|(other, candidate)| {
                    !candidate.contains(block)
                        || self.loops[index].body.len() <= candidate.body.len()
                        || other == index
                }),
                "block must map to its innermost loop"
            );
        }
    }
}

#[cfg(debug_assertions)]
fn assert_laminar(loops: &[Loop]) {
    for (index, lhs) in loops.iter().enumerate() {
        for rhs in loops.iter().skip(index + 1) {
            let overlaps = lhs.body.iter().any(|block| rhs.body.contains(block));
            if !overlaps {
                continue;
            }
            let lhs_in_rhs = lhs.body.iter().all(|block| rhs.body.contains(block));
            let rhs_in_lhs = rhs.body.iter().all(|block| lhs.body.contains(block));
            debug_assert!(
                lhs_in_rhs || rhs_in_lhs,
                "natural loops in a reducible CFG must be nested or disjoint"
            );
            debug_assert_ne!(
                lhs.body, rhs.body,
                "distinct natural loops must not have identical block sets"
            );
        }
    }
}

#[cfg(debug_assertions)]
fn assert_reducible(cfg: &utils::cfg::CFG, dom_tree: &dom_tree::v2::DominanceTree) {
    debug_assert_eq!(cfg.entry(), dom_tree.entry());
    debug_assert!(cfg.blocks().iter().all(|&block| dom_tree.contains(block)));

    let mut indegrees = FxHashMap::default();
    for &block in cfg.blocks() {
        indegrees.insert(block, 0usize);
    }
    for edge in cfg.edges() {
        if !dom_tree.dominates(edge.dst, edge.src) {
            indegrees.entry(edge.dst).and_modify(|degree| *degree += 1);
        }
    }

    let mut worklist = VecDeque::from_iter(
        indegrees
            .iter()
            .filter_map(|(&block, &degree)| (degree == 0).then_some(block)),
    );
    let mut visited = 0;
    while let Some(block) = worklist.pop_front() {
        visited += 1;
        for &successor in cfg.successors_of(block) {
            if dom_tree.dominates(successor, block) {
                continue;
            }
            let degree = indegrees
                .get_mut(&successor)
                .expect("every successor must be reachable");
            *degree -= 1;
            if *degree == 0 {
                worklist.push_back(successor);
            }
        }
    }
    debug_assert_eq!(
        visited,
        cfg.block_count(),
        "irreducible CFG is not supported by natural loop analysis"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NestedLoops {
        outer_header: BasicBlock,
        inner_header: BasicBlock,
        inner_body: BasicBlock,
        outer_latch: BasicBlock,
        exit: BasicBlock,
    }

    fn build_nested_loops() -> (Program, Function, NestedLoops) {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "nested_loops".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let outer_header = data
            .new_basic_block()
            .basic_block("outer_header".into(), vec![]);
        let inner_header = data
            .new_basic_block()
            .basic_block("inner_header".into(), vec![]);
        let inner_body = data
            .new_basic_block()
            .basic_block("inner_body".into(), vec![]);
        let after_inner = data
            .new_basic_block()
            .basic_block("after_inner".into(), vec![]);
        let outer_latch = data
            .new_basic_block()
            .basic_block("outer_latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [
            outer_header,
            inner_header,
            inner_body,
            after_inner,
            outer_latch,
            exit,
        ] {
            data.layout_mut().push_bb_back(block);
        }

        let condition = data.new_local_inst().integer(1);
        let entry_jump = data.new_local_inst().jump(outer_header, vec![]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let outer_branch =
            data.new_local_inst()
                .branch(condition, inner_header, vec![], exit, vec![]);
        data.layout_mut().insert_inst(outer_header, outer_branch);
        let inner_branch =
            data.new_local_inst()
                .branch(condition, inner_body, vec![], after_inner, vec![]);
        data.layout_mut().insert_inst(inner_header, inner_branch);
        let inner_backedge = data.new_local_inst().jump(inner_header, vec![]);
        data.layout_mut().insert_inst(inner_body, inner_backedge);
        let after_inner_jump = data.new_local_inst().jump(outer_latch, vec![]);
        data.layout_mut().insert_inst(after_inner, after_inner_jump);
        let outer_backedge = data.new_local_inst().jump(outer_header, vec![]);
        data.layout_mut().insert_inst(outer_latch, outer_backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        (
            program,
            function,
            NestedLoops {
                outer_header,
                inner_header,
                inner_body,
                outer_latch,
                exit,
            },
        )
    }

    #[test]
    fn exposes_direct_parent_indices_and_loops() {
        let (program, function, blocks) = build_nested_loops();
        let (_cfg, _dom_tree, loops) = LoopAnalysis::new(program.func_data(function));
        let inner = loops.loop_index(blocks.inner_header).unwrap();
        let outer = loops.loop_index(blocks.outer_header).unwrap();

        assert_eq!(loops.parent_loop_index(inner), Some(outer));
        assert_eq!(
            loops.parent_loop(inner).map(Loop::header),
            Some(blocks.outer_header)
        );
        assert_eq!(loops.parent_loop_index(outer), None);
        assert!(loops.parent_loop(outer).is_none());
    }

    #[test]
    fn lists_containing_loops_from_inner_to_outer() {
        let (program, function, blocks) = build_nested_loops();
        let (_cfg, _dom_tree, loops) = LoopAnalysis::new(program.func_data(function));
        let inner = loops.loop_index(blocks.inner_header).unwrap();
        let outer = loops.loop_index(blocks.outer_header).unwrap();

        assert_eq!(loops.min_loop_contain_index(blocks.inner_body), Some(inner));
        assert_eq!(
            loops
                .containing_loop_indices(blocks.inner_body)
                .collect::<Vec<_>>(),
            vec![inner, outer]
        );
        assert_eq!(
            loops
                .containing_loops(blocks.inner_body)
                .map(Loop::header)
                .collect::<Vec<_>>(),
            vec![blocks.inner_header, blocks.outer_header]
        );
        assert_eq!(
            loops
                .containing_loop_indices(blocks.outer_latch)
                .collect::<Vec<_>>(),
            vec![outer]
        );
        assert_eq!(loops.min_loop_contain_index(blocks.exit), None);
        assert_eq!(loops.containing_loops(blocks.exit).count(), 0);
    }
}
