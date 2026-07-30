//! Shared cycle simulator for Cortex-A53 scheduling.
//!
//! Both the list scheduler and the fixed-order estimator use this module so
//! that latency, resource, and issue-width rules are defined in exactly one
//! place. The scheduler asks "when can this node issue?" and the estimator
//! replays a fixed permutation through the same logic.

use crate::sched::aarch53::{SchedClass, instr_profile};
use crate::sched::dag::DepGraph;

/// Per-cycle resource state for the Cortex-A53 dual-issue pipeline.
///
/// Each cycle can issue at most 2 instructions, but structural hazards limit
/// which combinations are legal:
/// - At most 1 LSU operation (load or store).
/// - At most 1 MAC/Div operation.
/// - At most 1 FP/Other operation.
/// - A Barrier instruction must issue alone.
#[derive(Clone, Debug, Default)]
pub struct CycleSimulator {
    mul_div_available_at: u32,
}

impl CycleSimulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check whether `class` can issue at `cycle` given the resource state.
    pub fn is_ready(&self, class: SchedClass, cycle: u32) -> bool {
        !matches!(class, SchedClass::Mul | SchedClass::Div) || cycle >= self.mul_div_available_at
    }

    /// Reserve resources for an instruction issued at `cycle`.
    pub fn reserve(&mut self, class: SchedClass, cycle: u32) {
        if matches!(class, SchedClass::Mul | SchedClass::Div) {
            let occupancy = if class == SchedClass::Div {
                instr_profile(class).latency
            } else {
                1
            };
            self.mul_div_available_at = cycle + occupancy;
        }
    }

    /// Check whether `class` can issue alongside `already_issued` in the same cycle.
    pub fn can_issue(class: SchedClass, already_issued: &[SchedClass]) -> bool {
        if already_issued.len() >= 2 {
            return false;
        }
        if matches!(class, SchedClass::Barrier) || already_issued.contains(&SchedClass::Barrier) {
            return already_issued.is_empty();
        }

        let uses_lsu = |c| matches!(c, SchedClass::Load | SchedClass::Store);
        let uses_mac = |c| matches!(c, SchedClass::Mul | SchedClass::Div);
        let uses_fp = |c| matches!(c, SchedClass::Other);

        !(uses_lsu(class) && already_issued.iter().copied().any(uses_lsu)
            || uses_mac(class) && already_issued.iter().copied().any(uses_mac)
            || uses_fp(class) && already_issued.iter().copied().any(uses_fp))
    }

    /// Earliest cycle at which `node` can issue, given when its predecessors
    /// were issued and the edge latencies in `dag`.
    ///
    /// This is the single rule used by both the scheduler and the estimator.
    /// An edge with latency `L` from predecessor `P` to node `N` means `N`
    /// cannot issue before `issued[P] + L`.
    pub fn earliest_issue_cycle(
        dag: &DepGraph,
        node: usize,
        issued_at: &[Option<u32>],
    ) -> u32 {
        let mut earliest = 0u32;
        for &pred in &dag.preds[node] {
            let pred_issued = issued_at[pred].unwrap_or(0);
            let edge_latency = dag.succs[pred]
                .iter()
                .find(|e| e.node == node)
                .map(|e| e.latency)
                .unwrap_or(0);
            earliest = earliest.max(pred_issued + edge_latency);
        }
        earliest
    }

    /// Completion cycle for a single instruction issued at `issue_cycle`.
    pub fn completion_cycle(class: SchedClass, issue_cycle: u32) -> u32 {
        issue_cycle + instr_profile(class).latency.max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn barrier_must_issue_alone() {
        assert!(CycleSimulator::can_issue(SchedClass::Barrier, &[]));
        assert!(!CycleSimulator::can_issue(SchedClass::Barrier, &[SchedClass::Alu]));
        assert!(!CycleSimulator::can_issue(SchedClass::Alu, &[SchedClass::Barrier]));
    }

    #[test]
    fn dual_issue_respects_resource_limits() {
        assert!(CycleSimulator::can_issue(SchedClass::Alu, &[SchedClass::Alu]));
        assert!(!CycleSimulator::can_issue(
            SchedClass::Alu,
            &[SchedClass::Alu, SchedClass::Alu]
        ));
        assert!(!CycleSimulator::can_issue(SchedClass::Store, &[SchedClass::Load]));
        assert!(!CycleSimulator::can_issue(SchedClass::Mul, &[SchedClass::Mul]));
        assert!(!CycleSimulator::can_issue(SchedClass::Other, &[SchedClass::Other]));
    }

    #[test]
    fn completion_includes_latency() {
        assert_eq!(CycleSimulator::completion_cycle(SchedClass::Div, 0), 11);
        assert_eq!(CycleSimulator::completion_cycle(SchedClass::Alu, 5), 6);
        assert_eq!(CycleSimulator::completion_cycle(SchedClass::Nop, 3), 4);
    }
}
