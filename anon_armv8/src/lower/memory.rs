//! Allocation, addressing, load/store, and memory-zeroing helpers.

use super::arith::add_sub_immediate;
use super::vector::vector_shape;
use super::*;
pub(super) fn lower_alloc(
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

pub(super) fn lower_get_elem_ptr(
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
            let extended = ctx.alloc_tmp(pointer_ty.clone());
            ctx.emit(MInst::Sxtw {
                size: OperandSize::Size32,
                dst: Writable::from_reg(extended),
                src: index,
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

pub(super) fn lower_load(
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

pub(super) fn lower_mem_zero(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    mem_zero: &MemZero,
) -> LoweredOutput {
    match mem_zero.byte_len_len() {
        MemZeroLen::Const(byte_len) => lower_const_mem_zero(ctx, arena, mem_zero, *byte_len),
        MemZeroLen::Value(byte_len) => {
            let dest = ctx.put_value_in_reg(mem_zero.dest());
            let byte_len = ctx.put_value_in_reg(*byte_len);
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
    }
}

pub(super) fn lower_const_mem_zero(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    mem_zero: &MemZero,
    byte_len: usize,
) -> LoweredOutput {
    let inline_store_count = byte_len / 4;
    if runtime::mem_zero_is_inline(byte_len) {
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
    let byte_len_reg = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
    if let Some(offset) = stack_offset {
        ctx.emit(<AArch64Abi as ABIMachineSpec>::gen_get_stack_addr(
            StackAMode::Slot(offset),
            Writable::from_reg(dest),
        ));
    }
    ctx.emit(MInst::LoadImm {
        size: OperandSize::Size64,
        dst: Writable::from_reg(byte_len_reg),
        value: byte_len as u64,
    });
    ctx.emit(MInst::Call {
        args: vec![
            CallArgPair {
                vreg: dest,
                preg: regs::INT_ARG_REGS[0],
            },
            CallArgPair {
                vreg: byte_len_reg,
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

pub(super) fn extended_index_shift(stride: u64) -> Option<u8> {
    match stride {
        1 => Some(0),
        2 => Some(1),
        4 => Some(2),
        8 => Some(3),
        16 => Some(4),
        _ => None,
    }
}

pub(super) fn memory_type(ty: &TypeKind) -> MemoryType {
    match ty {
        TypeKind::Int32 => MemoryType::I32,
        TypeKind::Float32 => MemoryType::F32,
        TypeKind::Pointer(_) | TypeKind::String => MemoryType::I64,
        TypeKind::Vector(..) => MemoryType::Vec128,
        ty => unreachable!("unsupported AArch64 integer memory type: {ty:?}"),
    }
}

pub(super) fn lower_store(
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
pub(super) fn store_base(
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

pub(super) fn emit_zero_init(
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

pub(super) fn emit_store_at(
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

pub(super) fn memory_address(
    ctx: &mut LowerContext<'_, MInst>,
    base: taki_mir::register::Reg,
    offset: i64,
    ty: MemoryType,
) -> AMode {
    if offset == 0 {
        return AMode::Reg { base };
    }
    if offset > 0 {
        if let Some(offset) = crate::instructions::UImm12Scaled::new(offset as u64, ty.byte_size())
        {
            return AMode::UnsignedOffset { base, offset };
        }
    }
    if let Ok(offset) = i16::try_from(offset) {
        if let Some(offset) = crate::instructions::SImm9::new(offset) {
            return AMode::SignedOffset { base, offset };
        }
    }

    let address = ctx.alloc_tmp(HirType::get_pointer(HirType::get_i32()));
    emit_add_offset(ctx, Writable::from_reg(address), base, offset);
    AMode::Reg { base: address }
}

/// Fold a constant GEP into a `(base, offset)` pair whose offset is
/// encodable in AArch64 load/store addressing for `width`-byte accesses.
/// Returns `None` for dynamic-index GEPs or unencodable offsets; callers then
/// materialize the address as before.
///
/// Unlike the single-use variant, the GEP may be shared by several loads and
/// stores (e.g. a 2x-unrolled vectorizer continuation GEP consumed by both the
/// unrolled vector load and its paired store): every memory user folds it into
/// its own addressing mode, so the producer is never materialized.
pub(super) fn try_fold_gep_offset(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    gep: HirInst,
    consumer: HirInst,
    width: u8,
) -> Option<(taki_mir::register::Reg, i64)> {
    fold_gep_constant_offset_shared(ctx, arena, gep, consumer, |off| {
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
pub(super) fn try_fold_dynamic_gep_amode(
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

pub(super) fn emit_add_offset(
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
