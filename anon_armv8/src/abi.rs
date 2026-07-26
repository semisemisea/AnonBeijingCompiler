//! AAPCS64 calling convention and frame hooks for AArch64.

use raana_ir::ir::arena::Arena;
use smallvec::{SmallVec, smallvec};
use taki_mir::{
    abi::{ABIMachineSpec, ArgSlot, FrameLayout, StackAMode},
    prelude::ArenaContext,
    reg_alloc::reg::{MachineEnv, PReg, RegClass},
    register::{Reg, Writable},
    types::{F32, I32, I64, LoweredType},
};

use crate::{
    constants::materialize_integer_constant,
    instructions::{
        AMode, AluOp, Imm12, MInst, MemoryType, PairAMode, SImm7Scaled, SImm9, UImm12Scaled,
    },
    labels::Label,
    regs::{self, Gpr, OperandSize, RegOrZr},
};

pub struct AArch64Abi;

impl ABIMachineSpec for AArch64Abi {
    type I = MInst;

    fn stack_align() -> u32 {
        16
    }
    fn spillslot_size(regclass: RegClass) -> u32 {
        match regclass {
            RegClass::Int | RegClass::Float => 1,
            RegClass::Vector => panic!("AArch64 vector spills are unsupported"),
        }
    }

    fn spill_unit_bytes() -> u32 {
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

    fn gen_spill_store_at_sp(src: Reg, spill_off: i64, ty: LoweredType) -> SmallVec<[MInst; 4]> {
        smallvec![MInst::Store {
            ty: memory_type(ty),
            src,
            addr: AMode::SpOffset(spill_off),
        }]
    }

    fn gen_spill_load_at_sp(
        spill_off: i64,
        dst: Writable<Reg>,
        ty: LoweredType,
    ) -> SmallVec<[MInst; 4]> {
        smallvec![MInst::Load {
            ty: memory_type(ty),
            dst,
            addr: AMode::SpOffset(spill_off),
        }]
    }

    fn gen_load_imm(dst: Writable<Reg>, value: u64, ty: LoweredType) -> MInst {
        let size = match ty {
            I32 => OperandSize::Size32,
            I64 => OperandSize::Size64,
            _ => unreachable!("unsupported AArch64 immediate type: {ty:?}"),
        };
        MInst::LoadImm { size, dst, value }
    }

    fn gen_load_addr(dst: Writable<Reg>, label: taki_mir::prelude::HirInst) -> MInst {
        MInst::LoadAddr {
            dst,
            label: Label::from_global_inst(label),
        }
    }

    fn gen_get_stack_addr(mem: StackAMode, dst: Writable<Reg>) -> MInst {
        MInst::StackAddr {
            dst,
            addr: mem.into(),
        }
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

    fn compute_call_arg_loc(types: &[taki_mir::prelude::HirType]) -> (Vec<ArgSlot>, u32) {
        use raana_ir::ir::TypeKind;

        let mut slots = Vec::new();
        let (mut int_index, mut float_index, mut stack_offset) = (0usize, 0usize, 0u32);
        for ty in types {
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
                    ty: ty.clone(),
                });
            } else {
                slots.push(ArgSlot::Stack {
                    offset: i64::from(stack_offset),
                    ty: ty.clone(),
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
                addr: AMode::SpOffset(base - (index as i64 + 1) * 8),
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
                addr: AMode::SpOffset(base - (index as i64 + 1) * 8),
            });
        }
        insts
    }

    fn legalize_inst(frame: &FrameLayout, inst: MInst) -> SmallVec<[MInst; 4]> {
        match inst {
            MInst::StackAddr {
                dst,
                addr: AMode::FrameSlot(offset),
            } => {
                let mut insts = smallvec![];
                append_add_constant(
                    &mut insts,
                    dst,
                    Gpr::Sp,
                    i64::from(frame.outgoing_args_size) + offset,
                );
                insts.into_iter().collect()
            }
            MInst::Load { ty, dst, addr } => {
                let (addr, mut prefix) = legalize_amode(frame, addr, ty, None);
                prefix.push(MInst::Load { ty, dst, addr });
                prefix
            }
            MInst::Store { ty, src, addr } => {
                let (addr, mut prefix) = legalize_amode(frame, addr, ty, Some(src));
                prefix.push(MInst::Store { ty, src, addr });
                prefix
            }
            inst => smallvec![inst],
        }
    }

    fn gen_stack_to_stack_move(from: i64, to: i64) -> SmallVec<[MInst; 4]> {
        let scratch = Writable::from_reg(regs::int_reg(regs::INT_POST_RA_SCRATCH[0]));
        smallvec![
            MInst::Load {
                ty: MemoryType::I64,
                dst: scratch,
                addr: AMode::SpOffset(from),
            },
            MInst::Store {
                ty: MemoryType::I64,
                src: scratch.to_reg(),
                addr: AMode::SpOffset(to),
            },
        ]
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

/// Resolve frame-relative pseudo addresses after register allocation.  AArch64
/// has no arbitrary immediate memory form; retain directly encodable offsets
/// and otherwise materialize the address in declared post-RA integer scratches.
fn legalize_amode(
    frame: &FrameLayout,
    addr: AMode,
    ty: MemoryType,
    store_src: Option<Reg>,
) -> (AMode, SmallVec<[MInst; 4]>) {
    let (base, offset) = match addr {
        AMode::FrameSlot(offset) => (Gpr::Sp, i64::from(frame.outgoing_args_size) + offset),
        AMode::SpOffset(offset) => (Gpr::Sp, offset),
        AMode::OutgoingArg(offset) => (Gpr::Sp, offset),
        AMode::IncomingArg(offset) => (Gpr::Reg(regs::int_reg(regs::FP)), offset),
        addr => return (addr, smallvec![]),
    };

    if offset >= 0 {
        if let Some(offset) = UImm12Scaled::new(offset as u64, ty.byte_size()) {
            return (AMode::UnsignedOffset { base, offset }, smallvec![]);
        }
    }
    if let Some(offset) = SImm9::new(offset as i16).filter(|_| (-256..=255).contains(&offset)) {
        return (AMode::SignedOffset { base, offset }, smallvec![]);
    }

    let mut scratches = regs::INT_POST_RA_SCRATCH
        .into_iter()
        .map(regs::int_reg)
        .filter(|scratch| Some(*scratch) != store_src);
    let address = Writable::from_reg(scratches.next().expect("two integer post-RA scratches"));
    let offset_reg = Writable::from_reg(scratches.next().expect("two integer post-RA scratches"));
    let mut insts = materialize_integer_constant(offset as u64, OperandSize::Size64, offset_reg);
    let base = match base {
        Gpr::Reg(reg) => reg,
        Gpr::Sp => {
            insts.push(MInst::MovPhys {
                size: OperandSize::Size64,
                dst: Gpr::Reg(address.to_reg()),
                src: Gpr::Sp,
            });
            address.to_reg()
        }
        Gpr::Zr => unreachable!("stack address cannot use zero register as base"),
    };
    insts.push(MInst::AluRRR {
        op: AluOp::Add,
        size: OperandSize::Size64,
        dst: address,
        lhs: RegOrZr::Reg(base),
        rhs: RegOrZr::Reg(offset_reg.to_reg()),
    });
    (
        AMode::Reg {
            base: Gpr::Reg(address.to_reg()),
        },
        insts,
    )
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
        lhs: RegOrZr::Reg(base),
        rhs: RegOrZr::Reg(scratch.to_reg()),
    });
}
