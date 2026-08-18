//! Integer, pointer, and floating-point arithmetic lowering helpers.

use super::vector::{lower_vector_binary, vector_shape};
use super::*;
pub use log::{debug, error, info, trace, warn};
pub(super) fn lower_binary(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    binary: &Binary,
) -> LoweredOutput {
    let result = ctx.result_reg(inst);
    let dst = Writable::from_reg(result);
    match arena.inst_data(binary.lhs()).ty().kind() {
        TypeKind::Float32 => {
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
            LoweredOutput::Value(result)
        }
        TypeKind::Vector(_, _) => lower_vector_binary(ctx, arena, inst, binary),
        TypeKind::Unit
        | TypeKind::Int32
        | TypeKind::String
        | TypeKind::Pointer(_)
        | TypeKind::Function(_, _)
        | TypeKind::ArgList => {
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

            // 针对减去-1做优化
            if binary.op() == BinaryOp::Sub && integer_constant(arena, binary.lhs()) == Some(-1) {
                let rhs = ctx.put_value_in_reg(binary.rhs());
                ctx.emit(MInst::AluRRR {
                    op: AluOp::Orn,
                    size,
                    dst,
                    lhs: RegOrZr::Zr,
                    rhs: RegOrZr::Reg(rhs),
                });
                return LoweredOutput::Value(result);
            }
            let lhs = ctx.put_value_in_reg(binary.lhs());
            let rhs_imm = integer_constant(arena, binary.rhs());

            // 除以1短路处理
            if binary.op() == BinaryOp::Div && rhs_imm == Some(1) {
                return LoweredOutput::Value(lhs);
            }

            // 整数快速除法取余数
            if matches!(arena.inst_data(inst).ty().kind(), TypeKind::Int32)
                && matches!(binary.op(), BinaryOp::Div | BinaryOp::Rem)
                && rhs_imm.is_some_and(|divisor| {
                    lower_signed_div_rem_power_of_two(ctx, binary.op(), dst, lhs, divisor)
                        || lower_signed_div_rem_magic(ctx, binary.op(), dst, lhs, divisor)
                })
            {
                return LoweredOutput::Value(result);
            }

            // 正常处理运算符
            match binary.op() {
                BinaryOp::Add | BinaryOp::Sub => {
                    if binary.op() == BinaryOp::Add
                        && integer_constant(arena, binary.lhs()) == Some(0)
                    {
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
                    } else if matches!(binary.op(), BinaryOp::Or | BinaryOp::Xor)
                        && lhs_imm == Some(0)
                    {
                        let rhs = ctx.put_value_in_reg(binary.rhs());
                        ctx.emit(MInst::Mov {
                            size,
                            dst,
                            src: rhs,
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
                BinaryOp::MatMul => {
                    ctx.lowering_panic(
                        "AArch64 instruction selection",
                        format!(
                            "scalar {:?} is unsupported; min/max is tensor-only",
                            binary.op()
                        ),
                        Some(arena.inst_data(binary.lhs()).ty()),
                        Some(arena.inst_data(inst).ty()),
                    );
                }
            }
            LoweredOutput::Value(result)
        }
        TypeKind::Array(ty, size) => {
            debug!("Lowering Array Binary Ops");
            let size = operand_size(arena.inst_data(binary.lhs()).ty().kind());
            let lhs = ctx.put_value_in_reg(binary.lhs());
            match binary.op() {
                BinaryOp::Add | BinaryOp::Sub => {
                    let rhs = ctx.put_value_in_reg(binary.rhs());
                    ctx.emit(MInst::AluRRR {
                        op: alu_op(binary.op()),
                        size,
                        dst,
                        lhs: RegOrZr::Reg(lhs),
                        rhs: RegOrZr::Reg(rhs),
                    });
                }
                BinaryOp::And | BinaryOp::Or | BinaryOp::Xor => {
                    let rhs = ctx.put_value_in_reg(binary.rhs());
                    ctx.emit(MInst::AluRRR {
                        op: alu_op(binary.op()),
                        size,
                        dst,
                        lhs: RegOrZr::Reg(lhs),
                        rhs: RegOrZr::Reg(rhs),
                    });
                }
                BinaryOp::Shl | BinaryOp::Shr | BinaryOp::Sar => {
                    let rhs = ctx.put_value_in_reg(binary.rhs());
                    ctx.emit(MInst::AluRRR {
                        op: alu_op(binary.op()),
                        size,
                        dst,
                        lhs: RegOrZr::Reg(lhs),
                        rhs: RegOrZr::Reg(rhs),
                    });
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
                    let rhs = ctx.put_value_in_reg(binary.rhs());
                    ctx.emit(MInst::CmpRR {
                        size,
                        lhs,
                        rhs: RegOrZr::Reg(rhs),
                    });
                    ctx.emit(MInst::CSet {
                        cond: comparison_cond(binary.op()),
                        dst,
                    });
                }
                BinaryOp::MatMul => todo!(),
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
    }
}

pub(super) fn lower_cast(
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
        // Pointer-to-pointer is a no-op bitcast at the register level. This is
        // the channel that turns `*i32` into `*<4 x i32>` for vector loads and
        // stores: `get_elem_ptr` and `load`/`store` still address scalars, and
        // the vectorizing pass bitcasts the pointer to retype the pointee.
        (TypeKind::Pointer(..), TypeKind::Pointer(..)) => ctx.emit(MInst::Mov {
            size: OperandSize::Size64,
            dst,
            src: src_reg,
        }),
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

pub(super) fn integer_constant(arena: ArenaContext<'_>, inst: HirInst) -> Option<i32> {
    match arena.inst_data(inst).kind() {
        InstKind::Integer(value) => Some(value.value()),
        _ => None,
    }
}

pub(super) fn signed_power_of_two(value: i32) -> Option<(u8, bool)> {
    if value == 0 {
        return None;
    }
    let magnitude = value.unsigned_abs();
    magnitude
        .is_power_of_two()
        .then(|| (magnitude.trailing_zeros() as u8, value.is_negative()))
}

pub(super) fn lower_signed_div_rem_power_of_two(
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
pub(super) fn lower_signed_div_rem_magic(
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

    let divisor_value = i64::from(divisor as u32);
    let multiplier = match ctx.loop_const_to_reg(i64::from(magic.multiplier as u32), divisor_value)
    {
        Some(reg) => reg,
        None => match ctx.const_to_reg(i64::from(magic.multiplier as u32), divisor_value) {
            Some(reg) => reg,
            None => {
                let reg = ctx.alloc_tmp(HirType::get_i32());
                ctx.emit(MInst::LoadImm {
                    size,
                    dst: Writable::from_reg(reg),
                    value: u64::from(magic.multiplier as u32),
                });
                reg
            }
        },
    };
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
        let divisor_reg = match ctx.loop_const_to_reg(divisor_value, divisor_value) {
            Some(reg) => reg,
            None => match ctx.const_to_reg(divisor_value, divisor_value) {
                Some(reg) => reg,
                None => {
                    let reg = ctx.alloc_tmp(HirType::get_i32());
                    ctx.emit(MInst::LoadImm {
                        size,
                        dst: Writable::from_reg(reg),
                        value: u64::from(divisor as u32),
                    });
                    reg
                }
            },
        };
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
pub(super) fn try_fold_mul_constant(
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
pub(super) enum MulConstForm {
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
pub(super) fn fold_mul_constant(value: i32, size: OperandSize) -> Option<MulConstForm> {
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
pub(super) fn fold_mul_add_sub(
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
            size,
            FusionTypes {
                consumer,
                lhs,
                rhs,
                mul_inst,
                mul_lhs: mul.lhs(),
                mul_rhs: mul.rhs(),
            },
        )
        // Only fuse within one block. A multiplication hoisted by LICM to a
        // preheader (or any other dominator) executes less often than the
        // consuming add; fusing it back into a madd re-materializes the
        // product at the consumer's frequency and silently defeats the hoist
        // (conv2d inner loop regression). Checked before the sinking call:
        // `sink_pure_single_use_producer` has the side effect of claiming the
        // producer, which must not happen when the fusion is rejected.
        || arena.f().layout().parent_bb(mul_inst) != arena.f().layout().parent_bb(consumer)
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

/// The six instructions whose types must agree before a multiply-add can be
/// fused: the consuming add/sub and its operands, plus the multiplication and
/// its two operands.
struct FusionTypes {
    consumer: HirInst,
    lhs: HirInst,
    rhs: HirInst,
    mul_inst: HirInst,
    mul_lhs: HirInst,
    mul_rhs: HirInst,
}

pub(super) fn is_mul(arena: ArenaContext<'_>, inst: HirInst) -> bool {
    matches!(
        arena.inst_data(inst).kind(),
        InstKind::Binary(binary) if binary.op() == BinaryOp::Mul
    )
}

fn fusion_types_match(arena: ArenaContext<'_>, size: OperandSize, types: FusionTypes) -> bool {
    let FusionTypes {
        consumer,
        lhs,
        rhs,
        mul_inst,
        mul_lhs,
        mul_rhs,
    } = types;
    [consumer, lhs, rhs, mul_inst, mul_lhs, mul_rhs]
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
pub(super) fn fold_shifted_rhs(
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

pub(super) fn add_sub_immediate(op: BinaryOp, value: Option<i32>) -> Option<(AluOp, Imm12)> {
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

pub(super) fn positive_imm12(value: i32) -> Option<Imm12> {
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

pub(super) fn integer_bits(value: i32, size: OperandSize) -> u64 {
    match size {
        OperandSize::Size32 => value as u32 as u64,
        OperandSize::Size64 => value as i64 as u64,
    }
}

pub(super) fn operand_size(ty: &TypeKind) -> OperandSize {
    match ty {
        TypeKind::Int32 => OperandSize::Size32,
        TypeKind::Pointer(_) | TypeKind::String => OperandSize::Size64,
        ty => unreachable!("unsupported AArch64 scalar type: {ty:?}"),
    }
}

pub(super) fn alu_op(op: BinaryOp) -> AluOp {
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

pub(super) fn float_alu_op(op: BinaryOp) -> FpuOp {
    match op {
        BinaryOp::Add => FpuOp::Add,
        BinaryOp::Sub => FpuOp::Sub,
        BinaryOp::Mul => FpuOp::Mul,
        BinaryOp::Div => FpuOp::Div,
        _ => unreachable!("unsupported AArch64 floating binary operation: {op:?}"),
    }
}

pub(super) fn comparison_cond(op: BinaryOp) -> Cond {
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

pub(super) fn float_comparison_cond(op: BinaryOp) -> Cond {
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
