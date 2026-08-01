//! Logical control-flow edges and transactional terminator rewrites.
//!
//! Structural CFG edges deduplicate same-target branch arms. This module keeps
//! each jump/branch arm distinct so positional block arguments can be analyzed
//! and rewritten without losing edge multiplicity.

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::{
    ir::{BasicBlock, FunctionData, Inst, InstKind, arena::Arena, builder_trait::LocalInstBuilder},
    opt::{pass::ArenaContextMut, utils::cfg::CFG},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogicalEdgeArm {
    Jump,
    True,
    False,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LogicalEdge {
    pub from: BasicBlock,
    pub terminator: Inst,
    pub arm: LogicalEdgeArm,
}

impl LogicalEdge {
    pub fn new(source: BasicBlock, terminator: Inst, arm: LogicalEdgeArm) -> Self {
        Self {
            from: source,
            terminator,
            arm,
        }
    }

    pub fn source(self) -> BasicBlock {
        self.from
    }

    pub fn terminator(self) -> Inst {
        self.terminator
    }

    pub fn arm(self) -> LogicalEdgeArm {
        self.arm
    }

    pub fn target(self, data: &FunctionData) -> BasicBlock {
        match (self.arm, data.inst_data(self.terminator).kind()) {
            (LogicalEdgeArm::Jump, InstKind::Jump(jump)) => jump.target(),
            (LogicalEdgeArm::True, InstKind::Branch(branch)) => branch.t_target(),
            (LogicalEdgeArm::False, InstKind::Branch(branch)) => branch.f_target(),
            _ => panic!("logical edge arm does not match its terminator"),
        }
    }

    pub fn args(self, data: &FunctionData) -> &[Inst] {
        match (self.arm, data.inst_data(self.terminator).kind()) {
            (LogicalEdgeArm::Jump, InstKind::Jump(jump)) => jump.args(),
            (LogicalEdgeArm::True, InstKind::Branch(branch)) => branch.t_args(),
            (LogicalEdgeArm::False, InstKind::Branch(branch)) => branch.f_args(),
            _ => panic!("logical edge arm does not match its terminator"),
        }
    }
}

pub fn outgoing_edges(data: &FunctionData, source: BasicBlock) -> SmallVec<[LogicalEdge; 2]> {
    let terminator = data.layout().basicblock(source).terminator();
    match data.inst_data(terminator).kind() {
        InstKind::Jump(..) => {
            SmallVec::from_slice(&[LogicalEdge::new(source, terminator, LogicalEdgeArm::Jump)])
        }
        InstKind::Branch(..) => SmallVec::from_slice(&[
            LogicalEdge::new(source, terminator, LogicalEdgeArm::True),
            LogicalEdge::new(source, terminator, LogicalEdgeArm::False),
        ]),
        InstKind::Return(..) | InstKind::TailCall(..) => SmallVec::new(),
        kind => panic!("basic block has non-terminator last instruction: {kind:?}"),
    }
}

pub fn incoming_edges(
    data: &FunctionData,
    cfg: &CFG,
    target: BasicBlock,
) -> SmallVec<[LogicalEdge; 4]> {
    let mut incoming = SmallVec::new();
    for &source in cfg.predecessors_of(target) {
        let mut targets = false;
        for edge in outgoing_edges(data, source) {
            if edge.target(data) == target {
                incoming.push(edge);
                targets = true;
            }
        }
        assert!(
            targets,
            "a structural CFG predecessor must have a logical edge to its successor"
        );
    }
    incoming
}

#[derive(Debug, Clone)]
enum TerminatorRewrite {
    Jump {
        target: BasicBlock,
        args: Vec<Inst>,
    },
    Branch {
        cond: Inst,
        t_target: BasicBlock,
        t_args: Vec<Inst>,
        f_target: BasicBlock,
        f_args: Vec<Inst>,
    },
}

#[derive(Default)]
pub struct LogicalEdgeRewriter {
    pending: FxHashMap<Inst, TerminatorRewrite>,
}

impl LogicalEdgeRewriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn seed(&mut self, data: &FunctionData, terminator: Inst) {
        self.pending
            .entry(terminator)
            .or_insert_with(|| snapshot_terminator(data, terminator));
    }

    pub fn retarget(
        &mut self,
        data: &FunctionData,
        edge: LogicalEdge,
        target: BasicBlock,
        args: Vec<Inst>,
    ) {
        self.edit(data, edge, |edge_target, edge_args| {
            *edge_target = target;
            *edge_args = args;
        });
    }

    pub fn append_arg(&mut self, data: &FunctionData, edge: LogicalEdge, value: Inst) {
        self.edit(data, edge, |_target, args| args.push(value));
    }

    pub fn remove_arg(&mut self, data: &FunctionData, edge: LogicalEdge, index: usize) {
        self.edit(data, edge, |_target, args| {
            args.swap_remove(index);
        });
    }

    pub fn edit(
        &mut self,
        data: &FunctionData,
        edge: LogicalEdge,
        edit: impl FnOnce(&mut BasicBlock, &mut Vec<Inst>),
    ) {
        let rewrite = self
            .pending
            .entry(edge.terminator)
            .or_insert_with(|| snapshot_terminator(data, edge.terminator));
        match (edge.arm, rewrite) {
            (LogicalEdgeArm::Jump, TerminatorRewrite::Jump { target, args }) => edit(target, args),
            (
                LogicalEdgeArm::True,
                TerminatorRewrite::Branch {
                    t_target, t_args, ..
                },
            ) => edit(t_target, t_args),
            (
                LogicalEdgeArm::False,
                TerminatorRewrite::Branch {
                    f_target, f_args, ..
                },
            ) => edit(f_target, f_args),
            _ => panic!("logical edge arm does not match its terminator rewrite"),
        }
    }

    pub fn apply(self, data: &mut ArenaContextMut<'_>) -> bool {
        let changed = !self.pending.is_empty();
        for (terminator, rewrite) in self.pending {
            match rewrite {
                TerminatorRewrite::Jump { target, args } => {
                    data.replace_inst_with(terminator).jump(target, args);
                }
                TerminatorRewrite::Branch {
                    cond,
                    t_target,
                    t_args,
                    f_target,
                    f_args,
                } => {
                    data.replace_inst_with(terminator)
                        .branch(cond, t_target, t_args, f_target, f_args);
                }
            }
        }
        changed
    }
}

fn snapshot_terminator(data: &FunctionData, terminator: Inst) -> TerminatorRewrite {
    match data.inst_data(terminator).kind() {
        InstKind::Jump(jump) => TerminatorRewrite::Jump {
            target: jump.target(),
            args: jump.args().to_vec(),
        },
        InstKind::Branch(branch) => TerminatorRewrite::Branch {
            cond: branch.cond(),
            t_target: branch.t_target(),
            t_args: branch.t_args().to_vec(),
            f_target: branch.f_target(),
            f_args: branch.f_args().to_vec(),
        },
        _ => panic!("logical edge rewrite requires a jump or branch terminator"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Program, Type, builder_trait::*};

    #[test]
    fn enumerates_same_target_branch_arms_independently() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "edges".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        data.layout_mut().push_bb_back(merge);

        let cond = data.new_local_inst().integer(1);
        let zero = data.new_local_inst().integer(0);
        let one = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(cond, merge, vec![zero], merge, vec![one]);
        data.layout_mut().insert_inst(entry, branch);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(merge, ret);

        let cfg = CFG::new(data).unwrap();
        let edges = incoming_edges(data, &cfg, merge);
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].arm(), LogicalEdgeArm::True);
        assert_eq!(edges[0].args(data), [zero]);
        assert_eq!(edges[1].arm(), LogicalEdgeArm::False);
        assert_eq!(edges[1].args(data), [one]);
        assert_eq!(data.bb_data(merge).used_by().len(), 1);
    }

    #[test]
    fn groups_both_arm_edits_and_rebuilds_reverse_links() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "rewrite".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let old_target = data
            .new_basic_block()
            .basic_block("old".into(), vec![Type::get_i32()]);
        let new_target = data
            .new_basic_block()
            .basic_block("new".into(), vec![Type::get_i32()]);
        data.layout_mut().push_bb_back(old_target);
        data.layout_mut().push_bb_back(new_target);

        let cond = data.new_local_inst().integer(1);
        let zero = data.new_local_inst().integer(0);
        let one = data.new_local_inst().integer(1);
        let branch =
            data.new_local_inst()
                .branch(cond, old_target, vec![zero], old_target, vec![one]);
        data.layout_mut().insert_inst(entry, branch);
        for block in [old_target, new_target] {
            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(block, ret);
        }

        let edges = outgoing_edges(data, entry);
        let mut rewrites = LogicalEdgeRewriter::new();
        rewrites.retarget(data, edges[0], new_target, vec![one]);
        rewrites.retarget(data, edges[1], new_target, vec![zero]);

        let mut context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        assert!(rewrites.apply(&mut context));
        let InstKind::Branch(rewritten) = context.inst_data(branch).kind() else {
            panic!("terminator must remain a branch");
        };
        assert_eq!(rewritten.t_target(), new_target);
        assert_eq!(rewritten.t_args(), [one]);
        assert_eq!(rewritten.f_target(), new_target);
        assert_eq!(rewritten.f_args(), [zero]);
        assert!(!context.bb_data(old_target).used_by().contains(&branch));
        assert!(context.bb_data(new_target).used_by().contains(&branch));
        assert!(!context.inst_data(one).used_by().is_empty());
        assert!(!context.inst_data(zero).used_by().is_empty());
    }
}
