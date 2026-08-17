use crate::opt::prelude::*;
use crate::opt::utils::logical_edge::{LogicalEdgeRewriter, outgoing_edges};

pub struct SimplifyCFG;

impl Pass for SimplifyCFG {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().entry_bb().is_none() {
            return false;
        }
        let mut changed_any = false;
        loop {
            let mut changed = false;

            changed |= SimplifyCFG::fold_const_condition_branch(data);
            changed |= SimplifyCFG::fold_branch_same_target_and_args(data);
            changed |= SimplifyCFG::remove_trivial_jump_block(data);

            if !changed {
                break;
            }
            changed_any = true;
        }
        changed_any
    }
}

impl SimplifyCFG {
    /// br 1, t_target, t_args, f_target, f_args => jump t_target, t_args
    /// br 0, t_target, t_args, f_target, f_args => jump f_target, f_args
    pub fn fold_const_condition_branch(data: &mut ArenaContextMut<'_>) -> bool {
        struct Edit {
            branch: Inst,
            target: BasicBlock,
            args: Vec<Inst>,
        }
        let mut edits = Vec::new();
        for bb_layout in data.layout().basicblocks() {
            let terminator = bb_layout.terminator();
            let inst_data = data.inst_data(terminator);
            if let InstKind::Branch(branch) = inst_data.kind() {
                let cond = branch.cond();
                if let InstKind::Integer(int) = data.inst_data(cond).kind() {
                    let cond_eval = int.value() != 0;
                    let (target, args) = if cond_eval {
                        (branch.t_target(), branch.t_args().to_vec())
                    } else {
                        (branch.f_target(), branch.f_args().to_vec())
                    };
                    edits.push(Edit {
                        branch: terminator,
                        target,
                        args,
                    });
                }
            }
        }
        let changed = !edits.is_empty();
        for Edit {
            branch,
            target,
            args,
        } in edits
        {
            data.replace_inst_with(branch).jump(target, args);
        }

        changed
    }

    pub fn fold_branch_same_target_and_args(data: &mut ArenaContextMut<'_>) -> bool {
        struct Edit {
            branch: Inst,
            target: BasicBlock,
            args: Vec<Inst>,
        }
        let mut edits = Vec::new();
        for bb_layout in data.layout().basicblocks() {
            let terminator = bb_layout.terminator();
            let inst_data = data.inst_data(terminator);
            if let InstKind::Branch(branch) = inst_data.kind() {
                if branch.t_target() == branch.f_target()
                    && data.insts_equal(branch.t_args(), branch.f_args())
                {
                    edits.push(Edit {
                        branch: terminator,
                        target: branch.t_target(),
                        args: branch.t_args().to_vec(),
                    });
                };
            }
        }
        let changed = !edits.is_empty();
        for Edit {
            branch,
            target,
            args,
        } in edits
        {
            data.replace_inst_with(branch).jump(target, args);
        }

        changed
    }

    pub fn remove_trivial_jump_block(data: &mut ArenaContextMut<'_>) -> bool {
        let mut candidates = HashMap::default();
        // Skip entry bb
        for bb_layout in data.layout().basicblocks().iter().skip(1) {
            if bb_layout.insts().len() != 1 {
                continue;
            }
            let first = *bb_layout.insts().get_first().unwrap();
            // Find a block that only have a jump instruction with no argument.
            let InstKind::Jump(jump) = data.inst_data(first).kind() else {
                continue;
            };
            if !(jump.args().is_empty() && data.bb_data(bb_layout.bb()).params().is_empty()) {
                continue;
            }
            // Removing a self-jumping block would leave its predecessors
            // targeting a block that is no longer in the function layout.
            if jump.target() == bb_layout.bb() {
                continue;
            }
            candidates.insert(bb_layout.bb(), jump.target());
        }

        let mut final_targets: HashMap<BasicBlock, Option<BasicBlock>> = HashMap::default();
        for &start in candidates.keys() {
            if final_targets.contains_key(&start) {
                continue;
            }
            let mut path = Vec::new();
            let mut path_indices = HashMap::default();
            let mut current = start;
            let final_target = loop {
                if let Some(&resolved) = final_targets.get(&current) {
                    break resolved;
                }
                if path_indices.insert(current, path.len()).is_some() {
                    break None;
                }
                path.push(current);
                let target = candidates[&current];
                if !candidates.contains_key(&target) {
                    break Some(target);
                }
                current = target;
            };
            for block in path {
                final_targets.insert(block, final_target);
            }
        }

        let removable = final_targets
            .iter()
            .filter_map(|(&block, target)| target.map(|target| (block, target)))
            .collect::<HashMap<_, _>>();
        if removable.is_empty() {
            return false;
        }

        let blocks = data
            .layout()
            .basicblocks()
            .iter()
            .map(|layout| layout.bb())
            .collect::<Vec<_>>();
        let mut rewrites = LogicalEdgeRewriter::new();
        let mut skipped_targets = std::collections::HashSet::new();
        for source in blocks {
            for edge in outgoing_edges(data.curr_func_data(), source) {
                if let Some(&target) = removable.get(&edge.target(data.curr_func_data())) {
                    let args = edge.args(data.curr_func_data()).to_vec();
                    // The final target may carry parameters (the trivial
                    // jump block itself is parameterless, but its target
                    // need not be); retargeting with mismatched args would
                    // panic in the rewriter. Skip the edge conservatively —
                    // the candidate jump block stays in place (and is not
                    // removed).
                    if args.len() != data.bb_data(target).params().len() {
                        skipped_targets.insert(edge.target(data.curr_func_data()));
                        continue;
                    }
                    rewrites.retarget(data.curr_func_data(), edge, target, args);
                }
            }
        }
        rewrites.apply(data);
        let mut removed_any = false;
        for &bb in removable.keys() {
            if skipped_targets.contains(&bb) {
                continue;
            }
            data.curr_func_data_mut().remove_layout_basicblock(bb);
            removed_any = true;
        }
        removed_any
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{Program, Type, builder_trait::*},
        opt::utils::cfg::CFG,
    };

    #[test]
    fn keeps_trivial_self_loop_in_layout() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "self_loop".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let loop_block = data.new_basic_block().basic_block("loop".into(), vec![]);
        data.layout_mut().push_bb_back(loop_block);

        let enter = data.new_local_inst().jump(loop_block, vec![]);
        data.layout_mut().insert_inst(entry, enter);
        let backedge = data.new_local_inst().jump(loop_block, vec![]);
        data.layout_mut().insert_inst(loop_block, backedge);

        let mut context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        assert!(!SimplifyCFG::remove_trivial_jump_block(&mut context));
        assert!(CFG::new(context.curr_func_data()).is_some());
        assert_eq!(
            context
                .curr_func_data()
                .layout()
                .basicblock(loop_block)
                .terminator(),
            backedge
        );
    }

    #[test]
    fn redirects_both_same_target_branch_arms() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "same_target".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let trivial = data.new_basic_block().basic_block("trivial".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        data.layout_mut().push_bb_back(trivial);
        data.layout_mut().push_bb_back(exit);

        let cond = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(cond, trivial, vec![], trivial, vec![]);
        data.layout_mut().insert_inst(entry, branch);
        let bypass = data.new_local_inst().jump(exit, vec![]);
        data.layout_mut().insert_inst(trivial, bypass);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        let mut context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        assert!(SimplifyCFG::remove_trivial_jump_block(&mut context));
        let InstKind::Branch(branch_data) = context.inst_data(branch).kind() else {
            panic!("entry terminator must remain a branch");
        };
        assert_eq!(branch_data.t_target(), exit);
        assert_eq!(branch_data.f_target(), exit);
        assert!(!context.bb_data(exit).used_by().contains(&bypass));
        assert!(CFG::new(context.curr_func_data()).is_some());
    }

    #[test]
    fn removes_a_chain_of_trivial_blocks_in_one_run() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "trivial_chain".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let mut blocks = Vec::new();
        for index in 0..1_000 {
            let block = data
                .new_basic_block()
                .basic_block(format!("trivial_{index}"), vec![]);
            data.layout_mut().push_bb_back(block);
            blocks.push(block);
        }
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        data.layout_mut().push_bb_back(exit);

        let enter = data.new_local_inst().jump(blocks[0], vec![]);
        data.layout_mut().insert_inst(entry, enter);
        for (index, &block) in blocks.iter().enumerate() {
            let target = blocks.get(index + 1).copied().unwrap_or(exit);
            let jump = data.new_local_inst().jump(target, vec![]);
            data.layout_mut().insert_inst(block, jump);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        let mut context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        assert!(SimplifyCFG::remove_trivial_jump_block(&mut context));
        assert_eq!(context.curr_func_data().layout().basicblocks().len(), 2);
        assert!(matches!(
            context.inst_data(enter).kind(),
            InstKind::Jump(jump) if jump.target() == exit
        ));
        assert!(CFG::new(context.curr_func_data()).is_some());
    }

    #[test]
    fn keeps_a_trivial_block_cycle() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "trivial_cycle".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let first = data.new_basic_block().basic_block("first".into(), vec![]);
        let second = data.new_basic_block().basic_block("second".into(), vec![]);
        data.layout_mut().push_bb_back(first);
        data.layout_mut().push_bb_back(second);

        let enter = data.new_local_inst().jump(first, vec![]);
        data.layout_mut().insert_inst(entry, enter);
        let to_second = data.new_local_inst().jump(second, vec![]);
        data.layout_mut().insert_inst(first, to_second);
        let to_first = data.new_local_inst().jump(first, vec![]);
        data.layout_mut().insert_inst(second, to_first);

        let mut context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        assert!(!SimplifyCFG::remove_trivial_jump_block(&mut context));
        assert_eq!(context.curr_func_data().layout().basicblocks().len(), 3);
        assert!(CFG::new(context.curr_func_data()).is_some());
    }
}
