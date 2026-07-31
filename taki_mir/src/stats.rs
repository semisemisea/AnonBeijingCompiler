//! Structured statistics produced by MIR passes and code generation.

use std::collections::BTreeMap;

/// Complete statistics for one compilation unit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodegenStats {
    pub functions: Vec<FunctionCodegenStats>,
    pub total: FunctionCodegenStats,
}

impl CodegenStats {
    pub fn aggregate(functions: Vec<FunctionCodegenStats>) -> Self {
        let mut total = FunctionCodegenStats::default();
        for function in &functions {
            total.accumulate(function);
        }
        Self { functions, total }
    }
}

/// Statistics collected while compiling one function.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FunctionCodegenStats {
    pub function: String,
    pub dce: DceStats,
    pub peephole: PeepholeStats,
    pub pair: PairCombineStats,
    pub scheduler: SchedulerStats,
}

impl FunctionCodegenStats {
    fn accumulate(&mut self, other: &Self) {
        self.dce.accumulate(&other.dce);
        self.peephole.accumulate(&other.peephole);
        self.pair.accumulate(&other.pair);
        self.scheduler.accumulate(&other.scheduler);
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DceStats {
    pub ran: bool,
    pub changed: bool,
    pub instructions_removed: u64,
}

impl DceStats {
    fn accumulate(&mut self, other: &Self) {
        self.ran |= other.ran;
        self.changed |= other.changed;
        self.instructions_removed += other.instructions_removed;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeepholeStats {
    pub ran: bool,
    pub changed: bool,
    pub mac_pairs_formed: u64,
}

impl PeepholeStats {
    fn accumulate(&mut self, other: &Self) {
        self.ran |= other.ran;
        self.changed |= other.changed;
        self.mac_pairs_formed += other.mac_pairs_formed;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PairCombineStats {
    pub ran: bool,
    pub changed: bool,
    pub load_pairs_formed: u64,
    pub store_pairs_formed: u64,
    pub tombstone_removed_created: u64,
}

impl PairCombineStats {
    fn accumulate(&mut self, other: &Self) {
        self.ran |= other.ran;
        self.changed |= other.changed;
        self.load_pairs_formed += other.load_pairs_formed;
        self.store_pairs_formed += other.store_pairs_formed;
        self.tombstone_removed_created += other.tombstone_removed_created;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchedulerStats {
    pub ran: bool,
    pub blocks_total: u64,
    pub blocks_checked: u64,
    pub blocks_skipped_short: u64,
    pub identity_schedules: u64,
    pub estimator_rejections: u64,
    pub fallbacks: Vec<SchedulerFallback>,
    pub original: CycleEstimateStats,
    pub scheduled: CycleEstimateStats,
    pub dag: DagStats,
    pub timings: SchedulerTimings,
}

impl SchedulerStats {
    fn accumulate(&mut self, other: &Self) {
        self.ran |= other.ran;
        self.blocks_total += other.blocks_total;
        self.blocks_checked += other.blocks_checked;
        self.blocks_skipped_short += other.blocks_skipped_short;
        self.identity_schedules += other.identity_schedules;
        self.estimator_rejections += other.estimator_rejections;
        self.fallbacks.extend(other.fallbacks.iter().cloned());
        self.original.accumulate(&other.original);
        self.scheduled.accumulate(&other.scheduled);
        self.dag.accumulate(&other.dag);
        self.timings.accumulate(&other.timings);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchedulerFallback {
    pub block: usize,
    pub reason: SchedulerFallbackReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedulerFallbackReason {
    StallBudgetExhausted,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CycleEstimateStats {
    pub samples: u64,
    pub completion_cycles: u64,
    pub stall_cycles: u64,
    pub single_issue_cycles: u64,
    pub dual_issue_cycles: u64,
    pub idle_cycles: u64,
    pub resources: ResourceUseStats,
}

impl CycleEstimateStats {
    fn accumulate(&mut self, other: &Self) {
        self.samples += other.samples;
        self.completion_cycles += other.completion_cycles;
        self.stall_cycles += other.stall_cycles;
        self.single_issue_cycles += other.single_issue_cycles;
        self.dual_issue_cycles += other.dual_issue_cycles;
        self.idle_cycles += other.idle_cycles;
        self.resources.accumulate(&other.resources);
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResourceUseStats {
    pub lsu: u64,
    pub alu: u64,
    pub mac_div: u64,
    pub fp_other: u64,
    pub branch: u64,
}

impl ResourceUseStats {
    fn accumulate(&mut self, other: &Self) {
        self.lsu += other.lsu;
        self.alu += other.alu;
        self.mac_div += other.mac_div;
        self.fp_other += other.fp_other;
        self.branch += other.branch;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DagStats {
    pub nodes: u64,
    pub edges: u64,
    pub edges_by_kind: BTreeMap<String, u64>,
    pub memory: MemoryDagStats,
    pub max_block_nodes: u64,
}

impl DagStats {
    fn accumulate(&mut self, other: &Self) {
        self.nodes += other.nodes;
        self.edges += other.edges;
        for (kind, count) in &other.edges_by_kind {
            *self.edges_by_kind.entry(kind.clone()).or_default() += count;
        }
        self.memory.accumulate(&other.memory);
        self.max_block_nodes = self.max_block_nodes.max(other.max_block_nodes);
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryDagStats {
    pub accesses: u64,
    pub known_root_accesses: u64,
    pub unknown_root_accesses: u64,
    pub comparisons: u64,
    pub disjoint_comparisons: u64,
    pub may_alias_comparisons: u64,
    pub max_history_len: u64,
}

impl MemoryDagStats {
    fn accumulate(&mut self, other: &Self) {
        self.accesses += other.accesses;
        self.known_root_accesses += other.known_root_accesses;
        self.unknown_root_accesses += other.unknown_root_accesses;
        self.comparisons += other.comparisons;
        self.disjoint_comparisons += other.disjoint_comparisons;
        self.may_alias_comparisons += other.may_alias_comparisons;
        self.max_history_len = self.max_history_len.max(other.max_history_len);
    }
}

/// Observational timings; unlike deterministic counters these may differ
/// between runs and should not be used for byte-for-byte report comparisons.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchedulerTimings {
    pub dag_build_ns: u64,
    pub schedule_ns: u64,
    pub estimate_ns: u64,
}

impl SchedulerTimings {
    fn accumulate(&mut self, other: &Self) {
        self.dag_build_ns += other.dag_build_ns;
        self.schedule_ns += other.schedule_ns;
        self.estimate_ns += other.estimate_ns;
    }
}
