//! Branch and select condition lowering helpers.

use super::arith::{
    comparison_cond, float_comparison_cond, integer_constant, operand_size, positive_imm12,
};
use super::*;
pub(super) fn lower_select(
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
        let (cmp, cond1) = comparison_cmp(ctx, arena, first_binary);
        let (second_size, second_lhs, second_rhs, second_imm) =
            ccmp_operands(ctx, arena, second_binary);
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
pub(super) fn select_ccmp_chain(
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
pub(super) fn branch_ccmp_chain(
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
pub(super) fn ccmp_chain_operands(
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
pub(super) fn comparison_operands(
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
pub(super) fn ccmp_operands(
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
pub(super) fn comparison_cmp(
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
pub(super) fn nzcv_making_cond_false(cond: Cond) -> u8 {
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
pub(super) fn nzcv_making_cond_true(cond: Cond) -> u8 {
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

pub(super) fn select_comparison(
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

/// Select a branch-local, pure condition tree.  Claims are made through the
/// generic lowering context so reverse traversal never independently lowers a
/// producer whose result is consumed here.
pub(super) fn select_branch_condition(
    ctx: &mut LowerContext<'_, MInst>,
    branch: raana_ir::opt::prelude::Inst,
    cond: raana_ir::opt::prelude::Inst,
    true_target: MirBlockIndex,
    false_target: MirBlockIndex,
) -> bool {
    let arena = ctx.arena;
    if emit_and_comparison_tree(ctx, arena, branch, cond, true_target, false_target) {
        return true;
    }
    // Fold `br band(b1, b2)` / `br bor(b1, b2)` into `cmp; ccmp; b.cc`.
    if let Some((first, second, is_and)) = branch_ccmp_chain(ctx, arena, branch, cond) {
        let InstKind::Binary(first_binary) = arena.inst_data(first).kind() else {
            unreachable!("ccmp chain operand is a binary comparison");
        };
        let InstKind::Binary(second_binary) = arena.inst_data(second).kind() else {
            unreachable!("ccmp chain operand is a binary comparison");
        };
        let (size, lhs, rhs, imm) = comparison_operands(ctx, arena, first_binary);
        if let Some(imm) = imm {
            ctx.emit(MInst::CmpImm { size, lhs, imm });
        } else {
            ctx.emit(MInst::CmpRR { size, lhs, rhs });
        }
        let cond1 = comparison_cond(first_binary.op());
        let (second_size, second_lhs, second_rhs, second_imm) =
            ccmp_operands(ctx, arena, second_binary);
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
    let labels = (
        Label::from_block(true_target),
        Label::from_block(false_target),
    );
    let direct_bit = single_bit_mask(ctx, arena, outer, cond, branch);
    if let Some((tested, bit)) = direct_bit {
        if bit == 0 {
            if let InstKind::Binary(product) = arena.inst_data(tested).kind() {
                if product.op() == BinaryOp::Mul && has_only_user(ctx, tested, cond) {
                    if !ctx.sink_pure_single_use_chain(tested, cond, branch) {
                        return false;
                    }
                    let result = ctx.result_reg(tested);
                    let lhs = ctx.put_value_in_reg(product.lhs());
                    let rhs = ctx.put_value_in_reg(product.rhs());
                    ctx.emit(MInst::AluRRR {
                        op: AluOp::And,
                        size: OperandSize::Size32,
                        dst: Writable::from_reg(result),
                        lhs: RegOrZr::Reg(lhs),
                        rhs: RegOrZr::Reg(rhs),
                    });
                    let (true_label, false_label) = labels;
                    ctx.emit(MInst::Tbnz {
                        size: OperandSize::Size32,
                        reg: result,
                        bit,
                        true_label,
                        false_label,
                    });
                    return true;
                }
            }
        }
        if !ctx.sink_pure_single_use_producer(cond, branch) {
            return false;
        }
        let tested = ctx.put_value_in_reg(tested);
        let (true_label, false_label) = labels;
        ctx.emit(MInst::Tbnz {
            size: OperandSize::Size32,
            reg: tested,
            bit,
            true_label,
            false_label,
        });
        return true;
    }
    if !is_comparison(outer.op()) || !has_only_user(ctx, cond, branch) {
        return false;
    }

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

/// Lower a single-use conjunction tree of integer comparisons directly into
/// flags. Frontend boolean normalization may insert `value != 0` wrappers
/// between `and` nodes; those wrappers are transparent for canonical boolean
/// comparison results and are consumed with the tree.
pub(super) fn emit_and_comparison_tree(
    ctx: &mut LowerContext<'_, MInst>,
    arena: ArenaContext<'_>,
    branch: HirInst,
    root: HirInst,
    true_target: MirBlockIndex,
    false_target: MirBlockIndex,
) -> bool {
    fn collect(
        ctx: &LowerContext<'_, MInst>,
        arena: ArenaContext<'_>,
        node: HirInst,
        user: HirInst,
        comparisons: &mut Vec<HirInst>,
        edges: &mut Vec<(HirInst, HirInst)>,
        visited: &mut FxHashSet<HirInst>,
    ) -> bool {
        if !visited.insert(node) || !has_only_user(ctx, node, user) {
            return false;
        }
        let InstKind::Binary(binary) = arena.inst_data(node).kind() else {
            return false;
        };
        if binary.op() == BinaryOp::And {
            if !collect(ctx, arena, binary.lhs(), node, comparisons, edges, visited)
                || !collect(ctx, arena, binary.rhs(), node, comparisons, edges, visited)
            {
                return false;
            }
            edges.push((node, user));
            return true;
        }
        if let Some((inner, false)) = zero_comparison(arena, binary) {
            if collect(ctx, arena, inner, node, comparisons, edges, visited) {
                edges.push((node, user));
                return true;
            }
        }
        if !is_comparison(binary.op())
            || matches!(arena.inst_data(binary.lhs()).ty().kind(), TypeKind::Float32)
        {
            return false;
        }
        comparisons.push(node);
        edges.push((node, user));
        true
    }

    let mut comparisons = Vec::new();
    let mut edges = Vec::new();
    let mut visited = FxHashSet::default();
    if !collect(
        ctx,
        arena,
        root,
        branch,
        &mut comparisons,
        &mut edges,
        &mut visited,
    ) || comparisons.len() < 3
        || comparisons.len() > 8
        || !ctx.sink_pure_single_use_tree(&edges, branch)
    {
        return false;
    }

    let InstKind::Binary(first) = arena.inst_data(comparisons[0]).kind() else {
        unreachable!();
    };
    let (size, lhs, rhs, imm) = comparison_operands(ctx, arena, first);
    if let Some(imm) = imm {
        ctx.emit(MInst::CmpImm { size, lhs, imm });
    } else {
        ctx.emit(MInst::CmpRR { size, lhs, rhs });
    }
    let mut condition = comparison_cond(first.op());
    for comparison in comparisons.into_iter().skip(1) {
        let InstKind::Binary(binary) = arena.inst_data(comparison).kind() else {
            unreachable!();
        };
        let (size, lhs, rhs, imm) = ccmp_operands(ctx, arena, binary);
        let next_condition = comparison_cond(binary.op());
        ctx.emit(MInst::CCmp {
            size,
            lhs,
            rhs,
            imm,
            nzcv: nzcv_making_cond_false(next_condition),
            cond: condition,
        });
        condition = next_condition;
    }
    ctx.emit(MInst::CondBr {
        cond: condition,
        true_label: Label::from_block(true_target),
        false_label: Label::from_block(false_target),
    });
    true
}

pub(super) fn has_only_user(
    ctx: &LowerContext<'_, MInst>,
    producer: raana_ir::opt::prelude::Inst,
    user: raana_ir::opt::prelude::Inst,
) -> bool {
    let users = ctx.arena.inst_data(producer).used_by();
    users.len() == 1 && users.contains(&user)
}

pub(super) fn zero_comparison(arena: ArenaContext<'_>, binary: &Binary) -> Option<(HirInst, bool)> {
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

pub(super) fn single_bit_mask(
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

pub(super) fn emit_comparison_branch(
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

pub(super) fn emit_float_zero_branch(
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

pub(super) fn is_comparison(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Eq | BinaryOp::NotEq | BinaryOp::Gt | BinaryOp::Lt | BinaryOp::Ge | BinaryOp::Le
    )
}

pub(super) fn invert_float_comparison_cond(op: BinaryOp) -> Cond {
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
