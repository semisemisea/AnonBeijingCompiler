    use super::{
        MulConstForm, fold_mul_constant, nzcv_making_cond_false, nzcv_making_cond_true,
        signed_power_of_two,
    };
    use crate::instructions::{Cond, ImmShift, MInst};
    use crate::regs::OperandSize;
    use raana_ir::ir::{BinaryOp, Program, Type};
    use taki_mir::{
        reg_alloc::reg::{PReg, RegClass, SpillSlot},
        register::{Reg, VRegAllocator},
        types::{F32, I32, I64},
    };

    #[test]
    fn register_aliases_resolve_transitively() {
        let mut allocator = VRegAllocator::<MInst>::with_capaticy(3);
        let first = allocator.alloc(I32);
        let second = allocator.alloc(I32);
        let canonical = allocator.alloc(I32);

        allocator.set_reg_alias(first, second);
        allocator.set_reg_alias(second, canonical);

        assert_eq!(
            allocator.resolve_alias(first.to_virtual_reg().unwrap()),
            canonical.to_virtual_reg().unwrap()
        );
        assert_eq!(
            allocator.resolve_alias(second.to_virtual_reg().unwrap()),
            canonical.to_virtual_reg().unwrap()
        );
    }

    #[test]
    #[should_panic(expected = "register alias source was already assigned")]
    fn register_alias_source_is_single_assignment() {
        let mut allocator = VRegAllocator::<MInst>::with_capaticy(3);
        let source = allocator.alloc(I32);
        let first = allocator.alloc(I32);
        let second = allocator.alloc(I32);

        allocator.set_reg_alias(source, first);
        allocator.set_reg_alias(source, second);
    }

    #[test]
    #[should_panic(expected = "register alias would form a cycle")]
    fn register_alias_cycles_are_rejected() {
        let mut allocator = VRegAllocator::<MInst>::with_capaticy(2);
        let first = allocator.alloc(I32);
        let second = allocator.alloc(I32);

        allocator.set_reg_alias(first, second);
        allocator.set_reg_alias(second, first);
    }

    #[test]
    #[should_panic(expected = "register aliases must have identical lowered types")]
    fn register_aliases_reject_different_classes() {
        let mut allocator = VRegAllocator::<MInst>::with_capaticy(2);
        let integer = allocator.alloc(I32);
        let float = allocator.alloc(F32);

        allocator.set_reg_alias(integer, float);
    }

    #[test]
    #[should_panic(expected = "register aliases must have identical lowered types")]
    fn register_aliases_reject_different_widths_in_the_same_class() {
        let mut allocator = VRegAllocator::<MInst>::with_capaticy(2);
        let narrow = allocator.alloc(I32);
        let wide = allocator.alloc(I64);

        allocator.set_reg_alias(narrow, wide);
    }

    #[test]
    #[should_panic(expected = "register alias target must be a virtual register")]
    fn register_aliases_reject_physical_registers() {
        let mut allocator = VRegAllocator::<MInst>::with_capaticy(1);
        let source = allocator.alloc(I32);
        let physical = Reg::from_physical_reg(PReg::new(0, RegClass::Int));

        allocator.set_reg_alias(source, physical);
    }

    #[test]
    #[should_panic(expected = "register alias target must be a virtual register")]
    fn register_aliases_reject_spill_slots() {
        let mut allocator = VRegAllocator::<MInst>::with_capaticy(1);
        let source = allocator.alloc(I32);
        let spill = Reg::from_spillslot(SpillSlot::new(0));

        allocator.set_reg_alias(source, spill);
    }

    /// Emits `parameter <op> divisor` as a whole function and returns its
    /// AArch64 assembly.
    fn compile_constant_binary(op: BinaryOp, divisor: i32) -> String {
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "constant".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let numerator = data.params()[0];
        let constant = data.new_local_inst().integer(divisor);
        let binary = data.new_local_inst().binary(op, numerator, constant);
        let ret = data.new_local_inst().ret(Some(binary));
        data.layout_mut().insert_inst(entry, binary);
        data.layout_mut().insert_inst(entry, ret);
        taki_mir::compile::<crate::lower::AArch64Backend>(&program)
    }

    #[test]
    fn division_by_a_constant_replaces_sdiv_with_a_multiply_high() {
        for divisor in [3, 7, -7, 100, 1000000007, i32::MAX] {
            let assembly = compile_constant_binary(BinaryOp::Div, divisor);
            assert!(!assembly.contains("sdiv"), "{divisor}:\n{assembly}");
            assert!(assembly.contains("smull"), "{divisor}:\n{assembly}");
        }
    }

    #[test]
    fn remainder_by_a_constant_multiplies_the_quotient_back() {
        let assembly = compile_constant_binary(BinaryOp::Rem, 7);
        assert!(!assembly.contains("sdiv"), "{assembly}");
        assert!(assembly.contains("smull"), "{assembly}");
        assert!(assembly.contains("msub"), "{assembly}");
    }

    #[test]
    fn cheaper_divisors_keep_their_existing_sequences() {
        // Powers of two stay on the shift sequence, and a divisor of one
        // disappears entirely.
        for divisor in [2, -8, i32::MIN] {
            for op in [BinaryOp::Div, BinaryOp::Rem] {
                let assembly = compile_constant_binary(op, divisor);
                assert!(!assembly.contains("smull"), "{op:?} {divisor}:\n{assembly}");
                assert!(!assembly.contains("sdiv"), "{op:?} {divisor}:\n{assembly}");
            }
        }
        let assembly = compile_constant_binary(BinaryOp::Div, 1);
        assert!(!assembly.contains("smull"), "{assembly}");
        assert!(!assembly.contains("sdiv"), "{assembly}");
    }

    #[test]
    fn division_by_a_variable_still_uses_sdiv() {
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "variable".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let numerator = data.params()[0];
        let divisor = data.params()[1];
        let binary = data
            .new_local_inst()
            .binary(BinaryOp::Div, numerator, divisor);
        let ret = data.new_local_inst().ret(Some(binary));
        data.layout_mut().insert_inst(entry, binary);
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        assert!(assembly.contains("sdiv"), "{assembly}");
    }

    #[test]
    fn minus_one_subtraction_selects_bitwise_not() {
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "bitwise_not".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let value = data.params()[0];
        let minus_one = data.new_local_inst().integer(-1);
        let result = data
            .new_local_inst()
            .binary(BinaryOp::Sub, minus_one, value);
        let ret = data.new_local_inst().ret(Some(result));
        data.layout_mut().insert_inst(entry, result);
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        assert!(assembly.contains("orn "), "{assembly}");
        assert!(!assembly.contains("movn "), "{assembly}");
    }

    #[test]
    fn classifies_signed_power_of_two_divisors() {
        assert_eq!(signed_power_of_two(0), None);
        assert_eq!(signed_power_of_two(1), Some((0, false)));
        assert_eq!(signed_power_of_two(-1), Some((0, true)));
        assert_eq!(signed_power_of_two(2), Some((1, false)));
        assert_eq!(signed_power_of_two(-2), Some((1, true)));
        assert_eq!(signed_power_of_two(1 << 30), Some((30, false)));
        assert_eq!(signed_power_of_two(-(1 << 30)), Some((30, true)));
        assert_eq!(signed_power_of_two(i32::MIN), Some((31, true)));
        assert_eq!(signed_power_of_two(3), None);
        assert_eq!(signed_power_of_two(-3), None);
    }

    #[test]
    fn signed_power_of_two_formula_truncates_toward_zero() {
        let dividends = [i32::MIN, -17, -9, -8, -7, -1, 0, 1, 7, 8, 9, 17, i32::MAX];
        let divisors = [1, -1, 2, -2, 4, -4, 8, -8, 1 << 30, -(1 << 30), i32::MIN];

        for dividend in dividends {
            for divisor in divisors {
                let (shift, negate) = signed_power_of_two(divisor).unwrap();
                let positive_quotient = if shift == 0 {
                    dividend
                } else {
                    let sign = dividend >> 31;
                    let bias = ((sign as u32) >> (32 - shift)) as i32;
                    dividend.wrapping_add(bias) >> shift
                };
                let quotient = if negate {
                    positive_quotient.wrapping_neg()
                } else {
                    positive_quotient
                };
                let remainder =
                    dividend.wrapping_sub(positive_quotient.wrapping_shl(u32::from(shift)));

                let expected_quotient = if dividend == i32::MIN && divisor == -1 {
                    i32::MIN
                } else {
                    dividend / divisor
                };
                let expected_remainder = if divisor == -1 { 0 } else { dividend % divisor };
                assert_eq!(quotient, expected_quotient, "{dividend} / {divisor}");
                assert_eq!(remainder, expected_remainder, "{dividend} % {divisor}");
            }
        }
    }

    /// The NZCV fallback values written by `ccmp` must flip the named
    /// condition against the result of the actual comparison.  A wrong
    /// fallback silently changes the combined `band`/`bor` semantics (the
    /// `Le => 8` bug made `LE` evaluate true when the ccmp did not run).
    #[test]
    fn ccmp_nzcv_fallbacks_flip_each_condition() {
        for cond in [
            Cond::Eq,
            Cond::Ne,
            Cond::Hs,
            Cond::Lo,
            Cond::Mi,
            Cond::Pl,
            Cond::Vs,
            Cond::Vc,
            Cond::Hi,
            Cond::Ls,
            Cond::Ge,
            Cond::Lt,
            Cond::Gt,
            Cond::Le,
        ] {
            let false_flags = nzcv_making_cond_false(cond);
            let true_flags = nzcv_making_cond_true(cond);
            assert!(
                !evaluates(cond, false_flags),
                "nzcv_making_cond_false({cond:?}) = #{false_flags:x} must make it false"
            );
            assert!(
                evaluates(cond, true_flags),
                "nzcv_making_cond_true({cond:?}) = #{true_flags:x} must make it true"
            );
        }
    }

    /// Evaluate a condition against a bare NZCV value, using the same
    /// definition AArch64 uses (`LE` is `Z || (N != V)`, `GT` is
    /// `Z == 0 && N == V`, ...).
    fn evaluates(cond: Cond, nzcv: u8) -> bool {
        let n = nzcv >> 3 & 1 == 1;
        let z = nzcv >> 2 & 1 == 1;
        let v = nzcv & 1 == 1;
        match cond {
            Cond::Eq => z,
            Cond::Ne => !z,
            Cond::Hs => nzcv >> 1 & 1 == 1,
            Cond::Lo => nzcv >> 1 & 1 == 0,
            Cond::Mi => n,
            Cond::Pl => !n,
            Cond::Vs => v,
            Cond::Vc => !v,
            Cond::Hi => nzcv >> 1 & 1 == 1 && !z,
            Cond::Ls => nzcv >> 1 & 1 == 0 || z,
            Cond::Ge => n == v,
            Cond::Lt => n != v,
            Cond::Gt => !z && n == v,
            Cond::Le => z || n != v,
        }
    }

    #[test]
    fn single_use_dynamic_gep_folds_into_extended_addressing() {
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "gep_dyn".into(),
            vec![Type::get_pointer(Type::get_i32()), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let index = data.params()[1];
        let gep = data.new_local_inst().get_elem_ptr(base, vec![index]);
        let load = data.new_local_inst().load(gep);
        data.layout_mut().insert_inst(entry, load);
        let ret = data.new_local_inst().ret(Some(load));
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        // The dynamic index folds into the load addressing mode:
        // `ldr w?, [x?, x?, sxtw #2]` (stride 4 → scale 2).
        assert!(assembly.contains("sxtw #2]"), "{assembly}");
    }

    #[test]
    fn conjunction_tree_branches_without_materializing_booleans() {
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "and_branch".into(),
            vec![Type::get_i32(); 6],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let yes = data.new_basic_block().basic_block("yes".into(), vec![]);
        let no = data.new_basic_block().basic_block("no".into(), vec![]);
        data.layout_mut().push_bb_back(yes);
        data.layout_mut().push_bb_back(no);
        let p = data.params().to_vec();
        let first = data.new_local_inst().binary(BinaryOp::Lt, p[0], p[1]);
        let second = data.new_local_inst().binary(BinaryOp::Ge, p[2], p[3]);
        let pair = data.new_local_inst().binary(BinaryOp::And, first, second);
        let zero = data.new_local_inst().integer(0);
        let wrapped = data.new_local_inst().binary(BinaryOp::NotEq, pair, zero);
        let third = data.new_local_inst().binary(BinaryOp::NotEq, p[4], p[5]);
        let condition = data.new_local_inst().binary(BinaryOp::And, wrapped, third);
        let branch = data
            .new_local_inst()
            .branch(condition, yes, vec![], no, vec![]);
        for inst in [first, second, pair, wrapped, third, condition, branch] {
            data.layout_mut().insert_inst(entry, inst);
        }
        let one = data.new_local_inst().integer(1);
        let yes_ret = data.new_local_inst().ret(Some(one));
        let no_ret = data.new_local_inst().ret(Some(zero));
        data.layout_mut().insert_inst(yes, yes_ret);
        data.layout_mut().insert_inst(no, no_ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        assert_eq!(assembly.matches("ccmp ").count(), 2, "{assembly}");
        assert!(!assembly.contains("cset "), "{assembly}");
    }

    #[test]
    fn dynamic_gep_with_constant_offset_folds_into_extended_addressing() {
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "gep_dyn_const".into(),
            vec![
                Type::get_pointer(Type::get_array(Type::get_i32(), 1)),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let index = data.params()[1];
        let one = data.new_local_inst().integer(1);
        let gep = data.new_local_inst().get_elem_ptr(base, vec![index, one]);
        let forty_two = data.new_local_inst().integer(42);
        let store = data.new_local_inst().store(forty_two, gep);
        data.layout_mut().insert_inst(entry, store);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        // Dynamic term: stride 4 → scale 2 (matches the 4-byte store);
        // constant +4 folds into a fresh base temporary.
        assert!(assembly.contains("sxtw #2]"), "{assembly}");
    }

    #[test]
    fn multi_use_dynamic_gep_is_not_folded() {
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "gep_dyn_multi".into(),
            vec![Type::get_pointer(Type::get_i32()), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let index = data.params()[1];
        let gep = data.new_local_inst().get_elem_ptr(base, vec![index]);
        let load = data.new_local_inst().load(gep);
        let forty_two = data.new_local_inst().integer(42);
        let store = data.new_local_inst().store(forty_two, gep);
        data.layout_mut().insert_inst(entry, gep);
        data.layout_mut().insert_inst(entry, load);
        data.layout_mut().insert_inst(entry, store);
        let ret = data.new_local_inst().ret(Some(load));
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        // Two users: the GEP must be materialized, so no extended-register
        // *addressing* form (an ALU `add ..., sxtw #2` may still appear).
        assert!(!assembly.contains("sxtw #2]"), "{assembly}");
    }

    #[test]
    fn mul_mul_add_folds_into_madd() {
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "mul_mul_add".into(),
            vec![
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let [a, b, c, d] = [
            data.params()[0],
            data.params()[1],
            data.params()[2],
            data.params()[3],
        ];
        let mul1 = data.new_local_inst().binary(BinaryOp::Mul, a, b);
        let mul2 = data.new_local_inst().binary(BinaryOp::Mul, c, d);
        let add = data.new_local_inst().binary(BinaryOp::Add, mul1, mul2);
        data.layout_mut().insert_inst(entry, mul1);
        data.layout_mut().insert_inst(entry, mul2);
        data.layout_mut().insert_inst(entry, add);
        let ret = data.new_local_inst().ret(Some(add));
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        // mul(mul, mul) + add folds: one madd (lhs fused) + one standalone mul
        // (the addend), no separate add.
        assert_eq!(assembly.matches("madd").count(), 1, "{assembly}");
        assert_eq!(assembly.matches("\n    mul ").count(), 1, "{assembly}");
        assert!(!assembly.contains("\n    add w"), "{assembly}");
    }

    #[test]
    fn multi_use_mul_blocks_madd_fold() {
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "mul_mul_add_multi".into(),
            vec![
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
                Type::get_pointer(Type::get_i32()),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let [a, b, c, d] = [
            data.params()[0],
            data.params()[1],
            data.params()[2],
            data.params()[3],
        ];
        let ptr = data.params()[4];
        let mul1 = data.new_local_inst().binary(BinaryOp::Mul, a, b);
        let mul2 = data.new_local_inst().binary(BinaryOp::Mul, c, d);
        let add = data.new_local_inst().binary(BinaryOp::Add, mul1, mul2);
        // mul1 has a second user (the store), so it must remain materialized.
        // The single-use mul2 may still fold with mul1 as the madd addend.
        let store = data.new_local_inst().store(mul1, ptr);
        data.layout_mut().insert_inst(entry, mul1);
        data.layout_mut().insert_inst(entry, mul2);
        data.layout_mut().insert_inst(entry, add);
        data.layout_mut().insert_inst(entry, store);
        let ret = data.new_local_inst().ret(Some(add));
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        assert_eq!(assembly.matches("madd").count(), 1, "{assembly}");
        assert_eq!(assembly.matches("\n    mul ").count(), 1, "{assembly}");
        assert!(!assembly.contains("\n    add w"), "{assembly}");
        assert!(assembly.contains("str w"), "{assembly}");
    }

    #[test]
    fn classifies_mul_constants() {
        let s32 = OperandSize::Size32;
        let shift = |v: u8| ImmShift::new(v, s32).unwrap();
        assert!(matches!(fold_mul_constant(2, s32), Some(MulConstForm::Lsl(s)) if s == shift(1)));
        assert!(matches!(fold_mul_constant(8, s32), Some(MulConstForm::Lsl(s)) if s == shift(3)));
        assert!(
            matches!(fold_mul_constant(3, s32), Some(MulConstForm::AddLsl(s)) if s == shift(1))
        );
        assert!(
            matches!(fold_mul_constant(5, s32), Some(MulConstForm::AddLsl(s)) if s == shift(2))
        );
        assert!(
            matches!(fold_mul_constant(9, s32), Some(MulConstForm::AddLsl(s)) if s == shift(3))
        );
        assert!(
            matches!(fold_mul_constant(-1, s32), Some(MulConstForm::SubLsl(s)) if s == shift(1))
        );
        assert!(
            matches!(fold_mul_constant(-3, s32), Some(MulConstForm::SubLsl(s)) if s == shift(2))
        );
        assert!(
            matches!(fold_mul_constant(-7, s32), Some(MulConstForm::SubLsl(s)) if s == shift(3))
        );
        assert!(
            matches!(fold_mul_constant(-2, s32), Some(MulConstForm::NegLsl(s)) if s == shift(1))
        );
        assert!(
            matches!(fold_mul_constant(-8, s32), Some(MulConstForm::NegLsl(s)) if s == shift(3))
        );
        // Not encodable in one instruction.
        assert!(fold_mul_constant(6, s32).is_none());
        assert!(fold_mul_constant(7, s32).is_none());
        assert!(fold_mul_constant(0, s32).is_none());
        assert!(fold_mul_constant(1, s32).is_none());
    }

    #[test]
    fn constant_mul_folds_to_shift_or_add_shift() {
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "mul_const".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let x = data.params()[0];
        let three = data.new_local_inst().integer(3);
        let mul = data.new_local_inst().binary(BinaryOp::Mul, x, three);
        data.layout_mut().insert_inst(entry, mul);
        let ret = data.new_local_inst().ret(Some(mul));
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        // x * 3 = x + (x << 1): one add-shift, no mul, no movz 3.
        assert!(assembly.contains("lsl #1"), "{assembly}");
        assert!(!assembly.contains("\n    mul "), "{assembly}");
        assert!(!assembly.contains("movz"), "{assembly}");
    }

    #[test]
    fn unencodable_constant_mul_falls_back() {
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "mul_const_fallback".into(),
            vec![Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let x = data.params()[0];
        let six = data.new_local_inst().integer(6);
        let mul = data.new_local_inst().binary(BinaryOp::Mul, x, six);
        data.layout_mut().insert_inst(entry, mul);
        let ret = data.new_local_inst().ret(Some(mul));
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        // 6 is not a single-instruction multiplier: keep mul.
        assert_eq!(assembly.matches("\n    mul ").count(), 1, "{assembly}");
    }

    #[test]
    fn explicit_vector_vcode_emits_neon_assembly() {
        use crate::abi::AArch64Abi;
        use crate::instructions::{VecArithOp, VecShape};
        use raana_ir::ir::builder_trait::*;
        use taki_mir::abi::CalleeABI;
        use taki_mir::block_order::BlockLoweringOrder;
        use taki_mir::prelude::ArenaContext;
        use taki_mir::register::Writable;
        use taki_mir::types::V4I32;
        use taki_mir::vcode::VCodeBuilder;

        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "vec_test".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);
        let func_data = program.func_data(function);

        let arena = ArenaContext {
            program: &program,
            curr_func: Some(function),
        };
        let abi = CalleeABI::<AArch64Abi>::new(arena);
        let order = BlockLoweringOrder::new(arena);
        let mut builder = VCodeBuilder::new(abi, order);
        let mut vregs = VRegAllocator::<MInst>::with_capaticy(6);
        let z0 = vregs.alloc(I32);
        let z1 = vregs.alloc(I32);
        let v0 = vregs.alloc(V4I32);
        let v1 = vregs.alloc(V4I32);
        let sum = vregs.alloc(V4I32);
        let acc = vregs.alloc(F32);
        // Pushed in reverse of final order: dup(0)+dup(0) -> add -> addv -> ret.
        builder.push(MInst::Ret);
        builder.push(MInst::VecAddv {
            dst: Writable::from_reg(acc),
            src: sum,
        });
        builder.push(MInst::VecArithRRR {
            op: VecArithOp::Add,
            shape: VecShape::FourS,
            dst: Writable::from_reg(sum),
            lhs: v0,
            rhs: v1,
        });
        builder.push(MInst::VecDup {
            shape: VecShape::FourS,
            dst: Writable::from_reg(v1),
            src: z1,
        });
        builder.push(MInst::VecDup {
            shape: VecShape::FourS,
            dst: Writable::from_reg(v0),
            src: z0,
        });
        builder.push(MInst::MovFromZero {
            size: OperandSize::Size32,
            dst: Writable::from_reg(z1),
        });
        builder.push(MInst::MovFromZero {
            size: OperandSize::Size32,
            dst: Writable::from_reg(z0),
        });
        builder.end_bb();
        let mut vcode = builder.build(vregs);

        let output = taki_mir::reg_alloc::ion::run(&vcode, vcode.abi.machine_env())
            .expect("vector VCode allocation should succeed");
        assert!(vcode.verify_alloc_output(&output).is_ok());
        vcode.write_back_allocs(&output);
        let spill_size =
            u32::try_from(output.num_spillslots).unwrap() * vcode.abi.spill_unit_bytes();
        vcode
            .abi
            .compute_frame_layout(spill_size, &output)
            .expect("frame layout should accept vector spill units");
        vcode.finalize_for_emission(&output);

        let assembly = taki_mir::emit::emit_vcode_assembly::<crate::lower::AArch64Backend>(
            &program, func_data, &vcode,
        );
        // Register allocation assigns arbitrary vector numbers, so assert the
        // NEON forms rather than specific physical registers.
        assert!(assembly.contains("dup v"), "{assembly}");
        assert!(assembly.contains(".4s, w"), "{assembly}");
        assert!(assembly.contains("add v"), "{assembly}");
        assert!(assembly.contains(".4s, v"), "{assembly}");
        assert!(assembly.contains("addv s"), "{assembly}");
    }

    /// Construct a `<4 x i32>` kernel over vector parameters (v0-v7 ABI) and a
    /// scalar splat source, covering the integer NEON lowering surface:
    /// splat → add → mul → cmeq → bsl → smin/smax → addv → lane extract/insert.
    fn compile_vector_int_kernel() -> String {
        use raana_ir::ir::builder_trait::*;

        let v4i32 = Type::get_vector(Type::get_i32(), 4);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "vec_int_kernel".into(),
            vec![v4i32.clone(), v4i32.clone(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let v = data.params()[0];
        let w = data.params()[1];
        let s = data.params()[2];

        let splat = data.new_local_inst().vector_splat(s, v4i32.clone());
        let add = data.new_local_inst().binary(BinaryOp::Add, v, splat);
        let mul = data.new_local_inst().binary(BinaryOp::Mul, add, w);
        let mask = data.new_local_inst().binary(BinaryOp::Eq, v, w);
        let sel = data.new_local_inst().select(mask, add, mul);
        let mn = data.new_local_inst().binary(BinaryOp::Min, sel, w);
        let mx = data.new_local_inst().binary(BinaryOp::Max, mn, splat);
        let sum = data
            .new_local_inst()
            .vector_reduce(raana_ir::ir::VectorReduceOp::Add, mx);
        let zero = data.new_local_inst().integer(0);
        let e0 = data.new_local_inst().vector_extract_element(mx, zero);
        let one = data.new_local_inst().integer(1);
        let ins = data.new_local_inst().vector_insert_element(mx, e0, one);

        for inst in [splat, add, mul, mask, sel, mn, mx, sum, e0, ins] {
            data.layout_mut().insert_inst(entry, inst);
        }
        let ret = data.new_local_inst().ret(Some(sum));
        data.layout_mut().insert_inst(entry, ret);
        taki_mir::compile::<crate::lower::AArch64Backend>(&program)
    }

    #[test]
    fn vector_ir_function_emits_neon_integer_surface() {
        let assembly = compile_vector_int_kernel();
        assert!(assembly.contains("dup v"), "{assembly}");
        assert!(assembly.contains("add v"), "{assembly}");
        assert!(assembly.contains("mul v"), "{assembly}");
        assert!(assembly.contains("cmeq"), "{assembly}");
        assert!(assembly.contains("bsl"), "{assembly}");
        assert!(assembly.contains("smin"), "{assembly}");
        assert!(assembly.contains("smax"), "{assembly}");
        assert!(assembly.contains("addv s"), "{assembly}");
        // Lane ops: extract to a GPR and insert back from it.
        assert!(assembly.contains("mov w"), "{assembly}");
        assert!(assembly.contains("v0"), "{assembly}");
        // addv writes a SIMD register; the i32 result must be moved out.
        assert!(assembly.contains("fmov w"), "{assembly}");
    }

    #[test]
    fn vector_ir_function_emits_neon_float_surface() {
        use raana_ir::ir::builder_trait::*;

        let v4i32 = Type::get_vector(Type::get_i32(), 4);
        let v4f32 = Type::get_vector(Type::get_f32(), 4);
        let mut program = Program::new();
        let function = program.new_function(
            v4f32.clone(),
            "vec_float_kernel".into(),
            vec![v4i32.clone(), Type::get_f32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let vi = data.params()[0];
        let s = data.params()[1];

        // f32 splat (from a float register), i32→f32 conversion, fmla.
        let splat_f = data.new_local_inst().vector_splat(s, v4f32.clone());
        let vf = data.new_local_inst().cast(vi, v4f32.clone());
        let acc = data.new_local_inst().fma(vf, splat_f, splat_f);

        for inst in [splat_f, vf, acc] {
            data.layout_mut().insert_inst(entry, inst);
        }
        let ret = data.new_local_inst().ret(Some(acc));
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        // dup from a float scalar is `dup v.4s, s0`; scvtf converts the lanes;
        // fmla fuses the multiply-add.
        assert!(assembly.contains("dup v"), "{assembly}");
        assert!(assembly.contains(".4s, s"), "{assembly}");
        assert!(assembly.contains("scvtf"), "{assembly}");
        assert!(assembly.contains("fmla"), "{assembly}");
    }

    #[test]
    fn vector_ir_function_emits_neon_load_store_and_reduce() {
        use raana_ir::ir::builder_trait::*;

        let v4i32 = Type::get_vector(Type::get_i32(), 4);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "vec_mem_kernel".into(),
            vec![v4i32.clone(), Type::get_pointer(v4i32.clone())],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let vec = data.params()[0];
        let ptr = data.params()[1];

        let store = data.new_local_inst().store(vec, ptr);
        let loaded = data.new_local_inst().load(ptr);
        let sum = data
            .new_local_inst()
            .vector_reduce(raana_ir::ir::VectorReduceOp::Add, loaded);

        for inst in [store, loaded, sum] {
            data.layout_mut().insert_inst(entry, inst);
        }
        let ret = data.new_local_inst().ret(Some(sum));
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        assert!(assembly.contains("str q"), "{assembly}");
        assert!(assembly.contains("ldr q"), "{assembly}");
        assert!(assembly.contains("addv s"), "{assembly}");
    }

    /// Determinism gate: compiling the vector kernel at each `-O` level must be
    /// byte-identical across recompilations.
    #[test]
    fn vector_kernel_is_deterministic_across_opt_levels() {
        use crate::config::AArch64CodegenConfig;
        use raana_ir::ir::builder_trait::*;

        let v4i32 = Type::get_vector(Type::get_i32(), 4);
        let build = |program: &Program| {
            let mut program = program.clone();
            let mut programs = Vec::new();
            for config in [
                AArch64CodegenConfig {
                    dce: false,
                    peephole_combine: false,
                    pair_combine: false,
                    list_scheduler: false,
                    sched_model: Default::default(),
                    branch_opt: false,
                    chain_fusion: false,
                    const_cse: false,
                },
                AArch64CodegenConfig {
                    dce: true,
                    peephole_combine: true,
                    pair_combine: true,
                    list_scheduler: false,
                    sched_model: Default::default(),
                    branch_opt: true,
                    chain_fusion: true,
                    const_cse: true,
                },
                AArch64CodegenConfig {
                    dce: true,
                    peephole_combine: true,
                    pair_combine: true,
                    list_scheduler: true,
                    sched_model: Default::default(),
                    branch_opt: true,
                    chain_fusion: true,
                    const_cse: true,
                },
            ] {
                let first = taki_mir::compile_with_config::<crate::lower::AArch64Backend>(
                    &program, &config,
                )
                .assembly;
                for _ in 0..4 {
                    let again = taki_mir::compile_with_config::<crate::lower::AArch64Backend>(
                        &program, &config,
                    )
                    .assembly;
                    assert_eq!(first, again, "recompilation diverged");
                }
                programs.push(first);
            }
            programs
        };

        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "vec_det".into(),
            vec![v4i32.clone(), v4i32.clone(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let v = data.params()[0];
        let w = data.params()[1];
        let s = data.params()[2];
        let splat = data.new_local_inst().vector_splat(s, v4i32.clone());
        let add = data.new_local_inst().binary(BinaryOp::Add, v, splat);
        let mul = data.new_local_inst().binary(BinaryOp::Mul, add, w);
        let mask = data.new_local_inst().binary(BinaryOp::Eq, v, w);
        let sel = data.new_local_inst().select(mask, add, mul);
        let mn = data.new_local_inst().binary(BinaryOp::Min, sel, w);
        let mx = data.new_local_inst().binary(BinaryOp::Max, mn, splat);
        let sum = data
            .new_local_inst()
            .vector_reduce(raana_ir::ir::VectorReduceOp::Add, mx);
        for inst in [splat, add, mul, mask, sel, mn, mx, sum] {
            data.layout_mut().insert_inst(entry, inst);
        }
        let ret = data.new_local_inst().ret(Some(sum));
        data.layout_mut().insert_inst(entry, ret);

        let programs = build(&program);
        // Every optimization level must produce the NEON lowering.
        for assembly in &programs {
            assert!(assembly.contains("add v"), "{assembly}");
            assert!(assembly.contains("bsl"), "{assembly}");
            assert!(assembly.contains("addv s"), "{assembly}");
        }
    }

    /// Builds `r = ((a + C) + (b + C))` with two same-value constant uses in
    /// one block, and returns the assembly.
    fn compile_two_same_constant_adds(c: i32) -> String {
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "two_const".to_owned(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let a = data.params()[0];
        let b = data.params()[1];
        let c1 = data.new_local_inst().integer(c);
        let add1 = data.new_local_inst().binary(BinaryOp::Add, a, c1);
        let c2 = data.new_local_inst().integer(c);
        let add2 = data.new_local_inst().binary(BinaryOp::Add, b, c2);
        let add3 = data.new_local_inst().binary(BinaryOp::Add, add1, add2);
        let ret = data.new_local_inst().ret(Some(add3));
        for inst in [add1, add2, add3, ret] {
            data.layout_mut().insert_inst(entry, inst);
        }
        taki_mir::compile::<crate::lower::AArch64Backend>(&program)
    }

    #[test]
    fn shares_one_materialization_for_same_value_constants_in_a_block() {
        let assembly = compile_two_same_constant_adds(0xc811);
        assert_eq!(
            assembly.matches("0xc811").count(),
            1,
            "two uses of 0xc811 in one block must materialize once:\n{assembly}"
        );
    }

    #[test]
    fn does_not_materialize_an_embeddable_constant() {
        let assembly = compile_two_same_constant_adds(7);
        assert!(
            !assembly.contains("movz"),
            "an add-immediate constant must stay embedded:\n{assembly}"
        );
    }

    #[test]
    fn keeps_per_block_materialization_for_cross_block_uses() {
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "cross_block".to_owned(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let tail = data
            .new_basic_block()
            .basic_block("tail".to_owned(), vec![]);
        data.layout_mut().push_bb_back(tail);
        let a = data.params()[0];
        let b = data.params()[1];
        let c1 = data.new_local_inst().integer(0xc811);
        let add1 = data.new_local_inst().binary(BinaryOp::Add, a, c1);
        data.layout_mut().insert_inst(entry, add1);
        let one = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(one, tail, vec![], tail, vec![]);
        data.layout_mut().insert_inst(entry, branch);
        let c2 = data.new_local_inst().integer(0xc811);
        let add2 = data.new_local_inst().binary(BinaryOp::Add, b, c2);
        let add3 = data.new_local_inst().binary(BinaryOp::Add, add1, add2);
        let ret = data.new_local_inst().ret(Some(add3));
        for inst in [add2, add3, ret] {
            data.layout_mut().insert_inst(tail, inst);
        }
        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        assert_eq!(
            assembly.matches("0xc811").count(),
            2,
            "each block materializes its own 0xc811:\n{assembly}"
        );
    }
