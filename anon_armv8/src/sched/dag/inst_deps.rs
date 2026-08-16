//! Per-instruction register/NZCV/memory dependency extraction.

use super::memory::{MemAccess, MemKind, unknown_access};
use super::*;

/// Dependency metadata extracted from a single instruction.
pub struct InstDeps {
    pub defs: Vec<PReg>,
    pub uses: Vec<PReg>,
    pub flags_def: bool,
    pub flags_use: bool,
    pub class: SchedClass,
    pub mem: Option<MemAccess>,
    pub is_barrier: bool,
}

impl InstDeps {
    pub(super) fn profile(&self) -> InstrProfile {
        instr_profile(self.class)
    }
}

// ─── Instruction field extraction ──────────────────────────────────────────

/// Extract dependency information from an AArch64 MInst by inspecting its
/// fields directly. Post-RA, all Reg fields hold physical registers.
pub fn inst_deps(inst: &MInst) -> InstDeps {
    match inst {
        MInst::Nop | MInst::Removed => InstDeps {
            defs: vec![],
            uses: vec![],
            flags_def: false,
            flags_use: false,
            class: SchedClass::Nop,
            mem: None,
            is_barrier: false,
        },

        MInst::AluRRR {
            op, dst, lhs, rhs, ..
        } => {
            let mut uses = vec![];
            collect_reg_or_zr(lhs, &mut uses);
            collect_reg_or_zr(rhs, &mut uses);
            let class = if *op == AluOp::Mul {
                SchedClass::Mul
            } else {
                SchedClass::Alu
            };
            InstDeps {
                defs: vec![dst.reg.to_physical_reg()]
                    .into_iter()
                    .flatten()
                    .collect(),
                uses,
                flags_def: false,
                flags_use: false,
                class,
                mem: None,
                is_barrier: false,
            }
        }

        MInst::AluRRRR {
            dst,
            lhs,
            rhs,
            carry,
            ..
        } => InstDeps {
            defs: preg(dst.reg),
            uses: [preg(*lhs), preg(*rhs), preg(*carry)]
                .into_iter()
                .flatten()
                .collect(),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Mul,
            mem: None,
            is_barrier: false,
        },

        MInst::AluRRImm12 { dst, src, .. } => InstDeps {
            defs: preg(dst.reg),
            uses: preg(*src),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Alu,
            mem: None,
            is_barrier: false,
        },

        MInst::AluRRImmLogic { dst, src, .. } => InstDeps {
            defs: preg(dst.reg),
            uses: collect_reg_or_zr_vec(src),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Alu,
            mem: None,
            is_barrier: false,
        },

        MInst::AluRRImmShift { dst, src, .. } => InstDeps {
            defs: preg(dst.reg),
            uses: preg(*src),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Alu,
            mem: None,
            is_barrier: false,
        },

        MInst::AluRRRShift { dst, lhs, rhs, .. } => InstDeps {
            defs: preg(dst.reg),
            uses: [collect_reg_or_zr_vec(lhs), collect_reg_or_zr_vec(rhs)].concat(),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Alu,
            mem: None,
            is_barrier: false,
        },

        MInst::AluRRRExtend { dst, lhs, rhs, .. } => InstDeps {
            defs: preg(dst.reg),
            uses: [preg(*lhs), preg(*rhs)].into_iter().flatten().collect(),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Alu,
            mem: None,
            is_barrier: false,
        },

        MInst::SDiv {
            size,
            dst,
            lhs,
            rhs,
        } => InstDeps {
            defs: preg(dst.reg),
            uses: [preg(*lhs), preg(*rhs)].into_iter().flatten().collect(),
            flags_def: false,
            flags_use: false,
            class: match size {
                OperandSize::Size32 => SchedClass::Div32,
                OperandSize::Size64 => SchedClass::Div64,
            },
            mem: None,
            is_barrier: false,
        },

        MInst::SMulL { dst, lhs, rhs } => InstDeps {
            defs: preg(dst.reg),
            uses: [preg(*lhs), preg(*rhs)].into_iter().flatten().collect(),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Mul,
            mem: None,
            is_barrier: false,
        },

        MInst::MAdd {
            dst,
            lhs,
            rhs,
            addend,
            ..
        } => InstDeps {
            defs: preg(dst.reg),
            uses: [preg(*lhs), preg(*rhs), preg(*addend)]
                .into_iter()
                .flatten()
                .collect(),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Mul,
            mem: None,
            is_barrier: false,
        },

        MInst::MSub {
            dst,
            lhs,
            rhs,
            subtrahend,
            ..
        } => InstDeps {
            defs: preg(dst.reg),
            uses: [preg(*lhs), preg(*rhs), preg(*subtrahend)]
                .into_iter()
                .flatten()
                .collect(),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Mul,
            mem: None,
            is_barrier: false,
        },

        MInst::CmpRR { lhs, rhs, .. } => InstDeps {
            defs: vec![],
            uses: [preg(*lhs), collect_reg_or_zr_vec(rhs)].concat(),
            flags_def: true,
            flags_use: false,
            class: SchedClass::Alu,
            mem: None,
            is_barrier: false,
        },

        MInst::CmpImm { lhs, .. } => InstDeps {
            defs: vec![],
            uses: preg(*lhs),
            flags_def: true,
            flags_use: false,
            class: SchedClass::Alu,
            mem: None,
            is_barrier: false,
        },

        // Flag-producing fused forms: `subs`/`ands`/`tst` define NZCV like a
        // compare but also produce (or omit) a register result.
        MInst::SubsRRImm12 { dst, src, .. } => InstDeps {
            defs: preg(dst.reg),
            uses: preg(*src),
            flags_def: true,
            flags_use: false,
            class: SchedClass::Alu,
            mem: None,
            is_barrier: false,
        },

        MInst::AndsRRImmLogic { dst, src, .. } => InstDeps {
            defs: preg(dst.reg),
            uses: collect_reg_or_zr_vec(src),
            flags_def: true,
            flags_use: false,
            class: SchedClass::Alu,
            mem: None,
            is_barrier: false,
        },

        MInst::TstRRImmLogic { src, .. } => InstDeps {
            defs: vec![],
            uses: collect_reg_or_zr_vec(src),
            flags_def: true,
            flags_use: false,
            class: SchedClass::Alu,
            mem: None,
            is_barrier: false,
        },

        // `ccmp` consumes NZCV as its condition and redefines it, so it sits
        // between the preceding comparison and the consuming branch/select.
        MInst::CCmp { lhs, rhs, .. } => {
            let mut uses = preg(*lhs);
            uses.extend(collect_reg_or_zr_vec(rhs));
            InstDeps {
                defs: vec![],
                uses,
                flags_def: true,
                flags_use: true,
                class: SchedClass::Alu,
                mem: None,
                is_barrier: false,
            }
        }

        MInst::Mov { dst, src, .. } | MInst::MovPhys { dst, src, .. } => InstDeps {
            defs: preg(dst.reg),
            uses: preg(*src),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Alu,
            mem: None,
            is_barrier: false,
        },

        MInst::LoadImm { dst, .. } | MInst::MovZ { dst, .. } | MInst::MovN { dst, .. } => {
            InstDeps {
                defs: preg(dst.reg),
                uses: vec![],
                flags_def: false,
                flags_use: false,
                class: SchedClass::Alu,
                mem: None,
                is_barrier: false,
            }
        }

        MInst::Load { ty, dst, addr } => InstDeps {
            defs: preg(dst.reg),
            uses: amode_regs(addr),
            flags_def: false,
            flags_use: false,
            class: if ty.is_float() {
                SchedClass::LoadFp
            } else {
                SchedClass::LoadInt
            },
            mem: Some(unknown_access(MemKind::Load, 0)),
            is_barrier: false,
        },

        MInst::Store { ty, src, addr } => InstDeps {
            defs: vec![],
            uses: [preg(*src), amode_regs(addr)].concat(),
            flags_def: false,
            flags_use: false,
            class: if ty.is_float() {
                SchedClass::StoreFp
            } else {
                SchedClass::StoreInt
            },
            mem: Some(unknown_access(MemKind::Store, 0)),
            is_barrier: false,
        },

        MInst::LoadPair {
            ty,
            dst1,
            dst2,
            addr,
        } => InstDeps {
            defs: [preg(dst1.reg), preg(dst2.reg), pair_amode_defs(addr)].concat(),
            uses: pair_amode_regs(addr),
            flags_def: false,
            flags_use: false,
            class: if ty.is_float() {
                SchedClass::LoadPairFp
            } else {
                SchedClass::LoadPairInt
            },
            mem: Some(unknown_access(MemKind::Load, 0)),
            is_barrier: false,
        },

        MInst::StorePair {
            ty,
            src1,
            src2,
            addr,
        } => InstDeps {
            defs: pair_amode_defs(addr),
            uses: [preg(*src1), preg(*src2), pair_amode_regs(addr)].concat(),
            flags_def: false,
            flags_use: false,
            class: if ty.is_float() {
                SchedClass::StorePairFp
            } else {
                SchedClass::StorePairInt
            },
            mem: Some(unknown_access(MemKind::Store, 0)),
            is_barrier: false,
        },

        MInst::FAlu { dst, lhs, rhs, .. } => InstDeps {
            defs: preg(dst.reg),
            uses: [preg(*lhs), preg(*rhs)].into_iter().flatten().collect(),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Other,
            mem: None,
            is_barrier: false,
        },

        MInst::VecLd1 { dst, base } => InstDeps {
            defs: preg(dst.reg),
            uses: preg(*base),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Other,
            mem: Some(unknown_access(MemKind::Load, 16)),
            is_barrier: false,
        },

        MInst::VecSt1 { src, base } => InstDeps {
            defs: vec![],
            uses: [preg(*src), preg(*base)].into_iter().flatten().collect(),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Other,
            mem: Some(unknown_access(MemKind::Store, 16)),
            is_barrier: false,
        },

        MInst::VecDup { dst, src, .. }
        | MInst::VecCvt { dst, src, .. }
        | MInst::VecAddv { dst, src } => InstDeps {
            defs: preg(dst.reg),
            uses: preg(*src),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Other,
            mem: None,
            is_barrier: false,
        },

        MInst::VecArithRRR { dst, lhs, rhs, .. }
        | MInst::VecBitwise { dst, lhs, rhs, .. }
        | MInst::VecCmp { dst, lhs, rhs, .. }
        | MInst::VecMinMax { dst, lhs, rhs, .. } => InstDeps {
            defs: preg(dst.reg),
            uses: [preg(*lhs), preg(*rhs)].into_iter().flatten().collect(),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Other,
            mem: None,
            is_barrier: false,
        },

        MInst::VecFmla {
            dst, acc, lhs, rhs, ..
        }
        | MInst::VecBsl {
            dst,
            mask: acc,
            lhs,
            rhs,
        } => InstDeps {
            defs: preg(dst.reg),
            uses: [preg(*acc), preg(*lhs), preg(*rhs)]
                .into_iter()
                .flatten()
                .collect(),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Other,
            mem: None,
            is_barrier: false,
        },

        MInst::VecMovImm { dst, .. } => InstDeps {
            defs: preg(dst.reg),
            uses: vec![],
            flags_def: false,
            flags_use: false,
            class: SchedClass::Other,
            mem: None,
            is_barrier: false,
        },

        MInst::VecExtractLane { dst, src, .. } => InstDeps {
            defs: preg(dst.reg),
            uses: preg(*src),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Other,
            mem: None,
            is_barrier: false,
        },

        MInst::VecInsertLane {
            dst, vector, src, ..
        } => InstDeps {
            defs: preg(dst.reg),
            uses: [preg(*vector), preg(*src)].into_iter().flatten().collect(),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Other,
            mem: None,
            is_barrier: false,
        },

        MInst::FMov { dst, src } | MInst::VecMov { dst, src } => InstDeps {
            defs: preg(dst.reg),
            uses: preg(*src),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Other,
            mem: None,
            is_barrier: false,
        },

        MInst::FMovFromZero { dst } => InstDeps {
            defs: preg(dst.reg),
            uses: vec![],
            flags_def: false,
            flags_use: false,
            class: SchedClass::Other,
            mem: None,
            is_barrier: false,
        },

        MInst::Sxtw { dst, src, .. } => InstDeps {
            defs: preg(dst.reg),
            uses: preg(*src),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Alu,
            mem: None,
            is_barrier: false,
        },

        MInst::Scvtf { dst, src } | MInst::Fcvtzs { dst, src } => InstDeps {
            defs: preg(dst.reg),
            uses: preg(*src),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Other,
            mem: None,
            is_barrier: false,
        },

        MInst::FCmp { lhs, rhs } => InstDeps {
            defs: vec![],
            uses: [preg(*lhs), preg(*rhs)].concat(),
            flags_def: true,
            flags_use: false,
            class: SchedClass::Other,
            mem: None,
            is_barrier: false,
        },

        MInst::CSet { dst, .. } => InstDeps {
            defs: preg(dst.reg),
            uses: vec![],
            flags_def: false,
            flags_use: true,
            class: SchedClass::Alu,
            mem: None,
            is_barrier: false,
        },

        // Control flow remains fixed in place. Register and flag uses are
        // still represented so the graph documents the true dependency.
        MInst::BCond { .. } | MInst::CondBr { .. } => InstDeps {
            defs: vec![],
            uses: vec![],
            flags_def: false,
            flags_use: true,
            class: SchedClass::Branch,
            mem: None,
            is_barrier: true,
        },

        MInst::Cbz { reg, .. }
        | MInst::Cbnz { reg, .. }
        | MInst::Tbz { reg, .. }
        | MInst::Tbnz { reg, .. } => InstDeps {
            defs: vec![],
            uses: preg(*reg),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Branch,
            mem: None,
            is_barrier: true,
        },

        MInst::Jump { .. } => InstDeps {
            defs: vec![],
            uses: vec![],
            flags_def: false,
            flags_use: false,
            class: SchedClass::Branch,
            mem: None,
            is_barrier: true,
        },

        // Calls and returns are barriers — they clobber caller-save registers
        // and may read/write memory.
        MInst::Call { .. } | MInst::TailCall { .. } | MInst::Ret => InstDeps {
            defs: vec![],
            uses: vec![],
            flags_def: false,
            flags_use: false,
            class: SchedClass::Barrier,
            mem: None,
            is_barrier: true,
        },

        MInst::StackAddr { dst, .. } => InstDeps {
            defs: preg(dst.reg),
            uses: vec![],
            flags_def: false,
            flags_use: false,
            class: SchedClass::Alu,
            mem: None,
            is_barrier: false,
        },

        // LoadAddr emits an adjacent ADRP+ADD pair. Keep it atomic while
        // exposing its destination definition and global provenance.
        MInst::LoadAddr { dst, .. } => InstDeps {
            defs: preg(dst.reg),
            uses: vec![],
            flags_def: false,
            flags_use: false,
            class: SchedClass::Other,
            mem: None,
            is_barrier: true,
        },

        MInst::RetVal { pair } => InstDeps {
            defs: vec![],
            uses: preg(pair.vreg),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Nop,
            mem: None,
            is_barrier: false,
        },

        // The entry Args pseudo defines every register parameter and emits no
        // machine code. It consumes zero cycles and zero resources; its defs
        // still enforce the register RAW/WAW/WAR ordering that keeps argument
        // values alive until their first real use.
        MInst::Args { args } => InstDeps {
            defs: args.iter().flat_map(|pair| preg(pair.vreg.reg)).collect(),
            uses: vec![],
            flags_def: false,
            flags_use: false,
            class: SchedClass::Nop,
            mem: None,
            is_barrier: false,
        },

        MInst::MovK { dst, src, .. } => InstDeps {
            defs: preg(dst.reg),
            uses: preg(*src),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Alu,
            mem: None,
            is_barrier: false,
        },

        // Catch-all: anything we haven't modeled yet is conservatively treated
        // as a barrier to preserve correctness.
        _ => InstDeps {
            defs: vec![],
            uses: vec![],
            flags_def: false,
            flags_use: false,
            class: SchedClass::Other,
            mem: None,
            is_barrier: true,
        },
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────────

// ─── Helpers ───────────────────────────────────────────────────────────────

pub(super) fn preg(r: Reg) -> Vec<PReg> {
    r.to_physical_reg().map(|p| vec![p]).unwrap_or_default()
}

pub(super) fn collect_reg_or_zr(rz: &RegOrZr, out: &mut Vec<PReg>) {
    if let RegOrZr::Reg(r) = rz {
        out.extend(preg(*r));
    }
}

pub(super) fn collect_reg_or_zr_vec(rz: &RegOrZr) -> Vec<PReg> {
    let mut v = vec![];
    collect_reg_or_zr(rz, &mut v);
    v
}

pub(super) fn amode_regs(addr: &crate::instructions::AMode) -> Vec<PReg> {
    use crate::instructions::AMode;
    match addr {
        AMode::Reg { base } => preg(*base),
        AMode::UnsignedOffset { base, .. } | AMode::SignedOffset { base, .. } => preg(*base),
        AMode::RegOffset { base, index } => {
            [preg(*base), preg(*index)].into_iter().flatten().collect()
        }
        AMode::ScaledRegOffset { base, index, .. } => {
            [preg(*base), preg(*index)].into_iter().flatten().collect()
        }
        AMode::ExtendedRegOffset { base, index, .. } => {
            [preg(*base), preg(*index)].into_iter().flatten().collect()
        }
        _ => vec![],
    }
}

pub(super) fn pair_amode_regs(addr: &crate::instructions::PairAMode) -> Vec<PReg> {
    use crate::instructions::PairAMode;
    match addr {
        PairAMode::SignedOffset { base, .. }
        | PairAMode::PreIndex { base, .. }
        | PairAMode::PostIndex { base, .. } => preg(*base),
    }
}

pub(super) fn pair_amode_defs(addr: &crate::instructions::PairAMode) -> Vec<PReg> {
    use crate::instructions::PairAMode;
    match addr {
        PairAMode::SignedOffset { .. } => vec![],
        PairAMode::PreIndex { base, .. } | PairAMode::PostIndex { base, .. } => preg(*base),
    }
}
