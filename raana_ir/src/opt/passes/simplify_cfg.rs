use crate::opt::prelude::*;

pub struct SimplifyCFG;

impl Pass for SimplifyCFG {
    fn run_on(&self, data: &mut ArenaContext<'_>) -> bool {
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
    pub fn fold_const_condition_branch(data: &mut ArenaContext<'_>) -> bool {
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

    pub fn fold_branch_same_target_and_args(data: &mut ArenaContext<'_>) -> bool {
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

    pub fn remove_trivial_jump_block(data: &mut ArenaContext<'_>) -> bool {
        enum Edit {
            Jump {
                to_modify: Inst,
                new: BasicBlock,
            },
            Branch {
                to_modify: Inst,
                is_true_branch: bool,
                new: BasicBlock,
                cond: Inst,
                another_target: BasicBlock,
                another_args: Vec<Inst>,
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
            for &used in data.bb_data(bb_layout.bb()).used_by() {
                match data.inst_data(used).kind() {
                    InstKind::Jump(..) => {
                        edits.push(Edit::Jump {
                            to_modify: used,
                            new: jump.target(),
                        });
                    }
                    InstKind::Branch(branch) => {
                        let is_true_branch = branch.t_target() == bb_layout.bb();
                        let (another_target, another_args) = if is_true_branch {
                            (branch.f_target(), branch.f_args().to_vec())
                        } else {
                            (branch.t_target(), branch.t_args().to_vec())
                        };
                        edits.push(Edit::Branch {
                            to_modify: used,
                            is_true_branch,
                            new: jump.target(),
                            cond: branch.cond(),
                            another_target,
                            another_args,
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
                is_true_branch,
                new,
                cond,
                another_target,
                another_args,
            } => {
                if is_true_branch {
                    data.replace_inst_with(to_modify).branch(
                        cond,
                        new,
                        vec![],
                        another_target,
                        another_args,
                    );
                } else {
                    data.replace_inst_with(to_modify).branch(
                        cond,
                        another_target,
                        another_args,
                        new,
                        vec![],
                    );
                }
            }
        });
        trivial_block.into_iter().for_each(|bb| {
            data.curr_func_data_mut().layout_mut().remove_basicblock(bb);
        });
        changed
    }
}
