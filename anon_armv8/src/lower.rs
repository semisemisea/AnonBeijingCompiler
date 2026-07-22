//! AArch64 selection from Raana HIR into generic VCode.

use raana_ir::ir::{BinaryOp, InstKind, Type as HirType, TypeKind, arena::Arena};
use taki_mir::{
    abi::RetPair,
    block_order::{LoweredBlock, MirBlockIndex},
    lower::{LowerBackend, LowerContext},
    prelude::HirFunctionData,
    reg_alloc::reg::PReg,
    register::Writable,
    vcode::MachInst,
};

use crate::{
    instructions::{AluOp, Cond, Imm12, ImmLogic, ImmShift, MInst},
    labels::Label,
    regs::{self, Gpr, OperandSize},
};

pub struct AArch64Backend;

impl LowerBackend for AArch64Backend {
    type MInst = MInst;

    fn lower(ctx: &mut LowerContext<Self::MInst>, inst: raana_ir::opt::prelude::Inst) {
        let kind = ctx.arena.inst_data(inst).kind().clone();
        match kind {
            InstKind::BlockArgRef(..)
            | InstKind::FuncArgRef(..)
            | InstKind::Aggregate(..)
            | InstKind::GlobalAlloc(..)
            | InstKind::Undef
            | InstKind::ZeroInit
            | InstKind::Integer(..)
            | InstKind::Float(..) => {
                unreachable!("constants and argument references are rematerialized by LowerContext")
            }
            InstKind::Binary(binary) => {
                let lhs = ctx.put_value_in_reg(binary.lhs());
                let dst = Writable::from_reg(ctx.reg_map[&inst]);
                let size = operand_size(ctx.arena.inst_data(binary.lhs()).ty().kind());
                let rhs_imm = integer_constant(ctx, binary.rhs());

                match binary.op() {
                    BinaryOp::Add | BinaryOp::Sub => {
                        if let Some((op, imm)) = add_sub_immediate(binary.op(), rhs_imm) {
                            ctx.emit(MInst::AluRRImm12 {
                                op,
                                size,
                                dst,
                                src: Gpr::Reg(lhs),
                                imm,
                            });
                        } else {
                            ctx.emit(MInst::AluRRR {
                                op: alu_op(binary.op()),
                                size,
                                dst,
                                lhs,
                                rhs: ctx.put_value_in_reg(binary.rhs()),
                            });
                        }
                    }
                    BinaryOp::And | BinaryOp::Or | BinaryOp::Xor => {
                        if let Some(imm) =
                            rhs_imm.and_then(|value| ImmLogic::new(integer_bits(value, size), size))
                        {
                            ctx.emit(MInst::AluRRImmLogic {
                                op: alu_op(binary.op()),
                                size,
                                dst,
                                src: Gpr::Reg(lhs),
                                imm,
                            });
                        } else {
                            ctx.emit(MInst::AluRRR {
                                op: alu_op(binary.op()),
                                size,
                                dst,
                                lhs,
                                rhs: ctx.put_value_in_reg(binary.rhs()),
                            });
                        }
                    }
                    BinaryOp::Shl | BinaryOp::Shr | BinaryOp::Sar => {
                        if let Some(shift) = rhs_imm
                            .and_then(|value| u8::try_from(value).ok())
                            .and_then(|value| ImmShift::new(value, size))
                        {
                            ctx.emit(MInst::AluRRImmShift {
                                op: alu_op(binary.op()),
                                size,
                                dst,
                                src: lhs,
                                shift,
                            });
                        } else {
                            ctx.emit(MInst::AluRRR {
                                op: alu_op(binary.op()),
                                size,
                                dst,
                                lhs,
                                rhs: ctx.put_value_in_reg(binary.rhs()),
                            });
                        }
                    }
                    BinaryOp::Mul => ctx.emit(MInst::AluRRR {
                        op: alu_op(binary.op()),
                        size,
                        dst,
                        lhs,
                        rhs: ctx.put_value_in_reg(binary.rhs()),
                    }),
                    BinaryOp::Div => ctx.emit(MInst::SDiv {
                        size,
                        dst,
                        lhs,
                        rhs: ctx.put_value_in_reg(binary.rhs()),
                    }),
                    BinaryOp::Rem => {
                        let quotient = ctx.alloc_tmp(HirType::get_i32());
                        let rhs = ctx.put_value_in_reg(binary.rhs());
                        ctx.emit(MInst::SDiv {
                            size,
                            dst: Writable::from_reg(quotient),
                            lhs,
                            rhs,
                        });
                        ctx.emit(MInst::MSub {
                            size,
                            dst,
                            lhs: quotient,
                            rhs,
                            subtrahend: lhs,
                        });
                    }
                    BinaryOp::Eq
                    | BinaryOp::NotEq
                    | BinaryOp::Gt
                    | BinaryOp::Lt
                    | BinaryOp::Ge
                    | BinaryOp::Le => {
                        if let Some(imm) = rhs_imm.and_then(positive_imm12) {
                            ctx.emit(MInst::CmpImm { size, lhs, imm });
                        } else {
                            ctx.emit(MInst::CmpRR {
                                size,
                                lhs,
                                rhs: Gpr::Reg(ctx.put_value_in_reg(binary.rhs())),
                            });
                        }
                        ctx.emit(MInst::CSet {
                            cond: comparison_cond(binary.op()),
                            dst,
                        });
                    }
                }
            }
            InstKind::Return(ret) => {
                if let Some(value) = ret.value() {
                    let src = ctx.put_value_in_reg(value);
                    let preg = match ctx.arena.inst_data(value).ty().kind() {
                        TypeKind::Int32 | TypeKind::Pointer(_) | TypeKind::String => {
                            regs::INT_RETURN_REG
                        }
                        ty => unreachable!("unsupported AArch64 return type: {ty:?}"),
                    };
                    ctx.emit(MInst::RetVal {
                        pair: RetPair { vreg: src, preg },
                    });
                }
                ctx.emit(MInst::Ret);
            }
            InstKind::Jump(..) | InstKind::Branch(..) => {
                unreachable!("terminators are lowered by LowerBackend::lower_branch")
            }
            kind => unreachable!("AArch64 lowering is not implemented for {kind:?}"),
        }
    }

    fn lower_branch(
        ctx: &mut LowerContext<Self::MInst>,
        inst: raana_ir::opt::prelude::Inst,
        target: &[MirBlockIndex],
    ) {
        let kind = ctx.arena.inst_data(inst).kind().clone();
        match kind {
            InstKind::Return(..) => Self::lower(ctx, inst),
            InstKind::Jump(jump) => {
                for &arg in jump.args() {
                    ctx.put_value_in_reg(arg);
                }
                let &[target] = target else {
                    unreachable!("jump must have one lowered successor");
                };
                ctx.emit(MInst::gen_jump(target));
            }
            InstKind::Branch(branch) => {
                let cond = ctx.put_value_in_reg(branch.cond());
                for &arg in branch.t_args().iter().chain(branch.f_args()) {
                    ctx.put_value_in_reg(arg);
                }
                let &[true_target, false_target] = target else {
                    unreachable!("branch must have two lowered successors");
                };
                ctx.emit(MInst::CmpImm {
                    size: OperandSize::Size32,
                    lhs: cond,
                    imm: Imm12::new(0, false).unwrap(),
                });
                ctx.emit(MInst::CondBr {
                    cond: Cond::Ne,
                    true_label: Label::from_block(true_target),
                    false_label: Label::from_block(false_target),
                });
            }
            _ => unreachable!("non-terminator passed to AArch64 branch lowering"),
        }
    }

    fn data_section_directive() -> &'static str {
        ".section .data"
    }

    fn text_section_directive() -> &'static str {
        ".section .text"
    }

    fn global_directive() -> &'static str {
        ".globl"
    }

    fn word_directive() -> &'static str {
        ".word"
    }

    fn zero_directive() -> &'static str {
        ".zero"
    }

    fn preg_name(preg: PReg) -> &'static str {
        regs::preg_name(preg)
    }

    fn format_block_label(lb: &LoweredBlock, func_data: &HirFunctionData) -> String {
        match lb {
            LoweredBlock::Orig { block } => {
                format!(".L_{}", func_data.bb_data(*block).name().replace('%', "_"))
            }
            LoweredBlock::Edge { pred, succ, .. } => format!(
                ".L_{}_to_{}_edge",
                func_data.bb_data(*pred).name().replace('%', "_"),
                func_data.bb_data(*succ).name().replace('%', "_")
            ),
        }
    }

    fn emit_long_jump(ctx: &mut LowerContext<Self::MInst>, target: MirBlockIndex) {
        ctx.emit(MInst::gen_jump(target));
    }
}

fn integer_constant(
    ctx: &LowerContext<'_, MInst>,
    inst: raana_ir::opt::prelude::Inst,
) -> Option<i32> {
    match ctx.arena.inst_data(inst).kind() {
        InstKind::Integer(value) => Some(value.value()),
        _ => None,
    }
}

fn add_sub_immediate(op: BinaryOp, value: Option<i32>) -> Option<(AluOp, Imm12)> {
    let value = i64::from(value?);
    let (op, magnitude) = match (op, value.is_negative()) {
        (BinaryOp::Add, false) | (BinaryOp::Sub, true) => (AluOp::Add, value.unsigned_abs()),
        (BinaryOp::Sub, false) | (BinaryOp::Add, true) => (AluOp::Sub, value.unsigned_abs()),
        _ => unreachable!("only add and sub have immediate forms"),
    };
    let (value, shift12) = if magnitude <= 0xfff {
        (magnitude, false)
    } else if magnitude % 4096 == 0 && magnitude / 4096 <= 0xfff {
        (magnitude / 4096, true)
    } else {
        return None;
    };
    Some((op, Imm12::new(value as u16, shift12).unwrap()))
}

fn positive_imm12(value: i32) -> Option<Imm12> {
    if value < 0 {
        return None;
    }
    let value = value as u32;
    if value <= 0xfff {
        Imm12::new(value as u16, false)
    } else if value % 4096 == 0 && value / 4096 <= 0xfff {
        Imm12::new((value / 4096) as u16, true)
    } else {
        None
    }
}

fn integer_bits(value: i32, size: OperandSize) -> u64 {
    match size {
        OperandSize::Size32 => value as u32 as u64,
        OperandSize::Size64 => value as i64 as u64,
    }
}

fn operand_size(ty: &TypeKind) -> OperandSize {
    match ty {
        TypeKind::Int32 => OperandSize::Size32,
        TypeKind::Pointer(_) | TypeKind::String => OperandSize::Size64,
        ty => unreachable!("unsupported AArch64 scalar type: {ty:?}"),
    }
}

fn alu_op(op: BinaryOp) -> AluOp {
    match op {
        BinaryOp::Add => AluOp::Add,
        BinaryOp::Sub => AluOp::Sub,
        BinaryOp::Mul => AluOp::Mul,
        BinaryOp::And => AluOp::And,
        BinaryOp::Or => AluOp::Orr,
        BinaryOp::Xor => AluOp::Eor,
        BinaryOp::Shl => AluOp::Lsl,
        BinaryOp::Shr => AluOp::Lsr,
        BinaryOp::Sar => AluOp::Asr,
        _ => unreachable!("binary operation has no direct AArch64 ALU form"),
    }
}

fn comparison_cond(op: BinaryOp) -> Cond {
    match op {
        BinaryOp::Eq => Cond::Eq,
        BinaryOp::NotEq => Cond::Ne,
        BinaryOp::Gt => Cond::Gt,
        BinaryOp::Lt => Cond::Lt,
        BinaryOp::Ge => Cond::Ge,
        BinaryOp::Le => Cond::Le,
        _ => unreachable!("binary operation is not a comparison"),
    }
}
