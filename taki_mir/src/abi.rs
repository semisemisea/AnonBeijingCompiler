use std::marker::PhantomData;
use std::num::NonZeroU64;

use rustc_hash::FxHashMap;
use smallvec::{SmallVec, smallvec};
use tomori_utils::{PrimaryMap, SecondaryMap, entity_impl};

use crate::prelude::*;
use crate::reg_alloc::reg::{MachineEnv, PReg, RegClass};
use crate::register::{Reg, Writable};
use crate::types::LoweredType;
use crate::vcode::VCodeInst;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackAMode {
    /// Offset into the current frame's argument area.
    IncomingArg(i64, u32),
    /// Offset within the stack slots in the current frame.
    Slot(i64),
    /// Offset into the callee frame's argument area.
    OutgoingArg(i64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgSlot {
    Reg { reg: PReg, ty: HirType },
    Stack { offset: i64, ty: HirType },
}

impl StackAMode {
    fn offset_by(&self, offset: u32) -> Self {
        match self {
            StackAMode::IncomingArg(off, size) => {
                StackAMode::IncomingArg(off.checked_add(i64::from(offset)).unwrap(), *size)
            }
            StackAMode::Slot(off) => StackAMode::Slot(off.checked_add(i64::from(offset)).unwrap()),
            StackAMode::OutgoingArg(off) => {
                StackAMode::OutgoingArg(off.checked_add(i64::from(offset)).unwrap())
            }
        }
    }
}

pub trait ABIMachineSpec {
    type I: VCodeInst;

    fn word_bits() -> u32 {
        64
    }

    fn word_bytes() -> u32 {
        Self::word_bits() / 8
    }

    fn stack_align() -> u32;

    fn spillslot_size(_regclass: RegClass) -> u32;

    fn is_callee_saved(_preg: PReg) -> bool;

    fn gen_load_stack(mem: StackAMode, dst: Writable<Reg>, ty: LoweredType) -> Self::I;

    fn gen_load_imm(dst: Writable<Reg>, imm: i32) -> Self::I;

    fn gen_load_addr(dst: Writable<Reg>, label: HirInst) -> Self::I;

    fn gen_args(args: Vec<ArgPair>) -> Self::I;

    fn gen_ret() -> Self::I;

    fn gen_store_stack(src: Reg, mem: StackAMode, ty: LoweredType) -> Self::I;

    fn gen_spill_store(src: Reg, spill_off: i64, ty: LoweredType) -> SmallVec<[Self::I; 4]> {
        smallvec![Self::gen_store_stack(src, StackAMode::Slot(spill_off), ty)]
    }

    fn gen_spill_load(
        spill_off: i64,
        dst: Writable<Reg>,
        ty: LoweredType,
    ) -> SmallVec<[Self::I; 4]> {
        smallvec![Self::gen_load_stack(StackAMode::Slot(spill_off), dst, ty)]
    }

    fn gen_incoming_arg_load(
        fp_off: i64,
        dst: Writable<Reg>,
        ty: LoweredType,
    ) -> SmallVec<[Self::I; 4]> {
        smallvec![Self::gen_load_stack(
            StackAMode::IncomingArg(fp_off, 0),
            dst,
            ty,
        )]
    }

    fn gen_jump(block: HirBasicBlock) -> Self::I;

    fn gen_nop() -> Self::I;

    fn gen_move(src: Reg, dst: Reg, ty: LoweredType) -> Self::I;

    fn compute_arg_loc(arena: ArenaContext<'_>) -> (Vec<ArgSlot>, u32);

    fn get_machine_env() -> &'static MachineEnv;

    fn gen_prologue_frame_setup(frame: &FrameLayout) -> SmallVec<[Self::I; 16]>;

    fn gen_epilogue_frame_restore(frame: &FrameLayout) -> SmallVec<[Self::I; 16]>;

    fn gen_clobber_save(frame: &FrameLayout) -> SmallVec<[Self::I; 16]>;

    fn gen_clobber_restore(frame: &FrameLayout) -> SmallVec<[Self::I; 16]>;
}

#[derive(Debug, Clone)]
pub struct FrameLayout {
    pub callee_saved: Vec<PReg>,
    pub setup_area_size: u32,
    pub clobber_size: u32,
    pub spill_size: u32,
    pub stackslots_size: u32,
    pub outgoing_args_size: u32,
    pub total_size: u32,
}

#[derive(Debug, Clone)]
pub struct ArgPair {
    pub vreg: Writable<Reg>,
    pub preg: Reg,
}

#[derive(Debug, Clone)]
pub struct RetPair {
    pub vreg: Reg,
    pub preg: Reg,
}

#[derive(Debug, Clone)]
pub struct CallArgPair {
    pub vreg: Reg,
    pub preg: Reg,
}

#[derive(Debug, Clone)]
pub struct CallRetPair {
    pub vreg: Writable<Reg>,
    pub preg: Reg,
}

/// The function abstraction at ABI level.
/// Most of the data is not initialized correctly at constructor.
/// It will gradually build during the lowering process.
pub struct CalleeABI<M: ABIMachineSpec> {
    args: Vec<ArgSlot>,

    /// Await for filling.
    reg_args: Vec<ArgPair>,

    /// Await for filling.
    total_stackslots_size: u32,

    /// Await for filling
    stackslots_offsets: PrimaryMap<StackSlot, u32>,

    sized_stack_arg_size: u32,

    alloc_to_ss: FxHashMap<HirInst, u32>,

    /// Await for filling
    outgoing_arg_size: u32,

    has_calls: bool,

    /// ?
    stackslots_keys: SecondaryMap<StackSlot, Option<StackSlotUniqueKey>>,

    frame_layout: Option<FrameLayout>,

    /// Spill slot offsets for register arguments (indexed by param index)
    reg_arg_spillslots: Vec<i64>,

    _mach: PhantomData<M>,
}

impl<M: ABIMachineSpec> CalleeABI<M> {
    pub fn new(arena: ArenaContext<'_>) -> CalleeABI<M> {
        let (args, sized_stack_arg_size) = M::compute_arg_loc(arena);
        let num_args = args.len();
        CalleeABI {
            args,
            reg_args: vec![],
            total_stackslots_size: 0,
            sized_stack_arg_size,
            outgoing_arg_size: 0,
            has_calls: false,
            stackslots_offsets: PrimaryMap::new(),
            stackslots_keys: SecondaryMap::new(),
            alloc_to_ss: FxHashMap::default(),
            frame_layout: None,
            reg_arg_spillslots: vec![-1; num_args],
            _mach: PhantomData,
        }
    }

    pub fn allocate_stackslot(&mut self, ty: HirType) -> u32 {
        let align = M::stack_align();
        let ret = self.outgoing_arg_size + self.total_stackslots_size;
        let size = ty.size() as u32;
        let actual_size = size.next_multiple_of(align);
        self.total_stackslots_size += actual_size;
        ret
    }

    pub fn alloc_stackslot_or_get(&mut self, alloc: HirInst, ty: HirType) -> u32 {
        if let Some(&offset) = self.alloc_to_ss.get(&alloc) {
            return offset;
        }
        let offset = self.allocate_stackslot(ty);
        self.alloc_to_ss.insert(alloc, offset);
        offset
    }

    pub fn set_outgoing_arg_size(&mut self, size: usize) {
        self.outgoing_arg_size = self.outgoing_arg_size.max(size as u32);
    }

    pub fn set_has_calls(&mut self) {
        self.has_calls = true;
    }

    pub fn machine_env(&self) -> &MachineEnv {
        M::get_machine_env()
    }

    pub fn spillslot_size(&self, regclass: RegClass) -> u32 {
        M::spillslot_size(regclass)
    }

    pub fn compute_frame_layout(
        &mut self,
        spill_size: u32,
        output: &crate::reg_alloc::reg::Output,
    ) {
        let mut callee_saved: Vec<PReg> = output
            .allocs
            .iter()
            .filter_map(|a| a.as_reg())
            .filter(|p| M::is_callee_saved(*p))
            .collect();
        callee_saved.sort_unstable();
        callee_saved.dedup();

        let stackslots_size = self.total_stackslots_size;
        let outgoing_args_size = self.outgoing_arg_size;
        let clobber_size = callee_saved.len() as u32 * M::word_bytes();
        let setup_area_size = if self.has_calls
            || self.sized_stack_arg_size > 0
            || self.total_stackslots_size > 0
            || clobber_size > 0
            || spill_size > 0
        {
            2 * M::word_bytes()
        } else {
            0
        };
        let mut total =
            setup_area_size + clobber_size + spill_size + stackslots_size + outgoing_args_size;
        let align = M::stack_align();
        total = total.next_multiple_of(align);
        let layout = FrameLayout {
            callee_saved,
            setup_area_size,
            clobber_size,
            spill_size,
            stackslots_size,
            outgoing_args_size,
            total_size: total,
        };
        self.frame_layout = Some(layout);
    }

    /// Pre-allocate spill slots for all register arguments without
    /// emitting any instructions.
    pub fn prealloc_reg_arg_spills(&mut self) {
        let mut reg_indices: Vec<(usize, HirType)> = Vec::new();
        for (idx, slot) in self.args.iter().enumerate() {
            if let ArgSlot::Reg { ty, .. } = slot {
                reg_indices.push((idx, ty.clone()));
            }
        }
        for (idx, ty) in reg_indices {
            if self.reg_arg_spillslots[idx] < 0 {
                self.reg_arg_spillslots[idx] = self.allocate_stackslot(ty) as i64;
            }
        }
    }

    pub fn gen_store_reg_args_to_stack(&mut self) -> SmallVec<[M::I; 4]> {
        let mut insts = smallvec![];
        let mut reg_slots: Vec<(usize, PReg, HirType)> = Vec::new();
        for (idx, slot) in self.args.iter().enumerate() {
            if let ArgSlot::Reg { reg: preg, ty } = slot {
                reg_slots.push((idx, *preg, ty.clone()));
            }
        }
        for (idx, preg, ty) in reg_slots {
            if self.reg_arg_spillslots[idx] < 0 {
                self.reg_arg_spillslots[idx] = self.allocate_stackslot(ty.clone()) as i64;
            }
            for inst in M::gen_spill_store(preg.into(), self.reg_arg_spillslots[idx], ty.into()) {
                insts.push(inst);
            }
        }
        insts
    }

    pub fn gen_copy_arg_to_reg(&mut self, idx: usize, into_reg: Reg) -> SmallVec<[M::I; 4]> {
        let mut insts = smallvec![];
        match self.args[idx].clone() {
            ArgSlot::Reg { reg: preg, ty } => {
                if self.reg_arg_spillslots[idx] < 0 {
                    self.reg_arg_spillslots[idx] = self.allocate_stackslot(ty.clone()) as i64;
                    for inst in M::gen_spill_store(
                        preg.into(),
                        self.reg_arg_spillslots[idx],
                        ty.clone().into(),
                    ) {
                        insts.push(inst);
                    }
                }
                for inst in M::gen_spill_load(
                    self.reg_arg_spillslots[idx],
                    Writable::from_reg(into_reg),
                    ty.clone().into(),
                ) {
                    insts.push(inst);
                }
                let arg = ArgPair {
                    vreg: Writable::from_reg(into_reg),
                    preg: preg.into(),
                };
                self.reg_args.push(arg);
            }
            ArgSlot::Stack { offset, ty } => {
                for inst in
                    M::gen_incoming_arg_load(offset, Writable::from_reg(into_reg), ty.into())
                {
                    insts.push(inst);
                }
            }
        }
        insts
    }

    pub fn take_args(&mut self) -> Option<M::I> {
        if !self.reg_args.is_empty() {
            Some(M::gen_args(std::mem::take(&mut self.reg_args)))
        } else {
            None
        }
    }

    pub fn frame_layout(&self) -> &FrameLayout {
        self.frame_layout
            .as_ref()
            .expect("compute_frame_layout must be called before gen_prologue/gen_epilogue")
    }

    pub fn gen_prologue(&self) -> SmallVec<[M::I; 16]> {
        let frame = self.frame_layout();
        let mut insts = smallvec![];
        insts.extend(M::gen_prologue_frame_setup(frame));
        insts.extend(M::gen_clobber_save(frame));
        insts
    }

    pub fn gen_epilogue(&self) -> SmallVec<[M::I; 16]> {
        let frame = self.frame_layout();
        let mut insts = smallvec![];
        insts.extend(M::gen_clobber_restore(frame));
        insts.extend(M::gen_epilogue_frame_restore(frame));
        insts
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StackSlot(u32);
entity_impl!(StackSlot, "ss");

pub struct StackSlotData {
    pub size: u32,
    pub align_lg2: u8,
    pub key: Option<StackSlotUniqueKey>,
}

#[derive(Debug, Clone)]
pub struct StackSlotUniqueKey(NonZeroU64);

impl StackSlotUniqueKey {
    fn new(id: u64) -> Self {
        Self(NonZeroU64::new(id).unwrap())
    }

    fn bits(self) -> u64 {
        self.0.get()
    }
}
