//! AArch64 physical register and AAPCS64 allocation policy.
//!
//! 定义 AArch64 物理寄存器集合与分配策略：
//!
//! - 寄存器类：`RegClass::Int`（x0-x28，x29=FP/x30=LR 保留）、
//!   `RegClass::Float`（d0-d31）、`RegClass::Vector`（v0-v31，与 Float 共用
//!   同一物理寄存器文件，见 `vector_reg` 系列工厂函数）；
//! - [`Gpr`] 与 [`RegOrZr`] 的类型级区分：编码 31 在数据处理指令里是 ZR、
//!   在内存指令里是 SP，两个枚举分别约束这两种位置（SP 本身以普通 `Reg`
//!   流经全流程，见 [`stack_reg`]）；
//! - scratch 寄存器：分配器/RA 前后可自由使用的寄存器（`INT_ALLOCATOR_
//!   SCRATCH` 等常量），`MachineEnv` 里排除在可分配集合之外；
//! - 特殊寄存器：`FP`(29)/`LR`(30)/`SP_HW_ENC`(63)。

use std::sync::OnceLock;

use taki_mir::{
    reg_alloc::reg::{MachineEnv, PReg, PRegSet, RegClass},
    register::{Reg, Writable},
};

pub const FP: u8 = 29;
pub const LR: u8 = 30;

pub const INT_ALLOCATOR_SCRATCH: u8 = 16;
pub const FLOAT_ALLOCATOR_SCRATCH: u8 = 31;
pub const INT_POST_RA_SCRATCH: [u8; 4] = [14, 15, 16, 17];
pub const FLOAT_POST_RA_SCRATCH: [u8; 2] = [30, 31];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperandSize {
    Size32,
    Size64,
}

impl OperandSize {
    pub const fn bits(self) -> u8 {
        match self {
            Self::Size32 => 32,
            Self::Size64 => 64,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gpr {
    Reg(Reg),
    Zr,
}

/// A general-purpose data operand. Unlike [`Gpr`], this deliberately cannot
/// name SP: in data-processing encodings register 31 denotes ZR, not SP.
///
/// Note: since SP now flows through the regular `Reg` type (see
/// [`stack_reg`]), the only purpose of the `Gpr`/`RegOrZr` split is to
/// distinguish "encoding 31 means SP" positions from "encoding 31 means ZR"
/// positions at the type level. SP itself is always wrapped in `Gpr::Reg`
/// or passed as a plain `Reg`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegOrZr {
    Reg(Reg),
    Zr,
}

pub const fn int_preg(index: u8) -> PReg {
    assert!(index <= LR);
    PReg::new(index as usize, RegClass::Int)
}

pub const fn float_preg(index: u8) -> PReg {
    assert!(index <= 31);
    PReg::new(index as usize, RegClass::Float)
}

pub const fn vector_preg(index: u8) -> PReg {
    assert!(index <= 31);
    PReg::new(index as usize, RegClass::Vector)
}

pub const fn int_reg(index: u8) -> Reg {
    Reg::from_physical_reg(int_preg(index))
}

pub const fn float_reg(index: u8) -> Reg {
    Reg::from_physical_reg(float_preg(index))
}

pub const fn vector_reg(index: u8) -> Reg {
    Reg::from_physical_reg(vector_preg(index))
}

/// Internal PReg encoding for the stack pointer. Uses hw_enc 63 in the Int
/// class (mirroring cranelift's `PReg::new(31 + 32, RegClass::Int)`), which
/// `emit_reg` collapses to the hardware encoding 31 (`sp`) at assembly emit
/// time. Slot 63 was freed by relocating `PReg::INVALID` to `(Int, 62)`.
pub const SP_HW_ENC: u8 = 63;

pub const fn stack_preg() -> PReg {
    PReg::new(SP_HW_ENC as usize, RegClass::Int)
}

pub const fn stack_reg() -> Reg {
    Reg::from_physical_reg(stack_preg())
}

pub const fn writable_stack_reg() -> Writable<Reg> {
    Writable::from_reg(stack_reg())
}

pub const INT_ARG_REGS: [Reg; 8] = [
    int_reg(0),
    int_reg(1),
    int_reg(2),
    int_reg(3),
    int_reg(4),
    int_reg(5),
    int_reg(6),
    int_reg(7),
];

// AAPCS64 passes f32 arguments in s0-s7 — the low 32 bits of v0-v7. f32
// scalars are Vector-class vregs (sN ≡ vN) and share the SIMD/FP argument
// sequence with 128-bit vectors (a single NSRN), so argument slots for f32
// come from `VECTOR_ARG_REGS`; there is no separate float argument bank.
pub const VECTOR_ARG_REGS: [Reg; 8] = [
    vector_reg(0),
    vector_reg(1),
    vector_reg(2),
    vector_reg(3),
    vector_reg(4),
    vector_reg(5),
    vector_reg(6),
    vector_reg(7),
];

pub const INT_RETURN_REG: Reg = int_reg(0);
/// s0, the low 32 bits of v0 (f32 scalars are Vector-class vregs).
pub const FLOAT_RETURN_REG: Reg = vector_reg(0);
pub const VECTOR_RETURN_REG: Reg = vector_reg(0);

pub const DEFAULT_CLOBBERS: PRegSet = PRegSet::empty()
    .with(int_preg(0))
    .with(int_preg(1))
    .with(int_preg(2))
    .with(int_preg(3))
    .with(int_preg(4))
    .with(int_preg(5))
    .with(int_preg(6))
    .with(int_preg(7))
    .with(int_preg(8))
    .with(int_preg(9))
    .with(int_preg(10))
    .with(int_preg(11))
    .with(int_preg(12))
    .with(int_preg(13))
    .with(int_preg(14))
    .with(int_preg(15))
    .with(int_preg(16))
    .with(int_preg(17))
    .with(int_preg(18))
    .with(int_preg(LR))
    .with(float_preg(0))
    .with(float_preg(1))
    .with(float_preg(2))
    .with(float_preg(3))
    .with(float_preg(4))
    .with(float_preg(5))
    .with(float_preg(6))
    .with(float_preg(7))
    .with(float_preg(16))
    .with(float_preg(17))
    .with(float_preg(18))
    .with(float_preg(19))
    .with(float_preg(20))
    .with(float_preg(21))
    .with(float_preg(22))
    .with(float_preg(23))
    .with(float_preg(24))
    .with(float_preg(25))
    .with(float_preg(26))
    .with(float_preg(27))
    .with(float_preg(28))
    .with(float_preg(29))
    .with(float_preg(30))
    .with(float_preg(31))
    .with(vector_preg(0))
    .with(vector_preg(1))
    .with(vector_preg(2))
    .with(vector_preg(3))
    .with(vector_preg(4))
    .with(vector_preg(5))
    .with(vector_preg(6))
    .with(vector_preg(7))
    .with(vector_preg(16))
    .with(vector_preg(17))
    .with(vector_preg(18))
    .with(vector_preg(19))
    .with(vector_preg(20))
    .with(vector_preg(21))
    .with(vector_preg(22))
    .with(vector_preg(23))
    .with(vector_preg(24))
    .with(vector_preg(25))
    .with(vector_preg(26))
    .with(vector_preg(27))
    .with(vector_preg(28))
    .with(vector_preg(29))
    .with(vector_preg(30))
    .with(vector_preg(31));

pub const fn is_callee_saved(preg: PReg) -> bool {
    match preg.class() {
        RegClass::Int => preg.hw_enc() >= 19 && preg.hw_enc() <= 28,
        // AAPCS64: v8-v15 are callee-saved for both scalar FP (d8-d15) and
        // 128-bit NEON vector values (the vector view of the same registers).
        RegClass::Float | RegClass::Vector => preg.hw_enc() >= 8 && preg.hw_enc() <= 15,
    }
}

pub fn preg_name(preg: PReg) -> &'static str {
    match preg.class() {
        RegClass::Int => INT_REG_NAMES[preg.hw_enc()],
        // Both scalar FP and vector values name the same 128-bit NEON
        // register file; `preg_name` is the generic fallback used by
        // `ctx.write_reg` when a register is not rendered with a size prefix.
        RegClass::Float | RegClass::Vector => FLOAT_REG_NAMES[preg.hw_enc()],
    }
}

pub fn machine_env() -> &'static MachineEnv {
    static ENV: OnceLock<MachineEnv> = OnceLock::new();
    ENV.get_or_init(|| MachineEnv {
        preferred_regs_by_class: [
            preg_set(
                &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13],
                RegClass::Int,
            ),
            // RegClass::Float has no allocatable registers on AArch64: f32
            // scalars are Vector-class vregs (sN ≡ vN, so `sN` would alias a
            // live `vN` if the two classes allocated independently). The class
            // variant remains for the shared RA and the RISC-V backend.
            PRegSet::empty(),
            // Vector preferred: v0-v7 (ABI arg/return regs) plus v16-v31
            // (caller-saved upper NEON regs). Callee-saved v8-v15 are
            // non-preferred so a call-free function never touches them.
            preg_set(
                &[
                    0, 1, 2, 3, 4, 5, 6, 7, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29,
                    30, 31,
                ],
                RegClass::Vector,
            ),
        ],
        non_preferred_regs_by_class: [
            preg_set(&[19, 20, 21, 22, 23, 24, 25, 26, 27, 28], RegClass::Int),
            PRegSet::empty(),
            preg_set(&[8, 9, 10, 11, 12, 13, 14, 15], RegClass::Vector),
        ],
        scratch_by_class: [
            Some(int_preg(INT_ALLOCATOR_SCRATCH)),
            None,
            None,
        ],
        post_ra_scratch_by_class: [
            INT_POST_RA_SCRATCH.map(int_preg).to_vec(),
            vec![],
            vec![],
        ],
        fixed_stack_slots: vec![],
        // `sN` is the low 32 bits of `vN`: a vector write clobbers the
        // aliased float register, so the allocator must treat Float and
        // Vector as interfering on a shared hw_enc.
        aliased_banks: &[(RegClass::Float, RegClass::Vector)],
    })
}

fn preg_set(indices: &[u8], class: RegClass) -> PRegSet {
    indices.iter().fold(PRegSet::empty(), |set, &index| {
        set.with(PReg::new(index as usize, class))
    })
}

const INT_REG_NAMES: [&str; 31] = [
    "x0", "x1", "x2", "x3", "x4", "x5", "x6", "x7", "x8", "x9", "x10", "x11", "x12", "x13", "x14",
    "x15", "x16", "x17", "x18", "x19", "x20", "x21", "x22", "x23", "x24", "x25", "x26", "x27",
    "x28", "x29", "x30",
];

const FLOAT_REG_NAMES: [&str; 32] = [
    "v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7", "v8", "v9", "v10", "v11", "v12", "v13", "v14",
    "v15", "v16", "v17", "v18", "v19", "v20", "v21", "v22", "v23", "v24", "v25", "v26", "v27",
    "v28", "v29", "v30", "v31",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_environment_reserves_abi_and_post_ra_scratch_registers() {
        let env = machine_env();
        let allocatable = PRegSet::from(env);

        for preg in [
            int_preg(FP),
            int_preg(LR),
            int_preg(INT_ALLOCATOR_SCRATCH),
            int_preg(INT_POST_RA_SCRATCH[0]),
            int_preg(INT_POST_RA_SCRATCH[1]),
            int_preg(INT_POST_RA_SCRATCH[3]),
            float_preg(FLOAT_ALLOCATOR_SCRATCH),
            float_preg(FLOAT_POST_RA_SCRATCH[0]),
        ] {
            assert!(!allocatable.contains(preg), "{preg:?} must be reserved");
        }
        assert_eq!(
            env.scratch_by_class[0],
            Some(int_preg(INT_ALLOCATOR_SCRATCH))
        );
    }

    #[test]
    fn float_class_is_merged_into_vector_class() {
        // AArch64's `sN` registers alias the low 32 bits of `vN`, so f32
        // scalars must allocate from the Vector bank; the Float class must
        // hold no allocatable registers on this backend (it remains only for
        // the shared RA and the RISC-V backend).
        let env = machine_env();
        let allocatable = PRegSet::from(env);
        for index in 0..=31u8 {
            assert!(
                !allocatable.contains(float_preg(index)),
                "Float-class PReg {index} must not be allocatable on AArch64"
            );
        }
        assert!(env.preferred_regs_by_class[1].is_empty(RegClass::Float));
        assert!(env.non_preferred_regs_by_class[1].is_empty(RegClass::Float));
        assert_eq!(env.scratch_by_class[1], None);
        assert!(env.post_ra_scratch_by_class[1].is_empty());

        // The f32 ABI registers are the low 32 bits of the vector ABI
        // registers: the f32 return register is v0, and there is no separate
        // float argument bank (f32 args share the SIMD/FP sequence).
        assert_eq!(FLOAT_RETURN_REG, vector_reg(0));
    }
}
