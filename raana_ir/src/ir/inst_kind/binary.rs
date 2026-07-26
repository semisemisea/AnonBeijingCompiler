use crate::ir::{
    inst_kind::InstKind,
    instruction::{Inst, InstData},
    types::Type,
};

#[derive(Debug, Clone)]
pub struct Binary {
    op: BinaryOp,
    lhs: Inst,
    rhs: Inst,
}

impl Binary {
    pub fn op(&self) -> BinaryOp {
        self.op
    }

    pub fn lhs(&self) -> Inst {
        self.lhs
    }

    pub fn rhs(&self) -> Inst {
        self.rhs
    }

    pub fn new_data(lhs: Inst, rhs: Inst, op: BinaryOp, ty: Type) -> InstData {
        let ty = if op.is_compare() { Type::get_i32() } else { ty };
        InstData::new(ty, InstKind::Binary(Binary { lhs, rhs, op }))
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    NotEq,
    Eq,
    Gt,
    Lt,
    Ge,
    Le,
    And,
    Or,
    Xor,
    Shl,
    Shr,
    Sar,
}

impl BinaryOp {
    pub fn is_compare(&self) -> bool {
        matches!(
            self,
            BinaryOp::NotEq
                | BinaryOp::Eq
                | BinaryOp::Gt
                | BinaryOp::Lt
                | BinaryOp::Ge
                | BinaryOp::Le
        )
    }

    /// Returns the integer comparison that is true exactly when `self` is
    /// false. Floating-point relational comparisons need unordered predicates,
    /// which RaanaIR does not represent, so callers must first establish that
    /// the operands are integers.
    pub fn complement_integer_compare(&self) -> Option<Self> {
        Some(match self {
            BinaryOp::NotEq => BinaryOp::Eq,
            BinaryOp::Eq => BinaryOp::NotEq,
            BinaryOp::Gt => BinaryOp::Le,
            BinaryOp::Lt => BinaryOp::Ge,
            BinaryOp::Ge => BinaryOp::Lt,
            BinaryOp::Le => BinaryOp::Gt,
            _ => return None,
        })
    }

    /// Folds the operator over two `i32` operands, using wrapping arithmetic
    /// and treating shift amounts modulo the operand width.
    ///
    /// Returns `None` for division or remainder by zero; callers decide how to
    /// report that.
    pub fn eval_i32(self, lhs: i32, rhs: i32) -> Option<i32> {
        Some(match self {
            BinaryOp::NotEq => (lhs != rhs) as i32,
            BinaryOp::Eq => (lhs == rhs) as i32,
            BinaryOp::Gt => (lhs > rhs) as i32,
            BinaryOp::Lt => (lhs < rhs) as i32,
            BinaryOp::Ge => (lhs >= rhs) as i32,
            BinaryOp::Le => (lhs <= rhs) as i32,
            BinaryOp::Add => lhs.wrapping_add(rhs),
            BinaryOp::Sub => lhs.wrapping_sub(rhs),
            BinaryOp::Mul => lhs.wrapping_mul(rhs),
            BinaryOp::Div | BinaryOp::Rem if rhs == 0 => return None,
            BinaryOp::Div => lhs.wrapping_div(rhs),
            BinaryOp::Rem => lhs.wrapping_rem(rhs),
            BinaryOp::And => lhs & rhs,
            BinaryOp::Or => lhs | rhs,
            BinaryOp::Xor => lhs ^ rhs,
            BinaryOp::Shl => lhs.wrapping_shl(rhs as u32),
            BinaryOp::Shr => (lhs as u32).wrapping_shr(rhs as u32) as i32,
            BinaryOp::Sar => lhs.wrapping_shr(rhs as u32),
        })
    }

    /// Returns the comparison equivalent to swapping the operands.
    pub fn swap_compare_args(&self) -> Option<Self> {
        Some(match self {
            BinaryOp::NotEq => BinaryOp::NotEq,
            BinaryOp::Eq => BinaryOp::Eq,
            BinaryOp::Gt => BinaryOp::Lt,
            BinaryOp::Lt => BinaryOp::Gt,
            BinaryOp::Ge => BinaryOp::Le,
            BinaryOp::Le => BinaryOp::Ge,
            _ => return None,
        })
    }
}

impl std::fmt::Display for BinaryOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                BinaryOp::Add => "add",
                BinaryOp::Sub => "sub",
                BinaryOp::Mul => "mul",
                BinaryOp::Div => "div",
                BinaryOp::Rem => "rem",
                BinaryOp::NotEq => "neq",
                BinaryOp::Eq => "eq",
                BinaryOp::Gt => "gt",
                BinaryOp::Lt => "lt",
                BinaryOp::Le => "le",
                BinaryOp::Ge => "ge",
                BinaryOp::And => "and",
                BinaryOp::Or => "or",
                BinaryOp::Xor => "xor",
                BinaryOp::Shl => "shl",
                BinaryOp::Shr => "shr",
                BinaryOp::Sar => "sar",
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::BinaryOp;

    #[test]
    fn complements_integer_comparisons() {
        for (op, complement) in [
            (BinaryOp::Eq, BinaryOp::NotEq),
            (BinaryOp::NotEq, BinaryOp::Eq),
            (BinaryOp::Lt, BinaryOp::Ge),
            (BinaryOp::Le, BinaryOp::Gt),
            (BinaryOp::Gt, BinaryOp::Le),
            (BinaryOp::Ge, BinaryOp::Lt),
        ] {
            assert_eq!(op.complement_integer_compare(), Some(complement));
        }
        assert_eq!(BinaryOp::Add.complement_integer_compare(), None);
    }

    #[test]
    fn swaps_comparison_arguments() {
        for (op, swapped) in [
            (BinaryOp::Eq, BinaryOp::Eq),
            (BinaryOp::NotEq, BinaryOp::NotEq),
            (BinaryOp::Lt, BinaryOp::Gt),
            (BinaryOp::Le, BinaryOp::Ge),
            (BinaryOp::Gt, BinaryOp::Lt),
            (BinaryOp::Ge, BinaryOp::Le),
        ] {
            assert_eq!(op.swap_compare_args(), Some(swapped));
        }
        assert_eq!(BinaryOp::Add.swap_compare_args(), None);
    }

    #[test]
    fn folds_integer_operands() {
        assert_eq!(BinaryOp::Add.eval_i32(i32::MAX, 1), Some(i32::MIN));
        assert_eq!(BinaryOp::Div.eval_i32(i32::MIN, -1), Some(i32::MIN));
        assert_eq!(BinaryOp::Shr.eval_i32(-1, 1), Some(i32::MAX));
        assert_eq!(BinaryOp::Sar.eval_i32(-1, 1), Some(-1));
        assert_eq!(BinaryOp::Div.eval_i32(1, 0), None);
        assert_eq!(BinaryOp::Rem.eval_i32(1, 0), None);
    }
}
