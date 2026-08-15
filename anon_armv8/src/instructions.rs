//! Typed AArch64 instruction forms and encoding-valid operands.

use taki_mir::{
    abi::{ArgPair, CallArgPair, CallRetPair, RetPair, StackAMode},
    emit_buffer::LabelKind,
    reg_alloc::reg::{OperandVisitor, OperandVisitorImpl, PRegSet, RegClass},
    register::{Reg, Writable},
    types::{F32, I32, I64, LoweredType, V2F64, V2I64, V4F32, V4I32},
    vcode::{EmitContext, MachInst, MachInstEmit, MachTerminator},
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

    /// Compute an [`Imm12`] from a raw value, using the unshifted form for
    /// `0..=0xfff` and the `lsl #12` form for multiples of 4096 up to
    /// `0xfff000`. Mirrors cranelift's `Imm12::maybe_from_u64`.
    pub fn maybe_from_u64(val: u64) -> Option<Self> {
        if val & !0xfff == 0 {
            Some(Self {
                value: val as u16,
                shift12: false,
            })
        } else if val & !(0xfff << 12) == 0 {
            Some(Self {
                value: (val >> 12) as u16,
                shift12: true,
            })
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
        let access_size = access_size as u64;
        // Scaled unsigned offsets cover 32/64-bit scalars and 128-bit vectors
        // (`ldr q`, scale 16); `SImm7Scaled` likewise for pair forms.
        if !matches!(access_size, 4 | 8 | 16) || value % access_size != 0 {
            return None;
        }
        let scaled = value / access_size;
        if scaled <= 0xfff {
            Some(Self {
                value: scaled as u16,
                access_size: access_size as u8,
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
        let access_size = access_size as i64;
        if !matches!(access_size, 4 | 8 | 16) || value % access_size != 0 {
            return None;
        }
        let scaled = value / access_size;
        if scaled >= -64 && scaled <= 63 {
            Some(Self {
                value: scaled as i8,
                access_size: access_size as u8,
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
    /// 128-bit vector value (`ldr q` / `str q`).
    Vec128,
}

impl MemoryType {
    pub const fn byte_size(self) -> u8 {
        match self {
            Self::I32 | Self::F32 => 4,
            Self::I64 | Self::F64 => 8,
            Self::Vec128 => 16,
        }
    }

    pub const fn is_float(self) -> bool {
        matches!(self, Self::F32 | Self::F64)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AMode {
    Reg {
        base: Reg,
    },
    UnsignedOffset {
        base: Reg,
        offset: UImm12Scaled,
    },
    SignedOffset {
        base: Reg,
        offset: SImm9,
    },
    RegOffset {
        base: Reg,
        index: Reg,
    },
    ScaledRegOffset {
        base: Reg,
        index: Reg,
        shift: u8,
    },
    ExtendedRegOffset {
        base: Reg,
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
    SignedOffset { base: Reg, offset: SImm7Scaled },
    PreIndex { base: Reg, offset: SImm7Scaled },
    PostIndex { base: Reg, offset: SImm7Scaled },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AluOp {
    Add,
    Sub,
    Mul,
    And,
    Orr,
    Orn,
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

/// NEON 128-bit vector arrangement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VecShape {
    /// 4 x 32-bit lanes (`v.d.4s`).
    FourS,
    /// 2 x 64-bit lanes (`v.d.2d`).
    TwoD,
}

impl VecShape {
    pub const fn arrangement(self) -> &'static str {
        match self {
            Self::FourS => "4s",
            Self::TwoD => "2d",
        }
    }

    pub const fn element_bytes(self) -> u8 {
        match self {
            Self::FourS => 4,
            Self::TwoD => 8,
        }
    }

    pub const fn lanes(self) -> u8 {
        match self {
            Self::FourS => 4,
            Self::TwoD => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VecArithOp {
    Add,
    Sub,
    Mul,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VecBitOp {
    And,
    Orr,
    Eor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VecCmpOp {
    Eq,
    Gt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VecCvtOp {
    Scvtf,
    Fcvtzs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VecMinMaxOp {
    Smin,
    Smax,
    Umin,
    Umax,
    Fmin,
    Fmax,
}

/// The flag-producing half of an atomic conditional-select pseudo.
///
/// Keeping this in the same [`MInst`] as the flag consumer is intentional:
/// NZCV is implicit state and is not represented by the register allocator.
#[derive(Clone, Debug)]
pub enum SelectCmp {
    IntRR {
        size: OperandSize,
        lhs: Reg,
        rhs: RegOrZr,
    },
    IntImm {
        size: OperandSize,
        lhs: Reg,
        imm: Imm12,
    },
    Float {
        lhs: Reg,
        rhs: Reg,
    },
}

/// The flag-consuming half of an atomic conditional-select pseudo.
#[derive(Clone, Debug)]
pub enum SelectValue {
    Int {
        size: OperandSize,
        dst: WritableReg,
        if_true: Reg,
        if_false: Reg,
    },
    Float {
        dst: WritableReg,
        if_true: Reg,
        if_false: Reg,
    },
    /// Materialize the selected boolean using `cset`.
    Bool { dst: WritableReg },
}

/// A chained conditional-compare step between a [`CmpSelect`]'s first
/// comparison and its select, or between a comparison and a conditional
/// branch. Semantics: if `cond` holds (based on the preceding NZCV), compare
/// `lhs` with `rhs`/`imm` and set NZCV from the result; otherwise set NZCV to
/// `nzcv`. With the right nzcv value this folds `band`/`bor` of two
/// comparisons into a single flag chain (clang's `ccmp` pattern).
#[derive(Clone, Debug)]
pub struct CCmpStep {
    pub size: OperandSize,
    pub lhs: Reg,
    pub rhs: RegOrZr,
    pub imm: Option<Imm12>,
    pub nzcv: u8,
    pub cond: Cond,
}

#[derive(Clone, Debug)]
pub enum MInst {
    Nop,
    /// Tombstone left by a MIR pass that consumed the original instruction.
    /// Emits nothing; the emitter must skip it.
    Removed,
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
        src: Reg,
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
        lhs: Reg,
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
    /// `smull xd, wn, wm`: the full 64-bit product of two 32-bit signed
    /// operands. Division by a constant needs the high half of the product,
    /// which a 32-bit multiply discards.
    SMulL {
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
    /// `subs dst, src, #imm`: subtract and set flags. Produced by fusing a
    /// `sub` with a following `cmp dst, #0` (count-down loop tests).
    SubsRRImm12 {
        size: OperandSize,
        dst: WritableReg,
        src: Reg,
        imm: Imm12,
    },
    /// `ands dst, src, #imm`: bitwise-and and set flags. Produced by fusing
    /// a live `and` with a following `cmp dst, #0`.
    AndsRRImmLogic {
        size: OperandSize,
        dst: WritableReg,
        src: RegOrZr,
        imm: ImmLogic,
    },
    /// `tst src, #imm`: set flags from `src & imm` without writing a result.
    /// Produced when the fused `and` result is dead.
    TstRRImmLogic {
        size: OperandSize,
        src: RegOrZr,
        imm: ImmLogic,
    },
    Mov {
        size: OperandSize,
        dst: WritableReg,
        src: Reg,
    },
    MovPhys {
        size: OperandSize,
        dst: WritableReg,
        src: Reg,
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
    /// 32-bit source sign-extended into a 64-bit destination (`sxtw xd, wm`).
    Sxtw {
        size: OperandSize,
        dst: WritableReg,
        src: Reg,
    },
    LoadAddr {
        dst: WritableReg,
        label: Label,
    },
    StackAddr {
        dst: WritableReg,
        addr: AMode,
    },
    BCond {
        cond: Cond,
        label: Label,
    },
    Cbz {
        size: OperandSize,
        reg: Reg,
        true_label: Label,
        false_label: Label,
    },
    Cbnz {
        size: OperandSize,
        reg: Reg,
        true_label: Label,
        false_label: Label,
    },
    Tbz {
        size: OperandSize,
        reg: Reg,
        bit: u8,
        true_label: Label,
        false_label: Label,
    },
    Tbnz {
        size: OperandSize,
        reg: Reg,
        bit: u8,
        true_label: Label,
        false_label: Label,
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
    /// Emits an adjacent comparison and `csel`, `fcsel`, or `cset` pair.
    /// This is atomic at the machine-instruction level because NZCV is not an
    /// allocatable value and must not be separated from its consumer. The
    /// optional `ccmp` chain step folds a `band`/`bor` of two comparisons.
    CmpSelect {
        cmp: SelectCmp,
        ccmp: Option<Box<CCmpStep>>,
        cond: Cond,
        value: SelectValue,
    },
    /// A standalone conditional compare: `ccmp lhs, rhs, #nzcv, cond`.
    /// Emitted between a comparison and a `CondBr` when the branch condition
    /// is a `band`/`bor` of two comparisons.
    CCmp {
        size: OperandSize,
        lhs: Reg,
        rhs: RegOrZr,
        imm: Option<Imm12>,
        nzcv: u8,
        cond: Cond,
    },
    FMov {
        dst: WritableReg,
        src: Reg,
    },
    /// 128-bit vector register copy: `mov v{d}.16b, v{s}.16b`.
    VecMov {
        dst: WritableReg,
        src: Reg,
    },
    /// 128-bit vector load: `ld1 {v{d}.16b}, [base]`.
    VecLd1 {
        dst: WritableReg,
        base: Reg,
    },
    /// 128-bit vector store: `st1 {v{s}.16b}, [base]`.
    VecSt1 {
        src: Reg,
        base: Reg,
    },
    /// `dup v{d}.<shape>, <scalar>`: replicate a scalar across all lanes.
    VecDup {
        shape: VecShape,
        dst: WritableReg,
        src: Reg,
    },
    /// Vector add/sub/mul: `{op} v{d}.<shape>, v{lhs}.<shape>, v{rhs}.<shape>`.
    VecArithRRR {
        op: VecArithOp,
        shape: VecShape,
        dst: WritableReg,
        lhs: Reg,
        rhs: Reg,
    },
    /// `fmla v{d}.<shape>, v{lhs}.<shape>, v{rhs}.<shape>` with the accumulator
    /// copied in first: `mov v{d}.16b, v{acc}.16b; fmla v{d}.<shape>, v{lhs}.<shape>, v{rhs}.<shape>`.
    /// `acc` is an explicit SSA read (read-modify-write on `dst`).
    VecFmla {
        shape: VecShape,
        dst: WritableReg,
        acc: Reg,
        lhs: Reg,
        rhs: Reg,
    },
    /// Vector bitwise and/orr/eor: `{op} v{d}.16b, v{lhs}.16b, v{rhs}.16b`.
    VecBitwise {
        op: VecBitOp,
        dst: WritableReg,
        lhs: Reg,
        rhs: Reg,
    },
    /// Vector compare: `cmeq/cmgt v{d}.<shape>, v{lhs}.<shape>, v{rhs}.<shape>`.
    VecCmp {
        op: VecCmpOp,
        shape: VecShape,
        dst: WritableReg,
        lhs: Reg,
        rhs: Reg,
    },
    /// `bsl v{d}.16b, v{lhs}.16b, v{rhs}.16b` with the mask copied in first:
    /// `mov v{d}.16b, v{mask}.16b; bsl v{d}.16b, v{lhs}.16b, v{rhs}.16b`.
    /// `mask` is an explicit SSA read (read-modify-write on `dst`).
    VecBsl {
        dst: WritableReg,
        mask: Reg,
        lhs: Reg,
        rhs: Reg,
    },
    /// Vector int<->float convert: `scvtf/fcvtzs v{d}.<shape>, v{s}.<shape>`.
    VecCvt {
        op: VecCvtOp,
        shape: VecShape,
        dst: WritableReg,
        src: Reg,
    },
    /// Horizontal reduction: `addv s{d}, v{s}.4s`.
    VecAddv {
        dst: WritableReg,
        src: Reg,
    },
    /// `movi v{d}.<shape>, #imm` (optionally `, lsl #shift`): materialize a
    /// vector with every lane set from an 8-bit immediate, zero-extended to
    /// the lane width and shifted to a byte position within each lane.
    VecMovImm {
        shape: VecShape,
        dst: WritableReg,
        imm: u8,
        shift: u8,
    },
    /// `mov w/x{d}, v{s}.s/d[lane]`: extract one lane to a general-purpose
    /// register.
    VecExtractLane {
        size: OperandSize,
        dst: WritableReg,
        src: Reg,
        lane: u8,
    },
    /// `mov v{d}.s/d[lane], w/x{src}`: insert a general-purpose value into
    /// one lane. The destination vector is copied in first:
    /// `mov v{d}.16b, v{vector}.16b; mov v{d}.s/d[lane], w/x{src}`.
    /// `vector` is an explicit SSA read (read-modify-write on `dst`).
    VecInsertLane {
        size: OperandSize,
        dst: WritableReg,
        vector: Reg,
        src: Reg,
        lane: u8,
    },
    /// Vector min/max: `smin/smax/umin/umax/fmin/fmax`.
    VecMinMax {
        op: VecMinMaxOp,
        shape: VecShape,
        dst: WritableReg,
        lhs: Reg,
        rhs: Reg,
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
    Call {
        args: Vec<CallArgPair>,
        ret: Option<CallRetPair>,
        clobbers: PRegSet,
        label: Label,
    },
    /// A tail transfer with arguments forced into the ABI registers. A
    /// function target restores the current frame before jumping; a local
    /// target keeps it and is used for self-tail-recursion loops.
    TailCall {
        args: Vec<CallArgPair>,
        clobbers: PRegSet,
        label: Label,
    },
    RetVal {
        pair: RetPair,
    },
    /// Bind incoming register parameters to their fixed ABI physical
    /// registers. Emits no machine code; register allocation resolves the
    /// fixed defs. Must be the first instruction of the entry block.
    Args {
        args: Vec<ArgPair>,
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
                extend,
                shift,
                ..
            } if !extended_alu_is_legal(*op, *size, *extend, *shift) => {
                Err("invalid AArch64 extended-register ALU form")
            }
            Self::Load { ty, addr, .. } | Self::Store { ty, addr, .. }
                if !amode_is_legal(addr, *ty) =>
            {
                Err("invalid AArch64 memory address form")
            }
            Self::LoadPair { addr, .. } | Self::StorePair { addr, .. }
                if !pair_amode_is_legal(addr) =>
            {
                Err("invalid AArch64 pair memory address form")
            }
            Self::Tbz { size, bit, .. } | Self::Tbnz { size, bit, .. } if *bit >= size.bits() => {
                Err("test-bit index exceeds AArch64 encoding range")
            }
            Self::CCmp { nzcv, .. } if *nzcv > 0xF => Err("ccmp NZCV immediate exceeds 4 bits"),
            Self::LoadPair { ty, addr, .. } | Self::StorePair { ty, addr, .. }
                if pair_access_size(addr) != ty.byte_size() =>
            {
                Err("pair address offset scaling does not match memory width")
            }
            Self::Args { args } => {
                for pair in args {
                    if pair.preg.class() != pair.vreg.to_reg().class() {
                        return Err("Args fixed def register class mismatch");
                    }
                }
                Ok(())
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
    fn verify(&self) -> Result<(), String> {
        MInst::verify(self).map_err(str::to_owned)
    }

    fn needs_epilogue(&self) -> bool {
        match self {
            Self::Ret => true,
            Self::TailCall { label, .. } => label.block().is_none(),
            _ => false,
        }
    }

    type ABISpec = AArch64Abi;

    fn get_operands(&mut self, collector: &mut impl OperandVisitor) {
        match self {
            Self::Nop | Self::Removed | Self::BCond { .. } | Self::Jump { .. } | Self::Ret => {}
            Self::AluRRR { dst, lhs, rhs, .. } => {
                use_reg_or_zr(collector, lhs);
                use_reg_or_zr(collector, rhs);
                collector.reg_def(dst);
            }
            Self::SDiv { dst, lhs, rhs, .. }
            | Self::SMulL { dst, lhs, rhs }
            | Self::FAlu { dst, lhs, rhs, .. } => {
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
                use_sp_aware_reg(collector, src);
                def_sp_aware_reg(collector, dst);
            }
            Self::AluRRImmLogic { dst, src, .. } => {
                use_reg_or_zr(collector, src);
                collector.reg_def(dst);
            }
            Self::AluRRImmShift { dst, src, .. }
            | Self::Mov { dst, src, .. }
            | Self::FMov { dst, src }
            | Self::VecMov { dst, src }
            | Self::VecDup { dst, src, .. }
            | Self::VecCvt { dst, src, .. }
            | Self::VecAddv { dst, src }
            | Self::Scvtf { dst, src }
            | Self::Fcvtzs { dst, src } => {
                collector.reg_use(src);
                collector.reg_def(dst);
            }
            Self::VecLd1 { dst, base } => {
                use_sp_aware_reg(collector, base);
                collector.reg_def(dst);
            }
            Self::VecSt1 { src, base } => {
                collector.reg_use(src);
                use_sp_aware_reg(collector, base);
            }
            Self::VecArithRRR { dst, lhs, rhs, .. }
            | Self::VecBitwise { dst, lhs, rhs, .. }
            | Self::VecCmp { dst, lhs, rhs, .. }
            | Self::VecMinMax { dst, lhs, rhs, .. } => {
                collector.reg_use(lhs);
                collector.reg_use(rhs);
                collector.reg_def(dst);
            }
            Self::VecFmla {
                dst, acc, lhs, rhs, ..
            }
            | Self::VecBsl {
                dst,
                mask: acc,
                lhs,
                rhs,
            } => {
                // The leading `mov` writes `dst` before the read-modify-write
                // reads `lhs`/`rhs`, so `dst` is an *early* def: it must not
                // alias any use (coalescing dst with rhs would clobber rhs).
                collector.reg_use(acc);
                collector.reg_use(lhs);
                collector.reg_use(rhs);
                collector.reg_early_def(dst);
            }
            Self::VecMovImm { dst, .. } => collector.reg_def(dst),
            Self::VecExtractLane { dst, src, .. } => {
                collector.reg_use(src);
                collector.reg_def(dst);
            }
            Self::VecInsertLane {
                dst, vector, src, ..
            } => {
                // The leading copy writes `dst` before `src` is read by the
                // lane insert, so `dst` must not alias either use.
                collector.reg_use(vector);
                collector.reg_use(src);
                collector.reg_early_def(dst);
            }
            Self::AluRRRShift { dst, lhs, rhs, .. } => {
                use_reg_or_zr(collector, lhs);
                use_reg_or_zr(collector, rhs);
                collector.reg_def(dst);
            }
            Self::AluRRRExtend { dst, lhs, rhs, .. } => {
                use_sp_aware_reg(collector, lhs);
                collector.reg_use(rhs);
                def_sp_aware_reg(collector, dst);
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
            Self::SubsRRImm12 { dst, src, .. } => {
                use_sp_aware_reg(collector, src);
                def_sp_aware_reg(collector, dst);
            }
            Self::AndsRRImmLogic { dst, src, .. } => {
                use_reg_or_zr(collector, src);
                collector.reg_def(dst);
            }
            Self::TstRRImmLogic { src, .. } => use_reg_or_zr(collector, src),
            Self::MovPhys { dst, src, .. } => {
                use_sp_aware_reg(collector, src);
                def_sp_aware_reg(collector, dst);
            }
            Self::Sxtw { size, dst, src } => {
                let _ = size;
                collector.reg_use(src);
                collector.reg_def(dst);
            }
            Self::LoadImm { dst, .. }
            | Self::MovZ { dst, .. }
            | Self::MovN { dst, .. }
            | Self::MovFromZero { dst, .. }
            | Self::FMovFromZero { dst }
            | Self::LoadAddr { dst, .. }
            | Self::StackAddr { dst, .. }
            | Self::CSet { dst, .. } => collector.reg_def(dst),
            Self::CmpSelect {
                cmp, ccmp, value, ..
            } => {
                match cmp {
                    SelectCmp::IntRR { lhs, rhs, .. } => {
                        collector.reg_use(lhs);
                        use_reg_or_zr(collector, rhs);
                    }
                    SelectCmp::IntImm { lhs, .. } => collector.reg_use(lhs),
                    SelectCmp::Float { lhs, rhs } => {
                        collector.reg_use(lhs);
                        collector.reg_use(rhs);
                    }
                }
                if let Some(ccmp) = ccmp {
                    collector.reg_use(&mut ccmp.lhs);
                    use_reg_or_zr(collector, &mut ccmp.rhs);
                }
                match value {
                    SelectValue::Int {
                        dst,
                        if_true,
                        if_false,
                        ..
                    }
                    | SelectValue::Float {
                        dst,
                        if_true,
                        if_false,
                    } => {
                        collector.reg_use(if_true);
                        collector.reg_use(if_false);
                        collector.reg_def(dst);
                    }
                    SelectValue::Bool { dst } => collector.reg_def(dst),
                }
            }
            Self::CCmp { lhs, rhs, imm, .. } => {
                collector.reg_use(lhs);
                use_reg_or_zr(collector, rhs);
                let _ = imm;
            }
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
                use_store_src(collector, src);
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
                collector.reg_clobbers(call_clobbers(*clobbers, ret.as_ref()));
            }
            Self::RetVal { pair } => collector.reg_fixed_use(&mut pair.vreg, pair.preg),
            Self::Args { args } => {
                for pair in args {
                    collector.reg_fixed_def(&mut pair.vreg, pair.preg);
                }
            }
            Self::TailCall { args, clobbers, .. } => {
                for pair in args {
                    collector.reg_fixed_use(&mut pair.vreg, pair.preg);
                }
                collector.reg_clobbers(*clobbers);
            }
            Self::CondBr { .. } => {}
        }
    }

    fn is_move(&self) -> Option<(Writable<Reg>, Reg)> {
        match self {
            Self::Mov { dst, src, .. } | Self::FMov { dst, src } | Self::VecMov { dst, src } => {
                Some((*dst, *src))
            }
            _ => None,
        }
    }
    fn is_term(&self) -> MachTerminator {
        match self {
            Self::Ret | Self::TailCall { .. } => MachTerminator::Return,
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
    fn rc_for_type(ty: LoweredType) -> (&'static [RegClass], &'static [LoweredType]) {
        match ty {
            I32 => (&[RegClass::Int], &[I32]),
            I64 => (&[RegClass::Int], &[I64]),
            F32 => (&[RegClass::Float], &[F32]),
            V4I32 => (&[RegClass::Vector], &[V4I32]),
            V2I64 => (&[RegClass::Vector], &[V2I64]),
            V4F32 => (&[RegClass::Vector], &[V4F32]),
            V2F64 => (&[RegClass::Vector], &[V2F64]),
            _ => unreachable!("unsupported AArch64 lowered type"),
        }
    }
    fn gen_jump(target: taki_mir::block_order::MirBlockIndex) -> Self {
        Self::Jump {
            label: Label::from_block(target),
        }
    }
}

fn call_clobbers(mut clobbers: PRegSet, ret: Option<&CallRetPair>) -> PRegSet {
    clobbers.union_from(crate::regs::DEFAULT_CLOBBERS);
    if let Some(ret) = ret {
        clobbers.remove(ret.preg.to_real_reg().unwrap());
    }
    clobbers
}

fn use_reg_or_zr(collector: &mut impl OperandVisitor, reg: &mut RegOrZr) {
    if let RegOrZr::Reg(reg) = reg {
        collector.reg_use(reg);
    }
}
fn use_store_src(collector: &mut impl OperandVisitor, reg: &mut Reg) {
    if reg.is_virtual() {
        collector.reg_use(reg);
    }
}
/// Like [`OperandVisitor::reg_use`] but silently skips the stack pointer,
/// which is a fixed architectural register that does not participate in
/// allocation.
fn use_sp_aware_reg(collector: &mut impl OperandVisitor, reg: &mut Reg) {
    if *reg != crate::regs::stack_reg() {
        collector.reg_use(reg);
    }
}
/// Like [`OperandVisitor::reg_def`] but silently skips the stack pointer.
fn def_sp_aware_reg(collector: &mut impl OperandVisitor, reg: &mut WritableReg) {
    if reg.to_reg() != crate::regs::stack_reg() {
        collector.reg_def(reg);
    }
}
fn visit_amode(collector: &mut impl OperandVisitor, addr: &mut AMode) {
    match addr {
        AMode::Reg { base }
        | AMode::UnsignedOffset { base, .. }
        | AMode::SignedOffset { base, .. } => use_sp_aware_reg(collector, base),
        AMode::RegOffset { base, index }
        | AMode::ScaledRegOffset { base, index, .. }
        | AMode::ExtendedRegOffset { base, index, .. } => {
            use_sp_aware_reg(collector, base);
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
        | PairAMode::PostIndex { base, .. } => use_sp_aware_reg(collector, base),
    }
}

impl MachInstEmit for MInst {
    fn emit(&self, ctx: &mut dyn EmitContext) -> core::fmt::Result {
        match self {
            Self::Nop => write!(ctx, "nop"),
            Self::Removed => Ok(()),
            Self::AluRRR {
                op,
                size,
                dst,
                lhs,
                rhs,
            } => emit_sized_data_rrr(ctx, alu_name(*op), *size, dst.to_reg(), lhs, rhs),
            Self::AluRRRR {
                op,
                size,
                dst,
                lhs,
                rhs,
                carry,
            } => emit_sized_rrrr(ctx, alu_name(*op), *size, dst.to_reg(), lhs, rhs, carry),
            Self::AluRRImm12 {
                op,
                size,
                dst,
                src,
                imm,
            } => {
                write!(ctx, "{} ", alu_name(*op))?;
                emit_reg(ctx, dst.to_reg(), *size)?;
                write!(ctx, ", ")?;
                emit_reg(ctx, *src, *size)?;
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
                write!(ctx, "{} ", alu_name(*op))?;
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
                write!(ctx, "{} ", alu_name(*op))?;
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
                write!(ctx, "{} ", alu_name(*op))?;
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
                write!(ctx, "{} ", alu_name(*op))?;
                emit_reg(ctx, dst.to_reg(), *size)?;
                write!(ctx, ", ")?;
                emit_reg(ctx, *lhs, *size)?;
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
            } => emit_sized_rrr(ctx, "sdiv", *size, dst.to_reg(), lhs, rhs),
            Self::SMulL { dst, lhs, rhs } => {
                write!(ctx, "smull ")?;
                emit_reg(ctx, dst.to_reg(), OperandSize::Size64)?;
                write!(ctx, ", ")?;
                emit_reg(ctx, *lhs, OperandSize::Size32)?;
                write!(ctx, ", ")?;
                emit_reg(ctx, *rhs, OperandSize::Size32)
            }
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
            Self::SubsRRImm12 {
                size,
                dst,
                src,
                imm,
            } => {
                write!(ctx, "subs ")?;
                emit_reg(ctx, dst.to_reg(), *size)?;
                write!(ctx, ", ")?;
                emit_reg(ctx, *src, *size)?;
                write!(ctx, ", #{}", imm.value())?;
                if imm.shift12() {
                    write!(ctx, ", lsl #12")?;
                }
                Ok(())
            }
            Self::AndsRRImmLogic {
                size,
                dst,
                src,
                imm,
            } => {
                write!(ctx, "ands ")?;
                emit_reg(ctx, dst.to_reg(), *size)?;
                write!(ctx, ", ")?;
                emit_reg_or_zr(ctx, src, *size)?;
                write!(ctx, ", #0x{:x}", imm.value())
            }
            Self::TstRRImmLogic { size, src, imm } => {
                write!(ctx, "tst ")?;
                emit_reg_or_zr(ctx, src, *size)?;
                write!(ctx, ", #0x{:x}", imm.value())
            }
            Self::Mov { size, dst, src } => {
                write!(ctx, "mov ")?;
                emit_reg(ctx, dst.to_reg(), *size)?;
                write!(ctx, ", ")?;
                emit_reg(ctx, *src, *size)
            }
            Self::MovPhys { size, dst, src } => {
                write!(ctx, "mov ")?;
                emit_reg(ctx, dst.to_reg(), *size)?;
                write!(ctx, ", ")?;
                emit_reg(ctx, *src, *size)
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
            Self::Sxtw { size, dst, src } => {
                write!(ctx, "sxtw ")?;
                emit_reg(ctx, dst.to_reg(), OperandSize::Size64)?;
                write!(ctx, ", ")?;
                emit_reg(ctx, *src, *size)
            }
            Self::LoadAddr { dst, label } => {
                write!(ctx, "adrp ")?;
                emit_reg(ctx, dst.to_reg(), OperandSize::Size64)?;
                write!(ctx, ", ")?;
                label.emit(ctx)?;
                ctx.end_inst()?;
                write!(ctx, "add ")?;
                emit_reg(ctx, dst.to_reg(), OperandSize::Size64)?;
                write!(ctx, ", ")?;
                emit_reg(ctx, dst.to_reg(), OperandSize::Size64)?;
                write!(ctx, ", :lo12:")?;
                label.emit(ctx)
            }
            Self::StackAddr { .. } => {
                unreachable!("stack addresses must be legalized before emission")
            }
            Self::BCond { cond, label } => {
                let target = label
                    .block()
                    .expect("BCond target must be an intra-function block");
                let cond_text = cond_name(*cond);
                let inverted_text = cond_name(invert_cond(*cond));
                ctx.put_branch(
                    &format!("b.{cond_text} "),
                    Some(&format!("b.{inverted_text} ")),
                    target,
                    LabelKind::BRANCH19,
                )
            }
            Self::Cbz {
                size,
                reg,
                true_label,
                false_label,
            }
            | Self::Cbnz {
                size,
                reg,
                true_label,
                false_label,
            } => {
                let (mnemonic, inverted_mnemonic) = match self {
                    Self::Cbz { .. } => ("cbz", "cbnz"),
                    _ => ("cbnz", "cbz"),
                };
                let true_target = true_label
                    .block()
                    .expect("Cbz/Cbnz target must be an intra-function block");
                let false_target = false_label
                    .block()
                    .expect("Cbz/Cbnz target must be an intra-function block");
                let prefix = branch_prefix(ctx, mnemonic, *reg, *size)?;
                let inv_prefix = branch_prefix(ctx, inverted_mnemonic, *reg, *size)?;
                ctx.put_branch(&prefix, Some(&inv_prefix), true_target, LabelKind::BRANCH19)?;
                ctx.put_uncond_branch("b ", false_target, LabelKind::BRANCH26)
            }
            Self::Tbz {
                size,
                reg,
                bit,
                true_label,
                false_label,
            }
            | Self::Tbnz {
                size,
                reg,
                bit,
                true_label,
                false_label,
            } => {
                let (mnemonic, inverted_mnemonic) = match self {
                    Self::Tbz { .. } => ("tbz", "tbnz"),
                    _ => ("tbnz", "tbz"),
                };
                let true_target = true_label
                    .block()
                    .expect("Tbz/Tbnz target must be an intra-function block");
                let false_target = false_label
                    .block()
                    .expect("Tbz/Tbnz target must be an intra-function block");
                let prefix = branch_prefix_bit(ctx, mnemonic, *reg, *size, *bit)?;
                let inv_prefix = branch_prefix_bit(ctx, inverted_mnemonic, *reg, *size, *bit)?;
                ctx.put_branch(&prefix, Some(&inv_prefix), true_target, LabelKind::BRANCH14)?;
                ctx.put_uncond_branch("b ", false_target, LabelKind::BRANCH26)
            }
            Self::CondBr {
                cond,
                true_label,
                false_label,
            } => {
                let true_target = true_label
                    .block()
                    .expect("CondBr target must be an intra-function block");
                let false_target = false_label
                    .block()
                    .expect("CondBr target must be an intra-function block");
                let cond_text = cond_name(*cond);
                let inverted_text = cond_name(invert_cond(*cond));
                ctx.put_branch(
                    &format!("b.{cond_text} "),
                    Some(&format!("b.{inverted_text} ")),
                    true_target,
                    LabelKind::BRANCH19,
                )?;
                ctx.put_uncond_branch("b ", false_target, LabelKind::BRANCH26)
            }
            Self::Jump { label } => {
                let target = label
                    .block()
                    .expect("Jump target must be an intra-function block");
                ctx.put_uncond_branch("b ", target, LabelKind::BRANCH26)
            }
            Self::CSet { cond, dst } => {
                write!(ctx, "cset ")?;
                emit_reg(ctx, dst.to_reg(), OperandSize::Size32)?;
                write!(ctx, ", {}", cond_name(*cond))
            }
            Self::CCmp {
                size,
                lhs,
                rhs,
                imm,
                nzcv,
                cond,
            } => emit_ccmp(ctx, *size, *lhs, rhs, *imm, *nzcv, *cond),
            Self::CmpSelect {
                cmp,
                ccmp,
                cond,
                value,
            } => {
                emit_select_cmp(ctx, cmp)?;
                ctx.end_inst()?;
                if let Some(ccmp) = ccmp {
                    emit_ccmp(
                        ctx, ccmp.size, ccmp.lhs, &ccmp.rhs, ccmp.imm, ccmp.nzcv, ccmp.cond,
                    )?;
                    ctx.end_inst()?;
                }
                match value {
                    SelectValue::Int {
                        size,
                        dst,
                        if_true,
                        if_false,
                    } => {
                        write!(ctx, "csel ")?;
                        emit_reg(ctx, dst.to_reg(), *size)?;
                        write!(ctx, ", ")?;
                        emit_reg(ctx, *if_true, *size)?;
                        write!(ctx, ", ")?;
                        emit_reg(ctx, *if_false, *size)?;
                        write!(ctx, ", {}", cond_name(*cond))
                    }
                    SelectValue::Float {
                        dst,
                        if_true,
                        if_false,
                    } => {
                        write!(ctx, "fcsel ")?;
                        emit_float_reg(ctx, dst.to_reg(), false)?;
                        write!(ctx, ", ")?;
                        emit_float_reg(ctx, *if_true, false)?;
                        write!(ctx, ", ")?;
                        emit_float_reg(ctx, *if_false, false)?;
                        write!(ctx, ", {}", cond_name(*cond))
                    }
                    SelectValue::Bool { dst } => {
                        write!(ctx, "cset ")?;
                        emit_reg(ctx, dst.to_reg(), OperandSize::Size32)?;
                        write!(ctx, ", {}", cond_name(*cond))
                    }
                }
            }
            Self::FMov { dst, src } => emit_fmov(ctx, dst.to_reg(), src),
            Self::VecMov { dst, src } => {
                write!(ctx, "mov ")?;
                emit_vec_reg(ctx, dst.to_reg())?;
                write!(ctx, ".16b, ")?;
                emit_vec_reg(ctx, *src)?;
                write!(ctx, ".16b")
            }
            Self::VecLd1 { dst, base } => {
                write!(ctx, "ld1 {{")?;
                emit_vec_reg(ctx, dst.to_reg())?;
                write!(ctx, ".16b}}, [")?;
                emit_reg(ctx, *base, OperandSize::Size64)?;
                write!(ctx, "]")
            }
            Self::VecSt1 { src, base } => {
                write!(ctx, "st1 {{")?;
                emit_vec_reg(ctx, *src)?;
                write!(ctx, ".16b}}, [")?;
                emit_reg(ctx, *base, OperandSize::Size64)?;
                write!(ctx, "]")
            }
            Self::VecDup { shape, dst, src } => {
                write!(ctx, "dup ")?;
                emit_vec_reg(ctx, dst.to_reg())?;
                write!(ctx, ".{}, ", shape.arrangement())?;
                emit_vec_scalar_reg(ctx, *src, *shape)
            }
            Self::VecArithRRR {
                op,
                shape,
                dst,
                lhs,
                rhs,
            } => {
                write!(ctx, "{} ", vec_arith_name(*op))?;
                emit_vec_reg(ctx, dst.to_reg())?;
                write!(ctx, ".{}, ", shape.arrangement())?;
                emit_vec_reg(ctx, *lhs)?;
                write!(ctx, ".{}, ", shape.arrangement())?;
                emit_vec_reg(ctx, *rhs)?;
                write!(ctx, ".{}", shape.arrangement())
            }
            Self::VecFmla {
                shape,
                dst,
                acc,
                lhs,
                rhs,
            } => {
                write!(ctx, "mov ")?;
                emit_vec_reg(ctx, dst.to_reg())?;
                write!(ctx, ".16b, ")?;
                emit_vec_reg(ctx, *acc)?;
                write!(ctx, ".16b")?;
                ctx.end_inst()?;
                write!(ctx, "fmla ")?;
                emit_vec_reg(ctx, dst.to_reg())?;
                write!(ctx, ".{}, ", shape.arrangement())?;
                emit_vec_reg(ctx, *lhs)?;
                write!(ctx, ".{}, ", shape.arrangement())?;
                emit_vec_reg(ctx, *rhs)?;
                write!(ctx, ".{}", shape.arrangement())
            }
            Self::VecBitwise { op, dst, lhs, rhs } => {
                write!(ctx, "{} ", vec_bit_name(*op))?;
                emit_vec_reg(ctx, dst.to_reg())?;
                write!(ctx, ".16b, ")?;
                emit_vec_reg(ctx, *lhs)?;
                write!(ctx, ".16b, ")?;
                emit_vec_reg(ctx, *rhs)?;
                write!(ctx, ".16b")
            }
            Self::VecCmp {
                op,
                shape,
                dst,
                lhs,
                rhs,
            } => {
                write!(ctx, "{} ", vec_cmp_name(*op))?;
                emit_vec_reg(ctx, dst.to_reg())?;
                write!(ctx, ".{}, ", shape.arrangement())?;
                emit_vec_reg(ctx, *lhs)?;
                write!(ctx, ".{}, ", shape.arrangement())?;
                emit_vec_reg(ctx, *rhs)?;
                write!(ctx, ".{}", shape.arrangement())
            }
            Self::VecBsl {
                dst,
                mask,
                lhs,
                rhs,
            } => {
                write!(ctx, "mov ")?;
                emit_vec_reg(ctx, dst.to_reg())?;
                write!(ctx, ".16b, ")?;
                emit_vec_reg(ctx, *mask)?;
                write!(ctx, ".16b")?;
                ctx.end_inst()?;
                write!(ctx, "bsl ")?;
                emit_vec_reg(ctx, dst.to_reg())?;
                write!(ctx, ".16b, ")?;
                emit_vec_reg(ctx, *lhs)?;
                write!(ctx, ".16b, ")?;
                emit_vec_reg(ctx, *rhs)?;
                write!(ctx, ".16b")
            }
            Self::VecCvt {
                op,
                shape,
                dst,
                src,
            } => {
                write!(ctx, "{} ", vec_cvt_name(*op))?;
                emit_vec_reg(ctx, dst.to_reg())?;
                write!(ctx, ".{}, ", shape.arrangement())?;
                emit_vec_reg(ctx, *src)?;
                write!(ctx, ".{}", shape.arrangement())
            }
            Self::VecAddv { dst, src } => {
                write!(ctx, "addv ")?;
                emit_float_reg(ctx, dst.to_reg(), false)?;
                write!(ctx, ", ")?;
                emit_vec_reg(ctx, *src)?;
                write!(ctx, ".4s")
            }
            Self::VecMovImm {
                shape,
                dst,
                imm,
                shift,
            } => {
                write!(ctx, "movi ")?;
                emit_vec_reg(ctx, dst.to_reg())?;
                write!(ctx, ".{}, #0x{:x}", shape.arrangement(), imm)?;
                if *shift != 0 {
                    write!(ctx, ", lsl #{shift}")?;
                }
                Ok(())
            }
            Self::VecExtractLane {
                size,
                dst,
                src,
                lane,
            } => {
                write!(ctx, "mov ")?;
                emit_reg(ctx, dst.to_reg(), *size)?;
                write!(ctx, ", ")?;
                emit_vec_reg(ctx, *src)?;
                write!(
                    ctx,
                    ".{}[{}]",
                    if *size == OperandSize::Size64 {
                        "d"
                    } else {
                        "s"
                    },
                    lane
                )
            }
            Self::VecInsertLane {
                size,
                dst,
                vector,
                src,
                lane,
            } => {
                write!(ctx, "mov ")?;
                emit_vec_reg(ctx, dst.to_reg())?;
                write!(ctx, ".16b, ")?;
                emit_vec_reg(ctx, *vector)?;
                write!(ctx, ".16b")?;
                ctx.end_inst()?;
                write!(ctx, "mov ")?;
                emit_vec_reg(ctx, dst.to_reg())?;
                write!(
                    ctx,
                    ".{}[{}], ",
                    if *size == OperandSize::Size64 {
                        "d"
                    } else {
                        "s"
                    },
                    lane
                )?;
                emit_reg(ctx, *src, *size)
            }
            Self::VecMinMax {
                op,
                shape,
                dst,
                lhs,
                rhs,
            } => {
                write!(ctx, "{} ", vec_minmax_name(*op))?;
                emit_vec_reg(ctx, dst.to_reg())?;
                write!(ctx, ".{}, ", shape.arrangement())?;
                emit_vec_reg(ctx, *lhs)?;
                write!(ctx, ".{}, ", shape.arrangement())?;
                emit_vec_reg(ctx, *rhs)?;
                write!(ctx, ".{}", shape.arrangement())
            }
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
                write!(ctx, "ldr ")?;
                emit_data_reg(ctx, dst.to_reg(), *ty)?;
                write!(ctx, ", ")?;
                emit_amode(ctx, addr)
            }
            Self::Store { ty, src, addr } => {
                write!(ctx, "str ")?;
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
            Self::RetVal { .. } => Ok(()),
            Self::Args { .. } => Ok(()),
            Self::Call { label, .. } => {
                write!(ctx, "bl ")?;
                label.emit(ctx)
            }
            Self::TailCall { label, .. } => {
                write!(ctx, "b ")?;
                label.emit(ctx)
            }
            Self::Ret => write!(ctx, "ret"),
        }
    }
}

/// Compose the text before a branch target for a register-testing branch
/// (cbz/cbnz): mnemonic, register, and the trailing separator. The text is
/// accumulated through `ctx` so register rendering stays in one place.
fn branch_prefix(
    ctx: &mut dyn EmitContext,
    mnemonic: &str,
    reg: Reg,
    size: OperandSize,
) -> Result<String, core::fmt::Error> {
    write!(ctx, "{mnemonic} ")?;
    emit_reg(ctx, reg, size)?;
    write!(ctx, ", ")?;
    Ok(ctx.take_inst_text())
}

/// Like [`branch_prefix`] for test-bit branches (tbz/tbnz), which also carry
/// the bit index.
fn branch_prefix_bit(
    ctx: &mut dyn EmitContext,
    mnemonic: &str,
    reg: Reg,
    size: OperandSize,
    bit: u8,
) -> Result<String, core::fmt::Error> {
    write!(ctx, "{mnemonic} ")?;
    emit_reg(ctx, reg, size)?;
    write!(ctx, ", #{bit}, ")?;
    Ok(ctx.take_inst_text())
}

fn emit_select_cmp(ctx: &mut dyn EmitContext, cmp: &SelectCmp) -> core::fmt::Result {
    match cmp {
        SelectCmp::IntRR { size, lhs, rhs } => {
            write!(ctx, "cmp ")?;
            emit_reg(ctx, *lhs, *size)?;
            write!(ctx, ", ")?;
            emit_reg_or_zr(ctx, rhs, *size)
        }
        SelectCmp::IntImm { size, lhs, imm } => {
            write!(ctx, "cmp ")?;
            emit_reg(ctx, *lhs, *size)?;
            write!(ctx, ", #{}", imm.value())?;
            if imm.shift12() {
                write!(ctx, ", lsl #12")?;
            }
            Ok(())
        }
        SelectCmp::Float { lhs, rhs } => emit_float_rr(ctx, "fcmp", *lhs, rhs),
    }
}

fn emit_ccmp(
    ctx: &mut dyn EmitContext,
    size: OperandSize,
    lhs: Reg,
    rhs: &RegOrZr,
    imm: Option<Imm12>,
    nzcv: u8,
    cond: Cond,
) -> core::fmt::Result {
    write!(ctx, "ccmp ")?;
    emit_reg(ctx, lhs, size)?;
    match (rhs, imm) {
        (RegOrZr::Reg(rhs), _) => {
            write!(ctx, ", ")?;
            emit_reg(ctx, *rhs, size)?;
        }
        (RegOrZr::Zr, Some(imm)) => write!(ctx, ", #{}", imm.value())?,
        (RegOrZr::Zr, None) => {
            write!(ctx, ", ")?;
            emit_gpr(ctx, &Gpr::Zr, size)?;
        }
    }
    write!(ctx, ", #{nzcv}, {}", cond_name(cond))
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
            ctx.end_inst()?;
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
    if reg.to_real_reg() == Some(crate::regs::stack_preg()) {
        return write!(ctx, "sp");
    }
    match (reg.to_real_reg(), size) {
        (Some(preg), OperandSize::Size32) if preg.class() == RegClass::Int => {
            write!(ctx, "w{}", preg.hw_enc())
        }
        (Some(preg), _) if preg.class() == RegClass::Int => write!(ctx, "x{}", preg.hw_enc()),
        _ => ctx.write_reg(&reg),
    }
}
fn emit_data_reg(ctx: &mut dyn EmitContext, reg: Reg, ty: MemoryType) -> core::fmt::Result {
    match ty {
        MemoryType::I64 => emit_reg(ctx, reg, OperandSize::Size64),
        MemoryType::I32 => emit_reg(ctx, reg, OperandSize::Size32),
        MemoryType::Vec128 => match reg.to_real_reg() {
            Some(preg) if preg.class() == RegClass::Vector => write!(ctx, "q{}", preg.hw_enc()),
            _ => ctx.write_reg(&reg),
        },
        MemoryType::F32 | MemoryType::F64 => emit_float_reg(ctx, reg, ty == MemoryType::F64),
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
fn emit_vec_reg(ctx: &mut dyn EmitContext, reg: Reg) -> core::fmt::Result {
    match reg.to_real_reg() {
        Some(preg) if preg.class() == RegClass::Vector => write!(ctx, "v{}", preg.hw_enc()),
        _ => ctx.write_reg(&reg),
    }
}
/// Render a scalar source for `dup`: a GPR is `w`/`x`, a float register `s`/`d`
/// depending on the lane element size of the destination arrangement.
fn emit_vec_scalar_reg(ctx: &mut dyn EmitContext, reg: Reg, shape: VecShape) -> core::fmt::Result {
    let wide = shape == VecShape::TwoD;
    match reg.to_real_reg() {
        Some(preg) if preg.class() == RegClass::Int => {
            write!(ctx, "{}{}", if wide { "x" } else { "w" }, preg.hw_enc())
        }
        Some(preg) if preg.class() == RegClass::Float => {
            write!(ctx, "{}{}", if wide { "d" } else { "s" }, preg.hw_enc())
        }
        _ => ctx.write_reg(&reg),
    }
}
fn vec_arith_name(op: VecArithOp) -> &'static str {
    match op {
        VecArithOp::Add => "add",
        VecArithOp::Sub => "sub",
        VecArithOp::Mul => "mul",
    }
}
fn vec_bit_name(op: VecBitOp) -> &'static str {
    match op {
        VecBitOp::And => "and",
        VecBitOp::Orr => "orr",
        VecBitOp::Eor => "eor",
    }
}
fn vec_cmp_name(op: VecCmpOp) -> &'static str {
    match op {
        VecCmpOp::Eq => "cmeq",
        VecCmpOp::Gt => "cmgt",
    }
}
fn vec_cvt_name(op: VecCvtOp) -> &'static str {
    match op {
        VecCvtOp::Scvtf => "scvtf",
        VecCvtOp::Fcvtzs => "fcvtzs",
    }
}
fn vec_minmax_name(op: VecMinMaxOp) -> &'static str {
    match op {
        VecMinMaxOp::Smin => "smin",
        VecMinMaxOp::Smax => "smax",
        VecMinMaxOp::Umin => "umin",
        VecMinMaxOp::Umax => "umax",
        VecMinMaxOp::Fmin => "fmin",
        VecMinMaxOp::Fmax => "fmax",
    }
}
fn emit_gpr(ctx: &mut dyn EmitContext, reg: &Gpr, size: OperandSize) -> core::fmt::Result {
    match reg {
        Gpr::Reg(reg) => emit_reg(ctx, *reg, size),
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
            emit_reg(ctx, *base, OperandSize::Size64)?;
            write!(ctx, "]")
        }
        AMode::UnsignedOffset { base, offset } => {
            write!(ctx, "[")?;
            emit_reg(ctx, *base, OperandSize::Size64)?;
            write!(ctx, ", #{}]", offset.byte_offset())
        }
        AMode::SignedOffset { base, offset } => {
            write!(ctx, "[")?;
            emit_reg(ctx, *base, OperandSize::Size64)?;
            write!(ctx, ", #{}]", offset.value())
        }
        AMode::RegOffset { base, index } => {
            write!(ctx, "[")?;
            emit_reg(ctx, *base, OperandSize::Size64)?;
            write!(ctx, ", ")?;
            emit_reg(ctx, *index, OperandSize::Size64)?;
            write!(ctx, "]")
        }
        AMode::ScaledRegOffset { base, index, shift } => {
            write!(ctx, "[")?;
            emit_reg(ctx, *base, OperandSize::Size64)?;
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
            debug_assert!(
                *shift <= 3,
                "AArch64 load/store extend scale is 0..=3, got {shift}"
            );
            write!(ctx, "[")?;
            emit_reg(ctx, *base, OperandSize::Size64)?;
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
            emit_reg(ctx, *base, OperandSize::Size64)?;
            write!(ctx, ", #{}]", offset.byte_offset())
        }
        PairAMode::PreIndex { base, offset } => {
            write!(ctx, "[")?;
            emit_reg(ctx, *base, OperandSize::Size64)?;
            write!(ctx, ", #{}]!", offset.byte_offset())
        }
        PairAMode::PostIndex { base, offset } => {
            write!(ctx, "[")?;
            emit_reg(ctx, *base, OperandSize::Size64)?;
            write!(ctx, "], #{}", offset.byte_offset())
        }
    }
}
fn alu_name(op: AluOp) -> &'static str {
    match op {
        AluOp::Add => "add",
        AluOp::Sub => "sub",
        AluOp::Mul => "mul",
        AluOp::And => "and",
        AluOp::Orr => "orr",
        AluOp::Orn => "orn",
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
        AMode::Reg { .. }
        | AMode::UnsignedOffset { .. }
        | AMode::SignedOffset { .. }
        | AMode::RegOffset { .. } => true,
        AMode::ScaledRegOffset { shift, .. } => *shift == ty.byte_size().trailing_zeros() as u8,
        AMode::ExtendedRegOffset { extend, shift, .. } => {
            matches!(
                extend,
                ExtendOp::Uxtw | ExtendOp::Sxtw | ExtendOp::Uxtx | ExtendOp::Sxtx
            ) && (*shift == 0 || *shift == ty.byte_size().trailing_zeros() as u8)
        }
        _ => true,
    }
}
fn pair_amode_is_legal(_addr: &PairAMode) -> bool {
    // A `Reg` base can always serve as a load/store base: it can name SP,
    // FP, or any allocatable general-purpose register, and XZR (which A64
    // excludes as a base register) is no longer representable in `Reg` now
    // that `Gpr::Zr` has been split out into `RegOrZr`.
    true
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

/// AArch64 condition-code negation table.
pub(crate) fn invert_cond(cond: Cond) -> Cond {
    match cond {
        Cond::Eq => Cond::Ne,
        Cond::Ne => Cond::Eq,
        Cond::Hs => Cond::Lo,
        Cond::Lo => Cond::Hs,
        Cond::Mi => Cond::Pl,
        Cond::Pl => Cond::Mi,
        Cond::Vs => Cond::Vc,
        Cond::Vc => Cond::Vs,
        Cond::Hi => Cond::Ls,
        Cond::Ls => Cond::Hi,
        Cond::Ge => Cond::Lt,
        Cond::Lt => Cond::Ge,
        Cond::Gt => Cond::Le,
        Cond::Le => Cond::Gt,
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

#[cfg(test)]
mod tests {
    use core::fmt::Write;

    use taki_mir::{
        abi::ArgPair,
        block_order::MirBlockIndex,
        prelude::{HirFunction, HirInst},
        reg_alloc::reg::{OperandConstraint, OperandKind, PReg, RegClass, VReg},
        register::{Reg, Writable},
        vcode::{EmitContext, MachInst, MachInstEmit, MachTerminator},
    };

    use super::{CCmpStep, Cond, Imm12, ImmLogic, MInst, SelectCmp, SelectValue, call_clobbers};
    use crate::regs::{OperandSize, float_reg, int_reg};

    #[derive(Default)]
    struct TestEmitContext(String);

    impl core::fmt::Write for TestEmitContext {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            self.0.push_str(text);
            Ok(())
        }
    }

    impl EmitContext for TestEmitContext {
        fn write_reg(&mut self, _reg: &taki_mir::register::Reg) -> core::fmt::Result {
            unreachable!("tests use physical registers")
        }

        fn write_label_ref(&mut self, _idx: MirBlockIndex) -> core::fmt::Result {
            unreachable!("select pseudo has no labels")
        }

        fn write_function_label(&mut self, _func: HirFunction) -> core::fmt::Result {
            unreachable!("select pseudo has no labels")
        }

        fn write_external_symbol(&mut self, symbol: &str) -> core::fmt::Result {
            write!(self, "{symbol}")
        }

        fn write_global_label(&mut self, _global: HirInst) -> core::fmt::Result {
            unreachable!("select pseudo has no labels")
        }

        /// Mirror the emission contract of `EmitBuffer::end_inst`: one flush
        /// = one instruction line.
        fn end_inst(&mut self) -> core::fmt::Result {
            self.0.push_str("\n    ");
            Ok(())
        }
    }

    fn emit(inst: MInst) -> String {
        let mut ctx = TestEmitContext::default();
        inst.emit(&mut ctx).unwrap();
        ctx.0
    }

    #[test]
    fn emits_adjacent_i32_cmp_and_csel() {
        let text = emit(MInst::CmpSelect {
            cmp: SelectCmp::IntImm {
                size: OperandSize::Size32,
                lhs: int_reg(1),
                imm: Imm12::new(0, false).unwrap(),
            },
            ccmp: None,
            cond: Cond::Ne,
            value: SelectValue::Int {
                size: OperandSize::Size32,
                dst: Writable::from_reg(int_reg(0)),
                if_true: int_reg(2),
                if_false: int_reg(3),
            },
        });
        assert_eq!(text, "cmp w1, #0\n    csel w0, w2, w3, ne");
    }

    #[test]
    fn call_clobbers_exclude_the_fixed_return_register() {
        let int = call_clobbers(
            crate::regs::DEFAULT_CLOBBERS,
            Some(&taki_mir::abi::CallRetPair {
                vreg: Writable::from_reg(int_reg(1)),
                preg: int_reg(0),
            }),
        );
        let float = call_clobbers(
            crate::regs::DEFAULT_CLOBBERS,
            Some(&taki_mir::abi::CallRetPair {
                vreg: Writable::from_reg(float_reg(1)),
                preg: float_reg(0),
            }),
        );

        assert!(!int.contains(crate::regs::int_preg(0)));
        assert!(!float.contains(crate::regs::float_preg(0)));
        assert!(int.contains(crate::regs::int_preg(1)));
        assert!(float.contains(crate::regs::float_preg(1)));
    }

    #[test]
    fn emits_64_bit_csel_for_pointer_values() {
        let text = emit(MInst::CmpSelect {
            cmp: SelectCmp::IntImm {
                // RaanaIR select conditions are always i32, even when the
                // selected values are pointers or strings.
                size: OperandSize::Size32,
                lhs: int_reg(1),
                imm: Imm12::new(0, false).unwrap(),
            },
            ccmp: None,
            cond: Cond::Ne,
            value: SelectValue::Int {
                size: OperandSize::Size64,
                dst: Writable::from_reg(int_reg(0)),
                if_true: int_reg(2),
                if_false: int_reg(3),
            },
        });
        assert_eq!(text, "cmp w1, #0\n    csel x0, x2, x3, ne");
    }

    #[test]
    fn emits_adjacent_float_cmp_and_fcsel() {
        let text = emit(MInst::CmpSelect {
            cmp: SelectCmp::Float {
                lhs: float_reg(4),
                rhs: float_reg(5),
            },
            cond: Cond::Mi,
            value: SelectValue::Float {
                dst: Writable::from_reg(float_reg(0)),
                if_true: float_reg(1),
                if_false: float_reg(2),
            },
            ccmp: None,
        });
        assert_eq!(text, "fcmp s4, s5\n    fcsel s0, s1, s2, mi");
    }

    #[test]
    fn emits_adjacent_cmp_and_cset() {
        let text = emit(MInst::CmpSelect {
            cmp: SelectCmp::IntRR {
                size: OperandSize::Size32,
                lhs: int_reg(1),
                rhs: crate::regs::RegOrZr::Reg(int_reg(2)),
            },
            ccmp: None,
            cond: Cond::Eq,
            value: SelectValue::Bool {
                dst: Writable::from_reg(int_reg(0)),
            },
        });
        assert_eq!(text, "cmp w1, w2\n    cset w0, eq");
    }

    #[test]
    fn emits_and_ccmp_chain_as_cmp_ccmp_csel() {
        let text = emit(MInst::CmpSelect {
            cmp: SelectCmp::IntImm {
                size: OperandSize::Size32,
                lhs: int_reg(1),
                imm: Imm12::new(1, false).unwrap(),
            },
            ccmp: Some(Box::new(CCmpStep {
                size: OperandSize::Size32,
                lhs: int_reg(2),
                rhs: crate::regs::RegOrZr::Zr,
                imm: Some(Imm12::new(1, false).unwrap()),
                nzcv: 0,
                cond: Cond::Eq,
            })),
            cond: Cond::Eq,
            value: SelectValue::Int {
                size: OperandSize::Size32,
                dst: Writable::from_reg(int_reg(0)),
                if_true: int_reg(3),
                if_false: int_reg(4),
            },
        });
        assert_eq!(
            text,
            "cmp w1, #1\n    ccmp w2, #1, #0, eq\n    csel w0, w3, w4, eq"
        );
    }

    #[test]
    fn emits_or_ccmp_chain_with_true_fallback_nzcv() {
        let text = emit(MInst::CmpSelect {
            cmp: SelectCmp::IntRR {
                size: OperandSize::Size32,
                lhs: int_reg(1),
                rhs: crate::regs::RegOrZr::Reg(int_reg(2)),
            },
            ccmp: Some(Box::new(CCmpStep {
                size: OperandSize::Size32,
                lhs: int_reg(3),
                rhs: crate::regs::RegOrZr::Zr,
                imm: Some(Imm12::new(1, false).unwrap()),
                nzcv: 4,
                cond: Cond::Ne,
            })),
            cond: Cond::Eq,
            value: SelectValue::Bool {
                dst: Writable::from_reg(int_reg(0)),
            },
        });
        assert_eq!(text, "cmp w1, w2\n    ccmp w3, #1, #4, ne\n    cset w0, eq");
    }

    #[test]
    fn emits_fused_flag_forms() {
        let subs = emit(MInst::SubsRRImm12 {
            size: OperandSize::Size32,
            dst: Writable::from_reg(int_reg(1)),
            src: int_reg(1),
            imm: Imm12::new(1, false).unwrap(),
        });
        assert_eq!(subs, "subs w1, w1, #1");

        let ands = emit(MInst::AndsRRImmLogic {
            size: OperandSize::Size32,
            dst: Writable::from_reg(int_reg(1)),
            src: crate::regs::RegOrZr::Reg(int_reg(2)),
            imm: ImmLogic::new(0x8000_0001, OperandSize::Size32).unwrap(),
        });
        assert_eq!(ands, "ands w1, w2, #0x80000001");

        let tst = emit(MInst::TstRRImmLogic {
            size: OperandSize::Size32,
            src: crate::regs::RegOrZr::Reg(int_reg(2)),
            imm: ImmLogic::new(0x8000_0001, OperandSize::Size32).unwrap(),
        });
        assert_eq!(tst, "tst w2, #0x80000001");
    }

    #[test]
    fn stack_pointer_prints_as_sp() {
        let text = emit(MInst::MovPhys {
            size: OperandSize::Size64,
            dst: Writable::from_reg(int_reg(0)),
            src: crate::regs::stack_reg(),
        });
        assert_eq!(text, "mov x0, sp");
    }

    #[test]
    fn alu_rr_imm12_accepts_sp_as_source_and_destination() {
        let text = emit(MInst::AluRRImm12 {
            op: super::AluOp::Sub,
            size: OperandSize::Size64,
            dst: crate::regs::writable_stack_reg(),
            src: crate::regs::stack_reg(),
            imm: Imm12::new(32, false).unwrap(),
        });
        assert_eq!(text, "sub sp, sp, #32");
    }

    #[test]
    fn imm12_maybe_from_u64_handles_unshifted_form() {
        let imm = Imm12::maybe_from_u64(0xfff).unwrap();
        assert_eq!(imm.value(), 0xfff);
        assert!(!imm.shift12());
        let imm = Imm12::maybe_from_u64(0).unwrap();
        assert_eq!(imm.value(), 0);
        assert!(!imm.shift12());
    }

    #[test]
    fn imm12_maybe_from_u64_handles_shift12_form() {
        let imm = Imm12::maybe_from_u64(4096).unwrap();
        assert_eq!(imm.value(), 1);
        assert!(imm.shift12());
        let imm = Imm12::maybe_from_u64(0xfff000).unwrap();
        assert_eq!(imm.value(), 0xfff);
        assert!(imm.shift12());
    }

    #[test]
    fn imm12_maybe_from_u64_rejects_unrepresentable_values() {
        assert!(Imm12::maybe_from_u64(0xfff001).is_none());
        assert!(Imm12::maybe_from_u64(0x1000_0000).is_none());
        assert!(Imm12::maybe_from_u64(u64::MAX).is_none());
    }

    fn virtual_reg(index: usize, class: RegClass) -> Reg {
        Reg::from_virtual_reg(VReg::new(192 + index, class))
    }

    fn int_args() -> MInst {
        MInst::Args {
            args: vec![
                ArgPair {
                    vreg: Writable::from_reg(virtual_reg(0, RegClass::Int)),
                    preg: int_reg(0),
                },
                ArgPair {
                    vreg: Writable::from_reg(virtual_reg(1, RegClass::Int)),
                    preg: int_reg(1),
                },
            ],
        }
    }

    struct TestOperandVisitor(Vec<(VReg, OperandConstraint, OperandKind)>);

    impl taki_mir::reg_alloc::reg::OperandVisitor for TestOperandVisitor {
        fn add_operand(
            &mut self,
            reg: &mut Reg,
            constraint: OperandConstraint,
            kind: OperandKind,
            _pos: taki_mir::reg_alloc::reg::OperandPos,
        ) {
            self.0
                .push((reg.to_virtual_reg().unwrap(), constraint, kind));
        }
    }

    #[test]
    fn args_pseudo_emits_no_machine_code() {
        assert_eq!(emit(int_args()), "");
    }

    #[test]
    fn args_pseudo_is_not_a_terminator() {
        assert_eq!(int_args().is_term(), MachTerminator::None);
    }

    #[test]
    fn args_pseudo_binds_each_parameter_with_a_fixed_def() {
        let mut args = int_args();
        let mut visitor = TestOperandVisitor(Vec::new());
        args.get_operands(&mut visitor);
        assert_eq!(visitor.0.len(), 2);
        for (index, (vreg, constraint, kind)) in visitor.0.iter().enumerate() {
            assert_eq!(*kind, OperandKind::Def);
            let OperandConstraint::FixedReg(preg) = constraint else {
                panic!("Args operands must use FixedReg constraints");
            };
            assert_eq!(*preg, int_reg(index as u8).to_physical_reg().unwrap());
            assert_eq!(vreg.class(), RegClass::Int);
        }
    }

    #[test]
    fn args_verify_accepts_matching_register_classes() {
        assert!(int_args().verify().is_ok());
    }

    #[test]
    fn args_verify_rejects_mismatched_register_classes() {
        let args = MInst::Args {
            args: vec![ArgPair {
                vreg: Writable::from_reg(virtual_reg(0, RegClass::Float)),
                preg: int_reg(0),
            }],
        };
        assert!(args.verify().is_err());
    }

    fn vec_reg(index: u8) -> Reg {
        Reg::from_physical_reg(PReg::new(index as usize, RegClass::Vector))
    }

    #[test]
    fn emits_vector_move_as_mov_v_b() {
        let text = emit(MInst::VecMov {
            dst: Writable::from_reg(vec_reg(1)),
            src: vec_reg(2),
        });
        assert_eq!(text, "mov v1.16b, v2.16b");
    }

    #[test]
    fn emits_128_bit_vector_load_and_store() {
        let load = emit(MInst::Load {
            ty: super::MemoryType::Vec128,
            dst: Writable::from_reg(vec_reg(3)),
            addr: super::AMode::UnsignedOffset {
                base: int_reg(0),
                offset: super::UImm12Scaled::new(32, 16).unwrap(),
            },
        });
        assert_eq!(load, "ldr q3, [x0, #32]");

        let store = emit(MInst::Store {
            ty: super::MemoryType::Vec128,
            src: vec_reg(4),
            addr: super::AMode::UnsignedOffset {
                base: int_reg(0),
                offset: super::UImm12Scaled::new(16, 16).unwrap(),
            },
        });
        assert_eq!(store, "str q4, [x0, #16]");
    }

    #[test]
    fn emits_ld1_st1_vector_memory_forms() {
        let load = emit(MInst::VecLd1 {
            dst: Writable::from_reg(vec_reg(0)),
            base: int_reg(1),
        });
        assert_eq!(load, "ld1 {v0.16b}, [x1]");

        let store = emit(MInst::VecSt1 {
            src: vec_reg(5),
            base: int_reg(2),
        });
        assert_eq!(store, "st1 {v5.16b}, [x2]");
    }

    #[test]
    fn emits_dup_from_gpr_and_float_scalars() {
        let dup_i32 = emit(MInst::VecDup {
            shape: super::VecShape::FourS,
            dst: Writable::from_reg(vec_reg(0)),
            src: int_reg(3),
        });
        assert_eq!(dup_i32, "dup v0.4s, w3");

        let dup_i64 = emit(MInst::VecDup {
            shape: super::VecShape::TwoD,
            dst: Writable::from_reg(vec_reg(0)),
            src: int_reg(4),
        });
        assert_eq!(dup_i64, "dup v0.2d, x4");

        let dup_f32 = emit(MInst::VecDup {
            shape: super::VecShape::FourS,
            dst: Writable::from_reg(vec_reg(0)),
            src: float_reg(5),
        });
        assert_eq!(dup_f32, "dup v0.4s, s5");
    }

    #[test]
    fn emits_vector_arith_forms() {
        for (op, mnemonic) in [
            (super::VecArithOp::Add, "add"),
            (super::VecArithOp::Sub, "sub"),
            (super::VecArithOp::Mul, "mul"),
        ] {
            let text = emit(MInst::VecArithRRR {
                op,
                shape: super::VecShape::FourS,
                dst: Writable::from_reg(vec_reg(0)),
                lhs: vec_reg(1),
                rhs: vec_reg(2),
            });
            assert_eq!(text, format!("{mnemonic} v0.4s, v1.4s, v2.4s"));
        }
        let two_d = emit(MInst::VecArithRRR {
            op: super::VecArithOp::Add,
            shape: super::VecShape::TwoD,
            dst: Writable::from_reg(vec_reg(0)),
            lhs: vec_reg(1),
            rhs: vec_reg(2),
        });
        assert_eq!(two_d, "add v0.2d, v1.2d, v2.2d");
    }

    #[test]
    fn emits_fmla_and_bitwise_and_compare_forms() {
        let fmla = emit(MInst::VecFmla {
            shape: super::VecShape::FourS,
            dst: Writable::from_reg(vec_reg(0)),
            acc: vec_reg(3),
            lhs: vec_reg(1),
            rhs: vec_reg(2),
        });
        assert_eq!(fmla, "mov v0.16b, v3.16b\n    fmla v0.4s, v1.4s, v2.4s");

        for (op, mnemonic) in [
            (super::VecBitOp::And, "and"),
            (super::VecBitOp::Orr, "orr"),
            (super::VecBitOp::Eor, "eor"),
        ] {
            let text = emit(MInst::VecBitwise {
                op,
                dst: Writable::from_reg(vec_reg(0)),
                lhs: vec_reg(1),
                rhs: vec_reg(2),
            });
            assert_eq!(text, format!("{mnemonic} v0.16b, v1.16b, v2.16b"));
        }

        let cmeq = emit(MInst::VecCmp {
            op: super::VecCmpOp::Eq,
            shape: super::VecShape::FourS,
            dst: Writable::from_reg(vec_reg(0)),
            lhs: vec_reg(1),
            rhs: vec_reg(2),
        });
        assert_eq!(cmeq, "cmeq v0.4s, v1.4s, v2.4s");

        let bsl = emit(MInst::VecBsl {
            dst: Writable::from_reg(vec_reg(0)),
            mask: vec_reg(3),
            lhs: vec_reg(1),
            rhs: vec_reg(2),
        });
        assert_eq!(bsl, "mov v0.16b, v3.16b\n    bsl v0.16b, v1.16b, v2.16b");
    }

    #[test]
    fn emits_vector_cvt_and_horizontal_add() {
        let scvtf = emit(MInst::VecCvt {
            op: super::VecCvtOp::Scvtf,
            shape: super::VecShape::FourS,
            dst: Writable::from_reg(vec_reg(0)),
            src: vec_reg(1),
        });
        assert_eq!(scvtf, "scvtf v0.4s, v1.4s");

        let fcvtzs = emit(MInst::VecCvt {
            op: super::VecCvtOp::Fcvtzs,
            shape: super::VecShape::FourS,
            dst: Writable::from_reg(vec_reg(2)),
            src: vec_reg(3),
        });
        assert_eq!(fcvtzs, "fcvtzs v2.4s, v3.4s");

        let addv = emit(MInst::VecAddv {
            dst: Writable::from_reg(float_reg(0)),
            src: vec_reg(1),
        });
        assert_eq!(addv, "addv s0, v1.4s");
    }

    #[test]
    fn emits_vector_mov_imm_lane_and_minmax_forms() {
        let movi = emit(MInst::VecMovImm {
            shape: super::VecShape::FourS,
            dst: Writable::from_reg(vec_reg(0)),
            imm: 0x3f,
            shift: 0,
        });
        assert_eq!(movi, "movi v0.4s, #0x3f");

        let movi_shift = emit(MInst::VecMovImm {
            shape: super::VecShape::FourS,
            dst: Writable::from_reg(vec_reg(0)),
            imm: 0xff,
            shift: 8,
        });
        assert_eq!(movi_shift, "movi v0.4s, #0xff, lsl #8");

        let extract = emit(MInst::VecExtractLane {
            size: OperandSize::Size32,
            dst: Writable::from_reg(int_reg(0)),
            src: vec_reg(1),
            lane: 2,
        });
        assert_eq!(extract, "mov w0, v1.s[2]");

        let extract_64 = emit(MInst::VecExtractLane {
            size: OperandSize::Size64,
            dst: Writable::from_reg(int_reg(0)),
            src: vec_reg(1),
            lane: 1,
        });
        assert_eq!(extract_64, "mov x0, v1.d[1]");

        let insert = emit(MInst::VecInsertLane {
            size: OperandSize::Size32,
            dst: Writable::from_reg(vec_reg(2)),
            vector: vec_reg(4),
            src: int_reg(3),
            lane: 0,
        });
        assert_eq!(insert, "mov v2.16b, v4.16b\n    mov v2.s[0], w3");

        for (op, mnemonic) in [
            (super::VecMinMaxOp::Smin, "smin"),
            (super::VecMinMaxOp::Smax, "smax"),
            (super::VecMinMaxOp::Umin, "umin"),
            (super::VecMinMaxOp::Umax, "umax"),
            (super::VecMinMaxOp::Fmin, "fmin"),
            (super::VecMinMaxOp::Fmax, "fmax"),
        ] {
            let text = emit(MInst::VecMinMax {
                op,
                shape: super::VecShape::FourS,
                dst: Writable::from_reg(vec_reg(0)),
                lhs: vec_reg(1),
                rhs: vec_reg(2),
            });
            assert_eq!(text, format!("{mnemonic} v0.4s, v1.4s, v2.4s"));
        }
    }

    #[test]
    fn vector_instructions_expose_their_operands() {
        let mut inst = MInst::VecArithRRR {
            op: super::VecArithOp::Add,
            shape: super::VecShape::FourS,
            dst: Writable::from_reg(virtual_reg(0, RegClass::Vector)),
            lhs: virtual_reg(1, RegClass::Vector),
            rhs: virtual_reg(2, RegClass::Vector),
        };
        let mut visitor = TestOperandVisitor(Vec::new());
        inst.get_operands(&mut visitor);
        assert_eq!(visitor.0.len(), 3);
        assert_eq!(visitor.0[0].1, OperandConstraint::Reg);
        assert_eq!(visitor.0[2].2, OperandKind::Def);
    }

    #[test]
    fn emits_sign_extension() {
        let text = emit(MInst::Sxtw {
            size: OperandSize::Size32,
            dst: Writable::from_reg(int_reg(3)),
            src: int_reg(2),
        });
        assert_eq!(text, "sxtw x3, w2");
    }
}
