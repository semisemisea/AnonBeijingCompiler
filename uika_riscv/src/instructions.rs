use smallvec::SmallVec;

use taki_mir::{
    abi::{ArgPair, CallArgPair, CallRetPair, RetPair, StackAMode},
    emit_buffer::LabelKind,
    reg_alloc::reg::{OperandVisitorImpl, PRegSet, RegClass},
    register::{Reg, Writable},
    types::{F32, I32, I64, LoweredType},
    vcode::{EmitContext, MachInst, MachInstEmit, MachTerminator},
};

use crate::{abi::Riscv64ABI, labels::Label, regs::preg_name};

pub type WritableReg = Writable<Reg>;

impl MachInst for MInst {
    type ABISpec = Riscv64ABI;

    fn get_operands(&mut self, collector: &mut impl taki_mir::reg_alloc::reg::OperandVisitor) {
        match self {
            MInst::Nop => {}
            MInst::AluRRR { rd, rs1, rs2, .. } => {
                collector.reg_use(rs1);
                collector.reg_use(rs2);
                collector.reg_def(rd);
            }
            MInst::AluRRImm12 { rd, rs, .. } => {
                collector.reg_use(rs);
                collector.reg_def(rd);
            }
            MInst::AluRRImmShift { rd, rs, .. } => {
                collector.reg_use(rs);
                collector.reg_def(rd);
            }
            MInst::Slli { rd, rs, .. } | MInst::Srai { rd, rs, .. } => {
                collector.reg_use(rs);
                collector.reg_def(rd);
            }
            MInst::LoadImm { rd, .. } => {
                collector.reg_def(rd);
            }
            MInst::LoadAddr { rd, .. } => {
                collector.reg_def(rd);
            }
            MInst::StackAddr { rd, .. } => {
                collector.reg_def(rd);
            }
            MInst::LoadWord { rd, addr, .. } => {
                collector.reg_def(rd);
                if let AMode::RegOffest(base, _) = addr {
                    collector.reg_use(base);
                }
            }
            MInst::Fcvt { rd, rs, .. } => {
                collector.reg_use(rs);
                collector.reg_def(rd);
            }
            MInst::FpuRRR { rd, rs1, rs2, .. } => {
                collector.reg_use(rs1);
                collector.reg_use(rs2);
                collector.reg_def(rd);
            }
            MInst::StoreWord { rs, addr, .. } => {
                use_store_src(collector, rs);
                if let AMode::RegOffest(base, _) = addr {
                    collector.reg_use(base);
                }
            }
            MInst::Call {
                arg_pairs,
                ret,
                clobbers,
                ..
            } => {
                for arg_pair in arg_pairs {
                    collector.reg_fixed_use(&mut arg_pair.vreg, arg_pair.preg);
                }
                if let Some(ret_pair) = ret {
                    collector.reg_fixed_def(&mut ret_pair.vreg, ret_pair.preg);
                }
                collector.reg_clobbers(call_clobbers(*clobbers, ret.as_ref()));
            }
            MInst::Ret => {}
            MInst::TailCall {
                arg_pairs,
                clobbers,
                ..
            } => {
                for arg_pair in arg_pairs {
                    collector.reg_fixed_use(&mut arg_pair.vreg, arg_pair.preg);
                }
                collector.reg_clobbers(call_clobbers(*clobbers, None));
            }
            MInst::RetVal { pair } => {
                collector.reg_fixed_use(&mut pair.vreg, pair.preg);
            }
            MInst::Args { args } => {
                for arg_pair in args {
                    collector.reg_fixed_def(&mut arg_pair.vreg, arg_pair.preg);
                }
            }
            MInst::Jump { .. } => {}
            MInst::JumpReg { rs } => {
                collector.reg_use(rs);
            }
            MInst::CondBr { rs1, rs2, .. } => {
                collector.reg_use(rs1);
                collector.reg_use(rs2);
            }
            MInst::Mov { src, dst } => {
                collector.reg_use(src);
                collector.reg_def(dst);
            }
        }
    }

    fn is_move(&self) -> Option<(Writable<Reg>, Reg)> {
        match self {
            MInst::Mov { src, dst } => Some((*dst, *src)),
            _ => None,
        }
    }

    fn is_term(&self) -> taki_mir::vcode::MachTerminator {
        match self {
            MInst::CondBr { .. } | MInst::Jump { .. } | MInst::JumpReg { .. } => {
                MachTerminator::Branch
            }
            MInst::Ret | MInst::TailCall { .. } => MachTerminator::Return,
            _ => MachTerminator::None,
        }
    }

    fn rc_for_type(
        ty: taki_mir::types::LoweredType,
    ) -> (
        &'static [taki_mir::reg_alloc::reg::RegClass],
        &'static [taki_mir::types::LoweredType],
    ) {
        match ty {
            I32 => (&[RegClass::Int], &[I32]),
            I64 => (&[RegClass::Int], &[I64]),
            F32 => (&[RegClass::Float], &[F32]),
            _ => unreachable!(),
        }
    }

    fn gen_jump(target: taki_mir::block_order::MirBlockIndex) -> Self {
        MInst::Jump {
            label: crate::labels::Label::Block(target),
        }
    }
}

/// The ABI saves incoming register arguments with stores whose source is the
/// fixed physical argument register, which the allocator neither renames nor
/// tracks. Only virtual sources become operands, mirroring the AArch64 backend.
fn use_store_src(collector: &mut impl taki_mir::reg_alloc::reg::OperandVisitor, reg: &mut Reg) {
    if reg.is_virtual() {
        collector.reg_use(reg);
    }
}

fn call_clobbers(mut clobbers: PRegSet, ret: Option<&CallRetPair>) -> PRegSet {
    if let Some(ret) = ret {
        clobbers.remove(ret.preg.to_real_reg().unwrap());
    }
    clobbers
}

impl MachInstEmit for MInst {
    fn emit(&self, ctx: &mut dyn EmitContext) -> core::fmt::Result {
        match self {
            MInst::Nop => write!(ctx, "nop"),
            MInst::AluRRR { op, rd, rs1, rs2 } => {
                write!(ctx, "{} ", op)?;
                ctx.write_reg(&rd.reg)?;
                write!(ctx, ", ")?;
                ctx.write_reg(rs1)?;
                if !matches!(op, AluRRROP::Snez | AluRRROP::Seqz) {
                    write!(ctx, ", ")?;
                    ctx.write_reg(rs2)?;
                }
                Ok(())
            }
            MInst::AluRRImm12 { op, rd, rs, imm } => {
                write!(ctx, "{} ", op)?;
                ctx.write_reg(&rd.reg)?;
                write!(ctx, ", ")?;
                ctx.write_reg(rs)?;
                write!(ctx, ", {}", imm)
            }
            MInst::AluRRImmShift { op, rd, rs, shamt } => {
                write!(ctx, "{} ", op)?;
                ctx.write_reg(&rd.reg)?;
                write!(ctx, ", ")?;
                ctx.write_reg(rs)?;
                write!(ctx, ", {}", shamt.value())
            }
            MInst::Slli { rd, rs, shamt } => {
                write!(ctx, "slli ")?;
                ctx.write_reg(&rd.reg)?;
                write!(ctx, ", ")?;
                ctx.write_reg(rs)?;
                write!(ctx, ", {}", shamt.value())
            }
            MInst::Srai { rd, rs, shamt } => {
                write!(ctx, "srai ")?;
                ctx.write_reg(&rd.reg)?;
                write!(ctx, ", ")?;
                ctx.write_reg(rs)?;
                write!(ctx, ", {}", shamt.value())
            }
            MInst::LoadImm { rd, value } => {
                write!(ctx, "li ")?;
                ctx.write_reg(&rd.reg)?;
                write!(ctx, ", 0x{value:x}")
            }
            MInst::LoadAddr { rd, label } => {
                write!(ctx, "la ")?;
                ctx.write_reg(&rd.reg)?;
                write!(ctx, ", ")?;
                label.emit(ctx)
            }
            MInst::StackAddr { .. } => {
                unreachable!("stack addresses must be legalized before emission")
            }
            MInst::LoadWord { rd, op, addr } => {
                write!(ctx, "{} ", op)?;
                ctx.write_reg(&rd.reg)?;
                write!(ctx, ", ")?;
                addr.emit(ctx)
            }
            MInst::Fcvt { mode, rd, rs } => {
                write!(ctx, "{} ", mode)?;
                ctx.write_reg(&rd.reg)?;
                write!(ctx, ", ")?;
                ctx.write_reg(rs)?;
                if matches!(mode, FcvtMode::SinglePrecisionToWord) {
                    write!(ctx, ", rtz")?;
                }
                Ok(())
            }
            MInst::FpuRRR { op, rd, rs1, rs2 } => {
                write!(ctx, "{} ", op)?;
                ctx.write_reg(&rd.reg)?;
                write!(ctx, ", ")?;
                ctx.write_reg(rs1)?;
                write!(ctx, ", ")?;
                ctx.write_reg(rs2)
            }
            MInst::StoreWord { rs, op, addr } => {
                write!(ctx, "{} ", op)?;
                ctx.write_reg(rs)?;
                write!(ctx, ", ")?;
                addr.emit(ctx)
            }
            MInst::Call { label, .. } => {
                write!(ctx, "call ")?;
                label.emit(ctx)
            }
            MInst::TailCall { label, .. } => {
                // `tail` is the auipc+jalr pseudoinstruction (2 instructions,
                // one fewer than the previous `la t6, fn; jr t6` form).
                write!(ctx, "tail ")?;
                label.emit(ctx)
            }
            MInst::Ret => write!(ctx, "ret"),
            MInst::RetVal { .. } => Ok(()),
            MInst::Args { .. } => Ok(()),
            MInst::Jump { label } => {
                let Label::Block(target) = label else {
                    unreachable!("Jump target must be an intra-function block");
                };
                ctx.put_uncond_branch("j ", *target, LabelKind::RV_JAL)
            }
            MInst::JumpReg { rs } => {
                write!(ctx, "jr ")?;
                ctx.write_reg(rs)
            }
            MInst::CondBr {
                op,
                rs1,
                rs2,
                true_label,
                false_label,
            } => {
                let Label::Block(true_target) = true_label else {
                    unreachable!("CondBr true target must be an intra-function block");
                };
                let Label::Block(false_target) = false_label else {
                    unreachable!("CondBr false target must be an intra-function block");
                };
                let prefix = condbr_prefix(*op, rs1, rs2);
                let inv_prefix = condbr_prefix(op.inverted(), rs1, rs2);
                ctx.put_branch(&prefix, Some(&inv_prefix), *true_target, LabelKind::RV_B)?;
                ctx.put_uncond_branch("j ", *false_target, LabelKind::RV_JAL)
            }
            MInst::Mov { src, dst } => {
                let src_real = src.to_real_reg();
                let dst_real = dst.reg.to_real_reg();
                let (int_src, float_src) = match src_real {
                    Some(r) => (r.class() == RegClass::Int, r.class() == RegClass::Float),
                    None => (true, false),
                };
                let (int_dst, float_dst) = match dst_real {
                    Some(r) => (r.class() == RegClass::Int, r.class() == RegClass::Float),
                    None => (true, false),
                };
                match (
                    int_src && int_dst,
                    float_src && float_dst,
                    float_src && int_dst,
                    int_src && float_dst,
                ) {
                    (true, _, _, _) => {
                        write!(ctx, "mv ")?;
                        ctx.write_reg(&dst.reg)?;
                        write!(ctx, ", ")?;
                        ctx.write_reg(src)
                    }
                    (_, true, _, _) => {
                        write!(ctx, "fmv.s ")?;
                        ctx.write_reg(&dst.reg)?;
                        write!(ctx, ", ")?;
                        ctx.write_reg(src)
                    }
                    (_, _, true, _) => {
                        write!(ctx, "fmv.x.w ")?;
                        ctx.write_reg(&dst.reg)?;
                        write!(ctx, ", ")?;
                        ctx.write_reg(src)
                    }
                    _ => {
                        write!(ctx, "fmv.w.x ")?;
                        ctx.write_reg(&dst.reg)?;
                        write!(ctx, ", ")?;
                        ctx.write_reg(src)
                    }
                }
            }
        }
    }
}

/// A two-register RISC-V B-type branch condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CondBrOp {
    Beq,
    Bne,
    Blt,
    Bge,
}

impl CondBrOp {
    pub fn inverted(self) -> Self {
        match self {
            CondBrOp::Beq => CondBrOp::Bne,
            CondBrOp::Bne => CondBrOp::Beq,
            CondBrOp::Blt => CondBrOp::Bge,
            CondBrOp::Bge => CondBrOp::Blt,
        }
    }

    /// Assembly mnemonic of the branch.
    pub fn mnemonic(self) -> &'static str {
        match self {
            CondBrOp::Beq => "beq",
            CondBrOp::Bne => "bne",
            CondBrOp::Blt => "blt",
            CondBrOp::Bge => "bge",
        }
    }
}

/// Text prefix of a B-type branch (`"beq a4, a0, "`); the emit buffer appends
/// the target label. Operands must be physical after register allocation.
fn condbr_prefix(op: CondBrOp, rs1: &Reg, rs2: &Reg) -> String {
    let name = |reg: &Reg| {
        preg_name(
            reg.to_real_reg()
                .expect("CondBr operands must be physical after register allocation"),
        )
    };
    format!("{} {}, {}, ", op.mnemonic(), name(rs1), name(rs2))
}

#[derive(Debug, Clone)]
pub enum MInst {
    Nop,
    AluRRR {
        op: AluRRROP,
        rd: WritableReg,
        rs1: Reg,
        rs2: Reg,
    },
    AluRRImm12 {
        op: AluRRImm12OP,
        rd: WritableReg,
        rs: Reg,
        imm: Imm12,
    },
    AluRRImmShift {
        op: AluRRImmShiftOP,
        rd: WritableReg,
        rs: Reg,
        shamt: ShiftImm,
    },
    Slli {
        rd: WritableReg,
        rs: Reg,
        shamt: ShiftImm64,
    },
    /// The 64-bit arithmetic shift right. Division by a constant needs it to
    /// reach the high half of a 64-bit product, which the `W` shifts cannot.
    Srai {
        rd: WritableReg,
        rs: Reg,
        shamt: ShiftImm64,
    },
    LoadImm {
        rd: WritableReg,
        value: u64,
    },
    LoadWord {
        rd: WritableReg,
        op: LoadOP,
        addr: AMode,
    },
    LoadAddr {
        rd: WritableReg,
        label: Label,
    },
    StackAddr {
        rd: WritableReg,
        addr: AMode,
    },
    Fcvt {
        mode: FcvtMode,
        rd: WritableReg,
        rs: Reg,
    },
    FpuRRR {
        op: FpuRRROP,
        rd: WritableReg,
        rs1: Reg,
        rs2: Reg,
    },
    StoreWord {
        rs: Reg,
        op: StoreOP,
        addr: AMode,
    },
    Call {
        arg_pairs: SmallVec<[CallArgPair; 8]>,
        ret: Option<CallRetPair>,
        clobbers: PRegSet,
        label: Label,
    },
    /// Tail call: the epilogue (frame restore) is emitted ahead of this, then
    /// control jumps to `label` without linking, reusing the caller's frame.
    TailCall {
        arg_pairs: SmallVec<[CallArgPair; 8]>,
        clobbers: PRegSet,
        label: Label,
    },
    Ret,
    RetVal {
        pair: RetPair,
    },
    /// Bind incoming register parameters to their fixed ABI physical
    /// registers. Emits no machine code; register allocation resolves the
    /// fixed defs. Must be the first instruction of the entry block.
    Args {
        args: Vec<ArgPair>,
    },
    Jump {
        label: Label,
    },
    JumpReg {
        rs: Reg,
    },
    CondBr {
        op: CondBrOp,
        rs1: Reg,
        rs2: Reg,
        true_label: Label,
        false_label: Label,
    },
    Mov {
        src: Reg,
        dst: WritableReg,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadOP {
    Lw,
    Ld,
    Flw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOP {
    Sw,
    Sd,
    Fsw,
}

impl From<LoweredType> for LoadOP {
    fn from(value: LoweredType) -> Self {
        use LoadOP::*;
        match value {
            I32 => Lw,
            I64 => Ld,
            F32 => Flw,
            _ => unreachable!(),
        }
    }
}

impl From<LoweredType> for StoreOP {
    fn from(value: LoweredType) -> Self {
        use StoreOP::*;
        match value {
            I32 => Sw,
            I64 => Sd,
            F32 => Fsw,
            _ => unreachable!(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FcvtMode {
    WordToSinglePrecision,
    SinglePrecisionToWord,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Imm12 {
    bits: u16,
}

impl Imm12 {
    pub const ZERO: Self = Imm12 { bits: 0 };
    pub const ONE: Self = Imm12 { bits: 1 };

    pub fn from_i32(v: i32) -> Option<Self> {
        if (-2048..2048).contains(&v) {
            Some(Self {
                bits: (v as u16) & 0xFFF,
            })
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AluRRImm12OP {
    Addi,
    Xori,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AluRRImmShiftOP {
    SlliW,
    SrliW,
    SraiW,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShiftImm(u8);

impl ShiftImm {
    pub const fn new(value: u8) -> Option<Self> {
        if value < 32 { Some(Self(value)) } else { None }
    }

    pub const fn value(self) -> u8 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShiftImm64(u8);

impl ShiftImm64 {
    pub const fn new(value: u8) -> Option<Self> {
        if value < 64 { Some(Self(value)) } else { None }
    }

    pub const fn value(self) -> u8 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AluRRROP {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    AddW,
    SubW,
    MulW,
    DivW,
    RemW,
    Snez,
    Seqz,
    Slt,
    And,
    Or,
    Xor,
    Shl,
    Shr,
    Sar,
    ShlW,
    ShrW,
    SarW,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FpuRRROP {
    FaddS,
    FsubS,
    FmulS,
    FdivS,
    FeqS,
    FltS,
    FleS,
}

#[derive(Debug, Clone)]
pub enum AMode {
    SPOffset(i64),
    FPOffset(i64),
    RegOffest(Reg, i64),
    SlotOffset(i64),
    IncomingArg(i64),
    OutgoingArg(i64),
    Label(Label),
}

impl From<StackAMode> for AMode {
    fn from(value: StackAMode) -> Self {
        match value {
            StackAMode::IncomingArg(offset, _stack_arg_size) => AMode::IncomingArg(offset),
            StackAMode::Slot(slot_offset) => AMode::SlotOffset(slot_offset),
            StackAMode::OutgoingArg(offset) => AMode::OutgoingArg(offset),
        }
    }
}

impl core::fmt::Display for AluRRROP {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        use AluRRROP::*;
        write!(
            f,
            "{}",
            match self {
                Add => "add",
                Sub => "sub",
                Mul => "mul",
                Div => "div",
                Rem => "rem",
                AddW => "addw",
                SubW => "subw",
                MulW => "mulw",
                DivW => "divw",
                RemW => "remw",
                Snez => "snez",
                Seqz => "seqz",
                Slt => "slt",
                And => "and",
                Or => "or",
                Xor => "xor",
                Shl => "sll",
                Shr => "srl",
                Sar => "sra",
                ShlW => "sllw",
                ShrW => "srlw",
                SarW => "sraw",
            }
        )
    }
}

impl core::fmt::Display for FpuRRROP {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        use FpuRRROP::*;
        write!(
            f,
            "{}",
            match self {
                FaddS => "fadd.s",
                FsubS => "fsub.s",
                FmulS => "fmul.s",
                FdivS => "fdiv.s",
                FeqS => "feq.s",
                FltS => "flt.s",
                FleS => "fle.s",
            }
        )
    }
}

impl core::fmt::Display for AluRRImm12OP {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            AluRRImm12OP::Addi => write!(f, "addi"),
            AluRRImm12OP::Xori => write!(f, "xori"),
        }
    }
}

impl core::fmt::Display for AluRRImmShiftOP {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            AluRRImmShiftOP::SlliW => write!(f, "slliw"),
            AluRRImmShiftOP::SrliW => write!(f, "srliw"),
            AluRRImmShiftOP::SraiW => write!(f, "sraiw"),
        }
    }
}

impl core::fmt::Display for Imm12 {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        let v = ((self.bits as i16) << 4) >> 4;
        write!(f, "{}", v)
    }
}

impl core::fmt::Display for LoadOP {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            LoadOP::Lw => write!(f, "lw"),
            LoadOP::Ld => write!(f, "ld"),
            LoadOP::Flw => write!(f, "flw"),
        }
    }
}

impl core::fmt::Display for StoreOP {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            StoreOP::Sw => write!(f, "sw"),
            StoreOP::Sd => write!(f, "sd"),
            StoreOP::Fsw => write!(f, "fsw"),
        }
    }
}

impl core::fmt::Display for FcvtMode {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            FcvtMode::WordToSinglePrecision => write!(f, "fcvt.s.w"),
            FcvtMode::SinglePrecisionToWord => write!(f, "fcvt.w.s"),
        }
    }
}

impl AMode {
    fn emit(&self, ctx: &mut dyn EmitContext) -> core::fmt::Result {
        match self {
            AMode::SPOffset(off) => write!(ctx, "{}(sp)", off),
            AMode::FPOffset(off) => write!(ctx, "{}(s0)", off),
            AMode::RegOffest(r, off) => {
                write!(ctx, "{}(", off)?;
                ctx.write_reg(r)?;
                write!(ctx, ")")
            }
            AMode::SlotOffset(off) => write!(ctx, "{}(sp)", off),
            AMode::IncomingArg(off) => write!(ctx, "{}(s0)", off),
            AMode::OutgoingArg(off) => write!(ctx, "{}(sp)", off),
            AMode::Label(l) => l.emit(ctx),
        }
    }

    /// Normalize any offset-based AMode so the offset fits in 12-bit signed range.
    /// Returns (safe_amode, extra_instructions). If the offset is already valid,
    /// extra_instructions is empty. Otherwise, extra contains li+add and the
    /// returned AMode is RegOffest(tmp, 0) where tmp = x31 (spilltmp).
    pub fn normalize_imm12(&self) -> (AMode, SmallVec<[MInst; 3]>) {
        use crate::regs::{fp_reg, stack_reg, writable_spilltmp_reg, writable_spilltmp_reg2};

        let (off, base) = match *self {
            AMode::SPOffset(o) | AMode::SlotOffset(o) | AMode::OutgoingArg(o) => (o, stack_reg()),
            AMode::FPOffset(o) | AMode::IncomingArg(o) => (o, fp_reg()),
            _ => return (self.clone(), smallvec::SmallVec::new()),
        };

        if (-2048..2048).contains(&off) {
            return (self.clone(), smallvec::SmallVec::new());
        }

        let mut extra: SmallVec<[MInst; 3]> = smallvec::SmallVec::new();
        let tmp2 = writable_spilltmp_reg2();
        extra.push(MInst::LoadImm {
            rd: tmp2,
            value: off as u64,
        });
        let tmp = writable_spilltmp_reg();
        extra.push(MInst::AluRRR {
            op: AluRRROP::Add,
            rd: tmp,
            rs1: base,
            rs2: tmp2.to_reg(),
        });
        (AMode::RegOffest(tmp.to_reg(), 0), extra)
    }
}

#[cfg(test)]
mod tests {
    use super::{MInst, ShiftImm, ShiftImm64, call_clobbers};
    use crate::{
        abi::DEFAULT_CLOBBERS,
        regs::{a0, a1, f_reg, fa0, pf_reg, px_reg, x_reg},
    };
    use taki_mir::{
        abi::{ArgPair, CallRetPair},
        reg_alloc::reg::{OperandConstraint, OperandKind, RegClass, VReg},
        register::{Reg, Writable},
        vcode::{EmitContext, MachInst, MachInstEmit, MachTerminator},
    };

    #[derive(Default)]
    struct TestEmitContext(String);

    impl core::fmt::Write for TestEmitContext {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            self.0.push_str(text);
            Ok(())
        }
    }

    impl EmitContext for TestEmitContext {
        fn write_reg(&mut self, _reg: &Reg) -> core::fmt::Result {
            unreachable!("Args pseudo has no register operands to emit")
        }

        fn write_label_ref(
            &mut self,
            _idx: taki_mir::block_order::MirBlockIndex,
        ) -> core::fmt::Result {
            unreachable!("Args pseudo has no labels")
        }

        fn write_function_label(
            &mut self,
            _func: taki_mir::prelude::HirFunction,
        ) -> core::fmt::Result {
            unreachable!("Args pseudo has no labels")
        }

        fn write_global_label(&mut self, _gv: taki_mir::prelude::HirInst) -> core::fmt::Result {
            unreachable!("Args pseudo has no labels")
        }

        fn write_external_symbol(&mut self, symbol: &str) -> core::fmt::Result {
            self.0.push_str(symbol);
            Ok(())
        }
    }

    fn virtual_reg(index: usize, class: RegClass) -> Reg {
        Reg::from_virtual_reg(VReg::new(192 + index, class))
    }

    fn int_args() -> MInst {
        MInst::Args {
            args: vec![
                ArgPair {
                    vreg: Writable::from_reg(virtual_reg(0, RegClass::Int)),
                    preg: a0(),
                },
                ArgPair {
                    vreg: Writable::from_reg(virtual_reg(1, RegClass::Int)),
                    preg: a1(),
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
        let mut ctx = TestEmitContext::default();
        int_args().emit(&mut ctx).unwrap();
        assert_eq!(ctx.0, "");
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
            let expected = [a0(), a1()][index];
            assert_eq!(*preg, expected.to_physical_reg().unwrap());
            assert_eq!(vreg.class(), RegClass::Int);
        }
    }

    #[test]
    fn shift_immediates_enforce_operand_width() {
        assert!(ShiftImm::new(0).is_some());
        assert!(ShiftImm::new(31).is_some());
        assert!(ShiftImm::new(32).is_none());

        assert!(ShiftImm64::new(0).is_some());
        assert!(ShiftImm64::new(31).is_some());
        assert!(ShiftImm64::new(32).is_some());
        assert!(ShiftImm64::new(63).is_some());
        assert!(ShiftImm64::new(64).is_none());
    }

    #[test]
    fn call_clobbers_exclude_the_fixed_return_register() {
        let int = call_clobbers(
            DEFAULT_CLOBBERS,
            Some(&CallRetPair {
                vreg: Writable::from_reg(x_reg(5)),
                preg: a0(),
            }),
        );
        let float = call_clobbers(
            DEFAULT_CLOBBERS,
            Some(&CallRetPair {
                vreg: Writable::from_reg(f_reg(5)),
                preg: fa0(),
            }),
        );

        assert!(!int.contains(px_reg(10)));
        assert!(!float.contains(pf_reg(10)));
        assert!(int.contains(px_reg(11)));
        assert!(float.contains(pf_reg(11)));
    }
}
