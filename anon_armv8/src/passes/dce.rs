//! Pre-RA dead code elimination pass for AArch64.
//!
//! Removes instructions whose results are never used and which have no
//! architectural side effects. Pre-RA VCode is in SSA form (every virtual
//! register is defined exactly once), so a virtual register with zero uses is
//! dead and its defining instruction can be tombstoned, provided the
//! instruction is known to be pure. Elimination runs to a fixpoint so that
//! chains of dead definitions (a value used only by another dead instruction)
//! are removed as well.
//!
//! Two subtle points:
//!
//! * Branch block arguments are uses that live in the VCode side tables
//!   rather than in any instruction's operand list; they are passed in as
//!   `extra_uses` so values feeding successor block parameters stay alive.
//! * Instructions with a non-virtual (physical) definition are never
//!   removed. Physical defs (e.g. pinned ABI registers) are not tracked by
//!   the use counts, so treating them as dead would be unsound.

use rustc_hash::FxHashMap;

use taki_mir::{
    passes::MIRPass,
    prelude::ArenaContext,
    reg_alloc::reg::OperandKind,
    register::Reg,
    stats::FunctionCodegenStats,
    vcode::{MachInst, VCodeContainer},
};

use crate::instructions::MInst;

pub struct DeadCodeElim;

impl MIRPass<MInst> for DeadCodeElim {
    fn name(&self) -> &'static str {
        "DeadCodeElim"
    }

    fn run(
        &self,
        vcode: &mut VCodeContainer<MInst>,
        _arena: ArenaContext,
        stats: &mut FunctionCodegenStats,
    ) -> bool {
        stats.dce.ran = true;
        let extra_uses: Vec<Reg> = vcode
            .branch_block_args()
            .iter()
            .map(|&vreg| Reg::from(vreg))
            .collect();
        let removed = eliminate_dead_insts(vcode.insts_mut(), &extra_uses);
        stats.dce.instructions_removed += removed;
        stats.dce.changed = removed > 0;
        stats.dce.changed
    }
}

/// Per-instruction operand summary used by the fixpoint loop.
#[derive(Default)]
struct InstInfo {
    uses: Vec<Reg>,
    defs: Vec<Reg>,
    removable: bool,
    has_non_virtual_def: bool,
}

impl InstInfo {
    /// An instruction is dead when it is on the removable whitelist, has at
    /// least one definition (instructions without defs are skipped so that
    /// flag writers such as `CmpRR` are never touched), every definition is
    /// virtual, and every defined virtual register has zero remaining uses.
    fn is_dead(&self, use_counts: &FxHashMap<Reg, u32>) -> bool {
        self.removable
            && !self.defs.is_empty()
            && !self.has_non_virtual_def
            && self
                .defs
                .iter()
                .all(|def| use_counts.get(def).copied().unwrap_or(0) == 0)
    }
}

/// Whitelist of instructions that are architecturally pure and may be
/// removed when all of their definitions are dead.
///
/// Includes dead loads (`Load`/`LoadPair`): an AArch64 load has no
/// architectural side effect, and SysY semantics guarantee the address is
/// valid whenever the load executes, so removing an unused load cannot
/// introduce or hide a fault.
///
/// `SDiv` is safe to remove: AArch64 division by zero produces zero rather
/// than trapping.
///
/// Excluded on purpose: terminators, calls, tail calls, stores, `CmpRR` /
/// `CmpImm` / `FCmp` (implicit NZCV definitions), `Nop` (a real instruction
/// with placement semantics), and anything not proven pure.
fn is_dce_removable(inst: &MInst) -> bool {
    matches!(
        inst,
        MInst::AluRRR { .. }
            | MInst::AluRRRR { .. }
            | MInst::AluRRImm12 { .. }
            | MInst::AluRRImmLogic { .. }
            | MInst::AluRRImmShift { .. }
            | MInst::AluRRRShift { .. }
            | MInst::AluRRRExtend { .. }
            | MInst::SDiv { .. }
            | MInst::SMulL { .. }
            | MInst::MAdd { .. }
            | MInst::MSub { .. }
            | MInst::Mov { .. }
            | MInst::MovPhys { .. }
            | MInst::LoadImm { .. }
            | MInst::MovZ { .. }
            | MInst::MovN { .. }
            | MInst::MovK { .. }
            | MInst::MovFromZero { .. }
            | MInst::Sxtw { .. }
            | MInst::LoadAddr { .. }
            | MInst::StackAddr { .. }
            | MInst::CSet { .. }
            | MInst::CmpSelect { .. }
            | MInst::FMov { .. }
            | MInst::VecMov { .. }
            | MInst::VecLd1 { .. }
            | MInst::VecDup { .. }
            | MInst::VecArithRRR { .. }
            | MInst::VecFmla { .. }
            | MInst::VecBitwise { .. }
            | MInst::VecCmp { .. }
            | MInst::VecBsl { .. }
            | MInst::VecCvt { .. }
            | MInst::VecAddv { .. }
            | MInst::VecMovImm { .. }
            | MInst::VecExtractLane { .. }
            | MInst::VecInsertLane { .. }
            | MInst::VecMinMax { .. }
            | MInst::FMovFromZero { .. }
            | MInst::FAlu { .. }
            | MInst::Scvtf { .. }
            | MInst::Fcvtzs { .. }
            | MInst::Load { .. }
            | MInst::LoadPair { .. }
    )
}

/// Tombstone every dead instruction in `insts`, returning the number of
/// eliminated instructions. `extra_uses` are virtual-register uses that do
/// not appear in any instruction operand list (branch block arguments).
fn eliminate_dead_insts(insts: &mut [MInst], extra_uses: &[Reg]) -> u64 {
    let mut use_counts: FxHashMap<Reg, u32> = FxHashMap::default();
    for &reg in extra_uses {
        if reg.is_virtual() {
            *use_counts.entry(reg).or_insert(0) += 1;
        }
    }

    let mut infos: Vec<InstInfo> = Vec::with_capacity(insts.len());
    let mut def_site: FxHashMap<Reg, usize> = FxHashMap::default();
    for (i, inst) in insts.iter_mut().enumerate() {
        let mut info = InstInfo {
            removable: is_dce_removable(inst),
            ..InstInfo::default()
        };
        inst.get_operands(&mut |reg: &mut Reg, _constraint, kind, _pos| match kind {
            OperandKind::Use => {
                if reg.is_virtual() {
                    info.uses.push(*reg);
                    *use_counts.entry(*reg).or_insert(0) += 1;
                }
            }
            OperandKind::Def => {
                info.defs.push(*reg);
                if reg.is_virtual() {
                    def_site.insert(*reg, i);
                } else {
                    info.has_non_virtual_def = true;
                }
            }
        });
        infos.push(info);
    }

    let mut worklist: Vec<usize> = (0..insts.len())
        .filter(|&i| infos[i].is_dead(&use_counts))
        .collect();
    let mut tombstoned = vec![false; insts.len()];
    let mut removed = 0u64;

    while let Some(i) = worklist.pop() {
        if tombstoned[i] || !infos[i].is_dead(&use_counts) {
            continue;
        }
        let uses = std::mem::take(&mut infos[i].uses);
        insts[i] = MInst::Removed;
        tombstoned[i] = true;
        removed += 1;
        for used in uses {
            let Some(count) = use_counts.get_mut(&used) else {
                continue;
            };
            *count -= 1;
            if *count == 0 {
                if let Some(&site) = def_site.get(&used) {
                    worklist.push(site);
                }
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use taki_mir::reg_alloc::reg::{RegClass, VReg};
    use taki_mir::register::Writable;

    use super::*;
    use crate::instructions::{AluOp, MemoryType, PairAMode, SImm7Scaled};
    use crate::regs::{OperandSize, RegOrZr, int_reg};

    fn vreg(index: u32) -> Reg {
        // The first 192 vregs are pinned to physical registers; allocate
        // test vregs above that range so `is_virtual` holds.
        Reg::from_virtual_reg(VReg::new(192 + index as usize, RegClass::Int))
    }

    fn writable(index: u32) -> Writable<Reg> {
        Writable::from_reg(vreg(index))
    }

    fn mov_zero(dst: u32) -> MInst {
        MInst::MovFromZero {
            size: OperandSize::Size32,
            dst: writable(dst),
        }
    }

    fn mov_reg(dst: u32, src: u32) -> MInst {
        MInst::Mov {
            size: OperandSize::Size32,
            dst: writable(dst),
            src: vreg(src),
        }
    }

    fn add(dst: u32, lhs: u32, rhs: u32) -> MInst {
        MInst::AluRRR {
            op: AluOp::Add,
            size: OperandSize::Size32,
            dst: writable(dst),
            lhs: RegOrZr::Reg(vreg(lhs)),
            rhs: RegOrZr::Reg(vreg(rhs)),
        }
    }

    fn load(dst: u32, base: u32) -> MInst {
        MInst::Load {
            ty: MemoryType::I32,
            dst: writable(dst),
            addr: crate::instructions::AMode::Reg { base: vreg(base) },
        }
    }

    fn load_pair(dst1: u32, dst2: u32, base: u32) -> MInst {
        MInst::LoadPair {
            ty: MemoryType::I64,
            dst1: writable(dst1),
            dst2: writable(dst2),
            addr: PairAMode::SignedOffset {
                base: vreg(base),
                offset: SImm7Scaled::new(0, 8).unwrap(),
            },
        }
    }

    fn store(src: u32, base: u32) -> MInst {
        MInst::Store {
            ty: MemoryType::I32,
            src: vreg(src),
            addr: crate::instructions::AMode::Reg { base: vreg(base) },
        }
    }

    fn is_removed(inst: &MInst) -> bool {
        matches!(inst, MInst::Removed)
    }

    #[test]
    fn removes_dead_constant_materialization() {
        let mut insts = vec![mov_zero(0), MInst::Ret];
        let removed = eliminate_dead_insts(&mut insts, &[]);
        assert_eq!(removed, 1);
        assert!(is_removed(&insts[0]));
        assert!(matches!(insts[1], MInst::Ret));
    }

    #[test]
    fn keeps_live_dependency_chain() {
        // v0 -> v1 -> returned through a branch block argument.
        let mut insts = vec![mov_zero(0), mov_reg(1, 0), MInst::Ret];
        let removed = eliminate_dead_insts(&mut insts, &[vreg(1)]);
        assert_eq!(removed, 0);
        assert!(!is_removed(&insts[0]));
        assert!(!is_removed(&insts[1]));
    }

    #[test]
    fn keeps_stores_and_flag_writers() {
        let mut insts = vec![
            mov_zero(0),
            store(0, 1),
            MInst::CmpRR {
                size: OperandSize::Size32,
                lhs: vreg(0),
                rhs: RegOrZr::Reg(vreg(1)),
            },
            MInst::Ret,
        ];
        let removed = eliminate_dead_insts(&mut insts, &[vreg(1)]);
        assert_eq!(removed, 0);
        assert!(matches!(insts[1], MInst::Store { .. }));
        assert!(matches!(insts[2], MInst::CmpRR { .. }));
    }

    #[test]
    fn removes_dead_load() {
        let mut insts = vec![load(0, 1), MInst::Ret];
        let removed = eliminate_dead_insts(&mut insts, &[vreg(1)]);
        assert_eq!(removed, 1);
        assert!(is_removed(&insts[0]));
    }

    #[test]
    fn load_pair_removed_only_when_both_defs_dead() {
        // One live def keeps the whole pair.
        let mut insts = vec![load_pair(0, 1, 2), MInst::Ret];
        let removed = eliminate_dead_insts(&mut insts, &[vreg(1), vreg(2)]);
        assert_eq!(removed, 0);
        assert!(matches!(insts[0], MInst::LoadPair { .. }));

        // Both defs dead: the pair is removed.
        let mut insts = vec![load_pair(0, 1, 2), MInst::Ret];
        let removed = eliminate_dead_insts(&mut insts, &[vreg(2)]);
        assert_eq!(removed, 1);
        assert!(is_removed(&insts[0]));
    }

    #[test]
    fn removes_dead_chains_to_fixpoint() {
        // v1 is used only by the dead v2 computation; both must go.
        let mut insts = vec![mov_zero(0), mov_reg(1, 0), add(2, 1, 1), MInst::Ret];
        let removed = eliminate_dead_insts(&mut insts, &[]);
        assert_eq!(removed, 3);
        assert!(insts[..3].iter().all(is_removed));
    }

    #[test]
    fn keeps_partially_live_chain() {
        // v0 -> v1 -> v2 (dead), but v1 is also returned.
        let mut insts = vec![mov_zero(0), mov_reg(1, 0), add(2, 1, 1), MInst::Ret];
        let removed = eliminate_dead_insts(&mut insts, &[vreg(1)]);
        assert_eq!(removed, 1);
        assert!(!is_removed(&insts[0]));
        assert!(!is_removed(&insts[1]));
        assert!(is_removed(&insts[2]));
    }

    #[test]
    fn never_removes_non_virtual_defs() {
        // A physical-register definition is not tracked by use counts and
        // must never be treated as dead.
        let mut insts = vec![
            MInst::Mov {
                size: OperandSize::Size64,
                dst: Writable::from_reg(int_reg(9)),
                src: vreg(0),
            },
            MInst::Ret,
        ];
        let removed = eliminate_dead_insts(&mut insts, &[vreg(0)]);
        assert_eq!(removed, 0);
        assert!(matches!(insts[0], MInst::Mov { .. }));
    }

    #[test]
    fn skips_nop_and_removed() {
        let mut insts = vec![MInst::Nop, MInst::Removed, MInst::Ret];
        let removed = eliminate_dead_insts(&mut insts, &[]);
        assert_eq!(removed, 0);
        assert!(matches!(insts[0], MInst::Nop));
        assert!(matches!(insts[1], MInst::Removed));
    }

    #[test]
    fn never_removes_args_pseudo() {
        use taki_mir::abi::ArgPair;
        let args = MInst::Args {
            args: vec![ArgPair {
                vreg: Writable::from_reg(vreg(0)),
                preg: int_reg(0),
            }],
        };
        let mut insts = vec![args, MInst::Ret];
        let removed = eliminate_dead_insts(&mut insts, &[]);
        assert_eq!(removed, 0);
        assert!(matches!(insts[0], MInst::Args { .. }));
    }
}
