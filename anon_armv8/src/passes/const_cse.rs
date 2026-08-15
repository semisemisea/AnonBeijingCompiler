//! Hoist loop-invariant constant materializations to loop preheaders and
//! CSE duplicate materializations (M65).
//!
//! The AArch64 lowering materializes a magic-number multiplier/divisor at
//! every `Div`/`Rem` site as a `LoadImm`, inside hot loops that is
//! `movz`/`movk` per iteration of pure loop-invariant work (h-4: 12
//! constants/iteration, fft0: 6×P + 2×magic). This pass finds natural loops
//! with a unique preheader, moves every `LoadImm`/`MovFromZero` inside the
//! loop to the preheader (standard LICM: the preheader dominates every loop
//! block, so the moved def still dominates every use), and dedupes identical
//! materializations so only one survives per constant.
//!
//! The pre-RA VCode is SSA (each vreg defined exactly once), so redirecting
//! uses of an eliminated materialization to the surviving def is sound as
//! long as that def dominates the uses, which the preheader guarantees.
//! Branch block arguments live in the CFG side table and are rewritten too.

use std::collections::HashMap;

use taki_mir::{
    passes::MIRPass,
    prelude::ArenaContext,
    reg_alloc::{
        function::Function,
        index::Block,
        reg::OperandKind,
    },
    register::Reg,
    stats::FunctionCodegenStats,
    vcode::{MachInst, VCodeContainer},
};

use crate::instructions::MInst;

pub struct ConstCse;

impl MIRPass<MInst> for ConstCse {
    fn name(&self) -> &'static str {
        "ConstCse"
    }

    fn run(
        &self,
        vcode: &mut VCodeContainer<MInst>,
        _arena: ArenaContext,
        _stats: &mut FunctionCodegenStats,
    ) -> bool {
        let Some(plan) = plan_const_cse(vcode) else {
            return false;
        };
        let applied = apply_const_cse(vcode, &plan);
        if applied {
            vcode.rebuild_operand_tables();
        }
        applied
    }
}

/// A constant materialization is only hoisted out of loops whose total
/// instruction count stays under this limit. Hoisting makes the constant live
/// across the whole loop; large loops (many live values, e.g. huffman's decode
/// loop) would spill the extra registers and lose more than the hoist saves.
const HOIST_BODY_LIMIT: usize = 60;

/// Loops above `HOIST_BODY_LIMIT` are still eligible when the hoist is tiny
/// (at most this many constant materializations total in the loop) and the
/// loop is not pathologically large, e.g. h-1's outer loop (~75 instructions,
/// 2 constants: `n*3+1`/`n*4+1` multipliers) whose per-iteration
/// `movz`/`movk` would otherwise stay.
const TINY_HOIST_LIMIT: usize = 3;
const TINY_HOIST_BODY_LIMIT: usize = 150;

/// An innermost natural loop with a unique preheader.
struct LoopInfo {
    /// All blocks of the loop, including the header.
    blocks: Vec<Block>,
    /// The unique block that dominates the header and is not part of the
    /// loop. `None` when the loop has zero or multiple such predecessors
    /// (irreducible / multiple-entry), in which case hoisting is skipped.
    preheader: Option<Block>,
}

struct Plan {
    /// `(original_inst_index, target_preheader_block)` for every surviving
    /// materialization that must be moved to its loop preheader.
    movers: Vec<(usize, Block)>,
    /// `(eliminated_vreg, surviving_vreg)` use redirections.
    redirects: Vec<(Reg, Reg)>,
}

fn plan_const_cse(vcode: &VCodeContainer<MInst>) -> Option<Plan> {
    let blocks = vcode.num_blocks();
    if blocks <= 1 {
        return None;
    }
    let dominates = compute_dominates(vcode, blocks);
    let loops = find_loops(vcode, &dominates, blocks);

    // Only loops small enough to absorb the hoisted constants' live ranges
    // participate (see `HOIST_BODY_LIMIT`); big loops still participate when
    // the hoist is tiny (see `TINY_HOIST_*`). Count every materialization in
    // the loop's blocks (inner loops count toward every containing loop) as a
    // conservative proxy for the register pressure the hoist would add.
    let mut per_loop_materializations = vec![0usize; loops.len()];
    for (index, loop_info) in loops.iter().enumerate() {
        for &block in &loop_info.blocks {
            per_loop_materializations[index] += vcode
                .block_inst_range(block.index())
                .filter(|&i| const_key(vcode.inst(i)).is_some())
                .count();
        }
    }
    let hoistable: Vec<bool> = loops
        .iter()
        .enumerate()
        .map(|(index, loop_info)| {
            if loop_info.preheader.is_none() {
                return false;
            }
            let total: usize = loop_info
                .blocks
                .iter()
                .map(|block| vcode.block_inst_range(block.index()).len())
                .sum();
            total <= HOIST_BODY_LIMIT
                || (per_loop_materializations[index] <= TINY_HOIST_LIMIT
                    && total <= TINY_HOIST_BODY_LIMIT)
        })
        .collect();
    if std::env::var("SOYO_CONST_CSE_DEBUG").is_ok() {
        eprintln!(
            "[const_cse] blocks={blocks} loops={} sizes={:?} mat={:?} hoist={:?}",
            loops.len(),
            loops
                .iter()
                .map(|l| l
                    .blocks
                    .iter()
                    .map(|b| vcode.block_inst_range(b.index()).len())
                    .sum::<usize>())
                .collect::<Vec<_>>(),
            per_loop_materializations,
            hoistable,
        );
    }

    // Innermost hoistable loop preheader per block. Hoisting to the innermost
    // preheader keeps the constant's live range as small as possible.
    let mut hoist_target: Vec<Option<Block>> = vec![None; blocks];
    for b in 0..blocks {
        let mut best: Option<usize> = None;
        for (index, loop_info) in loops.iter().enumerate() {
            if !hoistable[index] {
                continue;
            }
            let Some(preheader) = loop_info.preheader else {
                continue;
            };
            if loop_info.blocks.contains(&Block::new(b))
                && best.is_none_or(|cur| loop_info.blocks.len() < loops[cur].blocks.len())
            {
                best = Some(index);
                hoist_target[b] = Some(preheader);
            }
        }
    }
    if hoist_target.iter().all(|target| target.is_none()) {
        return None;
    }

    // Group surviving materializations by (target preheader, constant). The
    // first occurrence is the leader (moved); the rest are eliminated and
    // their uses redirected to the leader.
    let mut groups: HashMap<(Block, u8, u64), Vec<usize>> = HashMap::new();
    for block in 0..blocks {
        let Some(target) = hoist_target[block] else {
            continue;
        };
        for i in vcode.block_inst_range(block) {
            let Some((size, value)) = const_key(vcode.inst(i)) else {
                continue;
            };
            groups
                .entry((target, size, value))
                .or_default()
                .push(i);
        }
    }
    if groups.is_empty() {
        return None;
    }

    let mut plan = Plan {
        movers: Vec::new(),
        redirects: Vec::new(),
    };
    for ((target, _size, _value), group) in groups {
        let leader = group[0];
        plan.movers.push((leader, target));
        let leader_reg = def_reg(vcode.inst(leader));
        for &victim in &group[1..] {
            plan.redirects.push((def_reg(vcode.inst(victim)), leader_reg));
        }
    }
    if std::env::var("SOYO_CONST_CSE_DEBUG").is_ok() {
        eprintln!(
            "[const_cse] blocks={blocks} loops={} sizes={:?} mat={:?} hoist={:?} movers={}",
            loops.len(),
            loops
                .iter()
                .map(|l| l
                    .blocks
                    .iter()
                    .map(|b| vcode.block_inst_range(b.index()).len())
                    .sum::<usize>())
                .collect::<Vec<_>>(),
            per_loop_materializations,
            hoistable,
            plan.movers.len(),
        );
    }
    Some(plan)
}

fn apply_const_cse(vcode: &mut VCodeContainer<MInst>, plan: &Plan) -> bool {
    if plan.movers.is_empty() {
        return false;
    }

    // 1. Redirect uses of eliminated vregs inside instructions.
    let num_insts = vcode.num_insts();
    for i in 0..num_insts {
        vcode
            .inst_mut(i)
            .get_operands(&mut |reg: &mut Reg, _constraint, kind, _pos| {
                if kind == OperandKind::Use {
                    for &(from, to) in &plan.redirects {
                        if *reg == from {
                            *reg = to;
                            break;
                        }
                    }
                }
            });
    }

    // 2. Redirect uses that live in the CFG side table (branch block args).
    for &(from, to) in &plan.redirects {
        vcode.rewrite_branch_block_args(from, to);
    }

    // 3. Rebuild the instruction stream in block order. Eliminated
    //    materializations are dropped; surviving ones are placed at the end
    //    of their target preheader, just before the terminator.
    let blocks = vcode.num_blocks();
    let mut mover_target: Vec<Option<Block>> = vec![None; vcode.num_insts()];
    let mut moved: HashMap<Block, Vec<usize>> = HashMap::new();
    for &(i, target) in &plan.movers {
        mover_target[i] = Some(target);
        moved.entry(target).or_default().push(i);
    }
    let mut eliminated = vec![false; vcode.num_insts()];
    for &(i, _) in &plan.movers {
        eliminated[i] = true; // leaders are relocated, not eliminated
    }

    let mut new_insts: Vec<MInst> = Vec::with_capacity(vcode.num_insts());
    let mut new_block_range = taki_mir::vcode::Ranges::default();
    for block in 0..blocks {
        let block = Block::new(block);
        let range = vcode.block_inst_range(block.index());
        let terminator = range.end - 1;
        for i in range.start..terminator {
            if eliminated[i] || mover_target[i].is_some() {
                continue;
            }
            new_insts.push(vcode.inst(i).clone());
        }
        for &i in moved.get(&block).into_iter().flatten() {
            new_insts.push(vcode.inst(i).clone());
        }
        new_insts.push(vcode.inst(terminator).clone());
        new_block_range.push_end(new_insts.len());
    }
    vcode.set_insts_and_block_range(new_insts, new_block_range);
    true
}

/// Constant-materialization key of an instruction: `(operand size, value)`.
/// `LoadImm` is the lowering's generic form; single-instruction `MovZ`/`MovN`
/// (a complete constant in one step) and `MovFromZero` carry the same
/// constant payload and participate too. `MovK` alone is only a partial value
/// of a longer chain and is left alone.
fn const_key(inst: &MInst) -> Option<(u8, u64)> {
    match inst {
        MInst::LoadImm { size, value, .. } => Some((size.bits(), *value)),
        MInst::MovFromZero { size, .. } => Some((size.bits(), 0)),
        MInst::MovZ { size, imm, .. } => {
            Some((size.bits(), u64::from(imm.bits()) << imm.shift()))
        }
        MInst::MovN { size, imm, .. } => {
            let value = !(u64::from(imm.bits()) << imm.shift());
            let mask = if size.bits() == 32 { 0xFFFF_FFFF } else { u64::MAX };
            Some((size.bits(), value & mask))
        }
        _ => None,
    }
}

/// The virtual register defined by a constant materialization.
fn def_reg(inst: &MInst) -> Reg {
    match inst {
        MInst::LoadImm { dst, .. }
        | MInst::MovFromZero { dst, .. }
        | MInst::MovZ { dst, .. }
        | MInst::MovN { dst, .. } => dst.to_reg(),
        _ => unreachable!("const_key only accepts constant materializations"),
    }
}

/// `dominates[A][D]` == "block `D` dominates block `A`" (entry is 0).
fn compute_dominates(vcode: &VCodeContainer<MInst>, blocks: usize) -> Vec<Vec<bool>> {
    let all_true: Vec<bool> = vec![true; blocks];
    let mut dominates = vec![all_true; blocks];
    dominates[0] = (0..blocks).map(|index| index == 0).collect();
    loop {
        let mut changed = false;
        for block in 1..blocks {
            let preds = vcode.block_preds(Block::new(block));
            if preds.is_empty() {
                continue;
            }
            let mut next = vec![true; blocks];
            for pred in preds {
                for (index, value) in next.iter_mut().enumerate() {
                    *value &= dominates[pred.index()][index];
                }
            }
            next[block] = true;
            if next != dominates[block] {
                dominates[block] = next;
                changed = true;
            }
        }
        if !changed {
            return dominates;
        }
    }
}

/// Find every natural loop (by backedges) and its unique preheader.
fn find_loops(
    vcode: &VCodeContainer<MInst>,
    dominates: &[Vec<bool>],
    blocks: usize,
) -> Vec<LoopInfo> {
    // Reverse reachability from a backedge source, avoiding the header.
    let reach_avoiding = |start: Block, header: Block| {
        let mut reach = vec![false; blocks];
        let mut stack = vec![start];
        reach[start.index()] = true;
        while let Some(block) = stack.pop() {
            for &pred in vcode.block_preds(block) {
                if pred == header || reach[pred.index()] {
                    continue;
                }
                reach[pred.index()] = true;
                stack.push(pred);
            }
        }
        reach
    };

    // Union the reach sets of every backedge that targets the same header.
    let mut header_reach: HashMap<Block, Vec<bool>> = HashMap::new();
    for u in 0..blocks {
        let u = Block::new(u);
        for &v in vcode.block_succs(u) {
            if !dominates[u.index()][v.index()] {
                continue; // not a backedge (the edge target dominates its source)
            }
            let reach = reach_avoiding(u, v);
            match header_reach.get_mut(&v) {
                Some(acc) => {
                    for (index, value) in reach.iter().enumerate() {
                        acc[index] |= *value;
                    }
                }
                None => {
                    header_reach.insert(v, reach);
                }
            }
        }
    }

    let mut loops = Vec::new();
    for (header, reach) in header_reach {
        let mut loop_blocks = vec![header];
        for w in 0..blocks {
            if w != header.index() && reach[w] && dominates[w][header.index()] {
                loop_blocks.push(Block::new(w));
            }
        }
        // Unique preheader: the single predecessor of the header that
        // dominates it and is not part of the loop.
        let preheaders: Vec<Block> = vcode
            .block_preds(header)
            .iter()
            .copied()
            .filter(|pred| pred != &header)
            .filter(|pred| dominates[header.index()][pred.index()])
            .filter(|pred| !loop_blocks.contains(pred))
            .collect();
        let preheader = if preheaders.len() == 1 {
            Some(preheaders[0])
        } else {
            None
        };
        loops.push(LoopInfo {
            blocks: loop_blocks,
            preheader,
        });
    }
    loops
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instructions::MoveWideConst;
    use crate::regs::{OperandSize, int_reg};
    use taki_mir::register::Writable;

    #[test]
    fn const_key_classifies_materializations() {
        let load = MInst::LoadImm {
            size: OperandSize::Size32,
            dst: Writable::from_reg(int_reg(0)),
            value: 42,
        };
        assert_eq!(const_key(&load), Some((32, 42)));
        assert_eq!(def_reg(&load), int_reg(0));

        let zero = MInst::MovFromZero {
            size: OperandSize::Size64,
            dst: Writable::from_reg(int_reg(1)),
        };
        assert_eq!(const_key(&zero), Some((64, 0)));
        assert_eq!(def_reg(&zero), int_reg(1));

        let movz = MInst::MovZ {
            size: OperandSize::Size32,
            dst: Writable::from_reg(int_reg(2)),
            imm: MoveWideConst::new(0x1, 0, OperandSize::Size32).unwrap(),
        };
        assert_eq!(const_key(&movz), Some((32, 1)), "single-step movz is a constant");
        assert_eq!(def_reg(&movz), int_reg(2));
        let movn = MInst::MovN {
            size: OperandSize::Size32,
            dst: Writable::from_reg(int_reg(3)),
            imm: MoveWideConst::new(0x2, 16, OperandSize::Size32).unwrap(),
        };
        assert_eq!(
            const_key(&movn),
            Some((32, 0xFFFF_FFFF & !(0x2u64 << 16))),
            "movn is the bitwise complement of its shifted immediate"
        );
        // A `movk` alone is only a partial value of a longer chain.
        let movk = MInst::MovK {
            size: OperandSize::Size32,
            dst: Writable::from_reg(int_reg(4)),
            src: int_reg(5),
            imm: MoveWideConst::new(0x1, 16, OperandSize::Size32).unwrap(),
        };
        assert_eq!(const_key(&movk), None, "standalone movk is not a complete constant");
        let sxtw = MInst::Sxtw {
            size: OperandSize::Size64,
            dst: Writable::from_reg(int_reg(6)),
            src: int_reg(7),
        };
        assert_eq!(const_key(&sxtw), None);
    }
}
