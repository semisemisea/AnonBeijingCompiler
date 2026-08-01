//! Function-local control-flow graph analysis.
//!
//! This is independent from the legacy CFG helpers in `analysis_passes::cfg`.
//! Basic blocks remain the public identity; no analysis-specific block IDs are
//! exposed to users of the graph.

use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use crate::ir::{BasicBlock, FunctionData, InstKind, arena::Arena};

pub type BlockNeighbors = SmallVec<[BasicBlock; 2]>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CFGEdge {
    pub src: BasicBlock,
    pub dst: BasicBlock,
}

#[derive(Debug, Clone)]
pub struct CFG {
    entry: BasicBlock,
    postorder: Vec<BasicBlock>,
    reverse_postorder: Vec<BasicBlock>,
    successors: FxHashMap<BasicBlock, BlockNeighbors>,
    predecessors: FxHashMap<BasicBlock, BlockNeighbors>,
    acyclic: bool,
}

impl CFG {
    /// Build the CFG of a function definition.
    ///
    /// Function declarations have no entry block and return `None`. Only blocks
    /// reachable from the entry are included in the graph and traversal orders.
    /// Malformed reachable CFGs panic rather than producing a partial analysis.
    pub fn new(data: &FunctionData) -> Option<Self> {
        let entry = data.layout().entry_bb()?.bb();
        let layout_blocks = data
            .layout()
            .basicblocks()
            .iter()
            .map(|layout| layout.bb())
            .collect::<FxHashSet<_>>();

        fn block_successors(
            data: &FunctionData,
            block: BasicBlock,
            layout_blocks: &FxHashSet<BasicBlock>,
        ) -> BlockNeighbors {
            let layout = data.layout().basicblock(block);
            let instructions = layout.insts();
            let terminator = instructions
                .get_last()
                .copied()
                .unwrap_or_else(|| panic!("reachable basic block {block:?} is empty"));

            debug_assert!(
                instructions
                    .iter()
                    .take(instructions.len().saturating_sub(1))
                    .all(|&inst| !data.inst_data(inst).kind().is_terminator()),
                "reachable basic block {block:?} contains a terminator before its last instruction"
            );

            let mut successors = BlockNeighbors::new();
            match data.inst_data(terminator).kind() {
                InstKind::Jump(jump) => successors.push(jump.target()),
                InstKind::Branch(branch) => {
                    successors.push(branch.t_target());
                    if branch.f_target() != branch.t_target() {
                        successors.push(branch.f_target());
                    }
                }
                InstKind::Return(..) | InstKind::TailCall(..) => {}
                kind => panic!(
                    "last instruction of reachable basic block {block:?} is not a terminator: {kind:?}"
                ),
            }

            for &successor in &successors {
                assert!(
                    layout_blocks.contains(&successor),
                    "reachable basic block {block:?} targets block {successor:?} outside the function layout"
                );
            }
            successors
        }

        #[derive(Clone, Copy)]
        enum Visit {
            Enter(BasicBlock),
            Exit(BasicBlock),
        }

        let mut active = FxHashSet::default();
        let mut visited = FxHashSet::default();
        let mut successors = FxHashMap::default();
        let mut postorder = Vec::new();
        let mut acyclic = true;
        let mut stack = vec![Visit::Enter(entry)];
        while let Some(visit) = stack.pop() {
            match visit {
                Visit::Enter(block) => {
                    if visited.contains(&block) {
                        continue;
                    }
                    if !active.insert(block) {
                        acyclic = false;
                        continue;
                    }

                    let block_successors = block_successors(data, block, &layout_blocks);
                    stack.push(Visit::Exit(block));
                    for &successor in block_successors.iter().rev() {
                        if active.contains(&successor) {
                            acyclic = false;
                        } else if !visited.contains(&successor) {
                            stack.push(Visit::Enter(successor));
                        }
                    }
                    successors.insert(block, block_successors);
                }
                Visit::Exit(block) => {
                    active.remove(&block);
                    if visited.insert(block) {
                        postorder.push(block);
                    }
                }
            }
        }

        let mut reverse_postorder = postorder.clone();
        reverse_postorder.reverse();
        assert_eq!(reverse_postorder.first(), Some(&entry));

        let mut predecessors = FxHashMap::default();
        for &block in &reverse_postorder {
            predecessors.insert(block, BlockNeighbors::new());
        }
        for (&source, targets) in &successors {
            for &target in targets {
                predecessors
                    .get_mut(&target)
                    .expect("all successor blocks must be reachable")
                    .push(source);
            }
        }

        let graph = Self {
            entry,
            postorder,
            reverse_postorder,
            successors,
            predecessors,
            acyclic,
        };
        #[cfg(debug_assertions)]
        graph.verify();
        Some(graph)
    }

    pub fn entry(&self) -> BasicBlock {
        self.entry
    }

    /// Reachable blocks in reverse postorder.
    pub fn blocks(&self) -> &[BasicBlock] {
        &self.reverse_postorder
    }

    pub fn postorder(&self) -> &[BasicBlock] {
        &self.postorder
    }

    pub fn reverse_postorder(&self) -> &[BasicBlock] {
        &self.reverse_postorder
    }

    pub fn successors_of(&self, block: BasicBlock) -> &[BasicBlock] {
        self.successors
            .get(&block)
            .map(BlockNeighbors::as_slice)
            .unwrap_or_else(|| panic!("basic block {block:?} is not reachable in this CFG"))
    }

    pub fn predecessors_of(&self, block: BasicBlock) -> &[BasicBlock] {
        self.predecessors
            .get(&block)
            .map(BlockNeighbors::as_slice)
            .unwrap_or_else(|| panic!("basic block {block:?} is not reachable in this CFG"))
    }

    pub fn is_reachable(&self, block: BasicBlock) -> bool {
        self.successors.contains_key(&block)
    }

    pub fn block_count(&self) -> usize {
        self.reverse_postorder.len()
    }

    pub fn edge_count(&self) -> usize {
        self.successors.values().map(BlockNeighbors::len).sum()
    }

    pub fn is_acyclic(&self) -> bool {
        self.acyclic
    }

    pub fn edges(&self) -> impl Iterator<Item = CFGEdge> + '_ {
        self.reverse_postorder.iter().flat_map(|&src| {
            self.successors_of(src)
                .iter()
                .copied()
                .map(move |dst| CFGEdge { src, dst })
        })
    }

    #[cfg(debug_assertions)]
    fn verify(&self) {
        debug_assert_eq!(self.reverse_postorder.first(), Some(&self.entry));
        debug_assert_eq!(self.postorder.len(), self.reverse_postorder.len());
        debug_assert_eq!(self.successors.len(), self.reverse_postorder.len());
        debug_assert_eq!(self.predecessors.len(), self.reverse_postorder.len());

        let blocks = self
            .reverse_postorder
            .iter()
            .copied()
            .collect::<FxHashSet<_>>();
        debug_assert_eq!(blocks.len(), self.reverse_postorder.len());
        debug_assert_eq!(
            self.postorder.iter().copied().collect::<FxHashSet<_>>(),
            blocks
        );

        for edge in self.edges() {
            debug_assert!(blocks.contains(&edge.src));
            debug_assert!(blocks.contains(&edge.dst));
            debug_assert!(self.predecessors_of(edge.dst).contains(&edge.src));
        }
        for (&dst, sources) in &self.predecessors {
            for &src in sources {
                debug_assert!(self.successors_of(src).contains(&dst));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        Program, Type,
        builder_trait::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
    };

    #[test]
    fn declaration_has_no_cfg() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "decl".into(), vec![]);
        assert!(CFG::new(program.func_data(function)).is_none());
    }

    #[test]
    fn builds_reachable_bidirectional_cfg_and_orders() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "diamond".into(), vec![]);
        let data = program.func_data_mut(function);

        let entry = data.add_entry_block();
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let merge = data.new_basic_block().basic_block("merge".into(), vec![]);
        let unreachable = data
            .new_basic_block()
            .basic_block("unreachable".into(), vec![]);
        for block in [left, right, merge, unreachable] {
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
        let result = data.new_local_inst().integer(7);
        let ret = data.new_local_inst().ret(Some(result));
        data.layout_mut().insert_inst(merge, ret);

        let dead_jump = data.new_local_inst().jump(unreachable, vec![]);
        data.layout_mut().insert_inst(unreachable, dead_jump);

        let cfg = CFG::new(data).unwrap();
        assert_eq!(cfg.entry(), entry);
        assert_eq!(cfg.block_count(), 4);
        assert_eq!(cfg.edge_count(), 4);
        assert!(cfg.is_acyclic());
        assert_eq!(cfg.reverse_postorder().first(), Some(&entry));
        assert_eq!(cfg.postorder().last(), Some(&entry));
        assert!(!cfg.is_reachable(unreachable));
        assert_eq!(cfg.successors_of(entry), &[left, right]);
        assert_eq!(cfg.successors_of(left), &[merge]);
        assert_eq!(cfg.successors_of(right), &[merge]);
        assert!(cfg.successors_of(merge).is_empty());
        assert!(cfg.predecessors_of(entry).is_empty());
        assert_eq!(
            cfg.predecessors_of(merge)
                .iter()
                .copied()
                .collect::<FxHashSet<_>>(),
            FxHashSet::from_iter([left, right])
        );
    }

    #[test]
    fn represents_loops_and_deduplicates_same_target_branches() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "loop".into(), vec![]);
        let data = program.func_data_mut(function);

        let entry = data.add_entry_block();
        let header = data.new_basic_block().basic_block("header".into(), vec![]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let entry_jump = data.new_local_inst().jump(header, vec![]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let condition = data.new_local_inst().integer(1);
        let header_branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, header_branch);
        let body_branch = data
            .new_local_inst()
            .branch(condition, header, vec![], header, vec![]);
        data.layout_mut().insert_inst(body, body_branch);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        let cfg = CFG::new(data).unwrap();
        assert!(!cfg.is_acyclic());
        assert_eq!(cfg.successors_of(body), &[header]);
        assert_eq!(
            cfg.predecessors_of(header)
                .iter()
                .copied()
                .collect::<FxHashSet<_>>(),
            FxHashSet::from_iter([entry, body])
        );
        assert!(cfg.edges().any(|edge| edge
            == CFGEdge {
                src: body,
                dst: header,
            }));
    }

    #[test]
    #[should_panic(expected = "is not a terminator")]
    fn rejects_reachable_block_without_terminator() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "malformed".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let value = data.new_local_inst().integer(1);
        data.layout_mut().insert_inst(entry, value);

        let _ = CFG::new(data);
    }

    #[test]
    fn builds_a_deep_acyclic_cfg_without_recursion() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "deep_cfg".into(), vec![]);
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
        assert!(cfg.is_acyclic());
        assert_eq!(cfg.block_count(), blocks.len());
        assert_eq!(cfg.reverse_postorder(), blocks);
    }
}
