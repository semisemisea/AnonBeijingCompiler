//! NEON vector lowering helpers.

use super::arith::operand_size;
use super::*;
/// Select a vector binary operation onto the NEON instruction set.
///
/// Shape is derived from the vector type (`<4 x i32>`/`<4 x f32>` → `.4s`,
/// `<2 x i64>` → `.2d`). Operations without a NEON form panic with a clear
/// message instead of silently mis-selecting.
pub(super) fn lower_vector_binary(
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
pub(super) fn vector_shape(ty: &TypeKind) -> VecShape {
    let (elem, lanes) = match ty {
        TypeKind::Vector(elem, lanes) => (elem, *lanes),
        _ => unreachable!("vector_shape requires a vector type, got {ty:?}"),
    };
    match (lanes, elem.size()) {
        (4, 4) => VecShape::FourS,
        (2, 8) => VecShape::TwoD,
        _ => panic!("unsupported AArch64 vector shape: <{lanes} x {elem}> (only 128-bit .4s/.2d)"),
    }
}

/// `fma(acc, lhs, rhs)`: `acc` is a read-write accumulator, so copy it into
/// the result register before the fused multiply-add writes it.
/// `fma(acc, lhs, rhs)`: `fmla` is read-modify-write on the accumulator; the
/// `VecFmla` MInst takes the accumulator as an explicit SSA read and emits the
/// leading copy itself.
pub(super) fn lower_fma(
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

pub(super) fn lower_vector_splat(
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

pub(super) fn lower_vector_extract_element(
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
    ctx.emit(MInst::VecExtractLane {
        size,
        dst,
        src,
        lane,
    });
    LoweredOutput::Value(result)
}

pub(super) fn lower_vector_insert_element(
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

pub(super) fn lower_vector_reduce(
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

pub(super) fn lane_constant(arena: ArenaContext<'_>, index: HirInst) -> u8 {
    match arena.inst_data(index).kind() {
        InstKind::Integer(value) => value.value() as u8,
        _ => unreachable!("vector lane index must be a constant integer"),
    }
}
