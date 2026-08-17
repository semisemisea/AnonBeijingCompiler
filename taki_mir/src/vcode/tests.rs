use super::*;
use crate::{
    abi::{ABIMachineSpec, ArgPair, ArgSlot, CalleeABI, FrameLayout, StackAMode},
    prelude::{ArenaContext, HirBasicBlock, HirFunctionData, HirInst, HirType},
    reg_alloc::reg::{MachineEnv, OperandVisitorImpl, PReg},
    register::{Reg, VRegAllocator, Writable},
    types::{I64, LoweredType, V4I32},
};
use raana_ir::{
    ir::Program,
    opt::prelude::{BasicBlockBuilder, LocalInstBuilder},
};

#[derive(Clone, Debug)]
enum TestInst {
    Ret,
    LoadImm {
        rd: Writable<Reg>,
    },
    Mov {
        src: Reg,
        dst: Writable<Reg>,
    },
    /// Fake sink: keeps every listed register live through the block end.
    UseAll {
        regs: Vec<Reg>,
    },
    /// Fake call: clobbers the given physical registers, forcing values
    /// live across it to spill (mirrors a real call site).
    Call {
        clobbers: PRegSet,
    },
    Jump,
    Nop,
}

struct TestABI;

impl MachInst for TestInst {
    type ABISpec = TestABI;

    fn get_operands(&mut self, collector: &mut impl OperandVisitor) {
        match self {
            Self::LoadImm { rd } => collector.reg_def(rd),
            Self::Mov { src, dst } => {
                collector.reg_use(src);
                collector.reg_def(dst);
            }
            Self::UseAll { regs } => {
                for reg in regs {
                    collector.reg_use(reg);
                }
            }
            Self::Call { clobbers } => collector.reg_clobbers(*clobbers),
            Self::Ret | Self::Jump | Self::Nop => {}
        }
    }

    fn is_move(&self) -> Option<(Writable<Reg>, Reg)> {
        match self {
            Self::Mov { src, dst } => Some((*dst, *src)),
            _ => None,
        }
    }

    fn is_term(&self) -> MachTerminator {
        match self {
            Self::Ret => MachTerminator::Return,
            Self::Jump => MachTerminator::Branch,
            _ => MachTerminator::None,
        }
    }

    fn rc_for_type(ty: LoweredType) -> (&'static [RegClass], &'static [LoweredType]) {
        match ty {
            I64 => (&[RegClass::Int], &[I64]),
            V4I32 => (&[RegClass::Vector], &[V4I32]),
            _ => unreachable!(),
        }
    }

    fn gen_jump(_target: MirBlockIndex) -> Self {
        Self::Jump
    }
}

impl MachInstEmit for TestInst {
    fn emit(&self, _ctx: &mut dyn EmitContext) -> core::fmt::Result {
        Ok(())
    }
}

impl ABIMachineSpec for TestABI {
    type I = TestInst;

    fn stack_align() -> u32 {
        16
    }

    fn spillslot_size(regclass: RegClass) -> u32 {
        match regclass {
            // Vectors occupy two 8-byte spill units, mirroring AArch64.
            RegClass::Vector => 2,
            _ => 1,
        }
    }

    fn spill_unit_bytes() -> u32 {
        8
    }

    fn is_callee_saved(_preg: PReg) -> bool {
        false
    }

    fn gen_load_stack(_mem: StackAMode, dst: Writable<Reg>, _ty: LoweredType) -> Self::I {
        TestInst::LoadImm { rd: dst }
    }

    fn gen_load_imm(dst: Writable<Reg>, _value: u64, _ty: LoweredType) -> Self::I {
        TestInst::LoadImm { rd: dst }
    }

    fn gen_load_addr(dst: Writable<Reg>, _label: HirInst) -> Self::I {
        TestInst::LoadImm { rd: dst }
    }

    fn gen_get_stack_addr(_mem: StackAMode, dst: Writable<Reg>) -> Self::I {
        TestInst::LoadImm { rd: dst }
    }

    fn gen_args(_args: Vec<ArgPair>) -> Self::I {
        TestInst::Nop
    }

    fn gen_store_stack(_src: Reg, _mem: StackAMode, _ty: LoweredType) -> Self::I {
        TestInst::Nop
    }

    fn gen_move(src: Reg, dst: Reg, _ty: LoweredType) -> Self::I {
        TestInst::Mov {
            src,
            dst: Writable::from_reg(dst),
        }
    }

    fn compute_arg_loc(_arena: ArenaContext<'_>) -> (Vec<ArgSlot>, u32) {
        (vec![], 0)
    }

    fn compute_call_arg_loc(_types: &[HirType]) -> (Vec<ArgSlot>, u32) {
        (vec![], 0)
    }

    fn get_machine_env() -> &'static MachineEnv {
        static ENV: std::sync::LazyLock<MachineEnv> = std::sync::LazyLock::new(|| MachineEnv {
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

    fn gen_prologue_frame_setup(_frame: &FrameLayout) -> smallvec::SmallVec<[Self::I; 16]> {
        smallvec::SmallVec::new()
    }

    fn gen_epilogue_frame_restore(_frame: &FrameLayout) -> smallvec::SmallVec<[Self::I; 16]> {
        smallvec::SmallVec::new()
    }

    fn gen_clobber_save(_frame: &FrameLayout) -> smallvec::SmallVec<[Self::I; 16]> {
        smallvec::SmallVec::new()
    }

    fn gen_clobber_restore(_frame: &FrameLayout) -> smallvec::SmallVec<[Self::I; 16]> {
        smallvec::SmallVec::new()
    }
}

fn add_block(data: &mut HirFunctionData, name: &str) {
    let block = data.new_basic_block().basic_block(name.to_owned(), vec![]);
    let ret = data.new_local_inst().ret(None);
    data.layout_mut().push_bb_back(block);
    data.layout_mut().insert_inst(block, ret);
}

fn empty_vcode() -> VCodeContainer<TestInst> {
    let mut program = Program::new();
    let func = program.new_function(HirType::get_unit(), "verify".to_owned(), vec![]);
    add_block(program.func_data_mut(func), "entry");
    let arena = ArenaContext {
        program: &program,
        curr_func: Some(func),
    };
    let abi = CalleeABI::<TestABI>::new(arena);
    let order = BlockLoweringOrder::new(arena);
    let mut builder = VCodeBuilder::new(abi, order);
    builder.push(TestInst::Ret);
    builder.end_bb();
    builder.build(VRegAllocator::with_capaticy(0))
}

fn vcode_with_integer_def() -> VCodeContainer<TestInst> {
    let mut program = Program::new();
    let func = program.new_function(HirType::get_unit(), "verify".to_owned(), vec![]);
    add_block(program.func_data_mut(func), "entry");
    let arena = ArenaContext {
        program: &program,
        curr_func: Some(func),
    };
    let abi = CalleeABI::<TestABI>::new(arena);
    let order = BlockLoweringOrder::new(arena);
    let mut builder = VCodeBuilder::new(abi, order);
    let mut vregs = VRegAllocator::with_capaticy(1);
    let dst = Writable::from_reg(vregs.alloc(I64));
    builder.push(TestInst::Ret);
    builder.push(TestInst::LoadImm { rd: dst });
    builder.end_bb();
    builder.build(vregs)
}

#[test]
fn ion_allocation_output_verifies_for_basic_vcode() {
    let vcode = vcode_with_integer_def();
    let output = crate::reg_alloc::ion::run(&vcode, vcode.abi.machine_env())
        .expect("Ion allocation should succeed");

    assert!(vcode.verify_alloc_output(&output).is_ok());
}

#[test]
fn take_args_returns_none_when_no_register_arguments_bound() {
    let mut program = Program::new();
    let func = program.new_function(HirType::get_unit(), "args".to_owned(), vec![]);
    add_block(program.func_data_mut(func), "entry");
    let arena = ArenaContext {
        program: &program,
        curr_func: Some(func),
    };
    let mut abi = CalleeABI::<TestABI>::new(arena);
    assert!(abi.take_args().is_none());
}

#[test]
fn strict_ssa_verifier_rejects_duplicate_definitions() {
    let mut program = Program::new();
    let func = program.new_function(HirType::get_unit(), "verify".to_owned(), vec![]);
    add_block(program.func_data_mut(func), "entry");
    let arena = ArenaContext {
        program: &program,
        curr_func: Some(func),
    };
    let abi = CalleeABI::<TestABI>::new(arena);
    let order = BlockLoweringOrder::new(arena);
    let mut builder = VCodeBuilder::new(abi, order);
    let mut vregs = VRegAllocator::with_capaticy(1);
    let dst = Writable::from_reg(vregs.alloc(I64));
    builder.push(TestInst::Ret);
    builder.push(TestInst::LoadImm { rd: dst });
    builder.push(TestInst::LoadImm { rd: dst });
    builder.end_bb();

    let error = builder.build(vregs).verify("test").unwrap_err();
    assert!(error.contains("more than one definition"), "{error}");
}

#[test]
fn strict_ssa_verifier_rejects_undefined_instruction_use() {
    let mut program = Program::new();
    let func = program.new_function(HirType::get_unit(), "verify".to_owned(), vec![]);
    add_block(program.func_data_mut(func), "entry");
    let arena = ArenaContext {
        program: &program,
        curr_func: Some(func),
    };
    let abi = CalleeABI::<TestABI>::new(arena);
    let order = BlockLoweringOrder::new(arena);
    let mut builder = VCodeBuilder::new(abi, order);
    let mut vregs = VRegAllocator::with_capaticy(2);
    let undefined = vregs.alloc(I64);
    let dst = Writable::from_reg(vregs.alloc(I64));
    builder.push(TestInst::Ret);
    builder.push(TestInst::Mov {
        src: undefined,
        dst,
    });
    builder.end_bb();

    let error = builder.build(vregs).verify("test").unwrap_err();
    assert!(error.contains("has no dominating definition"), "{error}");
}

#[test]
fn strict_ssa_verifier_rejects_entry_block_parameters() {
    let mut program = Program::new();
    let func = program.new_function(HirType::get_unit(), "verify".to_owned(), vec![]);
    add_block(program.func_data_mut(func), "entry");
    let arena = ArenaContext {
        program: &program,
        curr_func: Some(func),
    };
    let abi = CalleeABI::<TestABI>::new(arena);
    let order = BlockLoweringOrder::new(arena);
    let mut builder = VCodeBuilder::new(abi, order);
    let mut vregs = VRegAllocator::with_capaticy(1);
    builder.push(TestInst::Ret);
    builder.add_block_param(vregs.alloc(I64).into());
    builder.end_bb();

    let error = builder.build(vregs).verify("test").unwrap_err();
    assert!(
        error.contains("entry block has live-in block parameter"),
        "{error}"
    );
}

#[test]
fn strict_ssa_verifier_rejects_undefined_edge_argument() {
    let mut program = Program::new();
    let func = program.new_function(HirType::get_unit(), "verify".to_owned(), vec![]);
    add_block(program.func_data_mut(func), "entry");
    let arena = ArenaContext {
        program: &program,
        curr_func: Some(func),
    };
    let abi = CalleeABI::<TestABI>::new(arena);
    let order = BlockLoweringOrder::new(arena);
    let mut builder = VCodeBuilder::new(abi, order);
    let mut vregs = VRegAllocator::with_capaticy(2);
    let param = vregs.alloc(I64);
    let undefined = vregs.alloc(I64);
    builder.push(TestInst::Ret);
    builder.add_block_param(param.into());
    builder.end_bb();
    builder.push(TestInst::gen_jump(Block::new(1)));
    builder.add_succ(Block::new(1), &[undefined]);
    builder.end_bb();

    let error = builder.build(vregs).verify("test").unwrap_err();
    assert!(
        error.contains("arguments") && error.contains("without a definition"),
        "{error}"
    );
}

#[test]
fn vcode_verifier_rejects_non_final_terminator() {
    let mut vcode = empty_vcode();
    vcode.insts.insert(0, TestInst::Ret);
    vcode.operands_range = Ranges::default();
    vcode.operands_range.push_end(0);
    vcode.operands_range.push_end(0);
    vcode.inst_is_branch.insert(0, false);
    vcode.inst_is_ret.insert(0, true);
    vcode.block_range = Ranges::default();
    vcode.block_range.push_end(2);

    let error = vcode.verify("test").unwrap_err();
    assert!(error.contains("non-final terminator"), "{error}");
}

#[test]
fn allocation_output_verifier_accepts_empty_return() {
    let vcode = empty_vcode();
    let output = Output {
        inst_alloc_offsets: vec![0],
        ..Output::default()
    };

    assert!(vcode.verify_alloc_output(&output).is_ok());
}

#[test]
fn allocation_output_verifier_rejects_out_of_range_edit_slot() {
    let vcode = empty_vcode();
    let output = Output {
        inst_alloc_offsets: vec![0],
        edits: vec![(
            crate::reg_alloc::reg::ProgPoint::before(0),
            Edit::Move {
                from: crate::reg_alloc::reg::Allocation::stack(
                    crate::reg_alloc::reg::SpillSlot::new(0),
                ),
                to: crate::reg_alloc::reg::Allocation::reg(PReg::new(5, RegClass::Int)),
                class: RegClass::Int,
                vreg: None,
            },
        )],
        ..Output::default()
    };

    let error = vcode.verify_alloc_output(&output).unwrap_err();
    assert!(error.contains("out-of-range spill slot"), "{error}");
}

#[test]
fn allocation_output_verifier_accepts_vector_edit() {
    let vcode = empty_vcode();
    let output = Output {
        inst_alloc_offsets: vec![0],
        edits: vec![(
            crate::reg_alloc::reg::ProgPoint::before(0),
            Edit::Move {
                from: crate::reg_alloc::reg::Allocation::stack(
                    crate::reg_alloc::reg::SpillSlot::new(0),
                ),
                to: crate::reg_alloc::reg::Allocation::stack(
                    crate::reg_alloc::reg::SpillSlot::new(0),
                ),
                class: RegClass::Vector,
                vreg: None,
            },
        )],
        num_spillslots: 2,
        ..Output::default()
    };

    assert!(
        vcode.verify_alloc_output(&output).is_ok(),
        "vector spill edits must pass allocation-output verification"
    );
}

#[test]
fn allocation_output_verifier_rejects_instruction_arity_mismatch() {
    let vcode = vcode_with_integer_def();
    let output = Output {
        inst_alloc_offsets: vec![0, 0],
        ..Output::default()
    };

    let error = vcode.verify_alloc_output(&output).unwrap_err();
    assert!(error.contains("expected 1"), "{error}");
}

#[test]
fn allocation_output_verifier_rejects_register_class_mismatch() {
    let vcode = vcode_with_integer_def();
    let output = Output {
        inst_alloc_offsets: vec![0, 1],
        allocs: vec![crate::reg_alloc::reg::Allocation::reg(PReg::new(
            0,
            RegClass::Float,
        ))],
        ..Output::default()
    };

    let error = vcode.verify_alloc_output(&output).unwrap_err();
    assert!(error.contains("assigned register"), "{error}");
}

/// Build a single-block VCode with two vector values copied through
/// register-register moves and then kept live across a call that clobbers
/// every vector register. Mirrors the AArch64 machine-layer acceptance:
/// vector values must complete register allocation (register/stack moves
/// and spills) without panicking.
fn vector_chain_vcode() -> VCodeContainer<TestInst> {
    let mut program = Program::new();
    let func = program.new_function(HirType::get_unit(), "vector".to_owned(), vec![]);
    add_block(program.func_data_mut(func), "entry");
    let arena = ArenaContext {
        program: &program,
        curr_func: Some(func),
    };
    let abi = CalleeABI::<TestABI>::new(arena);
    let order = BlockLoweringOrder::new(arena);
    let mut builder = VCodeBuilder::new(abi, order);
    let mut vregs = VRegAllocator::with_capaticy(4);

    let d0 = vregs.alloc(V4I32);
    let d1 = vregs.alloc(V4I32);
    let o0 = vregs.alloc(V4I32);
    let o1 = vregs.alloc(V4I32);
    let mut clobber_all = PRegSet::empty();
    for index in 0..4 {
        clobber_all = clobber_all.with(PReg::new(index, RegClass::Vector));
    }
    // Instructions are pushed in reverse of their final order (the block
    // terminator goes first). Final order: defs, moves, call, sink, ret.
    builder.push(TestInst::Ret);
    builder.push(TestInst::UseAll { regs: vec![o0, o1] });
    builder.push(TestInst::Call {
        clobbers: clobber_all,
    });
    for (src, dst) in [(d0, o0), (d1, o1)] {
        builder.push(TestInst::Mov {
            src,
            dst: Writable::from_reg(dst),
        });
    }
    builder.push(TestInst::LoadImm {
        rd: Writable::from_reg(d1),
    });
    builder.push(TestInst::LoadImm {
        rd: Writable::from_reg(d0),
    });
    builder.end_bb();
    builder.build(vregs)
}

#[test]
fn vector_values_allocate_through_moves_and_spills() {
    let mut vcode = vector_chain_vcode();
    let mut preferred = PRegSet::empty();
    for index in 0..4 {
        preferred = preferred.with(PReg::new(index, RegClass::Vector));
    }
    let env = MachineEnv {
        preferred_regs_by_class: [
            PRegSet::empty().with(PReg::new(0, RegClass::Int)),
            PRegSet::empty().with(PReg::new(0, RegClass::Float)),
            preferred,
        ],
        non_preferred_regs_by_class: [PRegSet::empty(); 3],
        scratch_by_class: [None; 3],
        post_ra_scratch_by_class: [vec![], vec![], vec![]],
        fixed_stack_slots: vec![],
        aliased_banks: &[],
    };

    let output = crate::reg_alloc::ion::run(&vcode, &env)
        .expect("Ion allocation should succeed for vector values");
    assert!(
        output.num_spillslots >= 2,
        "vector values live across an all-vector clobber must spill, got {} slots",
        output.num_spillslots
    );
    assert!(
        output.edits.iter().any(|(_, edit)| matches!(
            edit,
            Edit::Move {
                class: RegClass::Vector,
                ..
            }
        )),
        "vector moves and spills must materialize as Vector-class edits"
    );
    assert!(vcode.verify_alloc_output(&output).is_ok());

    let spill_size = u32::try_from(output.num_spillslots).unwrap() * vcode.abi.spill_unit_bytes();
    vcode
        .abi
        .compute_frame_layout(spill_size, &output)
        .expect("frame layout should accept vector spill units");
    vcode.write_back_allocs(&output);
    vcode.finalize_for_emission(&output);
    // The finalized stream contains spill moves plus the original
    // instructions; it must be non-empty and never panic on Vector class.
    assert!(vcode.num_insts() > 0);
}
