use raana_ir::ir::{BinaryOp, InstKind, Type as HirType, TypeKind as HirTypeKind, arena::Arena};
use smallvec::smallvec;

use crate::{
    abi::{DEFAULT_CLOBBERS, Riscv64ABI},
    instructions::{
        AMode, AluRRImm12OP, AluRRImmShiftOP, AluRRROP, FpuRRROP, Imm12, LoadOP, MInst, ShiftImm,
        StoreOP,
    },
    labels::Label,
    regs::{ARG_REG, FARG_REG, a0, fa0, fp_reg, preg_name, stack_reg, zero_reg},
};

use taki_mir::{
    abi::{ABIMachineSpec, ArgPair, CallArgPair, CallRetPair, RetPair, StackAMode},
    block_order::LoweredBlock,
    lower::{CodegenError, LowerBackend, LowerContext},
    prelude::HirFunctionData,
    reg_alloc::reg::PReg,
    register::Writable,
    types::LoweredType,
};

fn normalize_amode(amode: &AMode, ctx: &mut LowerContext<'_, MInst>) -> AMode {
    // Slot offsets need the final outgoing-argument-area displacement, which
    // is unavailable during lowering. Keep them symbolic for ABI legalization.
    if matches!(amode, AMode::SlotOffset(_)) {
        return amode.clone();
    }
    let (off, base) = match *amode {
        AMode::SPOffset(o) | AMode::OutgoingArg(o) => (o, stack_reg()),
        AMode::FPOffset(o) | AMode::IncomingArg(o) => (o, fp_reg()),
        _ => return amode.clone(),
    };
    if (-2048..2048).contains(&off) {
        return amode.clone();
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

fn integer_constant(
    ctx: &taki_mir::lower::LowerContext<'_, MInst>,
    inst: raana_ir::opt::prelude::Inst,
) -> Option<i32> {
    match ctx.arena.inst_data(inst).kind() {
        raana_ir::ir::InstKind::Integer(value) => Some(value.value()),
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
    ctx: &mut taki_mir::lower::LowerContext<'_, MInst>,
    op: BinaryOp,
    rd: Writable<taki_mir::register::Reg>,
    lhs: taki_mir::register::Reg,
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

pub struct Riscv64Backend;

impl LowerBackend for Riscv64Backend {
    type MInst = MInst;
    fn lower(
        ctx: &mut taki_mir::lower::LowerContext<Self::MInst>,
        inst: raana_ir::opt::prelude::Inst,
    ) -> Result<(), CodegenError> {
        let func_data = ctx.arena.program.func_data(ctx.arena.curr_func.unwrap());
        let inst_data = func_data.inst_data(inst);
        match inst_data.kind() {
            raana_ir::ir::InstKind::BlockArgRef(..)
            | raana_ir::ir::InstKind::FuncArgRef(..)
            | raana_ir::ir::InstKind::Aggregate(..)
            | raana_ir::ir::InstKind::GlobalAlloc(..)
            | raana_ir::ir::InstKind::Undef
            | raana_ir::ir::InstKind::ZeroInit
            | raana_ir::ir::InstKind::Integer(..)
            | raana_ir::ir::InstKind::Float(..) => {
                // Should not in layout for now.
                // Even when changed we don't have to do anything
                // Because these instruction are just defining a value.
                unreachable!("currently constants are not in layout")
            }
            raana_ir::ir::InstKind::Binary(binary) => {
                let bop = binary.op();
                let lhs = ctx.put_value_in_reg(binary.lhs());
                let def = *ctx.reg_map.get(&inst).unwrap();
                let rd = Writable::from_reg(def);

                let lhs_ty = ctx.arena.inst_data(binary.lhs()).ty();
                let is_float = matches!(inst_data.ty().kind(), raana_ir::ir::TypeKind::Float32)
                    || matches!(lhs_ty.kind(), raana_ir::ir::TypeKind::Float32);

                if is_float {
                    let rhs = ctx.put_value_in_reg(binary.rhs());
                    use crate::instructions::FpuRRROP;
                    match bop {
                        raana_ir::ir::BinaryOp::Add => {
                            ctx.emit(MInst::FpuRRR {
                                op: FpuRRROP::FaddS,
                                rd,
                                rs1: lhs,
                                rs2: rhs,
                            });
                        }
                        raana_ir::ir::BinaryOp::Sub => {
                            ctx.emit(MInst::FpuRRR {
                                op: FpuRRROP::FsubS,
                                rd,
                                rs1: lhs,
                                rs2: rhs,
                            });
                        }
                        raana_ir::ir::BinaryOp::Mul => {
                            ctx.emit(MInst::FpuRRR {
                                op: FpuRRROP::FmulS,
                                rd,
                                rs1: lhs,
                                rs2: rhs,
                            });
                        }
                        raana_ir::ir::BinaryOp::Div => {
                            ctx.emit(MInst::FpuRRR {
                                op: FpuRRROP::FdivS,
                                rd,
                                rs1: lhs,
                                rs2: rhs,
                            });
                        }
                        raana_ir::ir::BinaryOp::Lt => {
                            ctx.emit(MInst::FpuRRR {
                                op: FpuRRROP::FltS,
                                rd,
                                rs1: lhs,
                                rs2: rhs,
                            });
                        }
                        raana_ir::ir::BinaryOp::Gt => {
                            ctx.emit(MInst::FpuRRR {
                                op: FpuRRROP::FltS,
                                rd,
                                rs1: rhs,
                                rs2: lhs,
                            });
                        }
                        raana_ir::ir::BinaryOp::Le => {
                            ctx.emit(MInst::FpuRRR {
                                op: FpuRRROP::FleS,
                                rd,
                                rs1: lhs,
                                rs2: rhs,
                            });
                        }
                        raana_ir::ir::BinaryOp::Ge => {
                            ctx.emit(MInst::FpuRRR {
                                op: FpuRRROP::FleS,
                                rd,
                                rs1: rhs,
                                rs2: lhs,
                            });
                        }
                        raana_ir::ir::BinaryOp::Eq => {
                            ctx.emit(MInst::FpuRRR {
                                op: FpuRRROP::FeqS,
                                rd,
                                rs1: lhs,
                                rs2: rhs,
                            });
                        }
                        raana_ir::ir::BinaryOp::NotEq => {
                            ctx.emit(MInst::FpuRRR {
                                op: FpuRRROP::FeqS,
                                rd,
                                rs1: lhs,
                                rs2: rhs,
                            });
                            ctx.emit(MInst::AluRRImm12 {
                                op: crate::instructions::AluRRImm12OP::Xori,
                                rd,
                                rs: def,
                                imm: Imm12::ONE,
                            });
                        }
                        _ => unreachable!("unexpected float binary op: {:?}", bop),
                    }
                } else {
                    if matches!(inst_data.ty().kind(), HirTypeKind::Int32)
                        && matches!(bop, BinaryOp::Div | BinaryOp::Rem)
                        && integer_constant(ctx, binary.rhs()).is_some_and(|divisor| {
                            lower_signed_div_rem_power_of_two(ctx, bop, rd, lhs, divisor)
                        })
                    {
                        return Ok(());
                    }
                    let rhs = ctx.put_value_in_reg(binary.rhs());
                    let op = (!matches!(bop, BinaryOp::Eq | BinaryOp::NotEq))
                        .then(|| alu_op_for_hir_binary(bop, inst_data.ty()));
                    let sub_op = alu_op_for_hir_binary(BinaryOp::Sub, inst_data.ty());
                    match bop {
                        raana_ir::ir::BinaryOp::NotEq => {
                            ctx.emit(MInst::AluRRR {
                                op: sub_op,
                                rd,
                                rs1: lhs,
                                rs2: rhs,
                            });
                            ctx.emit(MInst::AluRRR {
                                op: AluRRROP::Snez,
                                rd,
                                rs1: def,
                                rs2: zero_reg(),
                            });
                        }
                        raana_ir::ir::BinaryOp::Eq => {
                            ctx.emit(MInst::AluRRR {
                                op: sub_op,
                                rd,
                                rs1: lhs,
                                rs2: rhs,
                            });
                            ctx.emit(MInst::AluRRR {
                                op: AluRRROP::Seqz,
                                rd,
                                rs1: def,
                                rs2: zero_reg(),
                            });
                        }
                        raana_ir::ir::BinaryOp::Lt
                        | raana_ir::ir::BinaryOp::Add
                        | raana_ir::ir::BinaryOp::Sub
                        | raana_ir::ir::BinaryOp::Mul
                        | raana_ir::ir::BinaryOp::Div
                        | raana_ir::ir::BinaryOp::Rem
                        | raana_ir::ir::BinaryOp::And
                        | raana_ir::ir::BinaryOp::Or
                        | raana_ir::ir::BinaryOp::Xor
                        | raana_ir::ir::BinaryOp::Shl
                        | raana_ir::ir::BinaryOp::Shr
                        | raana_ir::ir::BinaryOp::Sar => ctx.emit(MInst::AluRRR {
                            op: op.unwrap(),
                            rd,
                            rs1: lhs,
                            rs2: rhs,
                        }),
                        raana_ir::ir::BinaryOp::Gt => {
                            ctx.emit(MInst::AluRRR {
                                op: op.unwrap(),
                                rd,
                                rs1: rhs,
                                rs2: lhs,
                            });
                        }
                        raana_ir::ir::BinaryOp::Ge => {
                            ctx.emit(MInst::AluRRR {
                                op: op.unwrap(),
                                rd,
                                rs1: lhs,
                                rs2: rhs,
                            });
                            ctx.emit(MInst::AluRRImm12 {
                                op: super::instructions::AluRRImm12OP::Xori,
                                rd,
                                rs: def,
                                imm: Imm12::ONE,
                            });
                        }
                        raana_ir::ir::BinaryOp::Le => {
                            ctx.emit(MInst::AluRRR {
                                op: op.unwrap(),
                                rd,
                                rs1: rhs,
                                rs2: lhs,
                            });
                            ctx.emit(MInst::AluRRImm12 {
                                op: super::instructions::AluRRImm12OP::Xori,
                                rd,
                                rs: def,
                                imm: Imm12::ONE,
                            });
                        }
                    }
                }
            }
            raana_ir::ir::InstKind::Cast(cast) => {
                use crate::instructions::FcvtMode;
                let src = cast.src();
                let rs = ctx.put_value_in_reg(src);
                let into_ty = inst_data.ty();
                let def = *ctx.reg_map.get(&inst).unwrap();
                let rd = Writable::from_reg(def);
                match into_ty.kind() {
                    raana_ir::ir::TypeKind::Int32 => {
                        ctx.emit(MInst::Fcvt {
                            mode: FcvtMode::SinglePrecisionToWord,
                            rd,
                            rs,
                        });
                    }
                    raana_ir::ir::TypeKind::Float32 => {
                        ctx.emit(MInst::Fcvt {
                            mode: FcvtMode::WordToSinglePrecision,
                            rd,
                            rs,
                        });
                    }
                    _ => unreachable!("cast only produce i32 or f32"),
                }
            }
            raana_ir::ir::InstKind::Alloc => {
                let def = *ctx.reg_map.get(&inst).unwrap();
                let rd = Writable::from_reg(def);
                let pointee_ty = inst_data.ty().derefernce();
                let offset = ctx.vcode.vcode.abi.alloc_stackslot_or_get(inst, pointee_ty) as i64;
                ctx.emit(<Riscv64ABI as ABIMachineSpec>::gen_get_stack_addr(
                    StackAMode::Slot(offset),
                    rd,
                ));
            }
            raana_ir::ir::InstKind::GetElemPtr(get_elem_ptr) => {
                let indices = get_elem_ptr.offsets();
                let src = get_elem_ptr.base();
                let mut src_ty = ctx.arena.inst_data(src).ty().clone();
                let rs = ctx.put_value_in_reg(src);

                let def = *ctx.reg_map.get(&inst).unwrap();
                let rd = Writable::from_reg(def);
                let tmp = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
                let wtmp = Writable::from_reg(tmp);
                let acc = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
                let wacc = Writable::from_reg(acc);
                ctx.emit(MInst::LoadImm { rd: wacc, value: 0 });
                let mut ty = src_ty;
                for &index in indices {
                    let elem_size = if ty.is_pointer() {
                        let deref = ty.derefernce();
                        let size = deref.size();
                        ty = deref;
                        size
                    } else {
                        let (elem_ty, _len) = ty.get_array_info();
                        let size = elem_ty.size();
                        ty = elem_ty;
                        size
                    };
                    ctx.emit(MInst::LoadImm {
                        rd: wtmp,
                        value: elem_size as u64,
                    });
                    let rhs = ctx.put_value_in_reg(index);
                    ctx.emit(MInst::AluRRR {
                        op: AluRRROP::Mul,
                        rd: wtmp,
                        rs1: tmp,
                        rs2: rhs,
                    });
                    ctx.emit(MInst::AluRRR {
                        op: AluRRROP::Add,
                        rd: wacc,
                        rs1: acc,
                        rs2: tmp,
                    });
                }
                let final_ty = ty.reference();
                assert_eq!(
                    final_ty,
                    *inst_data.ty(),
                    "GEP type mismatch for {:?}: computed={:?}, declared={:?}, base_idx={:?}",
                    inst,
                    final_ty,
                    inst_data.ty(),
                    get_elem_ptr.base(),
                );
                ctx.emit(MInst::AluRRR {
                    op: AluRRROP::Add,
                    rd,
                    rs1: rs,
                    rs2: acc,
                });
            }
            raana_ir::ir::InstKind::Store(store) => {
                let src = store.src();
                let dst = store.dest();

                if let InstKind::Aggregate(agg) = ctx.arena.inst_data(src).kind() {
                    let elems = agg.flatten(&ctx.arena);
                    assert!(matches!(ctx.arena.inst_data(dst).kind(), InstKind::Alloc));
                    let dst_pointee = ctx.arena.inst_data(dst).ty().derefernce();
                    let base_offset = ctx
                        .vcode
                        .vcode
                        .abi
                        .alloc_stackslot_or_get(dst, dst_pointee.clone())
                        as i64;
                    let mut elem_offset: i64 = 0;
                    for elem in elems {
                        let rs = ctx.put_value_in_reg(elem);
                        let elem_ty = ctx.arena.inst_data(elem).ty().clone();
                        let m_type: LoweredType = elem_ty.clone().into();
                        let op: StoreOP = m_type.into();
                        let addr =
                            normalize_amode(&AMode::SlotOffset(base_offset + elem_offset), ctx);
                        ctx.emit(MInst::StoreWord { rs, op, addr });
                        elem_offset += elem_ty.size() as i64;
                    }
                } else if matches!(ctx.arena.inst_data(src).kind(), InstKind::ZeroInit) {
                    let src_ty = ctx.arena.inst_data(src).ty();
                    let dst_pointee = ctx.arena.inst_data(dst).ty().derefernce();
                    let base_offset = ctx
                        .vcode
                        .vcode
                        .abi
                        .alloc_stackslot_or_get(dst, dst_pointee.clone())
                        as i64;
                    let total = src_ty.array_flatten_length();
                    let scalar_ty = src_ty.array_base_scalar_type();
                    let elem_size = scalar_ty.size() as i64;
                    let zero = zero_reg();
                    for i in 0..total {
                        let addr = normalize_amode(
                            &AMode::SlotOffset(base_offset + i as i64 * elem_size),
                            ctx,
                        );
                        ctx.emit(MInst::StoreWord {
                            rs: zero,
                            op: StoreOP::Sw,
                            addr,
                        });
                    }
                } else {
                    let m_type: LoweredType = ctx.arena.inst_data(src).ty().clone().into();
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
                        match ctx.arena.inst_data(dst).kind() {
                            InstKind::GetElemPtr(..) => {
                                let addr = ctx.put_value_in_reg(dst);
                                ctx.emit(MInst::StoreWord {
                                    rs,
                                    op,
                                    addr: AMode::RegOffest(addr, 0),
                                });
                            }
                            InstKind::Alloc => {
                                let pointee_ty = ctx.arena.inst_data(dst).ty().derefernce();
                                let offset =
                                    ctx.vcode.vcode.abi.alloc_stackslot_or_get(dst, pointee_ty);
                                let addr = normalize_amode(&AMode::SlotOffset(offset as i64), ctx);
                                ctx.emit(MInst::StoreWord { rs, op, addr });
                            }
                            _ => {
                                unreachable!(
                                    "should not store in instruction other than GEP or Alloc"
                                )
                            }
                        }
                    }
                }
            }
            raana_ir::ir::InstKind::Load(load) => {
                let src = load.src();
                let src_ty = inst_data.ty().clone();
                let m_type: LoweredType = src_ty.clone().into();
                let op: LoadOP = m_type.into();
                let def = *ctx.reg_map.get(&inst).unwrap();
                let rd = Writable::from_reg(def);
                if src.is_global() {
                    ctx.emit(MInst::LoadWord {
                        rd,
                        op,
                        addr: AMode::Label(Label::GlobalValue(src)),
                    })
                } else if matches!(ctx.arena.inst_data(src).kind(), InstKind::GetElemPtr(..)) {
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
                    let alloc_ty = ctx.arena.inst_data(src).ty().derefernce();
                    let offset = ctx.vcode.vcode.abi.alloc_stackslot_or_get(src, alloc_ty);
                    let addr = normalize_amode(&AMode::SlotOffset(offset as i64), ctx);
                    ctx.emit(MInst::LoadWord { rd, op, addr });
                }
            }
            raana_ir::ir::InstKind::Call(call) => {
                use raana_ir::ir::TypeKind;
                let args = call.args();
                let callee = call.callee();

                let mut outgoing_arg_size = 0usize;
                let mut call_arg_pairs = smallvec![];
                let mut int_arg_idx = 0;
                let mut float_arg_idx = 0;
                for &arg in args {
                    let arg_reg = ctx.put_value_in_reg(arg);
                    let arg_ty = ctx.arena.inst_data(arg).ty().clone();
                    let m_type: LoweredType = arg_ty.clone().into();
                    match arg_ty.kind() {
                        TypeKind::Int32 | TypeKind::Pointer(_) => {
                            if int_arg_idx < 8 {
                                call_arg_pairs.push(CallArgPair {
                                    vreg: arg_reg,
                                    preg: ARG_REG[int_arg_idx],
                                });
                                int_arg_idx += 1;
                            } else {
                                let op: StoreOP = m_type.into();
                                let addr = normalize_amode(
                                    &AMode::OutgoingArg(outgoing_arg_size as i64),
                                    ctx,
                                );
                                ctx.emit(MInst::StoreWord {
                                    rs: arg_reg,
                                    op,
                                    addr,
                                });
                                outgoing_arg_size += arg_ty.size();
                            }
                        }
                        TypeKind::Float32 => {
                            if float_arg_idx < 8 {
                                call_arg_pairs.push(CallArgPair {
                                    vreg: arg_reg,
                                    preg: FARG_REG[float_arg_idx],
                                });
                                float_arg_idx += 1;
                            } else {
                                let op: StoreOP = m_type.into();
                                let addr = normalize_amode(
                                    &AMode::OutgoingArg(outgoing_arg_size as i64),
                                    ctx,
                                );
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
                let ret_arg_pair = match inst_data.ty().kind() {
                    raana_ir::ir::TypeKind::Unit => None,
                    raana_ir::ir::TypeKind::Int32 => Some(CallRetPair {
                        vreg: Writable::from_reg(*ctx.reg_map.get(&inst).unwrap()),
                        preg: a0(),
                    }),
                    raana_ir::ir::TypeKind::Float32 => Some(CallRetPair {
                        vreg: Writable::from_reg(*ctx.reg_map.get(&inst).unwrap()),
                        preg: fa0(),
                    }),
                    _ => unreachable!(),
                };
                ctx.emit(MInst::Call {
                    arg_pairs: call_arg_pairs,
                    ret: ret_arg_pair,
                    clobbers: DEFAULT_CLOBBERS,
                    label: Label::Function(callee),
                });
                ctx.vcode.vcode.abi.set_has_calls();
                ctx.vcode.vcode.abi.set_outgoing_arg_size(outgoing_arg_size);
            }
            raana_ir::ir::InstKind::Return(ret) => {
                if let Some(val) = ret.value() {
                    let preg = match ctx.arena.inst_data(val).ty().kind() {
                        raana_ir::ir::TypeKind::Int32 | raana_ir::ir::TypeKind::Pointer(_) => a0(),
                        raana_ir::ir::TypeKind::Float32 => fa0(),
                        ty => unreachable!("unexpected return type: {ty:?}"),
                    };
                    let src = ctx.put_value_in_reg(val);
                    ctx.emit(MInst::RetVal {
                        pair: RetPair { vreg: src, preg },
                    });
                }
                ctx.emit(MInst::Ret);
            }
            raana_ir::ir::InstKind::Jump(..) | raana_ir::ir::InstKind::Branch(..) => {
                unreachable!("should not lower branch instruction in here.")
            }
        }
        Ok(())
    }

    fn lower_branch(
        ctx: &mut taki_mir::lower::LowerContext<Self::MInst>,
        inst: raana_ir::opt::prelude::Inst,
        target: &[taki_mir::block_order::MirBlockIndex],
    ) -> Result<(), CodegenError> {
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
                let tmp = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
                ctx.emit(MInst::LoadAddr {
                    rd: Writable::from_reg(tmp),
                    label: Label::Block(target),
                });
                ctx.emit(MInst::JumpReg { rs: tmp });
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
                let tmp = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
                ctx.emit(MInst::LongBnez {
                    cond,
                    scratch: Writable::from_reg(tmp),
                    label: Label::Block(t_target),
                });
                let tmp = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
                ctx.emit(MInst::LoadAddr {
                    rd: Writable::from_reg(tmp),
                    label: Label::Block(f_target),
                });
                ctx.emit(MInst::JumpReg { rs: tmp });
            }
            _ => unreachable!("should not lower non-branch isntruction in here."),
        }
        Ok(())
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
        let tmp = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
        ctx.emit(MInst::LoadAddr {
            rd: Writable::from_reg(tmp),
            label: Label::Block(target),
        });
        ctx.emit(MInst::JumpReg { rs: tmp });
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
