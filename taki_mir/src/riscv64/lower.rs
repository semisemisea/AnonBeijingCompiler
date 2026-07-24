use raana_ir::ir::{BinaryOp, InstKind, Type as HirType, TypeKind as HirTypeKind, arena::Arena};
use smallvec::smallvec;

use crate::{
    abi::{ABIMachineSpec, ArgPair, CallArgPair, CallRetPair, RetPair, StackAMode},
    block_order::LoweredBlock,
    libcall::LibCall,
    lower::{CodegenError, LowerBackend, LowerContext},
    prelude::HirFunctionData,
    reg_alloc::reg::PReg,
    register::Writable,
    riscv64::{
        abi::{DEFAULT_CLOBBERS, Riscv64ABI},
        instructions::{AMode, AluRRImm12OP, AluRRROP, FpuRRROP, Imm12, LoadOP, MInst, StoreOP},
        labels::Label,
        regs::{ARG_REG, FARG_REG, a0, a1, a2, fa0, fp_reg, preg_name, stack_reg, zero_reg},
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectAluOps {
    sub: AluRRROP,
    xor: AluRRROP,
    and: AluRRROP,
}

/// Select is implemented as:
///
///   mask = 0 - (cond != 0)
///   result = if_false ^ ((if_true ^ if_false) & mask)
///
/// The subtraction width follows the selected value's representation. In
/// particular, pointers (and RaanaIR strings, which are pointer-sized) require
/// an XLEN mask, while i32 and f32 selects operate on 32-bit words.
fn select_alu_ops(ty: &HirType) -> Option<SelectAluOps> {
    match ty.kind() {
        HirTypeKind::Int32 | HirTypeKind::Float32 => Some(SelectAluOps {
            sub: AluRRROP::SubW,
            xor: AluRRROP::Xor,
            and: AluRRROP::And,
        }),
        HirTypeKind::Pointer(_) | HirTypeKind::String => Some(SelectAluOps {
            sub: AluRRROP::Sub,
            xor: alu_op_for_hir_binary(BinaryOp::Xor, ty),
            and: alu_op_for_hir_binary(BinaryOp::And, ty),
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

pub struct Riscv64Backend;

const INLINE_MEMZERO_MAX_STORES: usize = 4;

impl LowerBackend for Riscv64Backend {
    type MInst = MInst;
    fn lower(
        ctx: &mut crate::lower::LowerContext<Self::MInst>,
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
                let rhs = ctx.put_value_in_reg(binary.rhs());
                let def = *ctx.reg_map.get(&inst).unwrap();
                let rd = Writable::from_reg(def);

                let lhs_ty = ctx.arena.inst_data(binary.lhs()).ty();
                let is_float = matches!(inst_data.ty().kind(), raana_ir::ir::TypeKind::Float32)
                    || matches!(lhs_ty.kind(), raana_ir::ir::TypeKind::Float32);

                if is_float {
                    use crate::riscv64::instructions::FpuRRROP;
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
                            let compare = Writable::from_reg(ctx.alloc_tmp(HirType::get_i32()));
                            ctx.emit(MInst::FpuRRR {
                                op: FpuRRROP::FeqS,
                                rd: compare,
                                rs1: lhs,
                                rs2: rhs,
                            });
                            ctx.emit(MInst::AluRRImm12 {
                                op: super::instructions::AluRRImm12OP::Xori,
                                rd,
                                rs: compare.to_reg(),
                                imm: Imm12::ONE,
                            });
                        }
                        _ => unreachable!("unexpected float binary op: {:?}", bop),
                    }
                } else {
                    let op = (!matches!(bop, BinaryOp::Eq | BinaryOp::NotEq))
                        .then(|| alu_op_for_hir_binary(bop, inst_data.ty()));
                    let sub_op = alu_op_for_hir_binary(BinaryOp::Sub, inst_data.ty());
                    match bop {
                        raana_ir::ir::BinaryOp::NotEq => {
                            let compare = Writable::from_reg(ctx.alloc_tmp(HirType::get_i32()));
                            ctx.emit(MInst::AluRRR {
                                op: sub_op,
                                rd: compare,
                                rs1: lhs,
                                rs2: rhs,
                            });
                            ctx.emit(MInst::AluRRR {
                                op: AluRRROP::Snez,
                                rd,
                                rs1: compare.to_reg(),
                                rs2: zero_reg(),
                            });
                        }
                        raana_ir::ir::BinaryOp::Eq => {
                            let compare = Writable::from_reg(ctx.alloc_tmp(HirType::get_i32()));
                            ctx.emit(MInst::AluRRR {
                                op: sub_op,
                                rd: compare,
                                rs1: lhs,
                                rs2: rhs,
                            });
                            ctx.emit(MInst::AluRRR {
                                op: AluRRROP::Seqz,
                                rd,
                                rs1: compare.to_reg(),
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
                            let compare = Writable::from_reg(ctx.alloc_tmp(HirType::get_i32()));
                            ctx.emit(MInst::AluRRR {
                                op: op.unwrap(),
                                rd: compare,
                                rs1: lhs,
                                rs2: rhs,
                            });
                            ctx.emit(MInst::AluRRImm12 {
                                op: super::instructions::AluRRImm12OP::Xori,
                                rd,
                                rs: compare.to_reg(),
                                imm: Imm12::ONE,
                            });
                        }
                        raana_ir::ir::BinaryOp::Le => {
                            let compare = Writable::from_reg(ctx.alloc_tmp(HirType::get_i32()));
                            ctx.emit(MInst::AluRRR {
                                op: op.unwrap(),
                                rd: compare,
                                rs1: rhs,
                                rs2: lhs,
                            });
                            ctx.emit(MInst::AluRRImm12 {
                                op: super::instructions::AluRRImm12OP::Xori,
                                rd,
                                rs: compare.to_reg(),
                                imm: Imm12::ONE,
                            });
                        }
                    }
                }
            }
            raana_ir::ir::InstKind::Select(select) => {
                let ty = inst_data.ty();
                let Some(ops) = select_alu_ops(ty) else {
                    return Err(ctx.unsupported(
                        "RISC-V lowering",
                        "RaanaIR select supports only i32, f32, and pointer/string results",
                        None,
                        Some(ty),
                    ));
                };

                let cond = ctx.put_value_in_reg(select.cond());
                let if_true = ctx.put_value_in_reg(select.if_true());
                let if_false = ctx.put_value_in_reg(select.if_false());
                let def = *ctx.reg_map.get(&inst).unwrap();
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

                // Do not assume that an i32 condition has already been
                // canonicalized to 0 or 1: any nonzero value selects true.
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
            }
            raana_ir::ir::InstKind::Cast(cast) => {
                use crate::riscv64::instructions::FcvtMode;
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
                let acc = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
                let wacc = Writable::from_reg(acc);
                ctx.emit(MInst::LoadImm { rd: wacc, value: 0 });
                let mut ty = src_ty;
                let mut acc = acc;
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
                    let factor = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
                    ctx.emit(MInst::LoadImm {
                        rd: Writable::from_reg(factor),
                        value: elem_size as u64,
                    });
                    let rhs = ctx.put_value_in_reg(index);
                    let product = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
                    ctx.emit(MInst::AluRRR {
                        op: AluRRROP::Mul,
                        rd: Writable::from_reg(product),
                        rs1: factor,
                        rs2: rhs,
                    });
                    let next_acc = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
                    ctx.emit(MInst::AluRRR {
                        op: AluRRROP::Add,
                        rd: Writable::from_reg(next_acc),
                        rs1: acc,
                        rs2: product,
                    });
                    acc = next_acc;
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
            raana_ir::ir::InstKind::MemZero(mem_zero) => {
                let inline_store_count = mem_zero.byte_len() / 4;
                if mem_zero.byte_len() % 4 == 0 && inline_store_count <= INLINE_MEMZERO_MAX_STORES {
                    let alloc =
                        matches!(ctx.arena.inst_data(mem_zero.dest()).kind(), InstKind::Alloc)
                            .then_some(mem_zero.dest());
                    let (dest, stack_offset) = if let Some(alloc) = alloc {
                        let pointee = ctx.arena.inst_data(alloc).ty().derefernce();
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
                    return Ok(());
                }
                let alloc = matches!(ctx.arena.inst_data(mem_zero.dest()).kind(), InstKind::Alloc)
                    .then_some(mem_zero.dest());
                let (dest, stack_offset) = if let Some(alloc) = alloc {
                    let pointee = ctx.arena.inst_data(alloc).ty().derefernce();
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
                            preg: a0()
                        },
                        CallArgPair {
                            vreg: zero,
                            preg: a1()
                        },
                        CallArgPair {
                            vreg: byte_len,
                            preg: a2()
                        },
                    ],
                    ret: None,
                    clobbers: DEFAULT_CLOBBERS,
                    label: Label::libcall(LibCall::Memset),
                });
                ctx.vcode.vcode.abi.set_has_calls();
                ctx.vcode.vcode.abi.set_outgoing_arg_size(0);
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
        ctx: &mut crate::lower::LowerContext<Self::MInst>,
        inst: raana_ir::opt::prelude::Inst,
        target: &[crate::block_order::MirBlockIndex],
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
        ctx: &mut crate::lower::LowerContext<MInst>,
        target: crate::block_order::MirBlockIndex,
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
    use super::*;
    use raana_ir::ir::{
        Program,
        builder_trait::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
    };

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
        let entry = data
            .new_basic_block()
            .basic_block("entry".to_string(), Vec::new());
        data.layout_mut().push_bb_back(entry);

        let cond = data.new_local_inst().integer(2);
        let if_true = data.new_local_inst().integer(10);
        let if_false = data.new_local_inst().integer(20);
        let select = data.new_local_inst().select(cond, if_true, if_false);
        data.layout_mut().insert_inst(entry, select);
        let ret = data.new_local_inst().ret(Some(select));
        data.layout_mut().insert_inst(entry, ret);

        let asm = crate::compile::<Riscv64Backend>(&program).unwrap();
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
        let entry = data
            .new_basic_block()
            .basic_block("entry".to_string(), Vec::new());
        data.layout_mut().push_bb_back(entry);

        let cond = data.new_local_inst().integer(2);
        let if_true = data.new_local_inst().alloc(HirType::get_i32());
        let if_false = data.new_local_inst().alloc(HirType::get_i32());
        data.layout_mut().insert_inst(entry, if_true);
        data.layout_mut().insert_inst(entry, if_false);
        let select = data.new_local_inst().select(cond, if_true, if_false);
        data.layout_mut().insert_inst(entry, select);
        let ret = data.new_local_inst().ret(Some(select));
        data.layout_mut().insert_inst(entry, ret);

        let asm = crate::compile::<Riscv64Backend>(&program).unwrap();
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
        let entry = data
            .new_basic_block()
            .basic_block("entry".to_string(), Vec::new());
        data.layout_mut().push_bb_back(entry);

        let cond = data.new_local_inst().integer(-2);
        let if_true = data.new_local_inst().float(1.5);
        let if_false = data.new_local_inst().float(-0.0);
        let select = data.new_local_inst().select(cond, if_true, if_false);
        data.layout_mut().insert_inst(entry, select);
        let ret = data.new_local_inst().ret(Some(select));
        data.layout_mut().insert_inst(entry, ret);

        let asm = crate::compile::<Riscv64Backend>(&program).unwrap();
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
        let entry = data
            .new_basic_block()
            .basic_block("entry".to_string(), Vec::new());
        data.layout_mut().push_bb_back(entry);

        let alloc = data
            .new_local_inst()
            .alloc(HirType::get_array(HirType::get_i32(), 4));
        let clear = data.new_local_inst().mem_zero(alloc, 16);
        data.layout_mut().insert_inst(entry, clear);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let asm = crate::compile::<Riscv64Backend>(&program).unwrap();
        assert!(!asm.contains("call memset"), "{asm}");
        assert_eq!(asm.matches("sw zero").count(), 4, "{asm}");
    }
}
