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
        // Scalar comparisons produce an i32 boolean. Vector comparisons produce
        // a vector mask (all-ones / zero lanes) of the operand's vector type.
        let ty = if op.is_compare() && !ty.is_vector() {
            Type::get_i32()
        } else {
            ty
        };
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
    /// Vector lane-wise minimum (signed for integers).
    Min,
    /// Vector lane-wise maximum (signed for integers).
    Max,
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

    pub fn is_commutative_for(&self, operand_ty: &Type) -> bool {
        matches!(self, BinaryOp::NotEq | BinaryOp::Eq | BinaryOp::Min | BinaryOp::Max)
            || (operand_ty.is_i32()
                && matches!(
                    self,
                    BinaryOp::Add | BinaryOp::Mul | BinaryOp::And | BinaryOp::Or | BinaryOp::Xor
                ))
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
                BinaryOp::Min => "min",
                BinaryOp::Max => "max",
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{BinaryOp, Type};

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
    fn classifies_commutativity_by_operand_type() {
        let i32_ty = Type::get_i32();
        let f32_ty = Type::get_f32();

        for op in [
            BinaryOp::Add,
            BinaryOp::Mul,
            BinaryOp::And,
            BinaryOp::Or,
            BinaryOp::Xor,
        ] {
            assert!(op.is_commutative_for(&i32_ty));
            assert!(!op.is_commutative_for(&f32_ty));
        }
        for op in [BinaryOp::Eq, BinaryOp::NotEq, BinaryOp::Min, BinaryOp::Max] {
            assert!(op.is_commutative_for(&i32_ty));
            assert!(op.is_commutative_for(&f32_ty));
        }
        for op in [
            BinaryOp::Sub,
            BinaryOp::Div,
            BinaryOp::Rem,
            BinaryOp::Lt,
            BinaryOp::Shl,
        ] {
            assert!(!op.is_commutative_for(&i32_ty));
            assert!(!op.is_commutative_for(&f32_ty));
        }
    }

    #[test]
    fn min_and_max_display_and_are_not_comparisons() {
        assert_eq!(BinaryOp::Min.to_string(), "min");
        assert_eq!(BinaryOp::Max.to_string(), "max");
        assert!(!BinaryOp::Min.is_compare());
        assert!(!BinaryOp::Max.is_compare());
        assert_eq!(BinaryOp::Min.complement_integer_compare(), None);
        assert_eq!(BinaryOp::Max.swap_compare_args(), None);
    }
}
