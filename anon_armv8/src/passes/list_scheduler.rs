//! Post-RA list scheduler for Cortex-A53.
//!
//! Reorders instructions within each basic block to hide load-use latency
//! and improve dual-issue opportunities on the in-order Cortex-A53 pipeline.
//! Runs after `finalize_for_emission` (all registers physical, all moves
//! materialized, all addresses legalized) and before assembly emission.
//!
//! Algorithm: classic latency-aware list scheduling on a dependency DAG.
//! Priority = critical-path length. A node becomes ready when all its
//! predecessors have been issued for at least their latency duration.

use std::collections::HashSet;

use taki_mir::{
    passes::MIRPass,
    prelude::ArenaContext,
    stats::{
        CycleEstimateStats, FunctionCodegenStats, ResourceUseStats, SchedulerFallback,
        SchedulerFallbackReason,
    },
    vcode::{MachInst, VCodeContainer},
};

use crate::instructions::MInst;
use crate::sched::aarch53::{SchedClass, instr_profile};
use crate::sched::dag::DepGraph;

pub struct ListScheduler;

impl MIRPass<MInst> for ListScheduler {
    fn name(&self) -> &'static str {
        "ListScheduler"
    }

    fn run(
        &self,
        vcode: &mut VCodeContainer<MInst>,
        _arena: ArenaContext,
        stats: &mut FunctionCodegenStats,
    ) -> bool {
        stats.scheduler.ran = true;
        let mut any_changed = false;

        for block_idx in 0..vcode.num_blocks() {
            stats.scheduler.blocks_total += 1;
            let range = vcode.block_inst_range(block_idx);
            if range.len() <= 2 {
                stats.scheduler.blocks_skipped_short += 1;
                continue;
            }
            stats.scheduler.blocks_checked += 1;

            let insts: Vec<MInst> = vcode.block_insts(block_idx).to_vec();

            // Control flow is represented as a full barrier in the DAG, so a
            // terminator remains last while its register/NZCV uses stay visible.
            let dag = DepGraph::build(&insts);
            dag.stats.record_to_taki_stats(&mut stats.scheduler.dag);
            let (order, fallback) = schedule(&dag);
            if let Some(reason) = fallback {
                stats.scheduler.fallbacks.push(SchedulerFallback {
                    block: block_idx,
                    reason,
                });
            }
            let original_order: Vec<_> = (0..dag.n).collect();
            let original_estimate = estimate_cycles(&dag, &original_order);
            let scheduled_estimate = estimate_cycles(&dag, &order);
            accumulate_estimate(&mut stats.scheduler.original, original_estimate.as_ref());
            accumulate_estimate(&mut stats.scheduler.scheduled, scheduled_estimate.as_ref());
            let improves_or_matches = matches!(
                (scheduled_estimate, original_estimate),
                (Some(scheduled), Some(original)) if scheduled.completion_cycles <= original.completion_cycles
            );

            if is_identity(&order, insts.len()) {
                stats.scheduler.identity_schedules += 1;
                continue;
            }
            if !improves_or_matches {
                stats.scheduler.estimator_rejections += 1;
                continue;
            }

            // Write the scheduled instructions back.
            for (local, orig) in order.iter().enumerate() {
                let global = range.start + local;
                *vcode.inst_mut(global) = insts[*orig].clone();
            }

            // Rebuild inst_is_branch / inst_is_ret for this block.
            for local in 0..range.len() {
                let global = range.start + local;
                let term = vcode.inst(global).is_term();
                vcode.set_inst_terminator(global, term);
            }

            any_changed = true;
        }

        any_changed
    }
}

/// Run list scheduling on the dependency DAG, returning a permutation
/// `order` where `order[new_position] = old_index`, plus an optional fallback
/// reason if the stall budget was exhausted.
fn schedule(dag: &DepGraph) -> (Vec<usize>, Option<SchedulerFallbackReason>) {
    let n = dag.n;
    if n == 0 {
        return (vec![], None);
    }

    // remaining_preds[i] = number of unscheduled predecessors
    let mut remaining_preds: Vec<usize> = (0..n).map(|i| dag.preds[i].len()).collect();

    // issued_at[i] = cycle when node i was issued (None if not yet issued)
    let mut issued_at: Vec<Option<u32>> = vec![None; n];

    // result permutation
    let mut order: Vec<usize> = Vec::with_capacity(n);

    // ready set: nodes with all predecessors scheduled
    let mut ready: Vec<usize> = (0..n).filter(|&i| remaining_preds[i] == 0).collect();

    let mut cycle: u32 = 0;
    let mut stall_budget: u32 = n as u32 * 20; // safety: prevent infinite loops
    let mut resources = IssueResources::default();

    while order.len() < n {
        let mut issued_this_cycle = false;
        let mut issued_classes = Vec::with_capacity(2);

        // Sort ready nodes by critical path (descending) — highest priority first.
        ready.sort_by(|&a, &b| dag.crit[b].cmp(&dag.crit[a]));

        // Try to issue ready nodes whose data dependencies are satisfied.
        let mut next_ready = Vec::new();
        for &node in &ready {
            let data_ready = dag.preds[node].iter().all(|&pred| {
                let issued = issued_at[pred].unwrap_or(0);
                let latency = dag.succs[pred]
                    .iter()
                    .find(|edge| edge.node == node)
                    .map(|edge| edge.latency)
                    .unwrap_or(0);
                issued + latency <= cycle
            });

            let class = dag.deps[node].class;
            if data_ready && resources.is_ready(class, cycle) && can_issue(class, &issued_classes) {
                issued_at[node] = Some(cycle);
                order.push(node);
                issued_classes.push(class);
                issued_this_cycle = true;

                resources.reserve(class, cycle);

                // Decrement successor predecessor counts; add newly-ready nodes.
                for edge in &dag.succs[node] {
                    let succ = edge.node;
                    remaining_preds[succ] -= 1;
                    if remaining_preds[succ] == 0 {
                        next_ready.push(succ);
                    }
                }
            } else {
                next_ready.push(node);
            }
        }
        ready = next_ready;

        cycle += 1;
        if issued_this_cycle {
            stall_budget = n as u32 * 20;
        } else {
            stall_budget = stall_budget.saturating_sub(1);
        }
        if stall_budget == 0 {
            // Safety fallback: emit remaining nodes in original order.
            let scheduled: HashSet<usize> = order.iter().copied().collect();
            for i in 0..n {
                if !scheduled.contains(&i) {
                    order.push(i);
                }
            }
            return (order, Some(SchedulerFallbackReason::StallBudgetExhausted));
        }
    }

    (order, None)
}

#[derive(Default)]
struct IssueResources {
    mul_div_available_at: u32,
}

impl IssueResources {
    fn is_ready(&self, class: SchedClass, cycle: u32) -> bool {
        !matches!(class, SchedClass::Mul | SchedClass::Div) || cycle >= self.mul_div_available_at
    }

    fn reserve(&mut self, class: SchedClass, cycle: u32) {
        if matches!(class, SchedClass::Mul | SchedClass::Div) {
            let occupancy = if class == SchedClass::Div {
                instr_profile(class).latency
            } else {
                1
            };
            self.mul_div_available_at = cycle + occupancy;
        }
    }
}

fn can_issue(class: SchedClass, issued: &[SchedClass]) -> bool {
    if issued.len() >= 2 {
        return false;
    }
    if matches!(class, SchedClass::Barrier) || issued.contains(&SchedClass::Barrier) {
        return issued.is_empty();
    }

    let uses_lsu = |class| matches!(class, SchedClass::Load | SchedClass::Store);
    let uses_mac = |class| matches!(class, SchedClass::Mul | SchedClass::Div);
    let uses_fp = |class| matches!(class, SchedClass::Other);

    !(uses_lsu(class) && issued.iter().copied().any(uses_lsu)
        || uses_mac(class) && issued.iter().copied().any(uses_mac)
        || uses_fp(class) && issued.iter().copied().any(uses_fp))
}

/// Estimate completion cycles for a fixed topological instruction order using
/// the same dependency latency, issue width, and resource model as the scheduler.
fn estimate_cycles(dag: &DepGraph, order: &[usize]) -> Option<CycleEstimateStats> {
    if order.len() != dag.n {
        return None;
    }

    let mut seen = vec![false; dag.n];
    let mut issued_at = vec![None; dag.n];
    let mut resources = IssueResources::default();
    let mut next = 0;
    let mut cycle = 0;
    let mut completion_cycle = 0;
    let mut stall_budget = dag.n as u32 * 20;
    let mut result = CycleEstimateStats {
        samples: 1,
        ..CycleEstimateStats::default()
    };

    while next < order.len() {
        let mut issued_classes = Vec::with_capacity(2);
        while next < order.len() {
            let node = order[next];
            if node >= dag.n || seen[node] {
                return None;
            }
            if dag.preds[node].iter().any(|&pred| !seen[pred]) {
                return None;
            }
            let data_ready = dag.preds[node].iter().all(|&pred| {
                let latency = dag.succs[pred]
                    .iter()
                    .find(|edge| edge.node == node)
                    .map(|edge| edge.latency)
                    .unwrap_or(0);
                issued_at[pred].is_some_and(|issued| issued + latency.max(1) <= cycle)
            });
            let class = dag.deps[node].class;
            if !data_ready
                || !resources.is_ready(class, cycle)
                || !can_issue(class, &issued_classes)
            {
                break;
            }

            seen[node] = true;
            issued_at[node] = Some(cycle);
            issued_classes.push(class);
            resources.reserve(class, cycle);
            completion_cycle = completion_cycle.max(cycle + instr_profile(class).latency.max(1));
            next += 1;
        }

        match issued_classes.len() {
            0 => {
                result.idle_cycles += 1;
            }
            1 => {
                result.single_issue_cycles += 1;
            }
            _ => {
                result.dual_issue_cycles += 1;
            }
        }
        for class in &issued_classes {
            match class {
                SchedClass::Load | SchedClass::Store => result.resources.lsu += 1,
                SchedClass::Alu => result.resources.alu += 1,
                SchedClass::Mul | SchedClass::Div => result.resources.mac_div += 1,
                SchedClass::Other => result.resources.fp_other += 1,
                SchedClass::Branch | SchedClass::Barrier => result.resources.branch += 1,
                SchedClass::Nop => {}
            }
        }

        cycle += 1;
        if issued_classes.is_empty() {
            stall_budget = stall_budget.saturating_sub(1);
            if stall_budget == 0 {
                return None;
            }
        } else {
            stall_budget = dag.n as u32 * 20;
        }
    }

    result.completion_cycles = u64::from(completion_cycle);
    result.stall_cycles = result.idle_cycles;
    Some(result)
}

fn accumulate_estimate(target: &mut CycleEstimateStats, estimate: Option<&CycleEstimateStats>) {
    let Some(estimate) = estimate else { return };
    target.samples += estimate.samples;
    target.completion_cycles += estimate.completion_cycles;
    target.stall_cycles += estimate.stall_cycles;
    target.single_issue_cycles += estimate.single_issue_cycles;
    target.dual_issue_cycles += estimate.dual_issue_cycles;
    target.idle_cycles += estimate.idle_cycles;
    accumulate_resources(&mut target.resources, &estimate.resources);
}

fn accumulate_resources(target: &mut ResourceUseStats, source: &ResourceUseStats) {
    target.lsu += source.lsu;
    target.alu += source.alu;
    target.mac_div += source.mac_div;
    target.fp_other += source.fp_other;
    target.branch += source.branch;
}

/// Check if a permutation is the identity (0, 1, 2, ..., n-1).
fn is_identity(order: &[usize], n: usize) -> bool {
    order.len() == n && order.iter().enumerate().all(|(i, &v)| i == v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sched::dag::InstDeps;

    fn deps(class: SchedClass) -> InstDeps {
        InstDeps {
            defs: vec![],
            uses: vec![],
            flags_def: false,
            flags_use: false,
            class,
            mem: None,
            is_barrier: false,
        }
    }

    fn independent_graph(classes: &[SchedClass]) -> DepGraph {
        DepGraph {
            n: classes.len(),
            succs: vec![vec![]; classes.len()],
            preds: vec![vec![]; classes.len()],
            deps: classes.iter().copied().map(deps).collect(),
            crit: vec![1; classes.len()],
            stats: Default::default(),
        }
    }

    #[test]
    fn issue_model_limits_width_and_shared_units() {
        assert!(can_issue(SchedClass::Alu, &[]));
        assert!(can_issue(SchedClass::Alu, &[SchedClass::Alu]));
        assert!(!can_issue(
            SchedClass::Alu,
            &[SchedClass::Alu, SchedClass::Alu]
        ));
        assert!(!can_issue(SchedClass::Store, &[SchedClass::Load]));
        assert!(!can_issue(SchedClass::Mul, &[SchedClass::Mul]));
        assert!(!can_issue(SchedClass::Barrier, &[SchedClass::Alu]));
        let mut resources = IssueResources::default();
        resources.mul_div_available_at = 11;
        assert!(!resources.is_ready(SchedClass::Mul, 10));
        assert!(!resources.is_ready(SchedClass::Div, 10));
        assert!(resources.is_ready(SchedClass::Div, 11));
    }

    #[test]
    fn schedules_every_independent_instruction_once() {
        let graph = independent_graph(&[
            SchedClass::Load,
            SchedClass::Store,
            SchedClass::Alu,
            SchedClass::Alu,
            SchedClass::Mul,
        ]);
        let (mut order, fallback) = schedule(&graph);
        assert_eq!(fallback, None);
        order.sort_unstable();
        assert_eq!(order, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn fixed_order_estimator_rejects_invalid_permutations() {
        let graph = independent_graph(&[SchedClass::Alu, SchedClass::Alu]);

        assert_eq!(estimate_cycles(&graph, &[0]), None);
        assert_eq!(estimate_cycles(&graph, &[0, 0]), None);
        assert_eq!(estimate_cycles(&graph, &[0, 2]), None);
    }

    #[test]
    fn fixed_order_estimator_includes_final_instruction_latency() {
        let graph = independent_graph(&[SchedClass::Div]);

        assert_eq!(
            estimate_cycles(&graph, &[0]).map(|e| e.completion_cycles),
            Some(11)
        );
    }

    #[test]
    fn cortex_a53_model_benchmark_hides_load_use_stall() {
        use taki_mir::register::Writable;

        use crate::{
            instructions::{AMode, AluOp, MemoryType},
            regs::{OperandSize, RegOrZr, int_reg, stack_reg},
        };

        let writable = |index| Writable::from_reg(int_reg(index));
        let independent_alu = |dst, lhs, rhs| MInst::AluRRR {
            op: AluOp::Add,
            size: OperandSize::Size64,
            dst: writable(dst),
            lhs: RegOrZr::Reg(int_reg(lhs)),
            rhs: RegOrZr::Reg(int_reg(rhs)),
        };
        let insts = vec![
            MInst::Load {
                ty: MemoryType::I64,
                dst: writable(1),
                addr: AMode::Reg { base: stack_reg() },
            },
            independent_alu(2, 1, 3),
            independent_alu(4, 5, 6),
            independent_alu(7, 8, 9),
            independent_alu(10, 11, 12),
        ];
        let graph = DepGraph::build(&insts);
        let original = estimate_cycles(&graph, &[0, 1, 2, 3, 4]).unwrap();
        let (scheduled_order, _) = schedule(&graph);
        let scheduled = estimate_cycles(&graph, &scheduled_order).unwrap();

        assert_eq!(original.completion_cycles, 4);
        assert_eq!(scheduled.completion_cycles, 3);
        assert!(scheduled.completion_cycles < original.completion_cycles);
    }
}
