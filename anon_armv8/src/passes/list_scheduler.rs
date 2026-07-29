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

            if is_identity(&order, insts.len()) {
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
    let mut mul_div_available_at: u32 = 0;

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
            if data_ready
                && resource_ready(class, cycle, mul_div_available_at)
                && can_issue(class, &issued_classes)
            {
                issued_at[node] = Some(cycle);
                order.push(node);
                issued_classes.push(class);
                issued_this_cycle = true;

                if matches!(class, SchedClass::Mul | SchedClass::Div) {
                    let occupancy = if class == SchedClass::Div {
                        instr_profile(class).latency
                    } else {
                        1
                    };
                    mul_div_available_at = cycle + occupancy;
                }

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

fn resource_ready(class: SchedClass, cycle: u32, mul_div_available_at: u32) -> bool {
    !matches!(class, SchedClass::Mul | SchedClass::Div) || cycle >= mul_div_available_at
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
        assert!(!resource_ready(SchedClass::Mul, 10, 11));
        assert!(!resource_ready(SchedClass::Div, 10, 11));
        assert!(resource_ready(SchedClass::Div, 11, 11));
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
}
