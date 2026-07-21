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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoweredType(u16);

impl std::ops::Add for LoweredType {
    type Output = LoweredType;
    fn add(self, rhs: Self) -> Self::Output {
        LoweredType(self.0 + rhs.0)
    }
}

const fn combine(lhs: LoweredType, rhs: LoweredType) -> LoweredType {
    LoweredType(lhs.0 + rhs.0)
}

const INVALID: LoweredType = LoweredType(0xFFFF);
const INT: LoweredType = LoweredType(0x0001);
const FLOAT: LoweredType = LoweredType(0x0010);

const B32: LoweredType = LoweredType(0x0100);
const B64: LoweredType = LoweredType(0x1000);

pub const I32: LoweredType = combine(INT, B32);
pub const I64: LoweredType = combine(INT, B64);
pub const F32: LoweredType = combine(FLOAT, B32);

impl LoweredType {
    pub fn new_i32() -> LoweredType {
        INT + B32
    }

    pub fn new_i64() -> LoweredType {
        INT + B64
    }

    pub fn new_f32() -> LoweredType {
        FLOAT + B32
    }

    pub fn invalid() -> LoweredType {
        INVALID
    }
}

impl From<HirType> for LoweredType {
    fn from(value: HirType) -> Self {
        match value.kind() {
            raana_ir::ir::TypeKind::Unit => unreachable!(
                "should not encounter to allocate a unit type value. please filter it out before allocation"
            ),
            raana_ir::ir::TypeKind::Int32 => LoweredType::new_i32(),
            raana_ir::ir::TypeKind::Float32 => LoweredType::new_f32(),
            raana_ir::ir::TypeKind::String => {
                unreachable!("only used for a potential global value")
            }
            raana_ir::ir::TypeKind::Array(_, _) => {
                todo!("should not encounter to allocate a unit type value.")
            }
            // Pointer is treated as unsigned 64 bit integer.
            // "i64" doesn't indicate that it's signed integer.
            raana_ir::ir::TypeKind::Pointer(_) => LoweredType::new_i64(),
            raana_ir::ir::TypeKind::Function(..) => unreachable!(),
            raana_ir::ir::TypeKind::ArgList => unreachable!(),
        }
    }
}

impl From<&HirType> for LoweredType {
    fn from(value: &HirType) -> Self {
        match value.kind() {
            raana_ir::ir::TypeKind::Unit => unreachable!(
                "should not encounter to allocate a unit type value. please filter it out before allocation"
            ),
            raana_ir::ir::TypeKind::Int32 => LoweredType::new_i32(),
            raana_ir::ir::TypeKind::Float32 => LoweredType::new_f32(),
            raana_ir::ir::TypeKind::String => {
                unreachable!("only used for a potential global value")
            }
            raana_ir::ir::TypeKind::Array(_, _) => {
                todo!("should not encounter to allocate a unit type value.")
            }
            // Pointer is treated as unsigned 64 bit integer.
            // "i64" doesn't indicate that it's signed integer.
            raana_ir::ir::TypeKind::Pointer(_) => LoweredType::new_i64(),
            raana_ir::ir::TypeKind::Function(..) => unreachable!(),
            raana_ir::ir::TypeKind::ArgList => unreachable!(),
        }
    }
}
