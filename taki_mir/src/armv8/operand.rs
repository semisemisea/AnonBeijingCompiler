use crate::prelude::*;
use std::{fmt, num::NonZeroU32};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Register {
    id: u32,
    kind: RegisterKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegisterKind {
    I32,
    I64,
    F32,
}

impl fmt::Display for Register {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VReg({}, {:?})", self.id, self.kind)
    }
}

impl Register {
    pub fn new_virtual(id: u32, kind: RegisterKind) -> Register {
        assert!(id > 64);
        Register { id, kind }
    }

    pub fn new_physics(id: u32, kind: RegisterKind) -> Register {
        assert!(id <= 64);
        Register { id, kind }
    }

    pub fn is_virtual(&self) -> bool {
        self.id > 64
    }

    pub fn kind(&self) -> RegisterKind {
        self.kind
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Value {
    Register(Register, Size),
    I32(i32),
    F32(f32),
}

#[derive(Debug, Clone, Copy)]
pub enum Size {
    B32,
    B64,
}

impl From<usize> for Size {
    fn from(t: usize) -> Size {
        match t {
            32 => Self::B32,
            64 => Self::B64,
            _ => panic!("Not supported bit!"),
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub enum MemAddr {
    Base(Register, Size),
    BaseOffset(Register, i32),
    BaseIndexShift(Register, Register, u8),
    StackSlot(u32),
    Global(HirInst),
}

pub enum Cond {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Mi,
    Pl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum AddSubImm {
    Imm12(i16),
    Imm12Lsl12(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[allow(dead_code)]
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
#[allow(dead_code)]
pub enum ShiftSize {
    Imm6(u8),
    Register(Register),
}

#[allow(clippy::upper_case_acronyms)]
#[allow(dead_code)]
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

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicOperand {
    Register(Register),
    BitmaskImmediate(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovOperand {
    Register(Register),
    Immediate(i32),
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadSaveOffset {
    Imm12(i16),
    Register(Register),
}

#[allow(dead_code)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FcmpOperand {
    Register(Register),
    Fzero,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Copy)]
pub struct Inst(pub(crate) NonZeroU32);

#[allow(non_camel_case_types)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstKind {
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
    /// Save word: `str rs, [rd, offset]`
    str {
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
    fmov {
        rd: Register,
        src: Register,
    },
    fadd {
        rd: Register,
        rs1: Register,
        rs2: Register,
    },
    fsub {
        rd: Register,
        rs1: Register,
        rs2: Register,
    },
    fmul {
        rd: Register,
        rs1: Register,
        rs2: Register,
    },
    fdiv {
        rd: Register,
        rs1: Register,
        rs2: Register,
    },
    scvtf {
        rd: Register,
        rs: Register,
    },
    fcvtzs {
        rd: Register,
        rs: Register,
    },
    fcmp {
        rs1: Register,
        rs2: FcmpOperand,
    },
    _ParallelCopy(Vec<(Register, Register)>),
    GlobalInitI32 {
        init: Vec<i32>,
    },
    GlobalInitF32 {
        // bit representation of f32
        init: Vec<u32>,
    },
    _string {
        indent_level: usize,
        str: String,
    },
}

impl InstKind {
    #[allow(dead_code)]
    fn is_real_inst(&self) -> bool {
        !matches!(self, InstKind::_string { .. })
    }

    pub fn def(&self) -> InstDef<'_> {
        InstDef {
            data: match self {
                InstKind::mov { rd, .. }
                | InstKind::movz { rd, .. }
                | InstKind::movn { rd, .. }
                | InstKind::movk { rd, .. }
                | InstKind::add { rd, .. }
                | InstKind::sub { rd, .. }
                | InstKind::mul { rd, .. }
                | InstKind::sdiv { rd, .. }
                | InstKind::udiv { rd, .. }
                | InstKind::neg { rd, .. }
                | InstKind::and { rd, .. }
                | InstKind::orr { rd, .. }
                | InstKind::eor { rd, .. }
                | InstKind::lsl { rd, .. }
                | InstKind::lsr { rd, .. }
                | InstKind::asr { rd, .. }
                | InstKind::ldr { rd, .. }
                | InstKind::adrp { rd, .. }
                | InstKind::cset { rd, .. }
                | InstKind::fmov { rd, .. }
                | InstKind::fadd { rd, .. }
                | InstKind::fsub { rd, .. }
                | InstKind::fmul { rd, .. }
                | InstKind::fdiv { rd, .. }
                | InstKind::scvtf { rd, .. }
                | InstKind::fcvtzs { rd, .. } => InstDefData::One(*rd),
                InstKind::_ParallelCopy(edges) => InstDefData::ParallelCopy(edges.as_slice()),
                _ => InstDefData::None,
            },
            pos: 0,
        }
    }

    pub fn uses(&self) -> InstUses<'_> {
        InstUses {
            data: match self {
                InstKind::mov {
                    src: MovOperand::Register(r),
                    ..
                } => InstUsesData::One(*r),
                InstKind::mov {
                    src: MovOperand::Immediate(_),
                    ..
                } => InstUsesData::None,
                InstKind::movk { rd, .. } => InstUsesData::One(*rd),
                InstKind::add { rs1, rs2, .. } | InstKind::sub { rs1, rs2, .. } => {
                    from_add_sub_operand_uses(*rs1, rs2)
                }
                InstKind::cmp { rs1, rs2 } => from_add_sub_operand_uses(*rs1, rs2),
                InstKind::mul { rs1, rs2, .. }
                | InstKind::sdiv { rs1, rs2, .. }
                | InstKind::udiv { rs1, rs2, .. }
                | InstKind::fadd { rs1, rs2, .. }
                | InstKind::fsub { rs1, rs2, .. }
                | InstKind::fmul { rs1, rs2, .. }
                | InstKind::fdiv { rs1, rs2, .. } => InstUsesData::Two(*rs1, *rs2),
                InstKind::neg { src, .. } | InstKind::fmov { src, .. } => InstUsesData::One(*src),
                InstKind::scvtf { rs, .. }
                | InstKind::fcvtzs { rs, .. }
                | InstKind::cbnz { rs, .. }
                | InstKind::cbz { rs, .. } => InstUsesData::One(*rs),
                InstKind::and { rs1, rs2, .. }
                | InstKind::orr { rs1, rs2, .. }
                | InstKind::eor { rs1, rs2, .. } => from_logic_operand_uses(*rs1, rs2),
                InstKind::lsl { rs1, rs2, .. }
                | InstKind::lsr { rs1, rs2, .. }
                | InstKind::asr { rs1, rs2, .. } => from_shift_size_uses(*rs1, rs2),
                InstKind::ldr { rs, offset, .. } => from_load_save_reg_uses(&[*rs], offset),
                InstKind::str { rs, rd, offset } => from_load_save_reg_uses(&[*rs, *rd], offset),
                InstKind::fcmp { rs1, rs2 } => match rs2 {
                    FcmpOperand::Register(r) => InstUsesData::Two(*rs1, *r),
                    FcmpOperand::Fzero => InstUsesData::One(*rs1),
                },
                InstKind::_ParallelCopy(edges) => InstUsesData::ParallelCopy(edges.as_slice()),
                _ => InstUsesData::None,
            },
            pos: 0,
        }
    }
}

fn from_add_sub_operand_uses(rs1: Register, op: &AddSubOperand) -> InstUsesData<'static> {
    match op {
        AddSubOperand::Register(r) => InstUsesData::Two(rs1, *r),
        AddSubOperand::ExtendedRegister(ext) => InstUsesData::Two(rs1, ext.reg),
        AddSubOperand::Immediate(_) | AddSubOperand::AddrLo12(_) => InstUsesData::One(rs1),
    }
}

fn from_logic_operand_uses(rs1: Register, op: &LogicOperand) -> InstUsesData<'static> {
    match op {
        LogicOperand::Register(r) => InstUsesData::Two(rs1, *r),
        LogicOperand::BitmaskImmediate(_) => InstUsesData::One(rs1),
    }
}

fn from_shift_size_uses(rs1: Register, op: &ShiftSize) -> InstUsesData<'static> {
    match op {
        ShiftSize::Register(r) => InstUsesData::Two(rs1, *r),
        ShiftSize::Imm6(_) => InstUsesData::One(rs1),
    }
}

fn from_load_save_reg_uses(
    base_regs: &[Register],
    offset: &LoadSaveOffset,
) -> InstUsesData<'static> {
    match offset {
        LoadSaveOffset::Register(r) => {
            if base_regs.len() == 1 {
                InstUsesData::Two(base_regs[0], *r)
            } else {
                InstUsesData::Three(base_regs[0], base_regs[1], *r)
            }
        }
        LoadSaveOffset::Imm12(_) => {
            if base_regs.len() == 1 {
                InstUsesData::One(base_regs[0])
            } else {
                InstUsesData::Two(base_regs[0], base_regs[1])
            }
        }
    }
}

// ── InstDef ──────────────────────────────────────────────────────────

enum InstDefData<'a> {
    None,
    One(Register),
    ParallelCopy(&'a [(Register, Register)]),
}

pub struct InstDef<'a> {
    data: InstDefData<'a>,
    pos: u8,
}

impl Iterator for InstDef<'_> {
    type Item = Register;

    fn next(&mut self) -> Option<Register> {
        match &self.data {
            InstDefData::None => None,
            InstDefData::One(reg) => {
                if self.pos == 0 {
                    self.pos = 1;
                    Some(*reg)
                } else {
                    None
                }
            }
            InstDefData::ParallelCopy(edges) => {
                if (self.pos as usize) < edges.len() {
                    let reg = edges[self.pos as usize].0;
                    self.pos += 1;
                    Some(reg)
                } else {
                    None
                }
            }
        }
    }
}

// ── InstUses ─────────────────────────────────────────────────────────

enum InstUsesData<'a> {
    None,
    One(Register),
    Two(Register, Register),
    Three(Register, Register, Register),
    ParallelCopy(&'a [(Register, Register)]),
}

pub struct InstUses<'a> {
    data: InstUsesData<'a>,
    pos: u8,
}

impl Iterator for InstUses<'_> {
    type Item = Register;

    fn next(&mut self) -> Option<Register> {
        match &self.data {
            InstUsesData::None => None,
            InstUsesData::One(r0) => {
                if self.pos == 0 {
                    self.pos = 1;
                    Some(*r0)
                } else {
                    None
                }
            }
            InstUsesData::Two(r0, r1) => {
                let reg = match self.pos {
                    0 => Some(*r0),
                    1 => Some(*r1),
                    _ => None,
                };
                self.pos += 1;
                reg
            }
            InstUsesData::Three(r0, r1, r2) => {
                let reg = match self.pos {
                    0 => Some(*r0),
                    1 => Some(*r1),
                    2 => Some(*r2),
                    _ => None,
                };
                self.pos += 1;
                reg
            }
            InstUsesData::ParallelCopy(edges) => {
                if (self.pos as usize) < edges.len() {
                    let reg = edges[self.pos as usize].1;
                    self.pos += 1;
                    Some(reg)
                } else {
                    None
                }
            }
        }
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
            ShiftSize::Register(reg) => write!(f, "{reg}"),
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
        write!(f, "{}, {}", self.reg, self.extend)?;
        if self.shift != 0 {
            write!(f, " #{}", self.shift)?;
        }
        Ok(())
    }
}

impl fmt::Display for AddSubOperand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AddSubOperand::Register(reg) => write!(f, "{reg}"),
            AddSubOperand::Immediate(imm) => write!(f, "{imm}"),
            AddSubOperand::ExtendedRegister(reg) => write!(f, "{reg}"),
            AddSubOperand::AddrLo12(label) => write!(f, ":lo12:{label}"),
        }
    }
}

impl fmt::Display for LogicOperand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogicOperand::Register(reg) => write!(f, "{reg}"),
            LogicOperand::BitmaskImmediate(value) => write!(f, "#{value}"),
        }
    }
}

impl fmt::Display for MovOperand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MovOperand::Register(reg) => write!(f, "{reg}"),
            MovOperand::Immediate(imm) => write!(f, "#{imm}"),
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
            LoadSaveOffset::Register(reg) => write!(f, "{reg}"),
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

impl fmt::Display for FcmpOperand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FcmpOperand::Register(reg) => write!(f, "{reg}"),
            FcmpOperand::Fzero => write!(f, "#0.0"),
        }
    }
}

impl fmt::Display for InstKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "  ")?;
        match self {
            InstKind::mov { rd, src } => write!(f, "mov {rd}, {src}"),
            InstKind::fmov { rd, src } => write!(f, "fmov {rd}, {src}"),
            InstKind::movz { rd, imm } => write!(f, "movz {rd}, {imm}"),
            InstKind::movn { rd, imm } => write!(f, "movn {rd}, {imm}"),
            InstKind::movk { rd, imm } => write!(f, "movk {rd}, {imm}"),
            InstKind::add { rd, rs1, rs2 } => write!(f, "add {rd}, {rs1}, {rs2}"),
            InstKind::sub { rd, rs1, rs2 } => write!(f, "sub {rd}, {rs1}, {rs2}"),
            InstKind::mul { rd, rs1, rs2 } => write!(f, "mul {rd}, {rs1}, {rs2}"),
            InstKind::sdiv { rd, rs1, rs2 } => write!(f, "sdiv {rd}, {rs1}, {rs2}"),
            InstKind::udiv { rd, rs1, rs2 } => write!(f, "udiv {rd}, {rs1}, {rs2}"),
            InstKind::neg { rd, src } => write!(f, "neg {rd}, {src}"),
            InstKind::cmp { rs1, rs2 } => write!(f, "cmp {rs1}, {rs2}"),
            InstKind::and { rd, rs1, rs2 } => write!(f, "and {rd}, {rs1}, {rs2}"),
            InstKind::orr { rd, rs1, rs2 } => write!(f, "orr {rd}, {rs1}, {rs2}"),
            InstKind::eor { rd, rs1, rs2 } => write!(f, "eor {rd}, {rs1}, {rs2}"),
            InstKind::lsl { rd, rs1, rs2 } => write!(f, "lsl {rd}, {rs1}, {rs2}"),
            InstKind::lsr { rd, rs1, rs2 } => write!(f, "lsr {rd}, {rs1}, {rs2}"),
            InstKind::asr { rd, rs1, rs2 } => write!(f, "asr {rd}, {rs1}, {rs2}"),
            InstKind::ldr { rd, rs, offset } => write!(f, "ldr {rd}, [{rs}, {offset}]"),
            InstKind::str { rs, rd, offset } => write!(f, "str {rs}, [{rd}, {offset}]"),
            InstKind::adrp { rd, label } => write!(f, "adrp {rd}, {label}"),
            InstKind::cset { rd, condition } => write!(f, "cset {rd}, {condition}"),
            InstKind::b { label } => write!(f, "b {label}"),
            InstKind::bl { label } => write!(f, "bl {label}"),
            InstKind::cbnz { rs, label } => write!(f, "cbnz {rs}, {label}"),
            InstKind::cbz { rs, label } => write!(f, "cbz {rs}, {label}"),
            InstKind::fadd { rd, rs1, rs2 } => write!(f, "fadd {rd}, {rs1}, {rs2}"),
            InstKind::fsub { rd, rs1, rs2 } => write!(f, "fsub {rd}, {rs1}, {rs2}"),
            InstKind::fmul { rd, rs1, rs2 } => write!(f, "fmul {rd}, {rs1}, {rs2}"),
            InstKind::fdiv { rd, rs1, rs2 } => write!(f, "fdiv {rd}, {rs1}, {rs2}"),
            InstKind::scvtf { rd, rs } => write!(f, "scvtf {rd}, {rs}"),
            InstKind::fcvtzs { rd, rs } => write!(f, "fcvtzs {rd}, {rs}"),
            InstKind::fcmp { rs1, rs2 } => write!(f, "fcmp {rs1}, {rs2}"),
            InstKind::ret => write!(f, "ret"),
            InstKind::_ParallelCopy(edges) => {
                write!(f, "_ParallelCopy: ")?;
                edges
                    .iter()
                    .try_for_each(|edge| write!(f, "{} -> {}", edge.0, edge.1))
            }
            InstKind::GlobalInitI32 { init } => {
                let mut continuous_zero = 0;
                for &val in init {
                    if val == 0 {
                        continuous_zero += 1;
                        continue;
                    }
                    if continuous_zero > 0 {
                        write!(f, ".zero {continuous_zero}")?;
                    }
                    write!(f, ".data {val}");
                }
                Ok(())
            }
            InstKind::GlobalInitF32 { init } => {
                let mut continuous_zero = 0;
                for val in init.iter().map(|&bit| f32::from_bits(bit)) {
                    if val == 0.0f32 {
                        continuous_zero += 1;
                        continue;
                    }
                    if continuous_zero > 0 {
                        write!(f, ".zero {continuous_zero}")?;
                    }
                    write!(f, ".data {val}");
                }
                Ok(())
            }
            InstKind::_string { indent_level, str } => {
                write!(f, "{}{str}", "\t".repeat(*indent_level))
            }
        }
    }
}

// #[allow(non_camel_case_types)]
// pub enum Inst {
//     mov {
//         src: Register,
//         dst: Value,
//     },
//     fmov {
//         src: Value,
//         dst: Value,
//     },
//     itf {
//         dst: Value,
//         src: Value,
//     },
//     fti {
//         dst: Value,
//         src: Value,
//     },
//
//     /// add dst, lhs, rhs
//     add {
//         dst: Register,
//         lhs: Register,
//         rhs: Value,
//     },
//     /// sub dst, lhs, rhs
//     sub {
//         dst: Value,
//         lhs: Value,
//         rhs: Value,
//     },
//     /// mul dst, lhs, rhs
//     mul {
//         dst: Value,
//         lhs: Value,
//         rhs: Value,
//     },
//     /// sdiv dst, lhs, rhs
//     sdiv {
//         dst: Value,
//         lhs: Value,
//         rhs: Value,
//     },
//
//     /// msub dst, sub, lhs, rhs  dst = sub - (lhs * rhs)
//     msub {
//         dst: Value,
//         sub: Value,
//         lhs: Value,
//         rhs: Value,
//     },
//
//     fadd {
//         dst: Value,
//         lhs: Value,
//         rhs: Value,
//     },
//     fsub {
//         dst: Value,
//         lhs: Value,
//         rhs: Value,
//     },
//     fmul {
//         dst: Value,
//         lhs: Value,
//         rhs: Value,
//     },
//     fdiv {
//         dst: Value,
//         lhs: Value,
//         rhs: Value,
//     },
//
//     /// and dst, lhs, rhs
//     and {
//         dst: Value,
//         lhs: Value,
//         rhs: Value,
//     },
//
//     /// orr dst, lhs, rhs
//     orr {
//         dst: Value,
//         lhs: Value,
//         rhs: Value,
//     },
//     eor {
//         dst: Value,
//         lhs: Value,
//         rhs: Value,
//     },
//     lsl {
//         dst: Value,
//         lhs: Value,
//         rhs: Value,
//     },
//     asr {
//         dst: Value,
//         lhs: Value,
//         rhs: Value,
//     },
//
//     /// 比较指令 (cmp lhs, rhs) - 影响全局 NZCV 标志位，没有 dst！
//     cmp {
//         lhs: Value,
//         rhs: Value,
//     },
//     /// 浮点比较指令 (fcmp lhs, rhs)
//     fcmp {
//         lhs: Value,
//         rhs: Value,
//     },
//     /// 根据刚刚的比较结果，设置 dst 为 1 或 0 (cset dst, cond)
//     /// SysY 常见模式: a < b -> Cmp(a, b), Cset(dst, Lt)
//     cset {
//         dst: Value,
//         cond: Cond,
//     },
//
//     /// 加载 (ldr dst, addr) -> 如果 dst 是 float 就是 ldr sX，否则 ldr wX/xX
//     ldr {
//         dst: Value,
//         addr: MemAddr,
//     },
//     /// 存储 (str src, addr)
//     str {
//         src: Value,
//         addr: MemAddr,
//     },
//
//     /// 无条件跳转 (b label)
//     b {
//         target: String,
//     },
//     /// 条件跳转 (b.cond label) - 依赖前面的 Cmp
//     bcc {
//         cond: Cond,
//         target: String,
//     },
//     /// 函数调用 (bl func)。注意：要隐式地标记使用了哪些参数寄存器 (如 x0-x7)，
//     /// 以便寄存器分配器知道它们会被覆盖！
//     call {
//         func: String,
//         arg_regs: Vec<Register>,
//     },
//     /// ret
//     ret,
//
//     // ==========================================
//     // 7. 伪指令 (Pseudo Instructions - 扩展性核心)
//     // ==========================================
//     /// 解决 Phi 节点和带参数 Jump 的相互覆盖问题！
//     /// 寄存器分配器结束后，将其通过“拓扑排序”展开为安全的单步 Mov。
//     ParallelCopy(Vec<(Value, Value)>),
//
//     /// 加载全局变量的绝对地址 (adrp + add)
//     /// ARMv8 要求分两步加载全局变量地址，在 MIR 中可以先用一条 Pseudo 指令表示，
//     /// 等到最后生成汇编时再展开成两句。
//     LoadGlobalAddr {
//         dst: Value,
//         symbol: String,
//     },
//
//     /// 加载超过 12 bit 限制的大立即数 (movz + movk... 或 ldr =, 等)
//     LoadLargeImm {
//         dst: Value,
//         imm: i32,
//     },
//     GlobalInitI32 {
//         init: Vec<i32>,
//     },
//     GlobalInitF32 {
//         init: Vec<f32>,
//     },
// }
