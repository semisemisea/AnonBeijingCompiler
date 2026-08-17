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

    fn gen_load_imm(dst: crate::register::Writable<Reg>, _value: u64, _ty: LoweredType) -> Self::I {
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
                aliased_banks: &[],
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

    fn gen_clobber_restore(_frame: &crate::abi::FrameLayout) -> smallvec::SmallVec<[Self::I; 16]> {
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

    fn lower(_ctx: &mut LowerContext<Self::MInst>, _inst: HirInst) -> crate::lower::LoweredOutput {
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
