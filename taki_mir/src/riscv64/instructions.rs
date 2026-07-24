use smallvec::SmallVec;

use crate::{
    abi::{ArgPair, CallArgPair, CallRetPair, RetPair, StackAMode},
    reg_alloc::reg::{OperandVisitorImpl, PRegSet, RegClass},
    register::{Reg, Writable},
    riscv64::{abi::Riscv64ABI, labels::Label},
    types::{F32, I32, I64, LoweredType},
    vcode::{CallType, EmitContext, MachInst, MachInstEmit, MachTerminator},
};

pub type WritableReg = Writable<Reg>;

impl MachInst for MInst {
    type ABISpec = Riscv64ABI;

    fn get_operands(&mut self, collector: &mut impl crate::reg_alloc::reg::OperandVisitor) {
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
                collector.reg_use(rs);
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
            MInst::Args { pairs } => {
                for pair in pairs {
                    collector.reg_fixed_def(&mut pair.vreg, pair.preg);
                }
            }
            MInst::Ret => {}
            MInst::RetVal { pair } => {
                collector.reg_fixed_use(&mut pair.vreg, pair.preg);
            }
            MInst::Jump { .. } => {}
            MInst::JumpReg { rs } => {
                collector.reg_use(rs);
            }
            MInst::CondBr { cond, .. } => {
                collector.reg_use(cond);
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

    fn is_term(&self) -> crate::vcode::MachTerminator {
        match self {
            MInst::Jump { .. } | MInst::JumpReg { .. } | MInst::CondBr { .. } => {
                MachTerminator::Branch
            }
            MInst::Ret => MachTerminator::Return,
            _ => MachTerminator::None,
        }
    }

    fn call_type(&self) -> crate::vcode::CallType {
        match self {
            MInst::Call { .. } => CallType::Call,
            _ => CallType::None,
        }
    }

    fn is_mem_access(&self) -> bool {
        matches!(self, MInst::LoadWord { .. } | MInst::StoreWord { .. })
    }

    fn rc_for_type(
        ty: crate::types::LoweredType,
    ) -> (
        &'static [crate::reg_alloc::reg::RegClass],
        &'static [crate::types::LoweredType],
    ) {
        match ty {
            I32 => (&[RegClass::Int], &[I32]),
            I64 => (&[RegClass::Int], &[I64]),
            F32 => (&[RegClass::Float], &[F32]),
            _ => unreachable!(),
        }
    }

    fn gen_jump(target: crate::block_order::MirBlockIndex) -> Self {
        MInst::Jump {
            label: crate::riscv64::labels::Label::Block(target),
        }
    }
}

fn call_clobbers(mut clobbers: PRegSet, ret: Option<&CallRetPair>) -> PRegSet {
    if let Some(ret) = ret {
        clobbers.remove(ret.preg.to_real_reg().unwrap());
    }
    clobbers
}

#[cfg(test)]
mod tests {
    use super::call_clobbers;
    use crate::{
        abi::CallRetPair,
        register::Writable,
        riscv64::{
            abi::DEFAULT_CLOBBERS,
            regs::{a0, f_reg, fa0, pf_reg, px_reg, x_reg},
        },
    };

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
            MInst::Args { .. } => write!(ctx, "# args"),
            MInst::Ret => write!(ctx, "ret"),
            MInst::RetVal { .. } => Ok(()),
            MInst::Jump { label } => {
                // CFG labels are symbolic until emission. Use the post-RA scratch
                // register so allocator edge moves cannot clobber a jump target.
                write!(ctx, "la t6, ")?;
                label.emit(ctx)?;
                write!(ctx, "\n    jr t6")
            }
            MInst::JumpReg { rs } => {
                write!(ctx, "jr ")?;
                ctx.write_reg(rs)
            }
            MInst::CondBr {
                cond,
                true_label,
                false_label,
            } => {
                write!(ctx, "beqz ")?;
                ctx.write_reg(cond)?;
                writeln!(ctx, ", 1f")?;
                write!(ctx, "    la t6, ")?;
                true_label.emit(ctx)?;
                writeln!(ctx, "")?;
                writeln!(ctx, "    jr t6")?;
                writeln!(ctx, "1:")?;
                write!(ctx, "    la t6, ")?;
                false_label.emit(ctx)?;
                write!(ctx, "\n    jr t6")
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
    Args {
        pairs: SmallVec<[ArgPair; 8]>,
    },
    Ret,
    RetVal {
        pair: RetPair,
    },
    Jump {
        label: Label,
    },
    JumpReg {
        rs: Reg,
    },
    CondBr {
        cond: Reg,
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
        use crate::riscv64::regs::{
            fp_reg, stack_reg, writable_spilltmp_reg, writable_spilltmp_reg2,
        };

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
