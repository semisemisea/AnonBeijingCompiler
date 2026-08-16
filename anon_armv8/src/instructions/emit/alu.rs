//! Emission for ALU/address instructions.

use taki_mir::vcode::EmitContext;

use crate::regs::{Gpr, OperandSize};

use super::super::{
    MInst, alu_name, emit_add_sub_imm12, emit_gpr, emit_load_imm, emit_move_wide, emit_reg,
    emit_reg_or_zr, emit_sized_data_rrr, emit_sized_rrr, emit_sized_rrrr, extend_name,
    extend_source_size, shift_name,
};

pub(crate) fn emit(inst: &MInst, ctx: &mut dyn EmitContext) -> core::fmt::Result {
    match inst {
        MInst::AluRRR {
            op,
            size,
            dst,
            lhs,
            rhs,
        } => emit_sized_data_rrr(ctx, alu_name(*op), *size, dst.to_reg(), lhs, rhs),
        MInst::AluRRRR {
            op,
            size,
            dst,
            lhs,
            rhs,
            carry,
        } => emit_sized_rrrr(ctx, alu_name(*op), *size, dst.to_reg(), lhs, rhs, carry),
        MInst::AluRRImm12 {
            op,
            size,
            dst,
            src,
            imm,
        } => emit_add_sub_imm12(ctx, alu_name(*op), *size, *dst, *src, *imm),
        MInst::AluRRImmLogic {
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
        MInst::AluRRImmShift {
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
        MInst::AluRRRShift {
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
        MInst::AluRRRExtend {
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
        MInst::SDiv {
            size,
            dst,
            lhs,
            rhs,
        } => emit_sized_rrr(ctx, "sdiv", *size, dst.to_reg(), lhs, rhs),
        MInst::SMulL { dst, lhs, rhs } => {
            write!(ctx, "smull ")?;
            emit_reg(ctx, dst.to_reg(), OperandSize::Size64)?;
            write!(ctx, ", ")?;
            emit_reg(ctx, *lhs, OperandSize::Size32)?;
            write!(ctx, ", ")?;
            emit_reg(ctx, *rhs, OperandSize::Size32)
        }
        MInst::MAdd {
            size,
            dst,
            lhs,
            rhs,
            addend,
        } => emit_sized_rrrr(ctx, "madd", *size, dst.to_reg(), lhs, rhs, addend),
        MInst::MSub {
            size,
            dst,
            lhs,
            rhs,
            subtrahend,
        } => emit_sized_rrrr(ctx, "msub", *size, dst.to_reg(), lhs, rhs, subtrahend),
        MInst::CmpRR { size, lhs, rhs } => {
            write!(ctx, "cmp ")?;
            emit_reg(ctx, *lhs, *size)?;
            write!(ctx, ", ")?;
            emit_reg_or_zr(ctx, rhs, *size)
        }
        MInst::CmpImm { size, lhs, imm } => {
            write!(ctx, "cmp ")?;
            emit_reg(ctx, *lhs, *size)?;
            write!(ctx, ", #{}", imm.value())?;
            if imm.shift12() {
                write!(ctx, ", lsl #12")?;
            }
            Ok(())
        }
        MInst::SubsRRImm12 {
            size,
            dst,
            src,
            imm,
        } => emit_add_sub_imm12(ctx, "subs", *size, *dst, *src, *imm),
        MInst::AndsRRImmLogic {
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
        MInst::TstRRImmLogic { size, src, imm } => {
            write!(ctx, "tst ")?;
            emit_reg_or_zr(ctx, src, *size)?;
            write!(ctx, ", #0x{:x}", imm.value())
        }
        MInst::Mov { size, dst, src } | MInst::MovPhys { size, dst, src } => {
            write!(ctx, "mov ")?;
            emit_reg(ctx, dst.to_reg(), *size)?;
            write!(ctx, ", ")?;
            emit_reg(ctx, *src, *size)
        }
        MInst::LoadImm { size, dst, value } => emit_load_imm(ctx, dst.to_reg(), *value, *size),
        MInst::MovZ { size, dst, imm } => emit_move_wide(ctx, "movz", *size, *dst, *imm),
        MInst::MovN { size, dst, imm } => emit_move_wide(ctx, "movn", *size, *dst, *imm),
        MInst::MovK { size, dst, imm, .. } => emit_move_wide(ctx, "movk", *size, *dst, *imm),
        MInst::MovFromZero { size, dst } => {
            write!(ctx, "mov ")?;
            emit_reg(ctx, dst.to_reg(), *size)?;
            write!(ctx, ", ")?;
            emit_gpr(ctx, &Gpr::Zr, *size)
        }
        MInst::Sxtw { size, dst, src } => {
            write!(ctx, "sxtw ")?;
            emit_reg(ctx, dst.to_reg(), OperandSize::Size64)?;
            write!(ctx, ", ")?;
            emit_reg(ctx, *src, *size)
        }
        MInst::LoadAddr { dst, label } => {
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
        _ => unreachable!("alu emitter called for non-alu instruction"),
    }
}
