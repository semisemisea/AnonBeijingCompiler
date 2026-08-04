//! Text-level machine-code emission buffer, modeled on Cranelift's MachBuffer.
//!
//! Every slot corresponds to exactly one fixed-width (4-byte) instruction.
//! Instructions are accumulated as text templates, so branch optimization can
//! truncate, invert, or retarget a branch in O(1) without patching bytes.
//!
//! Emission flows through `EmitContext`: ordinary instructions accumulate
//! text into a pending buffer until `end_inst()` flushes them as one `Text`
//! slot; branches are emitted directly as structured `Branch` slots whose
//! target stays symbolic (a `MirBlockIndex`) so that label resolution happens
//! at `finish()` time.
//!
//! Milestone M25 introduces the buffer and moves every backend through it
//! without enabling any branch optimization. The `optimize_branches` and
//! `resolve` (range-check + veneer) machinery is wired up in later milestones.

use core::fmt::Write as _;
use std::marker::PhantomData;

use crate::block_order::MirBlockIndex;
use crate::lower::LowerBackend;
use crate::prelude::*;
use crate::register::Reg;
use crate::stats::BranchOptStats;
use crate::vcode::EmitContext;

/// Bound on a single branch's `labels_at_this_branch` list; beyond it,
/// simplification is skipped so that long `goto next; next:` chains cannot
/// produce quadratic alias coalescing (Cranelift #3468).
const LABEL_LIST_THRESHOLD: usize = 100;

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
fn slot_len(slot: &Slot) -> usize {
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

/// A text-level instruction buffer for one function.
pub struct EmitBuffer<'a, B: LowerBackend> {
    slots: Vec<Slot>,
    /// Text accumulated since the last `end_inst` / `take_inst_text`.
    pending: String,
    /// Symbol name of every lowered block, in lowered order.
    block_labels: Vec<String>,
    program: &'a HirProgram,
    /// Slot index at which each block label is bound; `usize::MAX` = unbound.
    label_offsets: Vec<usize>,
    /// Optional alias target for labels rewritten by branch optimization.
    label_aliases: Vec<Option<MirBlockIndex>>,
    /// Tail-adjacent branches, in emission order, with their slot positions.
    latest_branches: Vec<BranchRec>,
    /// Labels bound at the current tail offset.
    labels_at_tail: Vec<MirBlockIndex>,
    /// Slot offset the `labels_at_tail` list is valid for.
    labels_at_tail_off: usize,
    /// Whether branch simplification rules are enabled (`-O1` and up).
    enable_branch_opt: bool,
    /// Emission-time branch optimization counters.
    stats: BranchOptStats,
    /// Symbol names of veneers inserted by `resolve`, by veneer id.
    veneer_names: Vec<String>,
    /// Symbol names of the labels bound right after each veneer's lines.
    veneer_end_names: Vec<String>,
    /// Prefix used for synthesized veneer symbols (`.L{func}_veneer_{id}`).
    func_name: String,
    _phantom: PhantomData<B>,
}

impl<'a, B: LowerBackend> EmitBuffer<'a, B> {
    pub fn new(
        program: &'a HirProgram,
        func_name: String,
        block_labels: Vec<String>,
        enable_branch_opt: bool,
    ) -> Self {
        let count = block_labels.len();
        EmitBuffer {
            slots: Vec::new(),
            pending: String::new(),
            block_labels,
            program,
            label_offsets: vec![usize::MAX; count],
            label_aliases: vec![None; count],
            latest_branches: Vec::new(),
            labels_at_tail: Vec::new(),
            labels_at_tail_off: 0,
            enable_branch_opt,
            stats: BranchOptStats::default(),
            veneer_names: Vec::new(),
            veneer_end_names: Vec::new(),
            func_name,
            _phantom: PhantomData,
        }
    }

    /// Branch optimization counters accumulated during emission.
    pub fn branch_stats(&self) -> BranchOptStats {
        BranchOptStats {
            ran: self.enable_branch_opt,
            ..self.stats.clone()
        }
    }

    /// Current slot index (the next instruction's position).
    fn cur_slot(&self) -> usize {
        self.slots.len()
    }

    /// Bind a block label to the current position. A label can be bound once.
    pub fn bind_label(&mut self, block: MirBlockIndex) {
        let index = block.index();
        debug_assert_eq!(
            self.label_offsets[index],
            usize::MAX,
            "block label bound twice"
        );
        self.label_offsets[index] = self.slots.len();
        self.lazily_clear_labels_at_tail();
        self.labels_at_tail.push(block);
        self.optimize_branches();
    }

    /// Resolve a label through the alias chain, guarding against cycles.
    fn resolved(&self, mut label: MirBlockIndex) -> MirBlockIndex {
        for _ in 0..=self.label_aliases.len() {
            match self.label_aliases[label.index()] {
                Some(next) => label = next,
                None => return label,
            }
        }
        panic!("label alias cycle detected");
    }

    /// Symbol name of a label after alias resolution.
    fn label_name(&self, label: LabelRef) -> &str {
        match label {
            LabelRef::Block(block) => {
                let block = self.resolved(block);
                &self.block_labels[block.index()]
            }
            LabelRef::Veneer(id) => &self.veneer_names[id as usize],
            LabelRef::VeneerEnd(id) => &self.veneer_end_names[id as usize],
        }
    }

    /// Lazily clear `labels_at_tail` if the tail has moved past the offset the
    /// list applies to.
    fn lazily_clear_labels_at_tail(&mut self) {
        let offset = self.slots.len();
        if offset > self.labels_at_tail_off {
            self.labels_at_tail_off = offset;
            self.labels_at_tail.clear();
        }
    }

    /// Branch simplification rules (fallthrough elimination, label threading,
    /// dead-jump removal, condition inversion), ported from Cranelift's
    /// `MachBuffer::optimize_branches`. Only operates on tail-adjacent
    /// branches, so truncation never disturbs the block instruction order.
    ///
    /// Called from `bind_label` and once more at function end by the emitter.
    pub fn optimize_branches(&mut self) {
        if !self.enable_branch_opt {
            self.lazily_clear_labels_at_tail();
            return;
        }
        // R1: branch whose target resolves to the current tail is a no-op;
        // truncate it. R2: a tail unconditional branch's start labels are
        // aliased to its target. R3: an unreachable uncond after another
        // uncond is removed. R4: a cond branch followed by an uncond whose
        // cond target is the tail is inverted to skip the uncond.
        while let Some(b) = self.latest_branches.last() {
            let cur_off = self.slots.len();
            if b.start + 1 < cur_off {
                // Code was emitted after this branch; it is no longer at the
                // tail and cannot be edited.
                break;
            }
            if b.labels_at_this_branch.len() > LABEL_LIST_THRESHOLD {
                break;
            }

            let Slot::Branch(branch) = &self.slots[b.start] else {
                unreachable!("latest_branches entry must reference a branch slot");
            };
            let LabelRef::Block(target) = branch.target else {
                unreachable!("optimizable branches only target blocks");
            };
            let is_uncond = branch.inv_prefix.is_none();

            if self.resolve_label_offset(target) == cur_off {
                self.stats.changed = true;
                self.stats.fallthrough_removed += 1;
                self.truncate_last_branch();
                continue;
            }

            if is_uncond {
                let start = b.start;
                // Redirect labels bound at this branch's start to its target,
                // unless that would create an alias cycle (target resolving
                // back to this branch's start).
                if self.resolve_label_offset(target) != start {
                    let redirected = b.labels_at_this_branch.len();
                    for &label in &b.labels_at_this_branch {
                        self.label_aliases[label.index()] = Some(target);
                    }
                    self.latest_branches
                        .last_mut()
                        .unwrap()
                        .labels_at_this_branch
                        .clear();
                    if redirected > 0 {
                        self.stats.changed = true;
                        self.stats.labels_threaded += redirected as u64;
                        continue;
                    }
                } else {
                    break;
                }

                if self.latest_branches.len() > 1 {
                    let prev_start = self.latest_branches[self.latest_branches.len() - 2].start;
                    let Slot::Branch(prev_branch) = &self.slots[prev_start] else {
                        unreachable!("latest_branches entry must reference a branch slot");
                    };
                    let prev_is_uncond = prev_branch.inv_prefix.is_none();
                    let prev_is_cond = !prev_is_uncond;
                    let prev_adjacent = prev_start + 1 == start;

                    if prev_is_uncond
                        && prev_adjacent
                        && self
                            .latest_branches
                            .last()
                            .unwrap()
                            .labels_at_this_branch
                            .is_empty()
                    {
                        self.stats.changed = true;
                        self.stats.dead_jumps_removed += 1;
                        self.truncate_last_branch();
                        continue;
                    }

                    if prev_is_cond
                        && prev_adjacent
                        && self.resolve_label_offset(prev_branch.target.block().unwrap()) == cur_off
                    {
                        let target = branch.target;
                        self.stats.changed = true;
                        self.stats.branches_inverted += 1;
                        self.truncate_last_branch();
                        // Swap prefix and inverted prefix, retarget the cond
                        // branch to the removed uncond's target.
                        let Slot::Branch(prev_slot) = &mut self.slots[prev_start] else {
                            unreachable!("latest_branches entry must reference a branch slot");
                        };
                        let inverted = prev_slot.inv_prefix.take().unwrap();
                        let original = std::mem::replace(&mut prev_slot.prefix, inverted);
                        prev_slot.inv_prefix = Some(original);
                        prev_slot.target = target;
                        continue;
                    }
                }
            }
            break;
        }
        self.purge_latest_branches();
    }

    /// Remove the last branch and everything after its start. Labels bound at
    /// the (former) tail move back to the branch's start offset; labels bound
    /// at the branch's start become the new tail labels.
    fn truncate_last_branch(&mut self) {
        self.lazily_clear_labels_at_tail();
        let b = self.latest_branches.pop().unwrap();
        debug_assert_eq!(b.start + 1, self.slots.len());
        self.slots.truncate(b.start);

        let cur_off = self.slots.len();
        self.labels_at_tail_off = cur_off;
        for &label in &self.labels_at_tail {
            self.label_offsets[label.index()] = cur_off;
        }
        self.labels_at_tail.extend(b.labels_at_this_branch);
    }

    /// Drop all branch records once the tail has moved past the last one.
    fn purge_latest_branches(&mut self) {
        let cur_off = self.slots.len();
        if let Some(b) = self.latest_branches.last() {
            if b.start + 1 < cur_off {
                self.latest_branches.clear();
            }
        }
    }

    /// Resolve a label to its slot offset, or `usize::MAX` when not yet bound.
    fn resolve_label_offset(&self, label: MirBlockIndex) -> usize {
        self.label_offsets[self.resolved(label).index()]
    }

    /// Range check every branch against its `LabelKind` and insert veneers
    /// where needed.
    ///
    /// Every slot is a fixed 4-byte instruction, so byte offsets are exact.
    /// Out-of-range branches are rewritten to jump to a veneer spliced
    /// immediately after the branch (block terminators have no fallthrough
    /// into the veneer); the veneer reaches the original target. Offsets are
    /// then recomputed and the pass repeats until stable; insertion only
    /// grows offsets, so the relaxation is monotone and converges in a
    /// couple of rounds.
    pub fn resolve(&mut self) {
        const MAX_ROUNDS: usize = 8;
        let mut veneer_id = 0u32;
        for _ in 0..MAX_ROUNDS {
            // Byte offset of each slot boundary (including trailing end).
            let mut byte_offsets = Vec::with_capacity(self.slots.len() + 1);
            let mut bytes = 0usize;
            byte_offsets.push(0);
            for slot in &self.slots {
                bytes += slot_len(slot) * 4;
                byte_offsets.push(bytes);
            }

            // Collect every out-of-range branch against this round's snapshot
            // first: inserting a veneer shifts subsequent slots, which would
            // otherwise corrupt the offset table mid-round.
            let mut to_veneer: Vec<usize> = Vec::new();
            for (i, slot) in self.slots.iter().enumerate() {
                let Slot::Branch(branch) = slot else {
                    continue;
                };
                let LabelRef::Block(target) = branch.target else {
                    continue;
                };
                let target_off = byte_offsets[self.label_offsets[target.index()]];
                if !branch.kind.in_range(byte_offsets[i], target_off) {
                    to_veneer.push(i);
                }
            }
            if to_veneer.is_empty() {
                break;
            }
            // Insert from the end so earlier insertions never invalidate the
            // slot index of a later insertion.
            for &i in to_veneer.iter().rev() {
                self.insert_veneer(i, veneer_id);
                veneer_id += 1;
            }
        }

        // Invariant: every emitted branch is inside its `LabelKind` reach.
        let mut byte_offsets = Vec::with_capacity(self.slots.len() + 1);
        let mut bytes = 0usize;
        byte_offsets.push(0);
        for slot in &self.slots {
            bytes += slot_len(slot) * 4;
            byte_offsets.push(bytes);
        }
        for (i, slot) in self.slots.iter().enumerate() {
            if let Slot::Branch(branch) = slot {
                let LabelRef::Block(target) = branch.target else {
                    continue;
                };
                let target_off = byte_offsets[self.label_offsets[target.index()]];
                assert!(
                    branch.kind.in_range(byte_offsets[i], target_off),
                    "branch {i} ({}{}) out of {branch:?} range after veneer relaxation",
                    branch.prefix,
                    self.label_name(branch.target)
                );
            }
        }
    }

    /// Rewrite the branch at slot `i` to target a fresh veneer spliced right
    /// after it, which reaches the original target. Conditional branches are
    /// inverted so the false path skips the veneer (targeting the veneer's
    /// end label); unconditional branches target the veneer's start label.
    /// Also shifts every label bound after the insertion point by one slot.
    fn insert_veneer(&mut self, i: usize, veneer_id: u32) {
        let Slot::Branch(branch) = &self.slots[i] else {
            unreachable!("veneer insertion requires a branch slot");
        };
        let LabelRef::Block(target) = branch.target else {
            unreachable!("veneer insertion requires a block target");
        };
        let name = format!(".L_{}_veneer_{}", self.func_name, veneer_id);
        let end_name = format!("{name}_end");
        let target_name = self.label_name(LabelRef::Block(target));
        let (new_prefix, new_inv, new_target) = if branch.inv_prefix.is_some() {
            // `b.<cond> T` out of range -> `b.<!cond> V_end; V: b T; V_end:`.
            let inv = branch.inv_prefix.as_ref().unwrap().clone();
            (inv, None, LabelRef::VeneerEnd(veneer_id))
        } else {
            // Unconditional jump out of range -> `j V; V: <veneer lines>`.
            // Reuse the branch's own prefix so the veneer path works for any
            // backend's unconditional jump mnemonic (`b` / `j`).
            (branch.prefix.clone(), None, LabelRef::Veneer(veneer_id))
        };
        let lines = B::veneer_lines(branch.kind, target_name);
        self.veneer_names.push(name.clone());
        self.veneer_end_names.push(end_name.clone());
        self.slots[i] = Slot::Branch(BranchRef {
            prefix: new_prefix,
            inv_prefix: new_inv,
            target: new_target,
            kind: branch.kind,
        });
        self.slots.insert(
            i + 1,
            Slot::Veneer(VeneerRec {
                name,
                end_name,
                lines,
            }),
        );
        // Labels bound after the branch shift right by the inserted slot.
        for offset in self.label_offsets.iter_mut() {
            if *offset != usize::MAX && *offset > i {
                *offset += 1;
            }
        }
        self.stats.veneers_inserted += 1;
    }

    /// Render the buffer: block labels, then one `    `-indented line per
    /// slot, with branch targets resolved through the alias chain.
    ///
    /// Label definitions render at their recorded slot offset. Offsets are
    /// not necessarily monotonic in block order (branch truncation moves
    /// labels backward), so label events are sorted by (offset, block index)
    /// before interleaving with the slot stream.
    pub fn finish(self) -> String {
        let mut label_events: Vec<(usize, usize)> = self
            .label_offsets
            .iter()
            .enumerate()
            .filter(|&(_, offset)| *offset != usize::MAX)
            .map(|(index, &offset)| (offset, index))
            .collect();
        label_events.sort_unstable();

        let mut out = String::new();
        let mut label_cursor = 0;
        let mut emit_labels = |out: &mut String, at: usize, label_cursor: &mut usize| {
            while *label_cursor < label_events.len() && label_events[*label_cursor].0 == at {
                out.push_str(&self.block_labels[label_events[*label_cursor].1]);
                out.push_str(":\n");
                *label_cursor += 1;
            }
        };
        for (index, slot) in self.slots.iter().enumerate() {
            emit_labels(&mut out, index, &mut label_cursor);
            match slot {
                Slot::Text(text) => {
                    out.push_str("    ");
                    out.push_str(text);
                    out.push('\n');
                }
                Slot::Branch(branch) => {
                    out.push_str("    ");
                    out.push_str(&branch.prefix);
                    out.push_str(self.label_name(branch.target));
                    out.push('\n');
                }
                Slot::Veneer(veneer) => {
                    out.push_str(&veneer.name);
                    out.push_str(":\n");
                    for line in &veneer.lines {
                        out.push_str("    ");
                        out.push_str(line);
                        out.push('\n');
                    }
                    out.push_str(&veneer.end_name);
                    out.push_str(":\n");
                }
            }
        }
        emit_labels(&mut out, self.slots.len(), &mut label_cursor);
        out
    }
}

impl<B: LowerBackend> core::fmt::Write for EmitBuffer<'_, B> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        self.pending.push_str(text);
        Ok(())
    }
}

impl<B: LowerBackend> EmitContext for EmitBuffer<'_, B> {
    fn write_reg(&mut self, reg: &Reg) -> core::fmt::Result {
        if let Some(preg) = reg.to_real_reg() {
            write!(self, "{}", B::preg_name(preg))
        } else {
            panic!(
                "write_reg called on non-physical register {reg:?}; \
                 spill slots must be handled by regalloc Edit system \
                 (output.edits) before emission"
            )
        }
    }

    fn write_label_ref(&mut self, idx: MirBlockIndex) -> core::fmt::Result {
        let label = self.label_name(LabelRef::Block(idx)).to_owned();
        write!(self, "{label}")
    }

    fn write_function_label(&mut self, func: HirFunction) -> core::fmt::Result {
        let name = self.program.func_data(func).name();
        write!(self, "{name}")
    }

    fn write_global_label(&mut self, gv: HirInst) -> core::fmt::Result {
        if let Some(name) = self.program.inst_data(gv).name() {
            write!(self, "{name}")
        } else {
            write!(self, "<gv>")
        }
    }

    fn write_external_symbol(&mut self, symbol: &str) -> core::fmt::Result {
        write!(self, "{symbol}")
    }

    fn end_inst(&mut self) -> core::fmt::Result {
        if !self.pending.is_empty() {
            let text = std::mem::take(&mut self.pending);
            self.slots.push(Slot::Text(text));
        }
        Ok(())
    }

    fn take_inst_text(&mut self) -> String {
        std::mem::take(&mut self.pending)
    }

    fn put_branch(
        &mut self,
        prefix: &str,
        inv_prefix: Option<&str>,
        target: MirBlockIndex,
        kind: LabelKind,
    ) -> core::fmt::Result {
        debug_assert!(
            self.pending.is_empty(),
            "put_branch called with unflushed text: {:?}",
            self.pending
        );
        self.lazily_clear_labels_at_tail();
        self.latest_branches.push(BranchRec {
            start: self.slots.len(),
            labels_at_this_branch: self.labels_at_tail.clone(),
        });
        self.slots.push(Slot::Branch(BranchRef {
            prefix: prefix.to_owned(),
            inv_prefix: inv_prefix.map(str::to_owned),
            target: LabelRef::Block(target),
            kind,
        }));
        Ok(())
    }

    fn put_uncond_branch(
        &mut self,
        prefix: &str,
        target: MirBlockIndex,
        kind: LabelKind,
    ) -> core::fmt::Result {
        self.put_branch(prefix, None, target, kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        lower::LowerContext,
        reg_alloc::reg::{PReg, PRegSet, RegClass},
        types::LoweredType,
        vcode::{MachInst, MachInstEmit, MachTerminator},
    };
    use raana_ir::ir::Program;

    #[derive(Clone, Debug)]
    struct TestInst;

    struct TestAbi;

    impl crate::abi::ABIMachineSpec for TestAbi {
        type I = TestInst;

        fn stack_align() -> u32 {
            16
        }

        fn spillslot_size(_regclass: RegClass) -> u32 {
            1
        }

        fn spill_unit_bytes() -> u32 {
            8
        }

        fn is_callee_saved(_preg: PReg) -> bool {
            false
        }

        fn gen_load_stack(
            _mem: crate::abi::StackAMode,
            dst: crate::register::Writable<Reg>,
            _ty: LoweredType,
        ) -> Self::I {
            let _ = dst;
            TestInst
        }

        fn gen_load_imm(
            dst: crate::register::Writable<Reg>,
            _value: u64,
            _ty: LoweredType,
        ) -> Self::I {
            let _ = dst;
            TestInst
        }

        fn gen_load_addr(dst: crate::register::Writable<Reg>, _label: HirInst) -> Self::I {
            let _ = dst;
            TestInst
        }

        fn gen_get_stack_addr(
            _mem: crate::abi::StackAMode,
            dst: crate::register::Writable<Reg>,
        ) -> Self::I {
            let _ = dst;
            TestInst
        }

        fn gen_args(_args: Vec<crate::abi::ArgPair>) -> Self::I {
            TestInst
        }

        fn gen_store_stack(_src: Reg, _mem: crate::abi::StackAMode, _ty: LoweredType) -> Self::I {
            TestInst
        }

        fn gen_move(src: Reg, dst: Reg, _ty: LoweredType) -> Self::I {
            let _ = (src, dst);
            TestInst
        }

        fn compute_arg_loc(_arena: ArenaContext<'_>) -> (Vec<crate::abi::ArgSlot>, u32) {
            (vec![], 0)
        }

        fn compute_call_arg_loc(_types: &[HirType]) -> (Vec<crate::abi::ArgSlot>, u32) {
            (vec![], 0)
        }

        fn get_machine_env() -> &'static crate::reg_alloc::reg::MachineEnv {
            static ENV: std::sync::LazyLock<crate::reg_alloc::reg::MachineEnv> =
                std::sync::LazyLock::new(|| crate::reg_alloc::reg::MachineEnv {
                    preferred_regs_by_class: [
                        PRegSet::empty().with(PReg::new(0, RegClass::Int)),
                        PRegSet::empty().with(PReg::new(0, RegClass::Float)),
                        PRegSet::empty(),
                    ],
                    non_preferred_regs_by_class: [PRegSet::empty(); 3],
                    scratch_by_class: [None; 3],
                    post_ra_scratch_by_class: [vec![], vec![], vec![]],
                    fixed_stack_slots: vec![],
                });
            &ENV
        }

        fn gen_prologue_frame_setup(
            _frame: &crate::abi::FrameLayout,
        ) -> smallvec::SmallVec<[Self::I; 16]> {
            smallvec::SmallVec::new()
        }

        fn gen_epilogue_frame_restore(
            _frame: &crate::abi::FrameLayout,
        ) -> smallvec::SmallVec<[Self::I; 16]> {
            smallvec::SmallVec::new()
        }

        fn gen_clobber_save(_frame: &crate::abi::FrameLayout) -> smallvec::SmallVec<[Self::I; 16]> {
            smallvec::SmallVec::new()
        }

        fn gen_clobber_restore(
            _frame: &crate::abi::FrameLayout,
        ) -> smallvec::SmallVec<[Self::I; 16]> {
            smallvec::SmallVec::new()
        }
    }

    impl MachInst for TestInst {
        type ABISpec = TestAbi;

        fn get_operands(&mut self, _collector: &mut impl crate::reg_alloc::reg::OperandVisitor) {}

        fn is_move(&self) -> Option<(crate::register::Writable<Reg>, Reg)> {
            None
        }

        fn is_term(&self) -> MachTerminator {
            MachTerminator::None
        }

        fn rc_for_type(_ty: LoweredType) -> (&'static [RegClass], &'static [LoweredType]) {
            (&[], &[])
        }

        fn gen_jump(_target: MirBlockIndex) -> Self {
            TestInst
        }
    }

    impl MachInstEmit for TestInst {
        fn emit(&self, _ctx: &mut dyn EmitContext) -> core::fmt::Result {
            Ok(())
        }
    }

    struct TestBackend;

    impl LowerBackend for TestBackend {
        type MInst = TestInst;
        type CodegenConfig = ();

        fn lower(
            _ctx: &mut LowerContext<Self::MInst>,
            _inst: HirInst,
        ) -> crate::lower::LoweredOutput {
            unreachable!()
        }

        fn lower_branch(
            _ctx: &mut LowerContext<Self::MInst>,
            _inst: HirInst,
            _target: &[MirBlockIndex],
        ) {
            unreachable!()
        }

        fn data_section_directive() -> &'static str {
            unreachable!()
        }

        fn bss_section_directive() -> &'static str {
            unreachable!()
        }

        fn text_section_directive() -> &'static str {
            unreachable!()
        }

        fn global_directive() -> &'static str {
            unreachable!()
        }

        fn word_directive() -> &'static str {
            unreachable!()
        }

        fn zero_directive() -> &'static str {
            unreachable!()
        }

        fn preg_name(preg: PReg) -> &'static str {
            if preg.class() == RegClass::Float {
                "f0"
            } else {
                "x0"
            }
        }

        fn format_block_label(
            _lb: &crate::block_order::LoweredBlock,
            _func_data: &HirFunctionData,
        ) -> String {
            String::new()
        }

        fn emit_long_jump(_ctx: &mut LowerContext<Self::MInst>, _target: MirBlockIndex) {
            unreachable!()
        }

        fn veneer_lines(_kind: LabelKind, target: &str) -> Vec<String> {
            vec![format!("b {target}")]
        }
    }

    fn empty_program() -> HirProgram {
        Program::new()
    }

    fn buffer<'a>(program: &'a HirProgram, block_labels: Vec<&str>) -> EmitBuffer<'a, TestBackend> {
        buffer_with_opt(program, block_labels, true)
    }

    fn buffer_with_opt<'a>(
        program: &'a HirProgram,
        block_labels: Vec<&str>,
        branch_opt: bool,
    ) -> EmitBuffer<'a, TestBackend> {
        let labels = block_labels
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        EmitBuffer::new(program, "f".to_owned(), labels, branch_opt)
    }

    fn put_inst(buffer: &mut EmitBuffer<'_, TestBackend>, text: &str) {
        core::fmt::write(buffer, format_args!("{text}")).unwrap();
        buffer.end_inst().unwrap();
    }

    #[test]
    fn renders_text_slots_with_labels_in_order() {
        let program = empty_program();
        let mut buffer = buffer(&program, vec![".L_f_entry", ".L_f_exit"]);
        buffer.bind_label(MirBlockIndex::new(0));
        put_inst(&mut buffer, "mov x0, x1");
        buffer.bind_label(MirBlockIndex::new(1));
        put_inst(&mut buffer, "ret");
        assert_eq!(
            buffer.finish(),
            ".L_f_entry:\n    mov x0, x1\n.L_f_exit:\n    ret\n"
        );
    }

    #[test]
    fn renders_branch_slots_with_resolved_targets() {
        let program = empty_program();
        let mut buffer = buffer_with_opt(&program, vec![".L_f_a", ".L_f_b"], false);
        buffer.bind_label(MirBlockIndex::new(0));
        buffer
            .put_branch(
                "b.eq ",
                Some("b.ne "),
                MirBlockIndex::new(1),
                LabelKind::BRANCH19,
            )
            .unwrap();
        buffer
            .put_uncond_branch("b ", MirBlockIndex::new(1), LabelKind::BRANCH26)
            .unwrap();
        buffer.bind_label(MirBlockIndex::new(1));
        put_inst(&mut buffer, "ret");
        assert_eq!(
            buffer.finish(),
            ".L_f_a:\n    b.eq .L_f_b\n    b .L_f_b\n.L_f_b:\n    ret\n"
        );
    }

    #[test]
    fn end_inst_flushes_pending_text_as_one_slot() {
        let program = empty_program();
        let mut buffer = buffer(&program, vec![".L_f_entry"]);
        buffer.bind_label(MirBlockIndex::new(0));
        put_inst(&mut buffer, "add x0, x0, #1");
        put_inst(&mut buffer, "add x1, x1, #2");
        assert_eq!(buffer.cur_slot(), 2);
        assert_eq!(
            buffer.finish(),
            ".L_f_entry:\n    add x0, x0, #1\n    add x1, x1, #2\n"
        );
    }

    #[test]
    fn take_inst_text_consumes_pending_without_creating_a_slot() {
        let program = empty_program();
        let mut buffer = buffer_with_opt(&program, vec![".L_f_entry"], false);
        buffer.bind_label(MirBlockIndex::new(0));
        core::fmt::write(&mut buffer, format_args!("cbz w0, ")).unwrap();
        let prefix = buffer.take_inst_text();
        assert_eq!(prefix, "cbz w0, ");
        assert_eq!(
            buffer.cur_slot(),
            0,
            "take_inst_text must not create a slot"
        );
        buffer
            .put_branch(
                &prefix,
                Some("cbnz w0, "),
                MirBlockIndex::new(0),
                LabelKind::BRANCH19,
            )
            .unwrap();
        assert_eq!(buffer.finish(), ".L_f_entry:\n    cbz w0, .L_f_entry\n");
    }

    // ---- Branch optimization rules (M26) ----

    #[test]
    fn r1_removes_branch_pair_whose_target_is_the_fallthrough() {
        let program = empty_program();
        let mut buffer = buffer(&program, vec![".L_f_a", ".L_f_b"]);
        buffer.bind_label(MirBlockIndex::new(0));
        buffer
            .put_branch(
                "b.eq ",
                Some("b.ne "),
                MirBlockIndex::new(1),
                LabelKind::BRANCH19,
            )
            .unwrap();
        buffer
            .put_uncond_branch("b ", MirBlockIndex::new(1), LabelKind::BRANCH26)
            .unwrap();
        buffer.bind_label(MirBlockIndex::new(1));
        put_inst(&mut buffer, "ret");
        let stats = buffer.branch_stats();
        assert_eq!(stats.fallthrough_removed, 2);
        assert_eq!(
            buffer.finish(),
            ".L_f_a:\n.L_f_b:\n    ret\n",
            "both branches to the fallthrough block are no-ops and must vanish"
        );
    }

    #[test]
    fn r4_inverts_condition_to_skip_trailing_jump() {
        let program = empty_program();
        let mut buffer = buffer(&program, vec![".L_f_a", ".L_f_b", ".L_f_c"]);
        buffer.bind_label(MirBlockIndex::new(0));
        buffer
            .put_branch(
                "b.eq ",
                Some("b.ne "),
                MirBlockIndex::new(1),
                LabelKind::BRANCH19,
            )
            .unwrap();
        buffer
            .put_uncond_branch("b ", MirBlockIndex::new(2), LabelKind::BRANCH26)
            .unwrap();
        buffer.bind_label(MirBlockIndex::new(1));
        let stats = buffer.branch_stats();
        assert_eq!(stats.branches_inverted, 1);
        assert_eq!(
            buffer.finish(),
            ".L_f_a:\n    b.ne .L_f_c\n.L_f_b:\n",
            "b.eq B; b C with B as the fallthrough collapses to one inverted branch"
        );
    }

    #[test]
    fn r4_inverts_back_when_the_inverted_branch_precedes_another_jump() {
        let program = empty_program();
        let mut buffer = buffer(&program, vec![".L_f_a", ".L_f_b", ".L_f_c"]);
        buffer.bind_label(MirBlockIndex::new(0));
        buffer
            .put_branch(
                "b.eq ",
                Some("b.ne "),
                MirBlockIndex::new(1),
                LabelKind::BRANCH19,
            )
            .unwrap();
        buffer
            .put_uncond_branch("b ", MirBlockIndex::new(2), LabelKind::BRANCH26)
            .unwrap();
        buffer.bind_label(MirBlockIndex::new(1)); // R4: b.ne C
        buffer
            .put_uncond_branch("b ", MirBlockIndex::new(0), LabelKind::BRANCH26)
            .unwrap();
        buffer.bind_label(MirBlockIndex::new(2)); // R2 threads B->A; R4 inverts back
        let stats = buffer.branch_stats();
        assert_eq!(stats.branches_inverted, 2);
        assert_eq!(stats.labels_threaded, 1);
        assert_eq!(
            buffer.finish(),
            ".L_f_a:\n    b.eq .L_f_a\n.L_f_b:\n.L_f_c:\n",
            "double inversion restores the original condition"
        );
    }

    #[test]
    fn r2_threads_labels_and_r3_removes_unreachable_jump() {
        let program = empty_program();
        let mut buffer = buffer(&program, vec![".L_f_d", ".L_f_a", ".L_f_b", ".L_f_c"]);
        buffer.bind_label(MirBlockIndex::new(0));
        put_inst(&mut buffer, "nop");
        buffer.bind_label(MirBlockIndex::new(1));
        buffer
            .put_uncond_branch("b ", MirBlockIndex::new(0), LabelKind::BRANCH26)
            .unwrap();
        buffer.bind_label(MirBlockIndex::new(2));
        buffer
            .put_uncond_branch("b ", MirBlockIndex::new(0), LabelKind::BRANCH26)
            .unwrap();
        buffer.bind_label(MirBlockIndex::new(3));
        let stats = buffer.branch_stats();
        assert_eq!(stats.labels_threaded, 2);
        assert_eq!(stats.dead_jumps_removed, 1);
        assert_eq!(
            buffer.finish(),
            ".L_f_d:\n    nop\n.L_f_a:\n    b .L_f_d\n.L_f_b:\n.L_f_c:\n",
            "B's jump is unreachable (B threads to D) and must be removed"
        );
    }

    #[test]
    fn r2_cycle_guard_keeps_self_loop() {
        let program = empty_program();
        let mut buffer = buffer(&program, vec![".L_f_a", ".L_f_b"]);
        buffer.bind_label(MirBlockIndex::new(0));
        buffer
            .put_uncond_branch("b ", MirBlockIndex::new(0), LabelKind::BRANCH26)
            .unwrap();
        buffer.bind_label(MirBlockIndex::new(1));
        put_inst(&mut buffer, "ret");
        assert_eq!(
            buffer.finish(),
            ".L_f_a:\n    b .L_f_a\n.L_f_b:\n    ret\n",
            "aliasing A to itself would create a cycle, so the loop must survive"
        );
    }

    #[test]
    fn finish_renders_labels_with_non_monotonic_offsets() {
        // Branch truncation can move labels backward out of block order; every
        // definition must still render, interleaved by its (possibly reused)
        // slot offset. Regression for the empty-chain cascade in 68_brainfk.
        let program = empty_program();
        let mut buffer = buffer(&program, vec![".L_f_a", ".L_f_b", ".L_f_c", ".L_f_d"]);
        buffer.bind_label(MirBlockIndex::new(0));
        buffer
            .put_uncond_branch("b ", MirBlockIndex::new(3), LabelKind::BRANCH26)
            .unwrap();
        buffer.bind_label(MirBlockIndex::new(1));
        buffer
            .put_uncond_branch("b ", MirBlockIndex::new(3), LabelKind::BRANCH26)
            .unwrap();
        // Truncations below move labels 1 and 0 backward past label 3's slot.
        buffer.bind_label(MirBlockIndex::new(2));
        put_inst(&mut buffer, "nop");
        buffer.bind_label(MirBlockIndex::new(3));
        put_inst(&mut buffer, "ret");
        let out = buffer.finish();
        for name in [".L_f_a", ".L_f_b", ".L_f_c", ".L_f_d"] {
            assert!(
                out.contains(&format!("{name}:")),
                "label {name} must be defined:\n{out}"
            );
        }
        assert_eq!(
            out,
            ".L_f_a:\n    b .L_f_d\n.L_f_b:\n.L_f_c:\n    nop\n.L_f_d:\n    ret\n"
        );
    }

    #[test]
    fn resolve_inserts_veneer_for_forward_out_of_range_branch() {
        let program = empty_program();
        let mut buffer = buffer(&program, vec![".L_f_a", ".L_f_b"]);
        buffer.bind_label(MirBlockIndex::new(0));
        buffer
            .put_branch(
                "b.eq ",
                Some("b.ne "),
                MirBlockIndex::new(1),
                LabelKind::BRANCH14,
            )
            .unwrap();
        for _ in 0..9000 {
            put_inst(&mut buffer, "nop");
        }
        buffer.bind_label(MirBlockIndex::new(1));
        put_inst(&mut buffer, "ret");
        buffer.resolve();
        let stats = buffer.branch_stats();
        assert_eq!(stats.veneers_inserted, 1);
        let out = buffer.finish();
        assert!(
            out.contains("    b.ne .L_f_veneer_0_end\n"),
            "out-of-range branch must be inverted to skip the veneer:\n{out}"
        );
        assert!(
            out.contains(".L_f_veneer_0:\n    b .L_f_b\n.L_f_veneer_0_end:\n"),
            "veneer must reach the original target, with its end label bound after it:\n{out}"
        );
        assert!(
            out.contains("    nop\n.L_f_b:\n    ret\n"),
            "labels bound after the veneer must render at their shifted position:\n{out}"
        );
    }

    #[test]
    fn resolve_inserts_veneer_for_backward_out_of_range_branch() {
        let program = empty_program();
        let mut buffer = buffer(&program, vec![".L_f_a", ".L_f_b"]);
        buffer.bind_label(MirBlockIndex::new(0));
        put_inst(&mut buffer, "ret");
        for _ in 0..9000 {
            put_inst(&mut buffer, "nop");
        }
        buffer.bind_label(MirBlockIndex::new(1));
        buffer
            .put_branch(
                "b.eq ",
                Some("b.ne "),
                MirBlockIndex::new(0),
                LabelKind::BRANCH14,
            )
            .unwrap();
        buffer.resolve();
        let stats = buffer.branch_stats();
        assert_eq!(stats.veneers_inserted, 1);
        let out = buffer.finish();
        assert!(
            out.contains(".L_f_veneer_0:\n    b .L_f_a\n.L_f_veneer_0_end:\n"),
            "backward branch needs a veneer reaching its target:\n{out}"
        );
    }

    #[test]
    fn resolve_relaxes_multiple_out_of_range_branches_in_few_rounds() {
        // Three branches over a tiny reach: every relaxation round must keep
        // previously inserted veneers intact and converge within 3 rounds.
        let program = empty_program();
        let mut buffer = buffer(&program, vec![".L_f_a", ".L_f_b", ".L_f_c", ".L_f_d"]);
        let tiny = LabelKind {
            positive: 4,
            negative: 4,
        };
        buffer.bind_label(MirBlockIndex::new(0));
        buffer
            .put_branch("b.eq ", Some("b.ne "), MirBlockIndex::new(1), tiny)
            .unwrap();
        buffer
            .put_uncond_branch("b ", MirBlockIndex::new(2), tiny)
            .unwrap();
        buffer
            .put_branch("b.ne ", Some("b.eq "), MirBlockIndex::new(3), tiny)
            .unwrap();
        buffer.bind_label(MirBlockIndex::new(1));
        put_inst(&mut buffer, "nop");
        buffer.bind_label(MirBlockIndex::new(2));
        put_inst(&mut buffer, "nop");
        buffer.bind_label(MirBlockIndex::new(3));
        put_inst(&mut buffer, "ret");
        buffer.resolve();
        let stats = buffer.branch_stats();
        assert_eq!(stats.veneers_inserted, 3);
        let out = buffer.finish();
        for id in 0..3 {
            assert!(
                out.contains(&format!(".L_f_veneer_{id}:")),
                "veneer {id} must be present:\n{out}"
            );
        }
        assert!(
            out.contains("    b.eq .L_f_veneer_0_end\n"),
            "the third branch (b.ne D) must invert and skip its veneer:\n{out}"
        );
    }

    #[test]
    fn resolve_handles_branch19_out_of_range_over_one_megabyte() {
        // A genuine >1MB forward jump: BRANCH19 reach is ±1MB, so the veneer
        // must be spliced and the final label must render past all nops.
        let program = empty_program();
        let mut buffer = buffer(&program, vec![".L_f_a", ".L_f_b"]);
        buffer.bind_label(MirBlockIndex::new(0));
        buffer
            .put_branch(
                "b.eq ",
                Some("b.ne "),
                MirBlockIndex::new(1),
                LabelKind::BRANCH19,
            )
            .unwrap();
        for _ in 0..270_000 {
            put_inst(&mut buffer, "nop");
        }
        buffer.bind_label(MirBlockIndex::new(1));
        put_inst(&mut buffer, "ret");
        buffer.resolve();
        let stats = buffer.branch_stats();
        assert_eq!(stats.veneers_inserted, 1);
        let out = buffer.finish();
        assert!(
            out.contains("    b.ne .L_f_veneer_0_end\n"),
            "branch beyond ±1MB must target the veneer end:\n{out}"
        );
        assert!(
            out.contains("    nop\n.L_f_b:\n    ret\n"),
            "the shifted target label must render right before ret:\n{out}"
        );
    }
}
