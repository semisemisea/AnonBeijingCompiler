/*
 * Adapted from regalloc2 0.15.1 src/ion/merge.rs.
 *
 * Released under the Apache License 2.0 with LLVM Exception. See the
 * repository LICENSE for the full license text. This only establishes
 * coalescing bundles; allocation and splitting remain intentionally absent.
 */

use super::{LiveBundle, LiveRange, Liveness, Requirement, RequirementConflict};
use crate::reg_alloc::{
    function::Function,
    reg::{OperandConstraint, VReg},
};

#[derive(Clone, Debug)]
pub struct BundleSet {
    pub bundles: Vec<LiveBundle>,
    pub bundle_for_vreg: Vec<usize>,
}
impl BundleSet {
    pub fn bundle(&self, vreg: VReg) -> usize {
        self.bundle_for_vreg[vreg.vreg()]
    }
    pub fn merge(&mut self, from: usize, to: usize) -> bool {
        if from == to {
            return true;
        }
        let (source, target) = two_mut(&mut self.bundles, from, to);
        if source
            .vregs
            .first()
            .zip(target.vregs.first())
            .is_some_and(|(a, b)| a.class() != b.class())
            || !can_merge(source, target)
        {
            return false;
        }
        let source_vregs = core::mem::take(&mut source.vregs);
        let source_ranges = core::mem::take(&mut source.ranges);
        for vreg in source_vregs {
            self.bundle_for_vreg[vreg.vreg()] = to;
            target.vregs.push(vreg);
        }
        target.ranges.extend(source_ranges);
        target.ranges.sort_unstable_by_key(|range| range.range.from);
        true
    }
}

pub fn merge_vreg_bundles<F: Function>(
    function: &F,
    ranges: &[Vec<LiveRange>],
    liveness: &Liveness,
) -> BundleSet {
    let mut bundles = Vec::new();
    let mut bundle_for_vreg = vec![usize::MAX; function.num_vregs()];
    for (index, ranges) in ranges.iter().enumerate() {
        if ranges.is_empty() {
            continue;
        }
        bundle_for_vreg[index] = bundles.len();
        bundles.push(LiveBundle {
            vregs: vec![ranges[0].vreg],
            ranges: ranges.clone(),
        });
    }
    let mut set = BundleSet {
        bundles,
        bundle_for_vreg,
    };
    for inst in 0..function.num_insts() {
        let operands = function.inst_operands(crate::reg_alloc::index::Inst::new(inst));
        for operand in operands {
            if let OperandConstraint::Reuse(input) = operand.constraint() {
                set.merge(
                    set.bundle(operands[input].vreg()),
                    set.bundle(operand.vreg()),
                );
            }
        }
    }
    for edge in &liveness.blockparam_outs {
        if edge.from_vreg.vreg() < set.bundle_for_vreg.len()
            && edge.to_vreg.vreg() < set.bundle_for_vreg.len()
        {
            set.merge(set.bundle(edge.from_vreg), set.bundle(edge.to_vreg));
        }
    }
    set
}

fn can_merge(source: &LiveBundle, target: &LiveBundle) -> bool {
    if source
        .ranges
        .iter()
        .any(|a| target.ranges.iter().any(|b| a.range.overlaps(b.range)))
    {
        return false;
    }
    requirement(source)
        .and_then(|a| requirement(target).and_then(|b| a.merge(b)))
        .is_ok()
}
fn requirement(bundle: &LiveBundle) -> Result<Requirement, RequirementConflict> {
    bundle.ranges.iter().flat_map(|range| &range.uses).try_fold(
        Requirement::Any,
        |requirement, use_| {
            requirement.merge(Requirement::from_constraint(
                use_.operand.constraint(),
                |_| false,
            ))
        },
    )
}
fn two_mut<T>(items: &mut [T], a: usize, b: usize) -> (&mut T, &mut T) {
    assert_ne!(a, b);
    if a < b {
        let (left, right) = items.split_at_mut(b);
        (&mut left[a], &mut right[0])
    } else {
        let (left, right) = items.split_at_mut(a);
        (&mut right[0], &mut left[b])
    }
}
