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
use crate::vcode::EmitContext;

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
}

/// One emitted instruction slot.
#[derive(Clone, Debug)]
pub enum Slot {
    /// An ordinary instruction's text, rendered verbatim on one line.
    Text(String),
    /// An optimizable branch: mnemonic + operands before the target label.
    Branch(BranchRef),
    /// A long-branch veneer inserted during `resolve` (one text line each).
    Veneer(Vec<String>),
}

/// Structured form of an optimizable branch.
#[derive(Clone, Debug)]
pub struct BranchRef {
    /// Mnemonic and operands before the target label, e.g. `"b.eq "`.
    pub prefix: String,
    /// Inverted-encoding text, e.g. `"b.ne "` for `"b.eq "`. `None` for
    /// unconditional branches.
    pub inv_prefix: Option<String>,
    /// Intra-function target block.
    pub target: MirBlockIndex,
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
    _phantom: PhantomData<B>,
}

impl<'a, B: LowerBackend> EmitBuffer<'a, B> {
    pub fn new(program: &'a HirProgram, block_labels: Vec<String>) -> Self {
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
            _phantom: PhantomData,
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
    fn label_name(&self, label: MirBlockIndex) -> &str {
        let label = self.resolved(label);
        &self.block_labels[label.index()]
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
    /// dead-jump removal, condition inversion). Wired up in M26; the hook is
    /// called from `bind_label` and by the emitter at function end.
    fn optimize_branches(&mut self) {
        self.lazily_clear_labels_at_tail();
    }

    /// Range check every branch against its `LabelKind` and insert veneers
    /// where needed. Implemented in M27; currently a no-op.
    pub fn resolve(&mut self) {}

    /// Render the buffer: block labels, then one `    `-indented line per
    /// slot, with branch targets resolved through the alias chain.
    pub fn finish(self) -> String {
        let mut out = String::new();
        let mut next_block = 0;
        for (index, slot) in self.slots.iter().enumerate() {
            self.emit_labels_at(&mut out, index, &mut next_block);
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
                Slot::Veneer(lines) => {
                    for line in lines {
                        out.push_str("    ");
                        out.push_str(line);
                        out.push('\n');
                    }
                }
            }
        }
        self.emit_labels_at(&mut out, self.slots.len(), &mut next_block);
        out
    }

    fn emit_labels_at(&self, out: &mut String, at: usize, next_block: &mut usize) {
        while *next_block < self.label_offsets.len() && self.label_offsets[*next_block] == at {
            out.push_str(&self.block_labels[*next_block]);
            out.push_str(":\n");
            *next_block += 1;
        }
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
        let label = self.label_name(idx).to_owned();
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
            target,
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

        fn gen_store_stack(
            _src: Reg,
            _mem: crate::abi::StackAMode,
            _ty: LoweredType,
        ) -> Self::I {
            TestInst
        }

        fn gen_move(src: Reg, dst: Reg, _ty: LoweredType) -> Self::I {
            let _ = (src, dst);
            TestInst
        }

        fn compute_arg_loc(
            _arena: ArenaContext<'_>,
        ) -> (Vec<crate::abi::ArgSlot>, u32) {
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

        fn gen_clobber_save(
            _frame: &crate::abi::FrameLayout,
        ) -> smallvec::SmallVec<[Self::I; 16]> {
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

        fn rc_for_type(
            _ty: LoweredType,
        ) -> (&'static [RegClass], &'static [LoweredType]) {
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
    }

    fn empty_program() -> HirProgram {
        Program::new()
    }

    fn buffer<'a>(
        program: &'a HirProgram,
        block_labels: Vec<&str>,
    ) -> EmitBuffer<'a, TestBackend> {
        let labels = block_labels
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        EmitBuffer::new(program, labels)
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
        let mut buffer = buffer(&program, vec![".L_f_entry"]);
        buffer.bind_label(MirBlockIndex::new(0));
        core::fmt::write(&mut buffer, format_args!("cbz w0, ")).unwrap();
        let prefix = buffer.take_inst_text();
        assert_eq!(prefix, "cbz w0, ");
        assert_eq!(buffer.cur_slot(), 0, "take_inst_text must not create a slot");
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
}
