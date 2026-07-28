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
    vcode::{MachInst, MachTerminator, VCodeContainer},
};

use crate::instructions::MInst;
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

            // Never schedule the terminator — it must remain last.
            let term_offset = find_terminator_offset(&insts);
            let sched_range = match term_offset {
                Some(t) if t > 1 => 0..t,
                _ => continue,
            };

            let sched_insts = &insts[sched_range.clone()];
            let dag = DepGraph::build(sched_insts);
            let order = schedule(&dag);

            if is_identity(&order, sched_range.len()) {
                continue;
            }

            // Write the scheduled instructions back.
            for (local, orig) in order.iter().enumerate() {
                let global = range.start + local;
                *vcode.inst_mut(global) = insts[sched_range.start + orig].clone();
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

/// Find the index of the block-terminating instruction (branch/return),
/// if any. Returns None if the block has no terminator.
fn find_terminator_offset(insts: &[MInst]) -> Option<usize> {
    insts.iter().rposition(|inst| {
        !matches!(inst.is_term(), MachTerminator::None)
    })
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

    while order.len() < n {
        let mut issued_this_cycle = false;

        // Sort ready nodes by critical path (descending) — highest priority first.
        ready.sort_by(|&a, &b| dag.crit[b].cmp(&dag.crit[a]));

        // Try to issue ready nodes whose data dependencies are satisfied.
        let mut next_ready = Vec::new();
        for &node in &ready {
            let data_ready = dag.preds[node].iter().all(|&pred| {
                let issued = issued_at[pred].unwrap_or(0);
                let latency = dag
                    .succs[pred]
                    .iter()
                    .find(|(s, _)| *s == node)
                    .map(|(_, l)| *l)
                    .unwrap_or(0);
                issued + latency <= cycle
            });

            if data_ready {
                issued_at[node] = Some(cycle);
                order.push(node);
                issued_this_cycle = true;

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
        stall_budget -= 1;
        if stall_budget == 0 && !issued_this_cycle {
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

/// Check if a permutation is the identity (0, 1, 2, ..., n-1).
fn is_identity(order: &[usize], n: usize) -> bool {
    order.len() == n && order.iter().enumerate().all(|(i, &v)| i == v)
}
