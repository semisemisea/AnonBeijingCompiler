/*
 * Adapted from regalloc2 0.15.1 src/ion/requirement.rs.
 *
 * Released under the Apache License 2.0 with LLVM Exception. See the
 * repository LICENSE for the full license text.
 */

use crate::reg_alloc::reg::{OperandConstraint, PReg, ProgPoint};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequirementConflict;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequirementConflictAt {
    StackToReg(ProgPoint),
    RegToStack(ProgPoint),
    Other(ProgPoint),
}
impl RequirementConflictAt {
    pub fn suggested_split_point(self) -> ProgPoint {
        match self {
            Self::StackToReg(p) | Self::RegToStack(p) | Self::Other(p) => p,
        }
    }
    pub fn should_trim_edges_around_split(self) -> bool {
        matches!(self, Self::Other(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requirement {
    Any,
    Register,
    FixedReg(PReg),
    Limit(usize),
    Stack,
    FixedStack(PReg),
}
impl Requirement {
    pub fn from_constraint(constraint: OperandConstraint, is_stack: impl Fn(PReg) -> bool) -> Self {
        match constraint {
            OperandConstraint::Any => Self::Any,
            OperandConstraint::Reg | OperandConstraint::Reuse(_) => Self::Register,
            OperandConstraint::Stack => Self::Stack,
            OperandConstraint::Limit(n) => Self::Limit(n),
            OperandConstraint::FixedReg(reg) if is_stack(reg) => Self::FixedStack(reg),
            OperandConstraint::FixedReg(reg) => Self::FixedReg(reg),
        }
    }
    pub fn merge(self, other: Self) -> Result<Self, RequirementConflict> {
        use Requirement::*;
        match (self, other) {
            (Any, x) | (x, Any) => Ok(x),
            (Register, Register) | (Stack, Stack) => Ok(self),
            (Limit(a), Limit(b)) => Ok(Limit(a.min(b))),
            (FixedReg(a), FixedReg(b)) if a == b => Ok(FixedReg(a)),
            (FixedStack(a), FixedStack(b)) if a == b => Ok(FixedStack(a)),
            (Limit(a), Register) | (Register, Limit(a)) => Ok(Limit(a)),
            (Limit(a), FixedReg(reg)) | (FixedReg(reg), Limit(a)) if reg.hw_enc() < a => {
                Ok(FixedReg(reg))
            }
            (Register, FixedReg(reg)) | (FixedReg(reg), Register) => Ok(FixedReg(reg)),
            (Stack, FixedStack(reg)) | (FixedStack(reg), Stack) => Ok(FixedStack(reg)),
            _ => Err(RequirementConflict),
        }
    }
    pub fn is_stack(self) -> bool {
        matches!(self, Self::Stack | Self::FixedStack(_))
    }
    pub fn is_reg(self) -> bool {
        matches!(self, Self::Register | Self::FixedReg(_) | Self::Limit(_))
    }
}
