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

    fn run(&self, vcode: &mut VCodeContainer<MInst>, _arena: ArenaContext) -> bool {
        let mut any_changed = false;

        for block_idx in 0..vcode.num_blocks() {
            let range = vcode.block_inst_range(block_idx);
            if range.len() <= 2 {
                continue;
            }

            let insts: Vec<MInst> = vcode.block_insts(block_idx).to_vec();

            // Control flow is represented as a full barrier in the DAG, so a
            // terminator remains last while its register/NZCV uses stay visible.
            let dag = DepGraph::build(&insts);
            let order = schedule(&dag);
            let original_order: Vec<_> = (0..dag.n).collect();
            let improves_or_matches = matches!(
                (
                    estimate_cycles(&dag, &order),
                    estimate_cycles(&dag, &original_order)
                ),
                (Some(scheduled), Some(original)) if scheduled <= original
            );

            if is_identity(&order, insts.len()) || !improves_or_matches {
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
/// `order` where `order[new_position] = old_index`.
fn schedule(dag: &DepGraph) -> Vec<usize> {
    let n = dag.n;
    if n == 0 {
        return vec![];
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
                    .find(|(s, _)| *s == node)
                    .map(|(_, l)| *l)
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
                for &(succ, _) in &dag.succs[node] {
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
            break;
        }
    }

    order
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
fn estimate_cycles(dag: &DepGraph, order: &[usize]) -> Option<u32> {
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
                    .find(|(succ, _)| *succ == node)
                    .map(|(_, latency)| *latency)
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

    Some(completion_cycle)
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
        let mut order = schedule(&graph);
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

        assert_eq!(estimate_cycles(&graph, &[0]), Some(11));
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
        let scheduled_order = schedule(&graph);
        let scheduled = estimate_cycles(&graph, &scheduled_order).unwrap();

        assert_eq!(original, 4);
        assert_eq!(scheduled, 3);
        assert!(scheduled < original);
    }
}
