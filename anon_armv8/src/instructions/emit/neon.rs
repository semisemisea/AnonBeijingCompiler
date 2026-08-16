//! Emission for NEON and scalar floating-point instructions.

use taki_mir::vcode::EmitContext;

use crate::regs::OperandSize;

use super::super::{
    MInst, emit_float_reg, emit_float_rr, emit_float_rrr, emit_fmov, emit_reg, emit_vec_reg,
    emit_vec_rrr, emit_vec_scalar_reg, fpu_name, vec_arith_name, vec_bit_name, vec_cmp_name,
    vec_cvt_name, vec_minmax_name,
};

pub(crate) fn emit(inst: &MInst, ctx: &mut dyn EmitContext) -> core::fmt::Result {
    match inst {
        MInst::FMov { dst, src } => emit_fmov(ctx, dst.to_reg(), src),
        MInst::VecMov { dst, src } => {
            write!(ctx, "mov ")?;
            emit_vec_reg(ctx, dst.to_reg())?;
            write!(ctx, ".16b, ")?;
            emit_vec_reg(ctx, *src)?;
            write!(ctx, ".16b")
        }
        MInst::VecLd1 { dst, base } => {
            write!(ctx, "ld1 {{")?;
            emit_vec_reg(ctx, dst.to_reg())?;
            write!(ctx, ".16b}}, [")?;
            emit_reg(ctx, *base, OperandSize::Size64)?;
            write!(ctx, "]")
        }
        MInst::VecSt1 { src, base } => {
            write!(ctx, "st1 {{")?;
            emit_vec_reg(ctx, *src)?;
            write!(ctx, ".16b}}, [")?;
            emit_reg(ctx, *base, OperandSize::Size64)?;
            write!(ctx, "]")
        }
        MInst::VecDup { shape, dst, src } => {
            write!(ctx, "dup ")?;
            emit_vec_reg(ctx, dst.to_reg())?;
            write!(ctx, ".{}, ", shape.arrangement())?;
            emit_vec_scalar_reg(ctx, *src, *shape)
        }
        MInst::VecArithRRR {
            op,
            shape,
            dst,
            lhs,
            rhs,
        } => emit_vec_rrr(ctx, vec_arith_name(*op), *dst, *shape, *lhs, *rhs),
        MInst::VecFmla {
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
        MInst::VecBitwise { op, dst, lhs, rhs } => {
            write!(ctx, "{} ", vec_bit_name(*op))?;
            emit_vec_reg(ctx, dst.to_reg())?;
            write!(ctx, ".16b, ")?;
            emit_vec_reg(ctx, *lhs)?;
            write!(ctx, ".16b, ")?;
            emit_vec_reg(ctx, *rhs)?;
            write!(ctx, ".16b")
        }
        MInst::VecCmp {
            op,
            shape,
            dst,
            lhs,
            rhs,
        } => emit_vec_rrr(ctx, vec_cmp_name(*op), *dst, *shape, *lhs, *rhs),
        MInst::VecBsl {
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
        MInst::VecCvt {
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
        MInst::VecAddv { dst, src } => {
            write!(ctx, "addv ")?;
            emit_float_reg(ctx, dst.to_reg(), false)?;
            write!(ctx, ", ")?;
            emit_vec_reg(ctx, *src)?;
            write!(ctx, ".4s")
        }
        MInst::VecMovImm {
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
        MInst::VecExtractLane {
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
        MInst::VecInsertLane {
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
        MInst::VecMinMax {
            op,
            shape,
            dst,
            lhs,
            rhs,
        } => emit_vec_rrr(ctx, vec_minmax_name(*op), *dst, *shape, *lhs, *rhs),
        MInst::FMovFromZero { dst } => {
            write!(ctx, "fmov ")?;
            emit_float_reg(ctx, dst.to_reg(), false)?;
            write!(ctx, ", wzr")
        }
        MInst::FAlu { op, dst, lhs, rhs } => {
            emit_float_rrr(ctx, fpu_name(*op), dst.to_reg(), lhs, rhs)
        }
        MInst::FCmp { lhs, rhs } => emit_float_rr(ctx, "fcmp", *lhs, rhs),
        MInst::Scvtf { dst, src } => {
            write!(ctx, "scvtf ")?;
            emit_float_reg(ctx, dst.to_reg(), false)?;
            write!(ctx, ", ")?;
            emit_reg(ctx, *src, OperandSize::Size32)
        }
        MInst::Fcvtzs { dst, src } => {
            write!(ctx, "fcvtzs ")?;
            emit_reg(ctx, dst.to_reg(), OperandSize::Size32)?;
            write!(ctx, ", ")?;
            emit_float_reg(ctx, *src, false)
        }
        _ => unreachable!("neon emitter called for non-neon instruction"),
    }
}
