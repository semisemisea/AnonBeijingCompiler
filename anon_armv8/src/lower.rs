//! AArch64 selection from Raana HIR into generic VCode.

use raana_ir::ir::{arena::Arena, BinaryOp, InstKind, Type as HirType, TypeKind};
use taki_mir::{
    abi::{CallArgPair, CallRetPair, RetPair},
    block_order::{LoweredBlock, MirBlockIndex},
    lower::{LowerBackend, LowerContext},
    prelude::HirFunctionData,
    reg_alloc::reg::PReg,
    register::Writable,
    vcode::MachInst,
};

use crate::{
    instructions::{
        AMode, AluOp, Cond, ExtendOp, FpuOp, Imm12, ImmLogic, ImmShift, MInst, MemoryType, ShiftOp,
    },
    labels::Label,
    regs::{self, Gpr, OperandSize, RegOrZr},
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
            | InstKind::Integer(..)
            | InstKind::Float(..) => {
                unreachable!("constants and argument references are rematerialized by LowerContext")
            }
            InstKind::Binary(binary) => {
                let lhs = ctx.put_value_in_reg(binary.lhs());
                let dst = Writable::from_reg(ctx.reg_map[&inst]);
                if matches!(
                    ctx.arena.inst_data(binary.lhs()).ty().kind(),
                    TypeKind::Float32
                ) {
                    let rhs = ctx.put_value_in_reg(binary.rhs());
                    match binary.op() {
                        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div => {
                            ctx.emit(MInst::FAlu {
                                op: float_alu_op(binary.op()),
                                dst,
                                lhs,
                                rhs,
                            });
                        }
                        BinaryOp::Eq
                        | BinaryOp::NotEq
                        | BinaryOp::Gt
                        | BinaryOp::Lt
                        | BinaryOp::Ge
                        | BinaryOp::Le => {
                            ctx.emit(MInst::FCmp { lhs, rhs });
                            ctx.emit(MInst::CSet {
                                cond: float_comparison_cond(binary.op()),
                                dst,
                            });
                        }
                        op => unreachable!("unsupported AArch64 floating binary operation: {op:?}"),
                    }
                    return;
                }
                let size = operand_size(ctx.arena.inst_data(binary.lhs()).ty().kind());
                let rhs_imm = integer_constant(ctx, binary.rhs());

                match binary.op() {
                    BinaryOp::Add | BinaryOp::Sub => {
                        if binary.op() == BinaryOp::Add
                            && integer_constant(ctx, binary.lhs()) == Some(0)
                        {
                            ctx.emit(MInst::Mov {
                                size,
                                dst,
                                src: ctx.put_value_in_reg(binary.rhs()),
                            });
                        } else if rhs_imm == Some(0) {
                            ctx.emit(MInst::Mov {
                                size,
                                dst,
                                src: lhs,
                            });
                        } else if binary.op() == BinaryOp::Sub
                            && integer_constant(ctx, binary.lhs()) == Some(0)
                        {
                            ctx.emit(MInst::AluRRR {
                                op: AluOp::Sub,
                                size,
                                dst,
                                lhs: RegOrZr::Zr,
                                rhs: RegOrZr::Reg(ctx.put_value_in_reg(binary.rhs())),
                            });
                        } else if let Some((op, imm)) = add_sub_immediate(binary.op(), rhs_imm) {
                            ctx.emit(MInst::AluRRImm12 {
                                op,
                                size,
                                dst,
                                src: Gpr::Reg(lhs),
                                imm,
                            });
                        } else if let Some((rhs, shift, amount)) =
                            fold_shifted_rhs(ctx, inst, binary.rhs(), size)
                        {
                            ctx.emit(MInst::AluRRRShift {
                                op: alu_op(binary.op()),
                                size,
                                dst,
                                lhs: RegOrZr::Reg(lhs),
                                rhs: RegOrZr::Reg(rhs),
                                shift,
                                amount,
                            });
                        } else {
                            ctx.emit(MInst::AluRRR {
                                op: alu_op(binary.op()),
                                size,
                                dst,
                                lhs: RegOrZr::Reg(lhs),
                                rhs: RegOrZr::Reg(ctx.put_value_in_reg(binary.rhs())),
                            });
                        }
                    }
                    BinaryOp::And | BinaryOp::Or | BinaryOp::Xor => {
                        let lhs_imm = integer_constant(ctx, binary.lhs());
                        if binary.op() == BinaryOp::And
                            && (lhs_imm == Some(0) || rhs_imm == Some(0))
                        {
                            ctx.emit(MInst::MovFromZero { size, dst });
                        } else if matches!(binary.op(), BinaryOp::Or | BinaryOp::Xor)
                            && lhs_imm == Some(0)
                        {
                            ctx.emit(MInst::Mov {
                                size,
                                dst,
                                src: ctx.put_value_in_reg(binary.rhs()),
                            });
                        } else if matches!(binary.op(), BinaryOp::Or | BinaryOp::Xor)
                            && rhs_imm == Some(0)
                        {
                            ctx.emit(MInst::Mov {
                                size,
                                dst,
                                src: lhs,
                            });
                        } else if let Some(imm) =
                            rhs_imm.and_then(|value| ImmLogic::new(integer_bits(value, size), size))
                        {
                            ctx.emit(MInst::AluRRImmLogic {
                                op: alu_op(binary.op()),
                                size,
                                dst,
                                src: RegOrZr::Reg(lhs),
                                imm,
                            });
                        } else if let Some((rhs, shift, amount)) =
                            fold_shifted_rhs(ctx, inst, binary.rhs(), size)
                        {
                            ctx.emit(MInst::AluRRRShift {
                                op: alu_op(binary.op()),
                                size,
                                dst,
                                lhs: RegOrZr::Reg(lhs),
                                rhs: RegOrZr::Reg(rhs),
                                shift,
                                amount,
                            });
                        } else {
                            ctx.emit(MInst::AluRRR {
                                op: alu_op(binary.op()),
                                size,
                                dst,
                                lhs: RegOrZr::Reg(lhs),
                                rhs: RegOrZr::Reg(ctx.put_value_in_reg(binary.rhs())),
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
                                lhs: RegOrZr::Reg(lhs),
                                rhs: RegOrZr::Reg(ctx.put_value_in_reg(binary.rhs())),
                            });
                        }
                    }
                    BinaryOp::Mul => ctx.emit(MInst::AluRRR {
                        op: alu_op(binary.op()),
                        size,
                        dst,
                        lhs: RegOrZr::Reg(lhs),
                        rhs: RegOrZr::Reg(ctx.put_value_in_reg(binary.rhs())),
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
                                rhs: RegOrZr::Reg(ctx.put_value_in_reg(binary.rhs())),
                            });
                        }
                        ctx.emit(MInst::CSet {
                            cond: comparison_cond(binary.op()),
                            dst,
                        });
                    }
                }
            }
            InstKind::Cast(cast) => {
                let src = cast.src();
                let src_ty = ctx.arena.inst_data(src).ty().kind().clone();
                let dst_ty = ctx.arena.inst_data(inst).ty().kind().clone();
                let dst = Writable::from_reg(ctx.reg_map[&inst]);
                let src = ctx.put_value_in_reg(src);
                match (src_ty, dst_ty) {
                    (TypeKind::Int32, TypeKind::Float32) => ctx.emit(MInst::Scvtf { dst, src }),
                    (TypeKind::Float32, TypeKind::Int32) => ctx.emit(MInst::Fcvtzs { dst, src }),
                    (src_ty, dst_ty) => {
                        unreachable!("unsupported AArch64 cast: {src_ty:?} -> {dst_ty:?}")
                    }
                }
            }
            InstKind::Alloc => {
                let dst = Writable::from_reg(ctx.reg_map[&inst]);
                let pointee = ctx.arena.inst_data(inst).ty().derefernce();
                let offset = i64::from(ctx.alloc_stackslot_or_get(inst, pointee));
                emit_stack_address(ctx, dst, offset);
            }
            InstKind::GetElemPtr(gep) => {
                let dst = Writable::from_reg(ctx.reg_map[&inst]);
                let mut current_ty = ctx.arena.inst_data(gep.base()).ty().clone();
                let mut address = ctx.put_value_in_reg(gep.base());

                for &index in gep.offsets() {
                    let element_ty = if current_ty.is_pointer() {
                        current_ty.derefernce()
                    } else {
                        current_ty.get_array_elem_ty()
                    };
                    let stride = element_ty.size() as i64;
                    current_ty = element_ty;

                    if let Some(value) = integer_constant(ctx, index) {
                        let byte_offset = i64::from(value) * stride;
                        let next = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
                        emit_add_offset(ctx, Writable::from_reg(next), address, byte_offset);
                        address = next;
                    } else {
                        if let Some(shift) = stride_shift(stride) {
                            let next = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
                            ctx.emit(MInst::AluRRRExtend {
                                op: AluOp::Add,
                                size: OperandSize::Size64,
                                dst: Writable::from_reg(next),
                                lhs: Gpr::Reg(address),
                                rhs: ctx.put_value_in_reg(index),
                                extend: ExtendOp::Sxtw,
                                shift,
                            });
                            address = next;
                            continue;
                        }
                        // Indices are i32 in Raana IR. Sign-extend before the
                        // multiply so negative indices retain GEP semantics.
                        let extended = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
                        ctx.emit(MInst::MovFromZero {
                            size: OperandSize::Size64,
                            dst: Writable::from_reg(extended),
                        });
                        ctx.emit(MInst::AluRRRExtend {
                            op: AluOp::Add,
                            size: OperandSize::Size64,
                            dst: Writable::from_reg(extended),
                            lhs: Gpr::Reg(extended),
                            rhs: ctx.put_value_in_reg(index),
                            extend: ExtendOp::Sxtw,
                            shift: 0,
                        });
                        let byte_offset = if stride == 1 {
                            extended
                        } else {
                            let scale = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
                            ctx.emit(MInst::LoadImm {
                                size: OperandSize::Size64,
                                dst: Writable::from_reg(scale),
                                value: stride as u64,
                            });
                            let product = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
                            ctx.emit(MInst::AluRRR {
                                op: AluOp::Mul,
                                size: OperandSize::Size64,
                                dst: Writable::from_reg(product),
                                lhs: RegOrZr::Reg(extended),
                                rhs: RegOrZr::Reg(scale),
                            });
                            product
                        };
                        let next = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
                        ctx.emit(MInst::AluRRR {
                            op: AluOp::Add,
                            size: OperandSize::Size64,
                            dst: Writable::from_reg(next),
                            lhs: RegOrZr::Reg(address),
                            rhs: RegOrZr::Reg(byte_offset),
                        });
                        address = next;
                    }
                }
                assert_eq!(
                    current_ty.reference(),
                    ctx.arena.inst_data(inst).ty().clone()
                );
                ctx.emit(MInst::Mov {
                    size: OperandSize::Size64,
                    dst,
                    src: address,
                });
            }
            InstKind::Load(load) => {
                let dst = Writable::from_reg(ctx.reg_map[&inst]);
                let src = ctx.put_value_in_reg(load.src());
                ctx.emit(MInst::Load {
                    ty: memory_type(ctx.arena.inst_data(inst).ty().kind()),
                    dst,
                    addr: AMode::Reg {
                        base: Gpr::Reg(src),
                    },
                });
            }
            InstKind::Store(store) => lower_store(ctx, store.src(), store.dest()),
            InstKind::ZeroInit => unreachable!("zero initialization is lowered by its store"),
            InstKind::Call(call) => {
                let mut args = Vec::new();
                let (mut int_index, mut float_index, mut stack_offset) = (0usize, 0usize, 0i64);
                for &arg in call.args() {
                    let src = ctx.put_value_in_reg(arg);
                    match ctx.arena.inst_data(arg).ty().kind() {
                        TypeKind::Int32 | TypeKind::Pointer(_) | TypeKind::String => {
                            if let Some(&preg) = regs::INT_ARG_REGS.get(int_index) {
                                args.push(CallArgPair { vreg: src, preg });
                                int_index += 1;
                            } else {
                                ctx.emit(MInst::Store {
                                    ty: memory_type(ctx.arena.inst_data(arg).ty().kind()),
                                    src,
                                    addr: AMode::OutgoingArg(stack_offset),
                                });
                                int_index += 1;
                                stack_offset += 8;
                            }
                        }
                        TypeKind::Float32 => {
                            if let Some(&preg) = regs::FLOAT_ARG_REGS.get(float_index) {
                                args.push(CallArgPair { vreg: src, preg });
                                float_index += 1;
                            } else {
                                ctx.emit(MInst::Store {
                                    ty: MemoryType::F32,
                                    src,
                                    addr: AMode::OutgoingArg(stack_offset),
                                });
                                float_index += 1;
                                stack_offset += 8;
                            }
                        }
                        ty => unreachable!("unsupported AArch64 call argument type: {ty:?}"),
                    }
                }
                let ret = match ctx.arena.inst_data(inst).ty().kind() {
                    TypeKind::Unit => None,
                    TypeKind::Int32 | TypeKind::Pointer(_) | TypeKind::String => {
                        Some(CallRetPair {
                            vreg: Writable::from_reg(ctx.reg_map[&inst]),
                            preg: regs::INT_RETURN_REG,
                        })
                    }
                    TypeKind::Float32 => Some(CallRetPair {
                        vreg: Writable::from_reg(ctx.reg_map[&inst]),
                        preg: regs::FLOAT_RETURN_REG,
                    }),
                    ty => unreachable!("unsupported AArch64 call return type: {ty:?}"),
                };
                ctx.emit(MInst::Call {
                    args,
                    ret,
                    clobbers: regs::DEFAULT_CLOBBERS,
                    label: Label::from_function(call.callee()),
                });
                ctx.vcode.vcode.abi.set_has_calls();
                ctx.vcode
                    .vcode
                    .abi
                    .set_outgoing_arg_size(stack_offset as usize);
            }
            InstKind::Return(ret) => {
                if let Some(value) = ret.value() {
                    let src = ctx.put_value_in_reg(value);
                    let preg = match ctx.arena.inst_data(value).ty().kind() {
                        TypeKind::Int32 | TypeKind::Pointer(_) | TypeKind::String => {
                            regs::INT_RETURN_REG
                        }
                        TypeKind::Float32 => regs::FLOAT_RETURN_REG,
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

/// Fold `rhs = input <<const shift` (or its logical/arithmetic right-shift
/// counterparts) into an AArch64 shifted-register data-processing operand.
/// The generic context atomically claims the producer before we name its
/// input, preventing its later reverse-traversal lowering.
fn fold_shifted_rhs(
    ctx: &mut LowerContext<'_, MInst>,
    consumer: raana_ir::opt::prelude::Inst,
    rhs: raana_ir::opt::prelude::Inst,
    size: OperandSize,
) -> Option<(taki_mir::register::Reg, ShiftOp, ImmShift)> {
    let InstKind::Binary(shift) = ctx.arena.inst_data(rhs).kind().clone() else {
        return None;
    };
    let shift_op = match shift.op() {
        BinaryOp::Shl => ShiftOp::Lsl,
        BinaryOp::Shr => ShiftOp::Lsr,
        BinaryOp::Sar => ShiftOp::Asr,
        _ => return None,
    };
    let amount = integer_constant(ctx, shift.rhs())
        .and_then(|value| u8::try_from(value).ok())
        .and_then(|value| ImmShift::new(value, size))?;
    if !ctx.sink_pure_single_use_producer(rhs, consumer) {
        return None;
    }
    Some((ctx.put_value_in_reg(shift.lhs()), shift_op, amount))
}

fn stride_shift(stride: i64) -> Option<u8> {
    match stride {
        1 => Some(0),
        2 => Some(1),
        4 => Some(2),
        8 => Some(3),
        16 => Some(4),
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

fn memory_type(ty: &TypeKind) -> MemoryType {
    match ty {
        TypeKind::Int32 => MemoryType::I32,
        TypeKind::Float32 => MemoryType::F32,
        TypeKind::Pointer(_) | TypeKind::String => MemoryType::I64,
        ty => unreachable!("unsupported AArch64 integer memory type: {ty:?}"),
    }
}

fn lower_store(
    ctx: &mut LowerContext<'_, MInst>,
    src: raana_ir::opt::prelude::Inst,
    dest: raana_ir::opt::prelude::Inst,
) {
    let base = ctx.put_value_in_reg(dest);
    match ctx.arena.inst_data(src).kind().clone() {
        InstKind::Aggregate(aggregate) => {
            let mut offset = 0i64;
            for value in aggregate.flatten(&ctx.arena) {
                let ty = ctx.arena.inst_data(value).ty().clone();
                if matches!(ctx.arena.inst_data(value).kind(), InstKind::ZeroInit) {
                    emit_zero_init(ctx, base, &ty, offset);
                } else {
                    let value = ctx.put_value_in_reg(value);
                    emit_store_at(ctx, value, &ty, base, offset);
                }
                offset += ty.size() as i64;
            }
        }
        InstKind::ZeroInit => {
            let ty = ctx.arena.inst_data(src).ty().clone();
            emit_zero_init(ctx, base, &ty, 0);
        }
        _ => {
            let ty = ctx.arena.inst_data(src).ty().clone();
            let value = ctx.put_value_in_reg(src);
            emit_store_at(ctx, value, &ty, base, 0);
        }
    }
}

fn emit_zero_init(
    ctx: &mut LowerContext<'_, MInst>,
    base: taki_mir::register::Reg,
    ty: &HirType,
    offset: i64,
) {
    match ty.kind() {
        TypeKind::Array(element, len) => {
            let stride = element.size() as i64;
            for index in 0..*len {
                emit_zero_init(ctx, base, element, offset + index as i64 * stride);
            }
        }
        TypeKind::Int32 | TypeKind::Pointer(_) | TypeKind::String => {
            let zero = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
            ctx.emit(MInst::MovFromZero {
                size: if matches!(ty.kind(), TypeKind::Int32) {
                    OperandSize::Size32
                } else {
                    OperandSize::Size64
                },
                dst: Writable::from_reg(zero),
            });
            emit_store_at(ctx, zero, ty, base, offset);
        }
        TypeKind::Float32 => {
            let zero = ctx.alloc_tmp(HirType::get_f32());
            ctx.emit(MInst::FMovFromZero {
                dst: Writable::from_reg(zero),
            });
            emit_store_at(ctx, zero, ty, base, offset);
        }
        ty => unreachable!("cannot zero-initialize AArch64 type: {ty:?}"),
    }
}

fn emit_store_at(
    ctx: &mut LowerContext<'_, MInst>,
    src: taki_mir::register::Reg,
    ty: &HirType,
    base: taki_mir::register::Reg,
    offset: i64,
) {
    let memory_ty = memory_type(ty.kind());
    ctx.emit(MInst::Store {
        ty: memory_ty,
        src,
        addr: memory_address(ctx, base, offset, memory_ty),
    });
}

fn memory_address(
    ctx: &mut LowerContext<'_, MInst>,
    base: taki_mir::register::Reg,
    offset: i64,
    ty: MemoryType,
) -> AMode {
    if offset == 0 {
        return AMode::Reg {
            base: Gpr::Reg(base),
        };
    }
    if offset > 0 {
        if let Some(offset) = crate::instructions::UImm12Scaled::new(offset as u64, ty.byte_size())
        {
            return AMode::UnsignedOffset {
                base: Gpr::Reg(base),
                offset,
            };
        }
    }
    if let Ok(offset) = i16::try_from(offset) {
        if let Some(offset) = crate::instructions::SImm9::new(offset) {
            return AMode::SignedOffset {
                base: Gpr::Reg(base),
                offset,
            };
        }
    }

    let address = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
    emit_add_offset(ctx, Writable::from_reg(address), base, offset);
    AMode::Reg {
        base: Gpr::Reg(address),
    }
}

fn emit_stack_address(
    ctx: &mut LowerContext<'_, MInst>,
    dst: Writable<taki_mir::register::Reg>,
    offset: i64,
) {
    if let Some((AluOp::Add, imm)) = add_sub_immediate(BinaryOp::Add, i32::try_from(offset).ok()) {
        ctx.emit(MInst::AluRRImm12 {
            op: AluOp::Add,
            size: OperandSize::Size64,
            dst,
            src: Gpr::Sp,
            imm,
        });
    } else {
        let constant = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
        ctx.emit(MInst::LoadImm {
            size: OperandSize::Size64,
            dst: Writable::from_reg(constant),
            value: offset as u64,
        });
        // The regular three-register form cannot name SP as its left operand.
        let sp_copy = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
        ctx.emit(MInst::MovPhys {
            size: OperandSize::Size64,
            dst: Gpr::Reg(sp_copy),
            src: Gpr::Sp,
        });
        ctx.emit(MInst::AluRRR {
            op: AluOp::Add,
            size: OperandSize::Size64,
            dst,
            lhs: RegOrZr::Reg(sp_copy),
            rhs: RegOrZr::Reg(constant),
        });
    }
}

fn emit_add_offset(
    ctx: &mut LowerContext<'_, MInst>,
    dst: Writable<taki_mir::register::Reg>,
    base: taki_mir::register::Reg,
    offset: i64,
) {
    if let Some((op, imm)) = add_sub_immediate(BinaryOp::Add, i32::try_from(offset).ok()) {
        ctx.emit(MInst::AluRRImm12 {
            op,
            size: OperandSize::Size64,
            dst,
            src: Gpr::Reg(base),
            imm,
        });
    } else {
        let constant = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
        ctx.emit(MInst::LoadImm {
            size: OperandSize::Size64,
            dst: Writable::from_reg(constant),
            value: offset as u64,
        });
        ctx.emit(MInst::AluRRR {
            op: AluOp::Add,
            size: OperandSize::Size64,
            dst,
            lhs: RegOrZr::Reg(base),
            rhs: RegOrZr::Reg(constant),
        });
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

fn float_alu_op(op: BinaryOp) -> FpuOp {
    match op {
        BinaryOp::Add => FpuOp::Add,
        BinaryOp::Sub => FpuOp::Sub,
        BinaryOp::Mul => FpuOp::Mul,
        BinaryOp::Div => FpuOp::Div,
        _ => unreachable!("unsupported AArch64 floating binary operation: {op:?}"),
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

fn float_comparison_cond(op: BinaryOp) -> Cond {
    match op {
        BinaryOp::Eq => Cond::Eq,
        BinaryOp::NotEq => Cond::Ne,
        BinaryOp::Gt => Cond::Gt,
        // `fcmp` sets N for ordered less-than and C for unordered operands.
        // `mi` and `ls` therefore implement ordered `<` and `<=` respectively.
        BinaryOp::Lt => Cond::Mi,
        BinaryOp::Ge => Cond::Ge,
        BinaryOp::Le => Cond::Ls,
        _ => unreachable!("binary operation is not a floating comparison"),
    }
}
