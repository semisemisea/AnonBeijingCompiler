use crate::opt::prelude::*;

pub struct StrengthReduction;

impl Pass for StrengthReduction {
    fn run_on(&self, data: &mut ArenaContext<'_>) -> bool {
        let mut changed = self.specialize_remainder_comparisons(data);
        loop {
            let mut iteration_changed = false;
            for inst in Self::layout_insts(data) {
                if data.layout().parent_bb(inst).is_some() {
                    iteration_changed |= self.reduce_binary(data, inst);
                }
            }
            if !iteration_changed {
                return changed;
            }
            changed = true;
        }
    }
}

impl StrengthReduction {
    fn layout_insts(data: &ArenaContext<'_>) -> Vec<Inst> {
        data.layout()
            .basicblocks()
            .iter()
            .flat_map(|layout| layout.insts().iter().copied())
            .collect()
    }

    fn specialize_remainder_comparisons(&self, data: &mut ArenaContext<'_>) -> bool {
        let mut changed = false;
        for inst in Self::layout_insts(data) {
            let InstKind::Binary(compare) = data.inst_data(inst).kind().clone() else {
                continue;
            };
            if !matches!(compare.op(), BinaryOp::Eq | BinaryOp::NotEq) {
                continue;
            }
            let candidate = self
                .remainder_comparison(data, compare.lhs(), compare.rhs())
                .or_else(|| self.remainder_comparison(data, compare.rhs(), compare.lhs()));
            let Some((value, compared, mask)) = candidate else {
                continue;
            };

            let mask_value = data.new_local_inst().integer(mask);
            let masked = data
                .new_local_inst()
                .binary(BinaryOp::And, value, mask_value);
            data.layout_mut().insert_inst_before(inst, masked);
            let expected = data.new_local_inst().integer(compared);
            data.replace_inst_with(inst)
                .binary(compare.op(), masked, expected);
            changed = true;
        }
        changed
    }

    fn remainder_comparison(
        &self,
        data: &ArenaContext<'_>,
        remainder: Inst,
        constant: Inst,
    ) -> Option<(Inst, i32, i32)> {
        let compared = utils::integer_constant(data, constant)?;
        let InstKind::Binary(rem) = data.inst_data(remainder).kind() else {
            return None;
        };
        if rem.op() != BinaryOp::Rem
            || !data.inst_data(remainder).ty().is_i32()
            || !matches!(utils::integer_constant(data, rem.rhs()), Some(2 | -2))
        {
            return None;
        }
        let (expected, mask) = match compared {
            0 => (0, 1),
            1 => (1, i32::MIN | 1),
            -1 => (i32::MIN | 1, i32::MIN | 1),
            _ => return None,
        };
        Some((rem.lhs(), expected, mask))
    }

    fn reduce_binary(&self, data: &mut ArenaContext<'_>, inst: Inst) -> bool {
        let InstKind::Binary(binary) = data.inst_data(inst).kind().clone() else {
            return false;
        };
        if !data.inst_data(inst).ty().is_i32()
            || !data.inst_data(binary.lhs()).ty().is_i32()
            || !data.inst_data(binary.rhs()).ty().is_i32()
        {
            return false;
        }
        match binary.op() {
            BinaryOp::Mul => self.reduce_mul(data, inst, binary.lhs(), binary.rhs()),
            BinaryOp::Div => self.reduce_div(data, inst, binary.lhs(), binary.rhs()),
            BinaryOp::Rem => self.reduce_rem(data, inst, binary.lhs(), binary.rhs()),
            BinaryOp::Shl | BinaryOp::Shr | BinaryOp::Sar => {
                self.reduce_shift(data, inst, binary.op(), binary.lhs(), binary.rhs())
            }
            _ => false,
        }
    }

    fn reduce_mul(&self, data: &mut ArenaContext<'_>, inst: Inst, lhs: Inst, rhs: Inst) -> bool {
        let (value, constant) = match (
            utils::integer_constant(data, lhs),
            utils::integer_constant(data, rhs),
        ) {
            (Some(constant), None) => (rhs, Some(constant)),
            (_, Some(constant)) => (lhs, Some(constant)),
            _ => (lhs, None),
        };
        if let Some(constant) = constant {
            match constant {
                0 => {
                    let zero = data.new_local_inst().integer(0);
                    self.replace_with_value(data, inst, zero);
                }
                1 => self.replace_with_value(data, inst, value),
                -1 => {
                    let zero = data.new_local_inst().integer(0);
                    data.replace_inst_with(inst)
                        .binary(BinaryOp::Sub, zero, value);
                }
                positive if positive > 0 && (positive as u32).is_power_of_two() => {
                    let shift = data
                        .new_local_inst()
                        .integer(positive.trailing_zeros() as i32);
                    data.replace_inst_with(inst)
                        .binary(BinaryOp::Shl, value, shift);
                }
                _ => return false,
            }
            return true;
        }

        let Some((value, shift)) = self
            .one_shift(data, rhs)
            .map(|shift| (lhs, shift))
            .or_else(|| self.one_shift(data, lhs).map(|shift| (rhs, shift)))
        else {
            return false;
        };
        data.replace_inst_with(inst)
            .binary(BinaryOp::Shl, value, shift);
        true
    }

    fn one_shift(&self, data: &ArenaContext<'_>, inst: Inst) -> Option<Inst> {
        let InstKind::Binary(shift) = data.inst_data(inst).kind() else {
            return None;
        };
        (shift.op() == BinaryOp::Shl
            && data.inst_data(inst).ty().is_i32()
            && utils::integer_constant(data, shift.lhs()) == Some(1))
        .then(|| shift.rhs())
    }

    fn reduce_div(&self, data: &mut ArenaContext<'_>, inst: Inst, lhs: Inst, rhs: Inst) -> bool {
        let Some(divisor) = utils::integer_constant(data, rhs) else {
            return false;
        };
        let Some((shift, negative)) = utils::signed_power_of_two(divisor) else {
            return false;
        };
        if shift == 0 {
            if negative {
                let zero = data.new_local_inst().integer(0);
                data.replace_inst_with(inst)
                    .binary(BinaryOp::Sub, zero, lhs);
            } else {
                self.replace_with_value(data, inst, lhs);
            }
            return true;
        }

        let quotient = self.insert_signed_pow2_quotient(data, inst, lhs, shift);
        if negative {
            let zero = data.new_local_inst().integer(0);
            data.replace_inst_with(inst)
                .binary(BinaryOp::Sub, zero, quotient);
        } else {
            let InstKind::Binary(result) = data.inst_data(quotient).kind().clone() else {
                unreachable!()
            };
            data.replace_inst_with(inst)
                .binary(result.op(), result.lhs(), result.rhs());
            let bb = data.layout().parent_bb(quotient).unwrap();
            data.remove_layout_inst(bb, quotient);
        }
        true
    }

    fn reduce_rem(&self, data: &mut ArenaContext<'_>, inst: Inst, lhs: Inst, rhs: Inst) -> bool {
        if data.inst_data(inst).used_by().is_empty() {
            return false;
        }
        let Some(divisor) = utils::integer_constant(data, rhs) else {
            return false;
        };
        let Some((shift, _)) = utils::signed_power_of_two(divisor) else {
            return false;
        };
        if shift == 0 {
            let zero = data.new_local_inst().integer(0);
            self.replace_with_value(data, inst, zero);
            return true;
        }

        let shift_minus_one = data.new_local_inst().integer(i32::from(shift - 1));
        let sign = self.insert_binary_before(data, inst, BinaryOp::Sar, lhs, shift_minus_one);
        let inverse_shift = data.new_local_inst().integer(i32::from(32 - shift));
        let bias = self.insert_binary_before(data, inst, BinaryOp::Shr, sign, inverse_shift);
        let biased = self.insert_binary_before(data, inst, BinaryOp::Add, lhs, bias);
        let mask = (1u32 << shift).wrapping_neg() as i32;
        let mask = data.new_local_inst().integer(mask);
        let masked = self.insert_binary_before(data, inst, BinaryOp::And, biased, mask);
        data.replace_inst_with(inst)
            .binary(BinaryOp::Sub, lhs, masked);
        true
    }

    fn insert_signed_pow2_quotient(
        &self,
        data: &mut ArenaContext<'_>,
        before: Inst,
        value: Inst,
        shift: u8,
    ) -> Inst {
        let shift_minus_one = data.new_local_inst().integer(i32::from(shift - 1));
        let sign = self.insert_binary_before(data, before, BinaryOp::Sar, value, shift_minus_one);
        let inverse_shift = data.new_local_inst().integer(i32::from(32 - shift));
        let bias = self.insert_binary_before(data, before, BinaryOp::Shr, sign, inverse_shift);
        let biased = self.insert_binary_before(data, before, BinaryOp::Add, value, bias);
        let shift = data.new_local_inst().integer(i32::from(shift));
        self.insert_binary_before(data, before, BinaryOp::Sar, biased, shift)
    }

    fn reduce_shift(
        &self,
        data: &mut ArenaContext<'_>,
        inst: Inst,
        op: BinaryOp,
        lhs: Inst,
        rhs: Inst,
    ) -> bool {
        let Some(outer) = utils::integer_constant(data, rhs).map(|value| (value as u32) & 31)
        else {
            return false;
        };
        if outer == 0 {
            self.replace_with_value(data, inst, lhs);
            return true;
        }
        let InstKind::Binary(inner) = data.inst_data(lhs).kind().clone() else {
            return false;
        };
        if inner.op() != op || !data.inst_data(lhs).ty().is_i32() {
            return false;
        }
        let Some(inner_amount) =
            utils::integer_constant(data, inner.rhs()).map(|value| (value as u32) & 31)
        else {
            return false;
        };
        let total = inner_amount + outer;
        if total < 32 {
            let amount = data.new_local_inst().integer(total as i32);
            data.replace_inst_with(inst).binary(op, inner.lhs(), amount);
        } else if matches!(op, BinaryOp::Shl | BinaryOp::Shr) {
            let zero = data.new_local_inst().integer(0);
            self.replace_with_value(data, inst, zero);
        } else {
            return false;
        }
        true
    }

    fn insert_binary_before(
        &self,
        data: &mut ArenaContext<'_>,
        before: Inst,
        op: BinaryOp,
        lhs: Inst,
        rhs: Inst,
    ) -> Inst {
        let inst = data.new_local_inst().binary(op, lhs, rhs);
        data.layout_mut().insert_inst_before(before, inst);
        inst
    }

    fn replace_with_value(&self, data: &mut ArenaContext<'_>, inst: Inst, replacement: Inst) {
        utils::visit_and_replace(data, inst, replacement);
        let bb = data.layout().parent_bb(inst).unwrap();
        data.remove_layout_inst(bb, inst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single_binary(op: BinaryOp, constant: i32, constant_on_left: bool) -> (Program, Function) {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "binary".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let x = data.params()[0];
        let constant = data.new_local_inst().integer(constant);
        let (lhs, rhs) = if constant_on_left {
            (constant, x)
        } else {
            (x, constant)
        };
        let binary = data.new_local_inst().binary(op, lhs, rhs);
        let ret = data.new_local_inst().ret(Some(binary));
        data.layout_mut().insert_inst(entry, binary);
        data.layout_mut().insert_inst(entry, ret);
        (program, function)
    }

    fn run(program: &mut Program) -> bool {
        StrengthReduction.run(program)
    }

    fn returned_value(data: &FunctionData) -> Inst {
        let entry = data.layout().entry_bb().unwrap().bb();
        let ret = data.layout().basicblock(entry).terminator();
        let InstKind::Return(ret) = data.inst_data(ret).kind() else {
            panic!("expected return")
        };
        ret.value().unwrap()
    }

    fn binary(data: &FunctionData, inst: Inst) -> crate::ir::Binary {
        let InstKind::Binary(binary) = data.inst_data(inst).kind() else {
            panic!("expected binary")
        };
        binary.clone()
    }

    fn constant(data: &FunctionData, inst: Inst) -> i32 {
        let InstKind::Integer(integer) = data.inst_data(inst).kind() else {
            panic!("expected integer")
        };
        integer.value()
    }

    #[test]
    fn reduces_integer_multiplication_identities_and_powers_of_two() {
        for (value, left, expected) in [
            (0, false, None),
            (0, true, None),
            (1, false, None),
            (1, true, None),
            (2, false, Some((BinaryOp::Shl, 1))),
            (8, true, Some((BinaryOp::Shl, 3))),
            (1 << 30, false, Some((BinaryOp::Shl, 30))),
        ] {
            let (mut program, function) = single_binary(BinaryOp::Mul, value, left);
            assert!(run(&mut program));
            let data = program.func_data(function);
            let result = returned_value(data);
            if let Some((op, amount)) = expected {
                let result = binary(data, result);
                assert_eq!(result.op(), op);
                assert_eq!(constant(data, result.rhs()), amount);
            } else if value == 0 {
                assert_eq!(constant(data, result), 0);
            } else {
                assert_eq!(result, data.params()[0]);
            }
            assert!(!run(&mut program));
        }
    }

    #[test]
    fn reduces_negation_and_variable_power_of_two_multiplication() {
        let (mut program, function) = single_binary(BinaryOp::Mul, -1, false);
        assert!(run(&mut program));
        let result = binary(
            program.func_data(function),
            returned_value(program.func_data(function)),
        );
        assert_eq!(result.op(), BinaryOp::Sub);

        for shift_on_left in [false, true] {
            let mut program = Program::new();
            let function = program.new_function(
                Type::get_i32(),
                "variable_shift".into(),
                vec![Type::get_i32(), Type::get_i32()],
            );
            let data = program.func_data_mut(function);
            let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
            data.layout_mut().push_bb_back(entry);
            let x = data.params()[0];
            let amount = data.params()[1];
            let one = data.new_local_inst().integer(1);
            let shift = data.new_local_inst().binary(BinaryOp::Shl, one, amount);
            let (lhs, rhs) = if shift_on_left {
                (shift, x)
            } else {
                (x, shift)
            };
            let mul = data.new_local_inst().binary(BinaryOp::Mul, lhs, rhs);
            let ret = data.new_local_inst().ret(Some(mul));
            for inst in [shift, mul, ret] {
                data.layout_mut().insert_inst(entry, inst);
            }
            assert!(run(&mut program));
            let result = binary(
                program.func_data(function),
                returned_value(program.func_data(function)),
            );
            assert_eq!(result.op(), BinaryOp::Shl);
            assert_eq!(result.lhs(), program.func_data(function).params()[0]);
            assert_eq!(result.rhs(), program.func_data(function).params()[1]);
        }
    }

    #[test]
    fn leaves_non_power_of_two_and_float_multiplication_unchanged() {
        let (mut program, function) = single_binary(BinaryOp::Mul, 3, false);
        assert!(!run(&mut program));
        assert_eq!(
            binary(
                program.func_data(function),
                returned_value(program.func_data(function))
            )
            .op(),
            BinaryOp::Mul
        );

        let mut program = Program::new();
        let function = program.new_function(Type::get_f32(), "float".into(), vec![Type::get_f32()]);
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let x = data.params()[0];
        let two = data.new_local_inst().float(2.0);
        let mul = data.new_local_inst().binary(BinaryOp::Mul, x, two);
        let ret = data.new_local_inst().ret(Some(mul));
        data.layout_mut().insert_inst(entry, mul);
        data.layout_mut().insert_inst(entry, ret);
        assert!(!run(&mut program));
    }

    #[test]
    fn reduces_signed_power_of_two_division_and_remainder() {
        for (op, divisor, final_op) in [
            (BinaryOp::Div, 2, BinaryOp::Sar),
            (BinaryOp::Div, -4, BinaryOp::Sub),
            (BinaryOp::Div, i32::MIN, BinaryOp::Sub),
            (BinaryOp::Rem, 8, BinaryOp::Sub),
            (BinaryOp::Rem, -4, BinaryOp::Sub),
            (BinaryOp::Rem, i32::MIN, BinaryOp::Sub),
        ] {
            let (mut program, function) = single_binary(op, divisor, false);
            assert!(run(&mut program), "{op:?} by {divisor}");
            let data = program.func_data(function);
            assert_eq!(binary(data, returned_value(data)).op(), final_op);
            assert!(data
                .layout()
                .entry_bb()
                .unwrap()
                .insts()
                .iter()
                .all(|&inst| !matches!(data.inst_data(inst).kind(), InstKind::Binary(b) if b.op() == BinaryOp::Div || b.op() == BinaryOp::Rem)));
            assert!(!run(&mut program));
        }
    }

    #[test]
    fn reduces_division_and_remainder_by_one_but_leaves_three() {
        for divisor in [1, -1] {
            for op in [BinaryOp::Div, BinaryOp::Rem] {
                let (mut program, _) = single_binary(op, divisor, false);
                assert!(run(&mut program));
                assert!(!run(&mut program));
            }
        }
        for op in [BinaryOp::Div, BinaryOp::Rem] {
            let (mut program, function) = single_binary(op, 3, false);
            assert!(!run(&mut program));
            assert_eq!(
                binary(
                    program.func_data(function),
                    returned_value(program.func_data(function))
                )
                .op(),
                op
            );
        }
    }

    #[test]
    fn specializes_remainder_by_two_comparisons() {
        for (divisor, compared, op, swapped, mask, expected) in [
            (2, 0, BinaryOp::Eq, false, 1, 0),
            (-2, 1, BinaryOp::Eq, true, i32::MIN | 1, 1),
            (2, -1, BinaryOp::NotEq, false, i32::MIN | 1, i32::MIN | 1),
        ] {
            let mut program = Program::new();
            let function =
                program.new_function(Type::get_i32(), "compare".into(), vec![Type::get_i32()]);
            let data = program.func_data_mut(function);
            let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
            data.layout_mut().push_bb_back(entry);
            let x = data.params()[0];
            let divisor = data.new_local_inst().integer(divisor);
            let rem = data.new_local_inst().binary(BinaryOp::Rem, x, divisor);
            let compared = data.new_local_inst().integer(compared);
            let (lhs, rhs) = if swapped {
                (compared, rem)
            } else {
                (rem, compared)
            };
            let compare = data.new_local_inst().binary(op, lhs, rhs);
            let ret = data.new_local_inst().ret(Some(compare));
            for inst in [rem, compare, ret] {
                data.layout_mut().insert_inst(entry, inst);
            }

            assert!(run(&mut program));
            let data = program.func_data(function);
            let compare = binary(data, returned_value(data));
            assert_eq!(compare.op(), op);
            assert_eq!(constant(data, compare.rhs()), expected);
            let masked = binary(data, compare.lhs());
            assert_eq!(masked.op(), BinaryOp::And);
            assert_eq!(constant(data, masked.rhs()), mask);
            assert!(!run(&mut program));
        }
    }

    #[test]
    fn preserves_numeric_remainder_user_while_specializing_comparison() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "mixed".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let x = data.params()[0];
        let two = data.new_local_inst().integer(2);
        let one = data.new_local_inst().integer(1);
        let rem = data.new_local_inst().binary(BinaryOp::Rem, x, two);
        let compare = data.new_local_inst().binary(BinaryOp::Eq, rem, one);
        let sum = data.new_local_inst().binary(BinaryOp::Add, rem, compare);
        let ret = data.new_local_inst().ret(Some(sum));
        for inst in [rem, compare, sum, ret] {
            data.layout_mut().insert_inst(entry, inst);
        }
        assert!(run(&mut program));
        let data = program.func_data(function);
        assert!(data
            .layout()
            .entry_bb()
            .unwrap()
            .insts()
            .iter()
            .all(|&inst| !matches!(data.inst_data(inst).kind(), InstKind::Binary(b) if b.op() == BinaryOp::Rem)));
    }

    #[test]
    fn simplifies_zero_and_consecutive_shifts() {
        for op in [BinaryOp::Shl, BinaryOp::Shr, BinaryOp::Sar] {
            let (mut program, function) = single_binary(op, 0, false);
            assert!(run(&mut program));
            assert_eq!(
                returned_value(program.func_data(function)),
                program.func_data(function).params()[0]
            );
        }

        for (op, expected) in [
            (BinaryOp::Shl, Some(7)),
            (BinaryOp::Shr, Some(7)),
            (BinaryOp::Sar, Some(7)),
        ] {
            let mut program = Program::new();
            let function =
                program.new_function(Type::get_i32(), "shifts".into(), vec![Type::get_i32()]);
            let data = program.func_data_mut(function);
            let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
            data.layout_mut().push_bb_back(entry);
            let x = data.params()[0];
            let three = data.new_local_inst().integer(3);
            let four = data.new_local_inst().integer(4);
            let inner = data.new_local_inst().binary(op, x, three);
            let outer = data.new_local_inst().binary(op, inner, four);
            let ret = data.new_local_inst().ret(Some(outer));
            for inst in [inner, outer, ret] {
                data.layout_mut().insert_inst(entry, inst);
            }
            assert!(run(&mut program));
            let result = binary(
                program.func_data(function),
                returned_value(program.func_data(function)),
            );
            assert_eq!(
                constant(program.func_data(function), result.rhs()),
                expected.unwrap()
            );
        }

        for op in [BinaryOp::Shl, BinaryOp::Shr] {
            let mut program = Program::new();
            let function =
                program.new_function(Type::get_i32(), "wide_shift".into(), vec![Type::get_i32()]);
            let data = program.func_data_mut(function);
            let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
            data.layout_mut().push_bb_back(entry);
            let x = data.params()[0];
            let sixteen = data.new_local_inst().integer(16);
            let inner = data.new_local_inst().binary(op, x, sixteen);
            let outer = data.new_local_inst().binary(op, inner, sixteen);
            let ret = data.new_local_inst().ret(Some(outer));
            for inst in [inner, outer, ret] {
                data.layout_mut().insert_inst(entry, inst);
            }
            assert!(run(&mut program));
            assert_eq!(
                constant(
                    program.func_data(function),
                    returned_value(program.func_data(function))
                ),
                0
            );
        }
    }

    #[test]
    fn signed_power_of_two_formulas_match_wrapping_operations() {
        let values = [0, 1, -1, 2, -2, 3, -3, i32::MAX, i32::MIN];
        for divisor in [1, -1, 2, -2, 4, -4, 8, -8, i32::MIN] {
            let (shift, negative) = utils::signed_power_of_two(divisor).unwrap();
            for value in values {
                let quotient = if shift == 0 {
                    if negative {
                        value.wrapping_neg()
                    } else {
                        value
                    }
                } else {
                    let sign = value.wrapping_shr(u32::from(shift - 1));
                    let bias = (sign as u32).wrapping_shr(u32::from(32 - shift)) as i32;
                    let quotient = value.wrapping_add(bias).wrapping_shr(u32::from(shift));
                    if negative {
                        quotient.wrapping_neg()
                    } else {
                        quotient
                    }
                };
                let remainder = if shift == 0 {
                    0
                } else {
                    let t1 = value.wrapping_shr(u32::from(shift - 1));
                    let t2 = (t1 as u32).wrapping_shr(u32::from(32 - shift)) as i32;
                    let mask = (1u32 << shift).wrapping_neg() as i32;
                    value.wrapping_sub(value.wrapping_add(t2) & mask)
                };
                assert_eq!(quotient, value.wrapping_div(divisor), "{value} / {divisor}");
                assert_eq!(
                    remainder,
                    value.wrapping_rem(divisor),
                    "{value} % {divisor}"
                );
            }
        }
    }
}
