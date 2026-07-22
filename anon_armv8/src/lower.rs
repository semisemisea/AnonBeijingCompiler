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
    instructions::{AluOp, Cond, Imm12, MInst},
    labels::Label,
    regs::{self, Gpr, OperandSize},
};

pub struct AArch64Backend;

impl LowerBackend for AArch64Backend {
    type MInst = MInst;

    fn lower(ctx: &mut LowerContext<Self::MInst>, inst: raana_ir::opt::prelude::Inst) {
        let inst_data = ctx.arena.inst_data(inst);
        match inst_data.kind() {
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
                let rhs = ctx.put_value_in_reg(binary.rhs());
                let dst = Writable::from_reg(ctx.reg_map[&inst]);
                let size = operand_size(ctx.arena.inst_data(binary.lhs()).ty().kind());

                match binary.op() {
                    BinaryOp::Add
                    | BinaryOp::Sub
                    | BinaryOp::Mul
                    | BinaryOp::And
                    | BinaryOp::Or
                    | BinaryOp::Xor
                    | BinaryOp::Shl
                    | BinaryOp::Shr
                    | BinaryOp::Sar => ctx.emit(MInst::AluRRR {
                        op: alu_op(binary.op()),
                        size,
                        dst,
                        lhs,
                        rhs,
                    }),
                    BinaryOp::Div => ctx.emit(MInst::SDiv {
                        size,
                        dst,
                        lhs,
                        rhs,
                    }),
                    BinaryOp::Rem => {
                        let quotient = ctx.alloc_tmp(HirType::get_i32());
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
                        ctx.emit(MInst::CmpRR {
                            size,
                            lhs,
                            rhs: Gpr::Reg(rhs),
                        });
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
        match ctx.arena.inst_data(inst).kind() {
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
