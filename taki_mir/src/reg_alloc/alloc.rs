use core::ops::{BitAnd, BitOr, Deref, DerefMut, Index, IndexMut, Not};
use rustc_hash::FxHashMap;

use crate::reg_alloc::{
    index::Inst as RegInst,
    lru::{Lrus, PartedByRegClass},
    reg::{
        Allocation, AllocationKind, Edit, InstPosition, MachineEnv, Operand, OperandConstraint,
        OperandKind, OperandPos, Output, PReg, PRegSet, ProgPoint, RegClass, SpillSlot, VReg,
    },
    vregset::VRegSet,
};

// ---------------------------------------------------------------------------
// Helper container types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PartedByOperandPos<T> {
    pub items: [T; 2],
}

impl<T: Copy> Copy for PartedByOperandPos<T> {}

impl<T: BitAnd<Output = T> + Copy> BitAnd for PartedByOperandPos<T> {
    type Output = Self;
    fn bitand(self, other: Self) -> Self {
        Self {
            items: [self.items[0] & other.items[0], self.items[1] & other.items[1]],
        }
    }
}

impl<T: BitOr<Output = T> + Copy> BitOr for PartedByOperandPos<T> {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        Self {
            items: [self.items[0] | other.items[0], self.items[1] | other.items[1]],
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

impl<T: core::fmt::Display> core::fmt::Display for PartedByOperandPos<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
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

struct Operands(pub Vec<Operand>);

impl Operands {
    fn new(ops: &[Operand]) -> Self {
        Self(ops.to_vec())
    }

    fn use_ops(&self) -> impl Iterator<Item = (usize, Operand)> + '_ {
        self.0.iter().cloned().enumerate().filter(|(_, op)| op.kind() == OperandKind::Use)
    }

    fn fixed(&self) -> impl Iterator<Item = (usize, Operand)> + '_ {
        self.0.iter().cloned().enumerate().filter(|(_, op)| matches!(op.constraint(), OperandConstraint::FixedReg(_)))
    }

    fn late(&self) -> impl Iterator<Item = (usize, Operand)> + '_ {
        self.0.iter().cloned().enumerate().filter(|(_, op)| op.pos() == OperandPos::Late)
    }

    fn early(&self) -> impl Iterator<Item = (usize, Operand)> + '_ {
        self.0.iter().cloned().enumerate().filter(|(_, op)| op.pos() == OperandPos::Early)
    }
}

impl Index<usize> for Operands {
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
    fn new(num_insts: usize, operands_range: &[(usize, usize)]) -> (Self, u32) {
        let mut allocs = Vec::new();
        let mut inst_alloc_offsets = Vec::with_capacity(num_insts);
        let mut max_operand_len = 0;
        let mut no_of_operands = 0;
        for i in 0..num_insts {
            let len = if i < operands_range.len() {
                let (start, end) = operands_range[i];
                end - start
            } else {
                0
            };
            max_operand_len = max_operand_len.max(len as u32);
            inst_alloc_offsets.push(no_of_operands as u32);
            no_of_operands += len as u32;
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

#[derive(Debug)]
struct Stack {
    num_spillslots: u32,
    spillslot_size: u32,
}

impl Stack {
    fn new(spillslot_size: u32) -> Self {
        Self {
            num_spillslots: 0,
            spillslot_size,
        }
    }

    fn alloc_slot(&mut self) -> SpillSlot {
        let size = self.spillslot_size;
        let mut offset = self.num_spillslots;
        offset = (offset + size - 1) & !(size - 1);
        let slot = offset;
        offset += size;
        self.num_spillslots = offset;
        SpillSlot::new(slot as usize)
    }
}

// ---------------------------------------------------------------------------
// Per-instruction allocation state
// ---------------------------------------------------------------------------

pub struct State {
    edits: Vec<(ProgPoint, Edit)>,
    fixed_stack_slots: PRegSet,
    scratch_regs: PartedByRegClass<Option<PReg>>,
    dedicated_scratch_regs: PartedByRegClass<Option<PReg>>,
    available_pregs: PartedByOperandPos<PRegSet>,
    num_available_pregs: PartedByExclusiveOperandPos<PartedByRegClass<i16>>,
    vreg_allocs: Vec<Allocation>,
    vreg_spillslots: Vec<SpillSlot>,
    vreg_in_preg: Vec<VReg>,
    stack: Stack,
    lrus: Lrus,
}

impl State {
    fn is_stack_alloc(&self, alloc: Allocation) -> bool {
        alloc.is_stack()
            || (alloc.is_reg() && self.fixed_stack_slots.contains(alloc.as_reg().unwrap()))
    }

    fn get_spillslot(&mut self, vreg: VReg) -> SpillSlot {
        if self.vreg_spillslots[vreg.vreg()].is_invalid() {
            self.vreg_spillslots[vreg.vreg()] = self.stack.alloc_slot();
        }
        self.vreg_spillslots[vreg.vreg()]
    }

    fn evict_vreg_in_preg(&mut self, inst: u32, preg: PReg, pos: InstPosition) -> Result<(), String> {
        let evicted_vreg = self.vreg_in_preg[preg.index()];
        debug_assert_ne!(evicted_vreg, VReg::invalid());
        if self.vreg_spillslots[evicted_vreg.vreg()].is_invalid() {
            self.vreg_spillslots[evicted_vreg.vreg()] = self.stack.alloc_slot();
        }
        let slot = self.vreg_spillslots[evicted_vreg.vreg()];
        self.vreg_allocs[evicted_vreg.vreg()] = Allocation::stack(slot);
        self.add_move(inst, self.vreg_allocs[evicted_vreg.vreg()], Allocation::reg(preg), evicted_vreg.class(), pos)
    }

    fn alloc_scratch_reg(&mut self, inst: u32, class: RegClass, pos: InstPosition) -> Result<(), String> {
        let avail_regs =
            self.available_pregs[OperandPos::Late] & self.available_pregs[OperandPos::Early];
        if let Some(preg) = self.lrus[class].last(avail_regs) {
            if self.vreg_in_preg[preg.index()] != VReg::invalid() {
                self.evict_vreg_in_preg(inst, preg, pos)?;
            }
            self.scratch_regs[class] = Some(preg);
            self.available_pregs[OperandPos::Early].remove(preg);
            self.available_pregs[OperandPos::Late].remove(preg);
            Ok(())
        } else {
            Err("Too many live registers for scratch".to_string())
        }
    }

    fn add_move(
        &mut self,
        inst: u32,
        from: Allocation,
        to: Allocation,
        class: RegClass,
        pos: InstPosition,
    ) -> Result<(), String> {
        if self.is_stack_alloc(from) && self.is_stack_alloc(to) {
            if self.scratch_regs[class].is_none() {
                self.alloc_scratch_reg(inst, class, pos)?;
                let dec = |x: &mut i16| *x = 0i16.max(*x - 1);
                dec(&mut self.num_available_pregs[ExclusiveOperandPos::Both][class]);
                dec(&mut self.num_available_pregs[ExclusiveOperandPos::EarlyOnly][class]);
                dec(&mut self.num_available_pregs[ExclusiveOperandPos::LateOnly][class]);
            }
            let scratch = self.scratch_regs[class].unwrap();
            let sa = Allocation::reg(scratch);
            self.edits
                .push((ProgPoint::new(inst, pos), Edit::Move { from: sa, to }));
            self.edits
                .push((ProgPoint::new(inst, pos), Edit::Move { from, to: sa }));
        } else {
            self.edits
                .push((ProgPoint::new(inst, pos), Edit::Move { from, to }));
        }
        Ok(())
    }

    fn freealloc(&mut self, vreg: VReg) {
        let alloc = self.vreg_allocs[vreg.vreg()];
        match alloc.kind() {
            AllocationKind::Reg => {
                let preg = alloc.as_reg().unwrap();
                self.vreg_in_preg[preg.index()] = VReg::invalid();
            }
            AllocationKind::Stack => (),
            AllocationKind::None => unreachable!(),
        }
        self.vreg_allocs[vreg.vreg()] = Allocation::none();
    }

    fn allocd_within_constraint(&self, op: Operand, inst: u32, clobbers: &FxHashMap<RegInst, PRegSet>) -> bool {
        let alloc = self.vreg_allocs[op.vreg().vreg()];
        let inst_c = clobbers.get(&RegInst(inst)).copied().unwrap_or_default();
        match op.constraint() {
            OperandConstraint::Any => {
                if let Some(preg) = alloc.as_reg() {
                    if !self.is_stack_alloc(alloc)
                        && self.num_available_pregs[op.into()][op.class()] < 0
                    {
                        return false;
                    }
                    if !self.available_pregs[op.pos()].contains(preg) {
                        self.vreg_in_preg[preg.index()] == op.vreg()
                            && (op.pos() != OperandPos::Late || !inst_c.contains(preg))
                    } else {
                        true
                    }
                } else {
                    !alloc.is_none()
                }
            }
            OperandConstraint::Reg => {
                if self.is_stack_alloc(alloc) {
                    return false;
                }
                if let Some(preg) = alloc.as_reg() {
                    if !self.available_pregs[op.pos()].contains(preg) {
                        self.vreg_in_preg[preg.index()] == op.vreg()
                            && (op.pos() != OperandPos::Late || !inst_c.contains(preg))
                    } else {
                        true
                    }
                } else {
                    false
                }
            }
            OperandConstraint::FixedReg(preg) => alloc.is_reg() && alloc.as_reg().unwrap() == preg,
            OperandConstraint::Reuse(_) => unreachable!(),
            OperandConstraint::Stack => self.is_stack_alloc(alloc),
            OperandConstraint::Limit(_) => true,
        }
    }

    fn select_suitable_reg_in_lru(&self, op: Operand) -> Result<PReg, String> {
        let draw_from = match (op.pos(), op.kind()) {
            (OperandPos::Late, OperandKind::Use) | (OperandPos::Early, OperandKind::Def) => {
                self.available_pregs[OperandPos::Late] & self.available_pregs[OperandPos::Early]
            }
            _ => self.available_pregs[op.pos()],
        };
        if draw_from.is_empty(op.class()) {
            return Err("No registers available".to_string());
        }
        self.lrus[op.class()].last(draw_from).ok_or("Failed to find reg in LRU".to_string())
    }

    fn alloc_reg_for_operand(&mut self, inst: u32, op: Operand) -> Result<Allocation, String> {
        let preg = self.select_suitable_reg_in_lru(op)?;
        if self.vreg_in_preg[preg.index()] != VReg::invalid() {
            self.evict_vreg_in_preg(inst, preg, InstPosition::After)?;
        }
        self.lrus[op.class()].poke(preg);
        self.available_pregs[op.pos()].remove(preg);
        match (op.pos(), op.kind()) {
            (OperandPos::Late, OperandKind::Use) => {
                self.available_pregs[OperandPos::Early].remove(preg);
            }
            (OperandPos::Early, OperandKind::Def) => {
                self.available_pregs[OperandPos::Late].remove(preg);
            }
            _ => (),
        }
        Ok(Allocation::reg(preg))
    }

    fn alloc_operand(&mut self, inst: u32, op: Operand) -> Result<Allocation, String> {
        Ok(match op.constraint() {
            OperandConstraint::Any => {
                if (op.kind() == OperandKind::Def
                    && self.vreg_allocs[op.vreg().vreg()] == Allocation::none())
                    || self.num_available_pregs[op.into()][op.class()]
                        < self.num_available_pregs[op.into()][op.class()]
                {
                    Allocation::stack(self.get_spillslot(op.vreg()))
                } else {
                    self.alloc_reg_for_operand(inst, op)
                        .unwrap_or_else(|_| Allocation::stack(self.get_spillslot(op.vreg())))
                }
            }
            OperandConstraint::Reg => {
                let alloc = self.alloc_reg_for_operand(inst, op)?;
                self.num_available_pregs[op.into()][op.class()] -= 1;
                alloc
            }
            OperandConstraint::FixedReg(preg) => Allocation::reg(preg),
            OperandConstraint::Reuse(_) => unreachable!(),
            OperandConstraint::Stack => Allocation::stack(self.get_spillslot(op.vreg())),
            OperandConstraint::Limit(_) => self.alloc_reg_for_operand(inst, op)?,
        })
    }
}

// ---------------------------------------------------------------------------
// VCode reference (simplified view for allocator)
// ---------------------------------------------------------------------------

pub struct VCodeRef<'a> {
    pub num_insts: usize,
    pub num_blocks: usize,
    pub num_vregs: usize,
    pub operands: &'a [Operand],
    pub operands_range: &'a [(usize, usize)],
    pub block_range: &'a [(usize, usize)],
    pub block_succ: &'a [u32],
    pub block_succ_range: &'a [(usize, usize)],
    pub block_pred: &'a [u32],
    pub block_pred_range: &'a [(usize, usize)],
    pub block_params: &'a [VReg],
    pub block_params_range: &'a [(usize, usize)],
    pub branch_block_args: &'a [VReg],
    pub branch_block_args_range: &'a [(usize, usize)],
    pub clobbers: &'a FxHashMap<RegInst, PRegSet>,
    pub spillslot_size: u32,
}

impl VCodeRef<'_> {
    pub fn block_insts(&self, block: usize) -> (usize, usize) {
        if block >= self.block_range.len() {
            return (0, 0);
        }
        self.block_range[block]
    }

    pub fn block_succs(&self, block: usize) -> &[u32] {
        if block >= self.block_succ_range.len() {
            return &[];
        }
        let (s, e) = self.block_succ_range[block];
        &self.block_succ[s..e]
    }

    pub fn block_preds(&self, block: usize) -> &[u32] {
        if block >= self.block_pred_range.len() {
            return &[];
        }
        let (s, e) = self.block_pred_range[block];
        &self.block_pred[s..e]
    }

    pub fn block_params_for(&self, block: usize) -> &[VReg] {
        if block >= self.block_params_range.len() {
            return &[];
        }
        let (s, e) = self.block_params_range[block];
        &self.block_params[s..e]
    }

    pub fn inst_operands(&self, inst: usize) -> &[Operand] {
        if inst >= self.operands_range.len() {
            return &[];
        }
        let (s, e) = self.operands_range[inst];
        &self.operands[s..e]
    }

    pub fn branch_blockparams(&self, block: usize, inst: usize, succ_idx: usize) -> &[VReg] {
        if inst >= self.branch_block_args_range.len() {
            return &[];
        }
        let (br_start, br_end) = self.branch_block_args_range[inst];
        let succs = self.block_succs(block);
        let mut offset = br_start;
        for s in 0..succ_idx.min(succs.len()) {
            let sb = succs[s] as usize;
            offset += self.block_params_for(sb).len();
        }
        if succ_idx < succs.len() {
            let sb = succs[succ_idx] as usize;
            let n = self.block_params_for(sb).len();
            if offset + n <= br_end {
                &self.branch_block_args[offset..offset + n]
            } else {
                &[]
            }
        } else {
            &[]
        }
    }

    pub fn is_branch(&self, block: usize, inst: usize) -> bool {
        if block >= self.block_range.len() {
            return false;
        }
        let (_, end) = self.block_range[block];
        inst + 1 == end && !self.block_succs(block).is_empty()
    }

    pub fn inst_clobbers(&self, inst: usize) -> PRegSet {
        self.clobbers
            .get(&RegInst(inst as u32))
            .copied()
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Top-level allocator environment
// ---------------------------------------------------------------------------

pub struct Env<'a> {
    allocatable_regs: PRegSet,
    init_available_pregs: PRegSet,
    init_num_available_pregs: PartedByRegClass<i16>,

    live_vregs: VRegSet,

    reused_input_to_reuse_op: Vec<usize>,

    num_any_reg_ops: PartedByExclusiveOperandPos<PartedByRegClass<i16>>,

    preferred_victim: PartedByRegClass<PReg>,

    vcode: &'a VCodeRef<'a>,

    allocs: Allocs,
    state: State,
}

impl<'a> Env<'a> {
    pub fn new(vcode: &'a VCodeRef<'a>, env: &'a MachineEnv) -> Self {
        let mut regs = [
            env.preferred_regs_by_class[RegClass::Int as usize],
            env.preferred_regs_by_class[RegClass::Float as usize],
            env.preferred_regs_by_class[RegClass::Vector as usize],
        ];
        regs[0].union_from(env.non_preferred_regs_by_class[RegClass::Int as usize]);
        regs[1].union_from(env.non_preferred_regs_by_class[RegClass::Float as usize]);
        regs[2].union_from(env.non_preferred_regs_by_class[RegClass::Vector as usize]);

        let allocatable_regs = PRegSet::from(env);
        let num_avail: PartedByRegClass<i16> = PartedByRegClass {
            items: [
                (env.preferred_regs_by_class[RegClass::Int as usize].len()
                    + env.non_preferred_regs_by_class[RegClass::Int as usize].len()) as i16,
                (env.preferred_regs_by_class[RegClass::Float as usize].len()
                    + env.non_preferred_regs_by_class[RegClass::Float as usize].len()) as i16,
                (env.preferred_regs_by_class[RegClass::Vector as usize].len()
                    + env.non_preferred_regs_by_class[RegClass::Vector as usize].len()) as i16,
            ],
        };

        let mut init_avail = allocatable_regs;
        for preg in env.fixed_stack_slots.iter() {
            init_avail.add(*preg);
        }

        let dedicated_scratch = PartedByRegClass {
            items: [
                env.scratch_by_class[0],
                env.scratch_by_class[1],
                env.scratch_by_class[2],
            ],
        };

        let (allocs, max_op_len) = Allocs::new(vcode.num_insts, vcode.operands_range);
        let fix_stack = PRegSet::from_iter(env.fixed_stack_slots.iter().cloned());
        let spillslot_size = vcode.spillslot_size;

        Self {
            allocatable_regs,
            live_vregs: VRegSet::with_capacity(vcode.num_vregs),
            preferred_victim: PartedByRegClass {
                items: [
                    regs[0].max_preg().unwrap_or(PReg::invalid()),
                    regs[1].max_preg().unwrap_or(PReg::invalid()),
                    regs[2].max_preg().unwrap_or(PReg::invalid()),
                ],
            },
            reused_input_to_reuse_op: vec![usize::MAX; max_op_len as usize],
            init_available_pregs: init_avail,
            init_num_available_pregs: num_avail.clone(),
            num_any_reg_ops: PartedByExclusiveOperandPos {
                items: [
                    PartedByRegClass { items: [0; 3] },
                    PartedByRegClass { items: [0; 3] },
                    PartedByRegClass { items: [0; 3] },
                ],
            },
            allocs,
            state: State {
                edits: Vec::with_capacity(vcode.num_insts),
                fixed_stack_slots: fix_stack,
                scratch_regs: dedicated_scratch.clone(),
                dedicated_scratch_regs: dedicated_scratch,
                num_available_pregs: PartedByExclusiveOperandPos {
                    items: [num_avail.clone(), num_avail.clone(), num_avail.clone()],
                },
                available_pregs: PartedByOperandPos {
                    items: [init_avail, init_avail],
                },
                lrus: Lrus::new(&regs[0], &regs[1], &regs[2]),
                vreg_in_preg: vec![VReg::invalid(); PReg::NUM_INDEX],
                stack: Stack::new(spillslot_size),
                vreg_allocs: vec![Allocation::none(); vcode.num_vregs],
                vreg_spillslots: vec![SpillSlot::invalid(); vcode.num_vregs],
            },
            vcode,
        }
    }

    fn reset_available_pregs(&mut self) {
        self.state.available_pregs = PartedByOperandPos {
            items: [self.init_available_pregs, self.init_available_pregs],
        };
        self.state.scratch_regs = self.state.dedicated_scratch_regs.clone();
        self.state.num_available_pregs = PartedByExclusiveOperandPos {
            items: [self.init_num_available_pregs; 3],
        };
        debug_assert_eq!(
            self.num_any_reg_ops,
            PartedByExclusiveOperandPos {
                items: [PartedByRegClass { items: [0; 3] }; 3]
            }
        );
    }
}

impl<'a> Deref for Env<'a> {
    type Target = State;
    fn deref(&self) -> &State {
        &self.state
    }
}

impl<'a> DerefMut for Env<'a> {
    fn deref_mut(&mut self) -> &mut State {
        &mut self.state
    }
}

// ---------------------------------------------------------------------------
// Core allocation logic
// ---------------------------------------------------------------------------

impl Env<'_> {
    fn reserve_reg_for_operand(&mut self, op: Operand, op_idx: usize, preg: PReg) -> Result<(), String> {
        let ea = self.available_pregs[OperandPos::Early];
        let la = self.available_pregs[OperandPos::Late];
        match (op.pos(), op.kind()) {
            (OperandPos::Early, OperandKind::Use) => {
                if op.as_fixed_nonallocatable().is_none() && !ea.contains(preg) {
                    return Err("fixed reg not avail".to_string());
                }
                self.available_pregs[OperandPos::Early].remove(preg);
                if self.reused_input_to_reuse_op[op_idx] != usize::MAX {
                    if op.as_fixed_nonallocatable().is_none() && !la.contains(preg) {
                        return Err("fixed reg not avail".to_string());
                    }
                    self.available_pregs[OperandPos::Late].remove(preg);
                }
            }
            (OperandPos::Late, OperandKind::Def) => {
                if op.as_fixed_nonallocatable().is_none() && !la.contains(preg) {
                    return Err("fixed reg not avail".to_string());
                }
                self.available_pregs[OperandPos::Late].remove(preg);
            }
            _ => {
                if op.as_fixed_nonallocatable().is_none()
                    && (!ea.contains(preg) || !la.contains(preg))
                {
                    return Err("fixed reg not avail".to_string());
                }
                self.available_pregs[OperandPos::Early].remove(preg);
                self.available_pregs[OperandPos::Late].remove(preg);
            }
        }
        Ok(())
    }

    fn remove_clobbers_from_available_pregs(&mut self, clobbers: PRegSet) {
        let inv = clobbers.invert();
        self.available_pregs[OperandPos::Late].intersect_from(inv);
    }

    fn process_operand_allocation(&mut self, inst: u32, op: Operand, op_idx: usize) -> Result<(), String> {
        if let Some(preg) = op.as_fixed_nonallocatable() {
            self.allocs[(inst as usize, op_idx)] = Allocation::reg(preg);
            return Ok(());
        }

        if !self.allocd_within_constraint(op, inst, &self.vcode.clobbers) {
            let curr = self.vreg_allocs[op.vreg().vreg()];
            let new = self.alloc_operand(inst, op)?;

            if curr.is_none() {
                self.live_vregs.insert(op.vreg());
                self.vreg_allocs[op.vreg().vreg()] = new;
                if let Some(p) = new.as_reg() {
                    self.vreg_in_preg[p.index()] = op.vreg();
                }
            } else {
                if op.kind() == OperandKind::Def {
                    self.add_move(inst, new, curr, op.class(), InstPosition::After)?;
                }
                if let Some(p) = new.as_reg() {
                    self.vreg_in_preg[p.index()] = VReg::invalid();
                }
            }
            self.allocs[(inst as usize, op_idx)] = new;
        } else {
            self.allocs[(inst as usize, op_idx)] = self.vreg_allocs[op.vreg().vreg()];
            if op.constraint() == OperandConstraint::Reg {
                self.num_any_reg_ops[op.into()][op.class()] -= 1;
            }
            if let Some(p) = self.allocs[(inst as usize, op_idx)].as_reg() {
                if self.allocatable_regs.contains(p) {
                    self.lrus[p.class()].poke(p);
                }
                self.available_pregs[op.pos()].remove(p);
                match (op.pos(), op.kind()) {
                    (OperandPos::Late, OperandKind::Use) => {
                        self.available_pregs[OperandPos::Early].remove(p);
                    }
                    (OperandPos::Early, OperandKind::Def) => {
                        self.available_pregs[OperandPos::Late].remove(p);
                    }
                    _ => (),
                }
            }
        }
        Ok(())
    }

    fn alloc_def_op(
        &mut self,
        op_idx: usize,
        op: Operand,
        operands: &Operands,
        block: usize,
        inst: u32,
    ) -> Result<(), String> {
        if let OperandConstraint::Reuse(reused_idx) = op.constraint() {
            let reused = operands[reused_idx];
            let new_op = Operand::new(op.vreg(), reused.constraint(), OperandKind::Def, OperandPos::Early);
            self.process_operand_allocation(inst, new_op, op_idx)?;
        } else {
            self.process_operand_allocation(inst, op, op_idx)?;
        }

        let slit = self.vreg_spillslots[op.vreg().vreg()];
        if slit.is_valid() {
            let curr = self.vreg_allocs[op.vreg().vreg()];
            let nalloc = Allocation::stack(self.vreg_spillslots[op.vreg().vreg()]);
            if curr != nalloc {
                self.add_move(inst, curr, nalloc, op.class(), InstPosition::After)?;
            }
        }
        self.freealloc(op.vreg());
        Ok(())
    }

    fn alloc_use(&mut self, op_idx: usize, op: Operand, inst: u32) -> Result<(), String> {
        if self.reused_input_to_reuse_op[op_idx] != usize::MAX {
            let reuse_idx = self.reused_input_to_reuse_op[op_idx];
            let reuse_alloc = self.allocs[(inst as usize, reuse_idx)];
            let preg = reuse_alloc.as_reg().expect("Reuse input must be in reg");
            let new_op = Operand::new(op.vreg(), OperandConstraint::FixedReg(preg), op.kind(), op.pos());
            self.process_operand_allocation(inst, new_op, op_idx)?;
        } else {
            self.process_operand_allocation(inst, op, op_idx)?;
        }
        Ok(())
    }

    fn alloc_inst(&mut self, block: usize, inst: u32) -> Result<(), String> {
        self.reset_available_pregs();

        let operands = Operands::new(self.vcode.inst_operands(inst as usize));
        let clobbers = self.vcode.inst_clobbers(inst as usize);
        let mut fixed_clobber_count = 0u16;

        // First pass: count reg-only operands and record reuse
        for (op_idx, op) in operands.0.iter().cloned().enumerate() {
            if let OperandConstraint::Reuse(reused_idx) = op.constraint() {
                self.reused_input_to_reuse_op[reused_idx] = op_idx;
                if operands[reused_idx].constraint() == OperandConstraint::Reg {
                    self.num_any_reg_ops[ExclusiveOperandPos::Both][op.class()] += 1;
                    self.num_any_reg_ops[ExclusiveOperandPos::EarlyOnly][op.class()] -= 1;
                }
            } else if op.constraint() == OperandConstraint::Reg {
                self.num_any_reg_ops[op.into()][op.class()] += 1;
            }
        }

        // Reserve fixed registers
        let mut seen = PRegSet::empty();
        for (op_idx, op) in operands.fixed() {
            let OperandConstraint::FixedReg(preg) = op.constraint() else {
                unreachable!()
            };
            self.reserve_reg_for_operand(op, op_idx, preg)?;

            if !seen.contains(preg) {
                seen.add(preg);
                if self.allocatable_regs.contains(preg) {
                    self.lrus[preg.class()].poke(preg);
                    self.num_available_pregs[op.into()][op.class()] -= 1;
                    if clobbers.contains(preg) {
                        fixed_clobber_count += 1;
                    }
                }
            }
        }

        self.remove_clobbers_from_available_pregs(clobbers);

        // Evict from fixed registers
        for (_, op) in operands.fixed() {
            let OperandConstraint::FixedReg(p) = op.constraint() else { unreachable!() };
            if self.vreg_in_preg[p.index()] != VReg::invalid()
                && self.vreg_in_preg[p.index()] != op.vreg()
            {
                self.evict_vreg_in_preg(inst, p, InstPosition::After)?;
                self.vreg_in_preg[p.index()] = VReg::invalid();
            }
        }
        // Evict from clobbers
        for p in clobbers {
            if self.vreg_in_preg[p.index()] != VReg::invalid() {
                self.evict_vreg_in_preg(inst, p, InstPosition::After)?;
                self.vreg_in_preg[p.index()] = VReg::invalid();
            }
            if self.allocatable_regs.contains(p) {
                if fixed_clobber_count == 0 {
                    self.num_available_pregs[ExclusiveOperandPos::LateOnly][p.class()] -= 1;
                    self.num_available_pregs[ExclusiveOperandPos::Both][p.class()] -= 1;
                } else {
                    fixed_clobber_count -= 1;
                }
            }
        }

        // Process late operands: defs first, then uses
        for (op_idx, op) in operands.late() {
            if op.kind() == OperandKind::Def {
                self.alloc_def_op(op_idx, op, &operands, block, inst)?;
            } else {
                self.alloc_use(op_idx, op, inst)?;
            }
        }
        // Process early operands: uses first, then defs
        for (op_idx, op) in operands.early() {
            if op.kind() == OperandKind::Use {
                self.alloc_use(op_idx, op, inst)?;
            } else {
                self.alloc_def_op(op_idx, op, &operands, block, inst)?;
            }
        }

        // Insert before-moves for use operands whose allocation changed
        for (op_idx, op) in operands.use_ops() {
            if op.as_fixed_nonallocatable().is_some() {
                continue;
            }
            let curr = self.vreg_allocs[op.vreg().vreg()];
            let new = self.allocs[(inst as usize, op_idx)];
            if curr != new {
                self.add_move(inst, curr, new, op.class(), InstPosition::Before)?;
            }
        }

        if self.vcode.is_branch(block, inst as usize) {
            self.process_branch(block, inst)?;
        }

        for entry in self.reused_input_to_reuse_op.iter_mut() {
            *entry = usize::MAX;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Branch processing with parallel move resolution
    // ------------------------------------------------------------------

    fn process_branch(&mut self, block: usize, inst: u32) -> Result<(), String> {
        let mut int_moves: Vec<(Allocation, Allocation, VReg)> = Vec::new();
        let mut float_moves: Vec<(Allocation, Allocation, VReg)> = Vec::new();

        for (succ_idx, &succ) in self.vcode.block_succs(block).iter().enumerate() {
            let succ = succ as usize;
            let branch_args = self.vcode.branch_blockparams(block, inst as usize, succ_idx);
            let succ_params = self.vcode.block_params_for(succ);

            for pos in 0..branch_args.len().min(succ_params.len()) {
                let vreg = branch_args[pos];
                let param_vreg = succ_params[pos];

                // Skip if vreg is defined on this branch instruction
                if self.vcode.inst_operands(inst as usize)
                    .iter()
                    .any(|op| op.vreg() == vreg && op.kind() == OperandKind::Def)
                {
                    continue;
                }

                if self.vreg_spillslots[param_vreg.vreg()].is_invalid() {
                    self.vreg_spillslots[param_vreg.vreg()] = self.stack.alloc_slot();
                }
                if self.vreg_spillslots[vreg.vreg()].is_invalid() {
                    self.vreg_spillslots[vreg.vreg()] = self.stack.alloc_slot();
                }

                let vreg_spill = Allocation::stack(self.vreg_spillslots[vreg.vreg()]);
                let curr = self.vreg_allocs[vreg.vreg()];
                if curr.is_none() {
                    self.live_vregs.insert(vreg);
                } else if curr != vreg_spill {
                    self.add_move(inst, vreg_spill, curr, vreg.class(), InstPosition::Before)?;
                }
                self.vreg_allocs[vreg.vreg()] = vreg_spill;

                let from = Allocation::stack(self.vreg_spillslots[vreg.vreg()]);
                let to = Allocation::stack(self.vreg_spillslots[param_vreg.vreg()]);
                match vreg.class() {
                    RegClass::Int => int_moves.push((from, to, vreg)),
                    RegClass::Float => float_moves.push((from, to, vreg)),
                    RegClass::Vector => {} // ignore for now
                }
            }
        }

        // Resolve parallel moves with scratch register support
        for (moves, class) in [
            (int_moves, RegClass::Int),
            (float_moves, RegClass::Float),
        ] {
            let resolved = self.resolve_parallel_moves(moves, class, inst)?;
            for (from, to, _) in resolved.iter().rev() {
                self.edits.push((
                    ProgPoint::before(inst),
                    Edit::Move { from: *from, to: *to },
                ));
            }
        }
        Ok(())
    }

    fn resolve_parallel_moves(
        &mut self,
        mut moves: Vec<(Allocation, Allocation, VReg)>,
        class: RegClass,
        inst: u32,
    ) -> Result<Vec<(Allocation, Allocation, VReg)>, String> {
        // Remove moves to self and duplicates
        moves.retain(|(from, to, _)| from != to);
        moves.sort_by_key(|(_, to, _)| to.bits());
        moves.dedup();

        if moves.len() <= 1 {
            return Ok(moves);
        }

        // Check if any destinations overlap sources
        let has_cycle = moves.iter().any(|(src, _, _)| {
            moves.binary_search_by_key(&src.bits(), |(_, dst, _)| dst.bits()).is_ok()
        });

        if !has_cycle {
            return Ok(moves);
        }

        // Build dependency graph
        const NONE: usize = usize::MAX;
        let must_come_before: Vec<usize> = moves
            .iter()
            .map(|(src, _, _)| {
                moves
                    .binary_search_by_key(&src.bits(), |(_, dst, _)| dst.bits())
                    .unwrap_or(NONE)
            })
            .collect();

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        enum State { ToDo, Pending, Done }

        let mut result: Vec<(Allocation, Allocation, VReg)> = Vec::new();
        let mut stack: Vec<usize> = Vec::new();
        let mut state: Vec<State> = vec![State::ToDo; moves.len()];
        let mut scratch_used = false;

        while let Some(next) = state.iter().position(|&s| s == State::ToDo) {
            stack.push(next);
            state[next] = State::Pending;

            while let Some(&top) = stack.last() {
                debug_assert_eq!(state[top], State::Pending);
                let next = must_come_before[top];
                if next == NONE || state[next] == State::Done {
                    result.push(moves[top]);
                    state[top] = State::Done;
                    stack.pop();
                    while let Some(t) = stack.pop() {
                        result.push(moves[t]);
                        state[t] = State::Done;
                    }
                } else if state[next] == State::ToDo {
                    stack.push(next);
                    state[next] = State::Pending;
                } else {
                    // Cycle detected
                    debug_assert_ne!(top, next);
                    state[top] = State::Done;
                    stack.pop();

                    let (scratch_src, dst, dst_vreg) = moves[top];
                    scratch_used = true;

                    // Find a scratch register
                    let scratch = self.find_scratch_for_move(class);
                    result.push((Allocation::none(), dst, dst_vreg));
                    while let Some(mi) = stack.pop() {
                        state[mi] = State::Done;
                        result.push(moves[mi]);
                        if mi == next { break; }
                    }
                    result.push((scratch_src, Allocation::none(), VReg::invalid()));
                }
            }
        }

        result.reverse();

        if scratch_used {
            // Fill in scratch with actual register or stack slot
            let scratch = self.find_scratch_for_move(class);
            for (src, dst, _) in &mut result {
                if src.is_none() { *src = scratch; }
                if dst.is_none() { *dst = scratch; }
            }
        }

        Ok(result)
    }

    fn find_scratch_for_move(&self, class: RegClass) -> Allocation {
        let avail = self.available_pregs[OperandPos::Late]
            & self.available_pregs[OperandPos::Early];
        if let Some(preg) = self.lrus[class].last(avail) {
            Allocation::reg(preg)
        } else {
            // Fall back to a stack slot - but we don't want to mutate stack here.
            // Use a high-numbered fake slot.
            // In practice, the caller should provide a scratch register.
            if let Some(scratch_preg) = self.scratch_regs[class] {
                Allocation::reg(scratch_preg)
            } else {
                // Emergency: use a stack slot
                Allocation::stack(SpillSlot::new(0xFFFF))
            }
        }
    }

    // ------------------------------------------------------------------
    // Block boundary handling
    // ------------------------------------------------------------------

    fn reload_at_begin(&mut self, block: usize) -> Result<(), String> {
        self.reset_available_pregs();
        let (first, _) = self.vcode.block_insts(block);
        let first_inst = first as u32;

        // Block params
        for &vreg in self.vcode.block_params_for(block) {
            if self.vreg_allocs[vreg.vreg()] == Allocation::none() {
                continue;
            }
            let prev = self.vreg_allocs[vreg.vreg()];
            let slot = Allocation::stack(self.get_spillslot(vreg));
            self.freealloc(vreg);
            if slot != prev {
                self.add_move(first_inst, slot, prev, vreg.class(), InstPosition::Before)?;
            }
        }

        // Live-in vregs
        let live_vregs: Vec<VReg> = self.live_vregs.iter().collect();
        for vreg in live_vregs {
            let prev = self.vreg_allocs[vreg.vreg()];
            let slot = Allocation::stack(self.get_spillslot(vreg));
            self.vreg_allocs[vreg.vreg()] = slot;
            if let Some(p) = prev.as_reg() {
                self.vreg_in_preg[p.index()] = VReg::invalid();
            }
            if slot != prev {
                self.add_move(first_inst, slot, prev, vreg.class(), InstPosition::Before)?;
            }
        }

        self.scratch_regs = self.dedicated_scratch_regs.clone();

        // Check for branch args defined with fixed-reg on predecessor branch
        let block_params: Vec<VReg> = self.vcode.block_params_for(block).to_vec();
        for (param_idx, &param) in block_params.iter().enumerate() {
            if self.vreg_spillslots[param.vreg()].is_invalid() {
                continue;
            }
            for &pred in self.vcode.block_preds(block) {
                let pred = pred as usize;
                let idx = self.vcode.block_succs(pred).iter()
                    .position(|&s| s as usize == block);
                let Some(succ_idx) = idx else { continue };
                let (_, end) = self.vcode.block_insts(pred);
                if end == 0 { continue; }
                let last = (end - 1) as u32;
                let args = self.vcode.branch_blockparams(pred, last as usize, succ_idx);
                if param_idx < args.len() {
                    let arg = args[param_idx];
                    self.move_if_def_pred_branch(block, pred, arg, self.vreg_spillslots[param.vreg()])?;
                }
            }
        }
        let vregs: Vec<VReg> = self.live_vregs.iter().collect();
        for vreg in vregs {
            for &pred in self.vcode.block_preds(block) {
                self.move_if_def_pred_branch(block, pred as usize, vreg, self.vreg_spillslots[vreg.vreg()])?;
            }
        }

        Ok(())
    }

    fn move_if_def_pred_branch(
        &mut self,
        block: usize,
        pred: usize,
        vreg: VReg,
        slot: SpillSlot,
    ) -> Result<(), String> {
        let (_, end) = self.vcode.block_insts(pred);
        if end == 0 { return Ok(()); }
        let last = (end - 1) as u32;
        for op in self.vcode.inst_operands(last as usize).iter() {
            if op.kind() == OperandKind::Def && op.vreg() == vreg {
                if self.vcode.block_preds(block).len() > 1 {
                    panic!("Multi-pred for branch-arg defined on branch");
                }
                match op.constraint() {
                    OperandConstraint::FixedReg(preg) => {
                        let f = self.vcode.block_insts(block).0 as u32;
                        self.add_move(f, Allocation::reg(preg), Allocation::stack(slot), vreg.class(), InstPosition::Before)?;
                    }
                    _ => {}
                }
                break;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Main loop
    // ------------------------------------------------------------------

    fn alloc_block(&mut self, block: usize) -> Result<(), String> {
        let (start, end) = self.vcode.block_insts(block);
        for i in (start..end).rev() {
            self.alloc_inst(block, i as u32)?;
        }
        self.reload_at_begin(block)?;
        Ok(())
    }

    fn run(&mut self) -> Result<(), String> {
        for block in (0..self.vcode.num_blocks).rev() {
            self.alloc_block(block)?;
        }
        self.state.edits.reverse();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run(vcode: &VCodeRef, mach_env: &MachineEnv) -> Result<Output, String> {
    let mut env = Env::new(vcode, mach_env);
    env.run()?;

    Ok(Output {
        allocs: env.allocs.allocs,
        inst_alloc_offsets: env.allocs.inst_alloc_offsets,
        edits: env.state.edits,
        num_spillslots: env.state.stack.num_spillslots as usize,
    })
}
