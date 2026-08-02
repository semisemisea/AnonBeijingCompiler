use crate::opt::prelude::*;

pub fn idom(prece: &CFGGraph, rpo: &[BId]) -> IDomMap {
    fn lca(n1: BId, n2: BId, map: &IDomMap, rpo_idx: &[BId]) -> BId {
        let mut p1 = n1;
        let mut p2 = n2;
        while p1 != p2 {
            while rpo_idx[p1] > rpo_idx[p2] {
                p1 = map[p1];
            }
            while rpo_idx[p1] < rpo_idx[p2] {
                p2 = map[p2];
            }
        }
        p1
    }

    let mut map = IDomMap::new();
    map.resize(rpo.len(), usize::MAX);
    debug!("rpo before panic: {:?}", rpo);
    let mut rpo_idx = vec![0; rpo.len()];
    for (i, &id) in rpo.iter().enumerate() {
        rpo_idx[id] = i;
    }

    map[0] = 0;

    let mut converged = false;
    while !converged {
        converged = true;
        for node in &rpo[1..] {
            let mut it = prece[node].iter();
            let mut new_idom = *it.find(|&&x| map[x] != usize::MAX).unwrap();
            for &other_node in it.filter(|&&x| map[x] != usize::MAX) {
                new_idom = lca(new_idom, other_node, &map, rpo);
            }
            if map[*node] != new_idom {
                map[*node] = new_idom;
                converged = false;
            }
        }
    }
    map
}

#[must_use]
pub fn build_dominance_tree(idom_map: &IDomMap, rpo_len: usize) -> DomTree {
    let mut ret = vec![vec![]; rpo_len];
    // INFO: remember that idom_map we make `idom_map[0] = 0`
    // that is not allowed in a tree (no loop or ring)
    for (vid, &pa) in idom_map.iter().enumerate().skip(1) {
        ret[pa].push(vid);
    }
    ret
}

pub struct DominanceTree {
    old_id_alloc: BIDAlloc,
    idom_edges: Vec<BasicBlock>,
}

pub mod v2 {
    use rustc_hash::FxHashMap;
    use smallvec::SmallVec;

    use crate::{
        ir::{BasicBlock, FunctionData},
        opt::utils::cfg::CFG,
    };

    pub type DomTreeChildren = SmallVec<[BasicBlock; 4]>;

    /// Dominance information for the reachable blocks of one function CFG.
    ///
    /// The entry has no immediate dominator in the public representation.
    /// Analysis results are tied to the CFG snapshot used to construct them and
    /// must be discarded after a control-flow mutation.
    #[derive(Debug, Clone)]
    pub struct DominanceTree {
        entry: BasicBlock,
        immediate_dominators: FxHashMap<BasicBlock, Option<BasicBlock>>,
        children: FxHashMap<BasicBlock, DomTreeChildren>,
        depths: FxHashMap<BasicBlock, usize>,
        dfs_in: FxHashMap<BasicBlock, usize>,
        dfs_out: FxHashMap<BasicBlock, usize>,
    }

    impl DominanceTree {
        /// Analyze a function definition. Function declarations have no CFG and
        /// therefore return `None`.
        pub fn new(data: &FunctionData) -> Option<Self> {
            CFG::new(data).map(|cfg| Self::from_cfg(&cfg))
        }

        /// Analyze an existing CFG using the Cooper-Harvey-Kennedy iterative
        /// immediate-dominator algorithm.
        pub fn from_cfg(cfg: &CFG) -> Self {
            let entry = cfg.entry();
            let reverse_postorder = cfg.reverse_postorder();
            assert_eq!(reverse_postorder.first(), Some(&entry));

            let rpo_indices = reverse_postorder
                .iter()
                .enumerate()
                .map(|(index, &block)| (block, index))
                .collect::<FxHashMap<_, _>>();

            // The entry self-dominates only as an internal algorithm sentinel.
            // It is converted to `None` in the public result below.
            let mut algorithm_idoms = FxHashMap::default();
            algorithm_idoms.insert(entry, entry);

            loop {
                let mut changed = false;
                for &block in reverse_postorder.iter().skip(1) {
                    let mut processed_predecessors = cfg
                        .predecessors_of(block)
                        .iter()
                        .copied()
                        .filter(|predecessor| algorithm_idoms.contains_key(predecessor));
                    let Some(first_predecessor) = processed_predecessors.next() else {
                        continue;
                    };

                    let new_idom = processed_predecessors.fold(first_predecessor, |idom, pred| {
                        intersect(pred, idom, &algorithm_idoms, &rpo_indices)
                    });
                    if algorithm_idoms.get(&block) != Some(&new_idom) {
                        algorithm_idoms.insert(block, new_idom);
                        changed = true;
                    }
                }
                if !changed {
                    break;
                }
            }

            assert_eq!(
                algorithm_idoms.len(),
                cfg.block_count(),
                "every reachable non-entry block must acquire an immediate dominator"
            );

            let mut immediate_dominators = FxHashMap::default();
            let mut children = FxHashMap::default();
            for &block in reverse_postorder {
                children.insert(block, DomTreeChildren::new());
                let idom = (block != entry).then(|| algorithm_idoms[&block]);
                immediate_dominators.insert(block, idom);
            }
            for &block in reverse_postorder.iter().skip(1) {
                let parent = algorithm_idoms[&block];
                children
                    .get_mut(&parent)
                    .expect("an immediate dominator must be reachable")
                    .push(block);
            }

            let mut depths = FxHashMap::default();
            let mut dfs_in = FxHashMap::default();
            let mut dfs_out = FxHashMap::default();
            number_tree(entry, &children, &mut depths, &mut dfs_in, &mut dfs_out);

            let tree = Self {
                entry,
                immediate_dominators,
                children,
                depths,
                dfs_in,
                dfs_out,
            };
            #[cfg(debug_assertions)]
            tree.verify(cfg);
            tree
        }

        pub fn entry(&self) -> BasicBlock {
            self.entry
        }

        pub fn contains(&self, block: BasicBlock) -> bool {
            self.immediate_dominators.contains_key(&block)
        }

        pub fn immediate_dominator(&self, block: BasicBlock) -> Option<BasicBlock> {
            *self
                .immediate_dominators
                .get(&block)
                .unwrap_or_else(|| panic!("basic block {block:?} is not in this dominance tree"))
        }

        pub fn children_of(&self, block: BasicBlock) -> &[BasicBlock] {
            self.children
                .get(&block)
                .map(DomTreeChildren::as_slice)
                .unwrap_or_else(|| panic!("basic block {block:?} is not in this dominance tree"))
        }

        pub fn depth_of(&self, block: BasicBlock) -> usize {
            *self
                .depths
                .get(&block)
                .unwrap_or_else(|| panic!("basic block {block:?} is not in this dominance tree"))
        }

        pub fn dominates(&self, dominator: BasicBlock, block: BasicBlock) -> bool {
            let dominator_in = self.dfs_in.get(&dominator).unwrap_or_else(|| {
                panic!("basic block {dominator:?} is not in this dominance tree")
            });
            let block_in = self
                .dfs_in
                .get(&block)
                .unwrap_or_else(|| panic!("basic block {block:?} is not in this dominance tree"));
            dominator_in <= block_in && self.dfs_out[&block] <= self.dfs_out[&dominator]
        }

        pub fn strictly_dominates(&self, dominator: BasicBlock, block: BasicBlock) -> bool {
            dominator != block && self.dominates(dominator, block)
        }

        #[cfg(debug_assertions)]
        fn verify(&self, cfg: &CFG) {
            debug_assert_eq!(self.entry, cfg.entry());
            debug_assert_eq!(self.immediate_dominators.len(), cfg.block_count());
            debug_assert_eq!(self.children.len(), cfg.block_count());
            debug_assert_eq!(self.depths.len(), cfg.block_count());
            debug_assert_eq!(self.dfs_in.len(), cfg.block_count());
            debug_assert_eq!(self.dfs_out.len(), cfg.block_count());
            debug_assert_eq!(self.immediate_dominator(self.entry), None);
            debug_assert_eq!(self.depth_of(self.entry), 0);

            for &block in cfg.blocks() {
                debug_assert!(self.dominates(block, block));
                if block == self.entry {
                    continue;
                }
                let idom = self
                    .immediate_dominator(block)
                    .expect("every reachable non-entry block must have an idom");
                debug_assert!(self.strictly_dominates(idom, block));
                debug_assert_eq!(self.depth_of(block), self.depth_of(idom) + 1);
                debug_assert!(self.children_of(idom).contains(&block));

                let mut ancestor = block;
                for _ in 0..cfg.block_count() {
                    if ancestor == self.entry {
                        break;
                    }
                    ancestor = self
                        .immediate_dominator(ancestor)
                        .expect("only the entry may lack an idom");
                }
                debug_assert_eq!(
                    ancestor, self.entry,
                    "immediate-dominator chain must reach the entry"
                );
            }
        }
    }

    fn intersect(
        mut lhs: BasicBlock,
        mut rhs: BasicBlock,
        idoms: &FxHashMap<BasicBlock, BasicBlock>,
        rpo_indices: &FxHashMap<BasicBlock, usize>,
    ) -> BasicBlock {
        while lhs != rhs {
            while rpo_indices[&lhs] > rpo_indices[&rhs] {
                lhs = idoms[&lhs];
            }
            while rpo_indices[&rhs] > rpo_indices[&lhs] {
                rhs = idoms[&rhs];
            }
        }
        lhs
    }

    fn number_tree(
        entry: BasicBlock,
        children: &FxHashMap<BasicBlock, DomTreeChildren>,
        depths: &mut FxHashMap<BasicBlock, usize>,
        dfs_in: &mut FxHashMap<BasicBlock, usize>,
        dfs_out: &mut FxHashMap<BasicBlock, usize>,
    ) {
        #[derive(Clone, Copy)]
        enum Visit {
            Enter(BasicBlock, usize),
            Exit(BasicBlock),
        }

        let mut timestamp = 0;
        let mut stack = vec![Visit::Enter(entry, 0)];
        while let Some(visit) = stack.pop() {
            match visit {
                Visit::Enter(block, depth) => {
                    depths.insert(block, depth);
                    dfs_in.insert(block, timestamp);
                    timestamp += 1;
                    stack.push(Visit::Exit(block));
                    for &child in children[&block].iter().rev() {
                        stack.push(Visit::Enter(child, depth + 1));
                    }
                }
                Visit::Exit(block) => {
                    dfs_out.insert(block, timestamp);
                    timestamp += 1;
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::collections::VecDeque;

        use rustc_hash::FxHashSet;

        use super::*;
        use crate::ir::{
            Program, Type,
            builder_trait::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
        };

        #[test]
        fn declaration_has_no_dominance_tree() {
            let mut program = Program::new();
            let function = program.new_function(Type::get_unit(), "decl".into(), vec![]);
            assert!(DominanceTree::new(program.func_data(function)).is_none());
        }

        #[test]
        fn computes_diamond_immediate_dominators() {
            let mut program = Program::new();
            let function = program.new_function(Type::get_i32(), "diamond".into(), vec![]);
            let data = program.func_data_mut(function);
            let entry = data.add_entry_block();
            let left = data.new_basic_block().basic_block("left".into(), vec![]);
            let right = data.new_basic_block().basic_block("right".into(), vec![]);
            let merge = data.new_basic_block().basic_block("merge".into(), vec![]);
            for block in [left, right, merge] {
                data.layout_mut().push_bb_back(block);
            }

            let condition = data.new_local_inst().integer(1);
            let branch = data
                .new_local_inst()
                .branch(condition, left, vec![], right, vec![]);
            data.layout_mut().insert_inst(entry, branch);
            let left_jump = data.new_local_inst().jump(merge, vec![]);
            data.layout_mut().insert_inst(left, left_jump);
            let right_jump = data.new_local_inst().jump(merge, vec![]);
            data.layout_mut().insert_inst(right, right_jump);
            let value = data.new_local_inst().integer(7);
            let ret = data.new_local_inst().ret(Some(value));
            data.layout_mut().insert_inst(merge, ret);

            let tree = DominanceTree::new(data).unwrap();
            assert_eq!(tree.immediate_dominator(entry), None);
            assert_eq!(tree.immediate_dominator(left), Some(entry));
            assert_eq!(tree.immediate_dominator(right), Some(entry));
            assert_eq!(tree.immediate_dominator(merge), Some(entry));
            assert!(tree.dominates(entry, merge));
            assert!(!tree.dominates(left, merge));
            assert!(!tree.dominates(right, merge));
            assert_eq!(tree.depth_of(entry), 0);
            assert_eq!(tree.depth_of(merge), 1);
            assert_eq!(
                tree.children_of(entry)
                    .iter()
                    .copied()
                    .collect::<FxHashSet<_>>(),
                FxHashSet::from_iter([left, right, merge])
            );
        }

        #[test]
        fn handles_backedges_and_ignores_unreachable_blocks() {
            let mut program = Program::new();
            let function = program.new_function(Type::get_unit(), "loop".into(), vec![]);
            let data = program.func_data_mut(function);
            let entry = data.add_entry_block();
            let header = data.new_basic_block().basic_block("header".into(), vec![]);
            let body = data.new_basic_block().basic_block("body".into(), vec![]);
            let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
            let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
            let dead = data.new_basic_block().basic_block("dead".into(), vec![]);
            for block in [header, body, latch, exit, dead] {
                data.layout_mut().push_bb_back(block);
            }

            let entry_jump = data.new_local_inst().jump(header, vec![]);
            data.layout_mut().insert_inst(entry, entry_jump);
            let condition = data.new_local_inst().integer(1);
            let header_branch = data
                .new_local_inst()
                .branch(condition, body, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, header_branch);
            let body_jump = data.new_local_inst().jump(latch, vec![]);
            data.layout_mut().insert_inst(body, body_jump);
            let latch_jump = data.new_local_inst().jump(header, vec![]);
            data.layout_mut().insert_inst(latch, latch_jump);
            let exit_ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(exit, exit_ret);
            let dead_ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(dead, dead_ret);

            let tree = DominanceTree::new(data).unwrap();
            assert_eq!(tree.immediate_dominator(header), Some(entry));
            assert_eq!(tree.immediate_dominator(body), Some(header));
            assert_eq!(tree.immediate_dominator(latch), Some(body));
            assert_eq!(tree.immediate_dominator(exit), Some(header));
            assert!(tree.dominates(header, latch));
            assert!(!tree.dominates(body, header));
            assert!(!tree.contains(dead));
        }

        #[test]
        fn numbers_a_deep_dominance_tree_without_recursion() {
            let mut program = Program::new();
            let function = program.new_function(Type::get_unit(), "deep_dom".into(), vec![]);
            let data = program.func_data_mut(function);
            let entry = data.add_entry_block();
            let mut blocks = vec![entry];
            for index in 0..20_000 {
                let block = data
                    .new_basic_block()
                    .basic_block(format!("block_{index}"), vec![]);
                data.layout_mut().push_bb_back(block);
                blocks.push(block);
            }
            for pair in blocks.windows(2) {
                let jump = data.new_local_inst().jump(pair[1], vec![]);
                data.layout_mut().insert_inst(pair[0], jump);
            }
            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(*blocks.last().unwrap(), ret);

            let cfg = CFG::new(data).unwrap();
            let tree = DominanceTree::from_cfg(&cfg);
            let last = *blocks.last().unwrap();
            assert_eq!(tree.depth_of(last), blocks.len() - 1);
            assert!(tree.dominates(entry, last));
            assert!(!tree.dominates(last, entry));
        }

        #[test]
        fn dominance_queries_match_the_graph_definition() {
            let mut program = Program::new();
            let function = program.new_function(Type::get_unit(), "nested".into(), vec![]);
            let data = program.func_data_mut(function);
            let entry = data.add_entry_block();
            let header = data.new_basic_block().basic_block("header".into(), vec![]);
            let left = data.new_basic_block().basic_block("left".into(), vec![]);
            let right = data.new_basic_block().basic_block("right".into(), vec![]);
            let merge = data.new_basic_block().basic_block("merge".into(), vec![]);
            let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
            let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
            for block in [header, left, right, merge, latch, exit] {
                data.layout_mut().push_bb_back(block);
            }

            let condition = data.new_local_inst().integer(1);
            let entry_jump = data.new_local_inst().jump(header, vec![]);
            data.layout_mut().insert_inst(entry, entry_jump);
            let header_branch = data
                .new_local_inst()
                .branch(condition, left, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, header_branch);
            let left_branch = data
                .new_local_inst()
                .branch(condition, right, vec![], merge, vec![]);
            data.layout_mut().insert_inst(left, left_branch);
            let right_jump = data.new_local_inst().jump(merge, vec![]);
            data.layout_mut().insert_inst(right, right_jump);
            let merge_jump = data.new_local_inst().jump(latch, vec![]);
            data.layout_mut().insert_inst(merge, merge_jump);
            let latch_jump = data.new_local_inst().jump(header, vec![]);
            data.layout_mut().insert_inst(latch, latch_jump);
            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(exit, ret);

            let cfg = CFG::new(data).unwrap();
            let tree = DominanceTree::from_cfg(&cfg);
            for &dominator in cfg.blocks() {
                for &block in cfg.blocks() {
                    assert_eq!(
                        tree.dominates(dominator, block),
                        dominates_by_removal(&cfg, dominator, block),
                        "mismatched dominance query for {dominator:?} -> {block:?}"
                    );
                }
            }
        }

        fn dominates_by_removal(cfg: &CFG, dominator: BasicBlock, block: BasicBlock) -> bool {
            if dominator == block || dominator == cfg.entry() {
                return true;
            }

            let mut visited = FxHashSet::default();
            let mut worklist = VecDeque::from([cfg.entry()]);
            while let Some(current) = worklist.pop_front() {
                if current == dominator || !visited.insert(current) {
                    continue;
                }
                if current == block {
                    return false;
                }
                worklist.extend(cfg.successors_of(current).iter().copied());
            }
            true
        }
    }
}
