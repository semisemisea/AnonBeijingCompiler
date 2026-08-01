use crate::opt::prelude::*;

/// Converts small, value-producing branches into `select`s, and folds
/// `&&`/`||` branch triangles into single `and`/`or` instructions.
///
/// Handles four canonical shapes: a branch whose two edges have the same
/// target, an empty diamond, a triangle containing a chain of speculatable
/// integer instructions, and a land/lor triangle (`br c1, rhs, merge(0)`
/// with `rhs: c2 = ...; jump merge(c2)` folding to `band(c1, c2)`).
pub struct IfConversion;

#[derive(Clone, Copy)]
enum MergedValue {
    Common(Inst),
    Select { if_true: Inst, if_false: Inst },
    BoolBinary { op: BinaryOp, lhs: Inst, rhs: Inst },
}

struct Candidate {
    head: BasicBlock,
    terminator: Inst,
    merge: BasicBlock,
    cond: Inst,
    values: Vec<MergedValue>,
    remove_blocks: Vec<BasicBlock>,
    move_insts: Vec<(BasicBlock, Inst)>,
}

impl Pass for IfConversion {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
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
    fn candidate(&self, data: &ArenaContextMut<'_>, head: BasicBlock) -> Option<Candidate> {
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
                vec![],
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
                    vec![],
                );
            }
        }

        // Triangle: one edge reaches the merge directly, the other through a
        // unique arm with a chain of safe integer operations. Prefer the
        // true-edge-arm shape; when the merge itself is jump-terminated (a
        // loop latch), `arm(f)` would still match and must not shadow the arm.
        let (arm, merge, direct_args, direct_is_true) = if let Some((_, m, _)) = self.arm(data, t) {
            if m == f {
                (t, f, branch.f_args(), false)
            } else if let Some((_, m2, _)) = self.arm(data, f) {
                if m2 == t {
                    (f, t, branch.t_args(), true)
                } else {
                    return None;
                }
            } else {
                return None;
            }
        } else if let Some((_, m2, _)) = self.arm(data, f) {
            if m2 == t {
                (f, t, branch.t_args(), true)
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

        // The arm may hold a chain of pure integer instructions ending in the
        // merge value (e.g. `c2 = eq(and(x, m), 1)` for a land/lor fold). Every
        // instruction must be single-use, feed the next link or the jump, and
        // have operands available at `head` (either dominating values or
        // earlier chain results that are hoisted together).
        let insts = data
            .layout()
            .basicblock(arm)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        // The arm may hold a chain of pure integer instructions ending in the
        // merge value (e.g. `c2 = eq(and(x, m), 1)` for a land/lor fold).
        // Every link must be single-use, feeding the next link (or the jump
        // for the last link), with operands available at `head` or from an
        // earlier link that is hoisted along with it.
        let mut move_insts = Vec::new();
        if insts.len() > 1 {
            let chain_len = insts.len() - 1;
            for (idx, &inst) in insts[..chain_len].iter().enumerate() {
                if !self.safe_arm_binary(data, inst, head, &move_insts.iter().map(|&(_, i)| i).collect::<Vec<_>>()) {
                    return None;
                }
                let users = data.inst_data(inst).used_by();
                if users.len() != 1 {
                    return None;
                }
                let feeds_next = idx + 1 < chain_len && users.contains(&insts[idx + 1]);
                let feeds_jump = idx + 1 == chain_len && users.contains(&arm_jump);
                if !feeds_next && !feeds_jump {
                    return None;
                }
                move_insts.push((arm, inst));
            }
        }
        if arm_args
            .iter()
            .any(|&value| {
                !move_insts.iter().any(|&(_, inst)| inst == value)
                    && !self.available_at(data, value, head)
            })
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
            move_insts,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_candidate(
        &self,
        data: &ArenaContextMut<'_>,
        head: BasicBlock,
        terminator: Inst,
        merge: BasicBlock,
        cond: Inst,
        true_args: &[Inst],
        false_args: &[Inst],
        remove_blocks: Vec<BasicBlock>,
        move_insts: Vec<(BasicBlock, Inst)>,
    ) -> Option<Candidate> {
        // The hoisted instructions are speculated into `head`, so `head` must
        // dominate `merge` (its operands are already checked to be available
        // at `head`, and `safe_arm_binary` excludes div/rem). The previous
        // `reaches(merge, head)` rejection blocked every loop-carried
        // accumulator (`if (bit_a==1 && bit_b==1) result += power`), which is
        // exactly the profitable case; dominance is the right precondition.
        if merge == head || !self.dominates(data, head, merge) {
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
            } else if let Some((op, lhs, rhs)) = self.fold_land_lor(data, cond, if_true, if_false) {
                MergedValue::BoolBinary { op, lhs, rhs }
            } else {
                MergedValue::Select { if_true, if_false }
            });
        }
        if !values
            .iter()
            .any(|value| matches!(value, MergedValue::Select { .. } | MergedValue::BoolBinary { .. }))
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
            move_insts,
        })
    }

    /// Fold `select(c1, c2, 0)` to `band(c1, c2)` and `select(c1, c1, c2)` to
    /// `bor(c1, c2)` when both operands are 0/1 comparison results. This is
    /// LLVM's `&&`/`||` lowering; Cranelift has no such pass.
    fn fold_land_lor(
        &self,
        data: &ArenaContextMut<'_>,
        cond: Inst,
        if_true: Inst,
        if_false: Inst,
    ) -> Option<(BinaryOp, Inst, Inst)> {
        // `c1 ? c2 : 0` with c1, c2 in {0,1} == `c1 && c2`.
        if self.zero_one(data, cond)
            && self.zero_one(data, if_true)
            && self.is_zero_const(data, if_false)
        {
            return Some((BinaryOp::And, cond, if_true));
        }
        // `c1 ? c1 : c2` with c1, c2 in {0,1} == `c1 || c2`.
        if self.zero_one(data, cond)
            && if_true == cond
            && self.zero_one(data, if_false)
        {
            return Some((BinaryOp::Or, cond, if_false));
        }
        None
    }

    /// Whether a value is guaranteed to be 0 or 1: an integer comparison
    /// result, or the constants 0/1 themselves.
    fn zero_one(&self, data: &ArenaContextMut<'_>, value: Inst) -> bool {
        match data.inst_data(value).kind() {
            InstKind::Binary(binary) => binary.op().is_compare(),
            InstKind::Integer(integer) => matches!(integer.value(), 0 | 1),
            _ => false,
        }
    }

    fn is_zero_const(&self, data: &ArenaContextMut<'_>, value: Inst) -> bool {
        matches!(data.inst_data(value).kind(), InstKind::Integer(i) if i.value() == 0)
    }

    fn empty_arm(
        &self,
        data: &ArenaContextMut<'_>,
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
        data: &ArenaContextMut<'_>,
        bb: BasicBlock,
    ) -> Option<(Inst, BasicBlock, Vec<Inst>)> {
        let jump_inst = *data.layout().basicblock(bb).insts().get_last()?;
        let InstKind::Jump(jump) = data.inst_data(jump_inst).kind() else {
            return None;
        };
        Some((jump_inst, jump.target(), jump.args().to_vec()))
    }

    /// Whether an arm instruction is speculatable: a pure i32 integer binary
    /// (no div/rem, no side effects) whose operands are either available at
    /// `head` or produced by an earlier chain link hoisted along with it.
    fn safe_arm_binary(
        &self,
        data: &ArenaContextMut<'_>,
        inst: Inst,
        head: BasicBlock,
        chain: &[Inst],
    ) -> bool {
        let InstKind::Binary(binary) = data.inst_data(inst).kind() else {
            return false;
        };
        let operand_ok = |value: Inst| chain.contains(&value) || self.available_at(data, value, head);
        data.inst_data(inst).ty().is_i32()
            && data.inst_data(binary.lhs()).ty().is_i32()
            && data.inst_data(binary.rhs()).ty().is_i32()
            && !matches!(binary.op(), BinaryOp::Div | BinaryOp::Rem)
            && operand_ok(binary.lhs())
            && operand_ok(binary.rhs())
    }

    fn available_at(&self, data: &ArenaContextMut<'_>, value: Inst, head: BasicBlock) -> bool {
        if value.is_global()
            || data.inst_data(value).is_const()
            || data.bb_data(head).params().contains(&value)
        {
            return true;
        }
        // Block parameters are not part of the instruction layout, so the
        // `parent_bb` lookup below cannot place them. Resolve their defining
        // block explicitly and apply ordinary dominance. This lets values
        // derived from the entry-block parameters (which mirror the function
        // arguments and dominate the whole body) be hoisted anywhere.
        if matches!(data.inst_data(value).kind(), InstKind::BlockArgRef(..)) {
            return self.block_of_param(data, value).is_some_and(|def_block| {
                def_block == head || self.dominates(data, def_block, head)
            });
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

    fn block_of_param(&self, data: &ArenaContextMut<'_>, value: Inst) -> Option<BasicBlock> {
        data.layout()
            .basicblocks()
            .iter()
            .find(|layout| data.bb_data(layout.bb()).params().contains(&value))
            .map(|layout| layout.bb())
    }

    fn dominates(
        &self,
        data: &ArenaContextMut<'_>,
        dominator: BasicBlock,
        block: BasicBlock,
    ) -> bool {
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

    fn exact_users(&self, data: &ArenaContextMut<'_>, bb: BasicBlock, expected: &[Inst]) -> bool {
        let users = data
            .bb_data(bb)
            .used_by()
            .iter()
            .copied()
            .filter(|&inst| data.layout().parent_bb(inst).is_some())
            .collect::<HashSet<_>>();
        users.len() == expected.len() && expected.iter().all(|inst| users.contains(inst))
    }

    fn apply(&self, data: &mut ArenaContextMut<'_>, candidate: Candidate) {
        let params = data.bb_data(candidate.merge).params().clone();

        // Remove the old terminator first so newly inserted values naturally
        // precede the replacement jump in layout order.
        data.remove_layout_inst(candidate.head, candidate.terminator);
        for (arm, inst) in &candidate.move_insts {
            // Moving preserves the instruction identity and all use-def links.
            data.layout_mut().remove_inst(*arm, *inst);
            data.layout_mut().insert_inst(candidate.head, *inst);
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
                MergedValue::BoolBinary { op, lhs, rhs } => {
                    let binary = data.new_local_inst().binary(op, lhs, rhs);
                    data.layout_mut().insert_inst(candidate.head, binary);
                    binary
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
        let __pty = data.params_ty().to_vec();
        let head = data.new_basic_block().basic_block("head".into(), __pty);
        let __params = data.bb_data(head).params().to_vec();

        data.set_params(__params);
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
        let __pty = data.params_ty().to_vec();
        let head = data.new_basic_block().basic_block("head".into(), __pty);
        let __params = data.bb_data(head).params().to_vec();

        data.set_params(__params);
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
        let __pty = data.params_ty().to_vec();
        let head = data.new_basic_block().basic_block("head".into(), __pty);
        let __params = data.bb_data(head).params().to_vec();

        data.set_params(__params);
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
        let __pty = data.params_ty().to_vec();
        let head = data.new_basic_block().basic_block("head".into(), __pty);
        let __params = data.bb_data(head).params().to_vec();

        data.set_params(__params);
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
        let __pty = data.params_ty().to_vec();
        let head = data.new_basic_block().basic_block("head".into(), __pty);
        let __params = data.bb_data(head).params().to_vec();

        data.set_params(__params);
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
    fn converts_same_target_loop_preserving_the_loop() {
        // A branch whose merge reaches the head used to be rejected wholesale;
        // M31 relaxes this to a dominance check, so the loop survives but the
        // branch becomes a select (correct: head dominates merge, and the
        // select's operands are all available at head).
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "loop".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let __pty = data.params_ty().to_vec();
        let head = data.new_basic_block().basic_block("head".into(), __pty);
        let __params = data.bb_data(head).params().to_vec();

        data.set_params(__params);
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
        let backedge = data.new_local_inst().jump(head, vec![cond]);
        data.layout_mut().insert_inst(merge, backedge);

        run(&mut program);
        let data = program.func_data(function);
        assert_eq!(blocks(data).len(), 2, "loop must survive");
        assert_eq!(select_count(data), 1);
        assert!(matches!(
            data.inst_data(backedge).kind(),
            InstKind::Jump(j) if j.target() == head
        ));
    }

    #[test]
    fn folds_land_triangle_into_band() {
        // `if (bit_a == 1 && bit_b == 1) result += power` (land shape):
        //   head: c1 = eq ...; br c1, rhs, merge(0, ...)
        //   rhs:  c2 = eq ...; jump merge(c2, ...)
        // folds to band(c1, c2) in head, rhs deleted.
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "land".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let __pty = data.params_ty().to_vec();
        let head = data.new_basic_block().basic_block("head".into(), __pty);
        let __params = data.bb_data(head).params().to_vec();

        data.set_params(__params);
        let rhs = data.new_basic_block().basic_block("rhs".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32(), Type::get_i32()]);
        for bb in [head, rhs, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let a = data.params()[0];
        let one_a = data.new_local_inst().integer(1);
        let c1 = data.new_local_inst().binary(BinaryOp::Eq, a, one_a);
        data.layout_mut().insert_inst(head, c1);
        let seven = data.new_local_inst().integer(7);
        let pass = data.new_local_inst().binary(BinaryOp::Add, a, seven);
        data.layout_mut().insert_inst(head, pass);
        let zero = data.new_local_inst().integer(0);
        let two_a = data.new_local_inst().integer(2);
        let c2 = data.new_local_inst().binary(BinaryOp::Eq, a, two_a);
        data.layout_mut().insert_inst(rhs, c2);
        let branch = data.new_local_inst().branch(c1, rhs, vec![], merge, vec![zero, pass]);
        data.layout_mut().insert_inst(head, branch);
        let rhs_jump = data.new_local_inst().jump(merge, vec![c2, pass]);
        data.layout_mut().insert_inst(rhs, rhs_jump);
        let param = data.bb_data(merge).params()[0];
        let ret = data.new_local_inst().ret(Some(param));
        data.layout_mut().insert_inst(merge, ret);

        run(&mut program);
        let data = program.func_data(function);
        assert_eq!(blocks(data).len(), 2, "rhs block must be removed");
        assert_eq!(select_count(data), 0);
        let band_count = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|layout| layout.insts())
            .filter(|&&inst| {
                matches!(data.inst_data(inst).kind(), InstKind::Binary(b) if b.op() == BinaryOp::And)
                    && data.inst_data(inst).used_by().len() == 1
            })
            .count();
        assert_eq!(band_count, 1, "one band must replace the land branch");
        assert!(data.layout().parent_bb(c2).is_some_and(|bb| bb == head));
    }

    #[test]
    fn folds_lor_triangle_into_bor() {
        // `if (bit_a == 1 || bit_b == 1) ...` (lor shape):
        //   head: c1 = eq ...; br c1, merge(c1, ...), rhs
        //   rhs:  c2 = eq ...; jump merge(c2, ...)
        // folds to bor(c1, c2) in head, rhs deleted.
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "lor".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let __pty = data.params_ty().to_vec();
        let head = data.new_basic_block().basic_block("head".into(), __pty);
        let __params = data.bb_data(head).params().to_vec();

        data.set_params(__params);
        let rhs = data.new_basic_block().basic_block("rhs".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32(), Type::get_i32()]);
        for bb in [head, rhs, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let a = data.params()[0];
        let one_a = data.new_local_inst().integer(1);
        let c1 = data.new_local_inst().binary(BinaryOp::Eq, a, one_a);
        data.layout_mut().insert_inst(head, c1);
        let seven = data.new_local_inst().integer(7);
        let pass = data.new_local_inst().binary(BinaryOp::Add, a, seven);
        data.layout_mut().insert_inst(head, pass);
        let branch = data.new_local_inst().branch(c1, merge, vec![c1, pass], rhs, vec![]);
        data.layout_mut().insert_inst(head, branch);
        let two_a = data.new_local_inst().integer(2);
        let c2 = data.new_local_inst().binary(BinaryOp::Eq, a, two_a);
        data.layout_mut().insert_inst(rhs, c2);
        let rhs_jump = data.new_local_inst().jump(merge, vec![c2, pass]);
        data.layout_mut().insert_inst(rhs, rhs_jump);
        let param = data.bb_data(merge).params()[0];
        let ret = data.new_local_inst().ret(Some(param));
        data.layout_mut().insert_inst(merge, ret);

        run(&mut program);
        let data = program.func_data(function);
        assert_eq!(blocks(data).len(), 2, "rhs block must be removed");
        assert_eq!(select_count(data), 0);
        let bor_count = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|layout| layout.insts())
            .filter(|&&inst| {
                matches!(data.inst_data(inst).kind(), InstKind::Binary(b) if b.op() == BinaryOp::Or)
                    && data.inst_data(inst).used_by().len() == 1
            })
            .count();
        assert_eq!(bor_count, 1, "one bor must replace the lor branch");
    }

    #[test]
    fn converts_loop_accumulator_with_speculation() {
        // The M31 headline: `while (len) { if (bit_a == 1 && bit_b == 1)
        // result += power; ... }` — the inner triangle's merge is reachable
        // from its head through the loop back-edge, which the old
        // `reaches(merge, head)` guard rejected. head dominates merge, so the
        // accumulator now converts to a select.
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "acc".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let __pty = data.params_ty().to_vec();
        let head = data.new_basic_block().basic_block("head".into(), __pty);
        let __params = data.bb_data(head).params().to_vec();

        data.set_params(__params);
        let arm = data.new_basic_block().basic_block("arm".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        for bb in [head, arm, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let a = data.params()[0];
        let one_a = data.new_local_inst().integer(1);
        let cond = data.new_local_inst().binary(BinaryOp::Eq, a, one_a);
        data.layout_mut().insert_inst(head, cond);
        let branch = data.new_local_inst().branch(cond, arm, vec![], merge, vec![a]);
        data.layout_mut().insert_inst(head, branch);
        // arm: result' = result + power (operands dominate head).
        let one_b = data.new_local_inst().integer(1);
        let inc = data.new_local_inst().binary(BinaryOp::Add, a, one_b);
        data.layout_mut().insert_inst(arm, inc);
        let arm_jump = data.new_local_inst().jump(merge, vec![inc]);
        data.layout_mut().insert_inst(arm, arm_jump);
        let param = data.bb_data(merge).params()[0];
        let back = data.new_local_inst().jump(head, vec![param]);
        data.layout_mut().insert_inst(merge, back);

        run(&mut program);
        let data = program.func_data(function);
        assert_eq!(blocks(data).len(), 2, "arm must be removed, loop survives");
        assert_eq!(select_count(data), 1);
        assert!(data.layout().parent_bb(inc).is_some_and(|bb| bb == head));
        assert!(matches!(
            data.inst_data(back).kind(),
            InstKind::Jump(j) if j.target() == head
        ));
    }
}
