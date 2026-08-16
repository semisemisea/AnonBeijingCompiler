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
//!
//! ---
//!
//! ## 补充说明（中文）
//!
//! ### 一句话定位
//!
//! 本模块定义 taki_mir 的**机器级类型系统**：`LoweredType`。定位链：SysY 源码
//! → RaanaIR（平台无关 SSA，类型带语义与约束）→ taki_mir（机器级，类型只剩
//! 位宽与类别）→ 汇编。与 HLIR 类型的关系：RaanaIR 的 `Type`（本 crate 别名
//! `HirType`）带有 unit / string / 函数 / 数组等语义约束，会限制机器级的使用；
//! `LoweredType` 把语义类型**拍平**成机器可用的形态——翻译上文英文清单即：
//! 32 位整数 `I32`、32 位浮点 `F32`、64 位地址/整数 `I64`（指针一律视为无符号
//! 64 位整数，`i64` 不代表有符号）、128 位 SIMD 向量（`V4I32` / `V2I64` /
//! `V4F32` / `V2F64`）。所有整数类型都是**符号无关（sign-agnostic）**的。
//!
//! ### 类型清单
//!
//! 唯一的公开类型是 `LoweredType(u16)`，一个 16 位**位打包**编码；公开常量
//! 与构造/查询接口：
//!
//! | 项 | 说明 |
//! |----|------|
//! | `I32` / `I64` / `F32` | 标量：32 位整数 / 64 位整数 / 32 位浮点 |
//! | `V4I32` / `V2I64` / `V4F32` / `V2F64` | 128 位 SIMD 向量（NEON `V` 寄存器） |
//! | `LoweredType::new_i32` / `new_i64` / `new_f32` | 标量构造器 |
//! | `LoweredType::invalid` | 无效哨兵（编码 `0xFFFF`） |
//! | `is_vector` / `lanes` / `size` | 是否向量 / 通道数（标量为 0）/ 存储字节数 |
//!
//! 位布局（私有常量，编码参考）：bit 0 `INT`、bit 1 `VECTOR`、bit 4 `FLOAT`
//! 三类类别 marker；bit 8 `B32`、bit 12 `B64` 表示位宽；bits 5-7 存向量通道数
//! （`N << 5`，即 `LANE2` / `LANE4`），通道字段刻意避开 bit 4 的 float marker。
//!
//! 寄存器类对应物是 `reg_alloc::reg::RegClass`（`Int` / `Float` / `Vector`），
//! 由后端 `MachInst::rc_for_type` 依据 `LoweredType` 选出（见"正确性"一节）。
//!
//! ### 谁在使用
//!
//! - **vcode**：指令的类型→寄存器类映射与操作数类型标注（`vcode.rs`）；
//! - **寄存器分配**：`register.rs` 的 `VRegAllocator` 用 `vreg_types:
//!   Vec<LoweredType>` 记录每个虚拟寄存器的类型；
//! - **ABI**：`abi.rs` 的 `ArgSlot::Stack { ty }` 用 `LoweredType` 描述栈槽
//!   宽度；
//! - **后端**：`anon_armv8` 与 `uika_riscv` 的 lower 把 `LoweredType` 转成访存
//!   操作数（`LoadOP` / `StoreOP`）以决定访存宽度，并各自实现 `rc_for_type`
//!   选择寄存器类。
//!
//! ### 与 RaanaIR 类型的映射
//!
//! 通过 `From<HirType>` / `From<&HirType>` 实现（`HirType` 即
//! `raana_ir::ir::Type`），按 `TypeKind` 匹配：
//!
//! - `Int32` → `I32`；`Float32` → `F32`；
//! - `Pointer(_)` → `I64`：指针视为无符号 64 位整数；
//! - `Vector(elem, lanes)` → 只支持总宽 128 位的组合：`Int32 × 4` → `V4I32`、
//!   `Float32 × 4` → `V4F32`、`Pointer × 2` → `V2I64`，其余组合直接 `panic!`
//!   （"unsupported machine vector type"）；
//! - `Unit` / `String` / `Function` / `ArgList` → `unreachable!`：分配前应
//!   过滤掉这些语义类型；`Array` → `todo!`（尚未支持）。
//!
//! ### 正确性：类型在机器级的意义
//!
//! 机器级类型不承诺语义、只承诺**位宽与类别**，这正是"符号无关"的根源：加减乘
//! 与移位按位定义，同一位模式交由指令语义解释。类型一旦选错，就会生成非法代码：
//!
//! - **寄存器类选择**：`rc_for_type` 决定值是进整数寄存器还是浮点/向量寄存器
//!   （`I32` / `I64` → `RegClass::Int`，`F32` → `RegClass::Float`，四个向量类型
//!   → `RegClass::Vector`），选错会产生非法指令形式（例如在浮点寄存器上跑整数
//!   指令）；
//! - **访存宽度**：`LoweredType` → `LoadOP` / `StoreOP` 的转换保证 load/store
//!   宽度与类型位宽一致；`size()` 给出存储大小（向量恒 16 字节、`I64` 8 字节、
//!   其余 4 字节）；
//! - **编码不相交**：标量 marker 与向量 marker、float marker 与通道字段互不
//!   重叠，保证 `is_vector` / `lanes` / `size` 的判定自洽；`invalid`（`0xFFFF`）
//!   作为未初始化/错误哨兵。
//!
//! ### 验证
//!
//! 本文件 `mod tests` 有两个回归测试：`vector_type_encoding_is_disjoint_from_
//! scalars`（标量与向量编码不相交、标量 `lanes() == 0`）、
//! `vector_types_carry_lane_count_and_128_bit_size`（四个向量类型的通道数与
//! 16 字节大小）。全量验证跑 `cargo test -p taki_mir`；后端侧，`rc_for_type`
//! 对未覆盖类型走 `unreachable!` 兜底，映射缺口会在编译/测试期暴露。
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
