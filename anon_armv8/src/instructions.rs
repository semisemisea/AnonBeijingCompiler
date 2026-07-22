//! Typed AArch64 instruction forms and encoding-valid operands.

use taki_mir::{
    abi::{ArgPair, CallArgPair, CallRetPair, RetPair, StackAMode},
    reg_alloc::reg::{OperandVisitor, OperandVisitorImpl, PRegSet, RegClass},
    register::{Reg, Writable},
    types::{LoweredType, F32, I32, I64},
    vcode::{CallType, EmitContext, MachInst, MachInstEmit, MachTerminator},
};

use crate::{
    abi::AArch64Abi,
    labels::Label,
    regs::{Gpr, OperandSize, RegOrZr},
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
    /// 64-bit floating register preservation; language-level values are f32.
    F64,
}

impl MemoryType {
    pub const fn byte_size(self) -> u8 {
        match self {
            Self::I32 | Self::F32 => 4,
            Self::I64 | Self::F64 => 8,
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
    /// A fixed offset from the post-prologue stack pointer.
    SpOffset(i64),
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
        lhs: RegOrZr,
        rhs: RegOrZr,
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
        src: RegOrZr,
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
        lhs: RegOrZr,
        rhs: RegOrZr,
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
        rhs: RegOrZr,
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
    /// A typed pseudo-instruction expanded to encoding-valid move-wide forms
    /// by `MachInstEmit`. It keeps the ABI's single-instruction hook sound.
    LoadImm {
        size: OperandSize,
        dst: WritableReg,
        value: u64,
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
    FMovFromZero {
        dst: WritableReg,
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
            Self::AluRRImm12 { op, .. } if !matches!(op, AluOp::Add | AluOp::Sub) => {
                Err("invalid AArch64 add/sub immediate operation")
            }
            Self::AluRRImmLogic { op, .. }
                if !matches!(op, AluOp::And | AluOp::Orr | AluOp::Eor) =>
            {
                Err("invalid AArch64 logical immediate operation")
            }
            Self::AluRRImmShift { op, .. }
                if !matches!(op, AluOp::Lsl | AluOp::Lsr | AluOp::Asr) =>
            {
                Err("invalid AArch64 immediate shift operation")
            }
            Self::AluRRImmLogic { size, imm, .. } if *size != imm.size() => {
                Err("logical immediate width does not match instruction width")
            }
            Self::AluRRRShift {
                op,
                size,
                shift,
                amount,
                ..
            } if !shifted_alu_is_legal(*op, *shift) || amount.value() >= size.bits() => {
                Err("invalid AArch64 shifted-register ALU form")
            }
            Self::AluRRRExtend {
                op,
                size,
                lhs,
                extend,
                shift,
                ..
            } if matches!(lhs, Gpr::Zr) || !extended_alu_is_legal(*op, *size, *extend, *shift) => {
                Err("invalid AArch64 extended-register ALU form")
            }
            Self::Load { ty, addr, .. } | Self::Store { ty, addr }
                if !amode_is_legal(addr, *ty) =>
            {
                Err("invalid AArch64 memory address form")
            }
            Self::LoadPair { addr, .. } | Self::StorePair { addr, .. }
                if !pair_amode_is_legal(addr) =>
            {
                Err("invalid AArch64 pair memory address form")
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

impl From<StackAMode> for AMode {
    fn from(value: StackAMode) -> Self {
        match value {
            StackAMode::IncomingArg(offset, _) => Self::IncomingArg(offset),
            StackAMode::Slot(offset) => Self::FrameSlot(offset),
            StackAMode::OutgoingArg(offset) => Self::OutgoingArg(offset),
        }
    }
}

impl MachInst for MInst {
    type ABISpec = AArch64Abi;

    fn get_operands(&mut self, collector: &mut impl OperandVisitor) {
        match self {
            Self::Nop | Self::BCond { .. } | Self::Jump { .. } | Self::Ret => {}
            Self::AluRRR { dst, lhs, rhs, .. } => {
                use_reg_or_zr(collector, lhs);
                use_reg_or_zr(collector, rhs);
                collector.reg_def(dst);
            }
            Self::SDiv { dst, lhs, rhs, .. } | Self::FAlu { dst, lhs, rhs, .. } => {
                collector.reg_use(lhs);
                collector.reg_use(rhs);
                collector.reg_def(dst);
            }
            Self::AluRRRR {
                dst,
                lhs,
                rhs,
                carry,
                ..
            } => {
                collector.reg_use(lhs);
                collector.reg_use(rhs);
                collector.reg_use(carry);
                collector.reg_def(dst);
            }
            Self::AluRRImm12 { dst, src, .. } => {
                use_gpr(collector, src);
                collector.reg_def(dst);
            }
            Self::AluRRImmLogic { dst, src, .. } => {
                use_reg_or_zr(collector, src);
                collector.reg_def(dst);
            }
            Self::AluRRImmShift { dst, src, .. }
            | Self::Mov { dst, src, .. }
            | Self::FMov { dst, src }
            | Self::Scvtf { dst, src }
            | Self::Fcvtzs { dst, src } => {
                collector.reg_use(src);
                collector.reg_def(dst);
            }
            Self::AluRRRShift { dst, lhs, rhs, .. } => {
                use_reg_or_zr(collector, lhs);
                use_reg_or_zr(collector, rhs);
                collector.reg_def(dst);
            }
            Self::AluRRRExtend { dst, lhs, rhs, .. } => {
                use_gpr(collector, lhs);
                collector.reg_use(rhs);
                collector.reg_def(dst);
            }
            Self::MAdd {
                dst,
                lhs,
                rhs,
                addend,
                ..
            } => {
                collector.reg_use(lhs);
                collector.reg_use(rhs);
                collector.reg_use(addend);
                collector.reg_def(dst);
            }
            Self::MSub {
                dst,
                lhs,
                rhs,
                subtrahend,
                ..
            } => {
                collector.reg_use(lhs);
                collector.reg_use(rhs);
                collector.reg_use(subtrahend);
                collector.reg_def(dst);
            }
            Self::CmpRR { lhs, rhs, .. } => {
                collector.reg_use(lhs);
                use_reg_or_zr(collector, rhs);
            }
            Self::CmpImm { lhs, .. }
            | Self::Cbz { reg: lhs, .. }
            | Self::Cbnz { reg: lhs, .. }
            | Self::Tbz { reg: lhs, .. }
            | Self::Tbnz { reg: lhs, .. } => collector.reg_use(lhs),
            Self::MovPhys { dst, src, .. } => {
                use_gpr(collector, src);
                def_gpr(collector, dst);
            }
            Self::LoadImm { dst, .. }
            | Self::MovZ { dst, .. }
            | Self::MovN { dst, .. }
            | Self::MovFromZero { dst, .. }
            | Self::FMovFromZero { dst }
            | Self::LoadAddr { dst, .. }
            | Self::CSet { dst, .. } => collector.reg_def(dst),
            Self::MovK { dst, src, .. } => {
                collector.reg_use(src);
                collector.reg_reuse_def(dst, 0);
            }
            Self::FCmp { lhs, rhs } => {
                collector.reg_use(lhs);
                collector.reg_use(rhs);
            }
            Self::Load { dst, addr, .. } => {
                visit_amode(collector, addr);
                collector.reg_def(dst);
            }
            Self::Store { src, addr, .. } => {
                collector.reg_use(src);
                visit_amode(collector, addr);
            }
            Self::LoadPair {
                dst1, dst2, addr, ..
            } => {
                visit_pair_amode(collector, addr);
                collector.reg_def(dst1);
                collector.reg_def(dst2);
            }
            Self::StorePair {
                src1, src2, addr, ..
            } => {
                collector.reg_use(src1);
                collector.reg_use(src2);
                visit_pair_amode(collector, addr);
            }
            Self::Args { pairs } => {
                for pair in pairs {
                    collector.reg_fixed_def(&mut pair.vreg, pair.preg);
                }
            }
            Self::Call {
                args,
                ret,
                clobbers,
                ..
            } => {
                for pair in args {
                    collector.reg_fixed_use(&mut pair.vreg, pair.preg);
                }
                if let Some(pair) = ret {
                    collector.reg_fixed_def(&mut pair.vreg, pair.preg);
                }
                collector.reg_clobbers(*clobbers | crate::regs::DEFAULT_CLOBBERS);
            }
            Self::RetVal { pair } => collector.reg_fixed_use(&mut pair.vreg, pair.preg),
            Self::CondBr { .. } => {}
        }
    }

    fn is_move(&self) -> Option<(Writable<Reg>, Reg)> {
        match self {
            Self::Mov { dst, src, .. } | Self::FMov { dst, src } => Some((*dst, *src)),
            _ => None,
        }
    }
    fn is_term(&self) -> MachTerminator {
        match self {
            Self::Ret => MachTerminator::Return,
            Self::BCond { .. }
            | Self::Cbz { .. }
            | Self::Cbnz { .. }
            | Self::Tbz { .. }
            | Self::Tbnz { .. }
            | Self::CondBr { .. }
            | Self::Jump { .. } => MachTerminator::Branch,
            _ => MachTerminator::None,
        }
    }
    fn call_type(&self) -> CallType {
        if matches!(self, Self::Call { .. }) {
            CallType::Call
        } else {
            CallType::None
        }
    }
    fn is_mem_access(&self) -> bool {
        matches!(
            self,
            Self::Load { .. } | Self::Store { .. } | Self::LoadPair { .. } | Self::StorePair { .. }
        )
    }
    fn rc_for_type(ty: LoweredType) -> (&'static [RegClass], &'static [LoweredType]) {
        match ty {
            I32 => (&[RegClass::Int], &[I32]),
            I64 => (&[RegClass::Int], &[I64]),
            F32 => (&[RegClass::Float], &[F32]),
            _ => unreachable!("unsupported AArch64 lowered type"),
        }
    }
    fn gen_jump(target: taki_mir::block_order::MirBlockIndex) -> Self {
        Self::Jump {
            label: Label::from_block(target),
        }
    }
}

fn use_gpr(collector: &mut impl OperandVisitor, gpr: &mut Gpr) {
    if let Gpr::Reg(reg) = gpr {
        collector.reg_use(reg);
    }
}
fn use_reg_or_zr(collector: &mut impl OperandVisitor, reg: &mut RegOrZr) {
    if let RegOrZr::Reg(reg) = reg {
        collector.reg_use(reg);
    }
}
fn def_gpr(collector: &mut impl OperandVisitor, gpr: &mut Gpr) {
    if let Gpr::Reg(reg) = gpr {
        collector.reg_def_reg(reg);
    }
}
fn visit_amode(collector: &mut impl OperandVisitor, addr: &mut AMode) {
    match addr {
        AMode::Reg { base }
        | AMode::UnsignedOffset { base, .. }
        | AMode::SignedOffset { base, .. } => use_gpr(collector, base),
        AMode::RegOffset { base, index }
        | AMode::ScaledRegOffset { base, index, .. }
        | AMode::ExtendedRegOffset { base, index, .. } => {
            use_gpr(collector, base);
            collector.reg_use(index);
        }
        AMode::FrameSlot(_)
        | AMode::SpOffset(_)
        | AMode::IncomingArg(_)
        | AMode::OutgoingArg(_) => {}
    }
}
fn visit_pair_amode(collector: &mut impl OperandVisitor, addr: &mut PairAMode) {
    match addr {
        PairAMode::SignedOffset { base, .. }
        | PairAMode::PreIndex { base, .. }
        | PairAMode::PostIndex { base, .. } => use_gpr(collector, base),
    }
}

impl MachInstEmit for MInst {
    fn emit(&self, ctx: &mut dyn EmitContext) -> core::fmt::Result {
        match self {
            Self::Nop => write!(ctx, "nop"),
            Self::AluRRR {
                op,
                size,
                dst,
                lhs,
                rhs,
            } => emit_sized_data_rrr(ctx, alu_name(*op, *size), *size, dst.to_reg(), lhs, rhs),
            Self::AluRRRR {
                op,
                size,
                dst,
                lhs,
                rhs,
                carry,
            } => emit_sized_rrrr(
                ctx,
                alu_name(*op, *size),
                *size,
                dst.to_reg(),
                lhs,
                rhs,
                carry,
            ),
            Self::AluRRImm12 {
                op,
                size,
                dst,
                src,
                imm,
            } => {
                write!(ctx, "{} ", alu_name(*op, *size))?;
                emit_reg(ctx, dst.to_reg(), *size)?;
                write!(ctx, ", ")?;
                emit_gpr(ctx, src, *size)?;
                write!(ctx, ", #{}", imm.value())?;
                if imm.shift12() {
                    write!(ctx, ", lsl #12")?;
                }
                Ok(())
            }
            Self::AluRRImmLogic {
                op,
                size,
                dst,
                src,
                imm,
            } => {
                write!(ctx, "{} ", alu_name(*op, *size))?;
                emit_reg(ctx, dst.to_reg(), *size)?;
                write!(ctx, ", ")?;
                emit_reg_or_zr(ctx, src, *size)?;
                write!(ctx, ", #0x{:x}", imm.value())
            }
            Self::AluRRImmShift {
                op,
                size,
                dst,
                src,
                shift,
            } => {
                write!(ctx, "{} ", alu_name(*op, *size))?;
                emit_reg(ctx, dst.to_reg(), *size)?;
                write!(ctx, ", ")?;
                emit_reg(ctx, *src, *size)?;
                write!(ctx, ", #{}", shift.value())
            }
            Self::AluRRRShift {
                op,
                size,
                dst,
                lhs,
                rhs,
                shift,
                amount,
            } => {
                write!(ctx, "{} ", alu_name(*op, *size))?;
                emit_reg(ctx, dst.to_reg(), *size)?;
                write!(ctx, ", ")?;
                emit_reg_or_zr(ctx, lhs, *size)?;
                write!(ctx, ", ")?;
                emit_reg_or_zr(ctx, rhs, *size)?;
                write!(ctx, ", {} #{}", shift_name(*shift), amount.value())
            }
            Self::AluRRRExtend {
                op,
                size,
                dst,
                lhs,
                rhs,
                extend,
                shift,
            } => {
                write!(ctx, "{} ", alu_name(*op, *size))?;
                emit_reg(ctx, dst.to_reg(), *size)?;
                write!(ctx, ", ")?;
                emit_gpr(ctx, lhs, *size)?;
                write!(ctx, ", ")?;
                emit_reg(ctx, *rhs, extend_source_size(*extend, *size))?;
                write!(ctx, ", {}", extend_name(*extend))?;
                if *shift != 0 {
                    write!(ctx, " #{}", shift)?;
                }
                Ok(())
            }
            Self::SDiv {
                size,
                dst,
                lhs,
                rhs,
            } => emit_sized_rrr(
                ctx,
                if *size == OperandSize::Size32 {
                    "sdiv"
                } else {
                    "sdiv"
                },
                *size,
                dst.to_reg(),
                lhs,
                rhs,
            ),
            Self::MAdd {
                size,
                dst,
                lhs,
                rhs,
                addend,
            } => emit_sized_rrrr(ctx, "madd", *size, dst.to_reg(), lhs, rhs, addend),
            Self::MSub {
                size,
                dst,
                lhs,
                rhs,
                subtrahend,
            } => emit_sized_rrrr(ctx, "msub", *size, dst.to_reg(), lhs, rhs, subtrahend),
            Self::CmpRR { size, lhs, rhs } => {
                write!(ctx, "cmp ")?;
                emit_reg(ctx, *lhs, *size)?;
                write!(ctx, ", ")?;
                emit_reg_or_zr(ctx, rhs, *size)
            }
            Self::CmpImm { size, lhs, imm } => {
                write!(ctx, "cmp ")?;
                emit_reg(ctx, *lhs, *size)?;
                write!(ctx, ", #{}", imm.value())?;
                if imm.shift12() {
                    write!(ctx, ", lsl #12")?;
                }
                Ok(())
            }
            Self::Mov { size, dst, src } => {
                write!(ctx, "mov ")?;
                emit_reg(ctx, dst.to_reg(), *size)?;
                write!(ctx, ", ")?;
                emit_reg(ctx, *src, *size)
            }
            Self::MovPhys { size, dst, src } => {
                write!(ctx, "mov ")?;
                emit_gpr(ctx, dst, *size)?;
                write!(ctx, ", ")?;
                emit_gpr(ctx, src, *size)
            }
            Self::LoadImm { size, dst, value } => emit_load_imm(ctx, dst.to_reg(), *value, *size),
            Self::MovZ { size, dst, imm } | Self::MovN { size, dst, imm } => {
                write!(
                    ctx,
                    "{} ",
                    if matches!(self, Self::MovZ { .. }) {
                        "movz"
                    } else {
                        "movn"
                    }
                )?;
                emit_reg(ctx, dst.to_reg(), *size)?;
                write!(ctx, ", #0x{:x}", imm.bits())?;
                if imm.shift() != 0 {
                    write!(ctx, ", lsl #{}", imm.shift())?;
                }
                Ok(())
            }
            Self::MovK { size, dst, imm, .. } => {
                write!(ctx, "movk ")?;
                emit_reg(ctx, dst.to_reg(), *size)?;
                write!(ctx, ", #0x{:x}", imm.bits())?;
                if imm.shift() != 0 {
                    write!(ctx, ", lsl #{}", imm.shift())?;
                }
                Ok(())
            }
            Self::MovFromZero { size, dst } => {
                write!(ctx, "mov ")?;
                emit_reg(ctx, dst.to_reg(), *size)?;
                write!(ctx, ", ")?;
                emit_gpr(ctx, &Gpr::Zr, *size)
            }
            Self::LoadAddr { dst, label } => {
                write!(ctx, "adrp ")?;
                emit_reg(ctx, dst.to_reg(), OperandSize::Size64)?;
                write!(ctx, ", ")?;
                label.emit(ctx)?;
                write!(ctx, "\n    add ")?;
                emit_reg(ctx, dst.to_reg(), OperandSize::Size64)?;
                write!(ctx, ", ")?;
                emit_reg(ctx, dst.to_reg(), OperandSize::Size64)?;
                write!(ctx, ", :lo12:")?;
                label.emit(ctx)
            }
            Self::BCond { cond, label } => {
                write!(ctx, "b.{} ", cond_name(*cond))?;
                label.emit(ctx)
            }
            Self::Cbz { size, reg, label } | Self::Cbnz { size, reg, label } => {
                write!(
                    ctx,
                    "{} ",
                    if matches!(self, Self::Cbz { .. }) {
                        "cbz"
                    } else {
                        "cbnz"
                    }
                )?;
                emit_reg(ctx, *reg, *size)?;
                write!(ctx, ", ")?;
                label.emit(ctx)
            }
            Self::Tbz { reg, bit, label } | Self::Tbnz { reg, bit, label } => {
                write!(
                    ctx,
                    "{} ",
                    if matches!(self, Self::Tbz { .. }) {
                        "tbz"
                    } else {
                        "tbnz"
                    }
                )?;
                emit_reg(ctx, *reg, OperandSize::Size64)?;
                write!(ctx, ", #{bit}, ")?;
                label.emit(ctx)
            }
            Self::CondBr {
                cond,
                true_label,
                false_label,
            } => {
                write!(ctx, "b.{} ", cond_name(*cond))?;
                true_label.emit(ctx)?;
                write!(ctx, "\n    b ")?;
                false_label.emit(ctx)
            }
            Self::Jump { label } => {
                write!(ctx, "b ")?;
                label.emit(ctx)
            }
            Self::CSet { cond, dst } => {
                write!(ctx, "cset ")?;
                emit_reg(ctx, dst.to_reg(), OperandSize::Size32)?;
                write!(ctx, ", {}", cond_name(*cond))
            }
            Self::FMov { dst, src } => emit_fmov(ctx, dst.to_reg(), src),
            Self::FMovFromZero { dst } => {
                write!(ctx, "fmov ")?;
                emit_float_reg(ctx, dst.to_reg(), false)?;
                write!(ctx, ", wzr")
            }
            Self::FAlu { op, dst, lhs, rhs } => {
                emit_float_rrr(ctx, fpu_name(*op), dst.to_reg(), lhs, rhs)
            }
            Self::FCmp { lhs, rhs } => emit_float_rr(ctx, "fcmp", *lhs, rhs),
            Self::Scvtf { dst, src } => {
                write!(ctx, "scvtf ")?;
                emit_float_reg(ctx, dst.to_reg(), false)?;
                write!(ctx, ", ")?;
                emit_reg(ctx, *src, OperandSize::Size32)
            }
            Self::Fcvtzs { dst, src } => {
                write!(ctx, "fcvtzs ")?;
                emit_reg(ctx, dst.to_reg(), OperandSize::Size32)?;
                write!(ctx, ", ")?;
                emit_float_reg(ctx, *src, false)
            }
            Self::Load { ty, dst, addr } => {
                write!(ctx, "{} ", load_name(*ty))?;
                emit_data_reg(ctx, dst.to_reg(), *ty)?;
                write!(ctx, ", ")?;
                emit_amode(ctx, addr)
            }
            Self::Store { ty, src, addr } => {
                write!(ctx, "{} ", store_name(*ty))?;
                emit_data_reg(ctx, *src, *ty)?;
                write!(ctx, ", ")?;
                emit_amode(ctx, addr)
            }
            Self::LoadPair {
                ty,
                dst1,
                dst2,
                addr,
            } => {
                write!(ctx, "ldp ")?;
                emit_data_reg(ctx, dst1.to_reg(), *ty)?;
                write!(ctx, ", ")?;
                emit_data_reg(ctx, dst2.to_reg(), *ty)?;
                write!(ctx, ", ")?;
                emit_pair_amode(ctx, addr)
            }
            Self::StorePair {
                ty,
                src1,
                src2,
                addr,
            } => {
                write!(ctx, "stp ")?;
                emit_data_reg(ctx, *src1, *ty)?;
                write!(ctx, ", ")?;
                emit_data_reg(ctx, *src2, *ty)?;
                write!(ctx, ", ")?;
                emit_pair_amode(ctx, addr)
            }
            Self::Args { .. } | Self::RetVal { .. } => write!(ctx, "// ABI register assignment"),
            Self::Call { label, .. } => {
                write!(ctx, "bl ")?;
                label.emit(ctx)
            }
            Self::Ret => write!(ctx, "ret"),
        }
    }
}

fn emit_rr(
    ctx: &mut dyn EmitContext,
    op: &str,
    dst: Reg,
    src: &Reg,
    size: OperandSize,
) -> core::fmt::Result {
    write!(ctx, "{op} ")?;
    emit_reg(ctx, dst, size)?;
    write!(ctx, ", ")?;
    emit_reg(ctx, *src, size)
}
fn emit_load_imm(
    ctx: &mut dyn EmitContext,
    dst: Reg,
    value: u64,
    size: OperandSize,
) -> core::fmt::Result {
    for (index, step) in crate::constants::plan_integer_constant(value, size)
        .iter()
        .enumerate()
    {
        if index != 0 {
            write!(ctx, "\n    ")?;
        }
        match step {
            crate::constants::ConstantStep::Zero => {
                write!(ctx, "mov ")?;
                emit_reg(ctx, dst, size)?;
                write!(ctx, ", ")?;
                emit_gpr(ctx, &Gpr::Zr, size)?;
            }
            crate::constants::ConstantStep::Logical(imm) => {
                write!(ctx, "orr ")?;
                emit_reg(ctx, dst, size)?;
                write!(ctx, ", ")?;
                emit_gpr(ctx, &Gpr::Zr, size)?;
                write!(ctx, ", #0x{:x}", imm.value())?;
            }
            crate::constants::ConstantStep::MovZ(imm)
            | crate::constants::ConstantStep::MovN(imm) => {
                write!(
                    ctx,
                    "{} ",
                    if matches!(step, crate::constants::ConstantStep::MovZ(_)) {
                        "movz"
                    } else {
                        "movn"
                    }
                )?;
                emit_reg(ctx, dst, size)?;
                write!(ctx, ", #0x{:x}", imm.bits())?;
                if imm.shift() != 0 {
                    write!(ctx, ", lsl #{}", imm.shift())?;
                }
            }
            crate::constants::ConstantStep::MovK(imm) => {
                write!(ctx, "movk ")?;
                emit_reg(ctx, dst, size)?;
                write!(ctx, ", #0x{:x}", imm.bits())?;
                if imm.shift() != 0 {
                    write!(ctx, ", lsl #{}", imm.shift())?;
                }
            }
        }
    }
    Ok(())
}
fn emit_rrr(
    ctx: &mut dyn EmitContext,
    op: &str,
    dst: Reg,
    lhs: &Reg,
    rhs: &Reg,
) -> core::fmt::Result {
    write!(ctx, "{op} ")?;
    ctx.write_reg(&dst)?;
    write!(ctx, ", ")?;
    ctx.write_reg(lhs)?;
    write!(ctx, ", ")?;
    ctx.write_reg(rhs)
}
fn emit_float_rr(ctx: &mut dyn EmitContext, op: &str, dst: Reg, src: &Reg) -> core::fmt::Result {
    write!(ctx, "{op} ")?;
    emit_float_reg(ctx, dst, false)?;
    write!(ctx, ", ")?;
    emit_float_reg(ctx, *src, false)
}
fn emit_fmov(ctx: &mut dyn EmitContext, dst: Reg, src: &Reg) -> core::fmt::Result {
    let dst_float = dst
        .to_real_reg()
        .is_none_or(|preg| preg.class() == RegClass::Float);
    let src_float = src
        .to_real_reg()
        .is_none_or(|preg| preg.class() == RegClass::Float);
    write!(ctx, "fmov ")?;
    if dst_float {
        emit_float_reg(ctx, dst, false)?;
    } else {
        emit_reg(ctx, dst, OperandSize::Size32)?;
    }
    write!(ctx, ", ")?;
    if src_float {
        emit_float_reg(ctx, *src, false)
    } else {
        emit_reg(ctx, *src, OperandSize::Size32)
    }
}
fn emit_float_rrr(
    ctx: &mut dyn EmitContext,
    op: &str,
    dst: Reg,
    lhs: &Reg,
    rhs: &Reg,
) -> core::fmt::Result {
    write!(ctx, "{op} ")?;
    emit_float_reg(ctx, dst, false)?;
    write!(ctx, ", ")?;
    emit_float_reg(ctx, *lhs, false)?;
    write!(ctx, ", ")?;
    emit_float_reg(ctx, *rhs, false)
}
fn emit_sized_rrr(
    ctx: &mut dyn EmitContext,
    op: &str,
    size: OperandSize,
    dst: Reg,
    lhs: &Reg,
    rhs: &Reg,
) -> core::fmt::Result {
    write!(ctx, "{op} ")?;
    emit_reg(ctx, dst, size)?;
    write!(ctx, ", ")?;
    emit_reg(ctx, *lhs, size)?;
    write!(ctx, ", ")?;
    emit_reg(ctx, *rhs, size)
}
fn emit_sized_data_rrr(
    ctx: &mut dyn EmitContext,
    op: &str,
    size: OperandSize,
    dst: Reg,
    lhs: &RegOrZr,
    rhs: &RegOrZr,
) -> core::fmt::Result {
    write!(ctx, "{op} ")?;
    emit_reg(ctx, dst, size)?;
    write!(ctx, ", ")?;
    emit_reg_or_zr(ctx, lhs, size)?;
    write!(ctx, ", ")?;
    emit_reg_or_zr(ctx, rhs, size)
}
fn emit_sized_rrrr(
    ctx: &mut dyn EmitContext,
    op: &str,
    size: OperandSize,
    dst: Reg,
    a: &Reg,
    b: &Reg,
    c: &Reg,
) -> core::fmt::Result {
    write!(ctx, "{op} ")?;
    emit_reg(ctx, dst, size)?;
    write!(ctx, ", ")?;
    emit_reg(ctx, *a, size)?;
    write!(ctx, ", ")?;
    emit_reg(ctx, *b, size)?;
    write!(ctx, ", ")?;
    emit_reg(ctx, *c, size)
}
fn emit_reg(ctx: &mut dyn EmitContext, reg: Reg, size: OperandSize) -> core::fmt::Result {
    match (reg.to_real_reg(), size) {
        (Some(preg), OperandSize::Size32) if preg.class() == RegClass::Int => {
            write!(ctx, "w{}", preg.hw_enc())
        }
        (Some(preg), _) if preg.class() == RegClass::Int => write!(ctx, "x{}", preg.hw_enc()),
        _ => ctx.write_reg(&reg),
    }
}
fn emit_data_reg(ctx: &mut dyn EmitContext, reg: Reg, ty: MemoryType) -> core::fmt::Result {
    if ty == MemoryType::I64 {
        emit_reg(ctx, reg, OperandSize::Size64)
    } else if ty == MemoryType::I32 {
        emit_reg(ctx, reg, OperandSize::Size32)
    } else {
        emit_float_reg(ctx, reg, ty == MemoryType::F64)
    }
}
fn emit_float_reg(ctx: &mut dyn EmitContext, reg: Reg, is_double: bool) -> core::fmt::Result {
    match reg.to_real_reg() {
        Some(preg) if preg.class() == RegClass::Float => {
            write!(
                ctx,
                "{}{}",
                if is_double { "d" } else { "s" },
                preg.hw_enc()
            )
        }
        _ => ctx.write_reg(&reg),
    }
}
fn emit_gpr(ctx: &mut dyn EmitContext, reg: &Gpr, size: OperandSize) -> core::fmt::Result {
    match reg {
        Gpr::Reg(reg) => emit_reg(ctx, *reg, size),
        Gpr::Sp => write!(ctx, "sp"),
        Gpr::Zr => write!(
            ctx,
            "{}zr",
            if size == OperandSize::Size32 {
                "w"
            } else {
                "x"
            }
        ),
    }
}
fn emit_reg_or_zr(
    ctx: &mut dyn EmitContext,
    reg: &RegOrZr,
    size: OperandSize,
) -> core::fmt::Result {
    match reg {
        RegOrZr::Reg(reg) => emit_reg(ctx, *reg, size),
        RegOrZr::Zr => emit_gpr(ctx, &Gpr::Zr, size),
    }
}
fn emit_amode(ctx: &mut dyn EmitContext, addr: &AMode) -> core::fmt::Result {
    match addr {
        AMode::Reg { base } => {
            write!(ctx, "[")?;
            emit_gpr(ctx, base, OperandSize::Size64)?;
            write!(ctx, "]")
        }
        AMode::UnsignedOffset { base, offset } => {
            write!(ctx, "[")?;
            emit_gpr(ctx, base, OperandSize::Size64)?;
            write!(ctx, ", #{}]", offset.byte_offset())
        }
        AMode::SignedOffset { base, offset } => {
            write!(ctx, "[")?;
            emit_gpr(ctx, base, OperandSize::Size64)?;
            write!(ctx, ", #{}]", offset.value())
        }
        AMode::RegOffset { base, index } => {
            write!(ctx, "[")?;
            emit_gpr(ctx, base, OperandSize::Size64)?;
            write!(ctx, ", ")?;
            emit_reg(ctx, *index, OperandSize::Size64)?;
            write!(ctx, "]")
        }
        AMode::ScaledRegOffset { base, index, shift } => {
            write!(ctx, "[")?;
            emit_gpr(ctx, base, OperandSize::Size64)?;
            write!(ctx, ", ")?;
            emit_reg(ctx, *index, OperandSize::Size64)?;
            write!(ctx, ", lsl #{shift}]")
        }
        AMode::ExtendedRegOffset {
            base,
            index,
            extend,
            shift,
        } => {
            write!(ctx, "[")?;
            emit_gpr(ctx, base, OperandSize::Size64)?;
            write!(ctx, ", ")?;
            emit_reg(
                ctx,
                *index,
                extend_source_size(*extend, OperandSize::Size64),
            )?;
            write!(ctx, ", {}", extend_name(*extend))?;
            if *shift != 0 {
                write!(ctx, " #{shift}")?;
            }
            write!(ctx, "]")
        }
        AMode::FrameSlot(offset) | AMode::SpOffset(offset) | AMode::OutgoingArg(offset) => {
            write!(ctx, "[sp, #{offset}]")
        }
        AMode::IncomingArg(offset) => write!(ctx, "[x29, #{offset}]"),
    }
}
fn emit_pair_amode(ctx: &mut dyn EmitContext, addr: &PairAMode) -> core::fmt::Result {
    match addr {
        PairAMode::SignedOffset { base, offset } => {
            write!(ctx, "[")?;
            emit_gpr(ctx, base, OperandSize::Size64)?;
            write!(ctx, ", #{}]", offset.byte_offset())
        }
        PairAMode::PreIndex { base, offset } => {
            write!(ctx, "[")?;
            emit_gpr(ctx, base, OperandSize::Size64)?;
            write!(ctx, ", #{}]!", offset.byte_offset())
        }
        PairAMode::PostIndex { base, offset } => {
            write!(ctx, "[")?;
            emit_gpr(ctx, base, OperandSize::Size64)?;
            write!(ctx, "], #{}", offset.byte_offset())
        }
    }
}
fn alu_name(op: AluOp, _size: OperandSize) -> &'static str {
    match op {
        AluOp::Add => "add",
        AluOp::Sub => "sub",
        AluOp::Mul => "mul",
        AluOp::And => "and",
        AluOp::Orr => "orr",
        AluOp::Eor => "eor",
        AluOp::Lsl => "lsl",
        AluOp::Lsr => "lsr",
        AluOp::Asr => "asr",
    }
}
fn shift_name(op: ShiftOp) -> &'static str {
    match op {
        ShiftOp::Lsl => "lsl",
        ShiftOp::Lsr => "lsr",
        ShiftOp::Asr => "asr",
        ShiftOp::Ror => "ror",
    }
}
fn extend_name(op: ExtendOp) -> &'static str {
    match op {
        ExtendOp::Uxtb => "uxtb",
        ExtendOp::Uxth => "uxth",
        ExtendOp::Uxtw => "uxtw",
        ExtendOp::Uxtx => "uxtx",
        ExtendOp::Sxtb => "sxtb",
        ExtendOp::Sxth => "sxth",
        ExtendOp::Sxtw => "sxtw",
        ExtendOp::Sxtx => "sxtx",
    }
}
fn extend_source_size(extend: ExtendOp, size: OperandSize) -> OperandSize {
    match extend {
        ExtendOp::Uxtb
        | ExtendOp::Uxth
        | ExtendOp::Uxtw
        | ExtendOp::Sxtb
        | ExtendOp::Sxth
        | ExtendOp::Sxtw => OperandSize::Size32,
        ExtendOp::Uxtx | ExtendOp::Sxtx => size,
    }
}
fn shifted_alu_is_legal(op: AluOp, shift: ShiftOp) -> bool {
    match op {
        AluOp::Add | AluOp::Sub => matches!(shift, ShiftOp::Lsl | ShiftOp::Lsr | ShiftOp::Asr),
        AluOp::And | AluOp::Orr | AluOp::Eor => {
            matches!(
                shift,
                ShiftOp::Lsl | ShiftOp::Lsr | ShiftOp::Asr | ShiftOp::Ror
            )
        }
        _ => false,
    }
}
fn extended_alu_is_legal(op: AluOp, size: OperandSize, extend: ExtendOp, shift: u8) -> bool {
    matches!(op, AluOp::Add | AluOp::Sub)
        && shift <= 4
        && match size {
            OperandSize::Size32 => matches!(
                extend,
                ExtendOp::Uxtb | ExtendOp::Uxth | ExtendOp::Sxtb | ExtendOp::Sxth
            ),
            OperandSize::Size64 => true,
        }
}
fn amode_is_legal(addr: &AMode, ty: MemoryType) -> bool {
    match addr {
        AMode::Reg { base }
        | AMode::UnsignedOffset { base, .. }
        | AMode::SignedOffset { base, .. }
        | AMode::RegOffset { base, .. } => !matches!(base, Gpr::Zr),
        AMode::ScaledRegOffset { base, shift, .. } => {
            !matches!(base, Gpr::Zr) && *shift == ty.byte_size().trailing_zeros() as u8
        }
        AMode::ExtendedRegOffset {
            base,
            extend,
            shift,
            ..
        } => {
            !matches!(base, Gpr::Zr)
                && matches!(
                    extend,
                    ExtendOp::Uxtw | ExtendOp::Sxtw | ExtendOp::Uxtx | ExtendOp::Sxtx
                )
                && (*shift == 0 || *shift == ty.byte_size().trailing_zeros() as u8)
        }
        _ => true,
    }
}
fn pair_amode_is_legal(addr: &PairAMode) -> bool {
    let base = match addr {
        PairAMode::SignedOffset { base, .. }
        | PairAMode::PreIndex { base, .. }
        | PairAMode::PostIndex { base, .. } => base,
    };
    !matches!(base, Gpr::Zr)
}
fn cond_name(cond: Cond) -> &'static str {
    match cond {
        Cond::Eq => "eq",
        Cond::Ne => "ne",
        Cond::Hs => "hs",
        Cond::Lo => "lo",
        Cond::Mi => "mi",
        Cond::Pl => "pl",
        Cond::Vs => "vs",
        Cond::Vc => "vc",
        Cond::Hi => "hi",
        Cond::Ls => "ls",
        Cond::Ge => "ge",
        Cond::Lt => "lt",
        Cond::Gt => "gt",
        Cond::Le => "le",
    }
}
fn fpu_name(op: FpuOp) -> &'static str {
    match op {
        FpuOp::Add => "fadd",
        FpuOp::Sub => "fsub",
        FpuOp::Mul => "fmul",
        FpuOp::Div => "fdiv",
    }
}
fn load_name(ty: MemoryType) -> &'static str {
    match ty {
        MemoryType::I32 | MemoryType::I64 | MemoryType::F32 | MemoryType::F64 => "ldr",
    }
}
fn store_name(ty: MemoryType) -> &'static str {
    match ty {
        MemoryType::I32 | MemoryType::I64 | MemoryType::F32 | MemoryType::F64 => "str",
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
