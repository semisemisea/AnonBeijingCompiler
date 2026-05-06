use std::fmt;

use crate::backend::armv8::register::Register;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddSubImm {
    Imm12(i16),
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
pub enum ShiftSize {
    Imm6(u8),
    Register(Register),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddSubOperand {
    Register(Register),
    Immediate(AddSubImm),
    ExtendedRegister(ExtendedRegister),
    AddrLo12(String),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadSaveOffset {
    Imm12(i16),
    Register(Register),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsetCondition {
    EQ, // equal
    NE, // not equal
    CS, // carry set (unsigned higher or same)
    CC, // carry clear (unsigned lower)
    MI, // minus/negative
    PL, // plus/positive or zero
    VS, // overflow
    VC, // no overflow
    HI, // unsigned higher
    LS, // unsigned lower or same
    GE, // signed greater than or equal
    LT, // signed less than
    GT, // signed greater than
    LE, // signed less than or equal
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
        rs2: ShiftSize,
    },
    lsr {
        rd: Register,
        rs1: Register,
        rs2: ShiftSize,
    },
    asr {
        rd: Register,
        rs1: Register,
        rs2: ShiftSize,
    },
    ret,
    // memory ops
    /// Load word: `ldr rd, [rs, offset]`
    ldr {
        rd: Register,
        rs: Register,
        offset: LoadSaveOffset,
    },
    /// Save word: `sdr rs, [rd, offset]`
    sdr {
        rs: Register,
        rd: Register,
        offset: LoadSaveOffset,
    },
    /// Get far address: `adrp rd, label`
    /// To get a address of a label not reachable by `adr`, we can use `adrp` to get the page address, then use `ldr` with an offset to get the exact address.
    /// ```arm
    /// adrp x0, label
    /// add x0, x0, :lo12:label
    /// ldr x0, x0
    /// ```
    adrp {
        rd: Register,
        label: String,
    },
    cset {
        rd: Register,
        condition: CsetCondition,
    },
    b {
        label: String,
    },
    bl {
        label: String,
    },
    cbnz {
        rs: Register,
        label: String,
    },
    cbz {
        rs: Register,
        label: String,
    },
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

impl fmt::Display for ShiftSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShiftSize::Imm6(value) => {
                debug_assert!(*value <= 63);
                write!(f, "#{value}")
            }
            ShiftSize::Register(reg) => write!(f, "{reg:?}"),
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
            AddSubOperand::AddrLo12(label) => write!(f, ":lo12:{label}"),
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

impl fmt::Display for LoadSaveOffset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadSaveOffset::Imm12(value) => {
                debug_assert!(*value <= 4095);
                write!(f, "#{value}")
            }
            LoadSaveOffset::Register(reg) => write!(f, "{reg:?}"),
        }
    }
}

impl fmt::Display for CsetCondition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CsetCondition::EQ => write!(f, "eq"),
            CsetCondition::NE => write!(f, "ne"),
            CsetCondition::CS => write!(f, "cs"),
            CsetCondition::CC => write!(f, "cc"),
            CsetCondition::MI => write!(f, "mi"),
            CsetCondition::PL => write!(f, "pl"),
            CsetCondition::VS => write!(f, "vs"),
            CsetCondition::VC => write!(f, "vc"),
            CsetCondition::HI => write!(f, "hi"),
            CsetCondition::LS => write!(f, "ls"),
            CsetCondition::GE => write!(f, "ge"),
            CsetCondition::LT => write!(f, "lt"),
            CsetCondition::GT => write!(f, "gt"),
            CsetCondition::LE => write!(f, "le"),
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
            Inst::ldr { rd, rs, offset } => write!(f, "ldr {rd:?}, [{rs:?}, {offset}]"),
            Inst::sdr { rs, rd, offset } => write!(f, "sdr {rs:?}, {rd:?}, {offset}"),
            Inst::adrp { rd, label } => write!(f, "adrp {rd:?}, {label}"),
            Inst::cset { rd, condition } => write!(f, "cset {rd:?}, {condition}"),
            Inst::b { label } => write!(f, "b {label}"),
            Inst::bl { label } => write!(f, "bl {label}"),
            Inst::cbnz { rs, label } => write!(f, "cbnz {rs:?}, {label}"),
            Inst::cbz { rs, label } => write!(f, "cbz {rs:?}, {label}"),
            Inst::ret => write!(f, "ret"),
            Inst::_string { indent_level, str } => write!(f, "{}{str}", "\t".repeat(*indent_level)),
        }
    }
}
