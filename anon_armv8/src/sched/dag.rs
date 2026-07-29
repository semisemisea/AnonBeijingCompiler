//! Per-block dependency DAG for post-RA instruction scheduling.
//!
//! After `finalize_for_emission`, all register fields hold physical registers.
//! The DAG builder inspects instruction fields directly (not via `get_operands`,
//! which skips physical-register operands post-RA) to build RAW / WAW / WAR
//! edges plus conservative memory-dependency edges.

use std::collections::HashMap;

use taki_mir::reg_alloc::reg::PReg;
use taki_mir::register::Reg;

use crate::instructions::MInst;
use crate::regs::RegOrZr;
use crate::sched::aarch53::{InstrProfile, SchedClass, instr_profile};

/// Memory access type for dependency tracking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemKind {
    Load,
    Store,
}

/// Dependency metadata extracted from a single instruction.
pub struct InstDeps {
    pub defs: Vec<PReg>,
    pub uses: Vec<PReg>,
    pub flags_def: bool,
    pub flags_use: bool,
    pub class: SchedClass,
    pub mem: Option<MemKind>,
    pub is_barrier: bool,
}

impl InstDeps {
    fn profile(&self) -> InstrProfile {
        instr_profile(self.class)
    }
}

/// Dependency DAG for one basic block.
pub struct DepGraph {
    /// Number of nodes (= instructions in the block).
    pub n: usize,
    /// `succs[i]` = list of (successor_index, edge_latency).
    pub succs: Vec<Vec<(usize, u32)>>,
    /// `preds[i]` = list of predecessor indices.
    pub preds: Vec<Vec<usize>>,
    /// `deps[i]` = extracted dependency info for instruction i.
    pub deps: Vec<InstDeps>,
    /// Critical-path length from each node to a leaf.
    pub crit: Vec<u32>,
}

impl DepGraph {
    /// Build the dependency DAG for a slice of instructions (one block).
    pub fn build(insts: &[MInst]) -> Self {
        let n = insts.len();
        let deps: Vec<InstDeps> = insts.iter().map(inst_deps).collect();

        let mut succs: Vec<Vec<(usize, u32)>> = vec![Vec::new(); n];
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];

        // Per-register last writer and all readers since that write.
        let mut last_def: HashMap<PReg, usize> = HashMap::new();
        let mut pending_uses: HashMap<PReg, Vec<usize>> = HashMap::new();

        // NZCV is implicit architectural state and needs the same dependency
        // treatment as a physical register.
        let mut last_flags_def: Option<usize> = None;
        let mut pending_flags_uses: Vec<usize> = Vec::new();

        // Keep every load since the last store: a following store may alias
        // any of them, not only the most recent load.
        let mut pending_loads: Vec<usize> = Vec::new();
        let mut last_store: Option<usize> = None;
        // A barrier (call/return) forces all subsequent instructions to depend
        // on it. Track the last barrier; everything after it must wait.
        let mut last_barrier: Option<usize> = None;

        for i in 0..n {
            let d = &deps[i];
            // If there was a prior barrier, this instruction depends on it.
            if let Some(b) = last_barrier {
                add_edge(&mut succs, &mut preds, b, i, deps[b].profile().latency);
            }

            // Register dependencies.
            for &u in &d.uses {
                // RAW: producer must complete before consumer reads.
                if let Some(&prev) = last_def.get(&u) {
                    add_edge(
                        &mut succs,
                        &mut preds,
                        prev,
                        i,
                        deps[prev].profile().latency,
                    );
                }
            }
            for &def in &d.defs {
                // WAW: must wait for prior write.
                if let Some(&prev) = last_def.get(&def) {
                    add_edge(&mut succs, &mut preds, prev, i, 0);
                }
                // WAR: must wait for prior read to finish.
                for &prev in pending_uses.get(&def).into_iter().flatten() {
                    add_edge(&mut succs, &mut preds, prev, i, 0);
                }
            }

            if d.flags_use {
                if let Some(prev) = last_flags_def {
                    add_edge(
                        &mut succs,
                        &mut preds,
                        prev,
                        i,
                        deps[prev].profile().latency,
                    );
                }
            }
            if d.flags_def {
                if let Some(prev) = last_flags_def {
                    add_edge(&mut succs, &mut preds, prev, i, 0);
                }
                for &prev in &pending_flags_uses {
                    add_edge(&mut succs, &mut preds, prev, i, 0);
                }
            }

            // Memory dependencies (conservative: no alias analysis).
            match d.mem {
                Some(MemKind::Load) => {
                    // Load-after-store: must wait (store might write what we read).
                    if let Some(s) = last_store {
                        add_edge(&mut succs, &mut preds, s, i, deps[s].profile().latency);
                    }
                    // Load-after-load is allowed (no ordering constraint on A53
                    // for loads from different addresses, but we keep it
                    // conservative only when a barrier is between them, which
                    // is already handled by the barrier logic above).
                    pending_loads.push(i);
                }
                Some(MemKind::Store) => {
                    // Store-after-load: keep stores in order with loads (may alias).
                    for &l in &pending_loads {
                        add_edge(&mut succs, &mut preds, l, i, 0);
                    }
                    // Store-after-store: preserve write ordering.
                    if let Some(s) = last_store {
                        add_edge(&mut succs, &mut preds, s, i, 0);
                    }
                    last_store = Some(i);
                    pending_loads.clear();
                }
                None => {}
            }

            // Record defs and uses.
            for &def in &d.defs {
                last_def.insert(def, i);
                pending_uses.remove(&def);
            }
            for &u in &d.uses {
                pending_uses.entry(u).or_default().push(i);
            }
            if d.flags_def {
                last_flags_def = Some(i);
                pending_flags_uses.clear();
            }
            if d.flags_use {
                pending_flags_uses.push(i);
            }

            if d.is_barrier {
                // A barrier cannot move before any instruction in its prefix.
                // Together with last_barrier above this pins both sides.
                for prev in 0..i {
                    add_edge(
                        &mut succs,
                        &mut preds,
                        prev,
                        i,
                        deps[prev].profile().latency,
                    );
                }
                last_barrier = Some(i);
                // A barrier consumes all pending loads/stores.
                pending_loads.clear();
                last_store = None;
            }
        }

        // Compute critical path (longest weighted path from each node to a leaf).
        let mut crit = vec![0u32; n];
        for i in (0..n).rev() {
            let node_latency = deps[i].profile().latency;
            let max_succ = succs[i].iter().map(|&(s, _)| crit[s]).max().unwrap_or(0);
            crit[i] = node_latency + max_succ;
        }

        DepGraph {
            n,
            succs,
            preds,
            deps,
            crit,
        }
    }
}

fn add_edge(
    succs: &mut [Vec<(usize, u32)>],
    preds: &mut [Vec<usize>],
    from: usize,
    to: usize,
    latency: u32,
) {
    if from == to {
        return;
    }
    // Avoid duplicate edges (only keep the highest-weight edge between a pair).
    if let Some(existing) = succs[from].iter_mut().find(|(s, _)| *s == to) {
        if latency > existing.1 {
            existing.1 = latency;
        }
        return;
    }
    succs[from].push((to, latency));
    preds[to].push(from);
}

// ─── Instruction field extraction ──────────────────────────────────────────

/// Extract dependency information from an AArch64 MInst by inspecting its
/// fields directly. Post-RA, all Reg fields hold physical registers.
pub fn inst_deps(inst: &MInst) -> InstDeps {
    use crate::instructions::AluOp;
    match inst {
        MInst::Nop => InstDeps {
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

        MInst::SDiv { dst, lhs, rhs, .. } => InstDeps {
            defs: preg(dst.reg),
            uses: [preg(*lhs), preg(*rhs)].into_iter().flatten().collect(),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Div,
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

        MInst::Load { dst, addr, .. } => InstDeps {
            defs: preg(dst.reg),
            uses: amode_regs(addr),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Load,
            mem: Some(MemKind::Load),
            is_barrier: false,
        },

        MInst::Store { src, addr, .. } => InstDeps {
            defs: vec![],
            uses: [preg(*src), amode_regs(addr)].concat(),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Store,
            mem: Some(MemKind::Store),
            is_barrier: false,
        },

        MInst::LoadPair {
            dst1, dst2, addr, ..
        } => InstDeps {
            defs: [preg(dst1.reg), preg(dst2.reg), pair_amode_defs(addr)].concat(),
            uses: pair_amode_regs(addr),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Load,
            mem: Some(MemKind::Load),
            is_barrier: false,
        },

        MInst::StorePair {
            src1, src2, addr, ..
        } => InstDeps {
            defs: pair_amode_defs(addr),
            uses: [preg(*src1), preg(*src2), pair_amode_regs(addr)].concat(),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Store,
            mem: Some(MemKind::Store),
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

        MInst::FMov { dst, src } => InstDeps {
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

        MInst::RetVal { pair } => InstDeps {
            defs: vec![],
            uses: preg(pair.vreg),
            flags_def: false,
            flags_use: false,
            class: SchedClass::Alu,
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

fn preg(r: Reg) -> Vec<PReg> {
    r.to_physical_reg().map(|p| vec![p]).unwrap_or_default()
}

fn collect_reg_or_zr(rz: &RegOrZr, out: &mut Vec<PReg>) {
    if let RegOrZr::Reg(r) = rz {
        out.extend(preg(*r));
    }
}

fn collect_reg_or_zr_vec(rz: &RegOrZr) -> Vec<PReg> {
    let mut v = vec![];
    collect_reg_or_zr(rz, &mut v);
    v
}

fn amode_regs(addr: &crate::instructions::AMode) -> Vec<PReg> {
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

fn pair_amode_regs(addr: &crate::instructions::PairAMode) -> Vec<PReg> {
    use crate::instructions::PairAMode;
    match addr {
        PairAMode::SignedOffset { base, .. }
        | PairAMode::PreIndex { base, .. }
        | PairAMode::PostIndex { base, .. } => preg(*base),
    }
}

fn pair_amode_defs(addr: &crate::instructions::PairAMode) -> Vec<PReg> {
    use crate::instructions::PairAMode;
    match addr {
        PairAMode::SignedOffset { .. } => vec![],
        PairAMode::PreIndex { base, .. } | PairAMode::PostIndex { base, .. } => preg(*base),
    }
}

#[cfg(test)]
mod tests {
    use taki_mir::register::Writable;

    use super::*;
    use crate::{
        instructions::{Cond, Imm12},
        regs::{OperandSize, RegOrZr, int_reg},
    };

    fn writable(index: u8) -> Writable<Reg> {
        Writable::from_reg(int_reg(index))
    }

    fn has_edge(graph: &DepGraph, from: usize, to: usize) -> bool {
        graph.succs[from].iter().any(|&(succ, _)| succ == to)
    }

    #[test]
    fn keeps_all_readers_before_a_later_write() {
        let insts = vec![
            MInst::Mov {
                size: OperandSize::Size64,
                dst: writable(1),
                src: int_reg(0),
            },
            MInst::Mov {
                size: OperandSize::Size64,
                dst: writable(2),
                src: int_reg(0),
            },
            MInst::Mov {
                size: OperandSize::Size64,
                dst: writable(0),
                src: int_reg(3),
            },
        ];
        let graph = DepGraph::build(&insts);

        assert!(has_edge(&graph, 0, 2));
        assert!(has_edge(&graph, 1, 2));
    }

    #[test]
    fn keeps_all_loads_before_a_later_store() {
        let addr = |base| crate::instructions::AMode::Reg { base };
        let insts = vec![
            MInst::Load {
                ty: crate::instructions::MemoryType::I64,
                dst: writable(1),
                addr: addr(int_reg(4)),
            },
            MInst::Load {
                ty: crate::instructions::MemoryType::I64,
                dst: writable(2),
                addr: addr(int_reg(5)),
            },
            MInst::Store {
                ty: crate::instructions::MemoryType::I64,
                src: int_reg(3),
                addr: addr(int_reg(6)),
            },
        ];
        let graph = DepGraph::build(&insts);

        assert!(has_edge(&graph, 0, 2));
        assert!(has_edge(&graph, 1, 2));
    }

    #[test]
    fn load_destination_is_only_a_definition() {
        let deps = inst_deps(&MInst::Load {
            ty: crate::instructions::MemoryType::I64,
            dst: writable(1),
            addr: crate::instructions::AMode::Reg { base: int_reg(2) },
        });

        assert_eq!(deps.defs, vec![crate::regs::int_preg(1)]);
        assert_eq!(deps.uses, vec![crate::regs::int_preg(2)]);
    }

    #[test]
    fn pair_writeback_defines_its_base_register() {
        let deps = inst_deps(&MInst::StorePair {
            ty: crate::instructions::MemoryType::I64,
            src1: int_reg(1),
            src2: int_reg(2),
            addr: crate::instructions::PairAMode::PostIndex {
                base: int_reg(3),
                offset: crate::instructions::SImm7Scaled::new(16, 8).unwrap(),
            },
        });

        assert_eq!(deps.defs, vec![crate::regs::int_preg(3)]);
        assert!(deps.uses.contains(&crate::regs::int_preg(3)));
    }

    #[test]
    fn models_nzcv_producers_and_consumers() {
        let insts = vec![
            MInst::CmpImm {
                size: OperandSize::Size64,
                lhs: int_reg(0),
                imm: Imm12::new(0, false).unwrap(),
            },
            MInst::CSet {
                cond: Cond::Eq,
                dst: writable(1),
            },
            MInst::CmpRR {
                size: OperandSize::Size64,
                lhs: int_reg(2),
                rhs: RegOrZr::Reg(int_reg(3)),
            },
        ];
        let graph = DepGraph::build(&insts);

        assert!(has_edge(&graph, 0, 1));
        assert!(has_edge(&graph, 1, 2));
    }

    #[test]
    fn barrier_is_ordered_after_the_entire_prefix() {
        let insts = vec![
            MInst::Mov {
                size: OperandSize::Size64,
                dst: writable(1),
                src: int_reg(0),
            },
            MInst::Mov {
                size: OperandSize::Size64,
                dst: writable(2),
                src: int_reg(3),
            },
            MInst::Jump {
                label: crate::labels::Label::from_block(taki_mir::block_order::MirBlockIndex::new(
                    0,
                )),
            },
        ];
        let graph = DepGraph::build(&insts);

        assert!(has_edge(&graph, 0, 2));
        assert!(has_edge(&graph, 1, 2));
    }
}
