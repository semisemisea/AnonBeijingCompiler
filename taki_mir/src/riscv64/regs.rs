//! Riscv64 ISA definitions: registers.
//!

use crate::{
    reg_alloc::reg::{PReg, RegClass, VReg},
    register::{Reg, Writable},
};

// first argument of function call
#[inline]
pub const fn a0() -> Reg {
    x_reg(10)
}

// second argument of function call
#[inline]
pub const fn a1() -> Reg {
    x_reg(11)
}

// third argument of function call
#[inline]
pub fn a2() -> Reg {
    x_reg(12)
}

#[inline]
pub fn writable_a0() -> Writable<Reg> {
    Writable::from_reg(a0())
}
#[inline]
#[cfg(test)]
pub fn writable_a1() -> Writable<Reg> {
    Writable::from_reg(a1())
}
#[inline]
#[expect(dead_code, reason = "here if needed in the future")]
pub fn writable_a2() -> Writable<Reg> {
    Writable::from_reg(a2())
}

#[inline]
pub fn fa0() -> Reg {
    f_reg(10)
}
#[inline]
pub fn writable_fa0() -> Writable<Reg> {
    Writable::from_reg(fa0())
}
#[inline]
#[expect(dead_code, reason = "here if needed in the future")]
pub fn writable_fa1() -> Writable<Reg> {
    Writable::from_reg(fa1())
}
#[inline]
pub fn fa1() -> Reg {
    f_reg(11)
}

/// Get a reference to the zero-register.
#[inline]
pub fn zero_reg() -> Reg {
    x_reg(0)
}

/// Get a writable reference to the zero-register (this discards a result).
#[inline]
pub fn writable_zero_reg() -> Writable<Reg> {
    Writable::from_reg(zero_reg())
}
#[inline]
pub fn stack_reg() -> Reg {
    x_reg(2)
}

/// Get a writable reference to the stack-pointer register.
#[inline]
pub fn writable_stack_reg() -> Writable<Reg> {
    Writable::from_reg(stack_reg())
}

/// Get a reference to the link register (x1).
pub fn link_reg() -> Reg {
    x_reg(1)
}

/// Get a writable reference to the link register.
#[inline]
pub fn writable_link_reg() -> Writable<Reg> {
    Writable::from_reg(link_reg())
}

/// Get a reference to the frame pointer (x8).
#[inline]
pub fn fp_reg() -> Reg {
    x_reg(8)
}

/// Get a writable reference to the frame pointer.
#[inline]
pub fn writable_fp_reg() -> Writable<Reg> {
    Writable::from_reg(fp_reg())
}

/// Get a reference to the first temporary, sometimes "spill temporary",
/// register. This register is used in various ways as a temporary.
#[inline]
pub fn spilltmp_reg() -> Reg {
    x_reg(31)
}

/// Get a writable reference to the spilltmp reg.
#[inline]
pub fn writable_spilltmp_reg() -> Writable<Reg> {
    Writable::from_reg(spilltmp_reg())
}

///spilltmp2
#[inline]
pub fn spilltmp_reg2() -> Reg {
    x_reg(30)
}

/// Get a writable reference to the spilltmp2 reg.
#[inline]
pub fn writable_spilltmp_reg2() -> Writable<Reg> {
    Writable::from_reg(spilltmp_reg2())
}

#[inline]
pub const fn x_reg(enc: usize) -> Reg {
    let p_reg = PReg::new(enc, RegClass::Int);
    let v_reg = VReg::new(p_reg.index(), p_reg.class());
    Reg::from_virtual_reg(v_reg)
}

pub const fn px_reg(enc: usize) -> PReg {
    PReg::new(enc, RegClass::Int)
}

#[inline]
pub const fn f_reg(enc: usize) -> Reg {
    let p_reg = PReg::new(enc, RegClass::Float);
    let v_reg = VReg::new(p_reg.index(), p_reg.class());
    Reg::from_virtual_reg(v_reg)
}
pub const fn pf_reg(enc: usize) -> PReg {
    PReg::new(enc, RegClass::Float)
}

pub const fn pv_reg(enc: usize) -> PReg {
    PReg::new(enc, RegClass::Vector)
}

pub const ARG_REG: [Reg; 8] = [
    x_reg(10),
    x_reg(11),
    x_reg(12),
    x_reg(13),
    x_reg(14),
    x_reg(15),
    x_reg(16),
    x_reg(17),
];

pub const FARG_REG: [Reg; 8] = [
    f_reg(10),
    f_reg(11),
    f_reg(12),
    f_reg(13),
    f_reg(14),
    f_reg(15),
    f_reg(16),
    f_reg(17),
];

pub fn preg_name(preg: PReg) -> &'static str {
    match preg.class() {
        RegClass::Int => INT_REG_NAMES[preg.hw_enc()],
        RegClass::Float => FLOAT_REG_NAMES[preg.hw_enc()],
        _ => unreachable!(),
    }
}

const INT_REG_NAMES: [&str; 32] = [
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2",
    "s0", "s1", "a0", "a1", "a2", "a3", "a4", "a5",
    "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7",
    "s8", "s9", "s10", "s11", "t3", "t4", "t5", "t6",
];

const FLOAT_REG_NAMES: [&str; 32] = [
    "ft0", "ft1", "ft2", "ft3", "ft4", "ft5", "ft6", "ft7",
    "fs0", "fs1", "fa0", "fa1", "fa2", "fa3", "fa4", "fa5",
    "fa6", "fa7", "fs2", "fs3", "fs4", "fs5", "fs6", "fs7",
    "fs8", "fs9", "fs10", "fs11", "ft8", "ft9", "ft10", "ft11",
];
