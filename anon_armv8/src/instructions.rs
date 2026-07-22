//! Typed AArch64 instruction forms and encoding-valid operands.

use taki_mir::{
    abi::{ArgPair, CallArgPair, CallRetPair, RetPair},
    reg_alloc::reg::PRegSet,
    register::{Reg, Writable},
};

use crate::{
    labels::Label,
    regs::{Gpr, OperandSize},
};

pub type WritableReg = Writable<Reg>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Imm12 {
    value: u16,
    shift12: bool,
}

impl Imm12 {
    pub const fn new(value: u16, shift12: bool) -> Option<Self> {
        if value <= 0xfff {
            Some(Self { value, shift12 })
        } else {
            None
        }
    }

    pub const fn value(self) -> u16 {
        self.value
    }

    pub const fn shift12(self) -> bool {
        self.shift12
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImmLogic {
    value: u64,
    size: OperandSize,
}

impl ImmLogic {
    pub fn new(value: u64, size: OperandSize) -> Option<Self> {
        let value = match size {
            OperandSize::Size32 => value & u64::from(u32::MAX),
            OperandSize::Size64 => value,
        };
        if is_logical_immediate(value, size) {
            Some(Self { value, size })
        } else {
            None
        }
    }

    pub const fn value(self) -> u64 {
        self.value
    }

    pub const fn size(self) -> OperandSize {
        self.size
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImmShift(u8);

impl ImmShift {
    pub const fn new(value: u8, size: OperandSize) -> Option<Self> {
        let limit = match size {
            OperandSize::Size32 => 32,
            OperandSize::Size64 => 64,
        };
        if value < limit {
            Some(Self(value))
        } else {
            None
        }
    }

    pub const fn value(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShiftOp {
    Lsl,
    Lsr,
    Asr,
    Ror,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtendOp {
    Uxtb,
    Uxth,
    Uxtw,
    Uxtx,
    Sxtb,
    Sxth,
    Sxtw,
    Sxtx,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MoveWideConst {
    bits: u16,
    shift: u8,
}

impl MoveWideConst {
    pub const fn new(bits: u16, shift: u8, size: OperandSize) -> Option<Self> {
        let valid_shift = match size {
            OperandSize::Size32 => shift == 0 || shift == 16,
            OperandSize::Size64 => shift == 0 || shift == 16 || shift == 32 || shift == 48,
        };
        if valid_shift {
            Some(Self { bits, shift })
        } else {
            None
        }
    }

    pub const fn bits(self) -> u16 {
        self.bits
    }

    pub const fn shift(self) -> u8 {
        self.shift
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SImm9(i16);

impl SImm9 {
    pub const fn new(value: i16) -> Option<Self> {
        if value >= -256 && value <= 255 {
            Some(Self(value))
        } else {
            None
        }
    }

    pub const fn value(self) -> i16 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UImm12Scaled {
    value: u16,
    access_size: u8,
}

impl UImm12Scaled {
    pub const fn new(value: u64, access_size: u8) -> Option<Self> {
        if !matches!(access_size, 4 | 8) || value % u64::from(access_size) != 0 {
            return None;
        }
        let scaled = value / u64::from(access_size);
        if scaled <= 0xfff {
            Some(Self {
                value: scaled as u16,
                access_size,
            })
        } else {
            None
        }
    }

    pub const fn byte_offset(self) -> u64 {
        self.value as u64 * self.access_size as u64
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SImm7Scaled {
    value: i8,
    access_size: u8,
}

impl SImm7Scaled {
    pub const fn new(value: i64, access_size: u8) -> Option<Self> {
        if !matches!(access_size, 4 | 8) || value % i64::from(access_size) != 0 {
            return None;
        }
        let scaled = value / i64::from(access_size);
        if (-64..=63).contains(&scaled) {
            Some(Self {
                value: scaled as i8,
                access_size,
            })
        } else {
            None
        }
    }

    pub const fn byte_offset(self) -> i64 {
        self.value as i64 * self.access_size as i64
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryType {
    I32,
    I64,
    F32,
}

impl MemoryType {
    pub const fn byte_size(self) -> u8 {
        match self {
            Self::I32 | Self::F32 => 4,
            Self::I64 => 8,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AMode {
    Reg {
        base: Gpr,
    },
    UnsignedOffset {
        base: Gpr,
        offset: UImm12Scaled,
    },
    SignedOffset {
        base: Gpr,
        offset: SImm9,
    },
    RegOffset {
        base: Gpr,
        index: Reg,
    },
    ScaledRegOffset {
        base: Gpr,
        index: Reg,
        shift: u8,
    },
    ExtendedRegOffset {
        base: Gpr,
        index: Reg,
        extend: ExtendOp,
        shift: u8,
    },
    FrameSlot(i64),
    IncomingArg(i64),
    OutgoingArg(i64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairAMode {
    SignedOffset { base: Gpr, offset: SImm7Scaled },
    PreIndex { base: Gpr, offset: SImm7Scaled },
    PostIndex { base: Gpr, offset: SImm7Scaled },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AluOp {
    Add,
    Sub,
    Mul,
    And,
    Orr,
    Eor,
    Lsl,
    Lsr,
    Asr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cond {
    Eq,
    Ne,
    Hs,
    Lo,
    Mi,
    Pl,
    Vs,
    Vc,
    Hi,
    Ls,
    Ge,
    Lt,
    Gt,
    Le,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FpuOp {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Clone, Debug)]
pub enum MInst {
    Nop,
    AluRRR {
        op: AluOp,
        size: OperandSize,
        dst: WritableReg,
        lhs: Reg,
        rhs: Reg,
    },
    AluRRRR {
        op: AluOp,
        size: OperandSize,
        dst: WritableReg,
        lhs: Reg,
        rhs: Reg,
        carry: Reg,
    },
    AluRRImm12 {
        op: AluOp,
        size: OperandSize,
        dst: WritableReg,
        src: Gpr,
        imm: Imm12,
    },
    AluRRImmLogic {
        op: AluOp,
        size: OperandSize,
        dst: WritableReg,
        src: Gpr,
        imm: ImmLogic,
    },
    AluRRImmShift {
        op: AluOp,
        size: OperandSize,
        dst: WritableReg,
        src: Reg,
        shift: ImmShift,
    },
    AluRRRShift {
        op: AluOp,
        size: OperandSize,
        dst: WritableReg,
        lhs: Reg,
        rhs: Reg,
        shift: ShiftOp,
        amount: ImmShift,
    },
    AluRRRExtend {
        op: AluOp,
        size: OperandSize,
        dst: WritableReg,
        lhs: Gpr,
        rhs: Reg,
        extend: ExtendOp,
        shift: u8,
    },
    SDiv {
        size: OperandSize,
        dst: WritableReg,
        lhs: Reg,
        rhs: Reg,
    },
    MAdd {
        size: OperandSize,
        dst: WritableReg,
        lhs: Reg,
        rhs: Reg,
        addend: Reg,
    },
    MSub {
        size: OperandSize,
        dst: WritableReg,
        lhs: Reg,
        rhs: Reg,
        subtrahend: Reg,
    },
    CmpRR {
        size: OperandSize,
        lhs: Reg,
        rhs: Gpr,
    },
    CmpImm {
        size: OperandSize,
        lhs: Reg,
        imm: Imm12,
    },
    Mov {
        size: OperandSize,
        dst: WritableReg,
        src: Reg,
    },
    MovPhys {
        size: OperandSize,
        dst: Gpr,
        src: Gpr,
    },
    MovZ {
        size: OperandSize,
        dst: WritableReg,
        imm: MoveWideConst,
    },
    MovN {
        size: OperandSize,
        dst: WritableReg,
        imm: MoveWideConst,
    },
    MovK {
        size: OperandSize,
        dst: WritableReg,
        src: Reg,
        imm: MoveWideConst,
    },
    MovFromZero {
        size: OperandSize,
        dst: WritableReg,
    },
    LoadAddr {
        dst: WritableReg,
        label: Label,
    },
    BCond {
        cond: Cond,
        label: Label,
    },
    Cbz {
        size: OperandSize,
        reg: Reg,
        label: Label,
    },
    Cbnz {
        size: OperandSize,
        reg: Reg,
        label: Label,
    },
    Tbz {
        reg: Reg,
        bit: u8,
        label: Label,
    },
    Tbnz {
        reg: Reg,
        bit: u8,
        label: Label,
    },
    CondBr {
        cond: Cond,
        true_label: Label,
        false_label: Label,
    },
    Jump {
        label: Label,
    },
    CSet {
        cond: Cond,
        dst: WritableReg,
    },
    FMov {
        dst: WritableReg,
        src: Reg,
    },
    FAlu {
        op: FpuOp,
        dst: WritableReg,
        lhs: Reg,
        rhs: Reg,
    },
    FCmp {
        lhs: Reg,
        rhs: Reg,
    },
    Scvtf {
        dst: WritableReg,
        src: Reg,
    },
    Fcvtzs {
        dst: WritableReg,
        src: Reg,
    },
    Load {
        ty: MemoryType,
        dst: WritableReg,
        addr: AMode,
    },
    Store {
        ty: MemoryType,
        src: Reg,
        addr: AMode,
    },
    LoadPair {
        ty: MemoryType,
        dst1: WritableReg,
        dst2: WritableReg,
        addr: PairAMode,
    },
    StorePair {
        ty: MemoryType,
        src1: Reg,
        src2: Reg,
        addr: PairAMode,
    },
    Args {
        pairs: Vec<ArgPair>,
    },
    Call {
        args: Vec<CallArgPair>,
        ret: Option<CallRetPair>,
        clobbers: PRegSet,
        label: Label,
    },
    RetVal {
        pair: RetPair,
    },
    Ret,
}

impl MInst {
    pub fn verify(&self) -> Result<(), &'static str> {
        match self {
            Self::AluRRImmLogic { size, imm, .. } if *size != imm.size() => {
                Err("logical immediate width does not match instruction width")
            }
            Self::AluRRRExtend { shift, .. } if *shift > 4 => {
                Err("extended register shift exceeds AArch64 encoding range")
            }
            Self::Tbz { bit, .. } | Self::Tbnz { bit, .. } if *bit >= 64 => {
                Err("test-bit index exceeds AArch64 encoding range")
            }
            Self::LoadPair { ty, addr, .. } | Self::StorePair { ty, addr, .. }
                if pair_access_size(addr) != ty.byte_size() =>
            {
                Err("pair address offset scaling does not match memory width")
            }
            _ => Ok(()),
        }
    }
}

fn pair_access_size(addr: &PairAMode) -> u8 {
    match addr {
        PairAMode::SignedOffset { offset, .. }
        | PairAMode::PreIndex { offset, .. }
        | PairAMode::PostIndex { offset, .. } => offset.access_size,
    }
}

fn is_logical_immediate(value: u64, size: OperandSize) -> bool {
    let width = match size {
        OperandSize::Size32 => 32,
        OperandSize::Size64 => 64,
    };
    let mask = if width == 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    };
    let value = value & mask;
    if value == 0 || value == mask {
        return false;
    }
    for element_width in [2u8, 4, 8, 16, 32, 64] {
        if element_width > width || width % element_width != 0 {
            continue;
        }
        let element_mask = if element_width == 64 {
            u64::MAX
        } else {
            (1u64 << element_width) - 1
        };
        for ones in 1..element_width {
            let pattern = (1u64 << ones) - 1;
            for rotate in 0..element_width {
                let rotated = if rotate == 0 {
                    pattern
                } else {
                    ((pattern >> rotate) | (pattern << (element_width - rotate))) & element_mask
                };
                let mut repeated = 0;
                for offset in (0..width).step_by(element_width as usize) {
                    repeated |= rotated << offset;
                }
                if repeated == value {
                    return true;
                }
            }
        }
    }
    false
}
