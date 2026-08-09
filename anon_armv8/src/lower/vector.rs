//! NEON vector lowering helpers.

use taki_mir::types::I32;

use super::arith::{integer_constant, operand_size, signed_power_of_two};
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
    // NEON has no integer vector divide (`sdiv` is scalar-only); integer
    // Div/Rem with a constant splat divisor is rewritten to a multiply-high
    // magic sequence instead. The constant splat is rematerialized, so it is
    // read before either operand is materialized.
    if matches!(binary.op(), BinaryOp::Div | BinaryOp::Rem) && !is_float {
        let Some(divisor) = splat_constant_i32(arena, binary.rhs()) else {
            ctx.lowering_panic(
                "AArch64 instruction selection",
                "vector integer divide/remainder requires a constant splat divisor",
                Some(arena.inst_data(binary.lhs()).ty()),
                Some(arena.inst_data(inst).ty()),
            );
        };
        let lhs = ctx.put_value_in_reg(binary.lhs());
        if lower_vector_constant_div_rem(ctx, binary.op(), dst, lhs, divisor, shape) {
            return LoweredOutput::Value(result);
        }
        ctx.lowering_panic(
            "AArch64 instruction selection",
            "unsupported vector divide/remainder shape",
            Some(arena.inst_data(binary.lhs()).ty()),
            Some(arena.inst_data(inst).ty()),
        );
    }
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
                // Float vectors need the `f*` NEON forms (`add v.4s` is an
                // integer add on the float bit patterns).
                op: match (binary.op(), is_float) {
                    (BinaryOp::Add, false) => VecArithOp::Add,
                    (BinaryOp::Sub, false) => VecArithOp::Sub,
                    (BinaryOp::Mul, false) => VecArithOp::Mul,
                    (BinaryOp::Add, true) => VecArithOp::Fadd,
                    (BinaryOp::Sub, true) => VecArithOp::Fsub,
                    _ => VecArithOp::Fmul,
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
        BinaryOp::Eq
        | BinaryOp::Gt
        | BinaryOp::Lt
        | BinaryOp::Le
        | BinaryOp::Ge
        | BinaryOp::NotEq => {
            if is_float {
                ctx.lowering_panic(
                    "AArch64 instruction selection",
                    "floating vector comparisons (fcmgt) are not in the NEON MInst set",
                    Some(arena.inst_data(binary.lhs()).ty()),
                    Some(arena.inst_data(inst).ty()),
                );
            }
            // NEON predicates missing from the MInst set decompose onto
            // `cmgt`/`cmeq` plus `mvn`:
            //   a <  b  ==  b >  a
            //   a <= b  ==  !(a >  b)
            //   a >= b  ==  !(a <  b)  ==  !(b >  a)
            //   a != b  ==  !(a == b)
            let (base_op, swap, invert) = match binary.op() {
                BinaryOp::Gt => (VecCmpOp::Gt, false, false),
                BinaryOp::Lt => (VecCmpOp::Gt, true, false),
                BinaryOp::Le => (VecCmpOp::Gt, false, true),
                BinaryOp::Ge => (VecCmpOp::Gt, true, true),
                BinaryOp::NotEq => (VecCmpOp::Eq, false, true),
                _ => (VecCmpOp::Eq, false, false),
            };
            let (clhs, crhs) = if swap { (rhs, lhs) } else { (lhs, rhs) };
            if invert {
                let cmp = ctx.alloc_tmp(arena.inst_data(inst).ty().clone());
                ctx.emit(MInst::VecCmp {
                    op: base_op,
                    shape,
                    dst: Writable::from_reg(cmp),
                    lhs: clhs,
                    rhs: crhs,
                });
                ctx.emit(MInst::VecBitwiseNot { dst, src: cmp });
            } else {
                ctx.emit(MInst::VecCmp {
                    op: base_op,
                    shape,
                    dst,
                    lhs: clhs,
                    rhs: crhs,
                });
            }
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
        BinaryOp::Shl | BinaryOp::Shr | BinaryOp::Sar => {
            if is_float {
                ctx.lowering_panic(
                    "AArch64 instruction selection",
                    "vector shifts have no float form (the loop vectorizer rejects float shifts)",
                    Some(arena.inst_data(binary.lhs()).ty()),
                    Some(arena.inst_data(inst).ty()),
                );
            }
            let shift_op = match binary.op() {
                BinaryOp::Shl => VecShiftOp::Shl,
                BinaryOp::Shr => VecShiftOp::Shr,
                _ => VecShiftOp::Sar,
            };
            // A constant amount (a splatted integer literal) uses the
            // immediate forms shl/ushr/sshr; anything else uses the
            // register forms sshl/ushl.
            match constant_shift_amount(arena, binary.rhs(), shape) {
                Some(imm) => ctx.emit(MInst::VecShift {
                    op: shift_op,
                    shape,
                    dst,
                    lhs,
                    rhs,
                    imm: Some(imm),
                }),
                None => {
                    // Variable right shifts have no direct NEON register
                    // form: sshl/ushl with a negative amount shift right, so
                    // negate the amount vector first.
                    if binary.op() == BinaryOp::Shl {
                        ctx.emit(MInst::VecShift {
                            op: shift_op,
                            shape,
                            dst,
                            lhs,
                            rhs,
                            imm: None,
                        });
                    } else {
                        let neg = ctx.alloc_tmp(arena.inst_data(binary.lhs()).ty().clone());
                        ctx.emit(MInst::VecNeg {
                            shape,
                            dst: Writable::from_reg(neg),
                            src: rhs,
                        });
                        ctx.emit(MInst::VecShift {
                            op: shift_op,
                            shape,
                            dst,
                            lhs,
                            rhs: neg,
                            imm: None,
                        });
                    }
                }
            }
        }
        BinaryOp::Div => {
            // Only f32 reaches here (integer Div/Rem is handled above via the
            // multiply-high magic).
            ctx.emit(MInst::VecDiv {
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

fn constant_shift_amount(arena: ArenaContext<'_>, rhs: HirInst, shape: VecShape) -> Option<u8> {
    let InstKind::VectorSplat(splat) = arena.inst_data(rhs).kind() else {
        return None;
    };
    let InstKind::Integer(value) = arena.inst_data(splat.src()).kind() else {
        return None;
    };
    let lane_bits = u8::from(shape.element_bytes()) * 8;
    u8::try_from(value.value()).ok().filter(|imm| *imm < lane_bits)
}

/// The `i32` divisor of a `VectorSplat` of a constant integer (vector Div/Rem
/// needs the whole word; divisors may be negative).
fn splat_constant_i32(arena: ArenaContext<'_>, inst: HirInst) -> Option<i32> {
    match arena.inst_data(inst).kind() {
        InstKind::VectorSplat(splat) => integer_constant(arena, splat.src()),
        _ => None,
    }
}

/// Vector signed division/remainder by a constant `<4 x i32>` divisor.
///
/// Only `.4s` lanes are supported (the magic sequence widens each pair of
/// 32-bit lanes to 64 bits). The multiplier/correction sequence mirrors
/// [`lower_signed_div_rem_magic`] but per-lane:
/// `smull/smull2 (32x32->64)` both halves, arithmetic-shift the 64-bit lanes,
/// narrow back with `xtn/xtn2`, add the sign bit, and (for remainder) `mls`
/// the divisor back off.
fn lower_vector_constant_div_rem(
    ctx: &mut LowerContext<'_, MInst>,
    op: BinaryOp,
    dst: Writable<taki_mir::register::Reg>,
    lhs: taki_mir::register::Reg,
    divisor: i32,
    shape: VecShape,
) -> bool {
    if shape != VecShape::FourS {
        return false;
    }
    let v4i32 = HirType::get_vector(HirType::get_i32(), 4);
    // Divisor of magnitude 1: identity / negation.
    if divisor == 1 {
        match op {
            BinaryOp::Div => ctx.emit(MInst::VecMov { dst, src: lhs }),
            BinaryOp::Rem => emit_zero_vector(ctx, dst, shape),
            _ => return false,
        }
        return true;
    }
    if divisor == -1 {
        match op {
            BinaryOp::Div => emit_neg_vector(ctx, dst, lhs, shape),
            BinaryOp::Rem => emit_zero_vector(ctx, dst, shape),
            _ => return false,
        }
        return true;
    }
    // Powers of two use the sign-bias shift sequence.
    if let Some((shift, negate)) = signed_power_of_two(divisor) {
        return lower_vector_signed_div_rem_power_of_two(ctx, op, dst, lhs, shift, negate, shape);
    }
    let Some(magic) = signed_magic_i32(divisor) else {
        return false;
    };
    // The widening multiply and its 64-bit-lane shifts use full 128-bit
    // vector registers; a `<4 x i32>` temp allocates the same register class
    // (the lane interpretation is an instruction-level detail).
    let m = materialize_splat_i32(ctx, magic.multiplier, shape);
    // product = x * multiplier, both halves widened to 64-bit lanes.
    let prod_lo = ctx.alloc_tmp(v4i32.clone());
    let prod_hi = ctx.alloc_tmp(v4i32.clone());
    ctx.emit(MInst::VecSMull {
        high: false,
        dst: Writable::from_reg(prod_lo),
        lhs,
        rhs: m,
    });
    ctx.emit(MInst::VecSMull {
        high: true,
        dst: Writable::from_reg(prod_hi),
        lhs,
        rhs: m,
    });

    // high = product >> 32 (the multiply-high word), narrowed to 4s.
    let high_lo = ctx.alloc_tmp(v4i32.clone());
    let high_hi = ctx.alloc_tmp(v4i32.clone());
    ctx.emit(MInst::VecShift {
        op: VecShiftOp::Sar,
        shape: VecShape::TwoD,
        dst: Writable::from_reg(high_lo),
        lhs: prod_lo,
        rhs: prod_lo,
        imm: Some(32),
    });
    ctx.emit(MInst::VecShift {
        op: VecShiftOp::Sar,
        shape: VecShape::TwoD,
        dst: Writable::from_reg(high_hi),
        lhs: prod_hi,
        rhs: prod_hi,
        imm: Some(32),
    });
    let high = ctx.alloc_tmp(v4i32.clone());
    ctx.emit(MInst::VecNarrow {
        high: false,
        dst: Writable::from_reg(high),
        acc: high,
        src: high_lo,
    });
    let high_result = ctx.alloc_tmp(v4i32.clone());
    ctx.emit(MInst::VecNarrow {
        high: true,
        dst: Writable::from_reg(high_result),
        acc: high,
        src: high_hi,
    });

    // corrected = high (+/-) numerator (identity for `MagicCorrection::None`).
    // `high_result` carries the full narrow (both `xtn` halves); `high` is
    // only the partial result.
    let corrected = match magic.correction {
        MagicCorrection::None => high_result,
        MagicCorrection::AddNumerator | MagicCorrection::SubNumerator => {
            let tmp = ctx.alloc_tmp(v4i32.clone());
            ctx.emit(MInst::VecArithRRR {
                op: if magic.correction == MagicCorrection::AddNumerator {
                    VecArithOp::Add
                } else {
                    VecArithOp::Sub
                },
                shape,
                dst: Writable::from_reg(tmp),
                lhs: high_result,
                rhs: lhs,
            });
            tmp
        }
    };

    // shifted = corrected >> shift (the multiplier shift, 0..=31).
    let shifted = if magic.shift == 0 {
        corrected
    } else {
        let tmp = ctx.alloc_tmp(v4i32.clone());
        ctx.emit(MInst::VecShift {
            op: VecShiftOp::Sar,
            shape,
            dst: Writable::from_reg(tmp),
            lhs: corrected,
            rhs: corrected,
            imm: Some(magic.shift),
        });
        tmp
    };

    // q = shifted + (shifted as u32 >> 31): round the negative quotient
    // toward zero.
    let sign = ctx.alloc_tmp(v4i32.clone());
    ctx.emit(MInst::VecShift {
        op: VecShiftOp::Shr,
        shape,
        dst: Writable::from_reg(sign),
        lhs: shifted,
        rhs: shifted,
        imm: Some(31),
    });
    let quotient = if op == BinaryOp::Rem {
        ctx.alloc_tmp(v4i32.clone())
    } else {
        dst.to_reg()
    };
    ctx.emit(MInst::VecArithRRR {
        op: VecArithOp::Add,
        shape,
        dst: Writable::from_reg(quotient),
        lhs: shifted,
        rhs: sign,
    });

    if op == BinaryOp::Rem {
        let d = materialize_splat_i32(ctx, divisor, shape);
        // rem = x - q * divisor  (`mls vd, vq, vd` computes vd -= vq * vd).
        ctx.emit(MInst::VecMla {
            op: VecMlaOp::Mls,
            shape,
            dst,
            acc: lhs,
            lhs: quotient,
            rhs: d,
        });
    }
    true
}

/// Vector signed division/remainder by `(+/-) 2^shift` (`<4 x i32>` lanes).
fn lower_vector_signed_div_rem_power_of_two(
    ctx: &mut LowerContext<'_, MInst>,
    op: BinaryOp,
    dst: Writable<taki_mir::register::Reg>,
    lhs: taki_mir::register::Reg,
    shift: u8,
    negate_quotient: bool,
    shape: VecShape,
) -> bool {
    if shape != VecShape::FourS {
        return false;
    }
    let v4i32 = HirType::get_vector(HirType::get_i32(), 4);
    if shift == 0 {
        // Divisor of magnitude 1 handled by the caller.
        return false;
    }
    let sign = ctx.alloc_tmp(v4i32.clone());
    ctx.emit(MInst::VecShift {
        op: VecShiftOp::Sar,
        shape,
        dst: Writable::from_reg(sign),
        lhs,
        rhs: lhs,
        imm: Some(31),
    });
    let biased = ctx.alloc_tmp(v4i32.clone());
    ctx.emit(MInst::VecShift {
        op: VecShiftOp::Shr,
        shape,
        dst: Writable::from_reg(biased),
        lhs: sign,
        rhs: sign,
        imm: Some(32 - shift),
    });
    let biased = {
        let tmp = ctx.alloc_tmp(v4i32.clone());
        ctx.emit(MInst::VecArithRRR {
            op: VecArithOp::Add,
            shape,
            dst: Writable::from_reg(tmp),
            lhs,
            rhs: biased,
        });
        tmp
    };
    let quotient = if op == BinaryOp::Rem || negate_quotient {
        ctx.alloc_tmp(v4i32.clone())
    } else {
        dst.to_reg()
    };
    ctx.emit(MInst::VecShift {
        op: VecShiftOp::Sar,
        shape,
        dst: Writable::from_reg(quotient),
        lhs: biased,
        rhs: biased,
        imm: Some(shift),
    });
    if negate_quotient && op == BinaryOp::Div {
        emit_neg_vector(ctx, dst, quotient, shape);
    }
    if op == BinaryOp::Rem {
        let d = materialize_splat_i32(ctx, (1_i32) << shift, shape);
        // rem = x - q * 2^shift.
        ctx.emit(MInst::VecMla {
            op: VecMlaOp::Mls,
            shape,
            dst,
            acc: lhs,
            lhs: quotient,
            rhs: d,
        });
    }
    true
}

/// Materialize an `i32` constant into a scalar GPR and `dup` it across `.4s`.
fn materialize_splat_i32(
    ctx: &mut LowerContext<'_, MInst>,
    value: i32,
    shape: VecShape,
) -> taki_mir::register::Reg {
    let scalar = ctx.alloc_tmp(HirType::get_i32());
    ctx.emit(MInst::LoadImm {
        size: OperandSize::Size32,
        dst: Writable::from_reg(scalar),
        value: u64::from(value as u32),
    });
    let vec = ctx.alloc_tmp(HirType::get_vector(HirType::get_i32(), 4));
    ctx.emit(MInst::VecDup {
        shape,
        dst: Writable::from_reg(vec),
        src: scalar,
    });
    vec
}

/// `dst = 0 - src` (vector negation).
fn emit_neg_vector(
    ctx: &mut LowerContext<'_, MInst>,
    dst: Writable<taki_mir::register::Reg>,
    src: taki_mir::register::Reg,
    shape: VecShape,
) {
    let zero = ctx.alloc_tmp(HirType::get_vector(HirType::get_i32(), 4));
    ctx.emit(MInst::VecMovImm {
        shape,
        dst: Writable::from_reg(zero),
        imm: 0,
        shift: 0,
    });
    ctx.emit(MInst::VecArithRRR {
        op: VecArithOp::Sub,
        shape,
        dst,
        lhs: zero,
        rhs: src,
    });
}

/// `dst = 0` vector.
fn emit_zero_vector(
    ctx: &mut LowerContext<'_, MInst>,
    dst: Writable<taki_mir::register::Reg>,
    shape: VecShape,
) {
    ctx.emit(MInst::VecMovImm {
        shape,
        dst,
        imm: 0,
        shift: 0,
    });
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
    // A constant splat source is materialized through a GPR (`dup vd.4s,
    // wn`), not a float scalar register: routing the constant through
    // `fmov sN` + `dup vd.4s, vN.s[0]` lets the `fmov` clobber a
    // concurrently live vector value that shares the aliased `vN` (on
    // AArch64 `sN` is the low 32 bits of `vN`) — the h-10 trsm
    // vectorization miscompile.
    let src = match arena.inst_data(splat.src()).kind() {
        InstKind::Float(f) => {
            let tmp = ctx.alloc_tmp(HirType::get_i32());
            ctx.emit(<AArch64Abi as ABIMachineSpec>::gen_load_imm(
                Writable::from_reg(tmp),
                u64::from(f.value().to_bits()),
                I32,
            ));
            tmp
        }
        _ => ctx.put_value_in_reg(splat.src()),
    };
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
