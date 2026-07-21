use std::sync::OnceLock;

use taki_mir::{
    reg_alloc::reg::{MachineEnv, PReg, PRegSet, RegClass},
    register::Reg,
    types::Type,
};

pub const FP: u8 = 29;
pub const LR: u8 = 30;
pub const INT_SCRATCH0: u8 = 16;
pub const INT_SCRATCH1: u8 = 17;
pub const FP_SCRATCH: u8 = 31;

pub const fn int_preg(index: u8) -> PReg {
    assert!(index <= 30);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecialGpr {
    Sp,
    Zr,
}

pub fn format_reg(reg: Reg, ty: Type) -> String {
    let preg = reg
        .to_physical_reg()
        .expect("assembly emission requires a physical register");
    match preg.class() {
        RegClass::Int => match ty {
            ty if ty.is_i32() => format!("w{}", preg.hw_enc()),
            _ => format!("x{}", preg.hw_enc()),
        },
        RegClass::Float => format!("s{}", preg.hw_enc()),
        RegClass::Vector => panic!("vector registers are not supported by the AArch64 backend"),
    }
}

pub fn format_special(reg: SpecialGpr, ty: Type) -> &'static str {
    match (reg, ty.is_i32()) {
        (SpecialGpr::Sp, _) => "sp",
        (SpecialGpr::Zr, true) => "wzr",
        (SpecialGpr::Zr, false) => "xzr",
    }
}

pub fn machine_env() -> &'static MachineEnv {
    static ENV: OnceLock<MachineEnv> = OnceLock::new();
    ENV.get_or_init(|| {
        let mut preferred_int = PRegSet::empty();
        for index in 0..=15 {
            preferred_int.add(int_preg(index));
        }
        let mut non_preferred_int = PRegSet::empty();
        for index in 19..=28 {
            non_preferred_int.add(int_preg(index));
        }
        let mut preferred_float = PRegSet::empty();
        for index in 0..=7 {
            preferred_float.add(float_preg(index));
        }
        for index in 16..=30 {
            preferred_float.add(float_preg(index));
        }
        MachineEnv {
            preferred_regs_by_class: [preferred_int, preferred_float, PRegSet::empty()],
            non_preferred_regs_by_class: [non_preferred_int, PRegSet::empty(), PRegSet::empty()],
            scratch_by_class: [
                Some(int_preg(INT_SCRATCH0)),
                Some(float_preg(FP_SCRATCH)),
                None,
            ],
            fixed_stack_slots: Vec::new(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_register_views_and_special_registers() {
        assert_eq!(format_reg(int_reg(3), Type::new_i32()), "w3");
        assert_eq!(format_reg(int_reg(3), Type::new_i64()), "x3");
        assert_eq!(format_reg(float_reg(5), Type::new_f32()), "s5");
        assert_eq!(format_special(SpecialGpr::Sp, Type::new_i64()), "sp");
        assert_eq!(format_special(SpecialGpr::Zr, Type::new_i32()), "wzr");
    }

    #[test]
    fn reserves_abi_and_scratch_registers() {
        let env = machine_env();
        let allocatable = PRegSet::from(env);
        assert!(!allocatable.contains(int_preg(INT_SCRATCH0)));
        assert!(!allocatable.contains(int_preg(FP)));
        assert!(!allocatable.contains(int_preg(LR)));
        assert!(!allocatable.contains(float_preg(FP_SCRATCH)));
        assert!(allocatable.contains(int_preg(0)));
        assert!(allocatable.contains(int_preg(19)));
    }
}
