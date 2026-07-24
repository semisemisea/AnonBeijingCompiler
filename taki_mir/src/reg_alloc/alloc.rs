use core::convert::TryInto;
use core::fmt;
use core::ops::{BitAnd, BitOr, Deref, DerefMut, Index, IndexMut, Not};

use crate::reg_alloc::{
    function::Function,
    index::{Block, Inst},
    lru::{Lrus, PartedByRegClass},
    moves::{MoveAndScratchResolver, ParallelMoves},
    reg::{
        Allocation, AllocationKind, Edit, InstPosition, MachineEnv, Operand, OperandConstraint,
        OperandKind, OperandPos, Output, PReg, PRegSet, ProgPoint, RegClass, SpillSlot, VReg,
    },
    vregset::VRegSet,
};

#[derive(Debug, Clone)]
pub struct PartedByOperandPos<T> {
    pub items: [T; 2],
}

impl<T: Copy> Copy for PartedByOperandPos<T> {}

impl<T: BitAnd<Output = T> + Copy> BitAnd for PartedByOperandPos<T> {
    type Output = Self;
    fn bitand(self, other: Self) -> Self {
        Self {
            items: [
                self.items[0] & other.items[0],
                self.items[1] & other.items[1],
            ],
        }
    }
}

impl<T: BitOr<Output = T> + Copy> BitOr for PartedByOperandPos<T> {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        Self {
            items: [
                self.items[0] | other.items[0],
                self.items[1] | other.items[1],
            ],
        }
    }
}

impl Not for PartedByOperandPos<PRegSet> {
    type Output = Self;
    fn not(self) -> Self {
        Self {
            items: [self.items[0].invert(), self.items[1].invert()],
        }
    }
}

impl<T> Index<OperandPos> for PartedByOperandPos<T> {
    type Output = T;
    fn index(&self, index: OperandPos) -> &Self::Output {
        &self.items[index as usize]
    }
}

impl<T> IndexMut<OperandPos> for PartedByOperandPos<T> {
    fn index_mut(&mut self, index: OperandPos) -> &mut Self::Output {
        &mut self.items[index as usize]
    }
}

impl<T: fmt::Display> fmt::Display for PartedByOperandPos<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{ early: {}, late: {} }}", self.items[0], self.items[1])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusiveOperandPos {
    EarlyOnly = 0,
    LateOnly = 1,
    Both = 2,
}

#[derive(Debug, Clone)]
pub struct PartedByExclusiveOperandPos<T> {
    pub items: [T; 3],
}

impl<T: PartialEq> PartialEq for PartedByExclusiveOperandPos<T> {
    fn eq(&self, other: &Self) -> bool {
        self.items.eq(&other.items)
    }
}

impl<T> Index<ExclusiveOperandPos> for PartedByExclusiveOperandPos<T> {
    type Output = T;
    fn index(&self, index: ExclusiveOperandPos) -> &Self::Output {
        &self.items[index as usize]
    }
}

impl<T> IndexMut<ExclusiveOperandPos> for PartedByExclusiveOperandPos<T> {
    fn index_mut(&mut self, index: ExclusiveOperandPos) -> &mut Self::Output {
        &mut self.items[index as usize]
    }
}

impl From<Operand> for ExclusiveOperandPos {
    fn from(op: Operand) -> Self {
        match (op.kind(), op.pos()) {
            (OperandKind::Use, OperandPos::Late) | (OperandKind::Def, OperandPos::Early) => {
                ExclusiveOperandPos::Both
            }
            _ if matches!(op.constraint(), OperandConstraint::Reuse(_)) => {
                ExclusiveOperandPos::Both
            }
            (_, OperandPos::Early) => ExclusiveOperandPos::EarlyOnly,
            (_, OperandPos::Late) => ExclusiveOperandPos::LateOnly,
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: Operands wrapper
// ---------------------------------------------------------------------------

struct Operands<'a>(pub &'a [Operand]);

impl<'a> Operands<'a> {
    fn new(operands: &'a [Operand]) -> Self {
        Self(operands)
    }

    fn matches<F: Fn(Operand) -> bool + 'a>(
        &self,
        predicate: F,
    ) -> impl Iterator<Item = (usize, Operand)> + 'a {
        self.0
            .iter()
            .cloned()
            .enumerate()
            .filter(move |(_, op)| predicate(*op))
    }

    fn use_ops(&self) -> impl Iterator<Item = (usize, Operand)> + 'a {
        self.matches(|op| op.kind() == OperandKind::Use)
    }

    fn fixed(&self) -> impl Iterator<Item = (usize, Operand)> + 'a {
        self.matches(|op| matches!(op.constraint(), OperandConstraint::FixedReg(_)))
    }

    fn late(&self) -> impl Iterator<Item = (usize, Operand)> + 'a {
        self.matches(|op| op.pos() == OperandPos::Late)
    }

    fn early(&self) -> impl Iterator<Item = (usize, Operand)> + 'a {
        self.matches(|op| op.pos() == OperandPos::Early)
    }
}

impl<'a> Index<usize> for Operands<'a> {
    type Output = Operand;

    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}

// ---------------------------------------------------------------------------
// Output storage
// ---------------------------------------------------------------------------

struct Allocs {
    allocs: Vec<Allocation>,
    inst_alloc_offsets: Vec<u32>,
}

impl Allocs {
    fn new<F: Function>(func: &F) -> (Self, u32) {
        let mut allocs = Vec::new();
        let mut inst_alloc_offsets = Vec::with_capacity(func.num_insts());
        let mut max_operand_len = 0;
        let mut no_of_operands = 0;
        for inst in 0..func.num_insts() {
            let operands_len = func.inst_operands(Inst::new(inst)).len() as u32;
            max_operand_len = max_operand_len.max(operands_len);
            inst_alloc_offsets.push(no_of_operands as u32);
            no_of_operands += operands_len;
        }
        allocs.resize(no_of_operands as usize, Allocation::none());
        (
            Self {
                allocs,
                inst_alloc_offsets,
            },
            max_operand_len,
        )
    }
}

impl Index<(usize, usize)> for Allocs {
    type Output = Allocation;

    fn index(&self, idx: (usize, usize)) -> &Allocation {
        &self.allocs[self.inst_alloc_offsets[idx.0] as usize + idx.1]
    }
}

impl IndexMut<(usize, usize)> for Allocs {
    fn index_mut(&mut self, idx: (usize, usize)) -> &mut Allocation {
        &mut self.allocs[self.inst_alloc_offsets[idx.0] as usize + idx.1]
    }
}

// ---------------------------------------------------------------------------
// Spillslot allocator
// ---------------------------------------------------------------------------

struct Stack<'a, F: Function> {
    num_spillslots: u32,
    func: &'a F,
}

impl<'a, F: Function> Stack<'a, F> {
    fn new(func: &'a F) -> Self {
        Self {
            num_spillslots: 0,
            func,
        }
    }

    fn allocstack(&mut self, class: RegClass) -> SpillSlot {
        let size = u32::try_from(self.func.spillslot_size(class))
            .expect("spill-slot size exceeds allocator index range");
        assert!(size > 0, "spill-slot size must be nonzero");
        log::trace!("Allocating {size} spillslot units for class {class:?}");
        let aligned = self.num_spillslots.next_multiple_of(size);
        let end = aligned
            .checked_add(size)
            .expect("spill-slot allocation overflow");
        assert!(end as usize <= SpillSlot::MAX, "spill-slot index overflow");
        self.num_spillslots = end;
        let slot = if self.func.multi_spillslot_named_by_last_slot() {
            end - 1
        } else {
            aligned
        };
        log::trace!("Allocated slot: {slot}");
        SpillSlot::new(slot as usize)
    }
}

// ---------------------------------------------------------------------------
// Per-instruction allocation state
// ---------------------------------------------------------------------------

pub struct State<'a, F: Function> {
    func: &'a F,
    edits: Vec<(ProgPoint, Edit)>,
    fixed_stack_slots: PRegSet,
    scratch_regs: PartedByRegClass<Option<PReg>>,
    dedicated_scratch_regs: PartedByRegClass<Option<PReg>>,
    available_pregs: PartedByOperandPos<PRegSet>,
    num_available_pregs: PartedByExclusiveOperandPos<PartedByRegClass<i16>>,
    vreg_allocs: Vec<Allocation>,
    vreg_spillslots: Vec<SpillSlot>,
    vreg_in_preg: Vec<VReg>,
    stack: Stack<'a, F>,
    lrus: Lrus,
}

impl<'a, F: Function> State<'a, F> {
    fn is_stack(&self, alloc: Allocation) -> bool {
        alloc.is_stack()
            || (alloc.is_reg() && self.fixed_stack_slots.contains(alloc.as_reg().unwrap()))
    }

    fn get_spillslot(&mut self, vreg: VReg) -> SpillSlot {
        if self.vreg_spillslots[vreg.vreg()].is_invalid() {
            self.vreg_spillslots[vreg.vreg()] = self.stack.allocstack(vreg.class());
        }
        self.vreg_spillslots[vreg.vreg()]
    }

    fn evict_vreg_in_preg(
        &mut self,
        inst: Inst,
        preg: PReg,
        pos: InstPosition,
    ) -> Result<(), String> {
        log::trace!("Removing the vreg in preg {} for eviction", preg);
        let evicted_vreg = self.vreg_in_preg[preg.index()];
        log::trace!("The removed vreg: {}", evicted_vreg);
        debug_assert_ne!(evicted_vreg, VReg::invalid());
        if self.vreg_spillslots[evicted_vreg.vreg()].is_invalid() {
            self.vreg_spillslots[evicted_vreg.vreg()] = self.stack.allocstack(evicted_vreg.class());
        }
        let slot = self.vreg_spillslots[evicted_vreg.vreg()];
        self.vreg_allocs[evicted_vreg.vreg()] = Allocation::stack(slot);
        log::trace!("Move reason: eviction");
        self.add_move(
            inst,
            self.vreg_allocs[evicted_vreg.vreg()],
            Allocation::reg(preg),
            evicted_vreg.class(),
            pos,
        )
    }

    fn alloc_scratch_reg(
        &mut self,
        inst: Inst,
        class: RegClass,
        pos: InstPosition,
    ) -> Result<(), String> {
        let avail_regs =
            self.available_pregs[OperandPos::Late] & self.available_pregs[OperandPos::Early];
        log::trace!("Checking {avail_regs} for scratch register for {class:?}");
        if let Some(preg) = self.lrus[class].last(avail_regs) {
            if self.vreg_in_preg[preg.index()] != VReg::invalid() {
                self.evict_vreg_in_preg(inst, preg, pos)?;
            }
            self.scratch_regs[class] = Some(preg);
            self.available_pregs[OperandPos::Early].remove(preg);
            self.available_pregs[OperandPos::Late].remove(preg);
            Ok(())
        } else {
            log::trace!("Can't get a scratch register for {class:?}");
            Err("Too many live registers for scratch".to_string())
        }
    }

    fn add_move(
        &mut self,
        inst: Inst,
        from: Allocation,
        to: Allocation,
        class: RegClass,
        pos: InstPosition,
    ) -> Result<(), String> {
        if self.is_stack(from) && self.is_stack(to) {
            if self.scratch_regs[class].is_none() {
                self.alloc_scratch_reg(inst, class, pos)?;
                let dec_clamp_zero = |x: &mut i16| {
                    *x = 0i16.max(*x - 1);
                };
                dec_clamp_zero(&mut self.num_available_pregs[ExclusiveOperandPos::Both][class]);
                dec_clamp_zero(
                    &mut self.num_available_pregs[ExclusiveOperandPos::EarlyOnly][class],
                );
                dec_clamp_zero(&mut self.num_available_pregs[ExclusiveOperandPos::LateOnly][class]);
            }
            log::trace!("Edit is stack-to-stack. Generating two edits with a scratch register");
            let scratch_reg = self.scratch_regs[class].unwrap();
            let scratch_alloc = Allocation::reg(scratch_reg);
            log::trace!("Move 1: {scratch_alloc:?} to {to:?}");
            self.edits.push((
                ProgPoint::new(inst.raw_u32(), pos),
                Edit::Move {
                    from: scratch_alloc,
                    to,
                    class,
                },
            ));
            log::trace!("Move 2: {from:?} to {scratch_alloc:?}");
            self.edits.push((
                ProgPoint::new(inst.raw_u32(), pos),
                Edit::Move {
                    from,
                    to: scratch_alloc,
                    class,
                },
            ));
        } else {
            self.edits.push((
                ProgPoint::new(inst.raw_u32(), pos),
                Edit::Move { from, to, class },
            ));
        }
        Ok(())
    }

    fn move_if_def_pred_branch(
        &mut self,
        block: Block,
        pred: Block,
        vreg: VReg,
        slot: SpillSlot,
    ) -> Result<(), String> {
        let pred_last_inst = self.func.block_insns(pred).last();
        let move_from = self.func.inst_operands(pred_last_inst).iter().find_map(|op| {
            if op.kind() == OperandKind::Def && op.vreg() == vreg {
                if self.func.block_preds(block).len() > 1 {
                    panic!(
                        "Multiple predecessors when a branch arg/livein is defined on the branch"
                    );
                }
                match op.constraint() {
                    OperandConstraint::FixedReg(reg) => {
                        log::trace!("Vreg {vreg} defined on pred {pred:?} branch");
                        Some(Allocation::reg(reg))
                    }
                    OperandConstraint::Stack | OperandConstraint::Any => None,
                    constraint => panic!("fastalloc does not support using any-reg or reuse constraints ({constraint}) defined on a branch instruction as a branch arg/livein on the same instruction"),
                }
            } else {
                None
            }
        });
        if let Some(from) = move_from {
            let to = Allocation::stack(slot);
            log::trace!("Inserting edit to move from {from} to {to}");
            self.add_move(
                self.func.block_insns(block).first(),
                from,
                to,
                vreg.class(),
                InstPosition::Before,
            )?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Top-level allocator environment
// ---------------------------------------------------------------------------

pub struct Env<'a, F: Function> {
    func: &'a F,

    live_vregs: VRegSet,
    reused_input_to_reuse_op: Vec<usize>,
    num_any_reg_ops: PartedByExclusiveOperandPos<PartedByRegClass<i16>>,
    init_num_available_pregs: PartedByRegClass<i16>,
    init_available_pregs: PRegSet,
    allocatable_regs: PRegSet,
    preferred_victim: PartedByRegClass<PReg>,
    vreg_to_live_inst_range: Vec<(ProgPoint, ProgPoint, Allocation)>,
    fixed_stack_slots: PRegSet,

    allocs: Allocs,
    state: State<'a, F>,
}

impl<'a, F: Function> Env<'a, F> {
    fn new(func: &'a F, env: &'a MachineEnv) -> Self {
        let mut regs = [
            env.preferred_regs_by_class[RegClass::Int as usize].clone(),
            env.preferred_regs_by_class[RegClass::Float as usize].clone(),
            env.preferred_regs_by_class[RegClass::Vector as usize].clone(),
        ];
        regs[0].union_from(env.non_preferred_regs_by_class[RegClass::Int as usize]);
        regs[1].union_from(env.non_preferred_regs_by_class[RegClass::Float as usize]);
        regs[2].union_from(env.non_preferred_regs_by_class[RegClass::Vector as usize]);
        let allocatable_regs = PRegSet::from(env);
        let num_available_pregs: PartedByRegClass<i16> = PartedByRegClass {
            items: [
                (env.preferred_regs_by_class[RegClass::Int as usize].len()
                    + env.non_preferred_regs_by_class[RegClass::Int as usize].len())
                .try_into()
                .unwrap(),
                (env.preferred_regs_by_class[RegClass::Float as usize].len()
                    + env.non_preferred_regs_by_class[RegClass::Float as usize].len())
                .try_into()
                .unwrap(),
                (env.preferred_regs_by_class[RegClass::Vector as usize].len()
                    + env.non_preferred_regs_by_class[RegClass::Vector as usize].len())
                .try_into()
                .unwrap(),
            ],
        };
        let init_available_pregs = {
            let mut regs = allocatable_regs;
            for preg in env.fixed_stack_slots.iter() {
                regs.add(*preg);
            }
            regs
        };
        let dedicated_scratch_regs = PartedByRegClass {
            items: [
                env.scratch_by_class[0],
                env.scratch_by_class[1],
                env.scratch_by_class[2],
            ],
        };
        log::trace!("{:#?}", env);
        let (allocs, max_operand_len) = Allocs::new(func);
        let fixed_stack_slots =
            core::iter::FromIterator::from_iter(env.fixed_stack_slots.iter().cloned());
        Self {
            func,
            allocatable_regs,
            live_vregs: VRegSet::with_capacity(func.num_vregs()),
            fixed_stack_slots,
            vreg_to_live_inst_range: vec![
                (
                    ProgPoint::invalid(),
                    ProgPoint::invalid(),
                    Allocation::none()
                );
                func.num_vregs()
            ],
            preferred_victim: PartedByRegClass {
                items: [
                    regs[0].max_preg().unwrap_or(PReg::invalid()),
                    regs[1].max_preg().unwrap_or(PReg::invalid()),
                    regs[2].max_preg().unwrap_or(PReg::invalid()),
                ],
            },
            reused_input_to_reuse_op: vec![usize::MAX; max_operand_len as usize],
            init_available_pregs,
            init_num_available_pregs: num_available_pregs.clone(),
            num_any_reg_ops: PartedByExclusiveOperandPos {
                items: [
                    PartedByRegClass { items: [0; 3] },
                    PartedByRegClass { items: [0; 3] },
                    PartedByRegClass { items: [0; 3] },
                ],
            },
            allocs,
            state: State {
                func,
                edits: Vec::with_capacity(func.num_insts()),
                fixed_stack_slots,
                scratch_regs: dedicated_scratch_regs.clone(),
                dedicated_scratch_regs,
                num_available_pregs: PartedByExclusiveOperandPos {
                    items: [
                        num_available_pregs.clone(),
                        num_available_pregs.clone(),
                        num_available_pregs.clone(),
                    ],
                },
                available_pregs: PartedByOperandPos {
                    items: [init_available_pregs, init_available_pregs],
                },
                lrus: Lrus::new(&regs[0], &regs[1], &regs[2]),
                vreg_in_preg: vec![VReg::invalid(); PReg::NUM_INDEX],
                stack: Stack::new(func),
                vreg_allocs: vec![Allocation::none(); func.num_vregs()],
                vreg_spillslots: vec![SpillSlot::invalid(); func.num_vregs()],
            },
        }
    }

    fn reset_available_pregs_and_scratch_regs(&mut self) {
        log::trace!("Resetting the available pregs");
        self.available_pregs = PartedByOperandPos {
            items: [self.init_available_pregs, self.init_available_pregs],
        };
        self.scratch_regs = self.dedicated_scratch_regs.clone();
        self.num_available_pregs = PartedByExclusiveOperandPos {
            items: [self.init_num_available_pregs; 3],
        };
        debug_assert_eq!(
            self.num_any_reg_ops,
            PartedByExclusiveOperandPos {
                items: [PartedByRegClass { items: [0; 3] }; 3]
            }
        );
    }

    fn reserve_reg_for_operand(
        &mut self,
        op: Operand,
        op_idx: usize,
        preg: PReg,
    ) -> Result<(), String> {
        log::trace!("Reserving register {preg} for operand {op}");
        let early_avail_pregs = self.available_pregs[OperandPos::Early];
        let late_avail_pregs = self.available_pregs[OperandPos::Late];
        match (op.pos(), op.kind()) {
            (OperandPos::Early, OperandKind::Use) => {
                if op.as_fixed_nonallocatable().is_none() && !early_avail_pregs.contains(preg) {
                    log::trace!("fixed {preg} for {op} isn't available");
                    return Err("Too many live registers".to_string());
                }
                self.available_pregs[OperandPos::Early].remove(preg);
                if self.reused_input_to_reuse_op[op_idx] != usize::MAX {
                    if op.as_fixed_nonallocatable().is_none() && !late_avail_pregs.contains(preg) {
                        log::trace!("fixed {preg} for {op} isn't available");
                        return Err("Too many live registers".to_string());
                    }
                    self.available_pregs[OperandPos::Late].remove(preg);
                }
            }
            (OperandPos::Late, OperandKind::Def) => {
                if op.as_fixed_nonallocatable().is_none() && !late_avail_pregs.contains(preg) {
                    log::trace!("fixed {preg} for {op} isn't available");
                    return Err("Too many live registers".to_string());
                }
                self.available_pregs[OperandPos::Late].remove(preg);
            }
            _ => {
                if op.as_fixed_nonallocatable().is_none()
                    && (!early_avail_pregs.contains(preg) || !late_avail_pregs.contains(preg))
                {
                    log::trace!("fixed {preg} for {op} isn't available");
                    return Err("Too many live registers".to_string());
                }
                self.available_pregs[OperandPos::Early].remove(preg);
                self.available_pregs[OperandPos::Late].remove(preg);
            }
        }
        Ok(())
    }

    fn allocd_within_constraint(&self, op: Operand, inst: Inst) -> bool {
        let alloc = self.vreg_allocs[op.vreg().vreg()];
        match op.constraint() {
            OperandConstraint::Any => {
                if let Some(preg) = alloc.as_reg() {
                    let exclusive_pos: ExclusiveOperandPos = op.into();
                    if !self.is_stack(alloc)
                        && self.num_available_pregs[exclusive_pos][op.class()]
                            < self.num_any_reg_ops[exclusive_pos][op.class()]
                    {
                        log::trace!("Need more registers to cover all any-reg ops.");
                        return false;
                    }
                    if !self.available_pregs[op.pos()].contains(preg) {
                        log::trace!("The vreg in {preg}: {}", self.vreg_in_preg[preg.index()]);
                        self.vreg_in_preg[preg.index()] == op.vreg()
                            && (op.pos() != OperandPos::Late
                                || !self.func.inst_clobbers(inst).contains(preg))
                    } else {
                        true
                    }
                } else {
                    !alloc.is_none()
                }
            }
            OperandConstraint::Reg => {
                if self.is_stack(alloc) {
                    return false;
                }
                if let Some(preg) = alloc.as_reg() {
                    if !self.available_pregs[op.pos()].contains(preg) {
                        log::trace!("The vreg in {preg}: {}", self.vreg_in_preg[preg.index()]);
                        self.vreg_in_preg[preg.index()] == op.vreg()
                            && (op.pos() != OperandPos::Late
                                || !self.func.inst_clobbers(inst).contains(preg))
                    } else {
                        true
                    }
                } else {
                    false
                }
            }
            OperandConstraint::FixedReg(preg) => alloc.is_reg() && alloc.as_reg().unwrap() == preg,
            OperandConstraint::Reuse(_) => unreachable!(),
            OperandConstraint::Stack => self.is_stack(alloc),
            OperandConstraint::Limit(_) => {
                todo!("limit constraints are not yet supported in fastalloc")
            }
        }
    }

    fn freealloc(&mut self, vreg: VReg) {
        log::trace!("Freeing vreg {}", vreg);
        let alloc = self.vreg_allocs[vreg.vreg()];
        match alloc.kind() {
            AllocationKind::Reg => {
                let preg = alloc.as_reg().unwrap();
                self.vreg_in_preg[preg.index()] = VReg::invalid();
            }
            AllocationKind::Stack => (),
            AllocationKind::None => unreachable!("Attempting to free an unallocated operand!"),
        }
        self.vreg_allocs[vreg.vreg()] = Allocation::none();
        self.live_vregs.remove(vreg.vreg());
        log::trace!(
            "{} curr alloc is now {}",
            vreg,
            self.vreg_allocs[vreg.vreg()]
        );
    }

    fn select_suitable_reg_in_lru(&self, op: Operand) -> Result<PReg, String> {
        let draw_from = match (op.pos(), op.kind()) {
            (OperandPos::Late, OperandKind::Use) | (OperandPos::Early, OperandKind::Def) => {
                self.available_pregs[OperandPos::Late] & self.available_pregs[OperandPos::Early]
            }
            _ => self.available_pregs[op.pos()],
        };
        if draw_from.is_empty(op.class()) {
            log::trace!("No registers available for {op} in selection");
            return Err("No registers available".to_string());
        }
        let Some(preg) = self.lrus[op.class()].last(draw_from) else {
            log::trace!(
                "Failed to find an available {:?} register in the LRU for operand {op}",
                op.class()
            );
            return Err("Failed to find reg in LRU".to_string());
        };
        Ok(preg)
    }

    fn alloc_reg_for_operand(&mut self, inst: Inst, op: Operand) -> Result<Allocation, String> {
        log::trace!("available regs: {}", self.available_pregs);
        log::trace!("Int LRU: {:?}", self.lrus[RegClass::Int]);
        log::trace!("Float LRU: {:?}", self.lrus[RegClass::Float]);
        log::trace!("Vector LRU: {:?}", self.lrus[RegClass::Vector]);
        log::trace!("");
        let preg = self.select_suitable_reg_in_lru(op)?;
        if self.vreg_in_preg[preg.index()] != VReg::invalid() {
            self.evict_vreg_in_preg(inst, preg, InstPosition::After)?;
        }
        log::trace!("The allocated register for vreg {}: {}", op.vreg(), preg);
        self.lrus[op.class()].poke(preg);
        self.available_pregs[op.pos()].remove(preg);
        match (op.pos(), op.kind()) {
            (OperandPos::Late, OperandKind::Use) => {
                self.available_pregs[OperandPos::Early].remove(preg);
            }
            (OperandPos::Early, OperandKind::Def) => {
                self.available_pregs[OperandPos::Late].remove(preg);
            }
            (OperandPos::Late, OperandKind::Def)
                if matches!(op.constraint(), OperandConstraint::Reuse(_)) =>
            {
                self.available_pregs[OperandPos::Early].remove(preg);
            }
            _ => (),
        };
        Ok(Allocation::reg(preg))
    }

    fn alloc_operand(
        &mut self,
        inst: Inst,
        op: Operand,
        op_idx: usize,
    ) -> Result<Allocation, String> {
        let new_alloc = match op.constraint() {
            OperandConstraint::Any => {
                if (op.kind() == OperandKind::Def
                    && self.vreg_allocs[op.vreg().vreg()] == Allocation::none())
                    || self.num_any_reg_ops[op.into()][op.class()]
                        >= self.num_available_pregs[op.into()][op.class()]
                {
                    Allocation::stack(self.get_spillslot(op.vreg()))
                } else {
                    match self.alloc_reg_for_operand(inst, op) {
                        Ok(alloc) => alloc,
                        Err(_) => Allocation::stack(self.get_spillslot(op.vreg())),
                    }
                }
            }
            OperandConstraint::Reg => {
                let alloc = self.alloc_reg_for_operand(inst, op)?;
                self.num_any_reg_ops[op.into()][op.class()] -= 1;
                log::trace!(
                    "Number of {:?} any-reg ops to allocate now: {}",
                    Into::<ExclusiveOperandPos>::into(op),
                    self.num_any_reg_ops[op.into()]
                );
                alloc
            }
            OperandConstraint::FixedReg(preg) => {
                log::trace!("The fixed preg: {} for operand {}", preg, op);
                Allocation::reg(preg)
            }
            OperandConstraint::Reuse(_) => {
                unreachable!();
            }
            OperandConstraint::Stack => Allocation::stack(self.get_spillslot(op.vreg())),
            OperandConstraint::Limit(_) => {
                todo!("limit constraints are not yet supported in fastalloc")
            }
        };
        self.allocs[(inst.index(), op_idx)] = new_alloc;
        Ok(new_alloc)
    }

    fn process_operand_allocation(
        &mut self,
        inst: Inst,
        op: Operand,
        op_idx: usize,
    ) -> Result<(), String> {
        if let Some(preg) = op.as_fixed_nonallocatable() {
            self.allocs[(inst.index(), op_idx)] = Allocation::reg(preg);
            log::trace!(
                "Allocation for instruction {:?} and operand {}: {}",
                inst,
                op,
                self.allocs[(inst.index(), op_idx)]
            );
            return Ok(());
        }
        if !self.allocd_within_constraint(op, inst) {
            log::trace!(
                "{op} isn't allocated within constraints (the alloc: {}).",
                self.vreg_allocs[op.vreg().vreg()]
            );
            let curr_alloc = self.vreg_allocs[op.vreg().vreg()];
            let new_alloc = self.alloc_operand(inst, op, op_idx)?;
            if curr_alloc.is_none() {
                self.live_vregs.insert(op.vreg());
                self.vreg_to_live_inst_range[op.vreg().vreg()].1 = match (op.pos(), op.kind()) {
                    (OperandPos::Late, OperandKind::Use) | (_, OperandKind::Def) => {
                        ProgPoint::before((inst.index() + 1) as u32)
                    }
                    (OperandPos::Early, OperandKind::Use) => ProgPoint::after(inst.raw_u32()),
                };
                self.vreg_to_live_inst_range[op.vreg().vreg()].2 = new_alloc;

                log::trace!("Setting vreg_allocs[{op}] to {new_alloc:?}");
                self.vreg_allocs[op.vreg().vreg()] = new_alloc;
                if let Some(preg) = new_alloc.as_reg() {
                    self.vreg_in_preg[preg.index()] = op.vreg();
                }
            } else {
                log::trace!("Move reason: Prev allocation doesn't meet constraints");
                if op.kind() == OperandKind::Def {
                    log::trace!(
                        "Adding edit from {new_alloc:?} to {curr_alloc:?} after inst {inst:?} for {op}"
                    );
                    self.add_move(inst, new_alloc, curr_alloc, op.class(), InstPosition::After)?;
                }
                if let Some(preg) = new_alloc.as_reg() {
                    self.vreg_in_preg[preg.index()] = VReg::invalid();
                }
            }
            log::trace!(
                "Allocation for instruction {:?} and operand {}: {}",
                inst,
                op,
                self.allocs[(inst.index(), op_idx)]
            );
        } else {
            log::trace!("{op} is already allocated within constraints");
            self.allocs[(inst.index(), op_idx)] = self.vreg_allocs[op.vreg().vreg()];
            if op.constraint() == OperandConstraint::Reg {
                self.num_any_reg_ops[op.into()][op.class()] -= 1;
                log::trace!(
                    "{op} is already within constraint. Number of reg-only ops that need to be allocated now: {}",
                    self.num_any_reg_ops[op.into()]
                );
            }
            if let Some(preg) = self.allocs[(inst.index(), op_idx)].as_reg() {
                if self.allocatable_regs.contains(preg) {
                    self.lrus[preg.class()].poke(preg);
                }
                self.available_pregs[op.pos()].remove(preg);
                match (op.pos(), op.kind()) {
                    (OperandPos::Late, OperandKind::Use) => {
                        self.available_pregs[OperandPos::Early].remove(preg);
                    }
                    (OperandPos::Early, OperandKind::Def) => {
                        self.available_pregs[OperandPos::Late].remove(preg);
                    }
                    _ => (),
                };
            }
            log::trace!(
                "Allocation for instruction {:?} and operand {}: {}",
                inst,
                op,
                self.allocs[(inst.index(), op_idx)]
            );
        }
        log::trace!(
            "Late available regs: {}",
            self.available_pregs[OperandPos::Late]
        );
        log::trace!(
            "Early available regs: {}",
            self.available_pregs[OperandPos::Early]
        );
        Ok(())
    }

    fn remove_clobbers_from_available_pregs(&mut self, clobbers: PRegSet) {
        log::trace!("Removing clobbers {clobbers} from late available reg sets");
        let all_but_clobbers = clobbers.invert();
        self.available_pregs[OperandPos::Late].intersect_from(all_but_clobbers);
    }

    fn process_branch(&mut self, block: Block, inst: Inst) -> Result<(), String> {
        log::trace!("Processing branch instruction {inst:?} in block {block:?}");

        let mut int_parallel_moves = ParallelMoves::new();
        let mut float_parallel_moves = ParallelMoves::new();
        let mut vec_parallel_moves = ParallelMoves::new();

        for (succ_idx, succ) in self.func.block_succs(block).iter().enumerate() {
            for (pos, vreg) in self
                .func
                .branch_blockparams(block, inst, succ_idx)
                .iter()
                .enumerate()
            {
                if self
                    .func
                    .inst_operands(inst)
                    .iter()
                    .any(|op| op.vreg() == *vreg && op.kind() == OperandKind::Def)
                {
                    continue;
                }
                let succ_params = self.func.block_params(*succ);
                let succ_param_vreg = succ_params[pos];
                if self.vreg_spillslots[succ_param_vreg.vreg()].is_invalid() {
                    self.vreg_spillslots[succ_param_vreg.vreg()] =
                        self.stack.allocstack(succ_param_vreg.class());
                }
                if self.vreg_spillslots[vreg.vreg()].is_invalid() {
                    self.vreg_spillslots[vreg.vreg()] = self.stack.allocstack(vreg.class());
                }
                let vreg_spill = Allocation::stack(self.vreg_spillslots[vreg.vreg()]);
                let curr_alloc = self.vreg_allocs[vreg.vreg()];
                if curr_alloc.is_none() {
                    self.live_vregs.insert(*vreg);
                    self.vreg_to_live_inst_range[vreg.vreg()].1 = ProgPoint::before(inst.raw_u32());
                } else if curr_alloc != vreg_spill {
                    self.add_move(
                        inst,
                        vreg_spill,
                        curr_alloc,
                        vreg.class(),
                        InstPosition::Before,
                    )?;
                }
                self.vreg_allocs[vreg.vreg()] = vreg_spill;
                let parallel_moves = match vreg.class() {
                    RegClass::Int => &mut int_parallel_moves,
                    RegClass::Float => &mut float_parallel_moves,
                    RegClass::Vector => &mut vec_parallel_moves,
                };
                let from = Allocation::stack(self.vreg_spillslots[vreg.vreg()]);
                let to = Allocation::stack(self.vreg_spillslots[succ_param_vreg.vreg()]);
                log::trace!("Recording parallel move from {from} to {to}");
                parallel_moves.add(from, to, Some(*vreg));
            }
        }

        let resolved_int = int_parallel_moves.resolve();
        let resolved_float = float_parallel_moves.resolve();
        let resolved_vec = vec_parallel_moves.resolve();
        let mut scratch_regs = self.scratch_regs.clone();
        let mut avail_regs =
            self.available_pregs[OperandPos::Early] & self.available_pregs[OperandPos::Late];
        let mut num_spillslots = self.stack.num_spillslots;

        log::trace!("Resolving parallel moves");
        for (resolved, class) in [
            (resolved_int, RegClass::Int),
            (resolved_float, RegClass::Float),
            (resolved_vec, RegClass::Vector),
        ] {
            if resolved.is_empty() {
                continue;
            }
            let borrowed_scratch_reg = self.preferred_victim[class];
            let fixed_stack_slots = self.fixed_stack_slots;
            let slot_size = u32::try_from(self.func.spillslot_size(class))
                .expect("spill-slot size exceeds allocator index range");
            assert!(slot_size > 0, "spill-slot size must be nonzero");
            let named_by_last = self.func.multi_spillslot_named_by_last_slot();
            let scratch_resolver = MoveAndScratchResolver {
                find_free_reg: || {
                    if let Some(reg) = scratch_regs[class] {
                        log::trace!("Retrieved reg {reg} for scratch resolver");
                        scratch_regs[class] = None;
                        Some(Allocation::reg(reg))
                    } else {
                        let Some(preg) = self.lrus[class].last(avail_regs) else {
                            log::trace!("Couldn't find any reg for scratch resolver");
                            return None;
                        };
                        avail_regs.remove(preg);
                        log::trace!("Retrieved reg {preg} for scratch resolver");
                        Some(Allocation::reg(preg))
                    }
                },
                get_stackslot: || {
                    let aligned = num_spillslots.next_multiple_of(slot_size);
                    let end = aligned
                        .checked_add(slot_size)
                        .expect("spill-slot allocation overflow");
                    assert!(end as usize <= SpillSlot::MAX, "spill-slot index overflow");
                    num_spillslots = end;
                    let slot = if named_by_last { end - 1 } else { aligned };
                    let slot = SpillSlot::new(slot as usize);
                    log::trace!("Retrieved slot {slot} for scratch resolver");
                    Allocation::stack(slot)
                },
                is_stack_alloc: |alloc| {
                    alloc.is_stack()
                        || (alloc.is_reg() && fixed_stack_slots.contains(alloc.as_reg().unwrap()))
                },
                borrowed_scratch_reg,
            };
            let moves = scratch_resolver.compute(resolved);
            log::trace!("Resolved {class:?} parallel moves");
            for (from, to, _) in moves.into_iter().rev() {
                self.edits.push((
                    ProgPoint::before(inst.raw_u32()),
                    Edit::Move { from, to, class },
                ))
            }
            self.stack.num_spillslots = num_spillslots;
        }
        log::trace!("Completed processing branch");
        Ok(())
    }

    fn alloc_def_op(
        &mut self,
        op_idx: usize,
        op: Operand,
        operands: &[Operand],
        block: Block,
        inst: Inst,
    ) -> Result<(), String> {
        log::trace!("Allocating def operand {op}");
        if let OperandConstraint::Reuse(reused_idx) = op.constraint() {
            let reused_op = operands[reused_idx];
            let new_reuse_op = Operand::new(
                op.vreg(),
                reused_op.constraint(),
                OperandKind::Def,
                OperandPos::Early,
            );
            log::trace!("allocating reuse op {op} as {new_reuse_op}");
            self.process_operand_allocation(inst, new_reuse_op, op_idx)?;
        } else if self.func.is_branch(inst) {
            let mut param_spillslot = None;
            'outer: for (succ_idx, succ) in self.func.block_succs(block).iter().cloned().enumerate()
            {
                for (param_idx, branch_arg_vreg) in self
                    .func
                    .branch_blockparams(block, inst, succ_idx)
                    .iter()
                    .cloned()
                    .enumerate()
                {
                    if op.vreg() == branch_arg_vreg {
                        if matches!(
                            op.constraint(),
                            OperandConstraint::Any | OperandConstraint::Stack
                        ) {
                            let block_param = self.func.block_params(succ)[param_idx];
                            param_spillslot = Some(self.get_spillslot(block_param));
                        }
                        break 'outer;
                    }
                }
            }
            if let Some(param_spillslot) = param_spillslot {
                let spillslot = self.vreg_spillslots[op.vreg().vreg()];
                self.vreg_spillslots[op.vreg().vreg()] = param_spillslot;
                let op = Operand::new(op.vreg(), OperandConstraint::Stack, op.kind(), op.pos());
                self.process_operand_allocation(inst, op, op_idx)?;
                self.vreg_spillslots[op.vreg().vreg()] = spillslot;
            } else {
                self.process_operand_allocation(inst, op, op_idx)?;
            }
        } else {
            self.process_operand_allocation(inst, op, op_idx)?;
        }
        let slot = self.vreg_spillslots[op.vreg().vreg()];
        if slot.is_valid() {
            self.vreg_to_live_inst_range[op.vreg().vreg()].2 = Allocation::stack(slot);
            let curr_alloc = self.vreg_allocs[op.vreg().vreg()];
            let new_alloc = Allocation::stack(self.vreg_spillslots[op.vreg().vreg()]);
            if curr_alloc != new_alloc {
                self.add_move(inst, curr_alloc, new_alloc, op.class(), InstPosition::After)?;
            }
        }
        self.vreg_to_live_inst_range[op.vreg().vreg()].0 = ProgPoint::after(inst.raw_u32());
        self.freealloc(op.vreg());
        Ok(())
    }

    fn alloc_use(&mut self, op_idx: usize, op: Operand, inst: Inst) -> Result<(), String> {
        log::trace!("Allocating use op {op}");
        if self.reused_input_to_reuse_op[op_idx] != usize::MAX {
            let reuse_op_idx = self.reused_input_to_reuse_op[op_idx];
            let reuse_op_alloc = self.allocs[(inst.index(), reuse_op_idx)];
            let Some(preg) = reuse_op_alloc.as_reg() else {
                unreachable!();
            };
            let new_reused_input_constraint = OperandConstraint::FixedReg(preg);
            let new_reused_input =
                Operand::new(op.vreg(), new_reused_input_constraint, op.kind(), op.pos());
            log::trace!("Allocating reused input {op} as {new_reused_input}");
            self.process_operand_allocation(inst, new_reused_input, op_idx)?;
        } else {
            self.process_operand_allocation(inst, op, op_idx)?;
        }
        Ok(())
    }

    fn alloc_inst(&mut self, block: Block, inst: Inst) -> Result<(), String> {
        log::trace!("Allocating instruction {:?}", inst);
        self.reset_available_pregs_and_scratch_regs();
        let operands = Operands::new(self.func.inst_operands(inst));
        let clobbers = self.func.inst_clobbers(inst);
        let mut num_fixed_regs_allocatable_clobbers = 0u16;
        log::trace!("init num avail pregs: {:?}", self.num_available_pregs);
        for (op_idx, op) in operands.0.iter().cloned().enumerate() {
            if let OperandConstraint::Reuse(reused_idx) = op.constraint() {
                log::trace!("Initializing reused_input_to_reuse_op for {op}");
                self.reused_input_to_reuse_op[reused_idx] = op_idx;
                if operands.0[reused_idx].constraint() == OperandConstraint::Reg {
                    log::trace!(
                        "Counting {op} as an any-reg op that needs a reg in phase {:?}",
                        ExclusiveOperandPos::Both
                    );
                    self.num_any_reg_ops[ExclusiveOperandPos::Both][op.class()] += 1;
                    log::trace!(
                        "Decreasing num any-reg ops in phase {:?}",
                        ExclusiveOperandPos::EarlyOnly
                    );
                    self.num_any_reg_ops[ExclusiveOperandPos::EarlyOnly][op.class()] -= 1;
                }
            } else if op.constraint() == OperandConstraint::Reg {
                log::trace!(
                    "Counting {op} as an any-reg op that needs a reg in phase {:?}",
                    Into::<ExclusiveOperandPos>::into(op)
                );
                self.num_any_reg_ops[op.into()][op.class()] += 1;
            };
        }
        let mut seen = PRegSet::empty();
        for (op_idx, op) in operands.fixed() {
            let OperandConstraint::FixedReg(preg) = op.constraint() else {
                unreachable!();
            };
            self.reserve_reg_for_operand(op, op_idx, preg)?;

            if !seen.contains(preg) {
                seen.add(preg);
                if self.allocatable_regs.contains(preg) {
                    self.lrus[preg.class()].poke(preg);
                    self.num_available_pregs[op.into()][op.class()] -= 1;
                    debug_assert!(self.num_available_pregs[op.into()][op.class()] >= 0);
                    if clobbers.contains(preg) {
                        num_fixed_regs_allocatable_clobbers += 1;
                    }
                }
            }
        }
        log::trace!("avail pregs after fixed: {:?}", self.num_available_pregs);

        self.remove_clobbers_from_available_pregs(clobbers);

        for (_, op) in operands.fixed() {
            let OperandConstraint::FixedReg(preg) = op.constraint() else {
                unreachable!();
            };
            if self.vreg_in_preg[preg.index()] != VReg::invalid()
                && self.vreg_in_preg[preg.index()] != op.vreg()
            {
                log::trace!(
                    "Evicting {} from fixed register {preg}",
                    self.vreg_in_preg[preg.index()]
                );
                self.evict_vreg_in_preg(inst, preg, InstPosition::After)?;
                self.vreg_in_preg[preg.index()] = VReg::invalid();
            }
        }
        for preg in clobbers {
            if self.vreg_in_preg[preg.index()] != VReg::invalid() {
                log::trace!(
                    "Evicting {} from clobber {preg}",
                    self.vreg_in_preg[preg.index()]
                );
                self.evict_vreg_in_preg(inst, preg, InstPosition::After)?;
                self.vreg_in_preg[preg.index()] = VReg::invalid();
            }
            if self.allocatable_regs.contains(preg) {
                if num_fixed_regs_allocatable_clobbers == 0 {
                    log::trace!("Decrementing clobber avail preg");
                    self.num_available_pregs[ExclusiveOperandPos::LateOnly][preg.class()] -= 1;
                    self.num_available_pregs[ExclusiveOperandPos::Both][preg.class()] -= 1;
                    debug_assert!(
                        self.num_available_pregs[ExclusiveOperandPos::LateOnly][preg.class()] >= 0
                    );
                    debug_assert!(
                        self.num_available_pregs[ExclusiveOperandPos::Both][preg.class()] >= 0
                    );
                } else {
                    num_fixed_regs_allocatable_clobbers -= 1;
                }
            }
        }

        for (op_idx, op) in operands.late() {
            if op.kind() == OperandKind::Def {
                self.alloc_def_op(op_idx, op, operands.0, block, inst)?;
            } else {
                self.alloc_use(op_idx, op, inst)?;
            }
        }
        for (op_idx, op) in operands.early() {
            log::trace!("Allocating use operand {op}");
            if op.kind() == OperandKind::Use {
                self.alloc_use(op_idx, op, inst)?;
            } else {
                self.alloc_def_op(op_idx, op, operands.0, block, inst)?;
            }
        }

        for (op_idx, op) in operands.use_ops() {
            if op.as_fixed_nonallocatable().is_some() {
                continue;
            }
            let curr_alloc = self.vreg_allocs[op.vreg().vreg()];
            let new_alloc = self.allocs[(inst.index(), op_idx)];
            if curr_alloc != new_alloc {
                log::trace!(
                    "Adding edit from {curr_alloc:?} to {new_alloc:?} before inst {inst:?} for {op}"
                );
                self.add_move(
                    inst,
                    curr_alloc,
                    new_alloc,
                    op.class(),
                    InstPosition::Before,
                )?;
            }
        }
        if self.func.is_branch(inst) {
            self.process_branch(block, inst)?;
        }
        for entry in self.reused_input_to_reuse_op.iter_mut() {
            *entry = usize::MAX;
        }
        Ok(())
    }

    fn reload_at_begin(&mut self, block: Block) -> Result<(), String> {
        log::trace!(
            "Reloading live registers at the beginning of block {:?}",
            block
        );
        log::trace!(
            "Block params at block {:?} beginning: {:?}",
            block,
            self.func.block_params(block)
        );
        self.reset_available_pregs_and_scratch_regs();
        let first_inst = self.func.block_insns(block).first();
        for vreg in self.func.block_params(block).iter().cloned() {
            log::trace!("Processing {}", vreg);
            if self.state.vreg_allocs[vreg.vreg()] == Allocation::none() {
                continue;
            }
            let prev_alloc = self.state.vreg_allocs[vreg.vreg()];
            let slot = Allocation::stack(self.state.get_spillslot(vreg));
            self.vreg_to_live_inst_range[vreg.vreg()].2 = slot;
            self.vreg_to_live_inst_range[vreg.vreg()].0 = ProgPoint::before(first_inst.raw_u32());
            log::trace!("{} is a block param. Freeing it", vreg);
            self.freealloc(vreg);
            if slot == prev_alloc {
                log::trace!(
                    "No need to reload {} because it's already in its expected allocation",
                    vreg
                );
                continue;
            }
            log::trace!(
                "Move reason: reload {} at begin - move from its spillslot",
                vreg
            );
            self.state.add_move(
                self.func.block_insns(block).first(),
                slot,
                prev_alloc,
                vreg.class(),
                InstPosition::Before,
            )?;
        }
        let live_vregs: Vec<VReg> = self.live_vregs.iter().collect();
        for vreg in live_vregs {
            log::trace!("Processing {}", vreg);
            log::trace!(
                "{} is not a block param. It's a liveout vreg from some predecessor",
                vreg
            );
            let prev_alloc = self.state.vreg_allocs[vreg.vreg()];
            let slot = Allocation::stack(self.state.get_spillslot(vreg));
            log::trace!("Setting {}'s current allocation to its spillslot", vreg);
            self.state.vreg_allocs[vreg.vreg()] = slot;
            if let Some(preg) = prev_alloc.as_reg() {
                log::trace!("{} was in {}. Removing it", preg, vreg);
                self.state.vreg_in_preg[preg.index()] = VReg::invalid();
            }
            if slot == prev_alloc {
                log::trace!(
                    "No need to reload {} because it's already in its expected allocation",
                    vreg
                );
                continue;
            }
            log::trace!(
                "Move reason: reload {} at begin - move from its spillslot",
                vreg
            );
            self.state.add_move(
                first_inst,
                slot,
                prev_alloc,
                vreg.class(),
                InstPosition::Before,
            )?;
        }
        self.state.scratch_regs = self.state.dedicated_scratch_regs.clone();

        let get_succ_idx_of_pred = |pred, func: &F| {
            for (idx, pred_succ) in func.block_succs(pred).iter().enumerate() {
                if *pred_succ == block {
                    return idx;
                }
            }
            unreachable!(
                "{:?} was not found in the successor list of its predecessor {:?}",
                block, pred
            );
        };
        log::trace!(
            "Checking for predecessor branch args/livein vregs defined in the branch with fixed-reg constraint"
        );
        for (param_idx, block_param) in self.func.block_params(block).iter().cloned().enumerate() {
            if self.state.vreg_spillslots[block_param.vreg()].is_invalid() {
                continue;
            }
            for pred in self.func.block_preds(block).iter().cloned() {
                let pred_last_inst = self.func.block_insns(pred).last();
                let curr_block_succ_idx = get_succ_idx_of_pred(pred, self.func);
                let branch_arg_for_param =
                    self.func
                        .branch_blockparams(pred, pred_last_inst, curr_block_succ_idx)[param_idx];
                self.state.move_if_def_pred_branch(
                    block,
                    pred,
                    branch_arg_for_param,
                    self.state.vreg_spillslots[block_param.vreg()],
                )?;
            }
        }
        let live_vregs2: Vec<VReg> = self.live_vregs.iter().collect();
        for vreg in live_vregs2 {
            for pred in self.func.block_preds(block).iter().cloned() {
                let slot = self.state.vreg_spillslots[vreg.vreg()];
                self.state
                    .move_if_def_pred_branch(block, pred, vreg, slot)?;
            }
        }
        Ok(())
    }

    fn alloc_block(&mut self, block: Block) -> Result<(), String> {
        log::trace!("{:?} start", block);
        for inst in self.func.block_insns(block).iter().rev() {
            self.alloc_inst(block, inst)?;
        }
        self.reload_at_begin(block)?;
        log::trace!("{:?} end\n", block);
        Ok(())
    }

    fn run(&mut self) -> Result<(), String> {
        debug_assert_eq!(self.func.entry_block().index(), 0);
        for block in (0..self.func.num_blocks()).rev() {
            self.alloc_block(Block::new(block))?;
        }
        // Allocation emits moves while walking backwards. Reversing once at the
        // output boundary yields ascending program points and runtime order for
        // edits that share a point.
        self.edits.reverse();
        Ok(())
    }
}

impl<'a, F: Function> Deref for Env<'a, F> {
    type Target = State<'a, F>;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl<'a, F: Function> DerefMut for Env<'a, F> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

pub fn run<F: Function>(func: &F, mach_env: &MachineEnv) -> Result<Output, String> {
    let mut env = Env::new(func, mach_env);
    env.run()?;
    Ok(Output {
        allocs: env.allocs.allocs,
        inst_alloc_offsets: env.allocs.inst_alloc_offsets,
        edits: env.state.edits,
        num_spillslots: env.state.stack.num_spillslots as usize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reg_alloc::index::InstRange;

    struct TestFunction {
        int_size: usize,
        float_size: usize,
        name_multi_unit_by_last_slot: bool,
    }

    impl Function for TestFunction {
        fn num_insts(&self) -> usize {
            0
        }

        fn num_blocks(&self) -> usize {
            0
        }

        fn entry_block(&self) -> Block {
            Block::invalid()
        }

        fn block_insns(&self, _block: Block) -> InstRange {
            unreachable!()
        }

        fn block_succs(&self, _block: Block) -> &[Block] {
            &[]
        }

        fn block_preds(&self, _block: Block) -> &[Block] {
            &[]
        }

        fn block_params(&self, _block: Block) -> &[VReg] {
            &[]
        }

        fn is_ret(&self, _insn: Inst) -> bool {
            false
        }

        fn is_branch(&self, _insn: Inst) -> bool {
            false
        }

        fn branch_blockparams(&self, _block: Block, _insn: Inst, _succ_idx: usize) -> &[VReg] {
            &[]
        }

        fn inst_operands(&self, _insn: Inst) -> &[Operand] {
            &[]
        }

        fn inst_clobbers(&self, _insn: Inst) -> PRegSet {
            PRegSet::empty()
        }

        fn num_vregs(&self) -> usize {
            0
        }

        fn spillslot_size(&self, class: RegClass) -> usize {
            match class {
                RegClass::Int => self.int_size,
                RegClass::Float => self.float_size,
                RegClass::Vector => unreachable!(),
            }
        }

        fn multi_spillslot_named_by_last_slot(&self) -> bool {
            self.name_multi_unit_by_last_slot
        }
    }

    #[test]
    fn stack_allocator_aligns_mixed_size_slots() {
        let func = TestFunction {
            int_size: 1,
            float_size: 2,
            name_multi_unit_by_last_slot: false,
        };
        let mut stack = Stack::new(&func);

        assert_eq!(stack.allocstack(RegClass::Int), SpillSlot::new(0));
        assert_eq!(stack.allocstack(RegClass::Float), SpillSlot::new(2));
        assert_eq!(stack.allocstack(RegClass::Int), SpillSlot::new(4));
        assert_eq!(stack.num_spillslots, 5);
    }

    #[test]
    fn stack_allocator_can_name_multi_unit_slot_by_last_unit() {
        let func = TestFunction {
            int_size: 1,
            float_size: 2,
            name_multi_unit_by_last_slot: true,
        };
        let mut stack = Stack::new(&func);

        assert_eq!(stack.allocstack(RegClass::Float), SpillSlot::new(1));
        assert_eq!(stack.num_spillslots, 2);
    }
}
