//! Pre-RA peephole combine pass for AArch64.
//!
//! Scans each basic block forward and fuses instruction pairs that the
//! hand-written ISel missed (or that were introduced by earlier IR passes)
//! into single AArch64 fused-form instructions.

use rustc_hash::FxHashMap;

use taki_mir::{
    block_order::MirBlockIndex,
    passes::MIRPass,
    prelude::ArenaContext,
    reg_alloc::{
        function::Function,
        index::{Block, Inst},
        reg::{OperandKind, VReg},
    },
    register::Reg,
    stats::FunctionCodegenStats,
    vcode::{MachInst, VCodeContainer},
};

use crate::instructions::{AluOp, Cond, MInst, VecArithOp, VecMlaOp, VecShape};
use crate::regs::RegOrZr;
use taki_mir::register::Writable;

pub struct PeepholeCombine;

impl MIRPass<MInst> for PeepholeCombine {
    fn name(&self) -> &'static str {
        "PeepholeCombine"
    }

    fn run(
        &self,
        vcode: &mut VCodeContainer<MInst>,
        _arena: ArenaContext,
        stats: &mut FunctionCodegenStats,
    ) -> bool {
        stats.peephole.ran = true;
        let use_counts = build_vreg_use_counts(vcode);

        let mut changed = false;
        for block_idx in 0..vcode.num_blocks() {
            let range = vcode.block_inst_range(block_idx);
            let fused = combine_mac_in_block(vcode, range, &use_counts);
            stats.peephole.mac_pairs_formed += fused;
            changed |= fused > 0;
        }
        let vec_fused = combine_vec_mac_in_block(vcode, &use_counts);
        stats.peephole.mac_pairs_formed += vec_fused;
        changed |= vec_fused > 0;
        // Flag fusion runs on the (possibly MAC-fused) instruction stream and
        // rewrites both a block's interior and its latch edge into another
        // block, so it cannot run per-block against cached ranges.
        let flag_fused = combine_flag_fusion(vcode, &use_counts);
        stats.peephole.flag_fusions_formed += flag_fused;
        changed |= flag_fused > 0;
        stats.peephole.changed = changed;
        changed
    }
}

/// Count how many times each virtual register appears as a `Use` operand
/// across the entire function. A register with a count of 1 is a candidate
/// for fusion — its producer can be absorbed into the sole consumer.
fn build_vreg_use_counts(vcode: &mut VCodeContainer<MInst>) -> FxHashMap<Reg, u32> {
    let mut counts: FxHashMap<Reg, u32> = FxHashMap::default();
    for i in 0..vcode.num_insts() {
        let inst = vcode.inst_mut(i);
        inst.get_operands(&mut |reg: &mut Reg, _constraint, kind, _pos| {
            if kind == OperandKind::Use && reg.is_virtual() {
                *counts.entry(*reg).or_insert(0) += 1;
            }
        });
    }
    counts
}

/// Scan a block forward and fuse `Mul + Add → MAdd`, `Mul + Sub → MSub`.
fn combine_mac_in_block(
    vcode: &mut VCodeContainer<MInst>,
    range: core::ops::Range<usize>,
    use_counts: &FxHashMap<Reg, u32>,
) -> u64 {
    let mut fused_count = 0;
    let mut i = range.start;
    while i + 1 < range.end {
        // Check if instruction `i` is a standalone Mul whose result is single-use.
        let (mul_dst, mul_lhs, mul_rhs, _size) = match vcode.inst(i) {
            MInst::AluRRR {
                op: AluOp::Mul,
                dst,
                lhs,
                rhs,
                size,
            } => {
                let lhs_reg = reg_or_zr_to_reg(lhs);
                let rhs_reg = reg_or_zr_to_reg(rhs);
                match (lhs_reg, rhs_reg) {
                    (Some(l), Some(r)) => (dst.reg, l, r, *size),
                    _ => {
                        i += 1;
                        continue;
                    }
                }
            }
            _ => {
                i += 1;
                continue;
            }
        };

        // Only fuse if the Mul result has exactly one use.
        if use_counts.get(&mul_dst).copied().unwrap_or(0) != 1 {
            i += 1;
            continue;
        }

        // Look at the next instruction to see if it consumes mul_dst.
        let fused = match vcode.inst(i + 1) {
            MInst::AluRRR {
                op: AluOp::Add,
                dst: add_dst,
                lhs: add_lhs,
                rhs: add_rhs,
                size: add_size,
            } => {
                let addend = other_operand(add_lhs, add_rhs, &mul_dst);
                addend.map(|a| MInst::MAdd {
                    size: *add_size,
                    dst: *add_dst,
                    lhs: mul_lhs,
                    rhs: mul_rhs,
                    addend: a,
                })
            }
            MInst::AluRRR {
                op: AluOp::Sub,
                dst: sub_dst,
                lhs: sub_lhs,
                rhs: sub_rhs,
                size: sub_size,
            } => {
                // MSub computes dst = subtrahend - lhs*rhs.
                // Match: sub dst, X, mul_dst  =>  dst = X - mul = msub(dst, mul_lhs, mul_rhs, X)
                let subtrahend = other_operand(sub_lhs, sub_rhs, &mul_dst);
                // Only the rhs-equals-mul_dst form is valid: `X - product`.
                // `product - X` cannot be expressed as a single MSub.
                if reg_or_zr_matches(sub_rhs, &mul_dst) {
                    subtrahend.map(|s| MInst::MSub {
                        size: *sub_size,
                        dst: *sub_dst,
                        lhs: mul_lhs,
                        rhs: mul_rhs,
                        subtrahend: s,
                    })
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some(fused_inst) = fused {
            *vcode.inst_mut(i) = MInst::Removed;
            *vcode.inst_mut(i + 1) = fused_inst;
            fused_count += 1;
            // Skip past the fused pair.
            i += 2;
        } else {
            i += 1;
        }
    }
    fused_count
}

/// Fuse `VecArithRRR(Mul)` + `VecArithRRR(Add)` into `VecMla` (`mla`).
///
/// Unlike the scalar MAC fusion, the vector mul and add are rarely adjacent:
/// the vectorizer lowers the mm kernel as `mul vD; <load B>; add vA, vD, vB`,
/// so the consumer must be located by scanning forward in the block. The
/// fused form reuses the accumulator register (`dst` reuses `acc`'s
/// allocation via `reg_reuse_def`), letting the emitter drop the leading
/// copy and emit a bare `mla vacc, vlhs, vrhs`.
fn combine_vec_mac_in_block(vcode: &mut VCodeContainer<MInst>, use_counts: &FxHashMap<Reg, u32>) -> u64 {
    let mut fused_count = 0;
    for block_idx in 0..vcode.num_blocks() {
        let range = vcode.block_inst_range(block_idx);
        let mut i = range.start;
        while i < range.end {
            // Candidate producer: `VecArithRRR Mul` whose result is single-use.
            let (mul_dst, mul_lhs, mul_rhs, shape) = match vcode.inst(i) {
                MInst::VecArithRRR {
                    op: VecArithOp::Mul,
                    shape,
                    dst,
                    lhs,
                    rhs,
                } => {
                    if use_counts.get(&dst.to_reg()).copied().unwrap_or(0) != 1 {
                        i += 1;
                        continue;
                    }
                    (dst.to_reg(), *lhs, *rhs, *shape)
                }
                _ => {
                    i += 1;
                    continue;
                }
            };

            // Locate the consumer `VecArithRRR Add` that uses `mul_dst` as
            // exactly one operand; the other operand is the accumulator.
            let mut j = i + 1;
            let mut fused = None;
            while j < range.end {
                match vcode.inst(j) {
                    MInst::VecArithRRR {
                        op: VecArithOp::Add,
                        shape: add_shape,
                        dst,
                        lhs,
                        rhs,
                    } if *add_shape == shape => {
                        // Vector operands are plain `Reg`s (no `RegOrZr`).
                        let acc = if *lhs == mul_dst {
                            Some(*rhs)
                        } else if *rhs == mul_dst {
                            Some(*lhs)
                        } else {
                            None
                        };
                        if let Some(acc) = acc {
                            if acc != mul_lhs && acc != mul_rhs {
                                // The accumulator must be single-use (its
                                // value is consumed in-place by the mla).
                                if use_counts.get(&acc).copied().unwrap_or(0) == 1 {
                                    fused = Some((j, dst.to_reg(), acc));
                                }
                            }
                        }
                        break;
                    }
                    _ => {
                        // Any intervening instruction that defines the mul
                        // operands or the accumulator invalidates the fusion.
                        if inst_defines_any(vcode, j, &[mul_lhs, mul_rhs, mul_dst]) {
                            break;
                        }
                        j += 1;
                    }
                }
            }

            if let Some((j, add_dst, acc)) = fused {
                // `VecMla { dst: add_dst, acc, lhs: mul_lhs, rhs: mul_rhs }`
                // computes `add_dst = acc + mul_lhs*mul_rhs`, i.e. the fused
                // mul+add. `add_dst` is what the surrounding code reads, so
                // the register allocator's reuse-def constraint ties it to
                // `acc`'s register (the accumulator is dead after the mla).
                *vcode.inst_mut(i) = MInst::Removed;
                *vcode.inst_mut(j) = MInst::VecMla {
                    op: VecMlaOp::Mla,
                    shape,
                    dst: Writable::from_reg(add_dst),
                    acc,
                    lhs: mul_lhs,
                    rhs: mul_rhs,
                };
                fused_count += 1;
                i = j + 1;
            } else {
                i += 1;
            }
        }
    }
    fused_count
}

/// Whether instruction `i` defines any of `regs` (as a non-removed def).
fn inst_defines_any(vcode: &VCodeContainer<MInst>, i: usize, regs: &[Reg]) -> bool {
    let mut defines = false;
    let mut inst = vcode.inst(i).clone();
    inst.get_operands(&mut |reg: &mut Reg, _constraint, kind, _pos| {
        if kind == OperandKind::Def && regs.contains(reg) {
            defines = true;
        }
    });
    defines
}

/// Extract the `Reg` from a `RegOrZr`, if it is not the zero register.
fn reg_or_zr_to_reg(rz: &RegOrZr) -> Option<Reg> {
    match rz {
        RegOrZr::Reg(r) => Some(*r),
        RegOrZr::Zr => None,
    }
}

/// Check whether a `RegOrZr` holds a specific `Reg`.
fn reg_or_zr_matches(rz: &RegOrZr, reg: &Reg) -> bool {
    matches!(rz, RegOrZr::Reg(r) if r == reg)
}

/// Given two `RegOrZr` operands of an Add/Sub, if exactly one of them equals
/// `mul_dst`, return the other one (the addend / subtrahend).
fn other_operand(lhs: &RegOrZr, rhs: &RegOrZr, mul_dst: &Reg) -> Option<Reg> {
    if reg_or_zr_matches(lhs, mul_dst) {
        reg_or_zr_to_reg(rhs)
    } else if reg_or_zr_matches(rhs, mul_dst) {
        reg_or_zr_to_reg(lhs)
    } else {
        None
    }
}

/// Conditions whose truth is fully determined by N/Z for a `cmp r, #0`,
/// which `subs r, r, #imm` preserves exactly. `C` and `V` are not preserved
/// by a subtract, so unsigned and overflow-sensing conditions are excluded.
///
/// `Gt` is additionally fused for the rotated-runtime vector-loop counter
/// (M70): the counter enters at `cnt0 = trip & -4` (a non-negative multiple
/// of VF) and steps down by 4 to exactly 0, so the `subs` can never overflow
/// and `subs wX, wX, #4; b.gt` is exactly the `x -= 4; if (x > 0)` idiom.
fn subs_safe_cond(cond: Cond) -> bool {
    matches!(cond, Cond::Eq | Cond::Ne | Cond::Mi | Cond::Pl | Cond::Gt)
}/// Conditions safe under `ands`/`tst` flag fusion. A logical operation sets
/// `V = 0` and `C = 0`, same as `cmp r, #0` except for `C`, which only the
/// unsigned conditions observe.
fn logical_safe_cond(cond: Cond) -> bool {
    !matches!(cond, Cond::Hs | Cond::Lo | Cond::Hi | Cond::Ls)
}

/// The in-block adjacency rule: `alu; cmp r, #0; b.cc` → fused flag form.
fn fuse_flag_triple(
    first: &MInst,
    second: &MInst,
    third: &MInst,
    use_counts: &FxHashMap<Reg, u32>,
) -> Option<MInst> {
    match (first, second, third) {
        (
            MInst::AluRRImm12 {
                op: AluOp::Sub,
                size,
                dst,
                src,
                imm,
            },
            MInst::CmpImm {
                size: cmp_size,
                lhs,
                imm: cmp_imm,
            },
            MInst::CondBr { cond, .. },
        ) if *cmp_size == *size
            && cmp_imm.value() == 0
            && *lhs == dst.reg
            && subs_safe_cond(*cond) =>
        {
            Some(MInst::SubsRRImm12 {
                size: *size,
                dst: *dst,
                src: *src,
                imm: *imm,
            })
        }
        (
            MInst::AluRRImmLogic {
                op: AluOp::And,
                size,
                dst,
                src,
                imm,
            },
            MInst::CmpImm {
                size: cmp_size,
                lhs,
                imm: cmp_imm,
            },
            MInst::CondBr { cond, .. },
        ) if *cmp_size == *size
            && cmp_imm.value() == 0
            && *lhs == dst.reg
            && logical_safe_cond(*cond) =>
        {
            if use_counts.get(&dst.reg).copied().unwrap_or(0) == 1 {
                Some(MInst::TstRRImmLogic {
                    size: *size,
                    src: *src,
                    imm: *imm,
                })
            } else {
                Some(MInst::AndsRRImmLogic {
                    size: *size,
                    dst: *dst,
                    src: *src,
                    imm: *imm,
                })
            }
        }
        _ => None,
    }
}

/// Fuse flag-producing ALU forms with an immediately following `cmp r, #0`
/// and its consuming branch, plus the loop-latch pattern introduced by loop
/// rotation (`sub r, r, #imm` at the end of the latch, `cmp r, #0` at the
/// head of the single-successor test block):
///
/// - `sub r, r, #imm; cmp r, #0; b.cc`  → `subs r, r, #imm; b.cc`
/// - `and r, r, #imm; cmp r, #0; b.cc`  → `ands` (result live) or
///   `tst r, #imm` (result dead) + `b.cc`
/// - latch: `sub r, r, #imm; b T` + `T: cmp r, #0; b.cc` → `T: subs r, r, #imm; b.cc`
fn combine_flag_fusion(vcode: &mut VCodeContainer<MInst>, use_counts: &FxHashMap<Reg, u32>) -> u64 {
    let mut fused_count = 0;

    // In-block adjacency rules.
    for block_idx in 0..vcode.num_blocks() {
        let range = vcode.block_inst_range(block_idx);
        let mut i = range.start;
        while i + 2 < range.end {
            let fused = fuse_flag_triple(
                vcode.inst(i),
                vcode.inst(i + 1),
                vcode.inst(i + 2),
                use_counts,
            );
            if let Some(fused_inst) = fused {
                *vcode.inst_mut(i) = fused_inst;
                *vcode.inst_mut(i + 1) = MInst::Removed;
                fused_count += 1;
                i += 3;
            } else {
                i += 1;
            }
        }
    }

    // Cross-block latch rule. The test block starts with `cmp r, #0` and the
    // latch falls into it through a plain jump whose penultimate instruction
    // is `sub r, r, #imm`.
    let block_count = vcode.num_blocks();
    for block_idx in 0..block_count {
        let range = vcode.block_inst_range(block_idx);
        let first = range
            .clone()
            .find(|&i| !matches!(vcode.inst(i), MInst::Removed));
        let Some(cmp_idx) = first else {
            continue;
        };
        let MInst::CmpImm {
            size,
            lhs,
            imm: cmp_imm,
        } = *vcode.inst(cmp_idx)
        else {
            continue;
        };
        if cmp_imm.value() != 0 || cmp_idx + 1 >= range.end {
            continue;
        }
        let MInst::CondBr { .. } = *vcode.inst(cmp_idx + 1) else {
            continue;
        };
        let preds = vcode.block_preds(Block::new(block_idx));
        if preds.len() != 1 {
            continue;
        }
        let pred_idx = preds[0].index();
        let pred_range = vcode.block_inst_range(pred_idx);
        let last = pred_range.end - 1;
        let MInst::Jump { label } = vcode.inst(last) else {
            continue;
        };
        if label.block() != Some(MirBlockIndex::new(block_idx)) {
            continue;
        }
        let MInst::AluRRImm12 {
            op: AluOp::Sub,
            size: sub_size,
            dst,
            src,
            imm,
        } = *vcode.inst(last - 1)
        else {
            continue;
        };
        if sub_size != size {
            continue;
        }
        // The compared register must be the latch's block param for this
        // decrement: the sub's result is exactly the edge argument fed into
        // it. The fused `subs` then sets the same flags the removed `cmp`
        // would have, and the RA's edge copy keeps the value flowing.
        let params = vcode.block_params(Block::new(block_idx));
        let lhs_vreg = VReg::from(lhs);
        let Some(param_index) = params.iter().position(|&p| p == lhs_vreg) else {
            continue;
        };
        let edge_args = vcode.branch_blockparams(Block::new(pred_idx), Inst::new(last), 0);
        if edge_args.get(param_index) != Some(&dst.reg.into()) {
            continue;
        }
        *vcode.inst_mut(last - 1) = MInst::SubsRRImm12 {
            size,
            dst,
            src,
            imm,
        };
        *vcode.inst_mut(cmp_idx) = MInst::Removed;
        fused_count += 1;
    }

    fused_count
}

#[cfg(test)]
mod tests {
    use raana_ir::ir::{Program, Type};
    use taki_mir::reg_alloc::reg::{RegClass, VReg};
    use taki_mir::register::{VRegAllocator, Writable};

    use super::*;
    use crate::{
        instructions::{Imm12, ImmLogic},
        regs::OperandSize,
    };

    fn vreg(index: u32) -> Reg {
        Reg::from_virtual_reg(VReg::new(192 + index as usize, RegClass::Int))
    }

    fn writable(index: u32) -> Writable<Reg> {
        Writable::from_reg(vreg(index))
    }

    fn sub_imm(dst: u32, src: u32, imm: u16) -> MInst {
        MInst::AluRRImm12 {
            op: AluOp::Sub,
            size: OperandSize::Size32,
            dst: writable(dst),
            src: vreg(src),
            imm: Imm12::new(imm, false).unwrap(),
        }
    }

    fn and_imm(dst: u32, src: u32, mask: u64) -> MInst {
        MInst::AluRRImmLogic {
            op: AluOp::And,
            size: OperandSize::Size32,
            dst: writable(dst),
            src: RegOrZr::Reg(vreg(src)),
            imm: ImmLogic::new(mask, OperandSize::Size32).unwrap(),
        }
    }

    fn cmp_zero(reg: u32) -> MInst {
        MInst::CmpImm {
            size: OperandSize::Size32,
            lhs: vreg(reg),
            imm: Imm12::new(0, false).unwrap(),
        }
    }

    fn cond_br(cond: Cond) -> MInst {
        MInst::CondBr {
            cond,
            true_label: crate::labels::Label::from_block(MirBlockIndex::new(0)),
            false_label: crate::labels::Label::from_block(MirBlockIndex::new(1)),
        }
    }

    fn no_uses() -> FxHashMap<Reg, u32> {
        FxHashMap::default()
    }

    #[test]
    fn fuses_sub_cmp_condbr_into_subs() {
        let fused = fuse_flag_triple(
            &sub_imm(0, 0, 1),
            &cmp_zero(0),
            &cond_br(Cond::Ne),
            &no_uses(),
        );
        assert!(matches!(
            fused,
            Some(MInst::SubsRRImm12 {
                size: OperandSize::Size32,
                dst,
                src,
                imm,
            }) if dst.to_reg() == vreg(0) && src == vreg(0) && imm.value() == 1
        ));
    }

    #[test]
    fn refuses_unsigned_condition_for_sub_fusion() {
        let fused = fuse_flag_triple(
            &sub_imm(0, 0, 1),
            &cmp_zero(0),
            &cond_br(Cond::Ls),
            &no_uses(),
        );
        assert!(
            fused.is_none(),
            "subs changes C, so unsigned conds must not fuse"
        );
    }

    #[test]
    fn fuses_sub_cmp_condbr_into_subs_for_countdown_gt() {
        // The rotated-runtime vector-loop counter (M70): `sub r,r,#4; cmp
        // r,#0; b.gt` → `subs r,r,#4; b.gt`. The counter enters at a
        // non-negative multiple of VF and steps down to exactly 0, so the
        // `subs` cannot overflow and its N/Z flags match `cmp r, #0` — the
        // same countdown idiom clang emits as `subs ...; b.gt`.
        let fused = fuse_flag_triple(
            &sub_imm(0, 0, 4),
            &cmp_zero(0),
            &cond_br(Cond::Gt),
            &no_uses(),
        );
        assert!(matches!(
            fused,
            Some(MInst::SubsRRImm12 {
                size: OperandSize::Size32,
                dst,
                src,
                imm,
            }) if dst.to_reg() == vreg(0) && src == vreg(0) && imm.value() == 4
        ));
    }

    #[test]
    fn fuses_live_and_into_ands_and_dead_and_into_tst() {
        let mut uses = FxHashMap::default();
        uses.insert(vreg(0), 2);
        let fused = fuse_flag_triple(
            &and_imm(0, 1, 0x8000_0001),
            &cmp_zero(0),
            &cond_br(Cond::Eq),
            &uses,
        );
        assert!(matches!(fused, Some(MInst::AndsRRImmLogic { .. })));

        let mut uses = FxHashMap::default();
        uses.insert(vreg(0), 1);
        let fused = fuse_flag_triple(
            &and_imm(0, 1, 0x8000_0001),
            &cmp_zero(0),
            &cond_br(Cond::Eq),
            &uses,
        );
        assert!(matches!(fused, Some(MInst::TstRRImmLogic { .. })));
    }

    #[test]
    fn refuses_mismatched_registers_and_nonzero_compare() {
        assert!(
            fuse_flag_triple(
                &sub_imm(0, 1, 1),
                &cmp_zero(2),
                &cond_br(Cond::Ne),
                &no_uses()
            )
            .is_none()
        );
        let bad_cmp = MInst::CmpImm {
            size: OperandSize::Size32,
            lhs: vreg(0),
            imm: Imm12::new(1, false).unwrap(),
        };
        assert!(
            fuse_flag_triple(&sub_imm(0, 0, 1), &bad_cmp, &cond_br(Cond::Ne), &no_uses()).is_none()
        );
    }

    // --- vector MAC fusion ---

    /// Build a single-block VCode with the given instructions (pushed in
    /// reverse of final order) and a fresh set of integer/vector vregs.
    fn build_vcode(
        insts: impl DoubleEndedIterator<Item = MInst>,
        num_vregs: usize,
    ) -> VCodeContainer<MInst> {
        use crate::abi::AArch64Abi;
        use raana_ir::ir::builder_trait::*;
        use taki_mir::abi::CalleeABI;
        use taki_mir::block_order::BlockLoweringOrder;
        use taki_mir::prelude::ArenaContext;
        use taki_mir::vcode::VCodeBuilder;

        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "vec_fusion".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);
        let arena = ArenaContext {
            program: &program,
            curr_func: Some(function),
        };
        let abi = CalleeABI::<AArch64Abi>::new(arena);
        let order = BlockLoweringOrder::new(arena);
        let mut builder = VCodeBuilder::new(abi, order);
        let mut vregs = VRegAllocator::<MInst>::with_capaticy(num_vregs);
        for inst in insts {
            builder.push(inst);
        }
        builder.end_bb();
        builder.build(vregs)
    }

    #[test]
    fn fuses_vec_mul_add_across_intervening_inst() {
        use taki_mir::types::V4I32;

        // Kernel shape from 01_mm1: mul vA, vC, vSplat; <load B>; add vSum, vA, vB.
        // The mul and add are separated by an instruction defining `b`.
        let mut vregs = VRegAllocator::<MInst>::with_capaticy(6);
        let c = vregs.alloc(V4I32);
        let splat = vregs.alloc(V4I32);
        let prod = vregs.alloc(V4I32);
        let b = vregs.alloc(V4I32);
        let sum = vregs.alloc(V4I32);
        let dummy = vregs.alloc(V4I32);

        let mut insts = vec![
            MInst::VecArithRRR {
                op: VecArithOp::Mul,
                shape: VecShape::FourS,
                dst: Writable::from_reg(prod),
                lhs: c,
                rhs: splat,
            },
            // intervening vector copy defining `b` (stands in for the B load)
            MInst::VecMov {
                dst: Writable::from_reg(b),
                src: dummy,
            },
            MInst::VecArithRRR {
                op: VecArithOp::Add,
                shape: VecShape::FourS,
                dst: Writable::from_reg(sum),
                lhs: prod,
                rhs: b,
            },
        ];
        insts.reverse();
        let mut vcode = build_vcode(insts.into_iter(), 6);

        let use_counts = build_vreg_use_counts(&mut vcode);
        let fused = combine_vec_mac_in_block(&mut vcode, &use_counts);
        assert_eq!(fused, 1);
        let insts: Vec<_> = (0..vcode.num_insts()).map(|i| vcode.inst(i)).collect();
        // mul removed, add replaced by VecMla.
        assert!(matches!(insts[0], MInst::Removed));
        assert!(matches!(insts[2], MInst::VecMla { .. }));
        let MInst::VecMla {
            op,
            shape,
            dst,
            acc,
            lhs,
            rhs,
        } = insts[2]
        else {
            panic!("expected VecMla");
        };
        assert_eq!(*op, VecMlaOp::Mla);
        assert_eq!(*shape, VecShape::FourS);
        assert_eq!(dst.to_reg(), sum);
        assert_eq!(*acc, b);
        assert_eq!(*lhs, c);
        assert_eq!(*rhs, splat);
    }

    #[test]
    fn refuses_vec_mul_add_when_accumulator_is_shared() {
        use taki_mir::types::V4I32;

        let mut vregs = VRegAllocator::<MInst>::with_capaticy(5);
        let c = vregs.alloc(V4I32);
        let splat = vregs.alloc(V4I32);
        let prod = vregs.alloc(V4I32);
        let b = vregs.alloc(V4I32);
        let sum = vregs.alloc(V4I32);

        let mut insts = vec![
            MInst::VecArithRRR {
                op: VecArithOp::Mul,
                shape: VecShape::FourS,
                dst: Writable::from_reg(prod),
                lhs: c,
                rhs: splat,
            },
            MInst::VecArithRRR {
                op: VecArithOp::Add,
                shape: VecShape::FourS,
                dst: Writable::from_reg(sum),
                lhs: prod,
                rhs: b,
            },
            // b is used again later: not single-use, so no fusion.
            MInst::VecArithRRR {
                op: VecArithOp::Sub,
                shape: VecShape::FourS,
                dst: Writable::from_reg(c),
                lhs: b,
                rhs: c,
            },
        ];
        insts.reverse();
        let mut vcode = build_vcode(insts.into_iter(), 5);
        let use_counts = build_vreg_use_counts(&mut vcode);
        let fused = combine_vec_mac_in_block(&mut vcode, &use_counts);
        assert_eq!(fused, 0);
    }
}
