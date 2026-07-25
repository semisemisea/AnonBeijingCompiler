use raana_ir::ir::{
    Binary, BinaryOp, Call, Cast, GetElemPtr, InstKind, Load, Return, Select, Store,
    Type as HirType, TypeKind as HirTypeKind, arena::Arena, inst_kind::MemZero,
};
use smallvec::smallvec;

use crate::{
    abi::{DEFAULT_CLOBBERS, Riscv64ABI},
    instructions::{
        AMode, AluRRImm12OP, AluRRImmShiftOP, AluRRROP, FcvtMode, FpuRRROP, Imm12, LoadOP, MInst,
        ShiftImm, ShiftImm64, StoreOP,
    },
    labels::Label,
    regs::{ARG_REG, FARG_REG, a0, a1, a2, fa0, fp_reg, preg_name, stack_reg, zero_reg},
};

use taki_mir::{
    abi::{ABIMachineSpec, CallArgPair, CallRetPair, RetPair, StackAMode},
    block_order::LoweredBlock,
    libcall::LibCall,
    lower::{LowerBackend, LowerContext, LoweredOutput, analyze_gep},
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

fn lower_binary(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    binary: &Binary,
) -> LoweredOutput {
    let bop = binary.op();
    let lhs = ctx.put_value_in_reg(binary.lhs());
    let def = ctx.result_reg(inst);
    let rd = Writable::from_reg(def);
    let inst_ty = arena.inst_data(inst).ty();
    let is_float = matches!(inst_ty.kind(), HirTypeKind::Float32)
        || matches!(
            arena.inst_data(binary.lhs()).ty().kind(),
            HirTypeKind::Float32
        );

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
            ctx.emit(MInst::AluRRImm12 {
                op: AluRRImm12OP::Xori,
                rd,
                rs: equal,
                imm: Imm12::ONE,
            });
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
            })
        {
            return LoweredOutput::Value(def);
        }
        let rhs = ctx.put_value_in_reg(binary.rhs());
        let op = (!matches!(bop, BinaryOp::Eq | BinaryOp::NotEq))
            .then(|| alu_op_for_hir_binary(bop, inst_ty));
        let sub_op = alu_op_for_hir_binary(BinaryOp::Sub, inst_ty);
        match bop {
            BinaryOp::NotEq => {
                let difference = ctx.alloc_tmp(HirType::get_i32());
                ctx.emit(MInst::AluRRR {
                    op: sub_op,
                    rd: Writable::from_reg(difference),
                    rs1: lhs,
                    rs2: rhs,
                });
                ctx.emit(MInst::AluRRR {
                    op: AluRRROP::Snez,
                    rd,
                    rs1: difference,
                    rs2: zero_reg(),
                });
            }
            BinaryOp::Eq => {
                let difference = ctx.alloc_tmp(HirType::get_i32());
                ctx.emit(MInst::AluRRR {
                    op: sub_op,
                    rd: Writable::from_reg(difference),
                    rs1: lhs,
                    rs2: rhs,
                });
                ctx.emit(MInst::AluRRR {
                    op: AluRRROP::Seqz,
                    rd,
                    rs1: difference,
                    rs2: zero_reg(),
                });
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
    let offset = ctx.vcode.vcode.abi.alloc_stackslot_or_get(inst, pointee_ty) as i64;
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
        let extended = ctx.alloc_tmp(pointer_ty.clone());
        ctx.emit(MInst::AluRRR {
            op: AluRRROP::AddW,
            rd: Writable::from_reg(extended),
            rs1: index,
            rs2: zero_reg(),
        });

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
    store: &Store,
) -> LoweredOutput {
    let src = store.src();
    let dst = store.dest();

    if let InstKind::Aggregate(agg) = arena.inst_data(src).kind() {
        let elems = agg.flatten(&arena);
        assert!(matches!(arena.inst_data(dst).kind(), InstKind::Alloc));
        let dst_pointee = arena.inst_data(dst).ty().derefernce();
        let base_offset = ctx.vcode.vcode.abi.alloc_stackslot_or_get(dst, dst_pointee) as i64;
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
        let base_offset = ctx.vcode.vcode.abi.alloc_stackslot_or_get(dst, dst_pointee) as i64;
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
                    let addr = ctx.put_value_in_reg(dst);
                    ctx.emit(MInst::StoreWord {
                        rs,
                        op,
                        addr: AMode::RegOffest(addr, 0),
                    });
                }
                InstKind::Alloc => {
                    let pointee_ty = arena.inst_data(dst).ty().derefernce();
                    let offset = ctx.vcode.vcode.abi.alloc_stackslot_or_get(dst, pointee_ty);
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
    let inline_store_count = mem_zero.byte_len() / 4;
    if mem_zero.byte_len() % 4 == 0 && inline_store_count <= INLINE_MEMZERO_MAX_STORES {
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
    let byte_len = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
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
        rd: Writable::from_reg(byte_len),
        value: mem_zero.byte_len() as u64,
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
    ctx.vcode.vcode.abi.set_has_calls();
    ctx.vcode.vcode.abi.set_outgoing_arg_size(0);
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
        // relative pointer.
        let rs = ctx.put_value_in_reg(src);
        ctx.emit(MInst::LoadWord {
            rd,
            op,
            addr: AMode::RegOffest(rs, 0),
        });
    } else {
        // For SysY, this branch only happen when SSA is disabled.
        // All load from integer/float is translated into SSA from.
        let alloc_ty = arena.inst_data(src).ty().derefernce();
        let offset = ctx.vcode.vcode.abi.alloc_stackslot_or_get(src, alloc_ty);
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
    let mut outgoing_arg_size = 0usize;
    let mut call_arg_pairs = smallvec![];
    let mut int_arg_idx = 0;
    let mut float_arg_idx = 0;
    for &arg in call.args() {
        let arg_reg = ctx.put_value_in_reg(arg);
        let arg_ty = arena.inst_data(arg).ty();
        let m_type: LoweredType = arg_ty.into();
        match arg_ty.kind() {
            HirTypeKind::Int32 | HirTypeKind::Pointer(_) => {
                if int_arg_idx < 8 {
                    call_arg_pairs.push(CallArgPair {
                        vreg: arg_reg,
                        preg: ARG_REG[int_arg_idx],
                    });
                    int_arg_idx += 1;
                } else {
                    let op: StoreOP = m_type.into();
                    let addr = normalize_amode(AMode::OutgoingArg(outgoing_arg_size as i64), ctx);
                    ctx.emit(MInst::StoreWord {
                        rs: arg_reg,
                        op,
                        addr,
                    });
                    outgoing_arg_size += arg_ty.size();
                }
            }
            HirTypeKind::Float32 => {
                if float_arg_idx < 8 {
                    call_arg_pairs.push(CallArgPair {
                        vreg: arg_reg,
                        preg: FARG_REG[float_arg_idx],
                    });
                    float_arg_idx += 1;
                } else {
                    let op: StoreOP = m_type.into();
                    let addr = normalize_amode(AMode::OutgoingArg(outgoing_arg_size as i64), ctx);
                    ctx.emit(MInst::StoreWord {
                        rs: arg_reg,
                        op,
                        addr,
                    });
                    outgoing_arg_size += arg_ty.size();
                }
            }
            _ => unreachable!("unexpected call argument type: {:?}", arg_ty.kind()),
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
    ctx.vcode.vcode.abi.set_has_calls();
    ctx.vcode.vcode.abi.set_outgoing_arg_size(outgoing_arg_size);
    result.map_or(LoweredOutput::None, LoweredOutput::Value)
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
    fn lower(ctx: &mut LowerContext<Self::MInst>, inst: HirInst) -> LoweredOutput {
        let arena = ctx.arena;
        match arena.inst_data(inst).kind() {
            InstKind::BlockArgRef(..)
            | InstKind::FuncArgRef(..)
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
            InstKind::Store(store) => lower_store(ctx, arena, store),
            InstKind::MemZero(mem_zero) => lower_mem_zero(ctx, arena, mem_zero),
            InstKind::Load(load) => lower_load(ctx, arena, inst, load),
            InstKind::Call(call) => lower_call(ctx, arena, inst, call),
            InstKind::Return(ret) => lower_return(ctx, arena, ret),
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
                let cond = branch.cond();
                let cond = ctx.put_value_in_reg(cond);
                let t_args = branch.t_args();
                let f_args = branch.f_args();
                for &arg in t_args.iter().chain(f_args) {
                    ctx.put_value_in_reg(arg);
                }
                let &[t_target, f_target] = target else {
                    unreachable!()
                };
                ctx.emit(MInst::CondBr {
                    cond,
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
}

#[cfg(test)]
mod tests {
    use super::signed_power_of_two;

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
}
