use smallvec::{SmallVec, smallvec};

use taki_mir::{
    abi::{
        ABIMachineSpec, ArgLayoutPlanner, ArgPair, ArgRegBank, ArgSlot, FrameLayout, StackAMode,
    },
    reg_alloc::reg::{MachineEnv, PReg, PRegSet, RegClass},
    register::{Reg, Writable},
};

use crate::{
    instructions::{AMode, AluRRImm12OP, Imm12, LoadOP, MInst, StoreOP},
    labels::Label,
    regs::{
        ARG_REG, FARG_REG, fp_reg, link_reg, pf_reg, pv_reg, px_reg, spilltmp_reg, stack_reg,
        writable_fp_reg, writable_link_reg, writable_spilltmp_reg, writable_spilltmp_reg2,
        writable_stack_reg,
    },
};

pub struct Riscv64ABI;

impl ABIMachineSpec for Riscv64ABI {
    type I = MInst;

    fn stack_align() -> u32 {
        16
    }

    fn gen_load_stack(
        mem: taki_mir::abi::StackAMode,
        dst: Writable<taki_mir::register::Reg>,
        ty: taki_mir::types::LoweredType,
    ) -> Self::I {
        MInst::LoadWord {
            rd: dst,
            op: ty.into(),
            addr: mem.into(),
        }
    }

    fn spillslot_size(_regclass: RegClass) -> u32 {
        1
    }

    fn spill_unit_bytes() -> u32 {
        8
    }

    fn gen_load_imm(dst: Writable<Reg>, value: u64, ty: taki_mir::types::LoweredType) -> Self::I {
        match ty {
            taki_mir::types::I32 => MInst::LoadImm {
                rd: dst,
                value: (value as u32 as i32) as i64 as u64,
            },
            taki_mir::types::I64 => MInst::LoadImm { rd: dst, value },
            _ => unreachable!("unsupported RISC-V immediate type: {ty:?}"),
        }
    }

    fn gen_load_addr(dst: Writable<Reg>, gv: raana_ir::opt::prelude::Inst) -> Self::I {
        MInst::LoadAddr {
            rd: dst,
            label: Label::GlobalValue(gv),
        }
    }

    fn gen_get_stack_addr(mem: StackAMode, dst: Writable<Reg>) -> Self::I {
        MInst::StackAddr {
            rd: dst,
            addr: mem.into(),
        }
    }

    fn gen_args(args: Vec<ArgPair>) -> Self::I {
        MInst::Args { args }
    }

    fn gen_store_stack(
        src: taki_mir::register::Reg,
        mem: taki_mir::abi::StackAMode,
        ty: taki_mir::types::LoweredType,
    ) -> Self::I {
        MInst::StoreWord {
            rs: src,
            op: ty.into(),
            addr: mem.into(),
        }
    }

    fn gen_spill_store(
        src: taki_mir::register::Reg,
        spill_off: i64,
        ty: taki_mir::types::LoweredType,
    ) -> SmallVec<[MInst; 4]> {
        let mut insts: SmallVec<[MInst; 4]> = smallvec![];
        let (addr, extras) = AMode::SPOffset(spill_off).normalize_imm12();
        for inst in extras {
            insts.push(inst);
        }
        insts.push(MInst::StoreWord {
            rs: src,
            op: ty.into(),
            addr,
        });
        insts
    }

    fn gen_spill_load(
        spill_off: i64,
        dst: Writable<taki_mir::register::Reg>,
        ty: taki_mir::types::LoweredType,
    ) -> SmallVec<[MInst; 4]> {
        let mut insts: SmallVec<[MInst; 4]> = smallvec![];
        let (addr, extras) = AMode::SPOffset(spill_off).normalize_imm12();
        for inst in extras {
            insts.push(inst);
        }
        insts.push(MInst::LoadWord {
            rd: dst,
            op: ty.into(),
            addr,
        });
        insts
    }

    fn gen_stack_to_stack_move(from: i64, to: i64) -> SmallVec<[MInst; 4]> {
        let mut insts = Self::gen_spill_load(from, writable_spilltmp_reg(), taki_mir::types::I64);
        insts.extend(Self::gen_spill_store(
            spilltmp_reg(),
            to,
            taki_mir::types::I64,
        ));
        insts
    }

    fn gen_incoming_arg_load(
        fp_off: i64,
        dst: Writable<taki_mir::register::Reg>,
        ty: taki_mir::types::LoweredType,
    ) -> SmallVec<[MInst; 4]> {
        let mut insts: SmallVec<[MInst; 4]> = smallvec![];
        let (addr, extras) = AMode::IncomingArg(fp_off).normalize_imm12();
        for inst in extras {
            insts.push(inst);
        }
        insts.push(MInst::LoadWord {
            rd: dst,
            op: ty.into(),
            addr,
        });
        insts
    }

    fn gen_move(
        src: taki_mir::register::Reg,
        dst: taki_mir::register::Reg,
        _ty: taki_mir::types::LoweredType,
    ) -> Self::I {
        MInst::Mov {
            src,
            dst: Writable::from_reg(dst),
        }
    }

    fn compute_call_arg_loc(types: &[raana_ir::ir::Type]) -> (Vec<ArgSlot>, u32) {
        use raana_ir::ir::TypeKind;

        ArgLayoutPlanner::new(&ARG_REG, &FARG_REG).compute(
            types,
            |ty| match ty.kind() {
                TypeKind::Int32 | TypeKind::Pointer(_) => ArgRegBank::Int,
                TypeKind::Float32 => ArgRegBank::Float,
                kind => panic!("unsupported RISC-V scalar argument type: {kind:?}"),
            },
            // psABI (riscv-cc.adoc, Integer Calling Convention): scalars
            // narrower than XLEN are widened to XLEN bits when passed on the
            // stack, so every RV64 stack argument slot is 8 bytes and 8-aligned.
            // Packing by ty.size() would put a 64-bit argument after a 32-bit
            // one at a 4-mod-8 offset, trapping on BOOM (QEMU silently allows
            // unaligned accesses). All current scalar parameter types fit in
            // one XLEN slot; re-evaluate if a wider scalar type is added.
            |_| Self::word_bytes(),
        )
    }

    fn get_machine_env() -> &'static MachineEnv {
        static MACHINE_ENV: std::sync::LazyLock<MachineEnv> =
            std::sync::LazyLock::new(create_reg_environment);
        &MACHINE_ENV
    }

    fn is_callee_saved(preg: PReg) -> bool {
        match preg.class() {
            RegClass::Int => matches!(preg.hw_enc(), 8 | 9 | 18..=27),
            RegClass::Float => matches!(preg.hw_enc(), 8 | 9 | 18..=27),
            _ => false,
        }
    }

    fn gen_prologue_frame_setup(frame: &FrameLayout) -> SmallVec<[MInst; 16]> {
        let total = i64::from(frame.total_size);
        let mut insts = smallvec![];
        if frame.setup_area_size > 0 {
            let base = frame.total_size as i64;
            sp_adjust(&mut insts, -total);
            store_stack_imm12(&mut insts, link_reg(), StoreOP::Sd, base - 8);
            store_stack_imm12(&mut insts, fp_reg(), StoreOP::Sd, base - 16);
            reg_add_imm(&mut insts, writable_fp_reg(), stack_reg(), base);
        } else if total > 0 {
            sp_adjust(&mut insts, -total);
        }
        insts
    }

    fn gen_epilogue_frame_restore(frame: &FrameLayout) -> SmallVec<[MInst; 16]> {
        let mut insts = smallvec![];
        if frame.setup_area_size > 0 {
            let base = frame.total_size as i64;
            load_stack_imm12(&mut insts, writable_link_reg(), LoadOP::Ld, base - 8);
            load_stack_imm12(&mut insts, writable_fp_reg(), LoadOP::Ld, base - 16);
        }
        if frame.total_size > 0 {
            sp_adjust(&mut insts, i64::from(frame.total_size));
        }
        insts
    }

    fn gen_clobber_save(frame: &FrameLayout) -> SmallVec<[MInst; 16]> {
        let mut insts = smallvec![];
        let base = frame.total_size as i64 - frame.setup_area_size as i64;
        for (i, preg) in frame.callee_saved.iter().enumerate() {
            let offset = base - (i as i64 + 1) * 8;
            let rs = Reg::from_physical_reg(*preg);
            let op = match preg.class() {
                RegClass::Int => StoreOP::Sd,
                _ => StoreOP::Fsw,
            };
            store_stack_imm12(&mut insts, rs, op, offset);
        }
        insts
    }

    fn gen_clobber_restore(frame: &FrameLayout) -> SmallVec<[MInst; 16]> {
        let mut insts = smallvec![];
        let base = frame.total_size as i64 - frame.setup_area_size as i64;
        for (i, preg) in frame.callee_saved.iter().enumerate() {
            let offset = base - (i as i64 + 1) * 8;
            let rd = Writable::from_reg(Reg::from_physical_reg(*preg));
            let op = match preg.class() {
                RegClass::Int => LoadOP::Ld,
                _ => LoadOP::Flw,
            };
            load_stack_imm12(&mut insts, rd, op, offset);
        }
        insts
    }

    fn legalize_inst(frame: &FrameLayout, inst: MInst) -> SmallVec<[MInst; 4]> {
        match inst {
            MInst::StackAddr {
                rd,
                addr: AMode::SlotOffset(offset),
            } => {
                let offset = offset + i64::from(frame.outgoing_args_size);
                if let Some(imm) = i32::try_from(offset).ok().and_then(Imm12::from_i32) {
                    smallvec![MInst::AluRRImm12 {
                        op: AluRRImm12OP::Addi,
                        rd,
                        rs: stack_reg(),
                        imm,
                    }]
                } else {
                    smallvec![
                        MInst::LoadImm {
                            rd: writable_spilltmp_reg2(),
                            value: offset as u64,
                        },
                        MInst::AluRRR {
                            op: crate::instructions::AluRRROP::Add,
                            rd,
                            rs1: stack_reg(),
                            rs2: writable_spilltmp_reg2().to_reg(),
                        },
                    ]
                }
            }
            MInst::LoadWord {
                rd,
                op,
                addr: AMode::SlotOffset(offset),
            } => {
                let (addr, mut insts) = legalize_slot_amode(frame, offset);
                insts.push(MInst::LoadWord { rd, op, addr });
                insts
            }
            MInst::StoreWord {
                rs,
                op,
                addr: AMode::SlotOffset(offset),
            } => {
                let (addr, mut insts) = legalize_slot_amode(frame, offset);
                insts.push(MInst::StoreWord { rs, op, addr });
                insts
            }
            inst => smallvec![inst],
        }
    }
}

fn legalize_slot_amode(frame: &FrameLayout, offset: i64) -> (AMode, SmallVec<[MInst; 4]>) {
    let offset = offset + i64::from(frame.outgoing_args_size);
    if i32::try_from(offset)
        .ok()
        .and_then(Imm12::from_i32)
        .is_some()
    {
        return (AMode::SPOffset(offset), smallvec![]);
    }

    let address = writable_spilltmp_reg();
    let offset_reg = writable_spilltmp_reg2();
    (
        AMode::RegOffest(address.to_reg(), 0),
        smallvec![
            MInst::LoadImm {
                rd: offset_reg,
                value: offset as u64,
            },
            MInst::AluRRR {
                op: crate::instructions::AluRRROP::Add,
                rd: address,
                rs1: stack_reg(),
                rs2: offset_reg.to_reg(),
            },
        ],
    )
}

fn sp_adjust(insts: &mut SmallVec<[MInst; 16]>, amount: i64) {
    if amount == 0 {
        return;
    }
    if let Some(imm) = i32::try_from(amount).ok().and_then(Imm12::from_i32) {
        insts.push(MInst::AluRRImm12 {
            op: AluRRImm12OP::Addi,
            rd: writable_stack_reg(),
            rs: stack_reg(),
            imm,
        });
    } else {
        let tmp = writable_spilltmp_reg();
        insts.push(MInst::LoadImm {
            rd: tmp,
            value: amount as u64,
        });
        insts.push(MInst::AluRRR {
            op: crate::instructions::AluRRROP::Add,
            rd: writable_stack_reg(),
            rs1: stack_reg(),
            rs2: tmp.to_reg(),
        });
    }
}

fn reg_add_imm(insts: &mut SmallVec<[MInst; 16]>, rd: Writable<Reg>, rs: Reg, amount: i64) {
    if let Some(imm) = i32::try_from(amount).ok().and_then(Imm12::from_i32) {
        insts.push(MInst::AluRRImm12 {
            op: AluRRImm12OP::Addi,
            rd,
            rs,
            imm,
        });
    } else {
        let tmp = writable_spilltmp_reg2();
        insts.push(MInst::LoadImm {
            rd: tmp,
            value: amount as u64,
        });
        insts.push(MInst::AluRRR {
            op: crate::instructions::AluRRROP::Add,
            rd,
            rs1: rs,
            rs2: tmp.to_reg(),
        });
    }
}

fn store_stack_imm12(insts: &mut SmallVec<[MInst; 16]>, rs: Reg, op: StoreOP, sp_offset: i64) {
    let (addr, extras) = AMode::SPOffset(sp_offset).normalize_imm12();
    for inst in extras {
        insts.push(inst);
    }
    insts.push(MInst::StoreWord { rs, op, addr });
}

fn load_stack_imm12(
    insts: &mut SmallVec<[MInst; 16]>,
    rd: Writable<Reg>,
    op: LoadOP,
    sp_offset: i64,
) {
    let (addr, extras) = AMode::SPOffset(sp_offset).normalize_imm12();
    for inst in extras {
        insts.push(inst);
    }
    insts.push(MInst::LoadWord { rd, op, addr });
}

pub const DEFAULT_CLOBBERS: PRegSet = PRegSet::empty()
    .with(px_reg(1))
    .with(px_reg(5))
    .with(px_reg(6))
    .with(px_reg(7))
    .with(px_reg(10))
    .with(px_reg(11))
    .with(px_reg(12))
    .with(px_reg(13))
    .with(px_reg(14))
    .with(px_reg(15))
    .with(px_reg(16))
    .with(px_reg(17))
    .with(px_reg(28))
    .with(px_reg(29))
    .with(px_reg(30))
    .with(px_reg(31))
    // F Regs
    .with(pf_reg(0))
    .with(pf_reg(1))
    .with(pf_reg(2))
    .with(pf_reg(3))
    .with(pf_reg(4))
    .with(pf_reg(5))
    .with(pf_reg(6))
    .with(pf_reg(7))
    .with(pf_reg(9))
    .with(pf_reg(10))
    .with(pf_reg(11))
    .with(pf_reg(12))
    .with(pf_reg(13))
    .with(pf_reg(14))
    .with(pf_reg(15))
    .with(pf_reg(16))
    .with(pf_reg(17))
    .with(pf_reg(28))
    .with(pf_reg(29))
    .with(pf_reg(30))
    .with(pf_reg(31))
    // V Regs - All vector regs get clobbered
    .with(pv_reg(0))
    .with(pv_reg(1))
    .with(pv_reg(2))
    .with(pv_reg(3))
    .with(pv_reg(4))
    .with(pv_reg(5))
    .with(pv_reg(6))
    .with(pv_reg(7))
    .with(pv_reg(8))
    .with(pv_reg(9))
    .with(pv_reg(10))
    .with(pv_reg(11))
    .with(pv_reg(12))
    .with(pv_reg(13))
    .with(pv_reg(14))
    .with(pv_reg(15))
    .with(pv_reg(16))
    .with(pv_reg(17))
    .with(pv_reg(18))
    .with(pv_reg(19))
    .with(pv_reg(20))
    .with(pv_reg(21))
    .with(pv_reg(22))
    .with(pv_reg(23))
    .with(pv_reg(24))
    .with(pv_reg(25))
    .with(pv_reg(26))
    .with(pv_reg(27))
    .with(pv_reg(28))
    .with(pv_reg(29))
    .with(pv_reg(30))
    .with(pv_reg(31));

fn create_reg_environment() -> MachineEnv {
    // Some C Extension instructions can only use a subset of the registers.
    // x8 - x15, f8 - f15, v8 - v15 so we should prefer to use those since
    // they allow us to emit C instructions more often.
    //
    // In general the order of preference is:
    //   1. Compressible Caller Saved registers.
    //   2. Non-Compressible Caller Saved registers.
    //   3. Compressible Callee Saved registers.
    //   4. Non-Compressible Callee Saved registers.

    let preferred_regs_by_class: [PRegSet; 3] = [
        PRegSet::empty()
            .with(px_reg(10))
            .with(px_reg(11))
            .with(px_reg(12))
            .with(px_reg(13))
            .with(px_reg(14))
            .with(px_reg(15)),
        PRegSet::empty()
            .with(pf_reg(10))
            .with(pf_reg(11))
            .with(pf_reg(12))
            .with(pf_reg(13))
            .with(pf_reg(14))
            .with(pf_reg(15)),
        PRegSet::empty()
            .with(pv_reg(8))
            .with(pv_reg(9))
            .with(pv_reg(10))
            .with(pv_reg(11))
            .with(pv_reg(12))
            .with(pv_reg(13))
            .with(pv_reg(14))
            .with(pv_reg(15)),
    ];

    let non_preferred_regs_by_class: [PRegSet; 3] = [
        // x0 - x4 are special registers, so we don't want to use them.
        // Omit x30 and x31 since they are the spilltmp registers.
        PRegSet::empty()
            .with(px_reg(5))
            .with(px_reg(6))
            .with(px_reg(7))
            // Start with the Non-Compressible Caller Saved registers.
            .with(px_reg(16))
            .with(px_reg(17))
            .with(px_reg(28))
            .with(px_reg(29))
            // The first Callee Saved register is x9 since its Compressible
            // Omit x8 since it's the frame pointer.
            .with(px_reg(9))
            // The rest of the Callee Saved registers are Non-Compressible
            .with(px_reg(18))
            .with(px_reg(19))
            .with(px_reg(20))
            .with(px_reg(21))
            .with(px_reg(22))
            .with(px_reg(23))
            .with(px_reg(24))
            .with(px_reg(25))
            .with(px_reg(26))
            .with(px_reg(27)),
        // Prefer Caller Saved registers.
        PRegSet::empty()
            .with(pf_reg(0))
            .with(pf_reg(1))
            .with(pf_reg(2))
            .with(pf_reg(3))
            .with(pf_reg(4))
            .with(pf_reg(5))
            .with(pf_reg(6))
            .with(pf_reg(7))
            .with(pf_reg(16))
            .with(pf_reg(17))
            .with(pf_reg(28))
            .with(pf_reg(29))
            .with(pf_reg(30))
            .with(pf_reg(31))
            // Once those are exhausted, we should prefer f8 and f9 since they are
            // callee saved, but compressible.
            .with(pf_reg(8))
            .with(pf_reg(9))
            .with(pf_reg(18))
            .with(pf_reg(19))
            .with(pf_reg(20))
            .with(pf_reg(21))
            .with(pf_reg(22))
            .with(pf_reg(23))
            .with(pf_reg(24))
            .with(pf_reg(25))
            .with(pf_reg(26))
            .with(pf_reg(27)),
        PRegSet::empty()
            .with(pv_reg(0))
            .with(pv_reg(1))
            .with(pv_reg(2))
            .with(pv_reg(3))
            .with(pv_reg(4))
            .with(pv_reg(5))
            .with(pv_reg(6))
            .with(pv_reg(7))
            .with(pv_reg(16))
            .with(pv_reg(17))
            .with(pv_reg(18))
            .with(pv_reg(19))
            .with(pv_reg(20))
            .with(pv_reg(21))
            .with(pv_reg(22))
            .with(pv_reg(23))
            .with(pv_reg(24))
            .with(pv_reg(25))
            .with(pv_reg(26))
            .with(pv_reg(27))
            .with(pv_reg(28))
            .with(pv_reg(29))
            .with(pv_reg(30))
            .with(pv_reg(31)),
    ];

    MachineEnv {
        preferred_regs_by_class,
        non_preferred_regs_by_class,
        fixed_stack_slots: vec![],
        scratch_by_class: [Some(px_reg(31)), None, None],
        post_ra_scratch_by_class: [vec![px_reg(30), px_reg(31)], vec![], vec![]],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        instructions::{AMode, LoadOP, MInst, StoreOP},
        regs::{ARG_REG, FARG_REG, px_reg},
    };
    use raana_ir::ir::Type as HirType;
    use taki_mir::reg_alloc::reg::PRegSet;
    use taki_mir::types::I32;

    #[test]
    fn i32_negative_immediates_are_sign_extended() {
        let inst = Riscv64ABI::gen_load_imm(
            Writable::from_reg(Reg::from_physical_reg(px_reg(5))),
            (-1_i32) as u32 as u64,
            I32,
        );

        let MInst::LoadImm { value, .. } = inst else {
            panic!("expected an immediate load");
        };
        assert_eq!(value, u64::MAX);
    }

    #[test]
    fn stack_to_stack_spill_move_uses_the_reserved_scratch_register() {
        let insts = Riscv64ABI::gen_stack_to_stack_move(16, 24);

        assert_eq!(insts.len(), 2);
        assert!(matches!(
            insts[0],
            MInst::LoadWord {
                rd,
                op: LoadOP::Ld,
                addr: AMode::SPOffset(16),
            } if rd.to_reg() == spilltmp_reg()
        ));
        assert!(matches!(
            insts[1],
            MInst::StoreWord {
                rs,
                op: StoreOP::Sd,
                addr: AMode::SPOffset(24),
            } if rs == spilltmp_reg()
        ));
    }

    #[test]
    fn machine_environment_reserves_abi_and_frame_offset_scratch_registers() {
        let env = create_reg_environment();
        let allocatable = PRegSet::from(&env);

        for preg in [px_reg(1), px_reg(2), px_reg(8), px_reg(30), px_reg(31)] {
            assert!(!allocatable.contains(preg), "{preg:?} must be reserved");
        }
        assert_eq!(env.scratch_by_class[0], Some(px_reg(31)));
        assert_eq!(
            env.post_ra_scratch_by_class[0],
            vec![px_reg(30), px_reg(31)]
        );
    }

    #[test]
    fn argument_layout_uses_independent_register_banks() {
        let types = vec![HirType::get_i32(); 8]
            .into_iter()
            .chain(vec![HirType::get_f32(); 8])
            .collect::<Vec<_>>();
        let (locations, stack_size) = Riscv64ABI::compute_call_arg_loc(&types);

        assert_eq!(stack_size, 0);
        assert!(matches!(
            locations[7],
            ArgSlot::Reg { reg, .. } if reg == ARG_REG[7].to_physical_reg().unwrap()
        ));
        assert!(matches!(
            locations[15],
            ArgSlot::Reg { reg, .. } if reg == FARG_REG[7].to_physical_reg().unwrap()
        ));
    }

    #[test]
    fn argument_layout_uses_fixed_eight_byte_stack_slots() {
        let mut types = vec![HirType::get_pointer(HirType::get_i32()); 9];
        types.extend(vec![HirType::get_i32(); 1]);
        types.extend(vec![HirType::get_f32(); 9]);
        types.extend(vec![HirType::get_pointer(HirType::get_i32()); 1]);
        let (locations, stack_size) = Riscv64ABI::compute_call_arg_loc(&types);
        let stack_offsets: Vec<_> = locations
            .iter()
            .filter_map(|location| match location {
                ArgSlot::Stack { offset, .. } => Some(*offset),
                ArgSlot::Reg { .. } => None,
            })
            .collect();

        assert_eq!(stack_offsets, vec![0, 8, 16, 24]);
        assert_eq!(stack_size, 32);
    }
}
