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
        self.block_to_inner_loop
            .get(&block)
            .map(|&i| &self.loops[i])
    }

    pub fn new(data: &FunctionData) -> (CFG, DominanceTree, LoopAnalysis) {
        let cfg = utils::cfg::CFG::new(data).unwrap();
        let dom_tree = dom_tree::v2::DominanceTree::from_cfg(&cfg);
        let mut back_edges: FxHashMap<BasicBlock, SmallVec<[BasicBlock; 2]>> = FxHashMap::default();
        for edge in cfg.edges() {
            if dom_tree.dominates(edge.dst, edge.src) {
                back_edges.entry(edge.dst).or_default().push(edge.src);
            }
        }
        assert_reducible(&cfg, &dom_tree);

        let mut loops = vec![];
        for (header, latches) in back_edges {
            for &latch in &latches {
                assert!(
                    cfg.successors_of(latch).contains(&header),
                    "loop latch must have an edge to its header"
                );
                assert!(
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
                        assert!(
                            dom_tree.dominates(header, pred),
                            "natural loop contains a block not dominated by its header"
                        );
                        worklist.push_back(pred);
                    }
                }
            }
            assert!(body.contains(&header));
            assert!(latches.iter().all(|latch| body.contains(latch)));
            assert!(
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
        assert_laminar(&loops);

        let mut direct_parent = Vec::with_capacity(loops.len());
        let mut block_to_inner_loop = FxHashMap::default();
        block_to_inner_loop.reserve(cfg.block_count());
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
                .find(|&(_j, l2)| l1.body.iter().all(|block| l2.body.contains(block)))
            {
                direct_parent[i] = j;
            }
        }
        let loop_index = FxHashMap::from_iter(loops.iter().enumerate().map(|(i, l)| (l.header, i)));
        assert_eq!(
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
        analysis.verify(&cfg, &dom_tree);
        (cfg, dom_tree, analysis)
    }

    fn verify(&self, cfg: &utils::cfg::CFG, dom_tree: &dom_tree::v2::DominanceTree) {
        assert_eq!(self.direct_parent.len(), self.loops.len());
        assert_eq!(self.loop_index.len(), self.loops.len());

        for (index, looop) in self.loops.iter().enumerate() {
            assert_eq!(self.loop_index.get(&looop.header), Some(&index));
            assert!(looop.body.contains(&looop.header));
            assert!(!looop.latches.is_empty());
            for &latch in &looop.latches {
                assert!(looop.body.contains(&latch));
                assert!(cfg.successors_of(latch).contains(&looop.header));
            }
            assert!(
                looop
                    .body
                    .iter()
                    .all(|&block| dom_tree.dominates(looop.header, block))
            );

            let parent = self.direct_parent[index];
            if parent == index {
                assert!(
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
                assert!(parent < self.loops.len());
                let parent_loop = &self.loops[parent];
                assert!(looop.body.len() < parent_loop.body.len());
                assert!(
                    looop
                        .body
                        .iter()
                        .all(|block| parent_loop.body.contains(block))
                );
                assert!(
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
            assert!(index < self.loops.len());
            assert!(self.loops[index].contains(block));
            assert!(
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

fn assert_laminar(loops: &[Loop]) {
    for (index, lhs) in loops.iter().enumerate() {
        for rhs in loops.iter().skip(index + 1) {
            let overlaps = lhs.body.iter().any(|block| rhs.body.contains(block));
            if !overlaps {
                continue;
            }
            let lhs_in_rhs = lhs.body.iter().all(|block| rhs.body.contains(block));
            let rhs_in_lhs = rhs.body.iter().all(|block| lhs.body.contains(block));
            assert!(
                lhs_in_rhs || rhs_in_lhs,
                "natural loops in a reducible CFG must be nested or disjoint"
            );
            assert_ne!(
                lhs.body, rhs.body,
                "distinct natural loops must not have identical block sets"
            );
        }
    }
}

fn assert_reducible(cfg: &utils::cfg::CFG, dom_tree: &dom_tree::v2::DominanceTree) {
    assert_eq!(cfg.entry(), dom_tree.entry());
    assert!(cfg.blocks().iter().all(|&block| dom_tree.contains(block)));

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
    assert_eq!(
        visited,
        cfg.block_count(),
        "irreducible CFG is not supported by natural loop analysis"
    );
}
