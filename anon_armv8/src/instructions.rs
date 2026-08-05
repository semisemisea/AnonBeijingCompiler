//! Typed AArch64 instruction forms and encoding-valid operands.

use taki_mir::{
    abi::{ArgPair, CallArgPair, CallRetPair, RetPair, StackAMode},
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

mod emit;

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
    /// Floating-point vector arithmetic (`fadd`/`fsub`/`fmul`): `<4 x f32>`
    /// lanes. The plain forms above are the integer NEON ops and must not be
    /// used on float vectors.
    Fadd,
    Fsub,
    Fmul,
}

/// Vector shift operations. Immediate forms use `shl`/`ushr`/`sshr`;
/// register (variable-amount) forms use `sshl`/`ushl`, with the amount
/// vector pre-negated for right shifts (NEON has no register-form right
/// shift; `sshl`/`ushl` with a negative amount shift right).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VecShiftOp {
    /// Logical left shift.
    Shl,
    /// Logical right shift (`ushr`/`ushl`).
    Shr,
    /// Arithmetic right shift (`sshr`/`sshl`).
    Sar,
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

/// Integer vector multiply-accumulate/negate-accumulate (`mla`/`mls`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VecMlaOp {
    Mla,
    Mls,
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
    /// Vector shift: immediate form `shl/ushr/sshr v{d}.<shape>, v{lhs}.<shape>, #imm`;
    /// register form `sshl/ushl v{d}.<shape>, v{lhs}.<shape>, v{rhs}.<shape>` with the
    /// amount vector pre-negated for right shifts (`neg` emitted separately).
    VecShift {
        op: VecShiftOp,
        shape: VecShape,
        dst: WritableReg,
        lhs: Reg,
        rhs: Reg,
        imm: Option<u8>,
    },
    /// Vector float divide: `fdiv v{d}.<shape>, v{lhs}.<shape>, v{rhs}.<shape>`.
    /// (NEON has no integer vector divide; i32 Div/Rem with a constant splat
    /// divisor is rewritten to a multiply-high magic sequence instead.)
    VecDiv {
        shape: VecShape,
        dst: WritableReg,
        lhs: Reg,
        rhs: Reg,
    },
    /// Vector negation: `neg v{d}.<shape>, v{src}.<shape>`.
    VecNeg {
        shape: VecShape,
        dst: WritableReg,
        src: Reg,
    },
    /// Vector bitwise not: `mvn v{d}.16b, v{s}.16b`.
    VecBitwiseNot {
        dst: WritableReg,
        src: Reg,
    },
    /// Integer vector multiply-accumulate / negate-accumulate. Read-modify-write
    /// on `acc`; the accumulator is copied in first:
    /// `mov v{d}.16b, v{acc}.16b; mla/mls v{d}.<shape>, v{lhs}.<shape>, v{rhs}.<shape>`.
    VecMla {
        op: VecMlaOp,
        shape: VecShape,
        dst: WritableReg,
        acc: Reg,
        lhs: Reg,
        rhs: Reg,
    },
    /// Signed widening multiply: `smull v{d}.2d, v{lhs}.2s, v{rhs}.2s` (low
    /// half) or `smull2 v{d}.2d, v{lhs}.4s, v{rhs}.4s` (high half).
    VecSMull {
        high: bool,
        dst: WritableReg,
        lhs: Reg,
        rhs: Reg,
    },
    /// Narrowing shift right: `xtn v{d}.4s, v{s}.2d` (low half) or
    /// `xtn2 v{d}.4s, v{s}.2d` (high half). `xtn2` preserves the destination's
    /// low 64 bits, so `high=true` is a read-modify-write on `dst`; the
    /// partial result is copied in first: `mov v{d}.16b, v{acc}.16b;
    /// xtn2 v{d}.4s, v{s}.2d`. For `high=false` `acc` is ignored.
    VecNarrow {
        high: bool,
        dst: WritableReg,
        acc: Reg,
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
            | Self::VecNeg { dst, src, .. }
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
            | Self::VecMinMax { dst, lhs, rhs, .. }
            | Self::VecShift { dst, lhs, rhs, .. }
            | Self::VecDiv { dst, lhs, rhs, .. } => {
                collector.reg_use(lhs);
                collector.reg_use(rhs);
                collector.reg_def(dst);
            }
            Self::VecBitwiseNot { dst, src } => {
                collector.reg_use(src);
                collector.reg_def(dst);
            }
            Self::VecMla {
                dst, acc, lhs, rhs, ..
            } => {
                // The leading `mov` writes `dst` before the read-modify-write
                // reads `lhs`/`rhs`, so `dst` is an *early* def: it must not
                // alias any use (coalescing dst with rhs would clobber rhs).
                collector.reg_use(acc);
                collector.reg_use(lhs);
                collector.reg_use(rhs);
                collector.reg_early_def(dst);
            }
            Self::VecSMull { dst, lhs, rhs, .. } => {
                collector.reg_use(lhs);
                collector.reg_use(rhs);
                collector.reg_def(dst);
            }
            Self::VecNarrow { high, dst, acc, src } => {
                if *high {
                    // `xtn2` preserves the destination's low half: the leading
                    // copy writes `dst` before the read-modify-write, so `dst`
                    // must not alias any use (early def).
                    collector.reg_use(acc);
                    collector.reg_use(src);
                    collector.reg_early_def(dst);
                } else {
                    collector.reg_use(src);
                    collector.reg_def(dst);
                }
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
            // f32 scalars share the NEON register bank with vectors: `sN` is
            // the low 32 bits of `vN`, so a single RegClass lets the allocator
            // prevent s/v aliasing (a Float-class vreg could otherwise be
            // assigned the same hw_enc as a live Vector-class vreg and be
            // silently clobbered). Emission renders these as `sN`.
            F32 => (&[RegClass::Vector], &[F32]),
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
            Self::StackAddr { .. } => {
                unreachable!("stack addresses must be legalized before emission")
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
            Self::AluRRR { .. }
            | Self::AluRRRR { .. }
            | Self::AluRRImm12 { .. }
            | Self::AluRRImmLogic { .. }
            | Self::AluRRImmShift { .. }
            | Self::AluRRRShift { .. }
            | Self::AluRRRExtend { .. }
            | Self::SDiv { .. }
            | Self::SMulL { .. }
            | Self::MAdd { .. }
            | Self::MSub { .. }
            | Self::CmpRR { .. }
            | Self::CmpImm { .. }
            | Self::SubsRRImm12 { .. }
            | Self::AndsRRImmLogic { .. }
            | Self::TstRRImmLogic { .. }
            | Self::Mov { .. }
            | Self::MovPhys { .. }
            | Self::LoadImm { .. }
            | Self::MovZ { .. }
            | Self::MovN { .. }
            | Self::MovK { .. }
            | Self::MovFromZero { .. }
            | Self::Sxtw { .. }
            | Self::LoadAddr { .. } => emit::emit_alu(self, ctx),
            Self::BCond { .. }
            | Self::Cbz { .. }
            | Self::Cbnz { .. }
            | Self::Tbz { .. }
            | Self::Tbnz { .. }
            | Self::CondBr { .. }
            | Self::Jump { .. }
            | Self::CSet { .. }
            | Self::CmpSelect { .. }
            | Self::CCmp { .. } => emit::emit_branch(self, ctx),
            Self::FMov { .. }
            | Self::VecMov { .. }
            | Self::VecLd1 { .. }
            | Self::VecSt1 { .. }
            | Self::VecDup { .. }
            | Self::VecArithRRR { .. }
            | Self::VecFmla { .. }
            | Self::VecBitwise { .. }
            | Self::VecCmp { .. }
            | Self::VecBsl { .. }
            | Self::VecCvt { .. }
            | Self::VecAddv { .. }
            | Self::VecMovImm { .. }
            | Self::VecExtractLane { .. }
            | Self::VecInsertLane { .. }
            | Self::VecMinMax { .. }
            | Self::VecShift { .. }
            | Self::VecDiv { .. }
            | Self::VecNeg { .. }
            | Self::VecBitwiseNot { .. }
            | Self::VecMla { .. }
            | Self::VecSMull { .. }
            | Self::VecNarrow { .. }
            | Self::FMovFromZero { .. }
            | Self::FAlu { .. }
            | Self::FCmp { .. }
            | Self::Scvtf { .. }
            | Self::Fcvtzs { .. } => emit::emit_neon(self, ctx),
            Self::Load { .. }
            | Self::Store { .. }
            | Self::LoadPair { .. }
            | Self::StorePair { .. } => emit::emit_memory(self, ctx),
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

/// Emit an add/sub-immediate form (`op rd, rn, #imm`, optionally
/// `, lsl #12`).  Shared by `AluRRImm12` and the fused `SubsRRImm12`.
fn emit_add_sub_imm12(
    ctx: &mut dyn EmitContext,
    mnemonic: &str,
    size: OperandSize,
    dst: WritableReg,
    src: Reg,
    imm: Imm12,
) -> core::fmt::Result {
    write!(ctx, "{mnemonic} ")?;
    emit_reg(ctx, dst.to_reg(), size)?;
    write!(ctx, ", ")?;
    emit_reg(ctx, src, size)?;
    write!(ctx, ", #{}", imm.value())?;
    if imm.shift12() {
        write!(ctx, ", lsl #12")?;
    }
    Ok(())
}

/// Emit a move-wide form (`movz`/`movn`/`movk`): `op rd, #imm`, optionally
/// `, lsl #shift`.
fn emit_move_wide(
    ctx: &mut dyn EmitContext,
    mnemonic: &str,
    size: OperandSize,
    dst: WritableReg,
    imm: MoveWideConst,
) -> core::fmt::Result {
    write!(ctx, "{mnemonic} ")?;
    emit_reg(ctx, dst.to_reg(), size)?;
    write!(ctx, ", #0x{:x}", imm.bits())?;
    if imm.shift() != 0 {
        write!(ctx, ", lsl #{}", imm.shift())?;
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
    // f32 scalars live in Vector-class registers (`sN` ≡ `vN` lane 0), so
    // both the scalar-FP class and the vector bank render as `sN`/`dN` here.
    let is_f32_reg = |reg: Reg| {
        reg.to_real_reg()
            .is_none_or(|preg| matches!(preg.class(), RegClass::Float | RegClass::Vector))
    };
    let dst_float = is_f32_reg(dst);
    let src_float = is_f32_reg(*src);
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
        // f32 scalars are Vector-class vregs; the scalar-FP view of a NEON
        // register (`sN`/`dN`) is just its low 32/64 bits, so Float and
        // Vector classes render identically in scalar-FP contexts.
        Some(preg) if matches!(preg.class(), RegClass::Float | RegClass::Vector) => {
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
        Some(preg) if matches!(preg.class(), RegClass::Float | RegClass::Vector) => {
            write!(ctx, "{}{}", if wide { "d" } else { "s" }, preg.hw_enc())
        }
        _ => ctx.write_reg(&reg),
    }
}

/// Emit a three-register vector instruction with the same arrangement on all
/// three operands (`{op} v{d}.<arr>, v{l}.<arr>, v{r}.<arr>`).  Kept as a
/// single helper so the arith/compare/minmax families cannot drift apart.
fn emit_vec_rrr(
    ctx: &mut dyn EmitContext,
    mnemonic: &str,
    dst: WritableReg,
    shape: VecShape,
    lhs: Reg,
    rhs: Reg,
) -> core::fmt::Result {
    write!(ctx, "{mnemonic} ")?;
    emit_vec_reg(ctx, dst.to_reg())?;
    write!(ctx, ".{}, ", shape.arrangement())?;
    emit_vec_reg(ctx, lhs)?;
    write!(ctx, ".{}, ", shape.arrangement())?;
    emit_vec_reg(ctx, rhs)?;
    write!(ctx, ".{}", shape.arrangement())
}
fn vec_arith_name(op: VecArithOp) -> &'static str {
    match op {
        VecArithOp::Add => "add",
        VecArithOp::Sub => "sub",
        VecArithOp::Mul => "mul",
        VecArithOp::Fadd => "fadd",
        VecArithOp::Fsub => "fsub",
        VecArithOp::Fmul => "fmul",
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
fn vec_mla_name(op: VecMlaOp) -> &'static str {
    match op {
        VecMlaOp::Mla => "mla",
        VecMlaOp::Mls => "mls",
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
fn vec_shift_name(op: VecShiftOp, is_reg: bool) -> &'static str {
    match (op, is_reg) {
        // Immediate forms: `shl` / `ushr` / `sshr v.4s, v.4s, #imm`.
        (VecShiftOp::Shl, false) => "shl",
        (VecShiftOp::Shr, false) => "ushr",
        (VecShiftOp::Sar, false) => "sshr",
        // Register forms: `sshl` / `ushl v.4s, v.4s, v.4s`. Right shifts use
        // a pre-negated amount vector (`neg` emitted before this), because
        // NEON has no register-form right-shift instruction.
        (VecShiftOp::Shl, true) => "sshl",
        (VecShiftOp::Shr, true) => "ushl",
        (VecShiftOp::Sar, true) => "sshl",
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
mod tests;

