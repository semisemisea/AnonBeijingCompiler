/*
 * Adapted from regalloc2 0.15.1 src/ion/data_structures.rs.
 *
 * Released under the Apache License 2.0 with LLVM Exception. See the
 * repository LICENSE for the full license text. This local subset contains
 * only the data needed by Ion's analysis and coalescing prerequisites.
 */

use crate::reg_alloc::{
    index::Block,
    reg::{Operand, ProgPoint, VReg},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeRange {
    pub from: ProgPoint,
    pub to: ProgPoint,
}
impl CodeRange {
    pub fn contains_point(self, point: ProgPoint) -> bool {
        self.from <= point && point < self.to
    }
    pub fn overlaps(self, other: Self) -> bool {
        self.to > other.from && self.from < other.to
    }
    pub fn join(self, other: Self) -> Self {
        Self {
            from: self.from.min(other.from),
            to: self.to.max(other.to),
        }
    }
    pub fn singleton(point: ProgPoint) -> Self {
        Self {
            from: point,
            to: point.next(),
        }
    }
}
impl PartialOrd for CodeRange {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for CodeRange {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        if self.to <= other.from {
            core::cmp::Ordering::Less
        } else if self.from >= other.to {
            core::cmp::Ordering::Greater
        } else {
            core::cmp::Ordering::Equal
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Use {
    pub operand: Operand,
    pub pos: ProgPoint,
    pub slot: u16,
    pub weight: u16,
}

#[derive(Clone, Debug)]
pub struct LiveRange {
    pub range: CodeRange,
    pub vreg: VReg,
    pub uses: Vec<Use>,
    pub starts_at_def: bool,
}

#[derive(Clone, Debug)]
pub struct LiveBundle {
    pub vregs: Vec<VReg>,
    pub ranges: Vec<LiveRange>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlockParamOut {
    pub from_block: Block,
    pub to_block: Block,
    pub from_vreg: VReg,
    pub to_vreg: VReg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlockParamIn {
    pub from_block: Block,
    pub to_block: Block,
    pub to_vreg: VReg,
}
