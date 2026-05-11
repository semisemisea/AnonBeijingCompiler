use num_enum::{IntoPrimitive, TryFromPrimitive};
use raana_ir::ir::Type;

#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Bit {
    b128,
    b64,
    b32,
    b16,
}

impl Bit {
    #[allow(unused)]
    pub fn width(&self) -> u8 {
        match self {
            Bit::b128 => 128,
            Bit::b64 => 64,
            Bit::b32 => 32,
            Bit::b16 => 16,
        }
    }
}

impl TryFrom<usize> for Bit {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            16 => Ok(Bit::b128),
            8 => Ok(Bit::b64),
            4 => Ok(Bit::b32),
            2 => Ok(Bit::b16),
            _ => Err(()),
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
    // FP, frame pointer
    x29,
    /// LR, link register, equivalent to `ra` in RISC-V.
    x30,
    /// zero
    xzr,
    /// stack pointer
    sp,
}

#[allow(non_camel_case_types)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, TryFromPrimitive, IntoPrimitive,
)]
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

    #[allow(unused)]
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

    pub fn temporary(id: usize, size: Bit) -> IReg {
        assert!(id < 7, "too many temporary registers");
        use IntRegister::*;
        let reg = [x9, x10, x11, x12, x13, x14, x15][id];
        IReg(size, reg)
    }
}

impl FReg {
    fn is_arg(&self) -> bool {
        use FloatRegister::*;
        matches!(self.1, v0 | v1 | v2 | v3 | v4 | v5 | v6 | v7)
    }

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterType {
    Int,
    Float,
}

impl TryInto<RegisterType> for Type {
    type Error = ();

    fn try_into(self) -> Result<RegisterType, Self::Error> {
        match self {
            t if t.is_f32() => Ok(RegisterType::Float),
            t if t.is_i32() => Ok(RegisterType::Int),
            _ => Err(()),
        }
    }
}

impl TryInto<RegisterType> for &Type {
    type Error = ();

    fn try_into(self) -> Result<RegisterType, ()> {
        match self {
            t if t.is_f32() => Ok(RegisterType::Float),
            _ => Ok(RegisterType::Int),
        }
    }
}

impl Register {
    #[allow(dead_code)]
    pub fn is_actual(&self) -> bool {
        match self {
            Register::I(reg) => reg.0 == Bit::b64,
            Register::F(reg) => reg.0 == Bit::b128,
        }
    }

    pub fn sz(&self) -> Bit {
        match self {
            Register::I(reg) => reg.0,
            Register::F(reg) => reg.0,
        }
    }

    pub fn ty(&self) -> RegisterType {
        match self {
            Register::I(_) => RegisterType::Int,
            Register::F(_) => RegisterType::Float,
        }
    }

    pub fn with_size(self, size: Bit) -> Register {
        match self {
            Register::I(IReg(_, reg)) => Register::I(IReg(size, reg)),
            Register::F(FReg(_, reg)) => Register::F(FReg(size, reg)),
        }
    }

    pub fn arguments(idx: usize) -> Register {
        assert!(idx < 8, "too many arguments");
        Register::I(IReg(Bit::b64, IntRegister::try_from(idx as u8).unwrap()))
    }

    pub fn float_arguments(idx: usize) -> Register {
        assert!(idx < 8, "too many arguments");
        use FloatRegister::*;
        let reg = [v0, v1, v2, v3, v4, v5, v6, v7][idx];
        Register::F(FReg(Bit::b32, reg))
    }

    pub fn temporary(id: usize, size: Bit) -> Register {
        assert!(id < 7, "too many temporary registers");
        Register::I(IReg::temporary(id, size))
    }

    pub fn is_arg(&self) -> bool {
        match self {
            Register::I(reg) => reg.is_arg(),
            Register::F(reg) => reg.is_arg(),
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

    #[allow(unused)]
    pub fn is_caller_saved(&self) -> bool {
        match self {
            Register::I(reg) => reg.is_caller_saved(),
            Register::F(reg) => reg.is_caller_saved(),
        }
    }
}

impl core::fmt::Display for Register {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Register::I(IReg(sz, reg)) => {
                if *reg == IntRegister::sp {
                    return write!(f, "{}sp", if *sz == Bit::b64 { "" } else { "w" });
                }
                let prefix = match sz {
                    Bit::b128 => unreachable!(),
                    Bit::b64 => "x",
                    Bit::b32 => "w",
                    Bit::b16 => unreachable!(),
                };
                write!(
                    f,
                    "{}{}",
                    prefix,
                    format!("{reg:?}").strip_prefix('x').unwrap()
                )
            }
            Register::F(FReg(sz, reg)) => {
                let prefix = match sz {
                    Bit::b128 => "v",
                    Bit::b64 => "d",
                    Bit::b32 => "s",
                    Bit::b16 => "h",
                };
                write!(
                    f,
                    "{}{}",
                    prefix,
                    format!("{reg:?}").strip_prefix('v').unwrap()
                )
            }
        }
    }
}
