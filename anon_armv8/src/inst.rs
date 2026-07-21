use taki_mir::{
    abi::ArgPair,
    block_order::MirBlockIndex,
    reg_alloc::reg::{OperandVisitor, OperandVisitorImpl},
    register::{Reg, Writable},
    types::Type,
    vcode::{CallType, MachInst, MachInstEmit, MachInstInfo, MachTerminator},
};

use crate::{abi::AArch64Abi, regs};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cond {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Cond {
    pub fn asm(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Ne => "ne",
            Self::Lt => "lt",
            Self::Le => "le",
            Self::Gt => "gt",
            Self::Ge => "ge",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Inst {
    MovImm {
        dst: Reg,
        value: i32,
    },
    Mov {
        dst: Reg,
        src: Reg,
        ty: Type,
    },
    /// Move-wide keep form with a tied input/output register.
    MovK {
        dst: Reg,
        src: Reg,
        imm16: u16,
        shift: u8,
        ty: Type,
    },
    FAdd {
        dst: Reg,
        lhs: Reg,
        rhs: Reg,
    },
    Add {
        dst: Reg,
        lhs: Reg,
        rhs: Reg,
        ty: Type,
    },
    Sub {
        dst: Reg,
        lhs: Reg,
        rhs: Reg,
        ty: Type,
    },
    Mul {
        dst: Reg,
        lhs: Reg,
        rhs: Reg,
        ty: Type,
    },
    SDiv {
        dst: Reg,
        lhs: Reg,
        rhs: Reg,
        ty: Type,
    },
    MSub {
        dst: Reg,
        mul_lhs: Reg,
        mul_rhs: Reg,
        sub: Reg,
        ty: Type,
    },
    And {
        dst: Reg,
        lhs: Reg,
        rhs: Reg,
        ty: Type,
    },
    Orr {
        dst: Reg,
        lhs: Reg,
        rhs: Reg,
        ty: Type,
    },
    Eor {
        dst: Reg,
        lhs: Reg,
        rhs: Reg,
        ty: Type,
    },
    Lsl {
        dst: Reg,
        lhs: Reg,
        rhs: Reg,
        ty: Type,
    },
    Lsr {
        dst: Reg,
        lhs: Reg,
        rhs: Reg,
        ty: Type,
    },
    Asr {
        dst: Reg,
        lhs: Reg,
        rhs: Reg,
        ty: Type,
    },
    Cmp {
        lhs: Reg,
        rhs: Reg,
        ty: Type,
    },
    CmpZero {
        src: Reg,
        ty: Type,
    },
    CSet {
        dst: Reg,
        cond: Cond,
    },
    Jump {
        target: MirBlockIndex,
    },
    Branch {
        cond: Cond,
        target: MirBlockIndex,
    },
    Args {
        args: Vec<ArgPair>,
    },
    Call {
        symbol: String,
        args: Vec<ArgPair>,
        result: Option<(Reg, Type)>,
    },
    /// Return an i32 value through the AAPCS64 w0 register.
    RetI32 {
        src: Reg,
    },
    /// Return an f32 value through the AAPCS64 s0 register.
    RetF32 {
        src: Reg,
    },
    Ret,
    Nop,
}

impl MachInst for Inst {
    type ABISpec = AArch64Abi;

    fn get_operands(&mut self, collector: &mut impl OperandVisitor) {
        match self {
            Self::MovImm { dst, .. } => collector.reg_def_reg(dst),
            Self::Mov { dst, src, .. } => {
                collector.reg_use(src);
                collector.reg_def_reg(dst);
            }
            Self::MovK { dst, src, .. } => {
                collector.reg_use(src);
                collector.reg_reuse_def_reg(dst, 0);
            }
            Self::FAdd { dst, lhs, rhs } => {
                collector.reg_use(lhs);
                collector.reg_use(rhs);
                collector.reg_def_reg(dst);
            }
            Self::Add { dst, lhs, rhs, .. } => {
                collector.reg_use(lhs);
                collector.reg_use(rhs);
                collector.reg_def_reg(dst);
            }
            Self::Sub { dst, lhs, rhs, .. }
            | Self::Mul { dst, lhs, rhs, .. }
            | Self::SDiv { dst, lhs, rhs, .. }
            | Self::And { dst, lhs, rhs, .. }
            | Self::Orr { dst, lhs, rhs, .. }
            | Self::Eor { dst, lhs, rhs, .. }
            | Self::Lsl { dst, lhs, rhs, .. }
            | Self::Lsr { dst, lhs, rhs, .. }
            | Self::Asr { dst, lhs, rhs, .. } => {
                collector.reg_use(lhs);
                collector.reg_use(rhs);
                collector.reg_def_reg(dst);
            }
            Self::MSub {
                dst,
                mul_lhs,
                mul_rhs,
                sub,
                ..
            } => {
                collector.reg_use(mul_lhs);
                collector.reg_use(mul_rhs);
                collector.reg_use(sub);
                collector.reg_def_reg(dst);
            }
            Self::Cmp { lhs, rhs, .. } => {
                collector.reg_use(lhs);
                collector.reg_use(rhs);
            }
            Self::CmpZero { src, .. } => collector.reg_use(src),
            Self::CSet { dst, .. } => collector.reg_def_reg(dst),
            Self::Args { args } => {
                for arg in args {
                    collector.reg_fixed_def_reg(&mut arg.vreg, arg.preg);
                }
            }
            Self::Call { args, result, .. } => {
                for arg in args {
                    collector.reg_fixed_use(&mut arg.vreg, arg.preg);
                }
                if let Some((result, ty)) = result {
                    let preg = if ty.is_f32() {
                        regs::float_reg(0)
                    } else {
                        regs::int_reg(0)
                    };
                    collector.reg_fixed_def_reg(result, preg);
                }
                let mut clobbers = taki_mir::reg_alloc::reg::PRegSet::empty();
                for index in 0..=18 {
                    clobbers.add(regs::int_preg(index));
                }
                for index in 0..=7 {
                    clobbers.add(regs::float_preg(index));
                }
                for index in 16..=30 {
                    clobbers.add(regs::float_preg(index));
                }
                collector.reg_clobbers(clobbers);
            }
            Self::RetI32 { src } => collector.reg_fixed_use(src, regs::int_reg(0)),
            Self::RetF32 { src } => collector.reg_fixed_use(src, regs::float_reg(0)),
            Self::Jump { .. } | Self::Branch { .. } | Self::Ret | Self::Nop => {}
        }
    }

    fn is_move(&self) -> Option<(Writable<Reg>, Reg)> {
        match self {
            Self::Mov { dst, src, .. } => Some((Writable::from_reg(*dst), *src)),
            _ => None,
        }
    }

    fn inst_info(&self) -> MachInstInfo {
        match self {
            Self::Mov { .. } => MachInstInfo {
                is_move: true,
                ..Default::default()
            },
            Self::Cmp { .. } | Self::CmpZero { .. } => MachInstInfo {
                produces_flags: true,
                ..Default::default()
            },
            Self::Branch { .. } => MachInstInfo {
                terminator: MachTerminator::Branch,
                uses_flags: true,
                has_side_effect: true,
                ..Default::default()
            },
            Self::Jump { .. } => MachInstInfo {
                terminator: MachTerminator::Branch,
                has_side_effect: true,
                ..Default::default()
            },
            Self::RetI32 { .. } | Self::RetF32 { .. } | Self::Ret => MachInstInfo {
                terminator: MachTerminator::Return,
                has_side_effect: true,
                ..Default::default()
            },
            Self::Call { .. } => MachInstInfo {
                call_type: CallType::Call,
                has_side_effect: true,
                ..Default::default()
            },
            _ => MachInstInfo::default(),
        }
    }

    fn rc_for_type(
        ty: Type,
    ) -> (
        &'static [taki_mir::reg_alloc::reg::RegClass],
        &'static [Type],
    ) {
        use taki_mir::reg_alloc::reg::RegClass;
        const INT_CLASSES: [RegClass; 1] = [RegClass::Int];
        const FLOAT_CLASSES: [RegClass; 1] = [RegClass::Float];
        const I32: [Type; 1] = [Type::new_i32()];
        const I64: [Type; 1] = [Type::new_i64()];
        const F32: [Type; 1] = [Type::new_f32()];
        if ty.is_f32() {
            (&FLOAT_CLASSES, &F32)
        } else if ty.is_i32() {
            (&INT_CLASSES, &I32)
        } else {
            (&INT_CLASSES, &I64)
        }
    }

    fn gen_jump(target: MirBlockIndex) -> Self {
        Self::Jump { target }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingVisitor {
        operands: Vec<(Reg, taki_mir::reg_alloc::reg::OperandConstraint)>,
        clobbers: taki_mir::reg_alloc::reg::PRegSet,
    }

    impl OperandVisitor for RecordingVisitor {
        fn add_operand(
            &mut self,
            reg: &mut Reg,
            constraint: taki_mir::reg_alloc::reg::OperandConstraint,
            _: taki_mir::reg_alloc::reg::OperandKind,
            _: taki_mir::reg_alloc::reg::OperandPos,
        ) {
            self.operands.push((*reg, constraint));
        }

        fn reg_clobbers(&mut self, regs: taki_mir::reg_alloc::reg::PRegSet) {
            self.clobbers.union_from(regs);
        }
    }

    #[test]
    fn fixed_abi_operands_and_call_clobbers_are_collected() {
        let mut inst = Inst::Call {
            symbol: "callee".into(),
            args: vec![ArgPair {
                vreg: Reg::from_virtual_reg(taki_mir::reg_alloc::reg::VReg::new(
                    192,
                    taki_mir::reg_alloc::reg::RegClass::Int,
                )),
                preg: regs::int_reg(0),
                ty: Type::new_i32(),
            }],
            result: Some((
                Reg::from_virtual_reg(taki_mir::reg_alloc::reg::VReg::new(
                    193,
                    taki_mir::reg_alloc::reg::RegClass::Int,
                )),
                Type::new_i32(),
            )),
        };
        let mut visitor = RecordingVisitor::default();
        inst.get_operands(&mut visitor);

        assert_eq!(visitor.operands.len(), 2);
        assert!(visitor.clobbers.contains(regs::int_preg(0)));
        assert!(visitor.clobbers.contains(regs::float_preg(0)));
    }

    #[test]
    fn instruction_metadata_classifies_control_flow_and_flags() {
        let cmp = Inst::Cmp {
            lhs: regs::int_reg(0),
            rhs: regs::int_reg(1),
            ty: Type::new_i32(),
        }
        .inst_info();
        assert!(cmp.produces_flags);

        let branch = Inst::Branch {
            cond: Cond::Eq,
            target: MirBlockIndex::new(0),
        }
        .inst_info();
        assert_eq!(branch.terminator, MachTerminator::Branch);
        assert!(branch.uses_flags);
        assert!(branch.has_side_effect);

        let call = Inst::Call {
            symbol: "callee".into(),
            args: vec![],
            result: None,
        }
        .inst_info();
        assert_eq!(call.call_type, CallType::Call);
        assert!(call.has_side_effect);

        let mov = Inst::Mov {
            dst: regs::int_reg(0),
            src: regs::int_reg(1),
            ty: Type::new_i32(),
        }
        .inst_info();
        assert!(mov.is_move);

        let ret = Inst::Ret.inst_info();
        assert_eq!(ret.terminator, MachTerminator::Return);
    }
}

impl MachInstEmit for Inst {}
