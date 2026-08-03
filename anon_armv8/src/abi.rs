//! AAPCS64 calling convention and frame hooks for AArch64.

use smallvec::{SmallVec, smallvec};
use taki_mir::{
    abi::{
        ABIMachineSpec, ArgLayoutPlanner, ArgPair, ArgRegBank, ArgSlot, FrameLayout, StackAMode,
    },
    reg_alloc::reg::{MachineEnv, PReg, RegClass},
    register::{Reg, Writable},
    types::{F32, I32, I64, LoweredType},
};

use crate::{
    constants::materialize_integer_constant,
    instructions::{
        AMode, AluOp, ExtendOp, Imm12, MInst, MemoryType, PairAMode, SImm7Scaled, SImm9,
        UImm12Scaled,
    },
    labels::Label,
    regs::{self, OperandSize, RegOrZr},
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
            // A 128-bit vector occupies two 8-byte spill units.
            RegClass::Vector => 2,
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

    fn gen_args(args: Vec<ArgPair>) -> MInst {
        MInst::Args { args }
    }

    fn gen_move(src: Reg, dst: Reg, ty: LoweredType) -> MInst {
        if ty.is_vector() {
            MInst::VecMov {
                dst: Writable::from_reg(dst),
                src,
            }
        } else if ty == F32 {
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

    fn compute_call_arg_loc(types: &[taki_mir::prelude::HirType]) -> (Vec<ArgSlot>, u32) {
        use raana_ir::ir::TypeKind;

        ArgLayoutPlanner::with_vector_regs(
            &regs::INT_ARG_REGS,
            &regs::FLOAT_ARG_REGS,
            &regs::VECTOR_ARG_REGS,
        )
        .compute(
            types,
            |ty| match ty.kind() {
                TypeKind::Float32 => ArgRegBank::Float,
                TypeKind::Vector(..) => ArgRegBank::Vector,
                TypeKind::Int32 | TypeKind::Pointer(_) | TypeKind::String => ArgRegBank::Int,
                _ => unreachable!("non-scalar AAPCS64 parameter: {:?}", ty.kind()),
            },
            |ty| if ty.is_vector() { 16 } else { 8 },
        )
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
                    base: regs::stack_reg(),
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
                regs::stack_reg(),
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
                    base: regs::stack_reg(),
                    offset: SImm7Scaled::new(16, 8).unwrap(),
                },
            });
        }
        insts
    }

    fn gen_clobber_save(frame: &FrameLayout) -> SmallVec<[MInst; 16]> {
        let mut insts = smallvec![];
        let base = i64::from(frame.total_size - frame.setup_area_size);
        let mut offset = 0i64;
        for preg in &frame.callee_saved {
            let size = i64::from(clobber_slot_size(*preg));
            if size > 8 {
                offset = (offset + size - 1) & !(size - 1);
            }
            offset += size;
            insts.push(MInst::Store {
                ty: clobber_memory_type(*preg),
                src: Reg::from_physical_reg(*preg),
                addr: AMode::SpOffset(base - offset),
            });
        }
        insts
    }

    fn gen_clobber_restore(frame: &FrameLayout) -> SmallVec<[MInst; 16]> {
        let mut insts = smallvec![];
        let base = i64::from(frame.total_size - frame.setup_area_size);
        let mut offset = 0i64;
        for preg in &frame.callee_saved {
            let size = i64::from(clobber_slot_size(*preg));
            if size > 8 {
                offset = (offset + size - 1) & !(size - 1);
            }
            offset += size;
            insts.push(MInst::Load {
                ty: clobber_memory_type(*preg),
                dst: Writable::from_reg(Reg::from_physical_reg(*preg)),
                addr: AMode::SpOffset(base - offset),
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
                    regs::stack_reg(),
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
    if ty.is_vector() {
        MemoryType::Vec128
    } else if ty == F32 {
        MemoryType::F32
    } else if ty == I32 {
        MemoryType::I32
    } else {
        MemoryType::I64
    }
}

fn clobber_slot_size(preg: PReg) -> u32 {
    match preg.class() {
        // v8-v15 are 128-bit NEON registers when used for vectors.
        RegClass::Vector => 16,
        _ => 8,
    }
}

fn clobber_memory_type(preg: PReg) -> MemoryType {
    match preg.class() {
        RegClass::Float => MemoryType::F64,
        RegClass::Vector => MemoryType::Vec128,
        _ => MemoryType::I64,
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
        AMode::FrameSlot(offset) => (
            regs::stack_reg(),
            i64::from(frame.outgoing_args_size) + offset,
        ),
        AMode::SpOffset(offset) => (regs::stack_reg(), offset),
        AMode::OutgoingArg(offset) => (regs::stack_reg(), offset),
        AMode::IncomingArg(offset) => (regs::int_reg(regs::FP), offset),
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
    // SP cannot be used as a base in the shifted-register `AluRRR` form
    // (encoding 31 denotes XZR there); copy it to a scratch first.
    let base = if base == regs::stack_reg() {
        insts.push(MInst::MovPhys {
            size: OperandSize::Size64,
            dst: address,
            src: regs::stack_reg(),
        });
        address.to_reg()
    } else {
        base
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
            base: address.to_reg(),
        },
        insts,
    )
}

/// Adjust the stack pointer by a signed amount. Mirrors cranelift's
/// `gen_sp_reg_adjust`: emit a single `add/sub sp, sp, #imm12` whenever the
/// magnitude fits an [`Imm12`] (possibly with `lsl #12`), and otherwise
/// materialize the constant in a post-RA scratch and use the extended-register
/// form `add/sub sp, sp, tmp, uxtx`.
fn append_sp_adjust(insts: &mut SmallVec<[MInst; 16]>, amount: i64) {
    if amount == 0 {
        return;
    }
    let (abs, op) = if amount < 0 {
        (amount.unsigned_abs(), AluOp::Sub)
    } else {
        (amount as u64, AluOp::Add)
    };
    if let Some(imm) = Imm12::maybe_from_u64(abs) {
        insts.push(MInst::AluRRImm12 {
            op,
            size: OperandSize::Size64,
            dst: regs::writable_stack_reg(),
            src: regs::stack_reg(),
            imm,
        });
        return;
    }
    let tmp = Writable::from_reg(regs::int_reg(regs::INT_POST_RA_SCRATCH[0]));
    insts.extend(materialize_integer_constant(abs, OperandSize::Size64, tmp));
    insts.push(MInst::AluRRRExtend {
        op,
        size: OperandSize::Size64,
        dst: regs::writable_stack_reg(),
        lhs: regs::stack_reg(),
        rhs: tmp.to_reg(),
        extend: ExtendOp::Uxtx,
        shift: 0,
    });
}

fn append_add_constant(
    insts: &mut SmallVec<[MInst; 16]>,
    dst: Writable<Reg>,
    base: Reg,
    amount: i64,
) {
    if amount >= 0 {
        if let Some(imm) = Imm12::maybe_from_u64(amount as u64) {
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
    // SP cannot be the base of a shifted-register `AluRRR` (encoding 31
    // denotes XZR there); copy it to a scratch first.
    let base = if base == regs::stack_reg() {
        let copy = Writable::from_reg(regs::int_reg(17));
        insts.push(MInst::MovPhys {
            size: OperandSize::Size64,
            dst: copy,
            src: regs::stack_reg(),
        });
        copy.to_reg()
    } else {
        base
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

#[cfg(test)]
mod tests {
    use super::*;
    use taki_mir::prelude::HirType;

    #[test]
    fn aapcs64_argument_layout_uses_independent_register_banks() {
        let types = vec![HirType::get_i32(); 8]
            .into_iter()
            .chain(vec![HirType::get_f32(); 8])
            .collect::<Vec<_>>();
        let (locations, stack_size) = AArch64Abi::compute_call_arg_loc(&types);

        assert_eq!(stack_size, 0);
        assert!(matches!(
            locations[7],
            ArgSlot::Reg { reg, .. } if reg == regs::int_preg(7)
        ));
        assert!(matches!(
            locations[15],
            ArgSlot::Reg { reg, .. } if reg == regs::float_preg(7)
        ));
    }

    #[test]
    fn aapcs64_overflow_arguments_use_fixed_eight_byte_slots() {
        let mut types = vec![HirType::get_i32(); 9];
        types.extend(vec![HirType::get_f32(); 9]);
        types.push(HirType::get_string());
        let (locations, stack_size) = AArch64Abi::compute_call_arg_loc(&types);
        let stack_offsets: Vec<_> = locations
            .iter()
            .filter_map(|location| match location {
                ArgSlot::Stack { offset, .. } => Some(*offset),
                ArgSlot::Reg { .. } => None,
            })
            .collect();

        assert_eq!(stack_offsets, vec![0, 8, 16]);
        assert_eq!(stack_size, 24);
    }

    #[test]
    fn aapcs64_vector_arguments_use_neon_registers() {
        let vec = HirType::get_vector(HirType::get_i32(), 4);
        let types = vec![vec.clone(); 9];
        let (locations, stack_size) = AArch64Abi::compute_call_arg_loc(&types);

        // First eight vectors land in NEON v0-v7, the ninth overflows to a
        // 16-byte stack slot.
        for (index, location) in locations.iter().enumerate().take(8) {
            assert!(
                matches!(
                    location,
                    ArgSlot::Reg { reg, .. } if *reg == regs::vector_preg(index as u8)
                ),
                "vector arg {index}: {location:?}"
            );
        }
        assert!(matches!(
            locations[8],
            ArgSlot::Stack { offset: 0, .. }
        ));
        assert_eq!(stack_size, 16);
    }

    #[test]
    fn aapcs64_vector_overflow_slots_are_sixteen_bytes() {
        let vec = HirType::get_vector(HirType::get_i32(), 4);
        let types = vec![
            HirType::get_i32(),
            vec.clone(),
            vec.clone(),
            HirType::get_i32(),
        ];
        let (locations, stack_size) = AArch64Abi::compute_call_arg_loc(&types);

        // int -> x0, vec -> v0, vec -> v1, int -> x1: all registers, no stack.
        assert_eq!(stack_size, 0);
        assert!(matches!(locations[0], ArgSlot::Reg { reg, .. } if reg == regs::int_preg(0)));
        assert!(matches!(locations[1], ArgSlot::Reg { reg, .. } if reg == regs::vector_preg(0)));
        assert!(matches!(locations[2], ArgSlot::Reg { reg, .. } if reg == regs::vector_preg(1)));
        assert!(matches!(locations[3], ArgSlot::Reg { reg, .. } if reg == regs::int_preg(1)));
    }
}
