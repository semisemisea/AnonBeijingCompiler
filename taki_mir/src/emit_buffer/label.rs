//! Branch labels, slot, and veneer data types for the emission buffer.

use crate::block_order::MirBlockIndex;

/// Signed branch reach in bytes, matching the ISA encoding of each form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LabelKind {
    /// Maximum positive offset (bytes).
    pub positive: u32,
    /// Maximum negative offset (bytes).
    pub negative: u32,
}

impl LabelKind {
    /// AArch64 `tbz`/`tbnz`: 14-bit signed offset scaled by 4.
    pub const BRANCH14: LabelKind = LabelKind {
        positive: (1 << 13) * 4,
        negative: (1 << 13) * 4,
    };
    /// AArch64 `b.cond`/`cbz`/`cbnz`: 19-bit signed offset scaled by 4.
    pub const BRANCH19: LabelKind = LabelKind {
        positive: (1 << 18) * 4,
        negative: (1 << 18) * 4,
    };
    /// AArch64 `b`/`bl`: 26-bit signed offset scaled by 4.
    pub const BRANCH26: LabelKind = LabelKind {
        positive: (1 << 25) * 4,
        negative: (1 << 25) * 4,
    };
    /// RISC-V B-type branches: 13-bit signed offset scaled by 2.
    pub const RV_B: LabelKind = LabelKind {
        positive: (1 << 12) * 2,
        negative: (1 << 12) * 2,
    };
    /// RISC-V `jal`: 21-bit signed offset scaled by 2.
    pub const RV_JAL: LabelKind = LabelKind {
        positive: (1 << 20) * 2,
        negative: (1 << 20) * 2,
    };

    /// Whether an offset of `to - from` bytes is reachable.
    pub fn in_range(self, from: usize, to: usize) -> bool {
        let distance = to as i64 - from as i64;
        -(self.negative as i64) <= distance && distance <= self.positive as i64
    }
}

/// Number of 4-byte instructions a slot occupies.
pub(crate) fn slot_len(slot: &Slot) -> usize {
    match slot {
        Slot::Text(_) | Slot::Branch(_) => 1,
        Slot::Veneer(veneer) => veneer.lines.len().max(1),
    }
}

/// One emitted instruction slot.
#[derive(Clone, Debug)]
pub enum Slot {
    /// An ordinary instruction's text, rendered verbatim on one line.
    Text(String),
    /// An optimizable branch: mnemonic + operands before the target label.
    Branch(BranchRef),
    /// A long-branch veneer inserted by `resolve` when a branch falls out of
    /// range: a labeled sequence of instruction lines jumping to the target.
    Veneer(VeneerRec),
}

/// A labeled veneer sequence.
#[derive(Clone, Debug)]
pub struct VeneerRec {
    /// Unique name of the veneer label.
    pub name: String,
    /// Unique name of the label bound right after the veneer's lines; the
    /// inverted conditional branch targets it so the false path skips the
    /// veneer entirely.
    pub end_name: String,
    /// One instruction per line, each rendered `    `-indented.
    pub lines: Vec<String>,
}

/// A branch target: an intra-function block or a veneer label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LabelRef {
    Block(MirBlockIndex),
    /// Start label of a veneer (target of an unconditional long branch).
    Veneer(u32),
    /// Label after a veneer's lines (target of an inverted conditional long
    /// branch: the false path skips the veneer and continues after it).
    VeneerEnd(u32),
}

impl LabelRef {
    /// The block target, when this reference names a block.
    pub fn block(self) -> Option<MirBlockIndex> {
        match self {
            LabelRef::Block(block) => Some(block),
            LabelRef::Veneer(_) | LabelRef::VeneerEnd(_) => None,
        }
    }
}

/// Structured form of an optimizable branch.
#[derive(Clone, Debug)]
pub struct BranchRef {
    /// Mnemonic and operands before the target label, e.g. `"b.eq "`.
    pub prefix: String,
    /// Inverted-encoding text, e.g. `"b.ne "` for `"b.eq "`. `None` for
    /// unconditional branches.
    pub inv_prefix: Option<String>,
    /// Target label.
    pub target: LabelRef,
    /// Branch reach.
    pub kind: LabelKind,
}

/// Bookkeeping record for a branch at the tail of the buffer.
///
/// `labels_at_this_branch` is the complete list of labels bound at the
/// branch's start offset; it is cloned from `labels_at_tail` when the branch
/// is emitted (matching MachBuffer's `add_*_branch`). Branch properties
/// (target, kind, inverted encoding) are read from `slots[start]` so that a
/// single slot edit stays authoritative.
pub(crate) struct BranchRec {
    pub start: usize,
    pub labels_at_this_branch: Vec<MirBlockIndex>,
}
