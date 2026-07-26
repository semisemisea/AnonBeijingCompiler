use crate::ir::{Binary, Branch, Select};
use crate::opt::prelude::*;

/// Canonicalizes boolean values without conflating them with arbitrary i32
/// truth values. Comparisons produce canonical `0`/`1`; branches and selects
/// merely test their condition for zero versus nonzero.
pub struct BooleanSimplification;

impl Pass for BooleanSimplification {
    fn run_on(&self, data: &mut ArenaContext<'_>) -> bool {
        let mut changed = false;
        loop {
            let insts = data
                .layout()
                .basicblocks()
                .iter()
                .flat_map(|layout| layout.insts().iter().copied())
                .collect::<Vec<_>>();
            if !insts.into_iter().any(|inst| self.simplify_inst(data, inst)) {
                return changed;
            }
            changed = true;
        }
    }
}

impl BooleanSimplification {
    fn simplify_inst(&self, data: &mut ArenaContext<'_>, inst: Inst) -> bool {
        match data.inst_data(inst).kind().clone() {
            InstKind::Binary(binary) => self.simplify_binary(data, inst, binary),
            InstKind::Branch(branch) => self.simplify_branch(data, inst, branch),
            InstKind::Select(select) => self.simplify_select(data, inst, select),
            _ => false,
        }
    }

    fn simplify_binary(&self, data: &mut ArenaContext<'_>, inst: Inst, binary: Binary) -> bool {
        if binary.op().is_compare()
            && data.inst_data(binary.lhs()).ty().is_i32()
            && binary.lhs() == binary.rhs()
        {
            let value = match binary.op() {
                BinaryOp::Eq | BinaryOp::Ge | BinaryOp::Le => 1,
                BinaryOp::NotEq | BinaryOp::Gt | BinaryOp::Lt => 0,
                _ => unreachable!(),
            };
            let replacement = data.new_local_inst().integer(value);
            self.replace_value(data, inst, replacement);
            return true;
        }

        let Some((value, expected)) = self.boolean_comparison(data, &binary) else {
            return false;
        };
        if !Self::is_canonical_bool(data, value, &mut HashSet::new()) {
            return false;
        }

        match (binary.op(), expected) {
            (BinaryOp::NotEq, 0) | (BinaryOp::Eq, 1) => {
                self.replace_value(data, inst, value);
            }
            (BinaryOp::Eq, 0) | (BinaryOp::NotEq, 1) => {
                let InstKind::Binary(inner) = data.inst_data(value).kind().clone() else {
                    return false;
                };
                if !inner.op().is_compare() || !data.inst_data(inner.lhs()).ty().is_i32() {
                    return false;
                }
                let Some(op) = inner.op().complement_integer_compare() else {
                    return false;
                };
                data.replace_inst_with(inst)
                    .binary(op, inner.lhs(), inner.rhs());
            }
            _ => return false,
        }
        true
    }

    fn simplify_branch(&self, data: &mut ArenaContext<'_>, inst: Inst, branch: Branch) -> bool {
        let Some((value, expected)) = self.zero_comparison(data, branch.cond()) else {
            return false;
        };
        if !data.inst_data(value).ty().is_i32() {
            return false;
        }
        if expected {
            data.replace_inst_with(inst).branch(
                value,
                branch.f_target(),
                branch.f_args().to_vec(),
                branch.t_target(),
                branch.t_args().to_vec(),
            );
        } else {
            data.replace_inst_with(inst).branch(
                value,
                branch.t_target(),
                branch.t_args().to_vec(),
                branch.f_target(),
                branch.f_args().to_vec(),
            );
        }
        true
    }

    fn simplify_select(&self, data: &mut ArenaContext<'_>, inst: Inst, select: Select) -> bool {
        if select.if_true() == select.if_false() {
            self.replace_value(data, inst, select.if_true());
            return true;
        }
        if select.if_true() == select.cond() && self.is_zero(data, select.if_false()) {
            self.replace_value(data, inst, select.cond());
            return true;
        }

        if let Some((value, is_eq)) = self.zero_comparison(data, select.cond()) {
            if data.inst_data(value).ty().is_i32() {
                if is_eq {
                    data.replace_inst_with(inst)
                        .select(value, select.if_false(), select.if_true());
                } else {
                    data.replace_inst_with(inst)
                        .select(value, select.if_true(), select.if_false());
                }
                return true;
            }
        }

        if !data.inst_data(inst).ty().is_i32()
            || !Self::is_canonical_bool(data, select.cond(), &mut HashSet::new())
        {
            return false;
        }
        if self.is_one(data, select.if_true()) && self.is_zero(data, select.if_false()) {
            self.replace_value(data, inst, select.cond());
            return true;
        }
        if self.is_zero(data, select.if_true()) && self.is_one(data, select.if_false()) {
            let zero = data.new_local_inst().integer(0);
            data.replace_inst_with(inst)
                .binary(BinaryOp::Eq, select.cond(), zero);
            return true;
        }
        false
    }

    /// Returns `(value, is_eq)` for `value == 0` / `value != 0`.
    fn zero_comparison(&self, data: &ArenaContext<'_>, inst: Inst) -> Option<(Inst, bool)> {
        let InstKind::Binary(binary) = data.inst_data(inst).kind() else {
            return None;
        };
        let is_eq = match binary.op() {
            BinaryOp::Eq => true,
            BinaryOp::NotEq => false,
            _ => return None,
        };
        if self.is_zero(data, binary.lhs()) {
            Some((binary.rhs(), is_eq))
        } else if self.is_zero(data, binary.rhs()) {
            Some((binary.lhs(), is_eq))
        } else {
            None
        }
    }

    /// Returns `(canonical_boolean, expected_value)` for equality-like tests
    /// against 0 or 1.
    fn boolean_comparison(&self, data: &ArenaContext<'_>, binary: &Binary) -> Option<(Inst, i32)> {
        if !matches!(binary.op(), BinaryOp::Eq | BinaryOp::NotEq) {
            return None;
        }
        if let Some(value) = utils::integer_constant(data, binary.lhs()) {
            if matches!(value, 0 | 1) {
                return Some((binary.rhs(), value));
            }
        }
        if let Some(value) = utils::integer_constant(data, binary.rhs()) {
            if matches!(value, 0 | 1) {
                return Some((binary.lhs(), value));
            }
        }
        None
    }

    fn is_canonical_bool(
        data: &ArenaContext<'_>,
        value: Inst,
        visiting: &mut HashSet<Inst>,
    ) -> bool {
        if !visiting.insert(value) {
            return false;
        }
        let result = matches!(utils::integer_constant(data, value), Some(0 | 1))
            || matches!(data.inst_data(value).kind(), InstKind::Binary(binary) if binary.op().is_compare())
            || matches!(data.inst_data(value).kind(), InstKind::Select(select)
                if Self::is_canonical_bool(data, select.if_true(), visiting)
                    && Self::is_canonical_bool(data, select.if_false(), visiting));
        visiting.remove(&value);
        result
    }

    fn is_zero(&self, data: &ArenaContext<'_>, inst: Inst) -> bool {
        utils::integer_constant(data, inst) == Some(0)
    }

    fn is_one(&self, data: &ArenaContext<'_>, inst: Inst) -> bool {
        utils::integer_constant(data, inst) == Some(1)
    }

    fn replace_value(&self, data: &mut ArenaContext<'_>, inst: Inst, replacement: Inst) {
        utils::visit_and_replace(data, inst, replacement);
        assert!(data.inst_data(inst).used_by().is_empty());
        let bb = data.layout().parent_bb(inst).unwrap();
        data.remove_layout_inst(bb, inst);
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

    fn run(program: &mut Program) {
        BooleanSimplification.run(program);
    }

    fn returned_value(data: &FunctionData, bb: BasicBlock) -> Inst {
        let ret = data.layout().basicblock(bb).terminator();
        let InstKind::Return(ret) = data.inst_data(ret).kind() else {
            panic!("expected return");
        };
        ret.value().unwrap()
    }

    #[test]
    fn removes_compare_truthiness_wrapper() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "truthy".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let x = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, x, zero);
        let wrapped = data.new_local_inst().binary(BinaryOp::NotEq, compare, zero);
        let ret = data.new_local_inst().ret(Some(wrapped));
        for inst in [compare, wrapped, ret] {
            data.layout_mut().insert_inst(entry, inst);
        }

        run(&mut program);
        assert_eq!(returned_value(program.func_data(function), entry), compare);
    }

    #[test]
    fn complements_integer_compare_against_zero() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "inverse".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let x = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, x, zero);
        let inverse = data.new_local_inst().binary(BinaryOp::Eq, compare, zero);
        let ret = data.new_local_inst().ret(Some(inverse));
        for inst in [compare, inverse, ret] {
            data.layout_mut().insert_inst(entry, inst);
        }

        run(&mut program);
        let data = program.func_data(function);
        let InstKind::Binary(result) = data.inst_data(inverse).kind() else {
            panic!("expected rewritten comparison");
        };
        assert_eq!(result.op(), BinaryOp::Ge);
        assert_eq!((result.lhs(), result.rhs()), (x, zero));
        assert_eq!(returned_value(data, entry), inverse);
    }

    #[test]
    fn retains_float_relational_complement() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "float".into(), vec![Type::get_f32()]);
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let x = data.params()[0];
        let zero_float = data.new_local_inst().float(0.0);
        let zero_int = data.new_local_inst().integer(0);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, x, zero_float);
        let inverse = data
            .new_local_inst()
            .binary(BinaryOp::Eq, compare, zero_int);
        let ret = data.new_local_inst().ret(Some(inverse));
        for inst in [compare, inverse, ret] {
            data.layout_mut().insert_inst(entry, inst);
        }

        run(&mut program);
        let data = program.func_data(function);
        assert!(matches!(
            data.inst_data(inverse).kind(),
            InstKind::Binary(binary)
                if binary.op() == BinaryOp::Eq && binary.lhs() == compare && binary.rhs() == zero_int
        ));
    }

    #[test]
    fn strips_branch_truthiness_and_swaps_zero_test_edges() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_unit(), "branch".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        let yes = data.new_basic_block().basic_block("yes".into(), vec![]);
        let no = data.new_basic_block().basic_block("no".into(), vec![]);
        for bb in [entry, yes, no] {
            data.layout_mut().push_bb_back(bb);
        }
        let x = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let cond = data.new_local_inst().binary(BinaryOp::Eq, x, zero);
        let branch = data.new_local_inst().branch(cond, yes, vec![], no, vec![]);
        let yes_ret = data.new_local_inst().ret(None);
        let no_ret = data.new_local_inst().ret(None);
        for (bb, inst) in [(entry, cond), (entry, branch), (yes, yes_ret), (no, no_ret)] {
            data.layout_mut().insert_inst(bb, inst);
        }

        run(&mut program);
        let data = program.func_data(function);
        let InstKind::Branch(branch) = data.inst_data(branch).kind() else {
            panic!("expected branch");
        };
        assert_eq!(branch.cond(), x);
        assert_eq!((branch.t_target(), branch.f_target()), (no, yes));
    }

    #[test]
    fn simplifies_select_truthiness_without_substituting_arbitrary_truthy_value() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "select".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let x = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let one = data.new_local_inst().integer(1);
        let cond = data.new_local_inst().binary(BinaryOp::NotEq, x, zero);
        let select = data.new_local_inst().select(cond, one, zero);
        let ret = data.new_local_inst().ret(Some(select));
        for inst in [cond, select, ret] {
            data.layout_mut().insert_inst(entry, inst);
        }

        run(&mut program);
        let data = program.func_data(function);
        assert!(matches!(
            data.inst_data(returned_value(data, entry)).kind(),
            InstKind::Select(result)
                if result.cond() == x && result.if_true() == one && result.if_false() == zero
        ));
    }

    #[test]
    fn swaps_select_arms_for_zero_test() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "select_zero".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let x = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let if_true = data.new_local_inst().integer(10);
        let if_false = data.new_local_inst().integer(20);
        let cond = data.new_local_inst().binary(BinaryOp::Eq, x, zero);
        let select = data.new_local_inst().select(cond, if_true, if_false);
        let ret = data.new_local_inst().ret(Some(select));
        for inst in [cond, select, ret] {
            data.layout_mut().insert_inst(entry, inst);
        }

        run(&mut program);
        let data = program.func_data(function);
        assert!(matches!(
            data.inst_data(returned_value(data, entry)).kind(),
            InstKind::Select(result)
                if result.cond() == x && result.if_true() == if_false && result.if_false() == if_true
        ));
    }
}
