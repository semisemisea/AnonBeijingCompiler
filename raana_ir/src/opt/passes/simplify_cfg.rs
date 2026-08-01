use crate::opt::prelude::*;

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
        enum Edit {
            Jump {
                to_modify: Inst,
                new: BasicBlock,
            },
            Branch {
                to_modify: Inst,
                cond: Inst,
                t_target: BasicBlock,
                t_args: Vec<Inst>,
                f_target: BasicBlock,
                f_args: Vec<Inst>,
            },
        }

        let mut edits = Vec::new();
        let mut trivial_block = Vec::new();
        // Skip entry bb
        for bb_layout in data.layout().basicblocks().iter().skip(1) {
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
            for &used in data.bb_data(bb_layout.bb()).used_by() {
                match data.inst_data(used).kind() {
                    InstKind::Jump(..) => {
                        edits.push(Edit::Jump {
                            to_modify: used,
                            new: jump.target(),
                        });
                    }
                    InstKind::Branch(branch) => {
                        edits.push(Edit::Branch {
                            to_modify: used,
                            cond: branch.cond(),
                            t_target: if branch.t_target() == bb_layout.bb() {
                                jump.target()
                            } else {
                                branch.t_target()
                            },
                            t_args: branch.t_args().to_vec(),
                            f_target: if branch.f_target() == bb_layout.bb() {
                                jump.target()
                            } else {
                                branch.f_target()
                            },
                            f_args: branch.f_args().to_vec(),
                        });
                    }
                    _ => unreachable!(),
                }
            }
            trivial_block.push(bb_layout.bb());
            break;
        }
        let changed = !edits.is_empty();
        edits.into_iter().for_each(|edit| match edit {
            Edit::Jump { to_modify, new } => {
                data.replace_inst_with(to_modify).jump(new, vec![]);
            }
            Edit::Branch {
                to_modify,
                cond,
                t_target,
                t_args,
                f_target,
                f_args,
            } => {
                data.replace_inst_with(to_modify)
                    .branch(cond, t_target, t_args, f_target, f_args);
            }
        });
        trivial_block.into_iter().for_each(|bb| {
            data.curr_func_data_mut().remove_layout_basicblock(bb);
        });
        changed
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
}
