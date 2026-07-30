//! Pre-RA peephole combine pass for AArch64.
//!
//! Scans each basic block forward and fuses instruction pairs that the
//! hand-written ISel missed (or that were introduced by earlier IR passes)
//! into single AArch64 fused-form instructions.

use std::collections::HashMap;

use taki_mir::{
    passes::MIRPass,
    prelude::ArenaContext,
    reg_alloc::reg::{OperandKind, OperandVisitor as _},
    register::Reg,
    stats::FunctionCodegenStats,
    vcode::{MachInst, VCodeContainer},
};

use crate::instructions::{AluOp, MInst};
use crate::regs::RegOrZr;

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
        stats.peephole.changed = changed;
        changed
    }
}

/// Count how many times each virtual register appears as a `Use` operand
/// across the entire function. A register with a count of 1 is a candidate
/// for fusion — its producer can be absorbed into the sole consumer.
fn build_vreg_use_counts(vcode: &mut VCodeContainer<MInst>) -> HashMap<Reg, u32> {
    let mut counts: HashMap<Reg, u32> = HashMap::new();
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
    use_counts: &HashMap<Reg, u32>,
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
            *vcode.inst_mut(i) = MInst::Nop;
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
