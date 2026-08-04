//! Per-block dependency DAG for post-RA instruction scheduling.
//!
//! After `finalize_for_emission`, all register fields hold physical registers.
//! The DAG builder inspects instruction fields directly (not via `get_operands`,
//! which skips physical-register operands post-RA) to build RAW / WAW / WAR
//! edges plus conservative memory-dependency edges.

use std::collections::HashMap;

use taki_mir::reg_alloc::reg::PReg;
use taki_mir::register::Reg;

use crate::instructions::{AMode, AluOp, MInst, PairAMode};
use crate::labels::Label;
use crate::regs::{FP, OperandSize, RegOrZr, int_preg, stack_preg};
use crate::sched::aarch53::{InstrProfile, SchedClass, instr_profile};

/// Memory access type for dependency tracking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemKind {
    Load,
    Store,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemRoot {
    StackSp,
    StackFp,
    Global(taki_mir::prelude::HirInst),
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemAccess {
    pub kind: MemKind,
    pub root: MemRoot,
    pub offset: Option<i64>,
    pub size: u8,
}

#[derive(Clone, Copy, Debug)]
struct Provenance {
    root: MemRoot,
    offset: i64,
}

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
    fn profile(&self) -> InstrProfile {
        instr_profile(self.class)
    }
}

/// Why a dependency edge exists. An edge between the same pair of nodes can
/// carry multiple reasons; `DepEdge::kinds` records them all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeKind {
    RegisterRaw,
    RegisterWar,
    RegisterWaw,
    NzcvRaw,
    NzcvWar,
    NzcvWaw,
    Memory,
    Barrier,
}

impl EdgeKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::RegisterRaw => "register-raw",
            Self::RegisterWar => "register-war",
            Self::RegisterWaw => "register-waw",
            Self::NzcvRaw => "nzcv-raw",
            Self::NzcvWar => "nzcv-war",
            Self::NzcvWaw => "nzcv-waw",
            Self::Memory => "memory",
            Self::Barrier => "barrier",
        }
    }

    const ALL: [Self; 8] = [
        Self::RegisterRaw,
        Self::RegisterWar,
        Self::RegisterWaw,
        Self::NzcvRaw,
        Self::NzcvWar,
        Self::NzcvWaw,
        Self::Memory,
        Self::Barrier,
    ];
}

/// A dependency edge with its earliest-issue latency and reason bitmask.
#[derive(Clone, Copy, Debug)]
pub struct DepEdge {
    pub node: usize,
    pub latency: u32,
    pub kinds: u16,
}

impl DepEdge {
    fn new(node: usize, latency: u32, kind: EdgeKind) -> Self {
        Self {
            node,
            latency,
            kinds: 1 << (kind as u16),
        }
    }

    pub fn has_kind(&self, kind: EdgeKind) -> bool {
        self.kinds & (1 << (kind as u16)) != 0
    }
}

/// Dependency DAG for one basic block.
pub struct DepGraph {
    /// Number of nodes (= instructions in the block).
    pub n: usize,
    /// `succs[i]` = list of outgoing edges.
    pub succs: Vec<Vec<DepEdge>>,
    /// `preds[i]` = list of predecessor indices.
    pub preds: Vec<Vec<usize>>,
    /// `deps[i]` = extracted dependency info for instruction i.
    pub deps: Vec<InstDeps>,
    /// Critical-path length from each node to a leaf.
    pub crit: Vec<u32>,
    /// Dependency statistics collected during build.
    pub stats: DagBuildStats,
}

/// Statistics collected while building the DAG.
#[derive(Clone, Debug, Default)]
pub struct DagBuildStats {
    pub nodes: u64,
    pub edges: u64,
    pub edge_kind_counts: [u64; 8],
    pub memory_accesses: u64,
    pub known_root_accesses: u64,
    pub unknown_root_accesses: u64,
    pub memory_comparisons: u64,
    pub disjoint_comparisons: u64,
    pub may_alias_comparisons: u64,
    pub max_memory_history: u64,
}

impl DagBuildStats {
    pub fn record_to_taki_stats(&self, stats: &mut taki_mir::stats::DagStats) {
        stats.nodes += self.nodes;
        stats.edges += self.edges;
        for (index, count) in self.edge_kind_counts.iter().enumerate() {
            if *count > 0 {
                *stats
                    .edges_by_kind
                    .entry(EdgeKind::ALL[index].name().to_string())
                    .or_default() += count;
            }
        }
        stats.max_block_nodes = stats.max_block_nodes.max(self.nodes);
        let memory = &mut stats.memory;
        memory.accesses += self.memory_accesses;
        memory.known_root_accesses += self.known_root_accesses;
        memory.unknown_root_accesses += self.unknown_root_accesses;
        memory.comparisons += self.memory_comparisons;
        memory.disjoint_comparisons += self.disjoint_comparisons;
        memory.may_alias_comparisons += self.may_alias_comparisons;
        memory.max_history_len = memory.max_history_len.max(self.max_memory_history);
    }
}

impl DepGraph {
    /// Build the dependency DAG for a slice of instructions (one block).
    pub fn build(insts: &[MInst]) -> Self {
        let n = insts.len();
        let mut deps: Vec<InstDeps> = insts.iter().map(inst_deps).collect();
        annotate_memory_accesses(insts, &mut deps);

        let mut succs: Vec<Vec<DepEdge>> = vec![Vec::new(); n];
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut stats = DagBuildStats {
            nodes: n as u64,
            ..DagBuildStats::default()
        };

        // Per-register last writer and all readers since that write.
        let mut last_def: HashMap<PReg, usize> = HashMap::new();
        let mut pending_uses: HashMap<PReg, Vec<usize>> = HashMap::new();

        // NZCV is implicit architectural state and needs the same dependency
        // treatment as a physical register.
        let mut last_flags_def: Option<usize> = None;
        let mut pending_flags_uses: Vec<usize> = Vec::new();

        // Compare against all memory operations since the last barrier. Once
        // disjoint operations can skip edges, the latest store alone is not
        // enough: an older store may still alias a new access.
        let mut prior_memory: Vec<usize> = Vec::new();
        // A barrier (call/return) forces all subsequent instructions to depend
        // on it. Track the last barrier; everything after it must wait.
        let mut last_barrier: Option<usize> = None;

        for i in 0..n {
            let d = &deps[i];
            // If there was a prior barrier, this instruction depends on it.
            if let Some(b) = last_barrier {
                add_edge(
                    &mut succs,
                    &mut preds,
                    &mut stats,
                    b,
                    i,
                    deps[b].profile().latency,
                    EdgeKind::Barrier,
                );
            }

            // Register dependencies.
            for &u in &d.uses {
                // RAW: producer must complete before consumer reads.
                if let Some(&prev) = last_def.get(&u) {
                    add_edge(
                        &mut succs,
                        &mut preds,
                        &mut stats,
                        prev,
                        i,
                        deps[prev].profile().latency,
                        EdgeKind::RegisterRaw,
                    );
                }
            }
            for &def in &d.defs {
                // WAW: must wait for prior write.
                if let Some(&prev) = last_def.get(&def) {
                    add_edge(
                        &mut succs,
                        &mut preds,
                        &mut stats,
                        prev,
                        i,
                        0,
                        EdgeKind::RegisterWaw,
                    );
                }
                // WAR: must wait for prior read to finish.
                for &prev in pending_uses.get(&def).into_iter().flatten() {
                    add_edge(
                        &mut succs,
                        &mut preds,
                        &mut stats,
                        prev,
                        i,
                        0,
                        EdgeKind::RegisterWar,
                    );
                }
            }

            if d.flags_use {
                if let Some(prev) = last_flags_def {
                    add_edge(
                        &mut succs,
                        &mut preds,
                        &mut stats,
                        prev,
                        i,
                        deps[prev].profile().latency,
                        EdgeKind::NzcvRaw,
                    );
                }
            }
            if d.flags_def {
                if let Some(prev) = last_flags_def {
                    add_edge(
                        &mut succs,
                        &mut preds,
                        &mut stats,
                        prev,
                        i,
                        0,
                        EdgeKind::NzcvWaw,
                    );
                }
                for &prev in &pending_flags_uses {
                    add_edge(
                        &mut succs,
                        &mut preds,
                        &mut stats,
                        prev,
                        i,
                        0,
                        EdgeKind::NzcvWar,
                    );
                }
            }

            if let Some(access) = d.mem {
                stats.memory_accesses += 1;
                match access.root {
                    MemRoot::Unknown => stats.unknown_root_accesses += 1,
                    _ => stats.known_root_accesses += 1,
                }
                stats.max_memory_history = stats.max_memory_history.max(prior_memory.len() as u64);
                for &prev in &prior_memory {
                    let prev_access = deps[prev].mem.expect("memory history contains access");
                    if access.kind == MemKind::Store || prev_access.kind == MemKind::Store {
                        stats.memory_comparisons += 1;
                        if may_alias(prev_access, access) {
                            stats.may_alias_comparisons += 1;
                            let latency = if prev_access.kind == MemKind::Store
                                && access.kind == MemKind::Load
                            {
                                deps[prev].profile().latency
                            } else {
                                0
                            };
                            add_edge(
                                &mut succs,
                                &mut preds,
                                &mut stats,
                                prev,
                                i,
                                latency,
                                EdgeKind::Memory,
                            );
                        } else {
                            stats.disjoint_comparisons += 1;
                        }
                    }
                }
                prior_memory.push(i);
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
                        &mut stats,
                        prev,
                        i,
                        deps[prev].profile().latency,
                        EdgeKind::Barrier,
                    );
                }
                last_barrier = Some(i);
                prior_memory.clear();
            }
        }

        // Compute critical path using edge latency: the longest path from each
        // node to a leaf, where each edge contributes its earliest-issue
        // distance. This is the bottom-level heuristic for list scheduling.
        let mut crit = vec![0u32; n];
        for i in (0..n).rev() {
            let node_latency = deps[i].profile().latency;
            let max_succ = succs[i]
                .iter()
                .map(|edge| edge.latency + crit[edge.node])
                .max()
                .unwrap_or(0);
            crit[i] = node_latency.max(max_succ);
        }

        DepGraph {
            n,
            succs,
            preds,
            deps,
            crit,
            stats,
        }
    }
}

fn add_edge(
    succs: &mut [Vec<DepEdge>],
    preds: &mut [Vec<usize>],
    stats: &mut DagBuildStats,
    from: usize,
    to: usize,
    latency: u32,
    kind: EdgeKind,
) {
    if from == to {
        return;
    }
    // Avoid duplicate edges (only keep the highest-weight edge between a pair,
    // but accumulate all dependency reasons).
    if let Some(existing) = succs[from].iter_mut().find(|edge| edge.node == to) {
        if latency > existing.latency {
            existing.latency = latency;
        }
        existing.kinds |= 1 << (kind as u16);
        stats.edge_kind_counts[kind as usize] += 1;
        return;
    }
    succs[from].push(DepEdge::new(to, latency, kind));
    preds[to].push(from);
    stats.edges += 1;
    stats.edge_kind_counts[kind as usize] += 1;
}

fn unknown_access(kind: MemKind, size: u8) -> MemAccess {
    MemAccess {
        kind,
        root: MemRoot::Unknown,
        offset: None,
        size,
    }
}

fn annotate_memory_accesses(insts: &[MInst], deps: &mut [InstDeps]) {
    let mut provenance = base_provenance();

    for (inst, deps) in insts.iter().zip(deps) {
        deps.mem = memory_access(inst, &provenance);

        let propagated = propagated_provenance(inst, &provenance);
        for def in &deps.defs {
            provenance.remove(def);
        }
        if deps.is_barrier {
            provenance = base_provenance();
        }
        if let Some((dst, value)) = propagated {
            provenance.insert(dst, value);
        }
    }
}

fn base_provenance() -> HashMap<PReg, Provenance> {
    HashMap::from([
        (
            stack_preg(),
            Provenance {
                root: MemRoot::StackSp,
                offset: 0,
            },
        ),
        (
            int_preg(FP),
            Provenance {
                root: MemRoot::StackFp,
                offset: 0,
            },
        ),
    ])
}

fn propagated_provenance(
    inst: &MInst,
    provenance: &HashMap<PReg, Provenance>,
) -> Option<(PReg, Provenance)> {
    match inst {
        MInst::Mov { size, dst, src } | MInst::MovPhys { size, dst, src }
            if *size == OperandSize::Size64 =>
        {
            Some((preg_one(dst.reg)?, reg_provenance(*src, provenance)?))
        }
        MInst::AluRRImm12 {
            op,
            size,
            dst,
            src,
            imm,
        } if *size == OperandSize::Size64 && matches!(op, AluOp::Add | AluOp::Sub) => {
            let mut value = reg_provenance(*src, provenance)?;
            let immediate = i64::from(imm.value()) << if imm.shift12() { 12 } else { 0 };
            value.offset = match op {
                AluOp::Add => value.offset.checked_add(immediate)?,
                AluOp::Sub => value.offset.checked_sub(immediate)?,
                _ => unreachable!(),
            };
            Some((preg_one(dst.reg)?, value))
        }
        MInst::LoadAddr {
            dst,
            label: Label::GlobalValue(global),
        } => Some((
            preg_one(dst.reg)?,
            Provenance {
                root: MemRoot::Global(*global),
                offset: 0,
            },
        )),
        MInst::StackAddr { dst, addr } => {
            let (root, offset) = pseudo_stack_address(addr)?;
            Some((preg_one(dst.reg)?, Provenance { root, offset }))
        }
        _ => None,
    }
}

fn memory_access(inst: &MInst, provenance: &HashMap<PReg, Provenance>) -> Option<MemAccess> {
    let (kind, ty, address, pair) = match inst {
        MInst::Load { ty, addr, .. } => (MemKind::Load, *ty, Some(addr), None),
        MInst::Store { ty, addr, .. } => (MemKind::Store, *ty, Some(addr), None),
        MInst::LoadPair { ty, addr, .. } => (MemKind::Load, *ty, None, Some(addr)),
        MInst::StorePair { ty, addr, .. } => (MemKind::Store, *ty, None, Some(addr)),
        _ => return None,
    };

    let size = ty.byte_size() * if pair.is_some() { 2 } else { 1 };
    let location = address
        .and_then(|addr| amode_location(addr, provenance))
        .or_else(|| pair.and_then(|addr| pair_amode_location(addr, provenance)));
    Some(match location {
        Some(value) => MemAccess {
            kind,
            root: value.root,
            offset: Some(value.offset),
            size,
        },
        None => unknown_access(kind, size),
    })
}

fn amode_location(addr: &AMode, provenance: &HashMap<PReg, Provenance>) -> Option<Provenance> {
    let (mut value, displacement) = match addr {
        AMode::Reg { base } => (reg_provenance(*base, provenance)?, 0),
        AMode::UnsignedOffset { base, offset } => (
            reg_provenance(*base, provenance)?,
            i64::try_from(offset.byte_offset()).ok()?,
        ),
        AMode::SignedOffset { base, offset } => (
            reg_provenance(*base, provenance)?,
            i64::from(offset.value()),
        ),
        AMode::FrameSlot(offset) | AMode::SpOffset(offset) | AMode::OutgoingArg(offset) => (
            Provenance {
                root: MemRoot::StackSp,
                offset: 0,
            },
            *offset,
        ),
        AMode::IncomingArg(offset) => (
            Provenance {
                root: MemRoot::StackFp,
                offset: 0,
            },
            *offset,
        ),
        AMode::RegOffset { .. }
        | AMode::ScaledRegOffset { .. }
        | AMode::ExtendedRegOffset { .. }
        | AMode::PostIndex { .. } => return None,
    };
    value.offset = value.offset.checked_add(displacement)?;
    Some(value)
}

fn pair_amode_location(
    addr: &PairAMode,
    provenance: &HashMap<PReg, Provenance>,
) -> Option<Provenance> {
    let PairAMode::SignedOffset { base, offset } = addr else {
        return None;
    };
    let mut value = reg_provenance(*base, provenance)?;
    value.offset = value.offset.checked_add(offset.byte_offset())?;
    Some(value)
}

fn pseudo_stack_address(addr: &AMode) -> Option<(MemRoot, i64)> {
    match addr {
        AMode::FrameSlot(offset) | AMode::SpOffset(offset) | AMode::OutgoingArg(offset) => {
            Some((MemRoot::StackSp, *offset))
        }
        AMode::IncomingArg(offset) => Some((MemRoot::StackFp, *offset)),
        _ => None,
    }
}

fn preg_one(reg: Reg) -> Option<PReg> {
    reg.to_physical_reg()
}

fn reg_provenance(reg: Reg, known: &HashMap<PReg, Provenance>) -> Option<Provenance> {
    known.get(&preg_one(reg)?).copied()
}

fn may_alias(a: MemAccess, b: MemAccess) -> bool {
    match (a.root, b.root) {
        (MemRoot::Global(left), MemRoot::Global(right)) if left != right => false,
        (MemRoot::Global(_), MemRoot::StackSp | MemRoot::StackFp)
        | (MemRoot::StackSp | MemRoot::StackFp, MemRoot::Global(_)) => false,
        (left, right) if left == right => {
            let (Some(a_start), Some(b_start)) = (a.offset, b.offset) else {
                return true;
            };
            let Some(a_end) = a_start.checked_add(i64::from(a.size)) else {
                return true;
            };
            let Some(b_end) = b_start.checked_add(i64::from(b.size)) else {
                return true;
            };
            a_start < b_end && b_start < a_end
        }
        // SP- and FP-relative ranges may address the same frame, but their
        // relationship is unavailable after frame legalization.
        _ => true,
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
            defs: [preg(dst.reg), amode_defs(addr)].concat(),
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
            defs: amode_defs(addr),
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
        AMode::PostIndex { base, .. } => preg(*base),
        _ => vec![],
    }
}

fn amode_defs(addr: &crate::instructions::AMode) -> Vec<PReg> {
    use crate::instructions::AMode;
    match addr {
        // Post-index addressing writes the advanced value back to base.
        AMode::PostIndex { base, .. } => preg(*base),
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
    use taki_mir::{abi::ArgPair, register::Writable};

    use super::*;
    use crate::{
        instructions::{Cond, Imm12, MemoryType},
        regs::{OperandSize, RegOrZr, int_reg},
    };

    fn writable(index: u8) -> Writable<Reg> {
        Writable::from_reg(int_reg(index))
    }

    fn args_deps() -> InstDeps {
        inst_deps(&MInst::Args {
            args: vec![
                ArgPair {
                    vreg: writable(0),
                    preg: int_reg(0),
                },
                ArgPair {
                    vreg: writable(1),
                    preg: int_reg(1),
                },
            ],
        })
    }

    #[test]
    fn args_pseudo_is_a_free_nop_with_register_defs() {
        let deps = args_deps();
        assert_eq!(deps.class, SchedClass::Nop);
        assert!(
            deps.defs
                .iter()
                .all(|p| matches!(*p, p if p.hw_enc() == 0 || p.hw_enc() == 1))
        );
        assert!(deps.uses.is_empty());
        assert!(!deps.flags_def && !deps.flags_use);
        assert!(deps.mem.is_none());
        assert!(!deps.is_barrier);
    }

    #[test]
    fn args_defs_keep_argument_readers_after_the_pseudo() {
        let insts = vec![
            MInst::Args {
                args: vec![ArgPair {
                    vreg: writable(0),
                    preg: int_reg(0),
                }],
            },
            MInst::AluRRR {
                op: crate::instructions::AluOp::Add,
                size: OperandSize::Size64,
                dst: writable(2),
                lhs: RegOrZr::Reg(int_reg(0)),
                rhs: RegOrZr::Reg(int_reg(3)),
            },
        ];
        let graph = DepGraph::build(&insts);
        assert!(has_edge(&graph, 0, 1));
        assert!(!has_edge(&graph, 1, 0));
    }

    #[test]
    fn retval_is_a_free_nop_that_reads_the_return_value() {
        let deps = inst_deps(&MInst::RetVal {
            pair: taki_mir::abi::RetPair {
                vreg: int_reg(0),
                preg: int_reg(0),
            },
        });
        assert_eq!(deps.class, SchedClass::Nop);
        assert_eq!(deps.uses, vec![int_preg(0)]);
        assert!(deps.defs.is_empty());
        assert!(!deps.is_barrier);
    }

    fn has_edge(graph: &DepGraph, from: usize, to: usize) -> bool {
        graph.succs[from].iter().any(|edge| edge.node == to)
    }

    #[test]
    fn records_edge_kind_statistics() {
        use crate::instructions::{AluOp, MemoryType};

        let writable = |index| Writable::from_reg(int_reg(index));
        let insts = vec![
            MInst::Load {
                ty: MemoryType::I64,
                dst: writable(1),
                addr: crate::instructions::AMode::Reg {
                    base: crate::regs::stack_reg(),
                },
            },
            MInst::AluRRR {
                op: AluOp::Add,
                size: OperandSize::Size64,
                dst: writable(2),
                lhs: RegOrZr::Reg(int_reg(1)),
                rhs: RegOrZr::Reg(int_reg(3)),
            },
            MInst::Mov {
                size: OperandSize::Size64,
                dst: writable(2),
                src: int_reg(4),
            },
        ];
        let graph = DepGraph::build(&insts);
        let stats = &graph.stats;

        assert_eq!(stats.nodes, 3);
        assert!(stats.edges >= 2);
        assert!(stats.edge_kind_counts[EdgeKind::RegisterRaw as usize] >= 1);
        assert!(stats.edge_kind_counts[EdgeKind::RegisterWaw as usize] >= 1);
        assert_eq!(stats.memory_accesses, 1);
        assert_eq!(stats.known_root_accesses, 1);
        assert_eq!(stats.unknown_root_accesses, 0);
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
    fn separates_non_overlapping_stack_ranges() {
        let insts = vec![
            MInst::Store {
                ty: MemoryType::I64,
                src: int_reg(1),
                addr: AMode::SignedOffset {
                    base: crate::regs::stack_reg(),
                    offset: crate::instructions::SImm9::new(0).unwrap(),
                },
            },
            MInst::Load {
                ty: MemoryType::I64,
                dst: writable(2),
                addr: AMode::SignedOffset {
                    base: crate::regs::stack_reg(),
                    offset: crate::instructions::SImm9::new(8).unwrap(),
                },
            },
        ];
        let graph = DepGraph::build(&insts);

        assert!(!has_edge(&graph, 0, 1));
    }

    #[test]
    fn preserves_overlapping_stack_ranges() {
        let insts = vec![
            MInst::Store {
                ty: MemoryType::I64,
                src: int_reg(1),
                addr: AMode::SignedOffset {
                    base: crate::regs::stack_reg(),
                    offset: crate::instructions::SImm9::new(0).unwrap(),
                },
            },
            MInst::Load {
                ty: MemoryType::I32,
                dst: writable(2),
                addr: AMode::SignedOffset {
                    base: crate::regs::stack_reg(),
                    offset: crate::instructions::SImm9::new(4).unwrap(),
                },
            },
        ];
        let graph = DepGraph::build(&insts);

        assert!(has_edge(&graph, 0, 1));
    }

    #[test]
    fn a_disjoint_store_does_not_hide_an_older_alias() {
        let stack_addr = |offset| AMode::SignedOffset {
            base: crate::regs::stack_reg(),
            offset: crate::instructions::SImm9::new(offset).unwrap(),
        };
        let insts = vec![
            MInst::Store {
                ty: MemoryType::I64,
                src: int_reg(1),
                addr: stack_addr(0),
            },
            MInst::Store {
                ty: MemoryType::I64,
                src: int_reg(2),
                addr: stack_addr(16),
            },
            MInst::Load {
                ty: MemoryType::I64,
                dst: writable(3),
                addr: stack_addr(0),
            },
        ];
        let graph = DepGraph::build(&insts);

        assert!(has_edge(&graph, 0, 2));
        assert!(!has_edge(&graph, 1, 2));
    }

    #[test]
    fn tracks_stack_addresses_through_moves_and_adds() {
        let insts = vec![
            MInst::MovPhys {
                size: OperandSize::Size64,
                dst: writable(4),
                src: crate::regs::stack_reg(),
            },
            MInst::AluRRImm12 {
                op: AluOp::Add,
                size: OperandSize::Size64,
                dst: writable(5),
                src: int_reg(4),
                imm: Imm12::new(16, false).unwrap(),
            },
            MInst::Store {
                ty: MemoryType::I64,
                src: int_reg(1),
                addr: AMode::Reg { base: int_reg(5) },
            },
            MInst::Load {
                ty: MemoryType::I64,
                dst: writable(2),
                addr: AMode::Reg {
                    base: crate::regs::stack_reg(),
                },
            },
        ];
        let graph = DepGraph::build(&insts);

        assert!(!has_edge(&graph, 2, 3));
    }

    #[test]
    fn keeps_unknown_and_sp_vs_fp_accesses_ordered() {
        let insts = vec![
            MInst::Store {
                ty: MemoryType::I64,
                src: int_reg(1),
                addr: AMode::Reg { base: int_reg(10) },
            },
            MInst::Load {
                ty: MemoryType::I64,
                dst: writable(2),
                addr: AMode::Reg {
                    base: crate::regs::stack_reg(),
                },
            },
            MInst::Store {
                ty: MemoryType::I64,
                src: int_reg(3),
                addr: AMode::Reg {
                    base: crate::regs::int_reg(crate::regs::FP),
                },
            },
        ];
        let graph = DepGraph::build(&insts);

        assert!(has_edge(&graph, 0, 1));
        assert!(has_edge(&graph, 1, 2));
    }

    #[test]
    fn pair_ranges_and_writeback_remain_safe() {
        let insts = vec![
            MInst::StorePair {
                ty: MemoryType::I64,
                src1: int_reg(1),
                src2: int_reg(2),
                addr: PairAMode::SignedOffset {
                    base: crate::regs::stack_reg(),
                    offset: crate::instructions::SImm7Scaled::new(0, 8).unwrap(),
                },
            },
            MInst::Load {
                ty: MemoryType::I64,
                dst: writable(3),
                addr: AMode::SignedOffset {
                    base: crate::regs::stack_reg(),
                    offset: crate::instructions::SImm9::new(8).unwrap(),
                },
            },
            MInst::Load {
                ty: MemoryType::I64,
                dst: writable(4),
                addr: AMode::SignedOffset {
                    base: crate::regs::stack_reg(),
                    offset: crate::instructions::SImm9::new(16).unwrap(),
                },
            },
            MInst::StorePair {
                ty: MemoryType::I64,
                src1: int_reg(5),
                src2: int_reg(6),
                addr: PairAMode::PostIndex {
                    base: crate::regs::stack_reg(),
                    offset: crate::instructions::SImm7Scaled::new(16, 8).unwrap(),
                },
            },
        ];
        let graph = DepGraph::build(&insts);

        assert!(has_edge(&graph, 0, 1));
        assert!(!has_edge(&graph, 0, 2));
        assert!(has_edge(&graph, 1, 3));
        assert!(has_edge(&graph, 2, 3));
    }

    #[test]
    fn register_redefinition_kills_address_provenance() {
        let insts = vec![
            MInst::MovPhys {
                size: OperandSize::Size64,
                dst: writable(4),
                src: crate::regs::stack_reg(),
            },
            MInst::LoadImm {
                size: OperandSize::Size64,
                dst: writable(4),
                value: 0,
            },
            MInst::Store {
                ty: MemoryType::I64,
                src: int_reg(1),
                addr: AMode::Reg { base: int_reg(4) },
            },
            MInst::Load {
                ty: MemoryType::I64,
                dst: writable(2),
                addr: AMode::SignedOffset {
                    base: crate::regs::stack_reg(),
                    offset: crate::instructions::SImm9::new(32).unwrap(),
                },
            },
        ];
        let graph = DepGraph::build(&insts);

        assert!(has_edge(&graph, 2, 3));
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
