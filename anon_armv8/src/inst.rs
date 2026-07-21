use taki_mir::{
    block_order::MirBlockIndex,
    reg_alloc::reg::{OperandVisitor, OperandVisitorImpl},
    register::{Reg, Writable},
    types::Type,
    vcode::{CallType, MachInst, MachInstEmit, MachTerminator},
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
    Call {
        symbol: String,
    },
    /// Return an i32 value through the AAPCS64 w0 register.
    RetI32 {
        src: Reg,
    },
    Ret,
    Nop,
}

impl MachInst for Inst {
    type ABISpec = AArch64Abi;

    fn get_operands(&mut self, collector: &mut impl OperandVisitor) {
        match self {
            Self::MovImm { dst, .. } => collector.reg_def(&mut Writable::from_reg(*dst)),
            Self::Mov { dst, src, .. } => {
                collector.reg_use(src);
                collector.reg_def(&mut Writable::from_reg(*dst));
            }
            Self::Add { dst, lhs, rhs, .. } => {
                collector.reg_use(lhs);
                collector.reg_use(rhs);
                collector.reg_def(&mut Writable::from_reg(*dst));
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
                collector.reg_def(&mut Writable::from_reg(*dst));
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
                collector.reg_def(&mut Writable::from_reg(*dst));
            }
            Self::Cmp { lhs, rhs, .. } => {
                collector.reg_use(lhs);
                collector.reg_use(rhs);
            }
            Self::CmpZero { src, .. } => collector.reg_use(src),
            Self::CSet { dst, .. } => collector.reg_def(&mut Writable::from_reg(*dst)),
            Self::Call { .. } => {
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
            Self::Jump { .. } | Self::Branch { .. } | Self::Ret | Self::Nop => {}
        }
    }

    fn is_move(&self) -> Option<(Writable<Reg>, Reg)> {
        match self {
            Self::Mov { dst, src, .. } => Some((Writable::from_reg(*dst), *src)),
            _ => None,
        }
    }

    fn is_term(&self) -> MachTerminator {
        match self {
            Self::RetI32 { .. } | Self::Ret => MachTerminator::Return,
            Self::Jump { .. } | Self::Branch { .. } => MachTerminator::Branch,
            _ => MachTerminator::None,
        }
    }

    fn call_type(&self) -> CallType {
        match self {
            Self::Call { .. } => CallType::Call,
            _ => CallType::None,
        }
    }

    fn is_mem_access(&self) -> bool {
        false
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

impl MachInstEmit for Inst {}
