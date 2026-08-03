//! AArch64 selection from Raana HIR into generic VCode.

use raana_ir::ir::{
    Binary, BinaryOp, Call, Cast, Fma, GetElemPtr, InstKind, Load, Return, Select, Store,
    TailCall, Type as HirType, TypeKind, VectorExtractElement, VectorInsertElement, VectorReduce,
    VectorReduceOp, VectorSplat, arena::Arena, inst_kind::MemZero,
};
use taki_mir::{
    abi::{ABIMachineSpec, ArgSlot, CallArgPair, CallRetPair, RetPair, StackAMode},
    block_order::{LoweredBlock, MirBlockIndex},
    div_magic::{MagicCorrection, signed_magic_i32},
    lower::{
        LowerBackend, LowerContext, LoweredOutput, analyze_gep, fold_gep_constant_offset,
        sink_gep_into_address,
    },
    prelude::{ArenaContext, HirFunctionData, HirInst},
    reg_alloc::reg::PReg,
    register::Writable,
    vcode::MachInst,
};

use crate::{
    abi::AArch64Abi,
    instructions::{
        AMode, AluOp, CCmpStep, Cond, ExtendOp, FpuOp, Imm12, ImmLogic, ImmShift, MInst,
        MemoryType, SelectCmp, SelectValue, ShiftOp, VecArithOp, VecBitOp, VecCmpOp, VecCvtOp,
        VecMinMaxOp, VecShape,
    },
    labels::Label,
    regs::{self, OperandSize, RegOrZr},
    runtime::{self, EmbeddedSymbol},
};
use taki_mir::register::Reg;

pub struct AArch64Backend;

fn lower_binary(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    binary: &Binary,
) -> LoweredOutput {
    let result = ctx.result_reg(inst);
    let dst = Writable::from_reg(result);
    if matches!(
        arena.inst_data(binary.lhs()).ty().kind(),
        TypeKind::Vector(..)
    ) {
        return lower_vector_binary(ctx, arena, inst, binary);
    }
    if matches!(arena.inst_data(binary.lhs()).ty().kind(), TypeKind::Float32) {
        let lhs = ctx.put_value_in_reg(binary.lhs());
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
            op => {
                ctx.lowering_panic(
                    "AArch64 instruction selection",
                    format!("floating binary operation {op:?} is unsupported"),
                    Some(arena.inst_data(binary.lhs()).ty()),
                    Some(arena.inst_data(inst).ty()),
                );
            }
        }
        return LoweredOutput::Value(result);
    }
    let size = operand_size(arena.inst_data(binary.lhs()).ty().kind());
    if let Some((op, lhs, rhs, addend)) = fold_mul_add_sub(
        ctx,
        arena,
        inst,
        binary.op(),
        binary.lhs(),
        binary.rhs(),
        size,
    ) {
        match op {
            BinaryOp::Add => ctx.emit(MInst::MAdd {
                size,
                dst,
                lhs,
                rhs,
                addend,
            }),
            BinaryOp::Sub => ctx.emit(MInst::MSub {
                size,
                dst,
                lhs,
                rhs,
                subtrahend: addend,
            }),
            _ => unreachable!("multiply-accumulate folding only selects add or sub"),
        }
        return LoweredOutput::Value(result);
    }
    // A constant multiplier with a single-instruction form folds before any
    // operand is materialized, so the constant itself never loads.
    if binary.op() == BinaryOp::Mul {
        if let Some(output) = try_fold_mul_constant(ctx, arena, inst, binary, dst) {
            return output;
        }
    }
    let lhs = ctx.put_value_in_reg(binary.lhs());
    let rhs_imm = integer_constant(arena, binary.rhs());

    if binary.op() == BinaryOp::Div && rhs_imm == Some(1) {
        return LoweredOutput::Value(lhs);
    }

    if matches!(arena.inst_data(inst).ty().kind(), TypeKind::Int32)
        && matches!(binary.op(), BinaryOp::Div | BinaryOp::Rem)
        && rhs_imm.is_some_and(|divisor| {
            lower_signed_div_rem_power_of_two(ctx, binary.op(), dst, lhs, divisor)
                || lower_signed_div_rem_magic(ctx, binary.op(), dst, lhs, divisor)
        })
    {
        return LoweredOutput::Value(result);
    }

    match binary.op() {
        BinaryOp::Add | BinaryOp::Sub => {
            if binary.op() == BinaryOp::Add && integer_constant(arena, binary.lhs()) == Some(0) {
                let rhs = ctx.put_value_in_reg(binary.rhs());
                ctx.emit(MInst::Mov {
                    size,
                    dst,
                    src: rhs,
                });
            } else if rhs_imm == Some(0) {
                ctx.emit(MInst::Mov {
                    size,
                    dst,
                    src: lhs,
                });
            } else if binary.op() == BinaryOp::Sub
                && integer_constant(arena, binary.lhs()) == Some(0)
            {
                let rhs = ctx.put_value_in_reg(binary.rhs());
                ctx.emit(MInst::AluRRR {
                    op: AluOp::Sub,
                    size,
                    dst,
                    lhs: RegOrZr::Zr,
                    rhs: RegOrZr::Reg(rhs),
                });
            } else if let Some((op, imm)) = add_sub_immediate(binary.op(), rhs_imm) {
                ctx.emit(MInst::AluRRImm12 {
                    op,
                    size,
                    dst,
                    src: lhs,
                    imm,
                });
            } else if let Some((rhs, shift, amount)) =
                fold_shifted_rhs(ctx, arena, inst, binary.rhs(), size)
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
                let rhs = ctx.put_value_in_reg(binary.rhs());
                ctx.emit(MInst::AluRRR {
                    op: alu_op(binary.op()),
                    size,
                    dst,
                    lhs: RegOrZr::Reg(lhs),
                    rhs: RegOrZr::Reg(rhs),
                });
            }
        }
        BinaryOp::And | BinaryOp::Or | BinaryOp::Xor => {
            let lhs_imm = integer_constant(arena, binary.lhs());
            if binary.op() == BinaryOp::And && (lhs_imm == Some(0) || rhs_imm == Some(0)) {
                ctx.emit(MInst::MovFromZero { size, dst });
            } else if matches!(binary.op(), BinaryOp::Or | BinaryOp::Xor) && lhs_imm == Some(0) {
                let rhs = ctx.put_value_in_reg(binary.rhs());
                ctx.emit(MInst::Mov {
                    size,
                    dst,
                    src: rhs,
                });
            } else if matches!(binary.op(), BinaryOp::Or | BinaryOp::Xor) && rhs_imm == Some(0) {
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
                fold_shifted_rhs(ctx, arena, inst, binary.rhs(), size)
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
                let rhs = ctx.put_value_in_reg(binary.rhs());
                ctx.emit(MInst::AluRRR {
                    op: alu_op(binary.op()),
                    size,
                    dst,
                    lhs: RegOrZr::Reg(lhs),
                    rhs: RegOrZr::Reg(rhs),
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
                let rhs = ctx.put_value_in_reg(binary.rhs());
                ctx.emit(MInst::AluRRR {
                    op: alu_op(binary.op()),
                    size,
                    dst,
                    lhs: RegOrZr::Reg(lhs),
                    rhs: RegOrZr::Reg(rhs),
                });
            }
        }
        BinaryOp::Mul => {
            let rhs = ctx.put_value_in_reg(binary.rhs());
            ctx.emit(MInst::AluRRR {
                op: alu_op(binary.op()),
                size,
                dst,
                lhs: RegOrZr::Reg(lhs),
                rhs: RegOrZr::Reg(rhs),
            });
        }
        BinaryOp::Div => {
            let rhs = ctx.put_value_in_reg(binary.rhs());
            ctx.emit(MInst::SDiv {
                size,
                dst,
                lhs,
                rhs,
            });
        }
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
                let rhs = ctx.put_value_in_reg(binary.rhs());
                ctx.emit(MInst::CmpRR {
                    size,
                    lhs,
                    rhs: RegOrZr::Reg(rhs),
                });
            }
            ctx.emit(MInst::CSet {
                cond: comparison_cond(binary.op()),
                dst,
            });
        }
        BinaryOp::Min | BinaryOp::Max => {
            ctx.lowering_panic(
                "AArch64 instruction selection",
                format!(
                    "scalar {:?} is unsupported; min/max is vector-only",
                    binary.op()
                ),
                Some(arena.inst_data(binary.lhs()).ty()),
                Some(arena.inst_data(inst).ty()),
            );
        }
    }
    LoweredOutput::Value(result)
}

/// Select a vector binary operation onto the NEON instruction set.
///
/// Shape is derived from the vector type (`<4 x i32>`/`<4 x f32>` → `.4s`,
/// `<2 x i64>` → `.2d`). Operations without a NEON form panic with a clear
/// message instead of silently mis-selecting.
fn lower_vector_binary(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    binary: &Binary,
) -> LoweredOutput {
    let result = ctx.result_reg(inst);
    let dst = Writable::from_reg(result);
    let ty = arena.inst_data(binary.lhs()).ty().kind();
    let shape = vector_shape(ty);
    let is_float = matches!(ty, TypeKind::Vector(elem, _) if elem.is_f32());
    let lhs = ctx.put_value_in_reg(binary.lhs());
    let rhs = ctx.put_value_in_reg(binary.rhs());
    match binary.op() {
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul => {
            if binary.op() == BinaryOp::Mul && shape == VecShape::TwoD {
                ctx.lowering_panic(
                    "AArch64 instruction selection",
                    "vector multiply has no .2d form; use a <4 x i32> vector",
                    Some(arena.inst_data(binary.lhs()).ty()),
                    Some(arena.inst_data(inst).ty()),
                );
            }
            ctx.emit(MInst::VecArithRRR {
                op: match binary.op() {
                    BinaryOp::Add => VecArithOp::Add,
                    BinaryOp::Sub => VecArithOp::Sub,
                    _ => VecArithOp::Mul,
                },
                shape,
                dst,
                lhs,
                rhs,
            });
        }
        BinaryOp::And | BinaryOp::Or | BinaryOp::Xor => {
            ctx.emit(MInst::VecBitwise {
                op: match binary.op() {
                    BinaryOp::And => VecBitOp::And,
                    BinaryOp::Or => VecBitOp::Orr,
                    _ => VecBitOp::Eor,
                },
                dst,
                lhs,
                rhs,
            });
        }
        BinaryOp::Eq | BinaryOp::Gt => {
            if is_float {
                ctx.lowering_panic(
                    "AArch64 instruction selection",
                    "floating vector comparisons (fcmgt) are not in the NEON MInst set",
                    Some(arena.inst_data(binary.lhs()).ty()),
                    Some(arena.inst_data(inst).ty()),
                );
            }
            ctx.emit(MInst::VecCmp {
                op: match binary.op() {
                    BinaryOp::Eq => VecCmpOp::Eq,
                    _ => VecCmpOp::Gt,
                },
                shape,
                dst,
                lhs,
                rhs,
            });
        }
        BinaryOp::Min | BinaryOp::Max => {
            if shape == VecShape::TwoD {
                ctx.lowering_panic(
                    "AArch64 instruction selection",
                    "vector min/max has no .2d form",
                    Some(arena.inst_data(binary.lhs()).ty()),
                    Some(arena.inst_data(inst).ty()),
                );
            }
            let op = match (is_float, binary.op()) {
                (false, BinaryOp::Min) => VecMinMaxOp::Smin,
                (false, BinaryOp::Max) => VecMinMaxOp::Smax,
                (true, BinaryOp::Min) => VecMinMaxOp::Fmin,
                (true, BinaryOp::Max) => VecMinMaxOp::Fmax,
                _ => unreachable!("min/max op checked above"),
            };
            ctx.emit(MInst::VecMinMax {
                op,
                shape,
                dst,
                lhs,
                rhs,
            });
        }
        op => {
            ctx.lowering_panic(
                "AArch64 instruction selection",
                format!("vector binary operation {op:?} is unsupported"),
                Some(arena.inst_data(binary.lhs()).ty()),
                Some(arena.inst_data(inst).ty()),
            );
        }
    }
    LoweredOutput::Value(result)
}

/// Map a HIR vector type onto a NEON arrangement. Only the machine-supported
/// 128-bit shapes reach lowering (`<4 x i32>`, `<4 x f32>` → `.4s`;
/// `<2 x i64>` → `.2d`); everything else is a frontend bug.
fn vector_shape(ty: &TypeKind) -> VecShape {
    let (elem, lanes) = match ty {
        TypeKind::Vector(elem, lanes) => (elem, *lanes),
        _ => unreachable!("vector_shape requires a vector type, got {ty:?}"),
    };
    match (lanes, elem.size()) {
        (4, 4) => VecShape::FourS,
        (2, 8) => VecShape::TwoD,
        _ => panic!(
            "unsupported AArch64 vector shape: <{lanes} x {elem}> (only 128-bit .4s/.2d)"
        ),
    }
}

fn lower_cast(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    cast: &Cast,
) -> LoweredOutput {
    let src = cast.src();
    let src_ty = arena.inst_data(src).ty().kind();
    let dst_ty = arena.inst_data(inst).ty().kind();
    let result = ctx.result_reg(inst);
    let dst = Writable::from_reg(result);
    if matches!(src_ty, TypeKind::Vector(..)) || matches!(dst_ty, TypeKind::Vector(..)) {
        let shape = vector_shape(src_ty);
        if vector_shape(dst_ty) != shape {
            ctx.lowering_panic(
                "AArch64 instruction selection",
                format!("vector cast changes the arrangement: {src_ty:?} -> {dst_ty:?}"),
                Some(arena.inst_data(src).ty()),
                Some(arena.inst_data(inst).ty()),
            );
        }
        let src_reg = ctx.put_value_in_reg(src);
        let (from_int, from_float) = match (src_ty, dst_ty) {
            (TypeKind::Vector(from, _), TypeKind::Vector(to, _)) => {
                (from.is_i32() && to.is_f32(), from.is_f32() && to.is_i32())
            }
            _ => (false, false),
        };
        if from_int {
            ctx.emit(MInst::VecCvt {
                op: VecCvtOp::Scvtf,
                shape,
                dst,
                src: src_reg,
            });
        } else if from_float {
            ctx.emit(MInst::VecCvt {
                op: VecCvtOp::Fcvtzs,
                shape,
                dst,
                src: src_reg,
            });
        } else {
            ctx.lowering_panic(
                "AArch64 instruction selection",
                format!("unsupported vector cast: {src_ty:?} -> {dst_ty:?}"),
                Some(arena.inst_data(src).ty()),
                Some(arena.inst_data(inst).ty()),
            );
        }
        return LoweredOutput::Value(result);
    }
    let src_reg = ctx.put_value_in_reg(src);
    match (src_ty, dst_ty) {
        (TypeKind::Int32, TypeKind::Float32) => ctx.emit(MInst::Scvtf { dst, src: src_reg }),
        (TypeKind::Float32, TypeKind::Int32) => ctx.emit(MInst::Fcvtzs { dst, src: src_reg }),
        (src_ty, dst_ty) => {
            ctx.lowering_panic(
                "AArch64 instruction selection",
                format!("cast from {src_ty:?} to {dst_ty:?} is unsupported"),
                Some(arena.inst_data(src).ty()),
                Some(arena.inst_data(inst).ty()),
            );
        }
    }
    LoweredOutput::Value(result)
}

fn lower_alloc(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
) -> LoweredOutput {
    let result = ctx.result_reg(inst);
    let dst = Writable::from_reg(result);
    let pointee = arena.inst_data(inst).ty().derefernce();
    let offset = i64::from(ctx.alloc_stackslot_or_get(inst, pointee));
    ctx.emit(<AArch64Abi as ABIMachineSpec>::gen_get_stack_addr(
        StackAMode::Slot(offset),
        dst,
    ));
    LoweredOutput::Value(result)
}

fn lower_get_elem_ptr(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    gep: &GetElemPtr,
) -> LoweredOutput {
    let analysis = analyze_gep(arena, inst, gep).unwrap_or_else(|error| {
        ctx.lowering_panic(
            "GEP analysis",
            error,
            Some(arena.inst_data(gep.base()).ty()),
            Some(arena.inst_data(inst).ty()),
        )
    });
    let pointer_ty = HirType::get_pointer(HirType::get_i32());
    let mut address = ctx.put_value_in_reg(analysis.base);

    for term in analysis.dynamic_terms {
        let index = ctx.put_value_in_reg(term.index);
        let next = ctx.alloc_tmp(pointer_ty.clone());
        if let Some(shift) = extended_index_shift(term.stride) {
            ctx.emit(MInst::AluRRRExtend {
                op: AluOp::Add,
                size: OperandSize::Size64,
                dst: Writable::from_reg(next),
                lhs: address,
                rhs: index,
                extend: ExtendOp::Sxtw,
                shift,
            });
        } else {
            let zero = ctx.alloc_tmp(pointer_ty.clone());
            ctx.emit(MInst::MovFromZero {
                size: OperandSize::Size64,
                dst: Writable::from_reg(zero),
            });
            let extended = ctx.alloc_tmp(pointer_ty.clone());
            ctx.emit(MInst::AluRRRExtend {
                op: AluOp::Add,
                size: OperandSize::Size64,
                dst: Writable::from_reg(extended),
                lhs: zero,
                rhs: index,
                extend: ExtendOp::Sxtw,
                shift: 0,
            });
            if term.stride.is_power_of_two() {
                let shift = u8::try_from(term.stride.trailing_zeros())
                    .expect("u64 trailing-zero count fits u8");
                ctx.emit(MInst::AluRRRShift {
                    op: AluOp::Add,
                    size: OperandSize::Size64,
                    dst: Writable::from_reg(next),
                    lhs: RegOrZr::Reg(address),
                    rhs: RegOrZr::Reg(extended),
                    shift: ShiftOp::Lsl,
                    amount: ImmShift::new(shift, OperandSize::Size64)
                        .expect("u64 power-of-two shift is encodable"),
                });
            } else {
                let stride = ctx.alloc_tmp(pointer_ty.clone());
                ctx.emit(MInst::LoadImm {
                    size: OperandSize::Size64,
                    dst: Writable::from_reg(stride),
                    value: term.stride,
                });
                ctx.emit(MInst::MAdd {
                    size: OperandSize::Size64,
                    dst: Writable::from_reg(next),
                    lhs: extended,
                    rhs: stride,
                    addend: address,
                });
            }
        }
        address = next;
    }

    if analysis.constant_offset == 0 {
        return LoweredOutput::Value(address);
    }

    let result = ctx.result_reg(inst);
    emit_add_offset(
        ctx,
        Writable::from_reg(result),
        address,
        analysis.constant_offset,
    );
    LoweredOutput::Value(result)
}

fn lower_load(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    load: &Load,
) -> LoweredOutput {
    let result = ctx.result_reg(inst);
    let dst = Writable::from_reg(result);
    let memory_ty = memory_type(arena.inst_data(inst).ty().kind());
    let src = load.src();
    // Fold a single-use constant GEP into the load addressing mode; otherwise
    // materialize the address.
    let addr = if matches!(arena.inst_data(src).kind(), InstKind::GetElemPtr(..)) {
        // v2: single dynamic index folds into extended-register addressing;
        // v1: constant offset folds into immediate addressing; else
        // materialize the address.
        try_fold_dynamic_gep_amode(ctx, arena, src, inst, memory_ty)
            .or_else(|| {
                try_fold_gep_offset(ctx, arena, src, inst, memory_ty.byte_size())
                    .map(|(base, off)| memory_address(ctx, base, off, memory_ty))
            })
            .unwrap_or_else(|| AMode::Reg {
                base: ctx.put_value_in_reg(src),
            })
    } else {
        AMode::Reg {
            base: ctx.put_value_in_reg(src),
        }
    };
    ctx.emit(MInst::Load {
        ty: memory_ty,
        dst,
        addr,
    });
    LoweredOutput::Value(result)
}

fn lower_select(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    select: &Select,
) -> LoweredOutput {
    let result_ty = arena.inst_data(inst).ty().kind();
    if matches!(result_ty, TypeKind::Vector(..)) {
        // `select(mask, if_true, if_false)` over vectors: bit-select. `VecBsl`
        // takes the mask as an explicit SSA read and emits its own leading
        // copy, leaving the mask untouched for any other users.
        let result = ctx.result_reg(inst);
        let dst = Writable::from_reg(result);
        let mask = ctx.put_value_in_reg(select.cond());
        let if_true = ctx.put_value_in_reg(select.if_true());
        let if_false = ctx.put_value_in_reg(select.if_false());
        ctx.emit(MInst::VecBsl {
            dst,
            mask,
            lhs: if_true,
            rhs: if_false,
        });
        return LoweredOutput::Value(result);
    }
    if !matches!(
        result_ty,
        TypeKind::Int32 | TypeKind::Pointer(_) | TypeKind::String | TypeKind::Float32
    ) {
        ctx.lowering_panic(
            "AArch64 instruction selection",
            format!("select result type {result_ty:?} is unsupported"),
            Some(arena.inst_data(select.cond()).ty()),
            Some(arena.inst_data(inst).ty()),
        );
    }

    // Fold `select(band(b1, b2), t, f)` / `select(bor(b1, b2), t, f)` with
    // single-use pure comparisons into `cmp; ccmp; csel/cset`.
    let chain = select_ccmp_chain(ctx, arena, inst, select);
    let (cmp, ccmp, cond) = if let Some((first, second, is_and)) = chain {
        let InstKind::Binary(first_binary) = arena.inst_data(first).kind() else {
            unreachable!("ccmp chain operand is a binary comparison");
        };
        let InstKind::Binary(second_binary) = arena.inst_data(second).kind() else {
            unreachable!("ccmp chain operand is a binary comparison");
        };
        let (cmp, cond1) = comparison_cmp(ctx, arena, &first_binary);
        let (second_size, second_lhs, second_rhs, second_imm) =
            ccmp_operands(ctx, arena, &second_binary);
        let cond2 = comparison_cond(second_binary.op());
        // `ccmp second, #nzcv, cond1` executes when `cond1` holds. For `and`
        // the fallback NZCV must make the final condition false (b1 false =>
        // whole and false); for `or` it must make it true (b1 true => whole
        // or true). clang: and -> `#0, eq`, or -> `#4, ne`.
        let (ccmp_cond, nzcv) = if is_and {
            (cond1, nzcv_making_cond_false(cond2))
        } else {
            (invert_cond(cond1), nzcv_making_cond_true(cond2))
        };
        let ccmp = CCmpStep {
            size: second_size,
            lhs: second_lhs,
            rhs: second_rhs,
            imm: second_imm,
            nzcv,
            cond: ccmp_cond,
        };
        (cmp, Some(Box::new(ccmp)), cond2)
    } else {
        // Fall back to a single comparison or a compare-against-zero.
        let condition = select_comparison(ctx, arena, inst, select);
        let (cmp, cond) = if let Some((binary, invert)) = condition {
            let lhs_ty = arena.inst_data(binary.lhs()).ty().kind();
            if matches!(lhs_ty, TypeKind::Float32) {
                let cond = if invert {
                    invert_float_comparison_cond(binary.op())
                } else {
                    float_comparison_cond(binary.op())
                };
                (
                    SelectCmp::Float {
                        lhs: ctx.put_value_in_reg(binary.lhs()),
                        rhs: ctx.put_value_in_reg(binary.rhs()),
                    },
                    cond,
                )
            } else {
                let (size, lhs, rhs, imm) = comparison_operands(ctx, arena, &binary);
                let cmp = if let Some(imm) = imm {
                    SelectCmp::IntImm { size, lhs, imm }
                } else {
                    SelectCmp::IntRR { size, lhs, rhs }
                };
                let cond = comparison_cond(binary.op());
                (cmp, if invert { invert_cond(cond) } else { cond })
            }
        } else {
            (
                SelectCmp::IntImm {
                    size: OperandSize::Size32,
                    lhs: ctx.put_value_in_reg(select.cond()),
                    imm: Imm12::new(0, false).unwrap(),
                },
                Cond::Ne,
            )
        };
        (cmp, None, cond)
    };

    let result = ctx.result_reg(inst);
    let dst = Writable::from_reg(result);
    let true_constant = integer_constant(arena, select.if_true());
    let false_constant = integer_constant(arena, select.if_false());
    let (cond, value) = if matches!(result_ty, TypeKind::Int32)
        && true_constant == Some(1)
        && false_constant == Some(0)
    {
        (cond, SelectValue::Bool { dst })
    } else if matches!(result_ty, TypeKind::Int32)
        && true_constant == Some(0)
        && false_constant == Some(1)
    {
        (invert_cond(cond), SelectValue::Bool { dst })
    } else {
        let if_true = ctx.put_value_in_reg(select.if_true());
        let if_false = ctx.put_value_in_reg(select.if_false());
        let value = match result_ty {
            TypeKind::Int32 | TypeKind::Pointer(_) | TypeKind::String => SelectValue::Int {
                size: operand_size(result_ty),
                dst,
                if_true,
                if_false,
            },
            TypeKind::Float32 => SelectValue::Float {
                dst,
                if_true,
                if_false,
            },
            _ => unreachable!("select result type checked above"),
        };
        (cond, value)
    };

    ctx.emit(MInst::CmpSelect {
        cmp,
        ccmp,
        cond,
        value,
    });
    LoweredOutput::Value(result)
}

/// Match `cond = band(b1, b2)` / `cond = bor(b1, b2)` where both `b1` and
/// `b2` are single-use, pure, integer comparisons. Returns `(b1, b2, is_and)`
/// as HIR instructions.
fn select_ccmp_chain(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    select: &Select,
) -> Option<(HirInst, HirInst, bool)> {
    let cond = select.cond();
    let chain = ccmp_chain_operands(ctx, arena, cond, inst)?;
    if !ctx.sink_pure_single_use_pair(chain.0, chain.1, cond, inst) {
        return None;
    }
    Some(chain)
}

/// Same detection for a branch condition.
fn branch_ccmp_chain(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    branch: HirInst,
    cond: HirInst,
) -> Option<(HirInst, HirInst, bool)> {
    let chain = ccmp_chain_operands(ctx, arena, cond, branch)?;
    if !ctx.sink_pure_single_use_pair(chain.0, chain.1, cond, branch) {
        return None;
    }
    Some(chain)
}

/// Recognize `band(b1, b2)` / `bor(b1, b2)` of two single-use, pure, integer
/// comparisons without claiming anything yet. Returns the two operand HIR
/// instructions and whether the combine is `and`.
fn ccmp_chain_operands(
    ctx: &LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    cond: HirInst,
    root: HirInst,
) -> Option<(HirInst, HirInst, bool)> {
    let InstKind::Binary(outer) = arena.inst_data(cond).kind() else {
        return None;
    };
    let (is_and, first, second) = match outer.op() {
        BinaryOp::And => (true, outer.lhs(), outer.rhs()),
        BinaryOp::Or => (false, outer.lhs(), outer.rhs()),
        _ => return None,
    };
    if !has_only_user(ctx, cond, root) {
        return None;
    }
    let InstKind::Binary(first_binary) = arena.inst_data(first).kind() else {
        return None;
    };
    let InstKind::Binary(second_binary) = arena.inst_data(second).kind() else {
        return None;
    };
    if !is_comparison(first_binary.op()) || !is_comparison(second_binary.op()) {
        return None;
    }
    if !has_only_user(ctx, first, cond) || !has_only_user(ctx, second, cond) {
        return None;
    }
    let first_ty = arena.inst_data(first_binary.lhs()).ty().kind();
    let second_ty = arena.inst_data(second_binary.lhs()).ty().kind();
    if matches!(first_ty, TypeKind::Float32) || matches!(second_ty, TypeKind::Float32) {
        return None;
    }
    Some((first, second, is_and))
}

/// Return the comparison operands as a `(size, lhs, rhs, imm)` tuple, with
/// `imm = Some` when the RHS is a legal positive 12-bit immediate.
#[allow(clippy::type_complexity)]
fn comparison_operands(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    binary: &Binary,
) -> (OperandSize, Reg, RegOrZr, Option<Imm12>) {
    let size = operand_size(arena.inst_data(binary.lhs()).ty().kind());
    let lhs = ctx.put_value_in_reg(binary.lhs());
    let imm = integer_constant(arena, binary.rhs()).and_then(positive_imm12);
    let rhs = if imm.is_some() {
        RegOrZr::Zr
    } else {
        RegOrZr::Reg(ctx.put_value_in_reg(binary.rhs()))
    };
    (size, lhs, rhs, imm)
}

/// Comparison operands for a `ccmp`: the immediate operand is 5-bit
/// (0..=31), so constants outside that range fall back to a register.
#[allow(clippy::type_complexity)]
fn ccmp_operands(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    binary: &Binary,
) -> (OperandSize, Reg, RegOrZr, Option<Imm12>) {
    let size = operand_size(arena.inst_data(binary.lhs()).ty().kind());
    let lhs = ctx.put_value_in_reg(binary.lhs());
    let imm = integer_constant(arena, binary.rhs())
        .filter(|value| (0..=31).contains(value))
        .and_then(|value| Imm12::new(value as u16, false));
    let rhs = if imm.is_some() {
        RegOrZr::Zr
    } else {
        RegOrZr::Reg(ctx.put_value_in_reg(binary.rhs()))
    };
    (size, lhs, rhs, imm)
}

/// Build the flag-producing comparison for `binary` alone.
fn comparison_cmp(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    binary: &Binary,
) -> (SelectCmp, Cond) {
    let (size, lhs, rhs, imm) = comparison_operands(ctx, arena, binary);
    let cmp = if let Some(imm) = imm {
        SelectCmp::IntImm { size, lhs, imm }
    } else {
        SelectCmp::IntRR { size, lhs, rhs }
    };
    (cmp, comparison_cond(binary.op()))
}

/// A 4-bit NZCV value that makes `cond` evaluate to false (the fallback
/// written by `ccmp` when its condition does not hold, for `band`).
fn nzcv_making_cond_false(cond: Cond) -> u8 {
    match cond {
        Cond::Eq => 0, // Z=0
        Cond::Ne => 4, // Z=1
        Cond::Hs => 0, // C=0
        Cond::Lo => 2, // C=1
        Cond::Mi => 0, // N=0
        Cond::Pl => 8, // N=1
        Cond::Vs => 0, // V=0
        Cond::Vc => 1, // V=1
        Cond::Hi => 4, // Z=1
        Cond::Ls => 2, // C=1,Z=0
        Cond::Ge => 8, // N=1,V=0
        Cond::Lt => 0, // N=0,V=0
        Cond::Gt => 4, // Z=1
        Cond::Le => 0, // Z=0,N=0,V=0
    }
}

/// A 4-bit NZCV value that makes `cond` evaluate to true (the fallback
/// written by `ccmp` when its condition does not hold, for `bor`).
fn nzcv_making_cond_true(cond: Cond) -> u8 {
    match cond {
        Cond::Eq => 4, // Z=1
        Cond::Ne => 0, // Z=0
        Cond::Hs => 2, // C=1
        Cond::Lo => 0, // C=0
        Cond::Mi => 8, // N=1
        Cond::Pl => 0, // N=0
        Cond::Vs => 1, // V=1
        Cond::Vc => 0, // V=0
        Cond::Hi => 2, // C=1,Z=0
        Cond::Ls => 4, // Z=1
        Cond::Ge => 0, // N=0,V=0
        Cond::Lt => 8, // N=1,V=0
        Cond::Gt => 0, // Z=0,N=0,V=0
        Cond::Le => 4, // Z=1
    }
}

fn select_comparison(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    select: &Select,
) -> Option<(Binary, bool)> {
    let cond = select.cond();
    if select.if_true() == cond || select.if_false() == cond || !has_only_user(ctx, cond, inst) {
        return None;
    }
    let InstKind::Binary(outer) = arena.inst_data(cond).kind() else {
        return None;
    };

    if is_comparison(outer.op()) {
        if let Some((inner_inst, is_eq)) = zero_comparison(arena, outer) {
            if let InstKind::Binary(inner) = arena.inst_data(inner_inst).kind() {
                if is_comparison(inner.op())
                    && has_only_user(ctx, inner_inst, cond)
                    && ctx.sink_pure_single_use_chain(inner_inst, cond, inst)
                {
                    return Some((inner.clone(), is_eq));
                }
            }
        }
        if ctx.sink_pure_single_use_producer(cond, inst) {
            return Some((outer.clone(), false));
        }
    }
    None
}

fn lower_mem_zero(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    mem_zero: &MemZero,
) -> LoweredOutput {
    let inline_store_count = mem_zero.byte_len() / 4;
    if runtime::mem_zero_is_inline(mem_zero.byte_len()) {
        let alloc = matches!(arena.inst_data(mem_zero.dest()).kind(), InstKind::Alloc)
            .then_some(mem_zero.dest());
        let (dest, stack_offset) = if let Some(alloc) = alloc {
            let pointee = arena.inst_data(alloc).ty().derefernce();
            let offset = i64::from(ctx.alloc_stackslot_or_get(alloc, pointee));
            (
                ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32())),
                Some(offset),
            )
        } else {
            (ctx.put_value_in_reg(mem_zero.dest()), None)
        };
        let zero = ctx.alloc_tmp(HirType::get_i32());
        if let Some(offset) = stack_offset {
            ctx.emit(<AArch64Abi as ABIMachineSpec>::gen_get_stack_addr(
                StackAMode::Slot(offset),
                Writable::from_reg(dest),
            ));
        }
        ctx.emit(MInst::MovFromZero {
            size: OperandSize::Size32,
            dst: Writable::from_reg(zero),
        });
        for index in 0..inline_store_count {
            emit_store_at(ctx, zero, &HirType::get_i32(), dest, (index * 4) as i64);
        }
        return LoweredOutput::None;
    }

    let alloc = matches!(arena.inst_data(mem_zero.dest()).kind(), InstKind::Alloc)
        .then_some(mem_zero.dest());
    let (dest, stack_offset) = if let Some(alloc) = alloc {
        let pointee = arena.inst_data(alloc).ty().derefernce();
        let offset = i64::from(ctx.alloc_stackslot_or_get(alloc, pointee));
        (
            ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32())),
            Some(offset),
        )
    } else {
        (ctx.put_value_in_reg(mem_zero.dest()), None)
    };
    let byte_len = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
    if let Some(offset) = stack_offset {
        ctx.emit(<AArch64Abi as ABIMachineSpec>::gen_get_stack_addr(
            StackAMode::Slot(offset),
            Writable::from_reg(dest),
        ));
    }
    ctx.emit(MInst::LoadImm {
        size: OperandSize::Size64,
        dst: Writable::from_reg(byte_len),
        value: mem_zero.byte_len() as u64,
    });
    ctx.emit(MInst::Call {
        args: vec![
            CallArgPair {
                vreg: dest,
                preg: regs::INT_ARG_REGS[0],
            },
            CallArgPair {
                vreg: byte_len,
                preg: regs::INT_ARG_REGS[1],
            },
        ],
        ret: None,
        clobbers: regs::DEFAULT_CLOBBERS,
        label: Label::Embedded(EmbeddedSymbol::Memset),
    });
    ctx.set_has_calls();
    ctx.set_outgoing_arg_size(0);
    LoweredOutput::None
}

fn lower_call(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    call: &Call,
) -> LoweredOutput {
    let mut args = Vec::new();
    let types: Vec<_> = call
        .args()
        .iter()
        .map(|&arg| arena.inst_data(arg).ty().clone())
        .collect();
    let (locations, outgoing_size) = AArch64Abi::compute_call_arg_loc(&types);
    for (&arg, location) in call.args().iter().zip(locations) {
        let src = ctx.put_value_in_reg(arg);
        match location {
            ArgSlot::Reg { reg, .. } => args.push(CallArgPair {
                vreg: src,
                preg: reg.into(),
            }),
            ArgSlot::Stack { offset, ty } => ctx.emit(MInst::Store {
                ty: memory_type(ty.kind()),
                src,
                addr: AMode::OutgoingArg(offset),
            }),
        }
    }
    let result = (!arena.inst_data(inst).ty().is_unit()).then(|| ctx.result_reg(inst));
    let ret = match arena.inst_data(inst).ty().kind() {
        TypeKind::Unit => None,
        TypeKind::Int32 | TypeKind::Pointer(_) | TypeKind::String => Some(CallRetPair {
            vreg: Writable::from_reg(result.unwrap()),
            preg: regs::INT_RETURN_REG,
        }),
        TypeKind::Float32 => Some(CallRetPair {
            vreg: Writable::from_reg(result.unwrap()),
            preg: regs::FLOAT_RETURN_REG,
        }),
        TypeKind::Vector(..) => Some(CallRetPair {
            vreg: Writable::from_reg(result.unwrap()),
            preg: regs::VECTOR_RETURN_REG,
        }),
        ty => {
            ctx.lowering_panic(
                "AArch64 instruction selection",
                format!("call return type {ty:?} is unsupported"),
                None,
                Some(arena.inst_data(inst).ty()),
            );
        }
    };
    ctx.emit(MInst::Call {
        args,
        ret,
        clobbers: regs::DEFAULT_CLOBBERS,
        label: Label::from_function(call.callee()),
    });
    ctx.set_has_calls();
    ctx.set_outgoing_arg_size(outgoing_size as usize);
    result.map_or(LoweredOutput::None, LoweredOutput::Value)
}

/// Lower a tail call. Each argument is placed where the callee will read it:
/// register arguments are forced into the ABI argument registers via the
/// `TailCall` instruction's fixed-register operands, and stack arguments are
/// stored to the incoming-argument slots ahead of it. The emitter prepends the
/// epilogue before cross-function transfers. Self-recursive calls instead jump
/// to the local entry block and keep the current frame.
///
/// Tail-call elimination guarantees that caller and callee signatures match,
/// so `abi.arg_slot(idx)` gives the correct destination for every argument.
fn lower_tail_call(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    tail_call: &TailCall,
) -> LoweredOutput {
    let mut args = Vec::new();
    for (idx, &arg) in tail_call.args().iter().enumerate() {
        let src = ctx.put_value_in_reg(arg);
        let ty = memory_type(arena.inst_data(arg).ty().kind());
        match ctx.arg_slot(idx) {
            ArgSlot::Reg { reg, .. } => args.push(CallArgPair {
                vreg: src,
                preg: reg.into(),
            }),
            ArgSlot::Stack { offset, .. } => ctx.emit(MInst::Store {
                ty,
                src,
                addr: AMode::IncomingArg(offset),
            }),
        }
    }
    let callee = tail_call.callee();
    let self_recursive = ctx.arena.curr_func == Some(callee);
    let label = if self_recursive {
        // Stay in the current invocation: argument moves target the same ABI
        // locations and control resumes after the one-time prologue.
        Label::from_block(ctx.entry_block())
    } else {
        ctx.set_has_calls();
        Label::from_function(callee)
    };
    ctx.emit(MInst::TailCall {
        args,
        clobbers: regs::DEFAULT_CLOBBERS,
        label,
    });
    LoweredOutput::None
}

fn lower_return(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    ret: &Return,
) -> LoweredOutput {
    if let Some(value) = ret.value() {
        let src = ctx.put_value_in_reg(value);
        let preg = match arena.inst_data(value).ty().kind() {
            TypeKind::Int32 | TypeKind::Pointer(_) | TypeKind::String => regs::INT_RETURN_REG,
            TypeKind::Float32 => regs::FLOAT_RETURN_REG,
            TypeKind::Vector(..) => regs::VECTOR_RETURN_REG,
            ty => {
                ctx.lowering_panic(
                    "AArch64 instruction selection",
                    format!("return type {ty:?} is unsupported"),
                    Some(arena.inst_data(value).ty()),
                    None,
                );
            }
        };
        ctx.emit(MInst::RetVal {
            pair: RetPair { vreg: src, preg },
        });
    }
    ctx.emit(MInst::Ret);
    LoweredOutput::None
}

/// `fma(acc, lhs, rhs)`: `acc` is a read-write accumulator, so copy it into
/// the result register before the fused multiply-add writes it.
/// `fma(acc, lhs, rhs)`: `fmla` is read-modify-write on the accumulator; the
/// `VecFmla` MInst takes the accumulator as an explicit SSA read and emits the
/// leading copy itself.
fn lower_fma(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    fma: &Fma,
) -> LoweredOutput {
    let result = ctx.result_reg(inst);
    let dst = Writable::from_reg(result);
    let shape = vector_shape(arena.inst_data(inst).ty().kind());
    let acc = ctx.put_value_in_reg(fma.acc());
    let lhs = ctx.put_value_in_reg(fma.lhs());
    let rhs = ctx.put_value_in_reg(fma.rhs());
    ctx.emit(MInst::VecFmla {
        shape,
        dst,
        acc,
        lhs,
        rhs,
    });
    LoweredOutput::Value(result)
}

fn lower_vector_splat(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    splat: &VectorSplat,
) -> LoweredOutput {
    let result = ctx.result_reg(inst);
    let dst = Writable::from_reg(result);
    let shape = vector_shape(arena.inst_data(inst).ty().kind());
    let src = ctx.put_value_in_reg(splat.src());
    ctx.emit(MInst::VecDup { shape, dst, src });
    LoweredOutput::Value(result)
}

fn lower_vector_extract_element(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    extract: &VectorExtractElement,
) -> LoweredOutput {
    let result = ctx.result_reg(inst);
    let dst = Writable::from_reg(result);
    let size = operand_size(arena.inst_data(inst).ty().kind());
    let src = ctx.put_value_in_reg(extract.src());
    let lane = lane_constant(arena, extract.index());
    ctx.emit(MInst::VecExtractLane { size, dst, src, lane });
    LoweredOutput::Value(result)
}

fn lower_vector_insert_element(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    insert: &VectorInsertElement,
) -> LoweredOutput {
    let result = ctx.result_reg(inst);
    let dst = Writable::from_reg(result);
    let size = operand_size(arena.inst_data(insert.element()).ty().kind());
    // `VecInsertLane` is read-modify-write on the destination vector; it takes
    // the vector as an explicit SSA read and emits its own leading copy.
    let vector = ctx.put_value_in_reg(insert.vector());
    let element = ctx.put_value_in_reg(insert.element());
    let lane = lane_constant(arena, insert.index());
    ctx.emit(MInst::VecInsertLane {
        size,
        dst,
        vector,
        src: element,
        lane,
    });
    LoweredOutput::Value(result)
}

fn lower_vector_reduce(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    reduce: &VectorReduce,
) -> LoweredOutput {
    if !matches!(reduce.op(), VectorReduceOp::Add) {
        ctx.lowering_panic(
            "AArch64 instruction selection",
            "only add reduction is supported",
            Some(arena.inst_data(reduce.src()).ty()),
            Some(arena.inst_data(inst).ty()),
        );
    }
    let src_ty = arena.inst_data(reduce.src()).ty().kind();
    if matches!(src_ty, TypeKind::Vector(elem, _) if elem.is_f32()) {
        ctx.lowering_panic(
            "AArch64 instruction selection",
            "float horizontal sums (faddp) are not in the NEON MInst set",
            Some(arena.inst_data(reduce.src()).ty()),
            Some(arena.inst_data(inst).ty()),
        );
    }
    if vector_shape(src_ty) != VecShape::FourS {
        ctx.lowering_panic(
            "AArch64 instruction selection",
            "vector add reduction (addv) only supports .4s",
            Some(arena.inst_data(reduce.src()).ty()),
            Some(arena.inst_data(inst).ty()),
        );
    }
    let result = ctx.result_reg(inst);
    let src = ctx.put_value_in_reg(reduce.src());
    if arena.inst_data(inst).ty().is_i32() {
        // `addv s0, v1.4s` writes a SIMD register; move the i32 result into
        // an integer register for the scalar consumer.
        let acc = ctx.alloc_tmp(HirType::get_f32());
        ctx.emit(MInst::VecAddv {
            dst: Writable::from_reg(acc),
            src,
        });
        ctx.emit(MInst::FMov {
            dst: Writable::from_reg(result),
            src: acc,
        });
    } else {
        ctx.emit(MInst::VecAddv {
            dst: Writable::from_reg(result),
            src,
        });
    }
    LoweredOutput::Value(result)
}

fn lane_constant(arena: ArenaContext<'_>, index: HirInst) -> u8 {
    match arena.inst_data(index).kind() {
        InstKind::Integer(value) => value.value() as u8,
        _ => unreachable!("vector lane index must be a constant integer"),
    }
}

impl LowerBackend for AArch64Backend {
    type MInst = MInst;
    type CodegenConfig = crate::config::AArch64CodegenConfig;

    fn lower(ctx: &mut LowerContext<Self::MInst>, inst: HirInst) -> LoweredOutput {
        let arena = ctx.arena;
        match arena.inst_data(inst).kind() {
            InstKind::BlockArgRef(..)
            | InstKind::Aggregate(..)
            | InstKind::GlobalAlloc(..)
            | InstKind::Undef
            | InstKind::Integer(..)
            | InstKind::Float(..) => {
                unreachable!("constants and argument references are rematerialized by LowerContext")
            }
            InstKind::Binary(binary) => lower_binary(ctx, arena, inst, binary),
            InstKind::Select(select) => lower_select(ctx, arena, inst, select),
            InstKind::Cast(cast) => lower_cast(ctx, arena, inst, cast),
            InstKind::Alloc => lower_alloc(ctx, arena, inst),
            InstKind::GetElemPtr(gep) => lower_get_elem_ptr(ctx, arena, inst, gep),
            InstKind::Load(load) => lower_load(ctx, arena, inst, load),
            InstKind::Store(store) => lower_store(ctx, arena, inst, store),
            InstKind::MemZero(mem_zero) => lower_mem_zero(ctx, arena, mem_zero),
            InstKind::ZeroInit => unreachable!("zero initialization is lowered by its store"),
            InstKind::Call(call) => lower_call(ctx, arena, inst, call),
            InstKind::TailCall(tail_call) => lower_tail_call(ctx, arena, tail_call),
            InstKind::Return(ret) => lower_return(ctx, arena, ret),
            InstKind::Fma(fma) => lower_fma(ctx, arena, inst, fma),
            InstKind::VectorSplat(splat) => lower_vector_splat(ctx, arena, inst, splat),
            InstKind::VectorExtractElement(extract) => {
                lower_vector_extract_element(ctx, arena, inst, extract)
            }
            InstKind::VectorInsertElement(insert) => {
                lower_vector_insert_element(ctx, arena, inst, insert)
            }
            InstKind::VectorReduce(reduce) => lower_vector_reduce(ctx, arena, inst, reduce),
            InstKind::Jump(..) | InstKind::Branch(..) => {
                unreachable!("terminators are lowered by LowerBackend::lower_branch")
            }
        }
    }

    fn lower_branch(
        ctx: &mut LowerContext<Self::MInst>,
        inst: raana_ir::opt::prelude::Inst,
        target: &[MirBlockIndex],
    ) {
        let arena = ctx.arena;
        match arena.inst_data(inst).kind() {
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
                for &arg in branch.t_args().iter().chain(branch.f_args()) {
                    ctx.put_value_in_reg(arg);
                }
                let &[true_target, false_target] = target else {
                    unreachable!("branch must have two lowered successors");
                };
                if select_branch_condition(ctx, inst, branch.cond(), true_target, false_target) {
                    return;
                }

                let cond = ctx.put_value_in_reg(branch.cond());
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
            kind => {
                ctx.lowering_panic(
                    "AArch64 branch selection",
                    format!("non-terminator HIR instruction {kind:?} cannot select a branch"),
                    None,
                    Some(arena.inst_data(inst).ty()),
                );
            }
        }
    }

    fn data_section_directive() -> &'static str {
        ".section .data"
    }

    fn bss_section_directive() -> &'static str {
        ".section .bss"
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
        let function = func_data.name().replace('%', "_");
        match lb {
            LoweredBlock::Orig { block } => {
                format!(
                    ".L_{function}_{}",
                    func_data.bb_data(*block).name().replace('%', "_")
                )
            }
            LoweredBlock::Edge {
                pred,
                succ,
                succ_idx,
            } => format!(
                ".L_{function}_{}_to_{}_edge_{}",
                func_data.bb_data(*pred).name().replace('%', "_"),
                func_data.bb_data(*succ).name().replace('%', "_"),
                succ_idx
            ),
        }
    }

    fn emit_long_jump(ctx: &mut LowerContext<Self::MInst>, target: MirBlockIndex) {
        ctx.emit(MInst::gen_jump(target));
    }

    fn runtime_assembly(program: &taki_mir::prelude::HirProgram) -> Option<String> {
        runtime::assembly(program)
    }

    fn mir_pipeline(
        config: &Self::CodegenConfig,
    ) -> taki_mir::passes::MIRPassPipeline<Self::MInst> {
        crate::passes::build_pipeline(config)
    }

    fn branch_opt_enabled(config: &Self::CodegenConfig) -> bool {
        config.branch_opt
    }

    fn veneer_lines(kind: taki_mir::emit_buffer::LabelKind, target: &str) -> Vec<String> {
        use taki_mir::emit_buffer::LabelKind;
        match kind {
            // A conditional branch fell out of ±1MB: the (inverted) branch
            // reaches the adjacent veneer; the veneer's `b` covers ±128MB.
            LabelKind::BRANCH14 | LabelKind::BRANCH19 => vec![format!("b {target}")],
            // `b` itself fell out of ±128MB: materialize the address through
            // the linker scratch register x16 (practically unreachable).
            LabelKind::BRANCH26 => vec![
                format!("adrp x16, {target}"),
                format!("add x16, x16, :lo12:{target}"),
                "br x16".to_owned(),
            ],
            other => unreachable!("AArch64 does not emit {other:?} branches"),
        }
    }
}

/// Select a branch-local, pure condition tree.  Claims are made through the
/// generic lowering context so reverse traversal never independently lowers a
/// producer whose result is consumed here.
fn select_branch_condition(
    ctx: &mut LowerContext<'_, MInst>,
    branch: raana_ir::opt::prelude::Inst,
    cond: raana_ir::opt::prelude::Inst,
    true_target: MirBlockIndex,
    false_target: MirBlockIndex,
) -> bool {
    let arena = ctx.arena;
    // Fold `br band(b1, b2)` / `br bor(b1, b2)` into `cmp; ccmp; b.cc`.
    if let Some((first, second, is_and)) = branch_ccmp_chain(ctx, arena, branch, cond) {
        let InstKind::Binary(first_binary) = arena.inst_data(first).kind() else {
            unreachable!("ccmp chain operand is a binary comparison");
        };
        let InstKind::Binary(second_binary) = arena.inst_data(second).kind() else {
            unreachable!("ccmp chain operand is a binary comparison");
        };
        let (size, lhs, rhs, imm) = comparison_operands(ctx, arena, &first_binary);
        if let Some(imm) = imm {
            ctx.emit(MInst::CmpImm { size, lhs, imm });
        } else {
            ctx.emit(MInst::CmpRR { size, lhs, rhs });
        }
        let cond1 = comparison_cond(first_binary.op());
        let (second_size, second_lhs, second_rhs, second_imm) =
            ccmp_operands(ctx, arena, &second_binary);
        let cond2 = comparison_cond(second_binary.op());
        let (ccmp_cond, nzcv) = if is_and {
            (cond1, nzcv_making_cond_false(cond2))
        } else {
            (invert_cond(cond1), nzcv_making_cond_true(cond2))
        };
        ctx.emit(MInst::CCmp {
            size: second_size,
            lhs: second_lhs,
            rhs: second_rhs,
            imm: second_imm,
            nzcv,
            cond: ccmp_cond,
        });
        let (true_label, false_label) = (
            Label::from_block(true_target),
            Label::from_block(false_target),
        );
        ctx.emit(MInst::CondBr {
            cond: cond2,
            true_label,
            false_label,
        });
        return true;
    }

    let InstKind::Binary(outer) = arena.inst_data(cond).kind() else {
        return false;
    };
    if !is_comparison(outer.op()) || !has_only_user(ctx, cond, branch) {
        return false;
    }

    let labels = (
        Label::from_block(true_target),
        Label::from_block(false_target),
    );
    let zero_outer = zero_comparison(arena, outer);
    if let Some((value, is_eq)) = zero_outer {
        if let InstKind::Binary(inner) = arena.inst_data(value).kind() {
            if is_comparison(inner.op()) && has_only_user(ctx, value, cond) {
                // Claim the leaf first: a rejection must leave the outer
                // condition available for the conservative fallback.
                if !ctx.sink_pure_single_use_producer(value, cond)
                    || !ctx.sink_pure_single_use_producer(cond, branch)
                {
                    return false;
                }
                emit_comparison_branch(ctx, arena, inner, !is_eq, labels);
                return true;
            }
            if let Some((tested, bit)) = single_bit_mask(ctx, arena, inner, value, cond) {
                if !ctx.sink_pure_single_use_producer(value, cond)
                    || !ctx.sink_pure_single_use_producer(cond, branch)
                {
                    return false;
                }
                let tested = ctx.put_value_in_reg(tested);
                let (true_label, false_label) = labels;
                ctx.emit(if is_eq {
                    MInst::Tbz {
                        size: OperandSize::Size32,
                        reg: tested,
                        bit,
                        true_label,
                        false_label,
                    }
                } else {
                    MInst::Tbnz {
                        size: OperandSize::Size32,
                        reg: tested,
                        bit,
                        true_label,
                        false_label,
                    }
                });
                return true;
            }
        }

        let ty = arena.inst_data(value).ty().kind();
        if matches!(
            ty,
            TypeKind::Int32 | TypeKind::Pointer(_) | TypeKind::String
        ) {
            if !ctx.sink_pure_single_use_producer(cond, branch) {
                return false;
            }
            let value = ctx.put_value_in_reg(value);
            let size = operand_size(ty);
            let (true_label, false_label) = labels;
            ctx.emit(if is_eq {
                MInst::Cbz {
                    size,
                    reg: value,
                    true_label,
                    false_label,
                }
            } else {
                MInst::Cbnz {
                    size,
                    reg: value,
                    true_label,
                    false_label,
                }
            });
            return true;
        }
        if matches!(ty, TypeKind::Float32) {
            if !ctx.sink_pure_single_use_producer(cond, branch) {
                return false;
            }
            let value = ctx.put_value_in_reg(value);
            emit_float_zero_branch(ctx, value, if is_eq { Cond::Eq } else { Cond::Ne }, labels);
            return true;
        }
    }

    if !ctx.sink_pure_single_use_producer(cond, branch) {
        return false;
    }
    emit_comparison_branch(ctx, arena, outer, false, labels);
    true
}

fn has_only_user(
    ctx: &LowerContext<'_, MInst>,
    producer: raana_ir::opt::prelude::Inst,
    user: raana_ir::opt::prelude::Inst,
) -> bool {
    let users = ctx.arena.inst_data(producer).used_by();
    users.len() == 1 && users.contains(&user)
}

fn zero_comparison(arena: ArenaContext<'_>, binary: &Binary) -> Option<(HirInst, bool)> {
    let is_eq = match binary.op() {
        BinaryOp::Eq => true,
        BinaryOp::NotEq => false,
        _ => return None,
    };
    if integer_constant(arena, binary.lhs()) == Some(0) {
        Some((binary.rhs(), is_eq))
    } else if integer_constant(arena, binary.rhs()) == Some(0) {
        Some((binary.lhs(), is_eq))
    } else {
        None
    }
}

fn single_bit_mask(
    ctx: &LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    and: &Binary,
    and_inst: HirInst,
    outer: HirInst,
) -> Option<(HirInst, u8)> {
    if and.op() != BinaryOp::And
        || !matches!(arena.inst_data(and_inst).ty().kind(), TypeKind::Int32)
        || !has_only_user(ctx, and_inst, outer)
    {
        return None;
    }
    let (value, mask) = if let Some(mask) = integer_constant(arena, and.lhs()) {
        (and.rhs(), mask)
    } else {
        (and.lhs(), integer_constant(arena, and.rhs())?)
    };
    let mask = u32::try_from(mask).ok()?;
    if mask.count_ones() != 1 || !matches!(arena.inst_data(value).ty().kind(), TypeKind::Int32) {
        return None;
    }
    Some((value, mask.trailing_zeros() as u8))
}

fn emit_comparison_branch(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    binary: &Binary,
    invert: bool,
    (true_label, false_label): (Label, Label),
) {
    let lhs_ty = arena.inst_data(binary.lhs()).ty().kind();
    let (cond, float_comparison) = if matches!(lhs_ty, TypeKind::Float32) {
        let lhs = ctx.put_value_in_reg(binary.lhs());
        let rhs = ctx.put_value_in_reg(binary.rhs());
        ctx.emit(MInst::FCmp { lhs, rhs });
        (float_comparison_cond(binary.op()), true)
    } else {
        let size = operand_size(lhs_ty);
        let lhs = ctx.put_value_in_reg(binary.lhs());
        if let Some(imm) = integer_constant(arena, binary.rhs()).and_then(positive_imm12) {
            ctx.emit(MInst::CmpImm { size, lhs, imm });
        } else {
            let rhs = ctx.put_value_in_reg(binary.rhs());
            ctx.emit(MInst::CmpRR {
                size,
                lhs,
                rhs: RegOrZr::Reg(rhs),
            });
        }
        (comparison_cond(binary.op()), false)
    };
    ctx.emit(MInst::CondBr {
        // Floating ordered `<`/`<=` need Mi/Ls for direct selection, but
        // their boolean negations include unordered values.  Select the
        // corresponding flag condition rather than mechanically inverting
        // Mi/Ls, while integer conditions use the complete inverse table.
        cond: if invert {
            if float_comparison {
                invert_float_comparison_cond(binary.op())
            } else {
                invert_cond(cond)
            }
        } else {
            cond
        },
        true_label,
        false_label,
    });
}

fn emit_float_zero_branch(
    ctx: &mut LowerContext<'_, MInst>,
    value: taki_mir::register::Reg,
    cond: Cond,
    (true_label, false_label): (Label, Label),
) {
    let zero = ctx.alloc_tmp(HirType::get_f32());
    ctx.emit(MInst::FMovFromZero {
        dst: Writable::from_reg(zero),
    });
    ctx.emit(MInst::FCmp {
        lhs: value,
        rhs: zero,
    });
    ctx.emit(MInst::CondBr {
        cond,
        true_label,
        false_label,
    });
}

fn is_comparison(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Eq | BinaryOp::NotEq | BinaryOp::Gt | BinaryOp::Lt | BinaryOp::Ge | BinaryOp::Le
    )
}

fn invert_cond(cond: Cond) -> Cond {
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

fn invert_float_comparison_cond(op: BinaryOp) -> Cond {
    match op {
        BinaryOp::Eq => Cond::Ne,
        BinaryOp::NotEq => Cond::Eq,
        BinaryOp::Gt => Cond::Le,
        // `hs` includes ordered >= and unordered, exactly `!(lhs < rhs)`.
        BinaryOp::Lt => Cond::Hs,
        // Generic `lt` includes unordered (N != V), exactly `!(lhs >= rhs)`.
        BinaryOp::Ge => Cond::Lt,
        // `hi` includes ordered > and unordered, exactly `!(lhs <= rhs)`.
        BinaryOp::Le => Cond::Hi,
        _ => unreachable!("binary operation is not a floating comparison"),
    }
}

fn integer_constant(arena: ArenaContext<'_>, inst: HirInst) -> Option<i32> {
    match arena.inst_data(inst).kind() {
        InstKind::Integer(value) => Some(value.value()),
        _ => None,
    }
}

fn signed_power_of_two(value: i32) -> Option<(u8, bool)> {
    if value == 0 {
        return None;
    }
    let magnitude = value.unsigned_abs();
    magnitude
        .is_power_of_two()
        .then(|| (magnitude.trailing_zeros() as u8, value.is_negative()))
}

fn lower_signed_div_rem_power_of_two(
    ctx: &mut LowerContext<'_, MInst>,
    op: BinaryOp,
    dst: Writable<taki_mir::register::Reg>,
    lhs: taki_mir::register::Reg,
    divisor: i32,
) -> bool {
    let Some((shift, negate_quotient)) = signed_power_of_two(divisor) else {
        return false;
    };
    let size = OperandSize::Size32;

    if shift == 0 {
        match op {
            BinaryOp::Div if negate_quotient => ctx.emit(MInst::AluRRR {
                op: AluOp::Sub,
                size,
                dst,
                lhs: RegOrZr::Zr,
                rhs: RegOrZr::Reg(lhs),
            }),
            BinaryOp::Div => ctx.emit(MInst::Mov {
                size,
                dst,
                src: lhs,
            }),
            BinaryOp::Rem => ctx.emit(MInst::MovFromZero { size, dst }),
            _ => return false,
        }
        return true;
    }

    let sign = ctx.alloc_tmp(HirType::get_i32());
    ctx.emit(MInst::AluRRImmShift {
        op: AluOp::Asr,
        size,
        dst: Writable::from_reg(sign),
        src: lhs,
        shift: ImmShift::new(31, size).unwrap(),
    });
    let biased = ctx.alloc_tmp(HirType::get_i32());
    ctx.emit(MInst::AluRRRShift {
        op: AluOp::Add,
        size,
        dst: Writable::from_reg(biased),
        lhs: RegOrZr::Reg(lhs),
        rhs: RegOrZr::Reg(sign),
        shift: ShiftOp::Lsr,
        amount: ImmShift::new(32 - shift, size).unwrap(),
    });

    let quotient = if op == BinaryOp::Rem || negate_quotient {
        ctx.alloc_tmp(HirType::get_i32())
    } else {
        dst.to_reg()
    };
    ctx.emit(MInst::AluRRImmShift {
        op: AluOp::Asr,
        size,
        dst: Writable::from_reg(quotient),
        src: biased,
        shift: ImmShift::new(shift, size).unwrap(),
    });

    match op {
        BinaryOp::Div if negate_quotient => ctx.emit(MInst::AluRRR {
            op: AluOp::Sub,
            size,
            dst,
            lhs: RegOrZr::Zr,
            rhs: RegOrZr::Reg(quotient),
        }),
        BinaryOp::Div => {}
        BinaryOp::Rem => ctx.emit(MInst::AluRRRShift {
            op: AluOp::Sub,
            size,
            dst,
            lhs: RegOrZr::Reg(lhs),
            rhs: RegOrZr::Reg(quotient),
            shift: ShiftOp::Lsl,
            amount: ImmShift::new(shift, size).unwrap(),
        }),
        _ => return false,
    }
    true
}

/// Selects a signed division or remainder by a constant that is neither a
/// power of two nor `0`, `1` or `-1`, replacing `sdiv` with the multiply-high
/// sequence of [`signed_magic_i32`].
///
/// `smull` keeps the full 64-bit product, so the multiply-high and the
/// magic-number shift collapse into a single arithmetic shift whenever the
/// multiplier needs no correction term.
fn lower_signed_div_rem_magic(
    ctx: &mut LowerContext<'_, MInst>,
    op: BinaryOp,
    dst: Writable<taki_mir::register::Reg>,
    lhs: taki_mir::register::Reg,
    divisor: i32,
) -> bool {
    if !matches!(op, BinaryOp::Div | BinaryOp::Rem) {
        return false;
    }
    let Some(magic) = signed_magic_i32(divisor) else {
        return false;
    };
    let size = OperandSize::Size32;

    let multiplier = ctx.alloc_tmp(HirType::get_i32());
    ctx.emit(MInst::LoadImm {
        size,
        dst: Writable::from_reg(multiplier),
        value: u64::from(magic.multiplier as u32),
    });
    let product = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
    ctx.emit(MInst::SMulL {
        dst: Writable::from_reg(product),
        lhs,
        rhs: multiplier,
    });

    // Every value below the product fits in 32 bits, so the temporaries stay
    // `i32` even where an instruction writes the whole 64-bit register.
    let shifted = ctx.alloc_tmp(HirType::get_i32());
    match magic.correction {
        MagicCorrection::None => ctx.emit(MInst::AluRRImmShift {
            op: AluOp::Asr,
            size: OperandSize::Size64,
            dst: Writable::from_reg(shifted),
            src: product,
            shift: ImmShift::new(32 + magic.shift, OperandSize::Size64).unwrap(),
        }),
        correction => {
            let high = ctx.alloc_tmp(HirType::get_i32());
            ctx.emit(MInst::AluRRImmShift {
                op: AluOp::Asr,
                size: OperandSize::Size64,
                dst: Writable::from_reg(high),
                src: product,
                shift: ImmShift::new(32, OperandSize::Size64).unwrap(),
            });
            let corrected = if magic.shift == 0 {
                shifted
            } else {
                ctx.alloc_tmp(HirType::get_i32())
            };
            ctx.emit(MInst::AluRRR {
                op: if correction == MagicCorrection::AddNumerator {
                    AluOp::Add
                } else {
                    AluOp::Sub
                },
                size,
                dst: Writable::from_reg(corrected),
                lhs: RegOrZr::Reg(high),
                rhs: RegOrZr::Reg(lhs),
            });
            if magic.shift != 0 {
                ctx.emit(MInst::AluRRImmShift {
                    op: AluOp::Asr,
                    size,
                    dst: Writable::from_reg(shifted),
                    src: corrected,
                    shift: ImmShift::new(magic.shift, size).unwrap(),
                });
            }
        }
    }

    // The shifts round toward negative infinity; adding the sign bit turns
    // that into the truncating quotient SysY requires.
    let quotient = if op == BinaryOp::Rem {
        ctx.alloc_tmp(HirType::get_i32())
    } else {
        dst.to_reg()
    };
    ctx.emit(MInst::AluRRRShift {
        op: AluOp::Add,
        size,
        dst: Writable::from_reg(quotient),
        lhs: RegOrZr::Reg(shifted),
        rhs: RegOrZr::Reg(shifted),
        shift: ShiftOp::Lsr,
        amount: ImmShift::new(31, size).unwrap(),
    });

    if op == BinaryOp::Rem {
        let divisor_reg = ctx.alloc_tmp(HirType::get_i32());
        ctx.emit(MInst::LoadImm {
            size,
            dst: Writable::from_reg(divisor_reg),
            value: u64::from(divisor as u32),
        });
        ctx.emit(MInst::MSub {
            size,
            dst,
            lhs: quotient,
            rhs: divisor_reg,
            subtrahend: lhs,
        });
    }
    true
}

/// Fold `lhs * rhs` when exactly one operand is a constant whose multiplier
/// has a single-instruction form (see `fold_mul_constant`). Runs before any
/// operand is materialized so the constant itself never loads. Returns
/// `Some` when folded; the caller then skips normal multiplication selection.
fn try_fold_mul_constant(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    binary: &Binary,
    dst: Writable<taki_mir::register::Reg>,
) -> Option<LoweredOutput> {
    let lhs_imm = integer_constant(arena, binary.lhs());
    let rhs_imm = integer_constant(arena, binary.rhs());
    let value = lhs_imm.or(rhs_imm)?;
    let operand = if lhs_imm.is_some() {
        binary.rhs()
    } else {
        binary.lhs()
    };
    let x = ctx.put_value_in_reg(operand);
    if value == 1 {
        return Some(LoweredOutput::Value(x));
    }
    let size = operand_size(arena.inst_data(inst).ty().kind());
    let form = fold_mul_constant(value, size)?;
    match form {
        MulConstForm::Lsl(shift) => ctx.emit(MInst::AluRRImmShift {
            op: alu_op(BinaryOp::Shl),
            size,
            dst,
            src: x,
            shift,
        }),
        MulConstForm::AddLsl(amount) => ctx.emit(MInst::AluRRRShift {
            op: AluOp::Add,
            size,
            dst,
            lhs: RegOrZr::Reg(x),
            rhs: RegOrZr::Reg(x),
            shift: ShiftOp::Lsl,
            amount,
        }),
        MulConstForm::SubLsl(amount) => ctx.emit(MInst::AluRRRShift {
            op: AluOp::Sub,
            size,
            dst,
            lhs: RegOrZr::Reg(x),
            rhs: RegOrZr::Reg(x),
            shift: ShiftOp::Lsl,
            amount,
        }),
        MulConstForm::NegLsl(amount) => ctx.emit(MInst::AluRRRShift {
            op: AluOp::Sub,
            size,
            dst,
            lhs: RegOrZr::Zr,
            rhs: RegOrZr::Reg(x),
            shift: ShiftOp::Lsl,
            amount,
        }),
    }
    Some(LoweredOutput::Value(ctx.result_reg(inst)))
}

/// A single-instruction rewrite of `x * constant`.
enum MulConstForm {
    /// `x << n` for value = 2^n (n >= 1).
    Lsl(ImmShift),
    /// `x + (x << n)` for value = 2^n + 1 (n >= 1).
    AddLsl(ImmShift),
    /// `x - (x << n)` for value = -(2^n - 1) (n >= 0).
    SubLsl(ImmShift),
    /// `xzr - (x << n)` for value = -2^n (n >= 1).
    NegLsl(ImmShift),
}

/// Fold `x * value` into a single AArch64 instruction: powers of two
/// (`lsl`), 2^n+1 (`add x, x, x, lsl #n`), -(2^n-1) (`sub x, x, x, lsl #n`),
/// and -2^n (`sub x, xzr, x, lsl #n`). Returns `None` when no single
/// instruction encodes the multiplier; the caller falls back to `mul`.
fn fold_mul_constant(value: i32, size: OperandSize) -> Option<MulConstForm> {
    let pow2_shift = |v: i64| -> Option<ImmShift> {
        let n = u8::try_from(v.trailing_zeros()).ok()?;
        ImmShift::new(n, size)
    };
    if value > 1 && (value as u32).is_power_of_two() {
        return pow2_shift(value.into()).map(MulConstForm::Lsl);
    }
    if value > 2 && ((value - 1) as u32).is_power_of_two() {
        return pow2_shift((value - 1).into()).map(MulConstForm::AddLsl);
    }
    let magnitude = value.unsigned_abs();
    if value < 0 && (magnitude + 1).is_power_of_two() {
        return pow2_shift((magnitude + 1).into()).map(MulConstForm::SubLsl);
    }
    if value < -1 && magnitude.is_power_of_two() {
        return pow2_shift(magnitude.into()).map(MulConstForm::NegLsl);
    }
    None
}

/// Fold a single-use integer or pointer multiplication into an add/sub
/// consumer. The sink claim happens only after all shape and type checks, so
/// a rejected candidate follows normal instruction selection unchanged.
fn fold_mul_add_sub(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    consumer: HirInst,
    op: BinaryOp,
    lhs: HirInst,
    rhs: HirInst,
    size: OperandSize,
) -> Option<(
    BinaryOp,
    taki_mir::register::Reg,
    taki_mir::register::Reg,
    taki_mir::register::Reg,
)> {
    let (mul_inst, addend) = match op {
        BinaryOp::Add if !is_mul(arena, lhs) && is_mul(arena, rhs) => (rhs, lhs),
        BinaryOp::Add if is_mul(arena, lhs) && !is_mul(arena, rhs) => (lhs, rhs),
        BinaryOp::Sub if !is_mul(arena, lhs) && is_mul(arena, rhs) => (rhs, lhs),
        // Both operands are multiplications: fold the LHS into the
        // accumulate, materializing the RHS multiplication as the addend.
        BinaryOp::Add if is_mul(arena, lhs) && is_mul(arena, rhs) => (lhs, rhs),
        _ => return None,
    };
    let InstKind::Binary(mul) = arena.inst_data(mul_inst).kind() else {
        unreachable!("multiply candidate must be a binary instruction");
    };

    if mul.op() != BinaryOp::Mul
        || !fusion_types_match(
            arena,
            consumer,
            lhs,
            rhs,
            mul_inst,
            mul.lhs(),
            mul.rhs(),
            size,
        )
        || !ctx.sink_pure_single_use_producer(mul_inst, consumer)
    {
        return None;
    }

    Some((
        op,
        ctx.put_value_in_reg(mul.lhs()),
        ctx.put_value_in_reg(mul.rhs()),
        ctx.put_value_in_reg(addend),
    ))
}

fn is_mul(arena: ArenaContext<'_>, inst: HirInst) -> bool {
    matches!(
        arena.inst_data(inst).kind(),
        InstKind::Binary(binary) if binary.op() == BinaryOp::Mul
    )
}

fn fusion_types_match(
    arena: ArenaContext<'_>,
    consumer: HirInst,
    lhs: HirInst,
    rhs: HirInst,
    mul: HirInst,
    mul_lhs: HirInst,
    mul_rhs: HirInst,
    size: OperandSize,
) -> bool {
    [consumer, lhs, rhs, mul, mul_lhs, mul_rhs]
        .into_iter()
        .all(|inst| {
            let ty = arena.inst_data(inst).ty().kind();
            matches!(ty, TypeKind::Int32 | TypeKind::Pointer(_)) && operand_size(ty) == size
        })
}

/// Fold `rhs = input <<const shift` (or its logical/arithmetic right-shift
/// counterparts) into an AArch64 shifted-register data-processing operand.
/// The generic context atomically claims the producer before we name its
/// input, preventing its later reverse-traversal lowering.
fn fold_shifted_rhs(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    consumer: HirInst,
    rhs: HirInst,
    size: OperandSize,
) -> Option<(taki_mir::register::Reg, ShiftOp, ImmShift)> {
    let InstKind::Binary(shift) = arena.inst_data(rhs).kind() else {
        return None;
    };
    let shift_op = match shift.op() {
        BinaryOp::Shl => ShiftOp::Lsl,
        BinaryOp::Shr => ShiftOp::Lsr,
        BinaryOp::Sar => ShiftOp::Asr,
        _ => return None,
    };
    let amount = integer_constant(arena, shift.rhs())
        .and_then(|value| u8::try_from(value).ok())
        .and_then(|value| ImmShift::new(value, size))?;
    if !ctx.sink_pure_single_use_producer(rhs, consumer) {
        return None;
    }
    Some((ctx.put_value_in_reg(shift.lhs()), shift_op, amount))
}

fn extended_index_shift(stride: u64) -> Option<u8> {
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
        TypeKind::Vector(..) => MemoryType::Vec128,
        ty => unreachable!("unsupported AArch64 integer memory type: {ty:?}"),
    }
}

fn lower_store(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    store: &Store,
) -> LoweredOutput {
    let src = store.src();
    let dest = store.dest();
    match arena.inst_data(src).kind() {
        InstKind::Aggregate(aggregate) => {
            let (base, base_offset) = store_base(ctx, arena, dest, inst);
            let mut offset = base_offset;
            for value in aggregate.flatten(&arena) {
                let ty = arena.inst_data(value).ty();
                if matches!(arena.inst_data(value).kind(), InstKind::ZeroInit) {
                    emit_zero_init(ctx, base, ty, offset);
                } else {
                    let value = ctx.put_value_in_reg(value);
                    emit_store_at(ctx, value, ty, base, offset);
                }
                offset += ty.size() as i64;
            }
        }
        InstKind::ZeroInit => {
            let (base, base_offset) = store_base(ctx, arena, dest, inst);
            emit_zero_init(ctx, base, arena.inst_data(src).ty(), base_offset);
        }
        _ => {
            let value = ctx.put_value_in_reg(src);
            let ty = arena.inst_data(src).ty();
            // v2: a single dynamic index folds into extended-register
            // addressing for single-element stores.
            if matches!(arena.inst_data(dest).kind(), InstKind::GetElemPtr(..)) {
                let memory_ty = memory_type(ty.kind());
                if let Some(addr) = try_fold_dynamic_gep_amode(ctx, arena, dest, inst, memory_ty) {
                    ctx.emit(MInst::Store {
                        ty: memory_ty,
                        src: value,
                        addr,
                    });
                    return LoweredOutput::None;
                }
            }
            let (base, base_offset) = store_base(ctx, arena, dest, inst);
            emit_store_at(ctx, value, ty, base, base_offset);
        }
    }
    LoweredOutput::None
}

/// Fold a GEP destination into a `(base, offset)` pair via constant-offset
/// folding, or materialize the destination address. Used where the store needs
/// a base register plus a running element offset (aggregate/zero expansion);
/// dynamic-index folding is handled separately in `lower_store`.
fn store_base(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    dest: HirInst,
    inst: HirInst,
) -> (taki_mir::register::Reg, i64) {
    match arena.inst_data(dest).kind() {
        InstKind::GetElemPtr(..) => try_fold_gep_offset(ctx, arena, dest, inst, 4)
            .unwrap_or_else(|| (ctx.put_value_in_reg(dest), 0)),
        _ => (ctx.put_value_in_reg(dest), 0),
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
        TypeKind::Vector(..) => {
            let zero = ctx.alloc_tmp(ty.clone());
            let shape = vector_shape(ty.kind());
            ctx.emit(MInst::VecMovImm {
                shape,
                dst: Writable::from_reg(zero),
                imm: 0,
                shift: 0,
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
    let addr = memory_address(ctx, base, offset, memory_ty);
    ctx.emit(MInst::Store {
        ty: memory_ty,
        src,
        addr,
    });
}

fn memory_address(
    ctx: &mut LowerContext<'_, MInst>,
    base: taki_mir::register::Reg,
    offset: i64,
    ty: MemoryType,
) -> AMode {
    if offset == 0 {
        return AMode::Reg { base: base };
    }
    if offset > 0 {
        if let Some(offset) = crate::instructions::UImm12Scaled::new(offset as u64, ty.byte_size())
        {
            return AMode::UnsignedOffset { base: base, offset };
        }
    }
    if let Ok(offset) = i16::try_from(offset) {
        if let Some(offset) = crate::instructions::SImm9::new(offset) {
            return AMode::SignedOffset { base: base, offset };
        }
    }

    let address = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
    emit_add_offset(ctx, Writable::from_reg(address), base, offset);
    AMode::Reg { base: address }
}

/// Fold a single-use constant GEP into a `(base, offset)` pair whose offset is
/// encodable in AArch64 load/store addressing for `width`-byte accesses.
/// Returns `None` for dynamic-index GEPs, unencodable offsets, or multi-user
/// GEPs; callers then materialize the address as before.
fn try_fold_gep_offset(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    gep: HirInst,
    consumer: HirInst,
    width: u8,
) -> Option<(taki_mir::register::Reg, i64)> {
    fold_gep_constant_offset(ctx, arena, gep, consumer, |off| {
        crate::instructions::UImm12Scaled::new(off as u64, width).is_some()
            || i16::try_from(off)
                .ok()
                .and_then(crate::instructions::SImm9::new)
                .is_some()
    })
}

/// Fold a single-use GEP with exactly one dynamic index whose stride matches
/// the access width (or is 1) into AArch64 extended-register addressing
/// (`[base, index, sxtw #scale]`). A nonzero constant offset is folded into a
/// fresh base temporary so the original base register is not clobbered.
/// Returns `None` for other GEP shapes; callers then try constant-offset
/// folding or materialize the address.
fn try_fold_dynamic_gep_amode(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    gep: HirInst,
    consumer: HirInst,
    memory_ty: MemoryType,
) -> Option<AMode> {
    let analysis = sink_gep_into_address(ctx, arena, gep, consumer, |a| {
        a.dynamic_terms.len() == 1 && a.dynamic_terms[0].stride.is_power_of_two() && {
            let shift = a.dynamic_terms[0].stride.trailing_zeros() as u8;
            // The extended-register scale must equal the access size's
            // log2 (or be 0); anything else is not encodable.
            shift == 0 || shift == memory_ty.byte_size().trailing_zeros() as u8
        }
    })?;
    let term = &analysis.dynamic_terms[0];
    let shift = term.stride.trailing_zeros() as u8;
    let base = ctx.put_value_in_reg(analysis.base);
    let base = if analysis.constant_offset != 0 {
        let tmp = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
        emit_add_offset(ctx, Writable::from_reg(tmp), base, analysis.constant_offset);
        tmp
    } else {
        base
    };
    let index = ctx.put_value_in_reg(term.index);
    Some(AMode::ExtendedRegOffset {
        base,
        index,
        extend: ExtendOp::Sxtw,
        shift,
    })
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
            src: base,
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

#[cfg(test)]
mod tests {
    use super::{
        MulConstForm, fold_mul_constant, nzcv_making_cond_false, nzcv_making_cond_true,
        signed_power_of_two,
    };
    use crate::instructions::{Cond, ImmShift, MInst};
    use crate::regs::OperandSize;
    use raana_ir::ir::{BinaryOp, Program, Type};
    use taki_mir::{
        reg_alloc::reg::{PReg, RegClass, SpillSlot},
        register::{Reg, VRegAllocator},
        types::{F32, I32, I64},
    };

    #[test]
    fn register_aliases_resolve_transitively() {
        let mut allocator = VRegAllocator::<MInst>::with_capaticy(3);
        let first = allocator.alloc(I32);
        let second = allocator.alloc(I32);
        let canonical = allocator.alloc(I32);

        allocator.set_reg_alias(first, second);
        allocator.set_reg_alias(second, canonical);

        assert_eq!(
            allocator.resolve_alias(first.to_virtual_reg().unwrap()),
            canonical.to_virtual_reg().unwrap()
        );
        assert_eq!(
            allocator.resolve_alias(second.to_virtual_reg().unwrap()),
            canonical.to_virtual_reg().unwrap()
        );
    }

    #[test]
    #[should_panic(expected = "register alias source was already assigned")]
    fn register_alias_source_is_single_assignment() {
        let mut allocator = VRegAllocator::<MInst>::with_capaticy(3);
        let source = allocator.alloc(I32);
        let first = allocator.alloc(I32);
        let second = allocator.alloc(I32);

        allocator.set_reg_alias(source, first);
        allocator.set_reg_alias(source, second);
    }

    #[test]
    #[should_panic(expected = "register alias would form a cycle")]
    fn register_alias_cycles_are_rejected() {
        let mut allocator = VRegAllocator::<MInst>::with_capaticy(2);
        let first = allocator.alloc(I32);
        let second = allocator.alloc(I32);

        allocator.set_reg_alias(first, second);
        allocator.set_reg_alias(second, first);
    }

    #[test]
    #[should_panic(expected = "register aliases must have identical lowered types")]
    fn register_aliases_reject_different_classes() {
        let mut allocator = VRegAllocator::<MInst>::with_capaticy(2);
        let integer = allocator.alloc(I32);
        let float = allocator.alloc(F32);

        allocator.set_reg_alias(integer, float);
    }

    #[test]
    #[should_panic(expected = "register aliases must have identical lowered types")]
    fn register_aliases_reject_different_widths_in_the_same_class() {
        let mut allocator = VRegAllocator::<MInst>::with_capaticy(2);
        let narrow = allocator.alloc(I32);
        let wide = allocator.alloc(I64);

        allocator.set_reg_alias(narrow, wide);
    }

    #[test]
    #[should_panic(expected = "register alias target must be a virtual register")]
    fn register_aliases_reject_physical_registers() {
        let mut allocator = VRegAllocator::<MInst>::with_capaticy(1);
        let source = allocator.alloc(I32);
        let physical = Reg::from_physical_reg(PReg::new(0, RegClass::Int));

        allocator.set_reg_alias(source, physical);
    }

    #[test]
    #[should_panic(expected = "register alias target must be a virtual register")]
    fn register_aliases_reject_spill_slots() {
        let mut allocator = VRegAllocator::<MInst>::with_capaticy(1);
        let source = allocator.alloc(I32);
        let spill = Reg::from_spillslot(SpillSlot::new(0));

        allocator.set_reg_alias(source, spill);
    }

    /// Emits `parameter <op> divisor` as a whole function and returns its
    /// AArch64 assembly.
    fn compile_constant_binary(op: BinaryOp, divisor: i32) -> String {
        use raana_ir::ir::arena::Arena;
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "constant".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let numerator = data.params()[0];
        let constant = data.new_local_inst().integer(divisor);
        let binary = data.new_local_inst().binary(op, numerator, constant);
        let ret = data.new_local_inst().ret(Some(binary));
        data.layout_mut().insert_inst(entry, binary);
        data.layout_mut().insert_inst(entry, ret);
        taki_mir::compile::<crate::lower::AArch64Backend>(&program)
    }

    #[test]
    fn division_by_a_constant_replaces_sdiv_with_a_multiply_high() {
        for divisor in [3, 7, -7, 100, 1000000007, i32::MAX] {
            let assembly = compile_constant_binary(BinaryOp::Div, divisor);
            assert!(!assembly.contains("sdiv"), "{divisor}:\n{assembly}");
            assert!(assembly.contains("smull"), "{divisor}:\n{assembly}");
        }
    }

    #[test]
    fn remainder_by_a_constant_multiplies_the_quotient_back() {
        let assembly = compile_constant_binary(BinaryOp::Rem, 7);
        assert!(!assembly.contains("sdiv"), "{assembly}");
        assert!(assembly.contains("smull"), "{assembly}");
        assert!(assembly.contains("msub"), "{assembly}");
    }

    #[test]
    fn cheaper_divisors_keep_their_existing_sequences() {
        // Powers of two stay on the shift sequence, and a divisor of one
        // disappears entirely.
        for divisor in [2, -8, i32::MIN] {
            for op in [BinaryOp::Div, BinaryOp::Rem] {
                let assembly = compile_constant_binary(op, divisor);
                assert!(!assembly.contains("smull"), "{op:?} {divisor}:\n{assembly}");
                assert!(!assembly.contains("sdiv"), "{op:?} {divisor}:\n{assembly}");
            }
        }
        let assembly = compile_constant_binary(BinaryOp::Div, 1);
        assert!(!assembly.contains("smull"), "{assembly}");
        assert!(!assembly.contains("sdiv"), "{assembly}");
    }

    #[test]
    fn division_by_a_variable_still_uses_sdiv() {
        use raana_ir::ir::arena::Arena;
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "variable".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let numerator = data.params()[0];
        let divisor = data.params()[1];
        let binary = data
            .new_local_inst()
            .binary(BinaryOp::Div, numerator, divisor);
        let ret = data.new_local_inst().ret(Some(binary));
        data.layout_mut().insert_inst(entry, binary);
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        assert!(assembly.contains("sdiv"), "{assembly}");
    }

    #[test]
    fn classifies_signed_power_of_two_divisors() {
        assert_eq!(signed_power_of_two(0), None);
        assert_eq!(signed_power_of_two(1), Some((0, false)));
        assert_eq!(signed_power_of_two(-1), Some((0, true)));
        assert_eq!(signed_power_of_two(2), Some((1, false)));
        assert_eq!(signed_power_of_two(-2), Some((1, true)));
        assert_eq!(signed_power_of_two(1 << 30), Some((30, false)));
        assert_eq!(signed_power_of_two(-(1 << 30)), Some((30, true)));
        assert_eq!(signed_power_of_two(i32::MIN), Some((31, true)));
        assert_eq!(signed_power_of_two(3), None);
        assert_eq!(signed_power_of_two(-3), None);
    }

    #[test]
    fn signed_power_of_two_formula_truncates_toward_zero() {
        let dividends = [i32::MIN, -17, -9, -8, -7, -1, 0, 1, 7, 8, 9, 17, i32::MAX];
        let divisors = [1, -1, 2, -2, 4, -4, 8, -8, 1 << 30, -(1 << 30), i32::MIN];

        for dividend in dividends {
            for divisor in divisors {
                let (shift, negate) = signed_power_of_two(divisor).unwrap();
                let positive_quotient = if shift == 0 {
                    dividend
                } else {
                    let sign = dividend >> 31;
                    let bias = ((sign as u32) >> (32 - shift)) as i32;
                    dividend.wrapping_add(bias) >> shift
                };
                let quotient = if negate {
                    positive_quotient.wrapping_neg()
                } else {
                    positive_quotient
                };
                let remainder =
                    dividend.wrapping_sub(positive_quotient.wrapping_shl(u32::from(shift)));

                let expected_quotient = if dividend == i32::MIN && divisor == -1 {
                    i32::MIN
                } else {
                    dividend / divisor
                };
                let expected_remainder = if divisor == -1 { 0 } else { dividend % divisor };
                assert_eq!(quotient, expected_quotient, "{dividend} / {divisor}");
                assert_eq!(remainder, expected_remainder, "{dividend} % {divisor}");
            }
        }
    }

    /// The NZCV fallback values written by `ccmp` must flip the named
    /// condition against the result of the actual comparison.  A wrong
    /// fallback silently changes the combined `band`/`bor` semantics (the
    /// `Le => 8` bug made `LE` evaluate true when the ccmp did not run).
    #[test]
    fn ccmp_nzcv_fallbacks_flip_each_condition() {
        for cond in [
            Cond::Eq,
            Cond::Ne,
            Cond::Hs,
            Cond::Lo,
            Cond::Mi,
            Cond::Pl,
            Cond::Vs,
            Cond::Vc,
            Cond::Hi,
            Cond::Ls,
            Cond::Ge,
            Cond::Lt,
            Cond::Gt,
            Cond::Le,
        ] {
            let false_flags = nzcv_making_cond_false(cond);
            let true_flags = nzcv_making_cond_true(cond);
            assert!(
                !evaluates(cond, false_flags),
                "nzcv_making_cond_false({cond:?}) = #{false_flags:x} must make it false"
            );
            assert!(
                evaluates(cond, true_flags),
                "nzcv_making_cond_true({cond:?}) = #{true_flags:x} must make it true"
            );
        }
    }

    /// Evaluate a condition against a bare NZCV value, using the same
    /// definition AArch64 uses (`LE` is `Z || (N != V)`, `GT` is
    /// `Z == 0 && N == V`, ...).
    fn evaluates(cond: Cond, nzcv: u8) -> bool {
        let n = nzcv >> 3 & 1 == 1;
        let z = nzcv >> 2 & 1 == 1;
        let v = nzcv & 1 == 1;
        match cond {
            Cond::Eq => z,
            Cond::Ne => !z,
            Cond::Hs => nzcv >> 1 & 1 == 1,
            Cond::Lo => nzcv >> 1 & 1 == 0,
            Cond::Mi => n,
            Cond::Pl => !n,
            Cond::Vs => v,
            Cond::Vc => !v,
            Cond::Hi => nzcv >> 1 & 1 == 1 && !z,
            Cond::Ls => nzcv >> 1 & 1 == 0 || z,
            Cond::Ge => n == v,
            Cond::Lt => n != v,
            Cond::Gt => !z && n == v,
            Cond::Le => z || n != v,
        }
    }

    #[test]
    fn single_use_dynamic_gep_folds_into_extended_addressing() {
        use raana_ir::ir::arena::Arena;
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "gep_dyn".into(),
            vec![Type::get_pointer(Type::get_i32()), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let index = data.params()[1];
        let gep = data.new_local_inst().get_elem_ptr(base, vec![index]);
        let load = data.new_local_inst().load(gep);
        data.layout_mut().insert_inst(entry, load);
        let ret = data.new_local_inst().ret(Some(load));
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        // The dynamic index folds into the load addressing mode:
        // `ldr w?, [x?, x?, sxtw #2]` (stride 4 → scale 2).
        assert!(assembly.contains("sxtw #2]"), "{assembly}");
    }

    #[test]
    fn dynamic_gep_with_constant_offset_folds_into_extended_addressing() {
        use raana_ir::ir::arena::Arena;
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "gep_dyn_const".into(),
            vec![
                Type::get_pointer(Type::get_array(Type::get_i32(), 1)),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let index = data.params()[1];
        let one = data.new_local_inst().integer(1);
        let gep = data.new_local_inst().get_elem_ptr(base, vec![index, one]);
        let forty_two = data.new_local_inst().integer(42);
        let store = data.new_local_inst().store(forty_two, gep);
        data.layout_mut().insert_inst(entry, store);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        // Dynamic term: stride 4 → scale 2 (matches the 4-byte store);
        // constant +4 folds into a fresh base temporary.
        assert!(assembly.contains("sxtw #2]"), "{assembly}");
    }

    #[test]
    fn multi_use_dynamic_gep_is_not_folded() {
        use raana_ir::ir::arena::Arena;
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "gep_dyn_multi".into(),
            vec![Type::get_pointer(Type::get_i32()), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let index = data.params()[1];
        let gep = data.new_local_inst().get_elem_ptr(base, vec![index]);
        let load = data.new_local_inst().load(gep);
        let forty_two = data.new_local_inst().integer(42);
        let store = data.new_local_inst().store(forty_two, gep);
        data.layout_mut().insert_inst(entry, gep);
        data.layout_mut().insert_inst(entry, load);
        data.layout_mut().insert_inst(entry, store);
        let ret = data.new_local_inst().ret(Some(load));
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        // Two users: the GEP must be materialized, so no extended-register
        // *addressing* form (an ALU `add ..., sxtw #2` may still appear).
        assert!(!assembly.contains("sxtw #2]"), "{assembly}");
    }

    #[test]
    fn mul_mul_add_folds_into_madd() {
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "mul_mul_add".into(),
            vec![
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let [a, b, c, d] = [
            data.params()[0],
            data.params()[1],
            data.params()[2],
            data.params()[3],
        ];
        let mul1 = data.new_local_inst().binary(BinaryOp::Mul, a, b);
        let mul2 = data.new_local_inst().binary(BinaryOp::Mul, c, d);
        let add = data.new_local_inst().binary(BinaryOp::Add, mul1, mul2);
        data.layout_mut().insert_inst(entry, mul1);
        data.layout_mut().insert_inst(entry, mul2);
        data.layout_mut().insert_inst(entry, add);
        let ret = data.new_local_inst().ret(Some(add));
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        // mul(mul, mul) + add folds: one madd (lhs fused) + one standalone mul
        // (the addend), no separate add.
        assert_eq!(assembly.matches("madd").count(), 1, "{assembly}");
        assert_eq!(assembly.matches("\n    mul ").count(), 1, "{assembly}");
        assert!(!assembly.contains("\n    add w"), "{assembly}");
    }

    #[test]
    fn multi_use_mul_blocks_madd_fold() {
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "mul_mul_add_multi".into(),
            vec![
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
                Type::get_pointer(Type::get_i32()),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let [a, b, c, d] = [
            data.params()[0],
            data.params()[1],
            data.params()[2],
            data.params()[3],
        ];
        let ptr = data.params()[4];
        let mul1 = data.new_local_inst().binary(BinaryOp::Mul, a, b);
        let mul2 = data.new_local_inst().binary(BinaryOp::Mul, c, d);
        let add = data.new_local_inst().binary(BinaryOp::Add, mul1, mul2);
        // mul1 has a second user (the store), so it must remain materialized.
        // The single-use mul2 may still fold with mul1 as the madd addend.
        let store = data.new_local_inst().store(mul1, ptr);
        data.layout_mut().insert_inst(entry, mul1);
        data.layout_mut().insert_inst(entry, mul2);
        data.layout_mut().insert_inst(entry, add);
        data.layout_mut().insert_inst(entry, store);
        let ret = data.new_local_inst().ret(Some(add));
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        assert_eq!(assembly.matches("madd").count(), 1, "{assembly}");
        assert_eq!(assembly.matches("\n    mul ").count(), 1, "{assembly}");
        assert!(!assembly.contains("\n    add w"), "{assembly}");
        assert!(assembly.contains("str w"), "{assembly}");
    }

    #[test]
    fn classifies_mul_constants() {
        let s32 = OperandSize::Size32;
        let shift = |v: u8| ImmShift::new(v, s32).unwrap();
        assert!(matches!(fold_mul_constant(2, s32), Some(MulConstForm::Lsl(s)) if s == shift(1)));
        assert!(matches!(fold_mul_constant(8, s32), Some(MulConstForm::Lsl(s)) if s == shift(3)));
        assert!(
            matches!(fold_mul_constant(3, s32), Some(MulConstForm::AddLsl(s)) if s == shift(1))
        );
        assert!(
            matches!(fold_mul_constant(5, s32), Some(MulConstForm::AddLsl(s)) if s == shift(2))
        );
        assert!(
            matches!(fold_mul_constant(9, s32), Some(MulConstForm::AddLsl(s)) if s == shift(3))
        );
        assert!(
            matches!(fold_mul_constant(-1, s32), Some(MulConstForm::SubLsl(s)) if s == shift(1))
        );
        assert!(
            matches!(fold_mul_constant(-3, s32), Some(MulConstForm::SubLsl(s)) if s == shift(2))
        );
        assert!(
            matches!(fold_mul_constant(-7, s32), Some(MulConstForm::SubLsl(s)) if s == shift(3))
        );
        assert!(
            matches!(fold_mul_constant(-2, s32), Some(MulConstForm::NegLsl(s)) if s == shift(1))
        );
        assert!(
            matches!(fold_mul_constant(-8, s32), Some(MulConstForm::NegLsl(s)) if s == shift(3))
        );
        // Not encodable in one instruction.
        assert!(fold_mul_constant(6, s32).is_none());
        assert!(fold_mul_constant(7, s32).is_none());
        assert!(fold_mul_constant(0, s32).is_none());
        assert!(fold_mul_constant(1, s32).is_none());
    }

    #[test]
    fn constant_mul_folds_to_shift_or_add_shift() {
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "mul_const".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let x = data.params()[0];
        let three = data.new_local_inst().integer(3);
        let mul = data.new_local_inst().binary(BinaryOp::Mul, x, three);
        data.layout_mut().insert_inst(entry, mul);
        let ret = data.new_local_inst().ret(Some(mul));
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        // x * 3 = x + (x << 1): one add-shift, no mul, no movz 3.
        assert!(assembly.contains("lsl #1"), "{assembly}");
        assert!(!assembly.contains("\n    mul "), "{assembly}");
        assert!(!assembly.contains("movz"), "{assembly}");
    }

    #[test]
    fn unencodable_constant_mul_falls_back() {
        use raana_ir::ir::builder_trait::*;

        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "mul_const_fallback".into(),
            vec![Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let x = data.params()[0];
        let six = data.new_local_inst().integer(6);
        let mul = data.new_local_inst().binary(BinaryOp::Mul, x, six);
        data.layout_mut().insert_inst(entry, mul);
        let ret = data.new_local_inst().ret(Some(mul));
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        // 6 is not a single-instruction multiplier: keep mul.
        assert_eq!(assembly.matches("\n    mul ").count(), 1, "{assembly}");
    }

    #[test]
    fn explicit_vector_vcode_emits_neon_assembly() {
        use crate::abi::AArch64Abi;
        use crate::instructions::{VecArithOp, VecShape};
        use raana_ir::ir::builder_trait::*;
        use taki_mir::abi::CalleeABI;
        use taki_mir::block_order::BlockLoweringOrder;
        use taki_mir::prelude::ArenaContext;
        use taki_mir::register::Writable;
        use taki_mir::types::V4I32;
        use taki_mir::vcode::VCodeBuilder;

        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "vec_test".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);
        let func_data = program.func_data(function);

        let arena = ArenaContext {
            program: &program,
            curr_func: Some(function),
        };
        let abi = CalleeABI::<AArch64Abi>::new(arena);
        let order = BlockLoweringOrder::new(arena);
        let mut builder = VCodeBuilder::new(abi, order);
        let mut vregs = VRegAllocator::<MInst>::with_capaticy(6);
        let z0 = vregs.alloc(I32);
        let z1 = vregs.alloc(I32);
        let v0 = vregs.alloc(V4I32);
        let v1 = vregs.alloc(V4I32);
        let sum = vregs.alloc(V4I32);
        let acc = vregs.alloc(F32);
        // Pushed in reverse of final order: dup(0)+dup(0) -> add -> addv -> ret.
        builder.push(MInst::Ret);
        builder.push(MInst::VecAddv {
            dst: Writable::from_reg(acc),
            src: sum,
        });
        builder.push(MInst::VecArithRRR {
            op: VecArithOp::Add,
            shape: VecShape::FourS,
            dst: Writable::from_reg(sum),
            lhs: v0,
            rhs: v1,
        });
        builder.push(MInst::VecDup {
            shape: VecShape::FourS,
            dst: Writable::from_reg(v1),
            src: z1,
        });
        builder.push(MInst::VecDup {
            shape: VecShape::FourS,
            dst: Writable::from_reg(v0),
            src: z0,
        });
        builder.push(MInst::MovFromZero {
            size: OperandSize::Size32,
            dst: Writable::from_reg(z1),
        });
        builder.push(MInst::MovFromZero {
            size: OperandSize::Size32,
            dst: Writable::from_reg(z0),
        });
        builder.end_bb();
        let mut vcode = builder.build(vregs);

        let output = taki_mir::reg_alloc::ion::run(&vcode, vcode.abi.machine_env())
            .expect("vector VCode allocation should succeed");
        assert!(vcode.verify_alloc_output(&output).is_ok());
        vcode.write_back_allocs(&output);
        let spill_size = u32::try_from(output.num_spillslots).unwrap() * vcode.abi.spill_unit_bytes();
        vcode
            .abi
            .compute_frame_layout(spill_size, &output)
            .expect("frame layout should accept vector spill units");
        vcode.finalize_for_emission(&output);

        let assembly = taki_mir::emit::emit_vcode_assembly::<crate::lower::AArch64Backend>(
            &program,
            func_data,
            &vcode,
        );
        // Register allocation assigns arbitrary vector numbers, so assert the
        // NEON forms rather than specific physical registers.
        assert!(assembly.contains("dup v"), "{assembly}");
        assert!(assembly.contains(".4s, w"), "{assembly}");
        assert!(assembly.contains("add v"), "{assembly}");
        assert!(assembly.contains(".4s, v"), "{assembly}");
        assert!(assembly.contains("addv s"), "{assembly}");
    }

    /// Construct a `<4 x i32>` kernel over vector parameters (v0-v7 ABI) and a
    /// scalar splat source, covering the integer NEON lowering surface:
    /// splat → add → mul → cmeq → bsl → smin/smax → addv → lane extract/insert.
    fn compile_vector_int_kernel() -> String {
        use raana_ir::ir::builder_trait::*;

        let v4i32 = Type::get_vector(Type::get_i32(), 4);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "vec_int_kernel".into(),
            vec![v4i32.clone(), v4i32.clone(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let v = data.params()[0];
        let w = data.params()[1];
        let s = data.params()[2];

        let splat = data.new_local_inst().vector_splat(s, v4i32.clone());
        let add = data.new_local_inst().binary(BinaryOp::Add, v, splat);
        let mul = data.new_local_inst().binary(BinaryOp::Mul, add, w);
        let mask = data.new_local_inst().binary(BinaryOp::Eq, v, w);
        let sel = data.new_local_inst().select(mask, add, mul);
        let mn = data.new_local_inst().binary(BinaryOp::Min, sel, w);
        let mx = data.new_local_inst().binary(BinaryOp::Max, mn, splat);
        let sum = data
            .new_local_inst()
            .vector_reduce(raana_ir::ir::VectorReduceOp::Add, mx);
        let zero = data.new_local_inst().integer(0);
        let e0 = data.new_local_inst().vector_extract_element(mx, zero);
        let one = data.new_local_inst().integer(1);
        let ins = data
            .new_local_inst()
            .vector_insert_element(mx, e0, one);

        for inst in [splat, add, mul, mask, sel, mn, mx, sum, e0, ins] {
            data.layout_mut().insert_inst(entry, inst);
        }
        let ret = data.new_local_inst().ret(Some(sum));
        data.layout_mut().insert_inst(entry, ret);
        taki_mir::compile::<crate::lower::AArch64Backend>(&program)
    }

    #[test]
    fn vector_ir_function_emits_neon_integer_surface() {
        let assembly = compile_vector_int_kernel();
        assert!(assembly.contains("dup v"), "{assembly}");
        assert!(assembly.contains("add v"), "{assembly}");
        assert!(assembly.contains("mul v"), "{assembly}");
        assert!(assembly.contains("cmeq"), "{assembly}");
        assert!(assembly.contains("bsl"), "{assembly}");
        assert!(assembly.contains("smin"), "{assembly}");
        assert!(assembly.contains("smax"), "{assembly}");
        assert!(assembly.contains("addv s"), "{assembly}");
        // Lane ops: extract to a GPR and insert back from it.
        assert!(assembly.contains("mov w"), "{assembly}");
        assert!(assembly.contains("v0"), "{assembly}");
        // addv writes a SIMD register; the i32 result must be moved out.
        assert!(assembly.contains("fmov w"), "{assembly}");
    }

    #[test]
    fn vector_ir_function_emits_neon_float_surface() {
        use raana_ir::ir::builder_trait::*;

        let v4i32 = Type::get_vector(Type::get_i32(), 4);
        let v4f32 = Type::get_vector(Type::get_f32(), 4);
        let mut program = Program::new();
        let function = program.new_function(
            v4f32.clone(),
            "vec_float_kernel".into(),
            vec![v4i32.clone(), Type::get_f32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let vi = data.params()[0];
        let s = data.params()[1];

        // f32 splat (from a float register), i32→f32 conversion, fmla.
        let splat_f = data.new_local_inst().vector_splat(s, v4f32.clone());
        let vf = data.new_local_inst().cast(vi, v4f32.clone());
        let acc = data.new_local_inst().fma(vf, splat_f, splat_f);

        for inst in [splat_f, vf, acc] {
            data.layout_mut().insert_inst(entry, inst);
        }
        let ret = data.new_local_inst().ret(Some(acc));
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        // dup from a float scalar is `dup v.4s, s0`; scvtf converts the lanes;
        // fmla fuses the multiply-add.
        assert!(assembly.contains("dup v"), "{assembly}");
        assert!(assembly.contains(".4s, s"), "{assembly}");
        assert!(assembly.contains("scvtf"), "{assembly}");
        assert!(assembly.contains("fmla"), "{assembly}");
    }

    #[test]
    fn vector_ir_function_emits_neon_load_store_and_reduce() {
        use raana_ir::ir::builder_trait::*;

        let v4i32 = Type::get_vector(Type::get_i32(), 4);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "vec_mem_kernel".into(),
            vec![v4i32.clone(), Type::get_pointer(v4i32.clone())],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let vec = data.params()[0];
        let ptr = data.params()[1];

        let store = data.new_local_inst().store(vec, ptr);
        let loaded = data.new_local_inst().load(ptr);
        let sum = data
            .new_local_inst()
            .vector_reduce(raana_ir::ir::VectorReduceOp::Add, loaded);

        for inst in [store, loaded, sum] {
            data.layout_mut().insert_inst(entry, inst);
        }
        let ret = data.new_local_inst().ret(Some(sum));
        data.layout_mut().insert_inst(entry, ret);

        let assembly = taki_mir::compile::<crate::lower::AArch64Backend>(&program);
        assert!(assembly.contains("str q"), "{assembly}");
        assert!(assembly.contains("ldr q"), "{assembly}");
        assert!(assembly.contains("addv s"), "{assembly}");
    }

    /// Determinism gate: compiling the vector kernel at each `-O` level must be
    /// byte-identical across recompilations.
    #[test]
    fn vector_kernel_is_deterministic_across_opt_levels() {
        use crate::config::AArch64CodegenConfig;
        use raana_ir::ir::builder_trait::*;

        let v4i32 = Type::get_vector(Type::get_i32(), 4);
        let build = |program: &Program| {
            let mut program = program.clone();
            let mut programs = Vec::new();
            for config in [
                AArch64CodegenConfig {
                    dce: false,
                    peephole_combine: false,
                    pair_combine: false,
                    list_scheduler: false,
                    sched_model: Default::default(),
                    branch_opt: false,
                    chain_fusion: false,
                },
                AArch64CodegenConfig {
                    dce: true,
                    peephole_combine: true,
                    pair_combine: true,
                    list_scheduler: false,
                    sched_model: Default::default(),
                    branch_opt: true,
                    chain_fusion: true,
                },
                AArch64CodegenConfig {
                    dce: true,
                    peephole_combine: true,
                    pair_combine: true,
                    list_scheduler: true,
                    sched_model: Default::default(),
                    branch_opt: true,
                    chain_fusion: true,
                },
            ] {
                let first =
                    taki_mir::compile_with_config::<crate::lower::AArch64Backend>(&program, &config)
                        .assembly;
                for _ in 0..4 {
                    let again =
                        taki_mir::compile_with_config::<crate::lower::AArch64Backend>(&program, &config)
                            .assembly;
                    assert_eq!(first, again, "recompilation diverged");
                }
                programs.push(first);
            }
            programs
        };

        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "vec_det".into(),
            vec![v4i32.clone(), v4i32.clone(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let v = data.params()[0];
        let w = data.params()[1];
        let s = data.params()[2];
        let splat = data.new_local_inst().vector_splat(s, v4i32.clone());
        let add = data.new_local_inst().binary(BinaryOp::Add, v, splat);
        let mul = data.new_local_inst().binary(BinaryOp::Mul, add, w);
        let mask = data.new_local_inst().binary(BinaryOp::Eq, v, w);
        let sel = data.new_local_inst().select(mask, add, mul);
        let mn = data.new_local_inst().binary(BinaryOp::Min, sel, w);
        let mx = data.new_local_inst().binary(BinaryOp::Max, mn, splat);
        let sum = data
            .new_local_inst()
            .vector_reduce(raana_ir::ir::VectorReduceOp::Add, mx);
        for inst in [splat, add, mul, mask, sel, mn, mx, sum] {
            data.layout_mut().insert_inst(entry, inst);
        }
        let ret = data.new_local_inst().ret(Some(sum));
        data.layout_mut().insert_inst(entry, ret);

        let programs = build(&program);
        // Every optimization level must produce the NEON lowering.
        for assembly in &programs {
            assert!(assembly.contains("add v"), "{assembly}");
            assert!(assembly.contains("bsl"), "{assembly}");
            assert!(assembly.contains("addv s"), "{assembly}");
        }
    }
}
