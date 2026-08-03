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
//! 0b 0000 0000 0000 0001 -> integer scalar
//! 0b 0000 0000 0000 0010 -> vector (SIMD) marker
//! 0b 0000 0000 0000 0100 -> (free)
//! 0b 0000 0000 0001 0000 -> float scalar
//! 0b 0000 0000 0000 0000 -> (bitwidth): 32-bit set in B32, 64-bit in B64
//! 0b 0000 0000 0000 00xx -> lanes of vector (bits 5-7), 0 for scalars
//! 0b 1111 1111 1111 1111 -> invalid type
//!
//! The lane-count field sits in bits 5-7 (up to 7 lanes; V2/V4/V8 for now).
//! Bit 4 is the float marker, so the lane field deliberately avoids it.
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
const VECTOR: LoweredType = LoweredType(0x0002);

const B32: LoweredType = LoweredType(0x0100);
const B64: LoweredType = LoweredType(0x1000);

/// Lane-count field at bits 5-7: `N` lanes of the scalar element encode as
/// `N << 5`. Kept clear of the float marker (bit 4) and the width bits.
const LANE2: LoweredType = LoweredType(0x0040);
const LANE4: LoweredType = LoweredType(0x0080);

pub const I32: LoweredType = combine(INT, B32);
pub const I64: LoweredType = combine(INT, B64);
pub const F32: LoweredType = combine(FLOAT, B32);

/// 128-bit vector types (NEON `V` registers). The `VECTOR` marker bit
/// distinguishes them from scalars; element type and lane count are
/// recoverable from the bit layout.
pub const V4I32: LoweredType = combine(combine(combine(INT, B32), VECTOR), LANE4);
pub const V2I64: LoweredType = combine(combine(combine(INT, B64), VECTOR), LANE2);
pub const V4F32: LoweredType = combine(combine(combine(FLOAT, B32), VECTOR), LANE4);
pub const V2F64: LoweredType = combine(combine(combine(FLOAT, B64), VECTOR), LANE2);

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

    /// Is this a vector (SIMD) value rather than a scalar?
    pub fn is_vector(self) -> bool {
        (self.0 & VECTOR.0) != 0
    }

    /// Number of vector lanes. Returns 0 for scalars.
    pub fn lanes(self) -> u32 {
        (self.0 as u32 >> 5) & 0x7
    }

    /// Storage size in bytes. All vector types are 128-bit.
    pub fn size(self) -> u32 {
        if self.is_vector() {
            16
        } else if self == I64 {
            8
        } else {
            4
        }
    }
}

impl LoweredType {
    /// Map a HIR vector type onto the supported 128-bit machine vector types.
    /// Only combinations whose total width is 128 bits are representable.
    fn from_vector_type(elem: &HirType, lanes: usize) -> LoweredType {
        use raana_ir::ir::TypeKind;
        match (elem.kind(), lanes) {
            (TypeKind::Int32, 4) => V4I32,
            (TypeKind::Float32, 4) => V4F32,
            (TypeKind::Pointer(_), 2) => V2I64,
            _ => {
                panic!("unsupported machine vector type: <{lanes} x {elem}> (only 128-bit vectors)")
            }
        }
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
            raana_ir::ir::TypeKind::Vector(elem, lanes) => {
                LoweredType::from_vector_type(elem, *lanes)
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
            raana_ir::ir::TypeKind::Vector(elem, lanes) => {
                LoweredType::from_vector_type(elem, *lanes)
            }
            // Pointer is treated as unsigned 64 bit integer.
            // "i64" doesn't indicate that it's signed integer.
            raana_ir::ir::TypeKind::Pointer(_) => LoweredType::new_i64(),
            raana_ir::ir::TypeKind::Function(..) => unreachable!(),
            raana_ir::ir::TypeKind::ArgList => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_type_encoding_is_disjoint_from_scalars() {
        // Regression guard: the vector marker bit must not alias any scalar
        // type (F32 in particular shares a byte with the earlier lane field).
        for scalar in [
            I32,
            I64,
            F32,
            LoweredType::new_i32(),
            LoweredType::new_i64(),
            LoweredType::new_f32(),
        ] {
            assert!(!scalar.is_vector(), "{scalar:?} must not be a vector");
            assert_eq!(scalar.lanes(), 0);
        }
    }

    #[test]
    fn vector_types_carry_lane_count_and_128_bit_size() {
        assert_eq!(V4I32.lanes(), 4);
        assert_eq!(V2I64.lanes(), 2);
        assert_eq!(V4F32.lanes(), 4);
        assert_eq!(V2F64.lanes(), 2);
        for ty in [V4I32, V2I64, V4F32, V2F64] {
            assert!(ty.is_vector());
            assert_eq!(ty.size(), 16);
        }
        assert_eq!(I32.size(), 4);
        assert_eq!(I64.size(), 8);
        assert_eq!(F32.size(), 4);
    }
}
