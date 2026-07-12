//! Fundemental type for Machine-level IR.
//!
//! HLIR type contains some semantic meaning and type constraints,
//! which will limit the usage at Machine-level IR.
//!
//! In order to have more low-level control, type system in MIR contains:
//! - 32 bit integer      as i32
//! - 32 bit float        as f32
//! - 64 bit address      as u64
//! - SIMD Vector (TODO)
//!
//! Notably, all the integer type is **sign-agnostic**.
//!
//! 0b 0000 0000 0000 0000
//! 0b 0000 0000 0000 00xx -> shows the fundemental type. 01 for Integer, 10 for Float.
//! 0b 0000 0000 0000 xx00 -> shows the bitwidth of type. 01 for 32 bits, 10 for 64 bits.
//! 0b 0000 0000 xxxx 0000 -> (planned) to indicate the lanes of vector.
//! 0b 1111 1111 1111 1111 -> invalid type
use crate::prelude::*;

#[derive(Debug, Clone, Copy)]
pub struct Type(u16);

impl std::ops::Add for Type {
    type Output = Type;
    fn add(self, rhs: Self) -> Self::Output {
        Type(self.0 + rhs.0)
    }
}

pub const INVALID: Type = Type(0xFFFF);
pub const INT: Type = Type(0x0001);
pub const FLOAT: Type = Type(0x0010);
pub const B32: Type = Type(0x0100);
pub const B64: Type = Type(0x1000);

impl Type {
    pub fn new_i32() -> Type {
        INT + B32
    }

    pub fn new_i64() -> Type {
        INT + B64
    }

    pub fn new_f32() -> Type {
        FLOAT + B32
    }

    pub fn invalid() -> Type {
        INVALID
    }
}

impl From<HirType> for Type {
    fn from(value: HirType) -> Self {
        match value.kind() {
            raana_ir::ir::TypeKind::Unit => unreachable!(
                "should not encounter to allocate a unit type value. please filter it out before allocation"
            ),
            raana_ir::ir::TypeKind::Int32 => Type::new_i32(),
            raana_ir::ir::TypeKind::Float32 => Type::new_f32(),
            raana_ir::ir::TypeKind::String => {
                unreachable!("only used for a potential global value")
            }
            raana_ir::ir::TypeKind::Array(_, _) => {
                todo!("should not encounter to allocate a unit type value.")
            }
            // Pointer is treated as unsigned 64 bit integer.
            // "i64" doesn't indicate that it's signed integer.
            raana_ir::ir::TypeKind::Pointer(_) => Type::new_i64(),
            raana_ir::ir::TypeKind::Function(..) => unreachable!(),
            raana_ir::ir::TypeKind::ArgList => unreachable!(),
        }
    }
}

impl From<&HirType> for Type {
    fn from(value: &HirType) -> Self {
        match value.kind() {
            raana_ir::ir::TypeKind::Unit => unreachable!(
                "should not encounter to allocate a unit type value. please filter it out before allocation"
            ),
            raana_ir::ir::TypeKind::Int32 => Type::new_i32(),
            raana_ir::ir::TypeKind::Float32 => Type::new_f32(),
            raana_ir::ir::TypeKind::String => {
                unreachable!("only used for a potential global value")
            }
            raana_ir::ir::TypeKind::Array(_, _) => {
                todo!("should not encounter to allocate a unit type value.")
            }
            // Pointer is treated as unsigned 64 bit integer.
            // "i64" doesn't indicate that it's signed integer.
            raana_ir::ir::TypeKind::Pointer(_) => Type::new_i64(),
            raana_ir::ir::TypeKind::Function(..) => unreachable!(),
            raana_ir::ir::TypeKind::ArgList => unreachable!(),
        }
    }
}
