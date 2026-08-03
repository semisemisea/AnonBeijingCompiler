//! AArch64 physical register and AAPCS64 allocation policy.

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

pub const fn int_reg(index: u8) -> Reg {
    Reg::from_physical_reg(int_preg(index))
}

pub const fn float_reg(index: u8) -> Reg {
    Reg::from_physical_reg(float_preg(index))
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

pub const FLOAT_ARG_REGS: [Reg; 8] = [
    float_reg(0),
    float_reg(1),
    float_reg(2),
    float_reg(3),
    float_reg(4),
    float_reg(5),
    float_reg(6),
    float_reg(7),
];

pub const INT_RETURN_REG: Reg = int_reg(0);
pub const FLOAT_RETURN_REG: Reg = float_reg(0);

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
    .with(float_preg(31));

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
            preg_set(
                &[
                    0, 1, 2, 3, 4, 5, 6, 7, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29,
                ],
                RegClass::Float,
            ),
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
            preg_set(&[8, 9, 10, 11, 12, 13, 14, 15], RegClass::Float),
            preg_set(&[8, 9, 10, 11, 12, 13, 14, 15], RegClass::Vector),
        ],
        scratch_by_class: [
            Some(int_preg(INT_ALLOCATOR_SCRATCH)),
            Some(float_preg(FLOAT_ALLOCATOR_SCRATCH)),
            None,
        ],
        post_ra_scratch_by_class: [
            INT_POST_RA_SCRATCH.map(int_preg).to_vec(),
            FLOAT_POST_RA_SCRATCH.map(float_preg).to_vec(),
            vec![],
        ],
        fixed_stack_slots: vec![],
    })
}

fn preg_set(indices: &[u8], class: RegClass) -> PRegSet {
    indices.iter().fold(PRegSet::empty(), |set, &index| {
        set.with(PReg::new(index as usize, class))
    })
}

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
        assert_eq!(
            env.scratch_by_class[1],
            Some(float_preg(FLOAT_ALLOCATOR_SCRATCH))
        );
    }
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
