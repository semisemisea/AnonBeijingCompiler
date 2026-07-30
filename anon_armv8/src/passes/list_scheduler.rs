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
use crate::sched::aarch53::SchedClass;
use crate::sched::dag::DepGraph;
use crate::sched::simulator::CycleSimulator;

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
///
/// Uses `CycleSimulator` for all latency/resource decisions so that the
/// scheduler and the fixed-order estimator share exactly one model.
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
    let mut sim = CycleSimulator::new();

    while order.len() < n {
        let mut issued_this_cycle = false;
        let mut issued_classes = Vec::with_capacity(2);

        // Deterministic priority: critical path (desc), then original index (asc).
        // When a slot is already occupied, prefer nodes that can fill the
        // remaining slot (dual-issue bonus).
        ready.sort_by(|&a, &b| {
            dag.crit[b]
                .cmp(&dag.crit[a])
                .then_with(|| {
                    // If one instruction is already issued, prefer a compatible
                    // second instruction to maximize dual-issue.
                    if !issued_classes.is_empty() {
                        let a_compat =
                            CycleSimulator::can_issue(dag.deps[a].class, &issued_classes);
                        let b_compat =
                            CycleSimulator::can_issue(dag.deps[b].class, &issued_classes);
                        b_compat.cmp(&a_compat)
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
                .then(a.cmp(&b))
        });

        // Try to issue ready nodes whose data dependencies are satisfied.
        let mut next_ready = Vec::new();
        for &node in &ready {
            let earliest = CycleSimulator::earliest_issue_cycle(dag, node, &issued_at);
            let data_ready = earliest <= cycle;

            let class = dag.deps[node].class;
            if data_ready
                && sim.is_ready(class, cycle)
                && CycleSimulator::can_issue(class, &issued_classes)
            {
                issued_at[node] = Some(cycle);
                order.push(node);
                issued_classes.push(class);
                issued_this_cycle = true;

                sim.reserve(class, cycle);

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

/// Estimate completion cycles for a fixed topological instruction order using
/// the same `CycleSimulator` model as the scheduler. Both paths share the
/// identical latency, resource, and issue-width rules.
fn estimate_cycles(dag: &DepGraph, order: &[usize]) -> Option<CycleEstimateStats> {
    if order.len() != dag.n {
        return None;
    }

    let mut seen = vec![false; dag.n];
    let mut issued_at = vec![None; dag.n];
    let mut sim = CycleSimulator::new();
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
            let earliest = CycleSimulator::earliest_issue_cycle(dag, node, &issued_at);
            let data_ready = earliest <= cycle;

            let class = dag.deps[node].class;
            if !data_ready
                || !sim.is_ready(class, cycle)
                || !CycleSimulator::can_issue(class, &issued_classes)
            {
                break;
            }

            seen[node] = true;
            issued_at[node] = Some(cycle);
            issued_classes.push(class);
            sim.reserve(class, cycle);
            completion_cycle = completion_cycle.max(CycleSimulator::completion_cycle(class, cycle));
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
                SchedClass::LoadInt
                | SchedClass::StoreInt
                | SchedClass::LoadFp
                | SchedClass::StoreFp
                | SchedClass::LoadPairInt
                | SchedClass::StorePairInt
                | SchedClass::LoadPairFp
                | SchedClass::StorePairFp => result.resources.lsu += 1,
                SchedClass::Alu | SchedClass::AluShift | SchedClass::AluMisc => {
                    result.resources.alu += 1
                }
                SchedClass::Mul | SchedClass::Div32 | SchedClass::Div64 => {
                    result.resources.mac_div += 1
                }
                SchedClass::FpMove
                | SchedClass::FpAddSub
                | SchedClass::FpMul
                | SchedClass::FpDiv
                | SchedClass::FpCmp
                | SchedClass::FpCvt
                | SchedClass::Other => result.resources.fp_other += 1,
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
        assert!(CycleSimulator::can_issue(SchedClass::Alu, &[]));
        assert!(CycleSimulator::can_issue(
            SchedClass::Alu,
            &[SchedClass::Alu]
        ));
        assert!(!CycleSimulator::can_issue(
            SchedClass::Alu,
            &[SchedClass::Alu, SchedClass::Alu]
        ));
        assert!(!CycleSimulator::can_issue(
            SchedClass::StoreInt,
            &[SchedClass::LoadInt]
        ));
        assert!(!CycleSimulator::can_issue(
            SchedClass::Mul,
            &[SchedClass::Mul]
        ));
        assert!(!CycleSimulator::can_issue(
            SchedClass::Barrier,
            &[SchedClass::Alu]
        ));
        let mut sim = CycleSimulator::new();
        sim.reserve(SchedClass::Div32, 0);
        assert!(!sim.is_ready(SchedClass::Mul, 10));
        assert!(!sim.is_ready(SchedClass::Div32, 10));
        assert!(sim.is_ready(SchedClass::Div32, 11));
    }

    #[test]
    fn schedules_every_independent_instruction_once() {
        let graph = independent_graph(&[
            SchedClass::LoadInt,
            SchedClass::StoreInt,
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
        let graph = independent_graph(&[SchedClass::Div32]);

        assert_eq!(
            estimate_cycles(&graph, &[0]).map(|e| e.completion_cycles),
            Some(11)
        );
    }

    #[test]
    fn div64_has_higher_latency_than_div32() {
        let g32 = independent_graph(&[SchedClass::Div32]);
        let g64 = independent_graph(&[SchedClass::Div64]);
        assert_eq!(
            estimate_cycles(&g32, &[0]).map(|e| e.completion_cycles),
            Some(11)
        );
        assert_eq!(
            estimate_cycles(&g64, &[0]).map(|e| e.completion_cycles),
            Some(19)
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

    #[test]
    fn critical_path_uses_edge_latency_not_node_latency() {
        // A zero-latency WAR edge should not inflate the critical path.
        // mov x1, x2  (reads x2)
        // mov x2, x3  (writes x2 — WAR edge from 0->1 with latency 0)
        // Without edge-latency critical path, node 0's crit would be
        //   latency(0) + crit(1) = 1 + 1 = 2.
        // With edge-latency critical path, node 0's crit should be
        //   max(1, 0 + 1) = 1.
        use crate::instructions::MInst;
        use crate::regs::OperandSize;

        let insts = vec![
            MInst::Mov {
                size: OperandSize::Size64,
                dst: taki_mir::register::Writable::from_reg(crate::regs::int_reg(1)),
                src: crate::regs::int_reg(2),
            },
            MInst::Mov {
                size: OperandSize::Size64,
                dst: taki_mir::register::Writable::from_reg(crate::regs::int_reg(2)),
                src: crate::regs::int_reg(3),
            },
        ];
        let graph = DepGraph::build(&insts);

        // Node 1 (leaf): crit = its own latency = 1
        assert_eq!(graph.crit[1], 1);
        // Node 0: max(1, 0 + 1) = 1 — not 1 + 1 = 2
        assert_eq!(graph.crit[0], 1);
    }

    #[test]
    fn scheduler_output_is_deterministic() {
        use crate::instructions::{AMode, AluOp, MemoryType};
        use crate::regs::{OperandSize, RegOrZr, int_reg, stack_reg};

        let writable = |index| taki_mir::register::Writable::from_reg(int_reg(index));
        let alu = |dst, lhs, rhs| MInst::AluRRR {
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
            alu(2, 1, 3),
            alu(4, 5, 6),
            alu(7, 8, 9),
        ];

        let (order1, _) = schedule(&DepGraph::build(&insts));
        let (order2, _) = schedule(&DepGraph::build(&insts));
        assert_eq!(order1, order2, "same input must produce same schedule");
    }

    #[test]
    fn scheduler_and_estimator_agree_on_latency_rule() {
        // Both paths must use the same `earliest_issue_cycle` from CycleSimulator.
        // A Nop (latency 0) followed by an ALU must be scheduled in consecutive
        // cycles, not the same cycle — the unified rule treats 0-latency edges
        // as "can issue in the same cycle" via earliest_issue_cycle.
        use crate::instructions::MInst;

        let insts = vec![
            MInst::Nop,
            MInst::Mov {
                size: crate::regs::OperandSize::Size64,
                dst: taki_mir::register::Writable::from_reg(crate::regs::int_reg(1)),
                src: crate::regs::int_reg(2),
            },
        ];
        let graph = DepGraph::build(&insts);
        let original = estimate_cycles(&graph, &[0, 1]).unwrap();
        let (sched_order, _) = schedule(&graph);
        let scheduled = estimate_cycles(&graph, &sched_order).unwrap();

        // Nop has latency 0, so the Mov can issue at cycle 0 or 1.
        // Both original and scheduled should agree.
        assert_eq!(original.completion_cycles, scheduled.completion_cycles);
    }

    #[test]
    fn slot_filling_maximizes_dual_issue() {
        // Three independent ALU ops: the scheduler should pack two into cycle 0
        // and one into cycle 1, rather than spreading them one per cycle.
        use crate::instructions::{AluOp, MInst};
        use crate::regs::{OperandSize, RegOrZr, int_reg};

        let writable = |index| taki_mir::register::Writable::from_reg(int_reg(index));
        let alu = |dst, lhs, rhs| MInst::AluRRR {
            op: AluOp::Add,
            size: OperandSize::Size64,
            dst: writable(dst),
            lhs: RegOrZr::Reg(int_reg(lhs)),
            rhs: RegOrZr::Reg(int_reg(rhs)),
        };
        let insts = vec![alu(1, 2, 3), alu(4, 5, 6), alu(7, 8, 9)];
        let graph = DepGraph::build(&insts);
        let (order, _) = schedule(&graph);
        let estimate = estimate_cycles(&graph, &order).unwrap();

        // 3 independent ALU ops should complete in 2 cycles (dual-issue first two).
        assert_eq!(estimate.completion_cycles, 2);
        assert!(estimate.dual_issue_cycles >= 1);
    }
}
