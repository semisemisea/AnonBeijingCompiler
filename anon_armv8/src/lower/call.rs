//! Call, tail-call, return, and builtin-expansion helpers.

use super::*;
/// The compiler-provided modular-multiplication builtin recognized by the
/// `mulmod_recognize` IR pass (AArch64 only). It has no body and is never
/// defined in the assembly: every call is expanded here.
const MULMOD_BUILTIN: &str = "soyo_mulmod";

pub(super) fn is_mulmod_builtin(arena: ArenaContext<'_>, callee: HirFunction) -> bool {
    arena.func_data(callee).name() == MULMOD_BUILTIN
}

/// Lower a call to the `soyo_mulmod(a, b, p)` builtin to the four-instruction
/// sequence
///
/// ```text
/// smull x2, w0, w1   // product = (i64)a*b  (exact: two 32-bit operands)
/// sxtw  x3, w2       // p64 = (i64)p
/// sdiv  x4, x2, x3   // quotient = product / p64 (truncating)
/// msub  x0, x4, x3, x2 // result = product - quotient*p64  (the remainder)
/// ```
///
/// The remainder of `(i64)a*b % p` is always in `(-p, p)`, so its low 32 bits
/// are the correct `i32` return value. No actual call is emitted.
///
/// A multiply-high magic-number sequence (`smulh` + shifts) was implemented
/// and measured: it regressed both the static count and the QEMU runtime,
/// because every inline-expanded site re-materializes the 64-bit magic
/// constant (4 `movz`/`movk`) plus the divisor inside the hot butterfly loop,
/// and QEMU executes `sdiv` natively. The generic `sdiv` stays until a
/// loop-invariant constant hoist exists (see TODO.md M62).
pub(super) fn lower_mulmod_builtin(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    call: &Call,
) -> LoweredOutput {
    let args = call.args();
    assert_eq!(args.len(), 3, "soyo_mulmod takes exactly three arguments");
    let a = ctx.put_value_in_reg(args[0]);
    let b = ctx.put_value_in_reg(args[1]);
    let p = ctx.put_value_in_reg(args[2]);

    // 64-bit temporaries: the product and the intermediate quotient.
    let tmp_ty = HirType::get_pointer(HirType::get_i32());
    let product = ctx.alloc_tmp(tmp_ty.clone());
    ctx.emit(MInst::SMulL {
        dst: Writable::from_reg(product),
        lhs: a,
        rhs: b,
    });

    let p64 = ctx.alloc_tmp(tmp_ty.clone());
    ctx.emit(MInst::Sxtw {
        size: OperandSize::Size32,
        dst: Writable::from_reg(p64),
        src: p,
    });

    let quotient = ctx.alloc_tmp(tmp_ty);
    ctx.emit(MInst::SDiv {
        size: OperandSize::Size64,
        dst: Writable::from_reg(quotient),
        lhs: product,
        rhs: p64,
    });

    let result = ctx.result_reg(inst);
    ctx.emit(MInst::MSub {
        size: OperandSize::Size64,
        dst: Writable::from_reg(result),
        lhs: quotient,
        rhs: p64,
        subtrahend: product,
    });
    let _ = arena;
    LoweredOutput::Value(result)
}

/// The compiler-provided runtime cache allocator declared by the M68
/// recursive-memoization pass. It has no body: every call is redirected to the
/// embedded `.Lsoyo_calloc` wrapper, which zero-extends the two 32-bit
/// arguments and tail-calls glibc `calloc`.
pub(super) fn is_calloc_builtin(arena: ArenaContext<'_>, callee: HirFunction) -> bool {
    arena.func_data(callee).name() == raana_ir::opt::CALLOO_NAME
}

pub(super) fn lower_calloc_builtin(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    call: &Call,
) -> LoweredOutput {
    let args = call.args();
    assert_eq!(args.len(), 2, "soyo_calloc takes exactly two arguments");
    let count = ctx.put_value_in_reg(args[0]);
    let elem_size = ctx.put_value_in_reg(args[1]);
    let result = ctx.result_reg(inst);
    ctx.emit(MInst::Call {
        args: vec![
            CallArgPair {
                vreg: count,
                preg: regs::INT_ARG_REGS[0],
            },
            CallArgPair {
                vreg: elem_size,
                preg: regs::INT_ARG_REGS[1],
            },
        ],
        ret: Some(CallRetPair {
            vreg: Writable::from_reg(result),
            preg: regs::INT_RETURN_REG,
        }),
        clobbers: regs::DEFAULT_CLOBBERS,
        label: Label::Embedded(EmbeddedSymbol::Calloc),
    });
    ctx.set_has_calls();
    ctx.set_outgoing_arg_size(0);
    let _ = arena;
    LoweredOutput::Value(result)
}

pub(super) fn lower_call(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    inst: HirInst,
    call: &Call,
) -> LoweredOutput {
    if is_mulmod_builtin(arena, call.callee()) {
        return lower_mulmod_builtin(ctx, arena, inst, call);
    }
    if is_calloc_builtin(arena, call.callee()) {
        return lower_calloc_builtin(ctx, arena, inst, call);
    }
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
pub(super) fn lower_tail_call(
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

pub(super) fn lower_return(
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
