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
    Cmp {
        lhs: Reg,
        rhs: Reg,
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
    Ret,
    Nop,
}

impl MachInst for Inst {
    type ABISpec = AArch64Abi;

    fn get_operands(&mut self, collector: &mut impl OperandVisitor) {
        match self {
            Self::Mov { dst, src, .. } => {
                collector.reg_use(src);
                collector.reg_def(&mut Writable::from_reg(*dst));
            }
            Self::Add { dst, lhs, rhs, .. } => {
                collector.reg_use(lhs);
                collector.reg_use(rhs);
                collector.reg_def(&mut Writable::from_reg(*dst));
            }
            Self::Cmp { lhs, rhs, .. } => {
                collector.reg_use(lhs);
                collector.reg_use(rhs);
            }
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
            Self::Ret => MachTerminator::Return,
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
