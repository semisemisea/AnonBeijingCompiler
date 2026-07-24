/*
 * Adapted from regalloc2 0.15.1 src/ion/liveranges.rs.
 *
 * Released under the Apache License 2.0 with LLVM Exception. See the
 * repository LICENSE for the full license text. This local port deliberately
 * stops after analysis; it does not allocate, spill, or emit moves.
 */

use std::collections::VecDeque;

use crate::reg_alloc::{
    function::Function,
    index::{Block, Inst},
    reg::{InstPosition, Operand, OperandConstraint, OperandKind, OperandPos, ProgPoint, VReg},
};

use super::{BlockParamIn, BlockParamOut, CFGInfo, CodeRange, IndexSet, LiveRange, Use};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpillWeight(f32);
impl SpillWeight {
    pub fn zero() -> Self {
        Self(0.0)
    }
    pub fn to_bits(self) -> u16 {
        (self.0.to_bits() >> 15) as u16
    }
    pub fn from_bits(bits: u16) -> Self {
        Self(f32::from_bits((bits as u32) << 15))
    }
    pub fn to_f32(self) -> f32 {
        self.0
    }
}
impl core::ops::Add for SpillWeight {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

pub fn spill_weight_from_constraint(
    constraint: OperandConstraint,
    loop_depth: u32,
    is_def: bool,
) -> SpillWeight {
    let hot = 1000.0 * 4.0f32.powi(loop_depth.min(10) as i32);
    let constraint = match constraint {
        OperandConstraint::Any => 1000.0,
        OperandConstraint::Reg | OperandConstraint::FixedReg(_) => 2000.0,
        _ => 0.0,
    };
    SpillWeight(hot + constraint + if is_def { 2000.0 } else { 0.0 })
}

#[derive(Clone, Debug, Default)]
pub struct Liveness {
    pub liveins: Vec<IndexSet>,
    pub liveouts: Vec<IndexSet>,
    pub blockparam_ins: Vec<BlockParamIn>,
    pub blockparam_outs: Vec<BlockParamOut>,
}

/// Fixed-point live-in/live-out computation. Edge arguments are uses at the
/// predecessor exit; successor block parameters are definitions at entry.
pub fn compute_liveness<F: Function>(function: &F, cfg: &CFGInfo) -> Result<Liveness, String> {
    let blocks = function.num_blocks();
    let mut result = Liveness {
        liveins: vec![IndexSet::new(); blocks],
        liveouts: vec![IndexSet::new(); blocks],
        ..Liveness::default()
    };
    let mut queued = vec![false; blocks];
    let mut work: VecDeque<_> = cfg.postorder.iter().copied().collect();
    for &block in &cfg.postorder {
        queued[block.index()] = true;
    }
    while let Some(block) = work.pop_front() {
        queued[block.index()] = false;
        let insns = function.block_insns(block);
        if function.is_branch(insns.last()) {
            for successor in 0..function.block_succs(block).len() {
                for &arg in function.branch_blockparams(block, insns.last(), successor) {
                    result.liveouts[block.index()].set(arg.vreg(), true);
                }
            }
        }
        let mut live = result.liveouts[block.index()].clone();
        for inst in insns.iter().rev() {
            for position in [OperandPos::Late, OperandPos::Early] {
                for &operand in function.inst_operands(inst) {
                    if operand.as_fixed_nonallocatable().is_some() || operand.pos() != position {
                        continue;
                    }
                    live.set(operand.vreg().vreg(), operand.kind() == OperandKind::Use);
                }
            }
        }
        for &param in function.block_params(block) {
            live.set(param.vreg(), false);
        }
        for &pred in function.block_preds(block) {
            if result.liveouts[pred.index()].union_with(&live) && !queued[pred.index()] {
                queued[pred.index()] = true;
                work.push_back(pred);
            }
        }
        result.liveins[block.index()] = live;
    }
    if !result.liveins[function.entry_block().index()].is_empty() {
        return Err("entry block has live-in virtual registers".into());
    }
    Ok(result)
}

/// Build per-vreg live ranges and record SSA edge copies for later coalescing.
pub fn build_live_ranges<F: Function>(
    function: &F,
    cfg: &CFGInfo,
    liveness: &mut Liveness,
) -> Vec<Vec<LiveRange>> {
    let mut result = vec![Vec::new(); function.num_vregs()];
    liveness.blockparam_ins.clear();
    liveness.blockparam_outs.clear();
    for block_index in (0..function.num_blocks()).rev() {
        let block = Block::new(block_index);
        let insns = function.block_insns(block);
        let mut live = liveness.liveouts[block_index].clone();
        let mut current: Vec<Option<usize>> = vec![None; function.num_vregs()];
        if function.is_branch(insns.last()) {
            for (successor_index, &to_block) in function.block_succs(block).iter().enumerate() {
                for (&to_vreg, &from_vreg) in function
                    .block_params(to_block)
                    .iter()
                    .zip(function.branch_blockparams(block, insns.last(), successor_index))
                {
                    liveness.blockparam_outs.push(BlockParamOut {
                        from_block: block,
                        to_block,
                        from_vreg,
                        to_vreg,
                    });
                    live.set(from_vreg.vreg(), true);
                }
            }
        }
        let whole_block = CodeRange {
            from: cfg.block_entry[block_index],
            to: cfg.block_exit[block_index].next(),
        };
        for vreg_index in live.iter() {
            if vreg_index < result.len() {
                current[vreg_index] = Some(push_range(
                    &mut result[vreg_index],
                    VReg::new(vreg_index, vreg_class(function, vreg_index)),
                    whole_block,
                ));
            }
        }
        for inst in insns.iter().rev() {
            for phase in [InstPosition::After, InstPosition::Before] {
                for (slot, &operand) in function.inst_operands(inst).iter().enumerate() {
                    if operand.as_fixed_nonallocatable().is_some() {
                        continue;
                    }
                    let pos = operand_point(function, inst, operand);
                    if pos.pos() != phase {
                        continue;
                    }
                    let index = operand.vreg().vreg();
                    if index >= result.len() {
                        continue;
                    }
                    match operand.kind() {
                        OperandKind::Def => {
                            let range = if live.get(index) {
                                current[index].expect("live vreg must have a range")
                            } else {
                                let range = CodeRange {
                                    from: pos,
                                    to: ProgPoint::before(pos.inst() + 1),
                                };
                                let range = push_range(&mut result[index], operand.vreg(), range);
                                current[index] = Some(range);
                                live.set(index, true);
                                range
                            };
                            result[index][range]
                                .uses
                                .push(make_use(operand, pos, slot, cfg, block));
                            result[index][range].starts_at_def = true;
                            if result[index][range].range.from == cfg.block_entry[block_index] {
                                result[index][range].range.from = pos;
                            }
                            live.set(index, false);
                            current[index] = None;
                        }
                        OperandKind::Use => {
                            let range = if live.get(index) {
                                current[index].expect("live vreg must have a range")
                            } else {
                                let range = push_range(
                                    &mut result[index],
                                    operand.vreg(),
                                    CodeRange {
                                        from: cfg.block_entry[block_index],
                                        to: pos.next(),
                                    },
                                );
                                current[index] = Some(range);
                                range
                            };
                            result[index][range]
                                .uses
                                .push(make_use(operand, pos, slot, cfg, block));
                            live.set(index, true);
                        }
                    }
                }
            }
        }
        for &param in function.block_params(block) {
            if param.vreg() < result.len() && !live.get(param.vreg()) {
                push_range(
                    &mut result[param.vreg()],
                    param,
                    CodeRange::singleton(cfg.block_entry[block_index]),
                );
            }
            for &pred in function.block_preds(block) {
                liveness.blockparam_ins.push(BlockParamIn {
                    from_block: pred,
                    to_block: block,
                    to_vreg: param,
                });
            }
        }
    }
    for ranges in &mut result {
        ranges.sort_unstable_by_key(|range| range.range.from);
        for range in ranges {
            range.uses.sort_unstable_by_key(|use_| use_.pos);
        }
    }
    liveness.blockparam_ins.sort_unstable();
    liveness.blockparam_outs.sort_unstable();
    result
}

fn push_range(ranges: &mut Vec<LiveRange>, vreg: VReg, range: CodeRange) -> usize {
    if let Some((index, previous)) = ranges
        .iter_mut()
        .enumerate()
        .find(|(_, previous)| previous.range.from == range.to)
    {
        previous.range.from = range.from;
        index
    } else {
        ranges.push(LiveRange {
            range,
            vreg,
            uses: Vec::new(),
            starts_at_def: false,
        });
        ranges.len() - 1
    }
}
fn vreg_class<F: Function>(function: &F, index: usize) -> crate::reg_alloc::reg::RegClass {
    for block in 0..function.num_blocks() {
        for &vreg in function.block_params(Block::new(block)) {
            if vreg.vreg() == index {
                return vreg.class();
            }
        }
        for inst in function.block_insns(Block::new(block)).iter() {
            for &operand in function.inst_operands(inst) {
                if operand.vreg().vreg() == index {
                    return operand.vreg().class();
                }
            }
        }
    }
    crate::reg_alloc::reg::RegClass::Int
}
fn operand_point<F: Function>(function: &F, inst: Inst, operand: Operand) -> ProgPoint {
    match (operand.kind(), operand.pos()) {
        (OperandKind::Def, OperandPos::Early) => ProgPoint::before(inst.raw_u32()),
        (OperandKind::Def, OperandPos::Late) | (OperandKind::Use, OperandPos::Late) => {
            ProgPoint::after(inst.raw_u32())
        }
        (OperandKind::Use, OperandPos::Early) => {
            let reused = function
                .inst_operands(inst)
                .iter()
                .find_map(|op| match op.constraint() {
                    OperandConstraint::Reuse(i) => Some(function.inst_operands(inst)[i].vreg()),
                    _ => None,
                });
            if reused.is_some_and(|vreg| vreg != operand.vreg()) {
                ProgPoint::after(inst.raw_u32())
            } else {
                ProgPoint::before(inst.raw_u32())
            }
        }
    }
}
fn make_use(operand: Operand, pos: ProgPoint, slot: usize, cfg: &CFGInfo, block: Block) -> Use {
    Use {
        operand,
        pos,
        slot: u16::try_from(slot).expect("too many instruction operands"),
        weight: spill_weight_from_constraint(
            operand.constraint(),
            cfg.approx_loop_depth[block.index()],
            operand.kind() == OperandKind::Def,
        )
        .to_bits(),
    }
}

#[cfg(test)]
mod tests {
    use crate::reg_alloc::{
        function::Function,
        index::{Block, Inst, InstRange},
        reg::{Operand, PRegSet, RegClass, VReg},
    };

    use super::*;
    use crate::reg_alloc::ion::{CFGInfo, merge_vreg_bundles};

    struct EdgeFunction {
        source: VReg,
        operands: [Vec<Operand>; 2],
        params: [Vec<VReg>; 2],
        succs: [Vec<Block>; 2],
        preds: [Vec<Block>; 2],
    }

    impl Function for EdgeFunction {
        fn num_insts(&self) -> usize {
            2
        }
        fn num_blocks(&self) -> usize {
            2
        }
        fn entry_block(&self) -> Block {
            Block::new(0)
        }
        fn block_insns(&self, block: Block) -> InstRange {
            InstRange::new(Inst::new(block.index()), Inst::new(block.index() + 1))
        }
        fn block_succs(&self, block: Block) -> &[Block] {
            &self.succs[block.index()]
        }
        fn block_preds(&self, block: Block) -> &[Block] {
            &self.preds[block.index()]
        }
        fn block_params(&self, block: Block) -> &[VReg] {
            &self.params[block.index()]
        }
        fn is_ret(&self, _inst: Inst) -> bool {
            false
        }
        fn is_branch(&self, inst: Inst) -> bool {
            inst == Inst::new(0)
        }
        fn branch_blockparams(&self, block: Block, _inst: Inst, _succ_idx: usize) -> &[VReg] {
            if block == Block::new(0) {
                core::slice::from_ref(&self.source)
            } else {
                &[]
            }
        }
        fn inst_operands(&self, inst: Inst) -> &[Operand] {
            &self.operands[inst.index()]
        }
        fn inst_clobbers(&self, _inst: Inst) -> PRegSet {
            PRegSet::empty()
        }
        fn num_vregs(&self) -> usize {
            2
        }
        fn spillslot_size(&self, _regclass: RegClass) -> usize {
            1
        }
    }

    #[test]
    fn block_param_edge_is_live_out_and_coalesced() {
        let source = VReg::new(0, RegClass::Int);
        let param = VReg::new(1, RegClass::Int);
        let function = EdgeFunction {
            source,
            operands: [
                vec![Operand::reg_def(source)],
                vec![Operand::reg_use(param)],
            ],
            params: [vec![], vec![param]],
            succs: [vec![Block::new(1)], vec![]],
            preds: [vec![], vec![Block::new(0)]],
        };
        let cfg = CFGInfo::new(&function).unwrap();
        let mut liveness = compute_liveness(&function, &cfg).unwrap();
        assert!(liveness.liveouts[0].get(source.vreg()));

        let ranges = build_live_ranges(&function, &cfg, &mut liveness);
        assert_eq!(liveness.blockparam_outs.len(), 1);
        assert_eq!(liveness.blockparam_outs[0].from_vreg, source);
        assert_eq!(liveness.blockparam_outs[0].to_vreg, param);
        let bundles = merge_vreg_bundles(&function, &ranges, &liveness);
        assert_eq!(bundles.bundle(source), bundles.bundle(param));
    }
}
