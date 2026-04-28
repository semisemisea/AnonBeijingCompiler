use std::fmt;

use crate::backend::armv8::register::Register;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddSubImm {
    Imm12(u16),
    Imm12Lsl12(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MoveWideImmShift {
    B0 = 0,
    B16 = 16,
    B32 = 32,
    B48 = 48,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveWideImm {
    Imm16 { value: u16, shift: MoveWideImmShift },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShiftImm {
    Imm6(u8),
}

#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extend {
    /// Unsigned extend byte
    UXTB,
    /// Unsigned extend halfword
    UXTH,
    /// Unsigned extend word
    UXTW,
    /// Unsigned extend doubleword
    UXTX,
    /// Signed extend byte
    SXTB,
    /// Signed extend halfword
    SXTH,
    /// Signed extend word
    SXTW,
    /// Signed extend doubleword
    SXTX,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtendedRegister {
    pub reg: Register,
    pub extend: Extend,
    pub shift: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddSubOperand {
    Register(Register),
    Immediate(AddSubImm),
    ExtendedRegister(ExtendedRegister),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicOperand {
    Register(Register),
    BitmaskImmediate(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovOperand {
    Register(Register),
    Immediate(MoveWideImm),
}

#[allow(unused)]
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inst {
    mov {
        rd: Register,
        src: MovOperand,
    },
    movz {
        rd: Register,
        imm: MoveWideImm,
    },
    movn {
        rd: Register,
        imm: MoveWideImm,
    },
    movk {
        rd: Register,
        imm: MoveWideImm,
    },
    add {
        rd: Register,
        rs1: Register,
        rs2: AddSubOperand,
    },
    sub {
        rd: Register,
        rs1: Register,
        rs2: AddSubOperand,
    },
    mul {
        rd: Register,
        rs1: Register,
        rs2: Register,
    },
    sdiv {
        rd: Register,
        rs1: Register,
        rs2: Register,
    },
    udiv {
        rd: Register,
        rs1: Register,
        rs2: Register,
    },
    neg {
        rd: Register,
        src: Register,
    },
    cmp {
        rs1: Register,
        rs2: AddSubOperand,
    },
    and {
        rd: Register,
        rs1: Register,
        rs2: LogicOperand,
    },
    orr {
        rd: Register,
        rs1: Register,
        rs2: LogicOperand,
    },
    eor {
        rd: Register,
        rs1: Register,
        rs2: LogicOperand,
    },
    lsl {
        rd: Register,
        rs1: Register,
        rs2: ShiftImm,
    },
    lsr {
        rd: Register,
        rs1: Register,
        rs2: ShiftImm,
    },
    asr {
        rd: Register,
        rs1: Register,
        rs2: ShiftImm,
    },
    ret,
    _string {
        indent_level: usize,
        str: String,
    },
}

impl Inst {
    fn is_real_inst(&self) -> bool {
        !matches!(self, Inst::_string { .. })
    }
}

impl fmt::Display for AddSubImm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AddSubImm::Imm12(value) => {
                debug_assert!(*value <= 4095);
                write!(f, "#{value}")
            }
            AddSubImm::Imm12Lsl12(value) => {
                debug_assert!(*value <= 4095);
                write!(f, "#{value}, lsl #12")
            }
        }
    }
}

impl fmt::Display for MoveWideImmShift {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MoveWideImmShift::B0 => write!(f, "0"),
            MoveWideImmShift::B16 => write!(f, "16"),
            MoveWideImmShift::B32 => write!(f, "32"),
            MoveWideImmShift::B48 => write!(f, "48"),
        }
    }
}

impl fmt::Display for MoveWideImm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MoveWideImm::Imm16 { value, shift } => {
                write!(f, "#{value}")?;
                if *shift != MoveWideImmShift::B0 {
                    write!(f, ", lsl {shift}")?;
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for ShiftImm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShiftImm::Imm6(value) => {
                debug_assert!(*value <= 63);
                write!(f, "#{value}")
            }
        }
    }
}

impl fmt::Display for Extend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Extend::UXTB => write!(f, "uxtb"),
            Extend::UXTH => write!(f, "uxth"),
            Extend::UXTW => write!(f, "uxtw"),
            Extend::UXTX => write!(f, "uxtx"),
            Extend::SXTB => write!(f, "sxtb"),
            Extend::SXTH => write!(f, "sxth"),
            Extend::SXTW => write!(f, "sxtw"),
            Extend::SXTX => write!(f, "sxtx"),
        }
    }
}

impl fmt::Display for ExtendedRegister {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        debug_assert!(self.shift <= 4);
        write!(f, "{:?}, {}", self.reg, self.extend)?;
        if self.shift != 0 {
            write!(f, " #{}", self.shift)?;
        }
        Ok(())
    }
}

impl fmt::Display for AddSubOperand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AddSubOperand::Register(reg) => write!(f, "{reg:?}"),
            AddSubOperand::Immediate(imm) => write!(f, "{imm}"),
            AddSubOperand::ExtendedRegister(reg) => write!(f, "{reg}"),
        }
    }
}

impl fmt::Display for LogicOperand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogicOperand::Register(reg) => write!(f, "{reg:?}"),
            LogicOperand::BitmaskImmediate(value) => write!(f, "#{value}"),
        }
    }
}

impl fmt::Display for MovOperand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MovOperand::Register(reg) => write!(f, "{reg:?}"),
            MovOperand::Immediate(imm) => write!(f, "{imm}"),
        }
    }
}

impl fmt::Display for Inst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Inst::mov { rd, src } => write!(f, "mov {rd:?}, {src}"),
            Inst::movz { rd, imm } => write!(f, "movz {rd:?}, {imm}"),
            Inst::movn { rd, imm } => write!(f, "movn {rd:?}, {imm}"),
            Inst::movk { rd, imm } => write!(f, "movk {rd:?}, {imm}"),
            Inst::add { rd, rs1, rs2 } => write!(f, "add {rd:?}, {rs1:?}, {rs2}"),
            Inst::sub { rd, rs1, rs2 } => write!(f, "sub {rd:?}, {rs1:?}, {rs2}"),
            Inst::mul { rd, rs1, rs2 } => write!(f, "mul {rd:?}, {rs1:?}, {rs2:?}"),
            Inst::sdiv { rd, rs1, rs2 } => write!(f, "sdiv {rd:?}, {rs1:?}, {rs2:?}"),
            Inst::udiv { rd, rs1, rs2 } => write!(f, "udiv {rd:?}, {rs1:?}, {rs2:?}"),
            Inst::neg { rd, src } => write!(f, "neg {rd:?}, {src:?}"),
            Inst::cmp { rs1, rs2 } => write!(f, "cmp {rs1:?}, {rs2}"),
            Inst::and { rd, rs1, rs2 } => write!(f, "and {rd:?}, {rs1:?}, {rs2}"),
            Inst::orr { rd, rs1, rs2 } => write!(f, "orr {rd:?}, {rs1:?}, {rs2}"),
            Inst::eor { rd, rs1, rs2 } => write!(f, "eor {rd:?}, {rs1:?}, {rs2}"),
            Inst::lsl { rd, rs1, rs2 } => write!(f, "lsl {rd:?}, {rs1:?}, {rs2}"),
            Inst::lsr { rd, rs1, rs2 } => write!(f, "lsr {rd:?}, {rs1:?}, {rs2}"),
            Inst::asr { rd, rs1, rs2 } => write!(f, "asr {rd:?}, {rs1:?}, {rs2}"),
            Inst::ret => write!(f, "ret"),
            Inst::_string { indent_level, str } => write!(f, "{}{str}", "\t".repeat(*indent_level)),
        }
    }
}
