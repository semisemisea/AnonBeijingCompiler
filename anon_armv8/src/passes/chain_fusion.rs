//! Pre-RA chain fusion for AArch64.
//!
//! The `chain_to_switch` IR pass shapes equality chains into balanced
//! decision trees where every internal node is a pair of blocks:
//!
//! ```text
//! check_k:  %t = eq x, k;    br %t, handler_k, split_k
//! split_k:  %u = lt x, k;    br %u, left_subtree, right_subtree
//! ```
//!
//! Both blocks lower to `CmpImm x, k` + `CondBr` (the backend lowers the
//! comparison and the branch together). This pass removes the second
//! comparison: the split block's `CmpImm` is identical to the check block's,
//! and the split block has exactly one predecessor (the check block), so the
//! flags its `CondBr` needs are produced by the check block's compare and
//! survive the edge between the two blocks (branches do not modify NZCV).
//!
//! The fused form emits as clang's `rotrN` shape:
//!
//! ```text
//! cmp w1, #4
//! b.eq <case 4>
//! b.lt <left>
//! b.ge <right>
//! ```

use taki_mir::{
    passes::MIRPass,
    prelude::ArenaContext,
    reg_alloc::{
        function::Function,
        index::{Block, Inst},
        reg::VReg,
    },
    stats::FunctionCodegenStats,
    vcode::{MachInst, VCodeContainer},
};

use crate::instructions::{Imm12, MInst};

pub struct ChainFusion;

impl MIRPass<MInst> for ChainFusion {
    fn name(&self) -> &'static str {
        "ChainFusion"
    }

    fn run(
        &self,
        vcode: &mut VCodeContainer<MInst>,
        _arena: ArenaContext,
        stats: &mut FunctionCodegenStats,
    ) -> bool {
        let mut fused = 0u64;
        let block_count = vcode.num_blocks();
        for block_idx in 0..block_count {
            fused += fuse_block(vcode, Block::new(block_idx));
        }
        stats.chain_fusion.fusions += fused;
        stats.chain_fusion.changed = fused > 0;
        fused > 0
    }
}

/// Whether the split block's `CmpImm x, k` can be removed because the
/// predecessor tail provides the same flags. The predecessor's branch must
/// reach the split block so the flags flow along the edge.
fn removable_compare(
    split_cmp: &MInst,
    pred_cmp: &MInst,
    pred_br: &MInst,
    split_idx: usize,
) -> bool {
    let MInst::CmpImm { size, lhs, imm } = split_cmp else {
        return false;
    };
    let MInst::CondBr {
        cond: _,
        true_label,
        false_label,
    } = pred_br
    else {
        return false;
    };
    let reaches_split = true_label.block().map(|b| b.index()) == Some(split_idx)
        || false_label.block().map(|b| b.index()) == Some(split_idx);
    if !reaches_split {
        return false;
    }
    match pred_cmp {
        MInst::CmpImm {
            size: pred_size,
            lhs: pred_lhs,
            imm: pred_imm,
        } => {
            pred_size == size
                && pred_lhs == lhs
                && pred_imm.value() == imm.value()
                && pred_imm.shift12() == imm.shift12()
        }
        _ => false,
    }
}

/// Try to fuse the (check, split) pair in front of `split_block`.
///
/// The split block must contain exactly `CmpImm x, k` followed by `CondBr`,
/// have exactly one predecessor, and that predecessor's terminator region
/// must end with `CmpImm x, k` followed by a `CondBr` that targets the split
/// block. When the pattern matches, the split block's compare is removed
/// (its branch reads the predecessor's flags instead).
fn fuse_block(vcode: &mut VCodeContainer<MInst>, split: Block) -> u64 {
    let split_idx = split.index();
    let range = vcode.block_inst_range(split_idx);
    let mut insts = range.filter(|&i| !matches!(vcode.inst(i), MInst::Removed));
    let Some(cmp_idx) = insts.next() else {
        return 0;
    };
    let Some(br_idx) = insts.next() else {
        return 0;
    };
    if insts.next().is_some() {
        return 0;
    }
    let split_cmp = vcode.inst(cmp_idx).clone();
    let split_br = vcode.inst(br_idx).clone();

    // The split block must be entered from exactly one predecessor.
    let preds = vcode.block_preds(split);
    if preds.len() != 1 {
        return 0;
    }
    let pred_idx = preds[0].index();
    // The predecessor's terminator region must end with a compare followed
    // by a conditional branch.
    let pred_range = vcode.block_inst_range(pred_idx);
    let mut tail = pred_range
        .rev()
        .filter(|&i| !matches!(vcode.inst(i), MInst::Removed));
    let Some(pred_br_idx) = tail.next() else {
        return 0;
    };
    let Some(pred_cmp_idx) = tail.next() else {
        return 0;
    };
    let pred_cmp = vcode.inst(pred_cmp_idx).clone();
    let pred_br = vcode.inst(pred_br_idx).clone();
    if !matches!(&split_br, MInst::CondBr { .. })
        || !removable_compare(&split_cmp, &pred_cmp, &pred_br, split_idx)
    {
        return 0;
    }

    *vcode.inst_mut(cmp_idx) = MInst::Removed;
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{instructions::Cond, labels::Label, regs::OperandSize};
    use taki_mir::{
        block_order::MirBlockIndex,
        reg_alloc::reg::{RegClass, VReg},
        register::{Reg, Writable},
    };

    fn vreg(index: u32) -> Reg {
        Reg::from_virtual_reg(VReg::new(192 + index as usize, RegClass::Int))
    }

    fn cmp(reg: u32, value: u16) -> MInst {
        MInst::CmpImm {
            size: OperandSize::Size32,
            lhs: vreg(reg),
            imm: Imm12::new(value, false).unwrap(),
        }
    }

    fn cond_br(cond: Cond, true_idx: usize, false_idx: usize) -> MInst {
        MInst::CondBr {
            cond,
            true_label: Label::from_block(MirBlockIndex::new(true_idx)),
            false_label: Label::from_block(MirBlockIndex::new(false_idx)),
        }
    }

    #[test]
    fn identical_compare_with_reaching_branch_fuses() {
        // The split block (0) is reached through the false arm of the check
        // block's branch; both compare x against #4.
        assert!(removable_compare(
            &cmp(1, 4),
            &cmp(1, 4),
            &cond_br(Cond::Eq, 2, 0),
            0,
        ));
    }

    #[test]
    fn fused_form_also_matches_the_true_arm() {
        assert!(removable_compare(
            &cmp(1, 4),
            &cmp(1, 4),
            &cond_br(Cond::Ne, 0, 3),
            0,
        ));
    }

    #[test]
    fn refuses_unreaching_branch_or_mismatched_compare() {
        // The branch does not reach the split block.
        assert!(!removable_compare(
            &cmp(1, 4),
            &cmp(1, 4),
            &cond_br(Cond::Eq, 2, 3),
            0,
        ));
        // Different immediate.
        assert!(!removable_compare(
            &cmp(1, 4),
            &cmp(1, 5),
            &cond_br(Cond::Eq, 2, 0),
            0,
        ));
        // Different register.
        assert!(!removable_compare(
            &cmp(1, 4),
            &cmp(2, 4),
            &cond_br(Cond::Eq, 2, 0),
            0,
        ));
        // The predecessor must end with a compare.
        let mov = MInst::Mov {
            size: OperandSize::Size32,
            dst: Writable::from_reg(vreg(9)),
            src: vreg(1),
        };
        assert!(!removable_compare(
            &cmp(1, 4),
            &mov,
            &cond_br(Cond::Eq, 2, 0),
            0,
        ));
    }
}
