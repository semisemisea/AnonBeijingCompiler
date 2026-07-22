//! AAPCS64 calling convention and frame hooks for AArch64.

use smallvec::{SmallVec, smallvec};
use taki_mir::{
    abi::{ABIMachineSpec, ArgSlot, FrameLayout, StackAMode},
    prelude::ArenaContext,
    reg_alloc::reg::{MachineEnv, PReg, RegClass},
    register::{Reg, Writable},
    types::{F32, I32, I64, LoweredType},
};
use raana_ir::ir::arena::Arena;

use crate::{
    constants::materialize_integer_constant,
    instructions::{AMode, AluOp, Imm12, MInst, MemoryType, PairAMode, SImm7Scaled},
    labels::Label,
    regs::{self, Gpr, OperandSize},
};

pub struct AArch64Abi;

impl ABIMachineSpec for AArch64Abi {
    type I = MInst;

    fn stack_align() -> u32 {
        16
    }
    fn spillslot_size(_regclass: RegClass) -> u32 {
        8
    }
    fn is_callee_saved(preg: PReg) -> bool {
        regs::is_callee_saved(preg)
    }

    fn gen_load_stack(mem: StackAMode, dst: Writable<Reg>, ty: LoweredType) -> MInst {
        MInst::Load {
            ty: memory_type(ty),
            dst,
            addr: mem.into(),
        }
    }

    fn gen_store_stack(src: Reg, mem: StackAMode, ty: LoweredType) -> MInst {
        MInst::Store {
            ty: memory_type(ty),
            src,
            addr: mem.into(),
        }
    }

    fn gen_load_imm(dst: Writable<Reg>, imm: i32) -> MInst {
        MInst::LoadImm {
            size: OperandSize::Size32,
            dst,
            value: imm as u32 as u64,
        }
    }

    fn gen_load_addr(dst: Writable<Reg>, label: taki_mir::prelude::HirInst) -> MInst {
        MInst::LoadAddr {
            dst,
            label: Label::from_global_inst(label),
        }
    }

    fn gen_args(args: Vec<taki_mir::abi::ArgPair>) -> MInst {
        MInst::Args { pairs: args }
    }
    fn gen_ret() -> MInst {
        MInst::Ret
    }
    fn gen_jump(block: taki_mir::prelude::HirBasicBlock) -> MInst {
        // HIR blocks require BlockLoweringOrder's edge-block mapping before
        // they can become MIR block labels. This ABI hook has no such map.
        panic!("AArch64 ABI jump generation requires BlockLoweringOrder mapping: {block:?}")
    }
    fn gen_nop() -> MInst {
        MInst::Nop
    }

    fn gen_move(src: Reg, dst: Reg, ty: LoweredType) -> MInst {
        if ty == F32 {
            MInst::FMov {
                dst: Writable::from_reg(dst),
                src,
            }
        } else {
            MInst::Mov {
                size: operand_size(ty),
                dst: Writable::from_reg(dst),
                src,
            }
        }
    }

    fn compute_arg_loc(arena: ArenaContext<'_>) -> (Vec<ArgSlot>, u32) {
        use raana_ir::ir::TypeKind;

        let mut slots = Vec::new();
        let (mut int_index, mut float_index, mut stack_offset) = (0usize, 0usize, 0u32);
        for &param in arena.f().params() {
            let ty = arena.inst_data(param).ty().clone();
            let reg = match ty.kind() {
                TypeKind::Float32 if float_index < regs::FLOAT_ARG_REGS.len() => {
                    let reg = regs::FLOAT_ARG_REGS[float_index];
                    float_index += 1;
                    Some(reg)
                }
                TypeKind::Int32 | TypeKind::Pointer(_) | TypeKind::String
                    if int_index < regs::INT_ARG_REGS.len() =>
                {
                    let reg = regs::INT_ARG_REGS[int_index];
                    int_index += 1;
                    Some(reg)
                }
                TypeKind::Float32 => {
                    float_index += 1;
                    None
                }
                TypeKind::Int32 | TypeKind::Pointer(_) | TypeKind::String => {
                    int_index += 1;
                    None
                }
                _ => unreachable!("non-scalar AAPCS64 parameter: {:?}", ty.kind()),
            };
            if let Some(reg) = reg {
                slots.push(ArgSlot::Reg {
                    reg: reg.to_physical_reg().unwrap(),
                    ty,
                });
            } else {
                // AAPCS64 uses one eight-byte stack slot per scalar overflow arg.
                slots.push(ArgSlot::Stack {
                    offset: i64::from(stack_offset),
                    ty,
                });
                stack_offset += 8;
            }
        }
        (slots, stack_offset)
    }

    fn get_machine_env() -> &'static MachineEnv {
        regs::machine_env()
    }

    fn gen_prologue_frame_setup(frame: &FrameLayout) -> SmallVec<[MInst; 16]> {
        let mut insts = smallvec![];
        if frame.setup_area_size != 0 {
            insts.push(MInst::StorePair {
                ty: MemoryType::I64,
                src1: regs::int_reg(regs::FP),
                src2: regs::int_reg(regs::LR),
                addr: PairAMode::PreIndex {
                    base: Gpr::Sp,
                    offset: SImm7Scaled::new(-16, 8).unwrap(),
                },
            });
        }
        append_sp_adjust(
            &mut insts,
            -(i64::from(frame.total_size) - i64::from(frame.setup_area_size)),
        );
        if frame.setup_area_size != 0 {
            // FP denotes the caller's SP, making incoming stack args [fp,#off].
            append_add_constant(
                &mut insts,
                Writable::from_reg(regs::int_reg(regs::FP)),
                Gpr::Sp,
                i64::from(frame.total_size),
            );
        }
        insts
    }

    fn gen_epilogue_frame_restore(frame: &FrameLayout) -> SmallVec<[MInst; 16]> {
        let mut insts = smallvec![];
        append_sp_adjust(
            &mut insts,
            i64::from(frame.total_size) - i64::from(frame.setup_area_size),
        );
        if frame.setup_area_size != 0 {
            insts.push(MInst::LoadPair {
                ty: MemoryType::I64,
                dst1: Writable::from_reg(regs::int_reg(regs::FP)),
                dst2: Writable::from_reg(regs::int_reg(regs::LR)),
                addr: PairAMode::PostIndex {
                    base: Gpr::Sp,
                    offset: SImm7Scaled::new(16, 8).unwrap(),
                },
            });
        }
        insts
    }

    fn gen_clobber_save(frame: &FrameLayout) -> SmallVec<[MInst; 16]> {
        let mut insts = smallvec![];
        let base = i64::from(frame.total_size - frame.setup_area_size);
        for (index, preg) in frame.callee_saved.iter().enumerate() {
            insts.push(MInst::Store {
                ty: if preg.class() == RegClass::Float {
                    MemoryType::F64
                } else {
                    MemoryType::I64
                },
                src: Reg::from_physical_reg(*preg),
                addr: AMode::FrameSlot(base - (index as i64 + 1) * 8),
            });
        }
        insts
    }

    fn gen_clobber_restore(frame: &FrameLayout) -> SmallVec<[MInst; 16]> {
        let mut insts = smallvec![];
        let base = i64::from(frame.total_size - frame.setup_area_size);
        for (index, preg) in frame.callee_saved.iter().enumerate() {
            insts.push(MInst::Load {
                ty: if preg.class() == RegClass::Float {
                    MemoryType::F64
                } else {
                    MemoryType::I64
                },
                dst: Writable::from_reg(Reg::from_physical_reg(*preg)),
                addr: AMode::FrameSlot(base - (index as i64 + 1) * 8),
            });
        }
        insts
    }
}

fn operand_size(ty: LoweredType) -> OperandSize {
    if ty == I32 {
        OperandSize::Size32
    } else {
        OperandSize::Size64
    }
}
fn memory_type(ty: LoweredType) -> MemoryType {
    if ty == F32 {
        MemoryType::F32
    } else if ty == I32 {
        MemoryType::I32
    } else {
        MemoryType::I64
    }
}

fn append_sp_adjust(insts: &mut SmallVec<[MInst; 16]>, amount: i64) {
    if amount == 0 {
        return;
    }
    // x0 carries an integer return value at epilogue time, so frame teardown
    // must use a caller-saved temporary outside the result registers.
    append_add_constant(
        insts,
        Writable::from_reg(regs::int_reg(17)),
        Gpr::Sp,
        amount,
    );
    insts.push(MInst::MovPhys {
        size: OperandSize::Size64,
        dst: Gpr::Sp,
        src: Gpr::Reg(regs::int_reg(17)),
    });
}

fn append_add_constant(
    insts: &mut SmallVec<[MInst; 16]>,
    dst: Writable<Reg>,
    base: Gpr,
    amount: i64,
) {
    if amount >= 0 {
        if let Some(imm) = Imm12::new(amount as u16, false).filter(|_| amount <= 0xfff) {
            insts.push(MInst::AluRRImm12 {
                op: AluOp::Add,
                size: OperandSize::Size64,
                dst,
                src: base,
                imm,
            });
            return;
        }
    }
    let base = match base {
        Gpr::Reg(reg) => reg,
        Gpr::Sp => {
            let copy = Writable::from_reg(regs::int_reg(17));
            insts.push(MInst::MovPhys {
                size: OperandSize::Size64,
                dst: Gpr::Reg(copy.to_reg()),
                src: Gpr::Sp,
            });
            copy.to_reg()
        }
        Gpr::Zr => unreachable!("frame arithmetic cannot use the zero register as its base"),
    };
    let scratch = Writable::from_reg(regs::int_reg(regs::INT_POST_RA_SCRATCH[0]));
    insts.extend(materialize_integer_constant(
        amount as u64,
        OperandSize::Size64,
        scratch,
    ));
    insts.push(MInst::AluRRR {
        op: AluOp::Add,
        size: OperandSize::Size64,
        dst,
        lhs: base,
        rhs: scratch.to_reg(),
    });
}
