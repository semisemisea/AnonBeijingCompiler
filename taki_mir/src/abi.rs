use std::marker::PhantomData;
use std::num::NonZeroU64;

use rustc_hash::FxHashMap;
use smallvec::{SmallVec, smallvec};
use tomori_utils::{PrimaryMap, SecondaryMap, entity_impl};

use crate::prelude::*;
use crate::reg_alloc::reg::{MachineEnv, PReg, RegClass, SpillSlot};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgRegBank {
    Int,
    Float,
}

/// Shared register-bank and stack-offset planning for scalar ABI arguments.
/// Type classification and overflow-slot sizing remain target policies.
pub struct ArgLayoutPlanner<'a> {
    int_regs: &'a [Reg],
    float_regs: &'a [Reg],
}

impl<'a> ArgLayoutPlanner<'a> {
    pub const fn new(int_regs: &'a [Reg], float_regs: &'a [Reg]) -> Self {
        Self {
            int_regs,
            float_regs,
        }
    }

    pub fn compute(
        &self,
        types: &[HirType],
        classify: impl Fn(&HirType) -> ArgRegBank,
        stack_slot_size: impl Fn(&HirType) -> u32,
    ) -> (Vec<ArgSlot>, u32) {
        let mut slots = Vec::with_capacity(types.len());
        let (mut int_index, mut float_index, mut stack_offset) = (0usize, 0usize, 0u32);

        for ty in types {
            let (regs, index) = match classify(ty) {
                ArgRegBank::Int => (self.int_regs, &mut int_index),
                ArgRegBank::Float => (self.float_regs, &mut float_index),
            };
            let reg = regs.get(*index).copied();
            *index = index
                .checked_add(1)
                .expect("argument register index overflow");

            if let Some(reg) = reg {
                slots.push(ArgSlot::Reg {
                    reg: reg
                        .to_physical_reg()
                        .expect("ABI argument register must be physical"),
                    ty: ty.clone(),
                });
            } else {
                slots.push(ArgSlot::Stack {
                    offset: i64::from(stack_offset),
                    ty: ty.clone(),
                });
                stack_offset = stack_offset
                    .checked_add(stack_slot_size(ty))
                    .expect("stack argument area overflow");
            }
        }

        (slots, stack_offset)
    }
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

    /// Number of logical allocator spill units needed by a value in `regclass`.
    ///
    /// `SpillSlot` values and `Output::num_spillslots` are expressed in these
    /// units. Frame layout converts units to bytes with `spill_unit_bytes()`.
    fn spillslot_size(_regclass: RegClass) -> u32;

    /// Physical byte width of one logical allocator spill unit.
    fn spill_unit_bytes() -> u32;

    fn is_callee_saved(_preg: PReg) -> bool;

    fn gen_load_stack(mem: StackAMode, dst: Writable<Reg>, ty: LoweredType) -> Self::I;

    /// Materialize the low bits of `value` according to `ty`.
    ///
    /// Integer constants are bit patterns, rather than host-sized signed values:
    /// this keeps i32 and pointer-width materialization distinct.
    fn gen_load_imm(dst: Writable<Reg>, value: u64, ty: LoweredType) -> Self::I;

    fn gen_load_addr(dst: Writable<Reg>, label: HirInst) -> Self::I;

    /// Generate a frame-dependent address for a stack location. `Slot` offsets
    /// are relative to the stack-object area and must be resolved after the
    /// final outgoing-argument area is known.
    fn gen_get_stack_addr(mem: StackAMode, dst: Writable<Reg>) -> Self::I;

    /// Build the function-entry pseudo-instruction that binds each register
    /// parameter virtual register to its fixed ABI physical register. It must
    /// emit no machine code; register allocation resolves the fixed defs.
    fn gen_args(args: Vec<ArgPair>) -> Self::I;

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

    /// Spill access for an allocator edit whose offset is already resolved
    /// against the post-prologue stack pointer.
    fn gen_spill_store_at_sp(src: Reg, spill_off: i64, ty: LoweredType) -> SmallVec<[Self::I; 4]> {
        Self::gen_spill_store(src, spill_off, ty)
    }

    fn gen_spill_load_at_sp(
        spill_off: i64,
        dst: Writable<Reg>,
        ty: LoweredType,
    ) -> SmallVec<[Self::I; 4]> {
        Self::gen_spill_load(spill_off, dst, ty)
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

    fn gen_move(src: Reg, dst: Reg, ty: LoweredType) -> Self::I;

    fn compute_arg_loc(arena: ArenaContext<'_>) -> (Vec<ArgSlot>, u32) {
        let types: Vec<_> = arena
            .f()
            .params()
            .iter()
            .map(|&param| arena.inst_data(param).ty().clone())
            .collect();
        Self::compute_call_arg_loc(&types)
    }

    /// Assign locations for a call signature using the same convention as
    /// incoming function parameters. The returned size includes ABI-required
    /// padding for the complete outgoing argument area.
    fn compute_call_arg_loc(types: &[HirType]) -> (Vec<ArgSlot>, u32);

    fn get_machine_env() -> &'static MachineEnv;

    fn gen_prologue_frame_setup(frame: &FrameLayout) -> SmallVec<[Self::I; 16]>;

    fn gen_epilogue_frame_restore(frame: &FrameLayout) -> SmallVec<[Self::I; 16]>;

    fn gen_clobber_save(frame: &FrameLayout) -> SmallVec<[Self::I; 16]>;

    fn gen_clobber_restore(frame: &FrameLayout) -> SmallVec<[Self::I; 16]>;

    /// Expand frame-dependent pseudo addressing after allocation. Implementations
    /// must use only `MachineEnv::post_ra_scratch_by_class` registers.
    fn legalize_inst(_frame: &FrameLayout, inst: Self::I) -> SmallVec<[Self::I; 4]> {
        smallvec![inst]
    }

    /// Copy between allocator spill slots. Spill slots are eight-byte units, so
    /// an integer-width raw copy also preserves f32 values without requiring
    /// type information in allocator edits.
    fn gen_stack_to_stack_move(_from: i64, _to: i64) -> SmallVec<[Self::I; 4]> {
        panic!("target does not implement stack-to-stack allocator edits")
    }
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

impl FrameLayout {
    /// Allocator spills follow outgoing arguments and normal frame objects.
    pub fn spill_base_bytes(&self) -> u32 {
        self.outgoing_args_size
            .checked_add(self.stackslots_size)
            .expect("spill base overflow")
    }

    pub fn spill_slot_offset(&self, slot: SpillSlot, unit_bytes: u32) -> i64 {
        assert!(unit_bytes > 0, "spill unit size must be nonzero");
        let offset = (slot.raw_bits() as u64)
            .checked_mul(unit_bytes as u64)
            .and_then(|offset| offset.checked_add(self.spill_base_bytes() as u64))
            .expect("spill offset overflow");
        let end = offset
            .checked_add(unit_bytes as u64)
            .expect("spill access end overflow");
        assert!(
            end <= self.spill_region_end() as u64,
            "spill slot {slot} is outside the spill region"
        );
        i64::try_from(offset).expect("spill offset exceeds signed address range")
    }

    pub fn spill_region_end(&self) -> u32 {
        self.spill_base_bytes()
            .checked_add(self.spill_size)
            .expect("spill region overflow")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reg_alloc::reg::{PReg, RegClass};

    fn reg(index: usize, class: RegClass) -> Reg {
        Reg::from_physical_reg(PReg::new(index, class))
    }

    #[test]
    fn argument_layout_uses_independent_register_banks() {
        let int_regs = [reg(0, RegClass::Int), reg(1, RegClass::Int)];
        let float_regs = [reg(0, RegClass::Float)];
        let types = vec![
            HirType::get_i32(),
            HirType::get_f32(),
            HirType::get_i32(),
            HirType::get_f32(),
            HirType::get_i32(),
        ];
        let (slots, stack_size) = ArgLayoutPlanner::new(&int_regs, &float_regs).compute(
            &types,
            |ty| {
                if ty.is_f32() {
                    ArgRegBank::Float
                } else {
                    ArgRegBank::Int
                }
            },
            |_| 8,
        );

        assert!(matches!(slots[0], ArgSlot::Reg { .. }));
        assert!(matches!(slots[1], ArgSlot::Reg { .. }));
        assert!(matches!(slots[2], ArgSlot::Reg { .. }));
        assert!(matches!(slots[3], ArgSlot::Stack { offset: 0, .. }));
        assert!(matches!(slots[4], ArgSlot::Stack { offset: 8, .. }));
        assert_eq!(stack_size, 16);
    }

    #[test]
    fn argument_layout_preserves_target_stack_slot_sizes() {
        let types = vec![HirType::get_i32(), HirType::get_f32()];
        let planner = ArgLayoutPlanner::new(&[], &[]);
        let (_, fixed_size) = planner.compute(&types, |_| ArgRegBank::Int, |_| 8);
        let (sized_slots, sized_size) = planner.compute(
            &types,
            |_| ArgRegBank::Int,
            |ty| u32::try_from(ty.size()).unwrap(),
        );

        assert_eq!(fixed_size, 16);
        assert!(matches!(sized_slots[1], ArgSlot::Stack { offset: 4, .. }));
        assert_eq!(sized_size, 8);
    }

    #[test]
    fn spill_slots_follow_outgoing_and_local_stack_areas() {
        let frame = FrameLayout {
            callee_saved: vec![],
            setup_area_size: 16,
            clobber_size: 0,
            spill_size: 24,
            stackslots_size: 16,
            outgoing_args_size: 32,
            total_size: 96,
        };

        assert_eq!(frame.spill_base_bytes(), 48);
        assert_eq!(frame.spill_slot_offset(SpillSlot::new(0), 8), 48);
        assert_eq!(frame.spill_slot_offset(SpillSlot::new(2), 8), 64);
        assert_eq!(frame.spill_region_end(), 72);
    }

    #[test]
    #[should_panic(expected = "outside the spill region")]
    fn spill_slot_offset_rejects_out_of_range_slot() {
        let frame = FrameLayout {
            callee_saved: vec![],
            setup_area_size: 0,
            clobber_size: 0,
            spill_size: 8,
            stackslots_size: 0,
            outgoing_args_size: 0,
            total_size: 16,
        };

        frame.spill_slot_offset(SpillSlot::new(1), 8);
    }
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

/// An incoming register function parameter bound directly to its ABI physical
/// register. The `Args` pseudo-instruction defines `vreg` from `preg` without
/// emitting any machine code, replacing the old unconditional home-slot
/// store/load round trip for register arguments.
#[derive(Debug, Clone)]
pub struct ArgPair {
    pub vreg: Writable<Reg>,
    pub preg: Reg,
}

/// The function abstraction at ABI level.
/// Most of the data is not initialized correctly at constructor.
/// It will gradually build during the lowering process.
pub struct CalleeABI<M: ABIMachineSpec> {
    args: Vec<ArgSlot>,

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

    /// Register arguments awaiting `take_args`, which packages them into the
    /// entry `Args` pseudo. Populated by `gen_copy_arg_to_reg`; consumed by
    /// `gen_arg_setup` in lowering.
    reg_args: Vec<ArgPair>,

    _mach: PhantomData<M>,
}

impl<M: ABIMachineSpec> CalleeABI<M> {
    pub fn new(arena: ArenaContext<'_>) -> CalleeABI<M> {
        let (args, sized_stack_arg_size) = M::compute_arg_loc(arena);
        CalleeABI {
            args,
            total_stackslots_size: 0,
            sized_stack_arg_size,
            outgoing_arg_size: 0,
            has_calls: false,
            stackslots_offsets: PrimaryMap::new(),
            stackslots_keys: SecondaryMap::new(),
            alloc_to_ss: FxHashMap::default(),
            frame_layout: None,
            reg_args: Vec::new(),
            _mach: PhantomData,
        }
    }

    pub fn allocate_stackslot(&mut self, ty: HirType) -> u32 {
        let align = M::stack_align();
        // Stack objects are addressed relative to the stack-object area, not
        // to the outgoing area as it happened to be sized at allocation time.
        // Calls are discovered during lowering, so folding the then-current
        // outgoing size here would make early objects overlap a later maximum
        // outgoing-call area.
        let ret = self.total_stackslots_size;
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

    pub fn spill_unit_bytes(&self) -> u32 {
        M::spill_unit_bytes()
    }

    pub fn compute_frame_layout(
        &mut self,
        spill_size: u32,
        output: &crate::reg_alloc::reg::Output,
    ) -> Result<(), String> {
        // Allocator edits may use a callee-saved register even when no
        // instruction operand is assigned to it. This happens, for example,
        // when Ion resolves a live-range split with a register-to-register
        // move. Include both endpoints of every edit when deriving the
        // callee-save set; otherwise a function can silently clobber a
        // caller's preserved register.
        let mut callee_saved: Vec<PReg> = output
            .allocs
            .iter()
            .filter_map(|a| a.as_reg())
            .chain(output.edits.iter().flat_map(|(_, edit)| {
                let crate::reg_alloc::reg::Edit::Move { from, to, .. } = edit;
                [from.as_reg(), to.as_reg()].into_iter().flatten()
            }))
            .filter(|p| M::is_callee_saved(*p))
            .collect();
        callee_saved.sort_unstable();
        callee_saved.dedup();

        let stackslots_size = self.total_stackslots_size;
        let outgoing_args_size = self.outgoing_arg_size;
        let clobber_size = u32::try_from(callee_saved.len())
            .map_err(|_| "callee-save count exceeds frame range")?
            .checked_mul(M::word_bytes())
            .ok_or("callee-save area exceeds frame range")?;
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
        let total = setup_area_size
            .checked_add(clobber_size)
            .and_then(|total| total.checked_add(spill_size))
            .and_then(|total| total.checked_add(stackslots_size))
            .and_then(|total| total.checked_add(outgoing_args_size))
            .ok_or("frame size exceeds frame range")?;
        let align = M::stack_align();
        let total = total
            .checked_add(align.checked_sub(1).ok_or("invalid stack alignment")?)
            .map(|size| size / align * align)
            .ok_or("aligned frame size exceeds frame range")?;
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
        Ok(())
    }

    /// Bind the `idx`-th function parameter to `into_reg`.
    ///
    /// Register arguments are recorded as [`ArgPair`]s to be packaged into the
    /// entry `Args` pseudo by [`CalleeABI::take_args`]; no instructions are
    /// emitted. Stack arguments are materialized with an incoming-argument
    /// load. Only called for parameters whose value is actually needed.
    pub fn gen_copy_arg_to_reg(&mut self, idx: usize, into_reg: Reg) -> SmallVec<[M::I; 4]> {
        let mut insts = smallvec![];
        match self.args[idx].clone() {
            ArgSlot::Reg { reg: preg, .. } => {
                self.reg_args.push(ArgPair {
                    vreg: Writable::from_reg(into_reg),
                    preg: preg.into(),
                });
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

    pub fn frame_layout(&self) -> &FrameLayout {
        self.frame_layout
            .as_ref()
            .expect("compute_frame_layout must be called before gen_prologue/gen_epilogue")
    }

    /// The ABI slot (register or incoming-stack) for the `idx`-th function
    /// parameter. Used by tail-call lowering to place each argument exactly
    /// where the callee will read it.
    pub fn arg_slot(&self, idx: usize) -> ArgSlot {
        self.args[idx].clone()
    }

    /// Drain the collected register-argument bindings and package them into the
    /// entry `Args` pseudo. Returns `None` when no register argument was bound.
    pub fn take_args(&mut self) -> Option<M::I> {
        if self.reg_args.is_empty() {
            None
        } else {
            Some(M::gen_args(core::mem::take(&mut self.reg_args)))
        }
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
