//! Emission for scalar memory instructions.

use taki_mir::vcode::EmitContext;

use super::super::{MInst, emit_amode, emit_data_reg, emit_pair_amode};

pub(super) fn emit(inst: &MInst, ctx: &mut dyn EmitContext) -> core::fmt::Result {
    match inst {
        MInst::Load { ty, dst, addr } => {
            write!(ctx, "ldr ")?;
            emit_data_reg(ctx, dst.to_reg(), *ty)?;
            write!(ctx, ", ")?;
            emit_amode(ctx, addr)
        }
        MInst::Store { ty, src, addr } => {
            write!(ctx, "str ")?;
            emit_data_reg(ctx, *src, *ty)?;
            write!(ctx, ", ")?;
            emit_amode(ctx, addr)
        }
        MInst::LoadPair {
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
        MInst::StorePair {
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
        _ => unreachable!("memory emitter called for non-memory instruction"),
    }
}
