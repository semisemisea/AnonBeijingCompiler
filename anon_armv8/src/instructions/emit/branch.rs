//! Emission for branch and conditional-select instructions.

use taki_mir::{emit_buffer::LabelKind, vcode::EmitContext};

use crate::regs::OperandSize;

use super::super::{
    MInst, SelectValue, branch_prefix, branch_prefix_bit, cond_name, emit_ccmp, emit_float_reg,
    emit_reg, emit_select_cmp, invert_cond,
};

pub(crate) fn emit(inst: &MInst, ctx: &mut dyn EmitContext) -> core::fmt::Result {
    match inst {
        MInst::BCond { cond, label } => {
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
        MInst::Cbz {
            size,
            reg,
            true_label,
            false_label,
        }
        | MInst::Cbnz {
            size,
            reg,
            true_label,
            false_label,
        } => {
            let (mnemonic, inverted_mnemonic) = match inst {
                MInst::Cbz { .. } => ("cbz", "cbnz"),
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
        MInst::Tbz {
            size,
            reg,
            bit,
            true_label,
            false_label,
        }
        | MInst::Tbnz {
            size,
            reg,
            bit,
            true_label,
            false_label,
        } => {
            let (mnemonic, inverted_mnemonic) = match inst {
                MInst::Tbz { .. } => ("tbz", "tbnz"),
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
        MInst::CondBr {
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
        MInst::Jump { label } => {
            let target = label
                .block()
                .expect("Jump target must be an intra-function block");
            ctx.put_uncond_branch("b ", target, LabelKind::BRANCH26)
        }
        MInst::CSet { cond, dst } => {
            write!(ctx, "cset ")?;
            emit_reg(ctx, dst.to_reg(), OperandSize::Size32)?;
            write!(ctx, ", {}", cond_name(*cond))
        }
        MInst::CCmp {
            size,
            lhs,
            rhs,
            imm,
            nzcv,
            cond,
        } => emit_ccmp(ctx, *size, *lhs, rhs, *imm, *nzcv, *cond),
        MInst::CmpSelect {
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
        _ => unreachable!("branch emitter called for non-branch instruction"),
    }
}
