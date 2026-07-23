use crate::opt::prelude::*;

/// Conservatively converts small, value-producing branches into `select`s.
///
/// This deliberately handles only three canonical shapes: a branch whose two
/// edges have the same target, an empty diamond, and a triangle containing at
/// most one speculatable integer binary instruction.
pub struct IfConversion;

#[derive(Clone, Copy)]
enum MergedValue {
    Common(Inst),
    Select { if_true: Inst, if_false: Inst },
}

struct Candidate {
    head: BasicBlock,
    terminator: Inst,
    merge: BasicBlock,
    cond: Inst,
    values: Vec<MergedValue>,
    remove_blocks: Vec<BasicBlock>,
    move_inst: Option<(BasicBlock, Inst)>,
}

impl Pass for IfConversion {
    fn run_on(&self, data: &mut ArenaContext<'_>) -> bool {
        // Re-scan after every rewrite. Apart from keeping the analysis simple,
        // this permits an exposed outer candidate to be converted as well.
        let mut changed = false;
        loop {
            let blocks = data
                .layout()
                .basicblocks()
                .iter()
                .map(|layout| layout.bb())
                .collect::<Vec<_>>();
            let Some(candidate) = blocks
                .into_iter()
                .find_map(|head| self.candidate(data, head))
            else {
                return changed;
            };
            self.apply(data, candidate);
            changed = true;
        }
    }
}

impl IfConversion {
    fn candidate(&self, data: &ArenaContext<'_>, head: BasicBlock) -> Option<Candidate> {
        let terminator = *data.layout().basicblock(head).insts().get_last()?;
        let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
            return None;
        };
        if !data.inst_data(branch.cond()).ty().is_i32() {
            return None;
        }

        if branch.t_target() == branch.f_target() {
            if branch.t_target() == head
                || !self.exact_users(data, branch.t_target(), &[terminator])
                || branch
                    .t_args()
                    .iter()
                    .chain(branch.f_args())
                    .any(|&value| !self.available_at(data, value, head))
            {
                return None;
            }
            return self.finish_candidate(
                data,
                head,
                terminator,
                branch.t_target(),
                branch.cond(),
                branch.t_args(),
                branch.f_args(),
                vec![],
                None,
            );
        }

        let t = branch.t_target();
        let f = branch.f_target();
        if t == head || f == head {
            return None;
        }

        // Full diamond: both arms are empty and jump to the same merge.
        if let (Some((tj, tm, ta)), Some((fj, fm, fa))) =
            (self.empty_arm(data, t), self.empty_arm(data, f))
        {
            if tm == fm
                && tm != head
                && tm != t
                && tm != f
                && data.bb_data(t).params().is_empty()
                && data.bb_data(f).params().is_empty()
                && branch.t_args().is_empty()
                && branch.f_args().is_empty()
                && self.exact_users(data, t, &[terminator])
                && self.exact_users(data, f, &[terminator])
                && self.exact_users(data, tm, &[tj, fj])
                && ta
                    .iter()
                    .chain(fa.iter())
                    .all(|&value| self.available_at(data, value, head))
            {
                return self.finish_candidate(
                    data,
                    head,
                    terminator,
                    tm,
                    branch.cond(),
                    &ta,
                    &fa,
                    vec![t, f],
                    None,
                );
            }
        }

        // Triangle: one edge reaches the merge directly, the other through a
        // unique arm with zero or one safe integer binary operation.
        let (arm, merge, direct_args, direct_is_true) = if let Some((_, m, _)) = self.arm(data, f) {
            if m == t {
                (f, t, branch.t_args(), true)
            } else {
                return None;
            }
        } else if let Some((_, m, _)) = self.arm(data, t) {
            if m == f {
                (t, f, branch.f_args(), false)
            } else {
                return None;
            }
        } else {
            return None;
        };
        let arm_edge_args = if arm == t {
            branch.t_args()
        } else {
            branch.f_args()
        };
        if merge == head
            || merge == arm
            || !data.bb_data(arm).params().is_empty()
            || !arm_edge_args.is_empty()
            || !self.exact_users(data, arm, &[terminator])
        {
            return None;
        }
        let (arm_jump, _, arm_args) = self.arm(data, arm)?;
        if !self.exact_users(data, merge, &[terminator, arm_jump]) {
            return None;
        }

        let insts = data
            .layout()
            .basicblock(arm)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let move_inst = match insts.as_slice() {
            [_jump] => None,
            [binary, _jump] if self.safe_arm_binary(data, *binary, arm_jump, head) => {
                Some((arm, *binary))
            }
            _ => return None,
        };
        let local = move_inst.map(|(_, inst)| inst);
        if arm_args
            .iter()
            .any(|&value| Some(value) != local && !self.available_at(data, value, head))
            || direct_args
                .iter()
                .any(|&value| !self.available_at(data, value, head))
        {
            return None;
        }

        let (true_args, false_args) = if direct_is_true {
            (direct_args, arm_args.as_slice())
        } else {
            (arm_args.as_slice(), direct_args)
        };
        self.finish_candidate(
            data,
            head,
            terminator,
            merge,
            branch.cond(),
            true_args,
            false_args,
            vec![arm],
            move_inst,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_candidate(
        &self,
        data: &ArenaContext<'_>,
        head: BasicBlock,
        terminator: Inst,
        merge: BasicBlock,
        cond: Inst,
        true_args: &[Inst],
        false_args: &[Inst],
        remove_blocks: Vec<BasicBlock>,
        move_inst: Option<(BasicBlock, Inst)>,
    ) -> Option<Candidate> {
        if merge == head || self.reaches(data, merge, head) {
            return None;
        }
        let params = data.bb_data(merge).params();
        if params.len() != true_args.len() || params.len() != false_args.len() {
            return None;
        }
        let mut values = Vec::with_capacity(params.len());
        for ((&param, &if_true), &if_false) in params.iter().zip(true_args).zip(false_args) {
            let ty = data.inst_data(param).ty();
            if ty.is_unit()
                || data.inst_data(if_true).ty() != ty
                || data.inst_data(if_false).ty() != ty
            {
                return None;
            }
            values.push(if if_true == if_false {
                MergedValue::Common(if_true)
            } else {
                MergedValue::Select { if_true, if_false }
            });
        }
        if !values
            .iter()
            .any(|value| matches!(value, MergedValue::Select { .. }))
        {
            return None;
        }
        Some(Candidate {
            head,
            terminator,
            merge,
            cond,
            values,
            remove_blocks,
            move_inst,
        })
    }

    fn reaches(&self, data: &ArenaContext<'_>, from: BasicBlock, target: BasicBlock) -> bool {
        let mut seen = HashSet::new();
        let mut work = VecDeque::from([from]);
        while let Some(block) = work.pop_front() {
            if !seen.insert(block) {
                continue;
            }
            if block == target {
                return true;
            }
            let Some(terminator) = data.layout().basicblock(block).insts().get_last().copied()
            else {
                continue;
            };
            work.extend(data.inst_data(terminator).bb_usage());
        }
        false
    }

    fn empty_arm(
        &self,
        data: &ArenaContext<'_>,
        bb: BasicBlock,
    ) -> Option<(Inst, BasicBlock, Vec<Inst>)> {
        let insts = data.layout().basicblock(bb).insts();
        if insts.len() != 1 {
            return None;
        }
        let jump_inst = *insts.get_last()?;
        let InstKind::Jump(jump) = data.inst_data(jump_inst).kind() else {
            return None;
        };
        Some((jump_inst, jump.target(), jump.args().to_vec()))
    }

    fn arm(
        &self,
        data: &ArenaContext<'_>,
        bb: BasicBlock,
    ) -> Option<(Inst, BasicBlock, Vec<Inst>)> {
        let jump_inst = *data.layout().basicblock(bb).insts().get_last()?;
        let InstKind::Jump(jump) = data.inst_data(jump_inst).kind() else {
            return None;
        };
        Some((jump_inst, jump.target(), jump.args().to_vec()))
    }

    fn safe_arm_binary(
        &self,
        data: &ArenaContext<'_>,
        inst: Inst,
        jump: Inst,
        head: BasicBlock,
    ) -> bool {
        let InstKind::Binary(binary) = data.inst_data(inst).kind() else {
            return false;
        };
        data.inst_data(inst).ty().is_i32()
            && data.inst_data(binary.lhs()).ty().is_i32()
            && data.inst_data(binary.rhs()).ty().is_i32()
            && !matches!(binary.op(), BinaryOp::Div | BinaryOp::Rem)
            && self.available_at(data, binary.lhs(), head)
            && self.available_at(data, binary.rhs(), head)
            && data
                .inst_data(inst)
                .used_by()
                .iter()
                .all(|&user| user == jump)
            && data.inst_data(inst).used_by().contains(&jump)
    }

    fn available_at(&self, data: &ArenaContext<'_>, value: Inst, head: BasicBlock) -> bool {
        if value.is_global()
            || data.inst_data(value).is_const()
            || matches!(data.inst_data(value).kind(), InstKind::FuncArgRef(..))
            || data.bb_data(head).params().contains(&value)
        {
            return true;
        }
        let Some(def_bb) = data.layout().parent_bb(value) else {
            return false;
        };
        if def_bb == head {
            let insts = data.layout().basicblock(head).insts();
            return insts.iter().any(|&inst| inst == value)
                && insts.get_last().is_some_and(|&term| term != value);
        }
        self.dominates(data, def_bb, head)
    }

    fn dominates(&self, data: &ArenaContext<'_>, dominator: BasicBlock, block: BasicBlock) -> bool {
        if dominator == block {
            return true;
        }
        let Some(entry) = data.layout().entry_bb().map(|layout| layout.bb()) else {
            return false;
        };
        if entry == dominator {
            return true;
        }

        // A block is dominated by `dominator` exactly when it is unreachable
        // from entry after removing `dominator`. Searching forward avoids
        // treating predecessor cycles as proof of dominance.
        let mut seen = HashSet::new();
        let mut work = VecDeque::from([entry]);
        while let Some(current) = work.pop_front() {
            if !seen.insert(current) {
                continue;
            }
            if current == block {
                return false;
            }
            let Some(terminator) = data
                .layout()
                .basicblock(current)
                .insts()
                .get_last()
                .copied()
            else {
                continue;
            };
            for successor in data.inst_data(terminator).bb_usage() {
                if successor != dominator {
                    work.push_back(successor);
                }
            }
        }
        true
    }

    fn exact_users(&self, data: &ArenaContext<'_>, bb: BasicBlock, expected: &[Inst]) -> bool {
        let users = data
            .bb_data(bb)
            .used_by()
            .iter()
            .copied()
            .filter(|&inst| data.layout().parent_bb(inst).is_some())
            .collect::<HashSet<_>>();
        users.len() == expected.len() && expected.iter().all(|inst| users.contains(inst))
    }

    fn apply(&self, data: &mut ArenaContext<'_>, candidate: Candidate) {
        let params = data.bb_data(candidate.merge).params().clone();

        // Remove the old terminator first so newly inserted values naturally
        // precede the replacement jump in layout order.
        data.remove_layout_inst(candidate.head, candidate.terminator);
        if let Some((arm, inst)) = candidate.move_inst {
            // Moving preserves the instruction identity and all use-def links.
            data.layout_mut().remove_inst(arm, inst);
            data.layout_mut().insert_inst(candidate.head, inst);
        }

        let mut replacements = Vec::with_capacity(params.len());
        for value in candidate.values {
            let replacement = match value {
                MergedValue::Common(value) => value,
                MergedValue::Select { if_true, if_false } => {
                    let select = data
                        .new_local_inst()
                        .select(candidate.cond, if_true, if_false);
                    data.layout_mut().insert_inst(candidate.head, select);
                    select
                }
            };
            replacements.push(replacement);
        }

        // Validate every parameter before this point and rewrite all of them as
        // one transaction; never leave a partially converted merge signature.
        for (&param, &replacement) in params.iter().zip(&replacements) {
            utils::visit_and_replace(data, param, replacement);
            assert!(data.inst_data(param).used_by().is_empty());
        }
        data.bb_data_mut(candidate.merge).params_mut().clear();
        for param in params {
            data.remove_orphan_inst(param);
        }
        let jump = data.new_local_inst().jump(candidate.merge, vec![]);
        data.layout_mut().insert_inst(candidate.head, jump);
        for bb in candidate.remove_blocks {
            data.remove_layout_basicblock(bb);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        Program,
        arena::Arena,
        builder::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
    };

    fn blocks(data: &FunctionData) -> Vec<BasicBlock> {
        data.layout()
            .basicblocks()
            .iter()
            .map(|layout| layout.bb())
            .collect()
    }

    fn select_count(data: &FunctionData) -> usize {
        data.layout()
            .basicblocks()
            .iter()
            .flat_map(|layout| layout.insts())
            .filter(|&&inst| matches!(data.inst_data(inst).kind(), InstKind::Select(..)))
            .count()
    }

    fn run(program: &mut Program) {
        IfConversion.run(program);
    }

    #[test]
    fn converts_same_target_branch_atomically() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "same".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let head = data.new_basic_block().basic_block("head".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32(), Type::get_i32()]);
        data.layout_mut().push_bb_back(head);
        data.layout_mut().push_bb_back(merge);
        let cond = data.params()[0];
        let one = data.new_local_inst().integer(1);
        let two = data.new_local_inst().integer(2);
        let branch =
            data.new_local_inst()
                .branch(cond, merge, vec![one, two], merge, vec![two, two]);
        data.layout_mut().insert_inst(head, branch);
        let params = data.bb_data(merge).params().clone();
        let sum = data
            .new_local_inst()
            .binary(BinaryOp::Add, params[0], params[1]);
        data.layout_mut().insert_inst(merge, sum);
        let ret = data.new_local_inst().ret(Some(sum));
        data.layout_mut().insert_inst(merge, ret);

        run(&mut program);
        let data = program.func_data(function);
        assert_eq!(blocks(data).len(), 2);
        assert_eq!(select_count(data), 1);
        assert!(data.bb_data(merge).params().is_empty());
        let terminator = utils::get_terminator_inst(data, head);
        assert!(
            matches!(data.inst_data(terminator).kind(), InstKind::Jump(j) if j.target() == merge && j.args().is_empty())
        );
    }

    #[test]
    fn converts_empty_diamond_with_dominating_values() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "diamond".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let head = data.new_basic_block().basic_block("head".into(), vec![]);
        let yes = data.new_basic_block().basic_block("yes".into(), vec![]);
        let no = data.new_basic_block().basic_block("no".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        for bb in [head, yes, no, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let cond = data.params()[0];
        let one = data.new_local_inst().integer(1);
        let dominating = data.new_local_inst().binary(BinaryOp::Add, cond, one);
        data.layout_mut().insert_inst(head, dominating);
        let branch = data.new_local_inst().branch(cond, yes, vec![], no, vec![]);
        data.layout_mut().insert_inst(head, branch);
        let yes_jump = data.new_local_inst().jump(merge, vec![dominating]);
        data.layout_mut().insert_inst(yes, yes_jump);
        let no_jump = data.new_local_inst().jump(merge, vec![one]);
        data.layout_mut().insert_inst(no, no_jump);
        let param = data.bb_data(merge).params()[0];
        let ret = data.new_local_inst().ret(Some(param));
        data.layout_mut().insert_inst(merge, ret);

        run(&mut program);
        let data = program.func_data(function);
        assert_eq!(blocks(data), vec![head, merge]);
        assert_eq!(select_count(data), 1);
    }

    #[test]
    fn converts_abs_triangle_with_sub_zero_x() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "abs".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let head = data.new_basic_block().basic_block("head".into(), vec![]);
        let neg = data.new_basic_block().basic_block("neg".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        for bb in [head, neg, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let x = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let branch = data.new_local_inst().branch(x, merge, vec![x], neg, vec![]);
        data.layout_mut().insert_inst(head, branch);
        let negate = data.new_local_inst().binary(BinaryOp::Sub, zero, x);
        data.layout_mut().insert_inst(neg, negate);
        let jump = data.new_local_inst().jump(merge, vec![negate]);
        data.layout_mut().insert_inst(neg, jump);
        let param = data.bb_data(merge).params()[0];
        let ret = data.new_local_inst().ret(Some(param));
        data.layout_mut().insert_inst(merge, ret);

        run(&mut program);
        let data = program.func_data(function);
        assert_eq!(blocks(data), vec![head, merge]);
        assert_eq!(data.layout().parent_bb(negate), Some(head));
        assert_eq!(select_count(data), 1);
    }

    #[test]
    fn rejects_arm_local_with_external_user() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "external".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let head = data.new_basic_block().basic_block("head".into(), vec![]);
        let arm = data.new_basic_block().basic_block("arm".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        for bb in [head, arm, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let x = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let branch = data.new_local_inst().branch(x, merge, vec![x], arm, vec![]);
        data.layout_mut().insert_inst(head, branch);
        let local = data.new_local_inst().binary(BinaryOp::Sub, zero, x);
        data.layout_mut().insert_inst(arm, local);
        let jump = data.new_local_inst().jump(merge, vec![local]);
        data.layout_mut().insert_inst(arm, jump);
        let param = data.bb_data(merge).params()[0];
        let external = data.new_local_inst().binary(BinaryOp::Add, param, local);
        data.layout_mut().insert_inst(merge, external);
        let ret = data.new_local_inst().ret(Some(external));
        data.layout_mut().insert_inst(merge, ret);

        run(&mut program);
        let data = program.func_data(function);
        assert_eq!(blocks(data).len(), 3);
        assert_eq!(select_count(data), 0);
        assert!(matches!(
            data.inst_data(branch).kind(),
            InstKind::Branch(..)
        ));
    }

    #[test]
    fn rejects_merge_with_extra_predecessor_and_trapping_binary() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "negative".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let head = data.new_basic_block().basic_block("head".into(), vec![]);
        let arm = data.new_basic_block().basic_block("arm".into(), vec![]);
        let extra = data.new_basic_block().basic_block("extra".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        for bb in [head, arm, extra, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let x = data.params()[0];
        let one = data.new_local_inst().integer(1);
        let branch = data.new_local_inst().branch(x, merge, vec![x], arm, vec![]);
        data.layout_mut().insert_inst(head, branch);
        let div = data.new_local_inst().binary(BinaryOp::Div, one, x);
        data.layout_mut().insert_inst(arm, div);
        let arm_jump = data.new_local_inst().jump(merge, vec![div]);
        data.layout_mut().insert_inst(arm, arm_jump);
        let extra_jump = data.new_local_inst().jump(merge, vec![one]);
        data.layout_mut().insert_inst(extra, extra_jump);
        let param = data.bb_data(merge).params()[0];
        let ret = data.new_local_inst().ret(Some(param));
        data.layout_mut().insert_inst(merge, ret);

        run(&mut program);
        let data = program.func_data(function);
        assert_eq!(blocks(data).len(), 4);
        assert_eq!(select_count(data), 0);
    }

    #[test]
    fn rejects_same_target_loop() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "loop".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let head = data.new_basic_block().basic_block("head".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        for bb in [head, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let cond = data.params()[0];
        let one = data.new_local_inst().integer(1);
        let two = data.new_local_inst().integer(2);
        let branch = data
            .new_local_inst()
            .branch(cond, merge, vec![one], merge, vec![two]);
        data.layout_mut().insert_inst(head, branch);
        let backedge = data.new_local_inst().jump(head, vec![]);
        data.layout_mut().insert_inst(merge, backedge);

        run(&mut program);
        let data = program.func_data(function);
        assert_eq!(blocks(data).len(), 2);
        assert_eq!(select_count(data), 0);
        assert!(matches!(
            data.inst_data(branch).kind(),
            InstKind::Branch(..)
        ));
    }
}
