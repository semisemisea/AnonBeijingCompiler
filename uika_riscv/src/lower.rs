use raana_ir::ir::{
    Binary, BinaryOp, Call, Cast, GetElemPtr, InstKind, Load, Return, Select, Store, TailCall,
    Type as HirType, TypeKind as HirTypeKind,
    arena::Arena,
    inst_kind::{MemZero, MemZeroLen},
};
use smallvec::{SmallVec, smallvec};

use crate::{
    abi::{DEFAULT_CLOBBERS, Riscv64ABI},
    instructions::{
        AMode, AluRRImm12OP, AluRRImmShiftOP, AluRRROP, CondBrOp, FcvtMode, FpuRRROP, Imm12,
        LoadOP, MInst, ShiftImm, ShiftImm64, StoreOP,
    },
    labels::Label,
    regs::{a0, a1, a2, fa0, fp_reg, preg_name, stack_reg, zero_reg},
};

use taki_mir::{
    abi::{ABIMachineSpec, ArgSlot, CallArgPair, CallRetPair, RetPair, StackAMode},
    block_order::LoweredBlock,
    div_magic::{MagicCorrection, signed_magic_i32},
    libcall::LibCall,
    lower::{LowerBackend, LowerContext, LoweredOutput, analyze_gep, fold_gep_constant_offset},
    prelude::{ArenaContext, HirFunctionData, HirInst},
    reg_alloc::reg::PReg,
    register::{Reg, Writable},
    types::LoweredType,
};

const INLINE_MEMZERO_MAX_STORES: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectAluOps {
    sub: AluRRROP,
    xor: AluRRROP,
    and: AluRRROP,
}

fn select_alu_ops(ty: &HirType) -> Option<SelectAluOps> {
    match ty.kind() {
        HirTypeKind::Int32 | HirTypeKind::Float32 => Some(SelectAluOps {
            sub: AluRRROP::SubW,
            xor: AluRRROP::Xor,
            and: AluRRROP::And,
        }),
        HirTypeKind::Pointer(_) | HirTypeKind::String => Some(SelectAluOps {
            sub: AluRRROP::Sub,
            xor: AluRRROP::Xor,
            and: AluRRROP::And,
        }),
        _ => None,
    }
}

fn select_tmp_ty(ty: &HirType) -> HirType {
    match ty.kind() {
        HirTypeKind::Int32 | HirTypeKind::Float32 => HirType::get_i32(),
        HirTypeKind::Pointer(_) | HirTypeKind::String => HirType::get_pointer(HirType::get_i32()),
        _ => unreachable!("unsupported RISC-V select type: {ty:?}"),
    }
}

fn normalize_amode(amode: AMode, ctx: &mut LowerContext<'_, MInst>) -> AMode {
    // Slot offsets need the final outgoing-argument-area displacement, which
    // is unavailable during lowering. Keep them symbolic for ABI legalization.
    if matches!(amode, AMode::SlotOffset(_)) {
        return amode;
    }
    let (off, base) = match &amode {
        AMode::SPOffset(o) | AMode::OutgoingArg(o) => (*o, stack_reg()),
        AMode::FPOffset(o) | AMode::IncomingArg(o) => (*o, fp_reg()),
        _ => return amode,
    };
    if (-2048..2048).contains(&off) {
        return amode;
    }
    let tmp_off = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
    ctx.emit(MInst::LoadImm {
        rd: Writable::from_reg(tmp_off),
        value: off as u64,
    });
    let tmp_addr = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
    ctx.emit(MInst::AluRRR {
        op: AluRRROP::Add,
        rd: Writable::from_reg(tmp_addr),
        rs1: base,
        rs2: tmp_off,
    });
    AMode::RegOffest(tmp_addr, 0)
}

/// Fold a single-use constant GEP into a RISC-V addressing mode
/// (`off(base)`), falling back to `None` when the GEP has dynamic indices,
/// the offset does not fit the 12-bit signed immediate, or the GEP has other
/// users. Callers then materialize the address as before.
fn try_fold_gep_amode(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    gep: HirInst,
    consumer: HirInst,
) -> Option<AMode> {
    fold_gep_constant_offset(ctx, arena, gep, consumer, |off| {
        (-2048..2048).contains(&off)
    })
    .map(|(base, off)| AMode::RegOffest(base, off))
}

fn alu_op_for_hir_binary(op: BinaryOp, ty: &HirType) -> AluRRROP {
    let is_i32 = matches!(ty.kind(), HirTypeKind::Int32);
    match (op, is_i32) {
        (BinaryOp::Add, true) => AluRRROP::AddW,
        (BinaryOp::Sub, true) => AluRRROP::SubW,
        (BinaryOp::Mul, true) => AluRRROP::MulW,
        (BinaryOp::Div, true) => AluRRROP::DivW,
        (BinaryOp::Rem, true) => AluRRROP::RemW,
        (BinaryOp::Shl, true) => AluRRROP::ShlW,
        (BinaryOp::Shr, true) => AluRRROP::ShrW,
        (BinaryOp::Sar, true) => AluRRROP::SarW,
        (BinaryOp::Add, false) => AluRRROP::Add,
        (BinaryOp::Sub, false) => AluRRROP::Sub,
        (BinaryOp::Mul, false) => AluRRROP::Mul,
        (BinaryOp::Div, false) => AluRRROP::Div,
        (BinaryOp::Rem, false) => AluRRROP::Rem,
        (BinaryOp::Shl, false) => AluRRROP::Shl,
        (BinaryOp::Shr, false) => AluRRROP::Shr,
        (BinaryOp::Sar, false) => AluRRROP::Sar,
        (BinaryOp::And, _) => AluRRROP::And,
        (BinaryOp::Or, _) => AluRRROP::Or,
        (BinaryOp::Xor, _) => AluRRROP::Xor,
        (BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge, _) => AluRRROP::Slt,
        (BinaryOp::Eq | BinaryOp::NotEq, _) => {
            unreachable!("eq/ne lower through sub + seqz/snez")
        }
        (BinaryOp::Min | BinaryOp::Max, _) => {
            unreachable!("vector min/max requires a vector-capable backend")
        }
    }
}

/// Whether `inst`'s 64-bit register representation is guaranteed to be a
/// sign-extended i32 on RISC-V.
///
/// Sources whose results are sign-extended: integer constants (remat goes
/// through `gen_load_imm`, which sign-extends 32-bit values, abi.rs), loads
/// (`lw` sign-extends), i32 arithmetic/shift binaries (lowered to the `w`
/// variants), and float->int casts (`fcvt.w.s`). Function arguments, phis,
/// selects (built from 64-bit Xor), and unknown sources are not guaranteed;
/// callers must keep the explicit extension.
fn is_sign_extended_i32(arena: ArenaContext<'_>, inst: HirInst) -> bool {
    let is_i32 = matches!(arena.inst_data(inst).ty().kind(), HirTypeKind::Int32);
    match arena.inst_data(inst).kind() {
        InstKind::Integer(_) => true,
        InstKind::Load(_) => true,
        InstKind::Binary(binary) => {
            is_i32
                && matches!(
                    binary.op(),
                    BinaryOp::Add
                        | BinaryOp::Sub
                        | BinaryOp::Mul
                        | BinaryOp::Div
                        | BinaryOp::Rem
                        | BinaryOp::Shl
                        | BinaryOp::Shr
                        | BinaryOp::Sar
                )
        }
        InstKind::Cast(_) => is_i32,
        _ => false,
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
    rd: Writable<Reg>,
    lhs: Reg,
    divisor: i32,
) -> bool {
    let Some((shift, negate_quotient)) = signed_power_of_two(divisor) else {
        return false;
    };

    if shift == 0 {
        match op {
            BinaryOp::Div if negate_quotient => ctx.emit(MInst::AluRRR {
                op: AluRRROP::SubW,
                rd,
                rs1: zero_reg(),
                rs2: lhs,
            }),
            BinaryOp::Div => ctx.emit(MInst::Mov { src: lhs, dst: rd }),
            BinaryOp::Rem => ctx.emit(MInst::LoadImm { rd, value: 0 }),
            _ => return false,
        }
        return true;
    }

    let sign = ctx.alloc_tmp(HirType::get_i32());
    ctx.emit(MInst::AluRRImmShift {
        op: AluRRImmShiftOP::SraiW,
        rd: Writable::from_reg(sign),
        rs: lhs,
        shamt: ShiftImm::new(31).unwrap(),
    });
    let bias = ctx.alloc_tmp(HirType::get_i32());
    ctx.emit(MInst::AluRRImmShift {
        op: AluRRImmShiftOP::SrliW,
        rd: Writable::from_reg(bias),
        rs: sign,
        shamt: ShiftImm::new(32 - shift).unwrap(),
    });
    let biased = ctx.alloc_tmp(HirType::get_i32());
    ctx.emit(MInst::AluRRR {
        op: AluRRROP::AddW,
        rd: Writable::from_reg(biased),
        rs1: lhs,
        rs2: bias,
    });

    let quotient = if op == BinaryOp::Rem || negate_quotient {
        ctx.alloc_tmp(HirType::get_i32())
    } else {
        rd.to_reg()
    };
    ctx.emit(MInst::AluRRImmShift {
        op: AluRRImmShiftOP::SraiW,
        rd: Writable::from_reg(quotient),
        rs: biased,
        shamt: ShiftImm::new(shift).unwrap(),
    });

    match op {
        BinaryOp::Div if negate_quotient => ctx.emit(MInst::AluRRR {
            op: AluRRROP::SubW,
            rd,
            rs1: zero_reg(),
            rs2: quotient,
        }),
        BinaryOp::Div => {}
        BinaryOp::Rem => {
            let scaled = ctx.alloc_tmp(HirType::get_i32());
            ctx.emit(MInst::AluRRImmShift {
                op: AluRRImmShiftOP::SlliW,
                rd: Writable::from_reg(scaled),
                rs: quotient,
                shamt: ShiftImm::new(shift).unwrap(),
            });
            ctx.emit(MInst::AluRRR {
                op: AluRRROP::SubW,
                rd,
                rs1: lhs,
                rs2: scaled,
            });
        }
        _ => return false,
    }
    true
}

/// Selects a signed division or remainder by a constant that is neither a
/// power of two nor `0`, `1` or `-1`, replacing `divw`/`remw` with the
/// multiply-high sequence of [`signed_magic_i32`].
///
/// The backend keeps `i32` values sign-extended in 64-bit registers, so a
/// plain `mul` yields the exact 64-bit product and `srai` reaches its high
/// half. When the multiplier needs no correction term, that same `srai`
/// absorbs the magic-number shift.
fn lower_signed_div_rem_magic(
    ctx: &mut LowerContext<'_, MInst>,
    op: BinaryOp,
    rd: Writable<Reg>,
    lhs: Reg,
    divisor: i32,
) -> bool {
    if !matches!(op, BinaryOp::Div | BinaryOp::Rem) {
        return false;
    }
    let Some(magic) = signed_magic_i32(divisor) else {
        return false;
    };

    let multiplier = ctx.alloc_tmp(HirType::get_i32());
    ctx.emit(MInst::LoadImm {
        rd: Writable::from_reg(multiplier),
        value: i64::from(magic.multiplier) as u64,
    });
    let product = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
    ctx.emit(MInst::AluRRR {
        op: AluRRROP::Mul,
        rd: Writable::from_reg(product),
        rs1: lhs,
        rs2: multiplier,
    });

    // Every value below the product fits in 32 bits, so the temporaries stay
    // `i32` even where an instruction writes the whole 64-bit register.
    let shifted = ctx.alloc_tmp(HirType::get_i32());
    match magic.correction {
        MagicCorrection::None => ctx.emit(MInst::Srai {
            rd: Writable::from_reg(shifted),
            rs: product,
            shamt: ShiftImm64::new(32 + magic.shift).unwrap(),
        }),
        correction => {
            let high = ctx.alloc_tmp(HirType::get_i32());
            ctx.emit(MInst::Srai {
                rd: Writable::from_reg(high),
                rs: product,
                shamt: ShiftImm64::new(32).unwrap(),
            });
            let corrected = if magic.shift == 0 {
                shifted
            } else {
                ctx.alloc_tmp(HirType::get_i32())
            };
            ctx.emit(MInst::AluRRR {
                op: if correction == MagicCorrection::AddNumerator {
                    AluRRROP::AddW
                } else {
                    AluRRROP::SubW
                },
                rd: Writable::from_reg(corrected),
                rs1: high,
                rs2: lhs,
            });
            if magic.shift != 0 {
                ctx.emit(MInst::AluRRImmShift {
                    op: AluRRImmShiftOP::SraiW,
                    rd: Writable::from_reg(shifted),
                    rs: corrected,
                    shamt: ShiftImm::new(magic.shift).unwrap(),
                });
            }
        }
    }

    // The shifts round toward negative infinity; adding the sign bit turns
    // that into the truncating quotient SysY requires.
    let sign = ctx.alloc_tmp(HirType::get_i32());
    ctx.emit(MInst::AluRRImmShift {
        op: AluRRImmShiftOP::SrliW,
        rd: Writable::from_reg(sign),
        rs: shifted,
        shamt: ShiftImm::new(31).unwrap(),
    });
    let quotient = if op == BinaryOp::Rem {
        ctx.alloc_tmp(HirType::get_i32())
    } else {
        rd.to_reg()
    };
    ctx.emit(MInst::AluRRR {
        op: AluRRROP::AddW,
        rd: Writable::from_reg(quotient),
        rs1: shifted,
        rs2: sign,
    });

    if op == BinaryOp::Rem {
        let divisor_reg = ctx.alloc_tmp(HirType::get_i32());
        ctx.emit(MInst::LoadImm {
            rd: Writable::from_reg(divisor_reg),
            value: i64::from(divisor) as u64,
        });
        let scaled = ctx.alloc_tmp(HirType::get_i32());
        ctx.emit(MInst::AluRRR {
            op: AluRRROP::MulW,
            rd: Writable::from_reg(scaled),
            rs1: quotient,
            rs2: divisor_reg,
        });
        ctx.emit(MInst::AluRRR {
            op: AluRRROP::SubW,
            rd,
            rs1: lhs,
            rs2: scaled,
        });
    }
    true
}

/// `xori rd, rs, 1`: the boolean flip behind `>=`/`<=`/`!=`.
fn emit_xori_one(ctx: &mut LowerContext<'_, MInst>, rd: Writable<Reg>, rs: Reg) {
    ctx.emit(MInst::AluRRImm12 {
        op: AluRRImm12OP::Xori,
        rd,
        rs,
        imm: Imm12::ONE,
    });
}

/// Emit a comparison through `emit_less`, writing `rd` directly, or into a
/// temporary flipped with `xori rd, less, 1` when `invert`.
fn lower_invertible_cmp(
    ctx: &mut LowerContext<'_, MInst>,
    rd: Writable<Reg>,
    invert: bool,
    emit_less: impl FnOnce(&mut LowerContext<'_, MInst>, Writable<Reg>),
) -> LoweredOutput {
    if !invert {
        emit_less(ctx, rd);
        return LoweredOutput::Value(rd.to_reg());
    }
    let less = ctx.alloc_tmp(HirType::get_i32());
    emit_less(ctx, Writable::from_reg(less));
    emit_xori_one(ctx, rd, less);
    LoweredOutput::Value(rd.to_reg())
}

/// `slt rd, rs1, rs2`, flipping the result with `xori 1` when `invert`.
fn lower_slt(
    ctx: &mut LowerContext<'_, MInst>,
    rd: Writable<Reg>,
    rs1: Reg,
    rs2: Reg,
    invert: bool,
) -> LoweredOutput {
    lower_invertible_cmp(ctx, rd, invert, |ctx, less| {
        ctx.emit(MInst::AluRRR {
            op: AluRRROP::Slt,
            rd: less,
            rs1,
            rs2,
        });
    })
}

/// `slti rd, rs, imm`, flipping the result with `xori 1` when `invert`.
fn lower_slti(
    ctx: &mut LowerContext<'_, MInst>,
    rd: Writable<Reg>,
    rs: Reg,
    imm: Imm12,
    invert: bool,
) -> LoweredOutput {
    lower_invertible_cmp(ctx, rd, invert, |ctx, less| {
        ctx.emit(MInst::AluRRImm12 {
            op: AluRRImm12OP::Slti,
            rd: less,
            rs,
            imm,
        });
    })
}

/// `seqz`/`snez rd, rs`: an equality test against zero.
fn lower_eq_zero(
    ctx: &mut LowerContext<'_, MInst>,
    rd: Writable<Reg>,
    equal: bool,
    rs: Reg,
) -> LoweredOutput {
    ctx.emit(MInst::AluRRR {
        op: if equal {
            AluRRROP::Seqz
        } else {
            AluRRROP::Snez
        },
        rd,
        rs1: rs,
        rs2: zero_reg(),
    });
    LoweredOutput::Value(rd.to_reg())
}

/// Fold `x * value` into a shift plus an optional add/sub when the
/// multiplier decomposes as 2^n, 2^n+1, or 2^n-1. Returns `None` otherwise;
/// the caller falls back to `mul`. Only positive multipliers are folded; a
/// negative one would need a `neg` that rarely beats `li` + `mul`.
///
/// x * 2^n     ->  slli(x, n)
/// x * (2^n+1) ->  slli(x, n) + addw
/// x * (2^n-1) ->  slli(x, n) - subw
fn fold_mul_constant_riscv(
    ctx: &mut LowerContext<'_, MInst>,
    rd: Writable<Reg>,
    x: Reg,
    value: i32,
) -> Option<()> {
    let pow2_shift =
        |v: u32| -> Option<ShiftImm> { ShiftImm::new(u8::try_from(v.trailing_zeros()).ok()?) };
    if value > 1 && (value as u32).is_power_of_two() {
        ctx.emit(MInst::AluRRImmShift {
            op: AluRRImmShiftOP::SlliW,
            rd,
            rs: x,
            shamt: pow2_shift(value as u32)?,
        });
        return Some(());
    }
    if value > 2 && ((value - 1) as u32).is_power_of_two() {
        let shifted = ctx.alloc_tmp(HirType::get_i32());
        ctx.emit(MInst::AluRRImmShift {
            op: AluRRImmShiftOP::SlliW,
            rd: Writable::from_reg(shifted),
            rs: x,
            shamt: pow2_shift((value - 1) as u32)?,
        });
        ctx.emit(MInst::AluRRR {
            op: AluRRROP::AddW,
            rd,
            rs1: shifted,
            rs2: x,
        });
        return Some(());
    }
    if value > 3 && ((value + 1) as u32).is_power_of_two() {
        let shifted = ctx.alloc_tmp(HirType::get_i32());
        ctx.emit(MInst::AluRRImmShift {
            op: AluRRImmShiftOP::SlliW,
            rd: Writable::from_reg(shifted),
            rs: x,
            shamt: pow2_shift((value + 1) as u32)?,
        });
        ctx.emit(MInst::AluRRR {
            op: AluRRROP::SubW,
            rd,
            rs1: shifted,
            rs2: x,
        });
        return Some(());
    }
    None
}

fn lower_binary(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    binary: &Binary,
) -> LoweredOutput {
    let bop = binary.op();
    let def = ctx.result_reg(inst);
    let rd = Writable::from_reg(def);
    let inst_ty = arena.inst_data(inst).ty();
    let is_i32 = matches!(inst_ty.kind(), HirTypeKind::Int32);
    let is_float = matches!(inst_ty.kind(), HirTypeKind::Float32)
        || matches!(
            arena.inst_data(binary.lhs()).ty().kind(),
            HirTypeKind::Float32
        );

    // `0 op x` identities fold before either operand is materialized: the
    // backend has no DCE pass, so a rematerialized `li 0` would stay in the
    // output. Float constants never reach here (`integer_constant` only
    // matches `Integer`). The `Value` aliases below rely on every i32
    // producer keeping values sign-extended in 64-bit registers (loads,
    // W-class ops, constants, ABI args); `slti`/`slli`/GEP indexing depend
    // on the same invariant.
    if integer_constant(arena, binary.lhs()) == Some(0) {
        match bop {
            BinaryOp::Add | BinaryOp::Or | BinaryOp::Xor => {
                let rhs = ctx.put_value_in_reg(binary.rhs());
                return LoweredOutput::Value(rhs);
            }
            BinaryOp::And => {
                ctx.emit(MInst::LoadImm { rd, value: 0 });
                return LoweredOutput::Value(def);
            }
            BinaryOp::Sub => {
                // 0 - x = neg(x), emitted as `subw rd, zero, x` (or `sub`).
                let rhs = ctx.put_value_in_reg(binary.rhs());
                ctx.emit(MInst::AluRRR {
                    op: if is_i32 {
                        AluRRROP::SubW
                    } else {
                        AluRRROP::Sub
                    },
                    rd,
                    rs1: zero_reg(),
                    rs2: rhs,
                });
                return LoweredOutput::Value(def);
            }
            BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Ge | BinaryOp::Le => {
                let rhs = ctx.put_value_in_reg(binary.rhs());
                let (rs1, rs2, invert) = match bop {
                    BinaryOp::Lt => (zero_reg(), rhs, false),
                    BinaryOp::Gt => (rhs, zero_reg(), false),
                    BinaryOp::Ge => (zero_reg(), rhs, true),
                    _ => (rhs, zero_reg(), true), // Le
                };
                return lower_slt(ctx, rd, rs1, rs2, invert);
            }
            BinaryOp::Eq | BinaryOp::NotEq => {
                let rhs = ctx.put_value_in_reg(binary.rhs());
                return lower_eq_zero(ctx, rd, bop == BinaryOp::Eq, rhs);
            }
            _ => {}
        }
    }

    let lhs = ctx.put_value_in_reg(binary.lhs());

    if is_float {
        let rhs = ctx.put_value_in_reg(binary.rhs());
        let op = match bop {
            BinaryOp::Add => FpuRRROP::FaddS,
            BinaryOp::Sub => FpuRRROP::FsubS,
            BinaryOp::Mul => FpuRRROP::FmulS,
            BinaryOp::Div => FpuRRROP::FdivS,
            BinaryOp::Lt | BinaryOp::Gt => FpuRRROP::FltS,
            BinaryOp::Le | BinaryOp::Ge => FpuRRROP::FleS,
            BinaryOp::Eq | BinaryOp::NotEq => FpuRRROP::FeqS,
            _ => unreachable!("unexpected float binary op: {:?}", bop),
        };
        let (rs1, rs2) = if matches!(bop, BinaryOp::Gt | BinaryOp::Ge) {
            (rhs, lhs)
        } else {
            (lhs, rhs)
        };
        if bop == BinaryOp::NotEq {
            let equal = ctx.alloc_tmp(HirType::get_i32());
            ctx.emit(MInst::FpuRRR {
                op,
                rd: Writable::from_reg(equal),
                rs1,
                rs2,
            });
            emit_xori_one(ctx, rd, equal);
        } else {
            ctx.emit(MInst::FpuRRR { op, rd, rs1, rs2 });
        }
    } else {
        if bop == BinaryOp::Div && integer_constant(arena, binary.rhs()) == Some(1) {
            return LoweredOutput::Value(lhs);
        }
        if matches!(inst_ty.kind(), HirTypeKind::Int32)
            && matches!(bop, BinaryOp::Div | BinaryOp::Rem)
            && integer_constant(arena, binary.rhs()).is_some_and(|divisor| {
                lower_signed_div_rem_power_of_two(ctx, bop, rd, lhs, divisor)
                    || lower_signed_div_rem_magic(ctx, bop, rd, lhs, divisor)
            })
        {
            return LoweredOutput::Value(def);
        }
        // Constant-operand folding: each arm returns when an immediate form
        // is selected; otherwise it falls through to the register form, which
        // is the only one allowed to materialize the constant (no DCE pass).
        let rhs_imm = integer_constant(arena, binary.rhs());
        let is_i32 = matches!(inst_ty.kind(), HirTypeKind::Int32);
        match bop {
            BinaryOp::Add | BinaryOp::Sub => {
                if rhs_imm == Some(0) {
                    return LoweredOutput::Value(lhs);
                }
                // add/sub x, x, k → addiw/addi x, x, ±k.
                let imm = rhs_imm
                    .map(|k| {
                        if bop == BinaryOp::Sub {
                            k.wrapping_neg()
                        } else {
                            k
                        }
                    })
                    .and_then(Imm12::from_i32);
                if let Some(imm) = imm {
                    ctx.emit(MInst::AluRRImm12 {
                        op: if is_i32 {
                            AluRRImm12OP::Addiw
                        } else {
                            AluRRImm12OP::Addi
                        },
                        rd,
                        rs: lhs,
                        imm,
                    });
                    return LoweredOutput::Value(def);
                }
            }
            BinaryOp::And | BinaryOp::Or | BinaryOp::Xor => {
                if rhs_imm == Some(0) {
                    if bop == BinaryOp::And {
                        ctx.emit(MInst::LoadImm { rd, value: 0 });
                        return LoweredOutput::Value(def);
                    }
                    return LoweredOutput::Value(lhs);
                }
                if let Some(imm) = rhs_imm.and_then(Imm12::from_i32) {
                    let op = match bop {
                        BinaryOp::And => AluRRImm12OP::Andi,
                        BinaryOp::Or => AluRRImm12OP::Ori,
                        BinaryOp::Xor => AluRRImm12OP::Xori,
                        _ => unreachable!(),
                    };
                    ctx.emit(MInst::AluRRImm12 {
                        op,
                        rd,
                        rs: lhs,
                        imm,
                    });
                    return LoweredOutput::Value(def);
                }
            }
            BinaryOp::Shl | BinaryOp::Shr | BinaryOp::Sar => {
                if rhs_imm == Some(0) {
                    // Zero shift = identity; aliasing keeps a negative i32
                    // sign-extended, unlike `srliw rd, rs, 0`.
                    return LoweredOutput::Value(lhs);
                }
                let shamt = rhs_imm.and_then(|v| u8::try_from(v).ok());
                if is_i32 {
                    if let Some(shamt) = shamt.and_then(ShiftImm::new) {
                        let op = match bop {
                            BinaryOp::Shl => AluRRImmShiftOP::SlliW,
                            BinaryOp::Shr => AluRRImmShiftOP::SrliW,
                            BinaryOp::Sar => AluRRImmShiftOP::SraiW,
                            _ => unreachable!(),
                        };
                        ctx.emit(MInst::AluRRImmShift {
                            op,
                            rd,
                            rs: lhs,
                            shamt,
                        });
                        return LoweredOutput::Value(def);
                    }
                } else if let Some(shamt) = shamt.and_then(ShiftImm64::new) {
                    let m = match bop {
                        BinaryOp::Shl => MInst::Slli { rd, rs: lhs, shamt },
                        BinaryOp::Shr => MInst::Srli { rd, rs: lhs, shamt },
                        BinaryOp::Sar => MInst::Srai { rd, rs: lhs, shamt },
                        _ => unreachable!(),
                    };
                    ctx.emit(m);
                    return LoweredOutput::Value(def);
                }
            }
            BinaryOp::Lt | BinaryOp::Ge => {
                // x < k → slti x, k; x >= k → slti x, k; xori 1.
                if let Some(imm) = rhs_imm.and_then(Imm12::from_i32) {
                    return lower_slti(ctx, rd, lhs, imm, bop == BinaryOp::Ge);
                }
            }
            BinaryOp::Gt | BinaryOp::Le => {
                if rhs_imm == Some(0) {
                    if bop == BinaryOp::Gt {
                        // x > 0 ⟺ 0 < x, folded to slt rd, zero, x.
                        return lower_slt(ctx, rd, zero_reg(), lhs, false);
                    }
                    // x <= 0 ⟺ x < 1.
                    return lower_slti(ctx, rd, lhs, Imm12::ONE, false);
                }
                // x > k ⟺ !(x < k+1); x <= k ⟺ x < k+1.
                if let Some(imm) = rhs_imm.and_then(|k| Imm12::from_i32(k.wrapping_add(1))) {
                    return lower_slti(ctx, rd, lhs, imm, bop == BinaryOp::Gt);
                }
            }
            BinaryOp::Eq | BinaryOp::NotEq => {
                // x == 0 → seqz x; x != 0 → snez x.
                if rhs_imm == Some(0) {
                    return lower_eq_zero(ctx, rd, bop == BinaryOp::Eq, lhs);
                }
                // x == k → addiw x, -k; seqz/snez. Only the truncating 32-bit
                // form is sound (comparison on the wrapping ring); wider
                // operands keep the exact 64-bit register form.
                if is_i32 {
                    if let Some(imm) = rhs_imm.and_then(|k| Imm12::from_i32(k.wrapping_neg())) {
                        let difference = ctx.alloc_tmp(HirType::get_i32());
                        ctx.emit(MInst::AluRRImm12 {
                            op: AluRRImm12OP::Addiw,
                            rd: Writable::from_reg(difference),
                            rs: lhs,
                            imm,
                        });
                        return lower_eq_zero(ctx, rd, bop == BinaryOp::Eq, difference);
                    }
                }
            }
            BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem | BinaryOp::Min | BinaryOp::Max => {}
        }
        // A constant multiplier that decomposes as 2^n or 2^n +/- 1 folds to
        // shift plus optional add/sub before either operand is materialized.
        if !is_float && bop == BinaryOp::Mul {
            let lhs_imm = integer_constant(arena, binary.lhs());
            let rhs_imm = integer_constant(arena, binary.rhs());
            if let Some((value, operand)) = lhs_imm
                .map(|value| (value, binary.rhs()))
                .or_else(|| rhs_imm.map(|value| (value, binary.lhs())))
            {
                let x = ctx.put_value_in_reg(operand);
                if value == 1 {
                    return LoweredOutput::Value(x);
                }
                if fold_mul_constant_riscv(ctx, rd, x, value).is_some() {
                    return LoweredOutput::Value(def);
                }
            }
        }
        let rhs = ctx.put_value_in_reg(binary.rhs());
        let op = (!matches!(bop, BinaryOp::Eq | BinaryOp::NotEq))
            .then(|| alu_op_for_hir_binary(bop, inst_ty));
        let sub_op = alu_op_for_hir_binary(BinaryOp::Sub, inst_ty);
        match bop {
            BinaryOp::Eq | BinaryOp::NotEq => {
                let difference = ctx.alloc_tmp(HirType::get_i32());
                ctx.emit(MInst::AluRRR {
                    op: sub_op,
                    rd: Writable::from_reg(difference),
                    rs1: lhs,
                    rs2: rhs,
                });
                return lower_eq_zero(ctx, rd, bop == BinaryOp::Eq, difference);
            }
            BinaryOp::Lt
            | BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::Mul
            | BinaryOp::Div
            | BinaryOp::Rem
            | BinaryOp::And
            | BinaryOp::Or
            | BinaryOp::Xor
            | BinaryOp::Shl
            | BinaryOp::Shr
            | BinaryOp::Sar => ctx.emit(MInst::AluRRR {
                op: op.unwrap(),
                rd,
                rs1: lhs,
                rs2: rhs,
            }),
            BinaryOp::Gt => ctx.emit(MInst::AluRRR {
                op: op.unwrap(),
                rd,
                rs1: rhs,
                rs2: lhs,
            }),
            BinaryOp::Ge | BinaryOp::Le => {
                let (rs1, rs2) = if bop == BinaryOp::Ge {
                    (lhs, rhs)
                } else {
                    (rhs, lhs)
                };
                let less = ctx.alloc_tmp(HirType::get_i32());
                ctx.emit(MInst::AluRRR {
                    op: op.unwrap(),
                    rd: Writable::from_reg(less),
                    rs1,
                    rs2,
                });
                ctx.emit(MInst::AluRRImm12 {
                    op: AluRRImm12OP::Xori,
                    rd,
                    rs: less,
                    imm: Imm12::ONE,
                });
            }
            BinaryOp::Min | BinaryOp::Max => {
                ctx.lowering_panic(
                    "RISC-V instruction selection",
                    "vector min/max requires a vector-capable backend",
                    Some(arena.inst_data(binary.lhs()).ty()),
                    Some(arena.inst_data(inst).ty()),
                );
            }
        }
    }
    LoweredOutput::Value(def)
}

fn lower_cast(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    cast: &Cast,
) -> LoweredOutput {
    let rs = ctx.put_value_in_reg(cast.src());
    let into_ty = arena.inst_data(inst).ty().kind();
    let def = ctx.result_reg(inst);
    let rd = Writable::from_reg(def);
    let mode = match into_ty {
        HirTypeKind::Int32 => FcvtMode::SinglePrecisionToWord,
        HirTypeKind::Float32 => FcvtMode::WordToSinglePrecision,
        _ => unreachable!("cast only produce i32 or f32"),
    };
    ctx.emit(MInst::Fcvt { mode, rd, rs });
    LoweredOutput::Value(def)
}

fn lower_select(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    select: &Select,
) -> LoweredOutput {
    let ty = arena.inst_data(inst).ty();
    let Some(ops) = select_alu_ops(ty) else {
        ctx.lowering_panic(
            "RISC-V instruction selection",
            "select supports only i32, f32, pointer, and string results",
            None,
            Some(ty),
        );
    };

    let cond = ctx.put_value_in_reg(select.cond());
    let if_true = ctx.put_value_in_reg(select.if_true());
    let if_false = ctx.put_value_in_reg(select.if_false());
    let def = ctx.result_reg(inst);
    let tmp_ty = select_tmp_ty(ty);
    let is_float = matches!(ty.kind(), HirTypeKind::Float32);
    let (if_true, if_false, result) = if is_float {
        let true_bits = ctx.alloc_tmp(HirType::get_i32());
        let false_bits = ctx.alloc_tmp(HirType::get_i32());
        let result_bits = ctx.alloc_tmp(HirType::get_i32());
        ctx.emit(MInst::Mov {
            src: if_true,
            dst: Writable::from_reg(true_bits),
        });
        ctx.emit(MInst::Mov {
            src: if_false,
            dst: Writable::from_reg(false_bits),
        });
        (true_bits, false_bits, result_bits)
    } else {
        (if_true, if_false, def)
    };
    let is_nonzero = ctx.alloc_tmp(HirType::get_i32());
    let mask = ctx.alloc_tmp(tmp_ty.clone());
    let delta = ctx.alloc_tmp(tmp_ty.clone());
    let masked_delta = ctx.alloc_tmp(tmp_ty);

    ctx.emit(MInst::AluRRR {
        op: AluRRROP::Snez,
        rd: Writable::from_reg(is_nonzero),
        rs1: cond,
        rs2: zero_reg(),
    });
    ctx.emit(MInst::AluRRR {
        op: ops.sub,
        rd: Writable::from_reg(mask),
        rs1: zero_reg(),
        rs2: is_nonzero,
    });
    ctx.emit(MInst::AluRRR {
        op: ops.xor,
        rd: Writable::from_reg(delta),
        rs1: if_true,
        rs2: if_false,
    });
    ctx.emit(MInst::AluRRR {
        op: ops.and,
        rd: Writable::from_reg(masked_delta),
        rs1: delta,
        rs2: mask,
    });
    ctx.emit(MInst::AluRRR {
        op: ops.xor,
        rd: Writable::from_reg(result),
        rs1: if_false,
        rs2: masked_delta,
    });
    if is_float {
        ctx.emit(MInst::Mov {
            src: result,
            dst: Writable::from_reg(def),
        });
    }
    LoweredOutput::Value(def)
}

fn lower_alloc(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
) -> LoweredOutput {
    let def = ctx.result_reg(inst);
    let rd = Writable::from_reg(def);
    let pointee_ty = arena.inst_data(inst).ty().derefernce();
    let offset = ctx.alloc_stackslot_or_get(inst, pointee_ty) as i64;
    ctx.emit(<Riscv64ABI as ABIMachineSpec>::gen_get_stack_addr(
        StackAMode::Slot(offset),
        rd,
    ));
    LoweredOutput::Value(def)
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
        // A sign-extended i32 index can be scaled directly; otherwise it must
        // be extended to 64 bits first (addw rd, rd, zero).
        let extended = if is_sign_extended_i32(arena, term.index) {
            index
        } else {
            let extended = ctx.alloc_tmp(pointer_ty.clone());
            ctx.emit(MInst::AluRRR {
                op: AluRRROP::AddW,
                rd: Writable::from_reg(extended),
                rs1: index,
                rs2: zero_reg(),
            });
            extended
        };

        let scaled = if term.stride == 1 {
            extended
        } else if term.stride.is_power_of_two() {
            let shift = u8::try_from(term.stride.trailing_zeros())
                .expect("u64 trailing-zero count fits u8");
            let scaled = ctx.alloc_tmp(pointer_ty.clone());
            ctx.emit(MInst::Slli {
                rd: Writable::from_reg(scaled),
                rs: extended,
                shamt: ShiftImm64::new(shift).expect("u64 power-of-two shift is encodable"),
            });
            scaled
        } else {
            let stride = ctx.alloc_tmp(pointer_ty.clone());
            ctx.emit(MInst::LoadImm {
                rd: Writable::from_reg(stride),
                value: term.stride,
            });
            let scaled = ctx.alloc_tmp(pointer_ty.clone());
            ctx.emit(MInst::AluRRR {
                op: AluRRROP::Mul,
                rd: Writable::from_reg(scaled),
                rs1: extended,
                rs2: stride,
            });
            scaled
        };

        let next = ctx.alloc_tmp(pointer_ty.clone());
        ctx.emit(MInst::AluRRR {
            op: AluRRROP::Add,
            rd: Writable::from_reg(next),
            rs1: address,
            rs2: scaled,
        });
        address = next;
    }

    if analysis.constant_offset == 0 {
        return LoweredOutput::Value(address);
    }

    let result = ctx.result_reg(inst);
    if let Some(imm) = i32::try_from(analysis.constant_offset)
        .ok()
        .and_then(Imm12::from_i32)
    {
        ctx.emit(MInst::AluRRImm12 {
            op: AluRRImm12OP::Addi,
            rd: Writable::from_reg(result),
            rs: address,
            imm,
        });
        return LoweredOutput::Value(result);
    }

    let offset = ctx.alloc_tmp(pointer_ty);
    ctx.emit(MInst::LoadImm {
        rd: Writable::from_reg(offset),
        value: analysis.constant_offset as u64,
    });
    ctx.emit(MInst::AluRRR {
        op: AluRRROP::Add,
        rd: Writable::from_reg(result),
        rs1: address,
        rs2: offset,
    });
    LoweredOutput::Value(result)
}

fn lower_store(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    store: &Store,
) -> LoweredOutput {
    let src = store.src();
    let dst = store.dest();

    if let InstKind::Aggregate(agg) = arena.inst_data(src).kind() {
        let elems = agg.flatten(&arena);
        assert!(matches!(arena.inst_data(dst).kind(), InstKind::Alloc));
        let dst_pointee = arena.inst_data(dst).ty().derefernce();
        let base_offset = ctx.alloc_stackslot_or_get(dst, dst_pointee) as i64;
        let mut elem_offset: i64 = 0;
        for elem in elems {
            let rs = ctx.put_value_in_reg(elem);
            let elem_ty = arena.inst_data(elem).ty();
            let m_type: LoweredType = elem_ty.into();
            let op: StoreOP = m_type.into();
            let addr = normalize_amode(AMode::SlotOffset(base_offset + elem_offset), ctx);
            ctx.emit(MInst::StoreWord { rs, op, addr });
            elem_offset += elem_ty.size() as i64;
        }
    } else if matches!(arena.inst_data(src).kind(), InstKind::ZeroInit) {
        let src_ty = arena.inst_data(src).ty();
        let total = src_ty.array_flatten_length();
        let elem_size = src_ty.array_base_scalar_type().size() as i64;
        let dst_pointee = arena.inst_data(dst).ty().derefernce();
        let base_offset = ctx.alloc_stackslot_or_get(dst, dst_pointee) as i64;
        for i in 0..total {
            let addr = normalize_amode(AMode::SlotOffset(base_offset + i as i64 * elem_size), ctx);
            ctx.emit(MInst::StoreWord {
                rs: zero_reg(),
                op: StoreOP::Sw,
                addr,
            });
        }
    } else {
        let m_type: LoweredType = arena.inst_data(src).ty().into();
        let op = m_type.into();
        let rs = ctx.put_value_in_reg(src);
        if dst.is_global() {
            let addr_tmp = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
            ctx.emit(MInst::LoadAddr {
                rd: Writable::from_reg(addr_tmp),
                label: Label::GlobalValue(dst),
            });
            ctx.emit(MInst::StoreWord {
                rs,
                op,
                addr: AMode::RegOffest(addr_tmp, 0),
            });
        } else {
            match arena.inst_data(dst).kind() {
                InstKind::GetElemPtr(..) => {
                    // Fold a single-use constant GEP into the store addressing
                    // mode (`sw rs, off(base)`); otherwise materialize.
                    let addr = try_fold_gep_amode(ctx, arena, dst, inst)
                        .unwrap_or_else(|| AMode::RegOffest(ctx.put_value_in_reg(dst), 0));
                    ctx.emit(MInst::StoreWord { rs, op, addr });
                }
                InstKind::Alloc => {
                    let pointee_ty = arena.inst_data(dst).ty().derefernce();
                    let offset = ctx.alloc_stackslot_or_get(dst, pointee_ty);
                    let addr = normalize_amode(AMode::SlotOffset(offset as i64), ctx);
                    ctx.emit(MInst::StoreWord { rs, op, addr });
                }
                _ => unreachable!("should not store in instruction other than GEP or Alloc"),
            }
        }
    }
    LoweredOutput::None
}

fn lower_mem_zero(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    mem_zero: &MemZero,
) -> LoweredOutput {
    match mem_zero.byte_len_len() {
        MemZeroLen::Const(byte_len) => lower_const_mem_zero(ctx, arena, mem_zero, *byte_len),
        MemZeroLen::Value(byte_len) => {
            let dest = ctx.put_value_in_reg(mem_zero.dest());
            let zero = ctx.alloc_tmp(HirType::get_i32());
            let byte_len = ctx.put_value_in_reg(*byte_len);
            ctx.emit(MInst::LoadImm {
                rd: Writable::from_reg(zero),
                value: 0,
            });
            ctx.emit(MInst::Call {
                arg_pairs: smallvec![
                    CallArgPair {
                        vreg: dest,
                        preg: a0(),
                    },
                    CallArgPair {
                        vreg: zero,
                        preg: a1(),
                    },
                    CallArgPair {
                        vreg: byte_len,
                        preg: a2(),
                    },
                ],
                ret: None,
                clobbers: DEFAULT_CLOBBERS,
                label: Label::LibCall(LibCall::Memset),
            });
            ctx.set_has_calls();
            ctx.set_outgoing_arg_size(0);
            LoweredOutput::None
        }
    }
}

fn lower_const_mem_zero(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    mem_zero: &MemZero,
    byte_len: usize,
) -> LoweredOutput {
    let inline_store_count = byte_len / 4;
    if byte_len % 4 == 0 && inline_store_count <= INLINE_MEMZERO_MAX_STORES {
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
        if let Some(offset) = stack_offset {
            ctx.emit(<Riscv64ABI as ABIMachineSpec>::gen_get_stack_addr(
                StackAMode::Slot(offset),
                Writable::from_reg(dest),
            ));
        }
        for index in 0..inline_store_count {
            ctx.emit(MInst::StoreWord {
                rs: zero_reg(),
                op: StoreOP::Sw,
                addr: AMode::RegOffest(dest, (index * 4) as i64),
            });
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
    let zero = ctx.alloc_tmp(HirType::get_i32());
    let byte_len_reg = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
    if let Some(offset) = stack_offset {
        ctx.emit(<Riscv64ABI as ABIMachineSpec>::gen_get_stack_addr(
            StackAMode::Slot(offset),
            Writable::from_reg(dest),
        ));
    }
    ctx.emit(MInst::LoadImm {
        rd: Writable::from_reg(zero),
        value: 0,
    });
    ctx.emit(MInst::LoadImm {
        rd: Writable::from_reg(byte_len_reg),
        value: byte_len as u64,
    });
    ctx.emit(MInst::Call {
        arg_pairs: smallvec![
            CallArgPair {
                vreg: dest,
                preg: a0(),
            },
            CallArgPair {
                vreg: zero,
                preg: a1(),
            },
            CallArgPair {
                vreg: byte_len_reg,
                preg: a2(),
            },
        ],
        ret: None,
        clobbers: DEFAULT_CLOBBERS,
        label: Label::LibCall(LibCall::Memset),
    });
    ctx.set_has_calls();
    ctx.set_outgoing_arg_size(0);
    LoweredOutput::None
}

fn lower_load(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    load: &Load,
) -> LoweredOutput {
    let src = load.src();
    let m_type: LoweredType = arena.inst_data(inst).ty().into();
    let op: LoadOP = m_type.into();
    let def = ctx.result_reg(inst);
    let rd = Writable::from_reg(def);
    if src.is_global() {
        ctx.emit(MInst::LoadWord {
            rd,
            op,
            addr: AMode::Label(Label::GlobalValue(src)),
        })
    } else if matches!(arena.inst_data(src).kind(), InstKind::GetElemPtr(..)) {
        // Fold a single-use constant GEP into the load addressing mode
        // (`lw rd, off(base)`); otherwise materialize the address.
        let addr = try_fold_gep_amode(ctx, arena, src, inst)
            .unwrap_or_else(|| AMode::RegOffest(ctx.put_value_in_reg(src), 0));
        ctx.emit(MInst::LoadWord { rd, op, addr });
    } else {
        // For SysY, this branch only happen when SSA is disabled.
        // All load from integer/float is translated into SSA from.
        let alloc_ty = arena.inst_data(src).ty().derefernce();
        let offset = ctx.alloc_stackslot_or_get(src, alloc_ty);
        let addr = normalize_amode(AMode::SlotOffset(offset as i64), ctx);
        ctx.emit(MInst::LoadWord { rd, op, addr });
    }
    LoweredOutput::Value(def)
}

fn lower_call(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    call: &Call,
) -> LoweredOutput {
    let mut call_arg_pairs = smallvec![];
    let types: Vec<_> = call
        .args()
        .iter()
        .map(|&arg| arena.inst_data(arg).ty().clone())
        .collect();
    let (locations, outgoing_arg_size) = Riscv64ABI::compute_call_arg_loc(&types);
    for (&arg, location) in call.args().iter().zip(locations) {
        let arg_reg = ctx.put_value_in_reg(arg);
        match location {
            ArgSlot::Reg { reg, .. } => call_arg_pairs.push(CallArgPair {
                vreg: arg_reg,
                preg: reg.into(),
            }),
            ArgSlot::Stack { offset, ty } => {
                let op: StoreOP = LoweredType::from(&ty).into();
                let addr = normalize_amode(AMode::OutgoingArg(offset), ctx);
                ctx.emit(MInst::StoreWord {
                    rs: arg_reg,
                    op,
                    addr,
                });
            }
        }
    }
    let result_ty = arena.inst_data(inst).ty();
    let result = (!result_ty.is_unit()).then(|| ctx.result_reg(inst));
    let ret_arg_pair = match result_ty.kind() {
        HirTypeKind::Unit => None,
        HirTypeKind::Int32 => Some(CallRetPair {
            vreg: Writable::from_reg(result.unwrap()),
            preg: a0(),
        }),
        HirTypeKind::Float32 => Some(CallRetPair {
            vreg: Writable::from_reg(result.unwrap()),
            preg: fa0(),
        }),
        _ => unreachable!(),
    };
    ctx.emit(MInst::Call {
        arg_pairs: call_arg_pairs,
        ret: ret_arg_pair,
        clobbers: DEFAULT_CLOBBERS,
        label: Label::Function(call.callee()),
    });
    ctx.set_has_calls();
    ctx.set_outgoing_arg_size(outgoing_arg_size as usize);
    result.map_or(LoweredOutput::None, LoweredOutput::Value)
}

/// Lower an ABI-compatible tail call. Register arguments are forced into the
/// ABI argument registers via the `TailCall` operands; stack arguments are
/// stored to the incoming-argument slots. Tail-call elimination guarantees
/// that caller and callee signatures match. The emitter prepends the epilogue
/// (frame restore) to the `TailCall`, which then jumps without linking.
fn lower_tail_call(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    tail_call: &TailCall,
) -> LoweredOutput {
    let mut call_arg_pairs: SmallVec<[CallArgPair; 8]> = smallvec![];
    for (idx, &arg) in tail_call.args().iter().enumerate() {
        let arg_reg = ctx.put_value_in_reg(arg);
        let arg_ty = arena.inst_data(arg).ty();
        let m_type: LoweredType = arg_ty.into();
        match ctx.arg_slot(idx) {
            ArgSlot::Reg { reg, .. } => call_arg_pairs.push(CallArgPair {
                vreg: arg_reg,
                preg: reg.into(),
            }),
            ArgSlot::Stack { offset, .. } => {
                let op: StoreOP = m_type.into();
                let addr = normalize_amode(AMode::IncomingArg(offset), ctx);
                ctx.emit(MInst::StoreWord {
                    rs: arg_reg,
                    op,
                    addr,
                });
            }
        }
    }
    ctx.set_has_calls();
    ctx.emit(MInst::TailCall {
        arg_pairs: call_arg_pairs,
        clobbers: DEFAULT_CLOBBERS,
        label: Label::Function(tail_call.callee()),
    });
    LoweredOutput::None
}

fn lower_return(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    ret: &Return,
) -> LoweredOutput {
    if let Some(val) = ret.value() {
        let preg = match arena.inst_data(val).ty().kind() {
            HirTypeKind::Int32 | HirTypeKind::Pointer(_) => a0(),
            HirTypeKind::Float32 => fa0(),
            ty => unreachable!("unexpected return type: {ty:?}"),
        };
        let src = ctx.put_value_in_reg(val);
        ctx.emit(MInst::RetVal {
            pair: RetPair { vreg: src, preg },
        });
    }
    ctx.emit(MInst::Ret);
    LoweredOutput::None
}

pub struct Riscv64Backend;

impl LowerBackend for Riscv64Backend {
    type MInst = MInst;
    type CodegenConfig = ();
    fn lower(ctx: &mut LowerContext<Self::MInst>, inst: HirInst) -> LoweredOutput {
        let arena = ctx.arena;
        match arena.inst_data(inst).kind() {
            InstKind::BlockArgRef(..)
            | InstKind::Aggregate(..)
            | InstKind::GlobalAlloc(..)
            | InstKind::Undef
            | InstKind::ZeroInit
            | InstKind::Integer(..)
            | InstKind::Float(..) => {
                // Should not in layout for now.
                // Even when changed we don't have to do anything
                // Because these instruction are just defining a value.
                unreachable!("currently constants are not in layout")
            }
            InstKind::Binary(binary) => lower_binary(ctx, arena, inst, binary),
            InstKind::Select(select) => lower_select(ctx, arena, inst, select),
            InstKind::Cast(cast) => lower_cast(ctx, arena, inst, cast),
            InstKind::Alloc => lower_alloc(ctx, arena, inst),
            InstKind::GetElemPtr(get_elem_ptr) => {
                lower_get_elem_ptr(ctx, arena, inst, get_elem_ptr)
            }
            InstKind::Store(store) => lower_store(ctx, arena, inst, store),
            InstKind::MemZero(mem_zero) => lower_mem_zero(ctx, arena, mem_zero),
            InstKind::Load(load) => lower_load(ctx, arena, inst, load),
            InstKind::Call(call) => lower_call(ctx, arena, inst, call),
            InstKind::TailCall(tail_call) => lower_tail_call(ctx, arena, tail_call),
            InstKind::Return(ret) => lower_return(ctx, arena, ret),
            InstKind::Fma(..)
            | InstKind::VectorSplat(..)
            | InstKind::VectorExtractElement(..)
            | InstKind::VectorInsertElement(..)
            | InstKind::VectorReduce(..) => {
                unreachable!("vector IR requires a vector-capable backend")
            }
            InstKind::Jump(..) | InstKind::Branch(..) => {
                unreachable!("should not lower branch instruction in here.")
            }
        }
    }

    fn lower_branch(
        ctx: &mut taki_mir::lower::LowerContext<Self::MInst>,
        inst: raana_ir::opt::prelude::Inst,
        target: &[taki_mir::block_order::MirBlockIndex],
    ) {
        let inst_data = ctx
            .arena
            .program
            .func_data(ctx.arena.curr_func.unwrap())
            .inst_data(inst);
        match inst_data.kind() {
            raana_ir::ir::InstKind::Jump(jump) => {
                let args = jump.args();
                for &arg in args {
                    ctx.put_value_in_reg(arg);
                }
                let &[target] = target else { unreachable!() };
                ctx.emit(MInst::Jump {
                    label: Label::Block(target),
                });
            }
            raana_ir::ir::InstKind::Branch(branch) => {
                let t_args = branch.t_args();
                let f_args = branch.f_args();
                for &arg in t_args.iter().chain(f_args) {
                    ctx.put_value_in_reg(arg);
                }
                let &[t_target, f_target] = target else {
                    unreachable!()
                };
                // Fold an IR comparison into a two-register B-type branch,
                // skipping the slt/subw+seqz materialization (see
                // `select_branch_condition`).
                let BranchCondition { op, rs1, rs2 } = select_branch_condition(ctx, branch.cond());
                ctx.emit(MInst::CondBr {
                    op,
                    rs1,
                    rs2,
                    true_label: Label::Block(t_target),
                    false_label: Label::Block(f_target),
                });
            }
            _ => unreachable!("should not lower non-branch isntruction in here."),
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
        preg_name(preg)
    }

    fn format_block_label(lb: &LoweredBlock, func_data: &HirFunctionData) -> String {
        let function = func_data.name().replace('%', "_");
        match lb {
            LoweredBlock::Orig { block } => {
                let bb_name = func_data.bb_data(*block).name().replace('%', "_");
                format!(".L_{function}_{bb_name}")
            }
            LoweredBlock::Edge {
                pred,
                succ,
                succ_idx,
            } => {
                let p = func_data.bb_data(*pred).name().replace('%', "_");
                let s = func_data.bb_data(*succ).name().replace('%', "_");
                format!(".L_{function}_{p}_to_{s}_edge_{succ_idx}")
            }
        }
    }

    fn emit_long_jump(
        ctx: &mut taki_mir::lower::LowerContext<MInst>,
        target: taki_mir::block_order::MirBlockIndex,
    ) {
        ctx.emit(MInst::Jump {
            label: Label::Block(target),
        });
    }

    fn branch_opt_enabled(_config: &Self::CodegenConfig) -> bool {
        true
    }

    fn veneer_lines(kind: taki_mir::emit_buffer::LabelKind, target: &str) -> Vec<String> {
        use taki_mir::emit_buffer::LabelKind;
        match kind {
            // A B-type branch fell out of ±4KiB: a `j` veneer covers the
            // whole ±1MiB jal range (functions here are far smaller).
            LabelKind::RV_B | LabelKind::RV_JAL => vec![format!("j {target}")],
            other => unreachable!("RISC-V does not emit {other:?} branches"),
        }
    }
}

/// Fold an IR comparison into a two-register RISC-V B-type branch condition.
/// Returns `None` when the condition is not a direct integer comparison; the
/// caller then falls back to a truthiness test (`bne reg, x0`).
/// A two-register B-type comparison selected for an IR branch condition.
struct BranchCondition {
    op: CondBrOp,
    rs1: Reg,
    rs2: Reg,
}

/// Lower an IR branch condition to a two-register B-type comparison.
///
/// Integer comparisons fold directly (`Lt` -> `blt`, `Eq` -> `beq`, ...),
/// skipping the `slt`/`subw;seqz` materialization. Float comparisons lower
/// to `flt.s`/`fle.s`/`feq.s`, whose raw bit patterns must not be compared
/// as integers, and any other condition (e.g. an inlined block param) tests
/// a register against x0 instead.
fn select_branch_condition(ctx: &mut LowerContext<'_, MInst>, cond: HirInst) -> BranchCondition {
    let InstKind::Binary(binary) = ctx.arena.inst_data(cond).kind() else {
        return branch_truthiness(ctx, cond);
    };
    let operand_float =
        |inst: HirInst| matches!(ctx.arena.inst_data(inst).ty().kind(), HirTypeKind::Float32);
    if operand_float(binary.lhs()) || operand_float(binary.rhs()) {
        return branch_truthiness(ctx, cond);
    }
    // Mirror `lower_binary`'s operand placement: Gt/Le emit `slt` with the
    // operands swapped, and the branch folds to the same B-type form.
    let (op, lhs, rhs) = match binary.op() {
        BinaryOp::Lt => (CondBrOp::Blt, binary.lhs(), binary.rhs()),
        BinaryOp::Gt => (CondBrOp::Blt, binary.rhs(), binary.lhs()),
        BinaryOp::Ge => (CondBrOp::Bge, binary.lhs(), binary.rhs()),
        BinaryOp::Le => (CondBrOp::Bge, binary.rhs(), binary.lhs()),
        BinaryOp::Eq => (CondBrOp::Beq, binary.lhs(), binary.rhs()),
        BinaryOp::NotEq => (CondBrOp::Bne, binary.lhs(), binary.rhs()),
        _ => return branch_truthiness(ctx, cond),
    };
    BranchCondition {
        op,
        rs1: ctx.put_value_in_reg(lhs),
        rs2: ctx.put_value_in_reg(rhs),
    }
}

/// Fallback: branch on the truthiness of a materialized condition register.
fn branch_truthiness(ctx: &mut LowerContext<'_, MInst>, cond: HirInst) -> BranchCondition {
    BranchCondition {
        op: CondBrOp::Bne,
        rs1: ctx.put_value_in_reg(cond),
        rs2: zero_reg(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use raana_ir::ir::{
        Program,
        builder_trait::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
    };

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

    /// Emits `parameter <op> divisor` as a whole function and returns its
    /// RISC-V assembly.
    fn compile_constant_binary(op: BinaryOp, divisor: i32) -> String {
        let mut program = Program::new();
        let function = program.new_function(
            HirType::get_i32(),
            "constant".to_string(),
            vec![HirType::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let numerator = data.params()[0];
        let constant = data.new_local_inst().integer(divisor);
        let binary = data.new_local_inst().binary(op, numerator, constant);
        let ret = data.new_local_inst().ret(Some(binary));
        data.layout_mut().insert_inst(entry, binary);
        data.layout_mut().insert_inst(entry, ret);
        taki_mir::compile::<Riscv64Backend>(&program)
    }

    #[test]
    fn division_by_a_constant_replaces_divw_with_a_multiply_high() {
        for divisor in [3, 7, -7, 100, 1000000007, i32::MAX] {
            let asm = compile_constant_binary(BinaryOp::Div, divisor);
            assert!(!asm.contains("divw "), "{divisor}:\n{asm}");
            assert!(asm.contains("mul "), "{divisor}:\n{asm}");
            assert!(asm.contains("srai "), "{divisor}:\n{asm}");
        }
    }

    #[test]
    fn remainder_by_a_constant_multiplies_the_quotient_back() {
        let asm = compile_constant_binary(BinaryOp::Rem, 7);
        assert!(!asm.contains("remw "), "{asm}");
        assert!(!asm.contains("divw "), "{asm}");
        assert!(asm.contains("mulw "), "{asm}");
        assert!(asm.contains("subw "), "{asm}");
    }

    #[test]
    fn cheaper_divisors_keep_their_existing_sequences() {
        // Powers of two stay on the shift sequence, and a divisor of one
        // disappears entirely.
        for divisor in [2, -8, i32::MIN] {
            for op in [BinaryOp::Div, BinaryOp::Rem] {
                let asm = compile_constant_binary(op, divisor);
                assert!(!asm.contains("srai "), "{op:?} {divisor}:\n{asm}");
                assert!(!asm.contains("divw "), "{op:?} {divisor}:\n{asm}");
                assert!(!asm.contains("remw "), "{op:?} {divisor}:\n{asm}");
            }
        }
        let asm = compile_constant_binary(BinaryOp::Div, 1);
        assert!(!asm.contains("srai "), "{asm}");
        assert!(!asm.contains("divw "), "{asm}");
    }

    #[test]
    fn division_by_a_variable_still_uses_divw() {
        let mut program = Program::new();
        let function = program.new_function(
            HirType::get_i32(),
            "variable".to_string(),
            vec![HirType::get_i32(), HirType::get_i32()],
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

        let asm = taki_mir::compile::<Riscv64Backend>(&program);
        assert!(asm.contains("divw "), "{asm}");
    }

    /// Emits `constant <op> parameter` as a whole function and returns its
    /// RISC-V assembly (mirror of `compile_constant_binary`).
    fn compile_constant_on_lhs(op: BinaryOp, value: i32) -> String {
        let mut program = Program::new();
        let function = program.new_function(
            HirType::get_i32(),
            "constant_lhs".to_string(),
            vec![HirType::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let parameter = data.params()[0];
        let constant = data.new_local_inst().integer(value);
        let binary = data.new_local_inst().binary(op, constant, parameter);
        let ret = data.new_local_inst().ret(Some(binary));
        data.layout_mut().insert_inst(entry, binary);
        data.layout_mut().insert_inst(entry, ret);
        taki_mir::compile::<Riscv64Backend>(&program)
    }

    #[test]
    fn logic_within_imm12_uses_immediate_forms() {
        let asm = compile_constant_binary(BinaryOp::And, 0xff);
        assert!(asm.contains("andi "), "{asm}");
        assert!(!asm.contains("and "), "{asm}");
        assert!(!asm.contains("li "), "{asm}");

        let asm = compile_constant_binary(BinaryOp::Or, 3);
        assert!(asm.contains("ori "), "{asm}");
        assert!(!asm.contains("or "), "{asm}");

        let asm = compile_constant_binary(BinaryOp::Xor, 1);
        assert!(asm.contains("xori "), "{asm}");
        assert!(!asm.contains("xor "), "{asm}");
        assert!(!asm.contains("li "), "{asm}");

        // Negative masks encode through the sign-extended 12-bit immediate.
        let asm = compile_constant_binary(BinaryOp::And, -2);
        assert!(asm.contains("andi "), "{asm}");
        assert!(!asm.contains("and "), "{asm}");

        let asm = compile_constant_binary(BinaryOp::Or, -1);
        assert!(asm.contains("ori "), "{asm}");
        assert!(!asm.contains("or "), "{asm}");
    }

    #[test]
    fn subtraction_from_zero_is_a_negation() {
        let asm = compile_constant_on_lhs(BinaryOp::Sub, 0);
        assert!(asm.contains("subw "), "{asm}");
        assert!(!asm.contains("li "), "{asm}");
    }

    #[test]
    fn zero_identity_operands_fold_before_materialization() {
        // Both the left intercept (`0 op x`) and the right fold (`x op 0`)
        // must avoid materializing `li 0` (no DCE). `0 - x` is covered by
        // `subtraction_from_zero_is_a_negation`; `x - 0` aliases below.
        for on_lhs in [false, true] {
            let add = if on_lhs {
                compile_constant_on_lhs(BinaryOp::Add, 0)
            } else {
                compile_constant_binary(BinaryOp::Add, 0)
            };
            assert!(!add.contains("li "), "{on_lhs}:\n{add}");
            assert!(!add.contains("addw "), "{on_lhs}:\n{add}");

            let or = if on_lhs {
                compile_constant_on_lhs(BinaryOp::Or, 0)
            } else {
                compile_constant_binary(BinaryOp::Or, 0)
            };
            assert!(!or.contains("li "), "{on_lhs}:\n{or}");
            assert!(!or.contains("or "), "{on_lhs}:\n{or}");

            let xor = if on_lhs {
                compile_constant_on_lhs(BinaryOp::Xor, 0)
            } else {
                compile_constant_binary(BinaryOp::Xor, 0)
            };
            assert!(!xor.contains("li "), "{on_lhs}:\n{xor}");
            assert!(!xor.contains("xor "), "{on_lhs}:\n{xor}");

            let and = if on_lhs {
                compile_constant_on_lhs(BinaryOp::And, 0)
            } else {
                compile_constant_binary(BinaryOp::And, 0)
            };
            assert!(and.contains("li "), "{on_lhs}:\n{and}");
        }

        let sub = compile_constant_binary(BinaryOp::Sub, 0);
        assert!(!sub.contains("li "), "{sub}");
        assert!(!sub.contains("subw "), "{sub}");
    }

    #[test]
    fn equality_with_zero_is_a_single_seqz_or_snez() {
        let asm = compile_constant_binary(BinaryOp::Eq, 0);
        assert!(asm.contains("seqz "), "{asm}");
        assert!(!asm.contains("subw "), "{asm}");
        assert!(!asm.contains("li "), "{asm}");

        let asm = compile_constant_binary(BinaryOp::NotEq, 0);
        assert!(asm.contains("snez "), "{asm}");
        assert!(!asm.contains("subw "), "{asm}");

        let asm = compile_constant_on_lhs(BinaryOp::Eq, 0);
        assert!(asm.contains("seqz "), "{asm}");
        assert!(!asm.contains("li "), "{asm}");
    }

    #[test]
    fn zero_comparisons_use_the_zero_register() {
        // `x > 0` (right fold) and `0 < x`/`0 > x` (left intercept) all
        // lower to `slt` against x0.
        let asm = compile_constant_binary(BinaryOp::Gt, 0);
        assert!(asm.contains("slt "), "{asm}");
        assert!(!asm.contains("li "), "{asm}");
        assert!(!asm.contains("xori "), "{asm}");

        let asm = compile_constant_on_lhs(BinaryOp::Lt, 0);
        assert!(asm.contains("slt "), "{asm}");
        assert!(!asm.contains("li "), "{asm}");

        let asm = compile_constant_on_lhs(BinaryOp::Gt, 0);
        assert!(asm.contains("slt "), "{asm}");
        assert!(!asm.contains("li "), "{asm}");
    }

    #[test]
    fn imm12_boundaries_fold_or_fall_back() {
        // Upper and lower encodable bounds, plus small constants.
        let asm = compile_constant_binary(BinaryOp::Add, 1);
        assert!(asm.contains("addiw "), "{asm}");
        assert!(!asm.contains("li "), "{asm}");

        let asm = compile_constant_binary(BinaryOp::Add, 2047);
        assert!(asm.contains("addiw "), "{asm}");
        assert!(asm.contains("2047"), "{asm}");

        let asm = compile_constant_binary(BinaryOp::Add, -2048);
        assert!(asm.contains("addiw "), "{asm}");
        assert!(asm.contains("-2048"), "{asm}");

        // Negative constants; sub folds through addiw of the negated value.
        let asm = compile_constant_binary(BinaryOp::Add, -1);
        assert!(asm.contains("addiw "), "{asm}");

        let asm = compile_constant_binary(BinaryOp::Sub, 1);
        assert!(asm.contains("addiw "), "{asm}");
        assert!(asm.contains(", -1"), "{asm}");

        let asm = compile_constant_binary(BinaryOp::Sub, 2048);
        assert!(asm.contains("addiw "), "{asm}");
        assert!(asm.contains("-2048"), "{asm}");
        assert!(!asm.contains("subw "), "{asm}");

        // Outside the 12-bit signed range: register form.
        let asm = compile_constant_binary(BinaryOp::Add, 2048);
        assert!(asm.contains("addw "), "{asm}");
        assert!(!asm.contains("addiw "), "{asm}");

        let asm = compile_constant_binary(BinaryOp::Sub, -2049);
        assert!(asm.contains("subw "), "{asm}");
        assert!(!asm.contains("addiw "), "{asm}");
    }

    #[test]
    fn comparison_boundaries_fold_or_fall_back() {
        // x > k ⟺ !(x < k+1); x <= k ⟺ x < k+1. k+1 = 2048 is not
        // encodable, so k = 2047 falls back to the register form.
        let asm = compile_constant_binary(BinaryOp::Gt, 2047);
        assert!(!asm.contains("slti "), "{asm}");
        assert!(asm.contains("slt "), "{asm}");

        let asm = compile_constant_binary(BinaryOp::Le, 2047);
        assert!(!asm.contains("slti "), "{asm}");
        assert!(asm.contains("slt "), "{asm}");

        // k+1 = 2047 / -2048 remain encodable.
        let asm = compile_constant_binary(BinaryOp::Gt, 2046);
        assert!(asm.contains("slti "), "{asm}");
        assert!(asm.contains("2047"), "{asm}");
        assert!(asm.contains("xori "), "{asm}");

        let asm = compile_constant_binary(BinaryOp::Le, -2049);
        assert!(asm.contains("slti "), "{asm}");
        assert!(asm.contains("-2048"), "{asm}");
        assert!(!asm.contains("xori "), "{asm}");

        // x < 0 / x >= 0 / x <= 0 fold through the slti forms.
        let asm = compile_constant_binary(BinaryOp::Lt, 0);
        assert!(asm.contains("slti "), "{asm}");
        assert!(asm.contains(", 0"), "{asm}");
        assert!(!asm.contains("li "), "{asm}");

        let asm = compile_constant_binary(BinaryOp::Ge, 0);
        assert!(asm.contains("slti "), "{asm}");
        assert!(asm.contains("xori "), "{asm}");

        let asm = compile_constant_binary(BinaryOp::Le, 0);
        assert!(asm.contains("slti "), "{asm}");
        assert!(asm.contains(", 1"), "{asm}");
        assert!(!asm.contains("xori "), "{asm}");
    }

    #[test]
    fn equality_boundaries_fold_or_fall_back() {
        // k = 2048 encodes as addiw x, -2048; k = -2048 needs +2048 (not
        // encodable) and wrapping_neg(i32::MIN) = i32::MIN is rejected, so
        // both fall back to the register form.
        let asm = compile_constant_binary(BinaryOp::Eq, 2048);
        assert!(asm.contains("addiw "), "{asm}");
        assert!(asm.contains("-2048"), "{asm}");
        assert!(asm.contains("seqz "), "{asm}");

        let asm = compile_constant_binary(BinaryOp::Eq, -2048);
        assert!(!asm.contains("addiw "), "{asm}");
        assert!(asm.contains("subw "), "{asm}");
        assert!(asm.contains("seqz "), "{asm}");

        let asm = compile_constant_binary(BinaryOp::Eq, i32::MIN);
        assert!(!asm.contains("addiw "), "{asm}");
        assert!(asm.contains("subw "), "{asm}");
        assert!(asm.contains("seqz "), "{asm}");
    }

    #[test]
    fn shift_amount_boundaries_fold_or_fall_back() {
        // Small amounts fold to the W-class immediate forms.
        let asm = compile_constant_binary(BinaryOp::Shl, 2);
        assert!(asm.contains("slliw "), "{asm}");
        assert!(!asm.contains("sllw "), "{asm}");

        let asm = compile_constant_binary(BinaryOp::Shr, 2);
        assert!(asm.contains("srliw "), "{asm}");
        assert!(!asm.contains("srlw "), "{asm}");

        let asm = compile_constant_binary(BinaryOp::Sar, 2);
        assert!(asm.contains("sraiw "), "{asm}");
        assert!(!asm.contains("sraw "), "{asm}");
        assert!(!asm.contains("li "), "{asm}");

        // Encodable upper bound: 31 (W class). 32 and negative amounts keep
        // the register form, which preserves the hardware's low-bit masking.
        let asm = compile_constant_binary(BinaryOp::Sar, 31);
        assert!(asm.contains("sraiw "), "{asm}");
        assert!(!asm.contains("sraw "), "{asm}");

        let asm = compile_constant_binary(BinaryOp::Sar, 32);
        assert!(asm.contains("sraw "), "{asm}");
        assert!(!asm.contains("sraiw "), "{asm}");

        let asm = compile_constant_binary(BinaryOp::Shl, -1);
        assert!(asm.contains("sllw "), "{asm}");
        assert!(!asm.contains("slliw "), "{asm}");
    }

    #[test]
    fn zero_shift_is_an_identity_alias() {
        // `srliw rd, rs, 0` would zero-extend a negative i32; aliasing keeps
        // it sign-extended.
        for op in [BinaryOp::Shl, BinaryOp::Shr, BinaryOp::Sar] {
            let asm = compile_constant_binary(op, 0);
            assert!(!asm.contains("slliw "), "{op:?}:\n{asm}");
            assert!(!asm.contains("sllw "), "{op:?}:\n{asm}");
            assert!(!asm.contains("srliw "), "{op:?}:\n{asm}");
            assert!(!asm.contains("sraw "), "{op:?}:\n{asm}");
            assert!(!asm.contains("li "), "{op:?}:\n{asm}");
        }
    }

    #[test]
    fn select_uses_word_mask_for_i32() {
        let ops = select_alu_ops(&HirType::get_i32()).unwrap();
        assert_eq!(ops.sub, AluRRROP::SubW);
        assert_eq!(ops.xor, AluRRROP::Xor);
        assert_eq!(ops.and, AluRRROP::And);
    }

    #[test]
    fn select_uses_full_width_mask_for_pointer_and_string() {
        for ty in [
            HirType::get_pointer(HirType::get_i32()),
            HirType::get_string(),
        ] {
            let ops = select_alu_ops(&ty).unwrap();
            assert_eq!(ops.sub, AluRRROP::Sub);
            assert_eq!(ops.xor, AluRRROP::Xor);
            assert_eq!(ops.and, AluRRROP::And);
        }
    }

    #[test]
    fn select_uses_word_mask_for_f32_bits() {
        let ops = select_alu_ops(&HirType::get_f32()).unwrap();
        assert_eq!(ops.sub, AluRRROP::SubW);
        assert_eq!(ops.xor, AluRRROP::Xor);
        assert_eq!(ops.and, AluRRROP::And);
    }

    #[test]
    fn lowers_noncanonical_i32_select_to_branchless_mask_sequence() {
        let mut program = Program::new();
        let function = program.new_function(HirType::get_i32(), "choose".to_string(), Vec::new());
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();

        let cond = data.new_local_inst().integer(2);
        let if_true = data.new_local_inst().integer(10);
        let if_false = data.new_local_inst().integer(20);
        let select = data.new_local_inst().select(cond, if_true, if_false);
        data.layout_mut().insert_inst(entry, select);
        let ret = data.new_local_inst().ret(Some(select));
        data.layout_mut().insert_inst(entry, ret);

        let asm = taki_mir::compile::<Riscv64Backend>(&program);
        assert!(asm.contains("snez "), "{asm}");
        assert!(asm.contains("subw "), "{asm}");
        assert!(asm.contains("and "), "{asm}");
        assert_eq!(asm.matches("xor ").count(), 2, "{asm}");
        assert!(!asm.contains("beqz "), "{asm}");
        assert!(!asm.contains("bnez "), "{asm}");
    }

    #[test]
    fn lowers_pointer_select_with_full_width_mask() {
        let mut program = Program::new();
        let pointer_ty = HirType::get_pointer(HirType::get_i32());
        let function =
            program.new_function(pointer_ty.clone(), "choose_pointer".to_string(), Vec::new());
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();

        let cond = data.new_local_inst().integer(2);
        let if_true = data.new_local_inst().alloc(HirType::get_i32());
        let if_false = data.new_local_inst().alloc(HirType::get_i32());
        data.layout_mut().insert_inst(entry, if_true);
        data.layout_mut().insert_inst(entry, if_false);
        let select = data.new_local_inst().select(cond, if_true, if_false);
        data.layout_mut().insert_inst(entry, select);
        let ret = data.new_local_inst().ret(Some(select));
        data.layout_mut().insert_inst(entry, ret);

        let asm = taki_mir::compile::<Riscv64Backend>(&program);
        assert!(asm.contains("snez "), "{asm}");
        assert!(asm.contains("sub "), "{asm}");
        assert!(!asm.contains("subw "), "{asm}");
        assert!(!asm.contains("beqz "), "{asm}");
        assert!(!asm.contains("bnez "), "{asm}");
    }

    #[test]
    fn lowers_f32_select_through_integer_bit_mask() {
        let mut program = Program::new();
        let function =
            program.new_function(HirType::get_f32(), "choose_float".to_string(), Vec::new());
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();

        let cond = data.new_local_inst().integer(-2);
        let if_true = data.new_local_inst().float(1.5);
        let if_false = data.new_local_inst().float(-0.0);
        let select = data.new_local_inst().select(cond, if_true, if_false);
        data.layout_mut().insert_inst(entry, select);
        let ret = data.new_local_inst().ret(Some(select));
        data.layout_mut().insert_inst(entry, ret);

        let asm = taki_mir::compile::<Riscv64Backend>(&program);
        assert_eq!(asm.matches("fmv.x.w ").count(), 2, "{asm}");
        assert!(asm.contains("fmv.w.x "), "{asm}");
        assert!(asm.contains("snez "), "{asm}");
        assert!(asm.contains("subw "), "{asm}");
        assert_eq!(asm.matches("xor ").count(), 2, "{asm}");
        assert!(!asm.contains("beqz "), "{asm}");
        assert!(!asm.contains("bnez "), "{asm}");
    }

    #[test]
    fn lowers_small_mem_zero_to_inline_stores() {
        let mut program = Program::new();
        let function = program.new_function(HirType::get_unit(), "clear".to_string(), Vec::new());
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();

        let alloc = data
            .new_local_inst()
            .alloc(HirType::get_array(HirType::get_i32(), 4));
        let clear = data.new_local_inst().mem_zero(alloc, 16);
        data.layout_mut().insert_inst(entry, clear);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let asm = taki_mir::compile::<Riscv64Backend>(&program);
        assert!(!asm.contains("call memset"), "{asm}");
        assert_eq!(asm.matches("sw zero").count(), 4, "{asm}");
    }

    #[test]
    fn single_use_constant_gep_folds_into_store_addressing() {
        let mut program = Program::new();
        let function = program.new_function(
            HirType::get_i32(),
            "gep_fold".to_string(),
            vec![HirType::get_pointer(HirType::get_i32())],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();

        let base = data.params()[0];
        let one = data.new_local_inst().integer(1);
        let gep = data.new_local_inst().get_elem_ptr(base, vec![one]);
        let forty_two = data.new_local_inst().integer(42);
        let store = data.new_local_inst().store(forty_two, gep);
        let zero = data.new_local_inst().integer(0);
        for inst in [gep, store] {
            data.layout_mut().insert_inst(entry, inst);
        }
        let ret = data.new_local_inst().ret(Some(zero));
        data.layout_mut().insert_inst(entry, ret);

        let asm = taki_mir::compile::<Riscv64Backend>(&program);
        // The GEP address addi is sunk; the store uses the folded offset
        // directly (`sw rs, 4(base)`).
        assert!(asm.contains(", 4("), "{asm}");
    }

    #[test]
    fn multi_use_gep_is_not_folded() {
        let mut program = Program::new();
        let function = program.new_function(
            HirType::get_i32(),
            "gep_multi".to_string(),
            vec![HirType::get_pointer(HirType::get_i32())],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();

        let base = data.params()[0];
        let one = data.new_local_inst().integer(1);
        let gep = data.new_local_inst().get_elem_ptr(base, vec![one]);
        let forty_two = data.new_local_inst().integer(42);
        let store = data.new_local_inst().store(forty_two, gep);
        let load = data.new_local_inst().load(gep);
        for inst in [gep, store, load] {
            data.layout_mut().insert_inst(entry, inst);
        }
        let ret = data.new_local_inst().ret(Some(load));
        data.layout_mut().insert_inst(entry, ret);

        let asm = taki_mir::compile::<Riscv64Backend>(&program);
        // Two users: the GEP must be materialized, so no direct-offset store.
        assert!(!asm.contains(", 4("), "{asm}");
    }

    #[test]
    fn sign_extended_i32_index_skips_extension_addw() {
        let mut program = Program::new();
        let function = program.new_function(
            HirType::get_i32(),
            "gep_ext_skip".to_string(),
            vec![HirType::get_pointer(HirType::get_i32())],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let two = data.new_local_inst().integer(2);
        let three = data.new_local_inst().integer(3);
        let index = data.new_local_inst().binary(BinaryOp::Add, two, three);
        let gep = data.new_local_inst().get_elem_ptr(base, vec![index]);
        let load = data.new_local_inst().load(gep);
        for inst in [index, gep, load] {
            data.layout_mut().insert_inst(entry, inst);
        }
        let ret = data.new_local_inst().ret(Some(load));
        data.layout_mut().insert_inst(entry, ret);

        let asm = taki_mir::compile::<Riscv64Backend>(&program);
        // addw sign-extends, so the dynamic index needs no extension.
        assert!(asm.contains("slli"), "{asm}");
        // No `addw rd, rd, zero` extension (zero appears nowhere here).
        assert!(!asm.contains("zero"), "{asm}");
    }

    #[test]
    fn loaded_i32_index_skips_extension_addw() {
        let mut program = Program::new();
        let function = program.new_function(
            HirType::get_i32(),
            "gep_ext_load".to_string(),
            vec![
                HirType::get_pointer(HirType::get_i32()),
                HirType::get_pointer(HirType::get_i32()),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let src = data.params()[1];
        let index = data.new_local_inst().load(src);
        let gep = data.new_local_inst().get_elem_ptr(base, vec![index]);
        let load = data.new_local_inst().load(gep);
        for inst in [index, gep, load] {
            data.layout_mut().insert_inst(entry, inst);
        }
        let ret = data.new_local_inst().ret(Some(load));
        data.layout_mut().insert_inst(entry, ret);

        let asm = taki_mir::compile::<Riscv64Backend>(&program);
        // lw sign-extends, so the loaded index needs no extension.
        assert!(asm.contains("slli"), "{asm}");
        assert!(!asm.contains("addw"), "{asm}");
    }

    #[test]
    fn parameter_index_keeps_extension_addw() {
        let mut program = Program::new();
        let function = program.new_function(
            HirType::get_i32(),
            "gep_ext_param".to_string(),
            vec![HirType::get_pointer(HirType::get_i32()), HirType::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let index = data.params()[1];
        let gep = data.new_local_inst().get_elem_ptr(base, vec![index]);
        let load = data.new_local_inst().load(gep);
        for inst in [gep, load] {
            data.layout_mut().insert_inst(entry, inst);
        }
        let ret = data.new_local_inst().ret(Some(load));
        data.layout_mut().insert_inst(entry, ret);

        let asm = taki_mir::compile::<Riscv64Backend>(&program);
        // ABI args are not guaranteed sign-extended: extension must stay.
        assert!(asm.contains("addw"), "{asm}");
    }

    /// Compiles `x <op> constant` and returns the assembly.
    fn compile_constant_comparison(op: BinaryOp, constant: i32) -> String {
        let mut program = Program::new();
        let function = program.new_function(
            HirType::get_i32(),
            "cmp_const".to_string(),
            vec![HirType::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let x = data.params()[0];
        let c = data.new_local_inst().integer(constant);
        let cmp = data.new_local_inst().binary(op, x, c);
        data.layout_mut().insert_inst(entry, cmp);
        let ret = data.new_local_inst().ret(Some(cmp));
        data.layout_mut().insert_inst(entry, ret);
        taki_mir::compile::<Riscv64Backend>(&program)
    }

    #[test]
    fn constant_comparison_folds_to_slti() {
        // x < 5: one slti, no materialized constant.
        let asm = compile_constant_comparison(BinaryOp::Lt, 5);
        assert!(asm.contains("slti"), "{asm}");
        assert!(!asm.contains("\n    li "), "{asm}");
        // x <= 5 == x < 6: slti with the shifted constant, no xori, no li.
        let asm = compile_constant_comparison(BinaryOp::Le, 5);
        assert!(asm.contains("slti"), "{asm}");
        assert!(!asm.contains("xori"), "{asm}");
        assert!(!asm.contains("\n    li "), "{asm}");
        // x >= 5 == !(x < 5): slti + xori, no li.
        let asm = compile_constant_comparison(BinaryOp::Ge, 5);
        assert!(asm.contains("slti"), "{asm}");
        assert!(asm.contains("xori"), "{asm}");
        assert!(!asm.contains("\n    li "), "{asm}");
        // x > 5 == !(x < 6): slti(c+1) + xori, no li.
        let asm = compile_constant_comparison(BinaryOp::Gt, 5);
        assert!(asm.contains("slti"), "{asm}");
        assert!(asm.contains("xori"), "{asm}");
        assert!(!asm.contains("\n    li "), "{asm}");
    }

    #[test]
    fn large_constant_comparison_falls_back() {
        // 4096 is not a 12-bit immediate: keep li + slt.
        let asm = compile_constant_comparison(BinaryOp::Lt, 4096);
        assert!(!asm.contains("slti"), "{asm}");
        assert!(asm.contains("slt "), "{asm}");
        assert!(asm.contains("\n    li "), "{asm}");
    }

    /// Compiles `x <op> constant` and returns the assembly.
    fn compile_constant_binary_imm(op: BinaryOp, constant: i32) -> String {
        let mut program = Program::new();
        let function = program.new_function(
            HirType::get_i32(),
            "bin_const".to_string(),
            vec![HirType::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let x = data.params()[0];
        let c = data.new_local_inst().integer(constant);
        let bin = data.new_local_inst().binary(op, x, c);
        data.layout_mut().insert_inst(entry, bin);
        let ret = data.new_local_inst().ret(Some(bin));
        data.layout_mut().insert_inst(entry, ret);
        taki_mir::compile::<Riscv64Backend>(&program)
    }

    #[test]
    fn constant_binary_folds_to_immediate() {
        // add: addi, no materialized constant.
        let asm = compile_constant_binary(BinaryOp::Add, 5);
        assert!(asm.contains("addi"), "{asm}");
        assert!(!asm.contains("\n    li "), "{asm}");
        // sub(x, 5) == addi(x, -5).
        let asm = compile_constant_binary(BinaryOp::Sub, 5);
        assert!(asm.contains("addi"), "{asm}");
        assert!(!asm.contains("\n    li "), "{asm}");
        // and: andi.
        let asm = compile_constant_binary_imm(BinaryOp::And, 0xF);
        assert!(asm.contains("andi"), "{asm}");
        assert!(!asm.contains("\n    li "), "{asm}");
        // or: ori.
        let asm = compile_constant_binary_imm(BinaryOp::Or, 0xF);
        assert!(asm.contains("ori"), "{asm}");
        assert!(!asm.contains("\n    li "), "{asm}");
        // shl: slli.
        let asm = compile_constant_binary_imm(BinaryOp::Shl, 3);
        assert!(asm.contains("slli"), "{asm}");
        assert!(!asm.contains("\n    li "), "{asm}");
        // eq: addiw(x, -5) + seqz, no sub, no li.
        let asm = compile_constant_binary_imm(BinaryOp::Eq, 5);
        assert!(asm.contains("addiw"), "{asm}");
        assert!(asm.contains("seqz"), "{asm}");
        assert!(!asm.contains("\n    sub"), "{asm}");
        assert!(!asm.contains("\n    li "), "{asm}");
    }

    #[test]
    fn large_constant_binary_falls_back() {
        // 4096 is not a 12-bit immediate: keep li + addw.
        let asm = compile_constant_binary_imm(BinaryOp::Add, 4096);
        assert!(!asm.contains("addi a"), "{asm}");
        assert!(asm.contains("li a1, 0x1000"), "{asm}");
        assert!(asm.contains("addw"), "{asm}");
        // sub(x, -2048) would need addi #2048, which is not encodable.
        let asm = compile_constant_binary_imm(BinaryOp::Sub, -2048);
        assert!(!asm.contains("addi a"), "{asm}");
        assert!(asm.contains("\n    li "), "{asm}");
        // shift amounts beyond 31 are not folded.
        let asm = compile_constant_binary_imm(BinaryOp::Shl, 32);
        assert!(!asm.contains("slli"), "{asm}");
    }

    #[test]
    fn constant_mul_folds_to_shift_sequence() {
        // x * 8 = slli #3: no li, no mulw.
        let asm = compile_constant_binary_imm(BinaryOp::Mul, 8);
        assert!(asm.contains("slli"), "{asm}");
        assert!(!asm.contains("\n    li "), "{asm}");
        assert!(!asm.contains("mulw"), "{asm}");
        // x * 3 = (x << 1) + x: slli + addw.
        let asm = compile_constant_binary_imm(BinaryOp::Mul, 3);
        assert!(asm.contains("slli"), "{asm}");
        assert!(asm.contains("addw"), "{asm}");
        assert!(!asm.contains("\n    li "), "{asm}");
        assert!(!asm.contains("mulw"), "{asm}");
        // x * 7 = (x << 3) - x: slli + subw.
        let asm = compile_constant_binary_imm(BinaryOp::Mul, 7);
        assert!(asm.contains("slli"), "{asm}");
        assert!(asm.contains("subw"), "{asm}");
        assert!(!asm.contains("\n    li "), "{asm}");
        assert!(!asm.contains("mulw"), "{asm}");
        // x * 1: the operand itself.
        let asm = compile_constant_binary_imm(BinaryOp::Mul, 1);
        assert!(!asm.contains("\n    li "), "{asm}");
        assert!(!asm.contains("mulw"), "{asm}");
        // x * 6 is not 2^n or 2^n +/- 1: fall back to li + mulw.
        let asm = compile_constant_binary_imm(BinaryOp::Mul, 6);
        assert!(asm.contains("mulw"), "{asm}");
        assert!(asm.contains("\n    li "), "{asm}");
    }
}
