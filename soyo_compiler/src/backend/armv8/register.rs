use num_enum::{IntoPrimitive, TryFromPrimitive};

#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Bit {
    b128,
    b64,
    b32,
    b16,
}

impl Bit {
    pub fn width(&self) -> u8 {
        match self {
            Bit::b128 => 128,
            Bit::b64 => 64,
            Bit::b32 => 32,
            Bit::b16 => 16,
        }
    }
}

#[allow(non_camel_case_types)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, TryFromPrimitive, IntoPrimitive,
)]
#[repr(u8)]
/// 64-bit integer registers.
pub enum IntRegister {
    x0,
    x1,
    x2,
    x3,
    x4,
    x5,
    x6,
    x7,
    x8,
    x9,
    x10,
    x11,
    x12,
    x13,
    x14,
    x15,
    x16,
    x17,
    x18,
    x19,
    x20,
    x21,
    x22,
    x23,
    x24,
    x25,
    x26,
    x27,
    x28,
    x29,
    /// LR, link register, equivalent to `ra` in RISC-V.
    x30,
    /// zero
    xzr,
    /// stack pointer
    sp,
}

#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
/// 128-bit floating-point registers.
pub enum FloatRegister {
    v0,
    v1,
    v2,
    v3,
    v4,
    v5,
    v6,
    v7,
    v8,
    v9,
    v10,
    v11,
    v12,
    v13,
    v14,
    v15,
    v16,
    v17,
    v18,
    v19,
    v20,
    v21,
    v22,
    v23,
    v24,
    v25,
    v26,
    v27,
    v28,
    v29,
    v30,
    v31,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IReg(pub Bit, pub IntRegister);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FReg(pub Bit, pub FloatRegister);

impl IReg {
    fn is_arg(&self) -> bool {
        use IntRegister::*;
        matches!(self.1, x0 | x1 | x2 | x3 | x4 | x5 | x6 | x7)
    }

    fn is_temp(&self) -> bool {
        use IntRegister::*;
        matches!(self.1, x9 | x10 | x11 | x12 | x13 | x14 | x15)
    }

    fn is_caller_saved(&self) -> bool {
        use IntRegister::*;
        matches!(
            self.1,
            x9 | x10 | x11 | x12 | x13 | x14 | x15 | x16 | x17 | x18
        )
    }

    fn is_callee_saved(&self) -> bool {
        use IntRegister::*;
        matches!(
            self.1,
            x19 | x20 | x21 | x22 | x23 | x24 | x25 | x26 | x27 | x28
        )
    }

    pub fn temporary(id: usize) -> IReg {
        assert!(id < 7, "too many temporary registers");
        use IntRegister::*;
        let reg = [x9, x10, x11, x12, x13, x14, x15][id];
        IReg(Bit::b64, reg)
    }
}

impl FReg {
    fn is_caller_saved(&self) -> bool {
        return !self.is_callee_saved();
    }

    fn is_callee_saved(&self) -> bool {
        use FloatRegister::*;
        matches!(self.1, v8 | v9 | v10 | v11 | v12 | v13 | v14 | v15)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Register {
    I(IReg),
    F(FReg),
}

impl Register {
    pub fn is_actual(&self) -> bool {
        match self {
            Register::I(reg) => reg.0 == Bit::b64,
            Register::F(reg) => reg.0 == Bit::b128,
        }
    }

    pub fn width(&self) -> u8 {
        match self {
            Register::I(reg) => reg.0.width(),
            Register::F(reg) => reg.0.width(),
        }
    }

    pub fn arguments(idx: usize) -> Register {
        assert!(idx < 8, "too many arguments");
        Register::I(IReg(Bit::b64, IntRegister::try_from(idx as u8).unwrap()))
    }

    pub fn temporary(id: usize) -> Register {
        assert!(id < 7, "too many temporary registers");
        Register::I(IReg::temporary(id))
    }

    pub fn is_arg(&self) -> bool {
        match self {
            Register::I(reg) => reg.is_arg(),
            Register::F(_) => false,
        }
    }

    /// Whether this register is callee-saved.
    /// Previously called `is_saved`.
    pub fn is_callee_saved(&self) -> bool {
        match self {
            Register::I(reg) => reg.is_callee_saved(),
            Register::F(reg) => reg.is_callee_saved(),
        }
    }

    pub fn is_caller_saved(&self) -> bool {
        match self {
            Register::I(reg) => reg.is_caller_saved(),
            Register::F(reg) => reg.is_caller_saved(),
        }
    }
}
