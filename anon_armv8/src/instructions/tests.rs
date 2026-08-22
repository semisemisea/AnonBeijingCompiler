use core::fmt::Write;

use taki_mir::{
    abi::ArgPair,
    block_order::MirBlockIndex,
    prelude::{HirFunction, HirInst},
    reg_alloc::reg::{OperandConstraint, OperandKind, PReg, RegClass, VReg},
    register::{Reg, Writable},
    vcode::{EmitContext, MachInst, MachInstEmit, MachTerminator},
};

use super::{
    CCmpStep, Cond, FpuOp, Imm12, ImmLogic, MInst, SelectCmp, SelectValue, call_clobbers,
};
use crate::regs::{OperandSize, float_reg, int_reg, vector_reg};

#[derive(Default)]
struct TestEmitContext(String);

impl core::fmt::Write for TestEmitContext {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        self.0.push_str(text);
        Ok(())
    }
}

impl EmitContext for TestEmitContext {
    fn write_reg(&mut self, _reg: &taki_mir::register::Reg) -> core::fmt::Result {
        unreachable!("tests use physical registers")
    }

    fn write_label_ref(&mut self, _idx: MirBlockIndex) -> core::fmt::Result {
        unreachable!("select pseudo has no labels")
    }

    fn write_function_label(&mut self, _func: HirFunction) -> core::fmt::Result {
        unreachable!("select pseudo has no labels")
    }

    fn write_external_symbol(&mut self, symbol: &str) -> core::fmt::Result {
        write!(self, "{symbol}")
    }

    fn write_global_label(&mut self, _global: HirInst) -> core::fmt::Result {
        unreachable!("select pseudo has no labels")
    }

    /// Mirror the emission contract of `EmitBuffer::end_inst`: one flush
    /// = one instruction line.
    fn end_inst(&mut self) -> core::fmt::Result {
        self.0.push_str("\n    ");
        Ok(())
    }
}

fn emit(inst: MInst) -> String {
    let mut ctx = TestEmitContext::default();
    inst.emit(&mut ctx).unwrap();
    ctx.0
}

#[test]
fn emits_adjacent_i32_cmp_and_csel() {
    let text = emit(MInst::CmpSelect {
        cmp: SelectCmp::IntImm {
            size: OperandSize::Size32,
            lhs: int_reg(1),
            imm: Imm12::new(0, false).unwrap(),
        },
        ccmp: None,
        cond: Cond::Ne,
        value: SelectValue::Int {
            size: OperandSize::Size32,
            dst: Writable::from_reg(int_reg(0)),
            if_true: int_reg(2),
            if_false: int_reg(3),
        },
    });
    assert_eq!(text, "cmp w1, #0\n    csel w0, w2, w3, ne");
}

#[test]
fn call_clobbers_exclude_the_fixed_return_register() {
    let int = call_clobbers(
        crate::regs::DEFAULT_CLOBBERS,
        Some(&taki_mir::abi::CallRetPair {
            vreg: Writable::from_reg(int_reg(1)),
            preg: int_reg(0),
        }),
    );
    let float = call_clobbers(
        crate::regs::DEFAULT_CLOBBERS,
        Some(&taki_mir::abi::CallRetPair {
            vreg: Writable::from_reg(float_reg(1)),
            preg: float_reg(0),
        }),
    );

    assert!(!int.contains(crate::regs::int_preg(0)));
    assert!(!float.contains(crate::regs::float_preg(0)));
    assert!(int.contains(crate::regs::int_preg(1)));
    assert!(float.contains(crate::regs::float_preg(1)));
}

#[test]
fn emits_64_bit_csel_for_pointer_values() {
    let text = emit(MInst::CmpSelect {
        cmp: SelectCmp::IntImm {
            // RaanaIR select conditions are always i32, even when the
            // selected values are pointers or strings.
            size: OperandSize::Size32,
            lhs: int_reg(1),
            imm: Imm12::new(0, false).unwrap(),
        },
        ccmp: None,
        cond: Cond::Ne,
        value: SelectValue::Int {
            size: OperandSize::Size64,
            dst: Writable::from_reg(int_reg(0)),
            if_true: int_reg(2),
            if_false: int_reg(3),
        },
    });
    assert_eq!(text, "cmp w1, #0\n    csel x0, x2, x3, ne");
}

#[test]
fn emits_adjacent_float_cmp_and_fcsel() {
    let text = emit(MInst::CmpSelect {
        cmp: SelectCmp::Float {
            lhs: float_reg(4),
            rhs: float_reg(5),
        },
        cond: Cond::Mi,
        value: SelectValue::Float {
            dst: Writable::from_reg(float_reg(0)),
            if_true: float_reg(1),
            if_false: float_reg(2),
        },
        ccmp: None,
    });
    assert_eq!(text, "fcmp s4, s5\n    fcsel s0, s1, s2, mi");
}

#[test]
fn emits_adjacent_cmp_and_cset() {
    let text = emit(MInst::CmpSelect {
        cmp: SelectCmp::IntRR {
            size: OperandSize::Size32,
            lhs: int_reg(1),
            rhs: crate::regs::RegOrZr::Reg(int_reg(2)),
        },
        ccmp: None,
        cond: Cond::Eq,
        value: SelectValue::Bool {
            dst: Writable::from_reg(int_reg(0)),
        },
    });
    assert_eq!(text, "cmp w1, w2\n    cset w0, eq");
}

#[test]
fn emits_and_ccmp_chain_as_cmp_ccmp_csel() {
    let text = emit(MInst::CmpSelect {
        cmp: SelectCmp::IntImm {
            size: OperandSize::Size32,
            lhs: int_reg(1),
            imm: Imm12::new(1, false).unwrap(),
        },
        ccmp: Some(Box::new(CCmpStep {
            size: OperandSize::Size32,
            lhs: int_reg(2),
            rhs: crate::regs::RegOrZr::Zr,
            imm: Some(Imm12::new(1, false).unwrap()),
            nzcv: 0,
            cond: Cond::Eq,
        })),
        cond: Cond::Eq,
        value: SelectValue::Int {
            size: OperandSize::Size32,
            dst: Writable::from_reg(int_reg(0)),
            if_true: int_reg(3),
            if_false: int_reg(4),
        },
    });
    assert_eq!(
        text,
        "cmp w1, #1\n    ccmp w2, #1, #0, eq\n    csel w0, w3, w4, eq"
    );
}

#[test]
fn emits_or_ccmp_chain_with_true_fallback_nzcv() {
    let text = emit(MInst::CmpSelect {
        cmp: SelectCmp::IntRR {
            size: OperandSize::Size32,
            lhs: int_reg(1),
            rhs: crate::regs::RegOrZr::Reg(int_reg(2)),
        },
        ccmp: Some(Box::new(CCmpStep {
            size: OperandSize::Size32,
            lhs: int_reg(3),
            rhs: crate::regs::RegOrZr::Zr,
            imm: Some(Imm12::new(1, false).unwrap()),
            nzcv: 4,
            cond: Cond::Ne,
        })),
        cond: Cond::Eq,
        value: SelectValue::Bool {
            dst: Writable::from_reg(int_reg(0)),
        },
    });
    assert_eq!(text, "cmp w1, w2\n    ccmp w3, #1, #4, ne\n    cset w0, eq");
}

#[test]
fn emits_fused_flag_forms() {
    let subs = emit(MInst::SubsRRImm12 {
        size: OperandSize::Size32,
        dst: Writable::from_reg(int_reg(1)),
        src: int_reg(1),
        imm: Imm12::new(1, false).unwrap(),
    });
    assert_eq!(subs, "subs w1, w1, #1");

    let ands = emit(MInst::AndsRRImmLogic {
        size: OperandSize::Size32,
        dst: Writable::from_reg(int_reg(1)),
        src: crate::regs::RegOrZr::Reg(int_reg(2)),
        imm: ImmLogic::new(0x8000_0001, OperandSize::Size32).unwrap(),
    });
    assert_eq!(ands, "ands w1, w2, #0x80000001");

    let tst = emit(MInst::TstRRImmLogic {
        size: OperandSize::Size32,
        src: crate::regs::RegOrZr::Reg(int_reg(2)),
        imm: ImmLogic::new(0x8000_0001, OperandSize::Size32).unwrap(),
    });
    assert_eq!(tst, "tst w2, #0x80000001");
}

#[test]
fn stack_pointer_prints_as_sp() {
    let text = emit(MInst::MovPhys {
        size: OperandSize::Size64,
        dst: Writable::from_reg(int_reg(0)),
        src: crate::regs::stack_reg(),
    });
    assert_eq!(text, "mov x0, sp");
}

#[test]
fn alu_rr_imm12_accepts_sp_as_source_and_destination() {
    let text = emit(MInst::AluRRImm12 {
        op: super::AluOp::Sub,
        size: OperandSize::Size64,
        dst: crate::regs::writable_stack_reg(),
        src: crate::regs::stack_reg(),
        imm: Imm12::new(32, false).unwrap(),
    });
    assert_eq!(text, "sub sp, sp, #32");
}

#[test]
fn imm12_maybe_from_u64_handles_unshifted_form() {
    let imm = Imm12::maybe_from_u64(0xfff).unwrap();
    assert_eq!(imm.value(), 0xfff);
    assert!(!imm.shift12());
    let imm = Imm12::maybe_from_u64(0).unwrap();
    assert_eq!(imm.value(), 0);
    assert!(!imm.shift12());
}

#[test]
fn imm12_maybe_from_u64_handles_shift12_form() {
    let imm = Imm12::maybe_from_u64(4096).unwrap();
    assert_eq!(imm.value(), 1);
    assert!(imm.shift12());
    let imm = Imm12::maybe_from_u64(0xfff000).unwrap();
    assert_eq!(imm.value(), 0xfff);
    assert!(imm.shift12());
}

#[test]
fn imm12_maybe_from_u64_rejects_unrepresentable_values() {
    assert!(Imm12::maybe_from_u64(0xfff001).is_none());
    assert!(Imm12::maybe_from_u64(0x1000_0000).is_none());
    assert!(Imm12::maybe_from_u64(u64::MAX).is_none());
}

fn virtual_reg(index: usize, class: RegClass) -> Reg {
    Reg::from_virtual_reg(VReg::new(192 + index, class))
}

fn int_args() -> MInst {
    MInst::Args {
        args: vec![
            ArgPair {
                vreg: Writable::from_reg(virtual_reg(0, RegClass::Int)),
                preg: int_reg(0),
            },
            ArgPair {
                vreg: Writable::from_reg(virtual_reg(1, RegClass::Int)),
                preg: int_reg(1),
            },
        ],
    }
}

struct TestOperandVisitor(Vec<(VReg, OperandConstraint, OperandKind)>);

impl taki_mir::reg_alloc::reg::OperandVisitor for TestOperandVisitor {
    fn add_operand(
        &mut self,
        reg: &mut Reg,
        constraint: OperandConstraint,
        kind: OperandKind,
        _pos: taki_mir::reg_alloc::reg::OperandPos,
    ) {
        self.0
            .push((reg.to_virtual_reg().unwrap(), constraint, kind));
    }
}

#[test]
fn args_pseudo_emits_no_machine_code() {
    assert_eq!(emit(int_args()), "");
}

#[test]
fn args_pseudo_is_not_a_terminator() {
    assert_eq!(int_args().is_term(), MachTerminator::None);
}

#[test]
fn args_pseudo_binds_each_parameter_with_a_fixed_def() {
    let mut args = int_args();
    let mut visitor = TestOperandVisitor(Vec::new());
    args.get_operands(&mut visitor);
    assert_eq!(visitor.0.len(), 2);
    for (index, (vreg, constraint, kind)) in visitor.0.iter().enumerate() {
        assert_eq!(*kind, OperandKind::Def);
        let OperandConstraint::FixedReg(preg) = constraint else {
            panic!("Args operands must use FixedReg constraints");
        };
        assert_eq!(*preg, int_reg(index as u8).to_physical_reg().unwrap());
        assert_eq!(vreg.class(), RegClass::Int);
    }
}

#[test]
fn args_verify_accepts_matching_register_classes() {
    assert!(int_args().verify().is_ok());
}

#[test]
fn args_verify_rejects_mismatched_register_classes() {
    let args = MInst::Args {
        args: vec![ArgPair {
            vreg: Writable::from_reg(virtual_reg(0, RegClass::Float)),
            preg: int_reg(0),
        }],
    };
    assert!(args.verify().is_err());
}

fn vec_reg(index: u8) -> Reg {
    Reg::from_physical_reg(PReg::new(index as usize, RegClass::Vector))
}

#[test]
fn emits_vector_move_as_mov_v_b() {
    let text = emit(MInst::VecMov {
        dst: Writable::from_reg(vec_reg(1)),
        src: vec_reg(2),
    });
    assert_eq!(text, "mov v1.16b, v2.16b");
}

#[test]
fn emits_128_bit_vector_load_and_store() {
    let load = emit(MInst::Load {
        ty: super::MemoryType::Vec128,
        dst: Writable::from_reg(vec_reg(3)),
        addr: super::AMode::UnsignedOffset {
            base: int_reg(0),
            offset: super::UImm12Scaled::new(32, 16).unwrap(),
        },
    });
    assert_eq!(load, "ldr q3, [x0, #32]");

    let store = emit(MInst::Store {
        ty: super::MemoryType::Vec128,
        src: vec_reg(4),
        addr: super::AMode::UnsignedOffset {
            base: int_reg(0),
            offset: super::UImm12Scaled::new(16, 16).unwrap(),
        },
    });
    assert_eq!(store, "str q4, [x0, #16]");
}

#[test]
fn emits_ld1_st1_vector_memory_forms() {
    let load = emit(MInst::VecLd1 {
        dst: Writable::from_reg(vec_reg(0)),
        base: int_reg(1),
    });
    assert_eq!(load, "ld1 {v0.16b}, [x1]");

    let store = emit(MInst::VecSt1 {
        src: vec_reg(5),
        base: int_reg(2),
    });
    assert_eq!(store, "st1 {v5.16b}, [x2]");
}

#[test]
fn emits_dup_from_gpr_and_float_scalars() {
    let dup_i32 = emit(MInst::VecDup {
        shape: super::VecShape::FourS,
        dst: Writable::from_reg(vec_reg(0)),
        src: int_reg(3),
    });
    assert_eq!(dup_i32, "dup v0.4s, w3");

    let dup_i64 = emit(MInst::VecDup {
        shape: super::VecShape::TwoD,
        dst: Writable::from_reg(vec_reg(0)),
        src: int_reg(4),
    });
    assert_eq!(dup_i64, "dup v0.2d, x4");

    let dup_f32 = emit(MInst::VecDup {
        shape: super::VecShape::FourS,
        dst: Writable::from_reg(vec_reg(0)),
        src: float_reg(5),
    });
    // LLVM MC rejects `dup vd.4s, sn`; the float scalar is read through
    // the aliased vector register (`s5` is the low 32 bits of `v5`).
    assert_eq!(dup_f32, "dup v0.4s, v5.s[0]");
}

#[test]
fn rc_for_type_maps_f32_to_the_vector_class() {
    use taki_mir::types::F32;
    // f32 scalars must allocate from the same bank as NEON vectors (`sN`
    // is the low 32 bits of `vN`): a separate Float class would let the
    // allocator hand an f32 and a live vector the same hw_enc, silently
    // clobbering one (the h-10 `fmov s0` / `fdiv v0` corruption).
    let (classes, types) = MInst::rc_for_type(F32);
    assert_eq!(classes, &[RegClass::Vector]);
    assert_eq!(types, &[F32]);
}

#[test]
fn f32_scalars_in_vector_class_render_as_s_registers() {
    // The h-10 corruption sequence: an f32 constant materialized with
    // `fmov s0, w21` (dst is a Vector-class vreg) must render as `sN`,
    // and every scalar-f32 view of a Vector-class register does.
    let fmov = emit(MInst::FMov {
        dst: Writable::from_reg(vec_reg(0)),
        src: int_reg(21),
    });
    assert_eq!(fmov, "fmov s0, w21");

    let fadd = emit(MInst::FAlu {
        op: FpuOp::Add,
        dst: Writable::from_reg(vec_reg(0)),
        lhs: vec_reg(1),
        rhs: vec_reg(2),
    });
    assert_eq!(fadd, "fadd s0, s1, s2");

    let addv = emit(MInst::VecAddv {
        dst: Writable::from_reg(vec_reg(0)),
        src: vec_reg(1),
    });
    assert_eq!(addv, "addv s0, v1.4s");

    let load = emit(MInst::Load {
        ty: super::MemoryType::F32,
        dst: Writable::from_reg(vec_reg(3)),
        addr: super::AMode::UnsignedOffset {
            base: int_reg(0),
            offset: super::UImm12Scaled::new(0, 4).unwrap(),
        },
    });
    assert_eq!(load, "ldr s3, [x0, #0]");

    let store = emit(MInst::Store {
        ty: super::MemoryType::F32,
        src: vec_reg(3),
        addr: super::AMode::UnsignedOffset {
            base: int_reg(0),
            offset: super::UImm12Scaled::new(0, 4).unwrap(),
        },
    });
    assert_eq!(store, "str s3, [x0, #0]");

    // Lane extract/insert of an f32 element: the scalar side is `sN`.
    let extract = emit(MInst::VecExtractLane {
        size: OperandSize::Size32,
        dst: Writable::from_reg(vec_reg(0)),
        src: vec_reg(1),
        lane: 0,
    });
    assert_eq!(extract, "mov s0, v1.s[0]");

    let insert = emit(MInst::VecInsertLane {
        size: OperandSize::Size32,
        dst: Writable::from_reg(vec_reg(0)),
        vector: vec_reg(1),
        src: vec_reg(2),
        lane: 1,
    });
    assert_eq!(insert, "mov v0.16b, v1.16b\n    mov v0.s[1], s2");
}

#[test]
fn dup_from_vector_class_f32_scalar_uses_element_form() {
    // Post-merge, the splat source of an f32 is a Vector-class register;
    // `dup vd.4s, sn` is rejected by LLVM MC, so the element form is used.
    let dup_f32 = emit(MInst::VecDup {
        shape: super::VecShape::FourS,
        dst: Writable::from_reg(vec_reg(0)),
        src: vec_reg(5),
    });
    assert_eq!(dup_f32, "dup v0.4s, v5.s[0]");
}

#[test]
fn emits_vector_arith_forms() {
    for (op, mnemonic) in [
        (super::VecArithOp::Add, "add"),
        (super::VecArithOp::Sub, "sub"),
        (super::VecArithOp::Mul, "mul"),
        // Float vectors must use the `f*` NEON forms: the plain integer
        // ops would add the float bit patterns.
        (super::VecArithOp::Fadd, "fadd"),
        (super::VecArithOp::Fsub, "fsub"),
        (super::VecArithOp::Fmul, "fmul"),
    ] {
        let text = emit(MInst::VecArithRRR {
            op,
            shape: super::VecShape::FourS,
            dst: Writable::from_reg(vec_reg(0)),
            lhs: vec_reg(1),
            rhs: vec_reg(2),
        });
        assert_eq!(text, format!("{mnemonic} v0.4s, v1.4s, v2.4s"));
    }
    let two_d = emit(MInst::VecArithRRR {
        op: super::VecArithOp::Add,
        shape: super::VecShape::TwoD,
        dst: Writable::from_reg(vec_reg(0)),
        lhs: vec_reg(1),
        rhs: vec_reg(2),
    });
    assert_eq!(two_d, "add v0.2d, v1.2d, v2.2d");
}

#[test]
fn emits_fmla_and_bitwise_and_compare_forms() {
    let fmla = emit(MInst::VecFmla {
        shape: super::VecShape::FourS,
        dst: Writable::from_reg(vec_reg(0)),
        acc: vec_reg(3),
        lhs: vec_reg(1),
        rhs: vec_reg(2),
    });
    assert_eq!(fmla, "mov v0.16b, v3.16b\n    fmla v0.4s, v1.4s, v2.4s");

    for (op, mnemonic) in [
        (super::VecBitOp::And, "and"),
        (super::VecBitOp::Orr, "orr"),
        (super::VecBitOp::Eor, "eor"),
    ] {
        let text = emit(MInst::VecBitwise {
            op,
            dst: Writable::from_reg(vec_reg(0)),
            lhs: vec_reg(1),
            rhs: vec_reg(2),
        });
        assert_eq!(text, format!("{mnemonic} v0.16b, v1.16b, v2.16b"));
    }

    let cmeq = emit(MInst::VecCmp {
        op: super::VecCmpOp::Eq,
        shape: super::VecShape::FourS,
        dst: Writable::from_reg(vec_reg(0)),
        lhs: vec_reg(1),
        rhs: vec_reg(2),
    });
    assert_eq!(cmeq, "cmeq v0.4s, v1.4s, v2.4s");

    let bsl = emit(MInst::VecBsl {
        dst: Writable::from_reg(vec_reg(0)),
        mask: vec_reg(3),
        lhs: vec_reg(1),
        rhs: vec_reg(2),
    });
    assert_eq!(bsl, "mov v0.16b, v3.16b\n    bsl v0.16b, v1.16b, v2.16b");
}

#[test]
fn emits_vector_cvt_and_horizontal_add() {
    let scvtf = emit(MInst::VecCvt {
        op: super::VecCvtOp::Scvtf,
        shape: super::VecShape::FourS,
        dst: Writable::from_reg(vec_reg(0)),
        src: vec_reg(1),
    });
    assert_eq!(scvtf, "scvtf v0.4s, v1.4s");

    let fcvtzs = emit(MInst::VecCvt {
        op: super::VecCvtOp::Fcvtzs,
        shape: super::VecShape::FourS,
        dst: Writable::from_reg(vec_reg(2)),
        src: vec_reg(3),
    });
    assert_eq!(fcvtzs, "fcvtzs v2.4s, v3.4s");

    let addv = emit(MInst::VecAddv {
        dst: Writable::from_reg(float_reg(0)),
        src: vec_reg(1),
    });
    assert_eq!(addv, "addv s0, v1.4s");
}

#[test]
fn emits_vector_mov_imm_lane_and_minmax_forms() {
    let movi = emit(MInst::VecMovImm {
        shape: super::VecShape::FourS,
        dst: Writable::from_reg(vec_reg(0)),
        imm: 0x3f,
        shift: 0,
    });
    assert_eq!(movi, "movi v0.4s, #0x3f");

    let movi_shift = emit(MInst::VecMovImm {
        shape: super::VecShape::FourS,
        dst: Writable::from_reg(vec_reg(0)),
        imm: 0xff,
        shift: 8,
    });
    assert_eq!(movi_shift, "movi v0.4s, #0xff, lsl #8");

    let extract = emit(MInst::VecExtractLane {
        size: OperandSize::Size32,
        dst: Writable::from_reg(int_reg(0)),
        src: vec_reg(1),
        lane: 2,
    });
    assert_eq!(extract, "mov w0, v1.s[2]");

    let extract_64 = emit(MInst::VecExtractLane {
        size: OperandSize::Size64,
        dst: Writable::from_reg(int_reg(0)),
        src: vec_reg(1),
        lane: 1,
    });
    assert_eq!(extract_64, "mov x0, v1.d[1]");

    let insert = emit(MInst::VecInsertLane {
        size: OperandSize::Size32,
        dst: Writable::from_reg(vec_reg(2)),
        vector: vec_reg(4),
        src: int_reg(3),
        lane: 0,
    });
    assert_eq!(insert, "mov v2.16b, v4.16b\n    mov v2.s[0], w3");

    for (op, mnemonic) in [
        (super::VecMinMaxOp::Smin, "smin"),
        (super::VecMinMaxOp::Smax, "smax"),
        (super::VecMinMaxOp::Umin, "umin"),
        (super::VecMinMaxOp::Umax, "umax"),
        (super::VecMinMaxOp::Fmin, "fmin"),
        (super::VecMinMaxOp::Fmax, "fmax"),
    ] {
        let text = emit(MInst::VecMinMax {
            op,
            shape: super::VecShape::FourS,
            dst: Writable::from_reg(vec_reg(0)),
            lhs: vec_reg(1),
            rhs: vec_reg(2),
        });
        assert_eq!(text, format!("{mnemonic} v0.4s, v1.4s, v2.4s"));
    }
}

#[test]
fn emits_vector_shift_div_neg_forms() {
    // Immediate shifts: shl / ushr / sshr with a #imm amount.
    for (op, mnemonic) in [
        (super::VecShiftOp::Shl, "shl"),
        (super::VecShiftOp::Shr, "ushr"),
        (super::VecShiftOp::Sar, "sshr"),
    ] {
        let text = emit(MInst::VecShift {
            op,
            shape: super::VecShape::FourS,
            dst: Writable::from_reg(vec_reg(0)),
            lhs: vec_reg(1),
            rhs: vec_reg(2),
            imm: Some(3),
        });
        assert_eq!(text, format!("{mnemonic} v0.4s, v1.4s, #3"));
    }
    // Register (variable-amount) forms: sshl / ushl / sshl v,v,v.
    for (op, mnemonic) in [
        (super::VecShiftOp::Shl, "sshl"),
        (super::VecShiftOp::Shr, "ushl"),
        (super::VecShiftOp::Sar, "sshl"),
    ] {
        let text = emit(MInst::VecShift {
            op,
            shape: super::VecShape::FourS,
            dst: Writable::from_reg(vec_reg(0)),
            lhs: vec_reg(1),
            rhs: vec_reg(2),
            imm: None,
        });
        assert_eq!(text, format!("{mnemonic} v0.4s, v1.4s, v2.4s"));
    }
    let neg = emit(MInst::VecNeg {
        shape: super::VecShape::FourS,
        dst: Writable::from_reg(vec_reg(0)),
        src: vec_reg(2),
    });
    assert_eq!(neg, "neg v0.4s, v2.4s");
    let fdiv = emit(MInst::VecDiv {
        shape: super::VecShape::FourS,
        dst: Writable::from_reg(vec_reg(0)),
        lhs: vec_reg(1),
        rhs: vec_reg(2),
    });
    assert_eq!(fdiv, "fdiv v0.4s, v1.4s, v2.4s");
    // Float-scalar dup prints through the aliased vector register
    // (`s0` is the low 32 bits of `v0`); LLVM MC rejects `dup vd.4s, sn`.
    let dup_float = emit(MInst::VecDup {
        shape: super::VecShape::FourS,
        dst: Writable::from_reg(vec_reg(0)),
        src: float_reg(3),
    });
    assert_eq!(dup_float, "dup v0.4s, v3.s[0]");
}

#[test]
fn vector_instructions_expose_their_operands() {
    let mut inst = MInst::VecArithRRR {
        op: super::VecArithOp::Add,
        shape: super::VecShape::FourS,
        dst: Writable::from_reg(virtual_reg(0, RegClass::Vector)),
        lhs: virtual_reg(1, RegClass::Vector),
        rhs: virtual_reg(2, RegClass::Vector),
    };
    let mut visitor = TestOperandVisitor(Vec::new());
    inst.get_operands(&mut visitor);
    assert_eq!(visitor.0.len(), 3);
    assert_eq!(visitor.0[0].1, OperandConstraint::Reg);
    assert_eq!(visitor.0[2].2, OperandKind::Def);
}

#[test]
fn emits_sign_extension() {
    let text = emit(MInst::Sxtw {
        size: OperandSize::Size32,
        dst: Writable::from_reg(int_reg(3)),
        src: int_reg(2),
    });
    assert_eq!(text, "sxtw x3, w2");
}
