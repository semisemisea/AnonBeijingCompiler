use std::marker::PhantomData;
use std::num::NonZeroU64;

use smallvec::{SmallVec, smallvec};
use tomori_utils::{PrimaryMap, SecondaryMap, entity_impl};

use crate::prelude::*;
use crate::reg_alloc::reg::{MachineEnv, PReg, VReg};
use crate::register::{Reg, VRegAllocator};
use crate::types::Type;
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

    fn gen_load_stack(mem: StackAMode, dst: Reg, ty: Type) -> Self::I;

    fn gen_args(args: Vec<ArgPair>) -> Self::I;

    fn gen_ret() -> Self::I;

    fn gen_store_stack(src: Reg, mem: StackAMode, ty: Type) -> Self::I;

    fn gen_jump(block: HirBasicBlock) -> Self::I;

    fn gen_branch() -> Self::I;

    fn gen_nop() -> Self::I;

    fn gen_move(src: Reg, dst: Reg, ty: Type) -> Self::I;

    fn compute_arg_loc(arena: ArenaContext<'_>, params: &[HirInst]);

    fn get_machine_env() -> &'static MachineEnv;
}

pub struct ArgPair {
    vreg: Reg,
    preg: Reg,
}

/// The function abstraction at ABI level.
/// Most of the data is not initialized correctly at constructor.
/// It will gradually build during the lowering process.
pub struct CalleeABI<M: ABIMachineSpec> {
    args: Vec<ArgSlot>,

    reg_args: Vec<ArgPair>,

    total_stackslots_size: u32,

    stackslots_offsets: PrimaryMap<StackSlot, u32>,

    sized_stack_arg_size: u32,

    outgoing_arg_size: u32,

    stackslots_keys: SecondaryMap<StackSlot, Option<StackSlotUniqueKey>>,

    _mach: PhantomData<M>,
}

impl<M: ABIMachineSpec> CalleeABI<M> {
    pub fn new(func: &HirFunctionData) -> CalleeABI<M> {
        // TODO: Calculate the total stack size.
        CalleeABI {
            args: todo!(),
            reg_args: todo!(),
            total_stackslots_size: 0,
            sized_stack_arg_size: todo!(),
            outgoing_arg_size: 0,
            stackslots_offsets: PrimaryMap::new(),
            stackslots_keys: SecondaryMap::new(),
            _mach: PhantomData,
        }
    }

    pub fn machine_env(&self) -> &MachineEnv {
        M::get_machine_env()
    }

    pub fn gen_copy_arg_to_reg(&mut self, idx: usize, into_reg: Reg) -> SmallVec<[M::I; 4]> {
        let mut insts = smallvec![];
        let mut copy_arg_to_reg = |slot: &ArgSlot, into_reg: Reg| match slot {
            ArgSlot::Reg { reg: preg, .. } => {
                let arg = ArgPair {
                    vreg: into_reg.into(),
                    preg: (*preg).into(),
                };
                self.reg_args.push(arg);
            }
            ArgSlot::Stack { offset, ty } => insts.push(M::gen_load_stack(
                StackAMode::IncomingArg(*offset, self.sized_stack_arg_size),
                into_reg,
                ty.into(),
            )),
        };
        copy_arg_to_reg(&self.args[idx], into_reg);
        insts
    }

    pub fn take_args(&mut self) -> Option<M::I> {
        if self.reg_args.len() > 0 {
            Some(M::gen_args(std::mem::take(&mut self.reg_args)))
        } else {
            None
        }
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
