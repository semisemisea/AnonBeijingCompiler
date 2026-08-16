//! Dependency DAG construction and edge bookkeeping.

use rustc_hash::FxHashMap;

use taki_mir::reg_alloc::reg::PReg;

use crate::instructions::MInst;

use super::inst_deps::{InstDeps, inst_deps};
use super::memory::{MemKind, MemRoot, annotate_memory_accesses, may_alias};

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
        let mut last_def: FxHashMap<PReg, usize> = FxHashMap::default();
        let mut pending_uses: FxHashMap<PReg, Vec<usize>> = FxHashMap::default();

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
                for (prev, dep) in deps.iter().enumerate().take(i) {
                    add_edge(
                        &mut succs,
                        &mut preds,
                        &mut stats,
                        prev,
                        i,
                        dep.profile().latency,
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

pub(super) fn add_edge(
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
