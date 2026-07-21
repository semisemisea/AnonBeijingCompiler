use crate::reg_alloc::reg::{Allocation, PReg};
use core::fmt::Debug;
use smallvec::{smallvec, SmallVec};

pub type MoveVec<T> = SmallVec<[(Allocation, Allocation, T); 16]>;

#[derive(Clone, Debug)]
pub enum MoveVecWithScratch<T> {
    NoScratch(MoveVec<T>),
    Scratch(MoveVec<T>),
}

pub struct ParallelMoves<T: Clone + Copy + Default> {
    parallel_moves: MoveVec<T>,
}

impl<T: Clone + Copy + Default + PartialEq> ParallelMoves<T> {
    pub fn new() -> Self {
        Self {
            parallel_moves: smallvec![],
        }
    }

    pub fn add(&mut self, from: Allocation, to: Allocation, t: T) {
        self.parallel_moves.push((from, to, t));
    }

    fn sources_overlap_dests(&self) -> bool {
        for &(src, _, _) in &self.parallel_moves {
            if self
                .parallel_moves
                .binary_search_by_key(&src, |&(_, dst, _)| dst)
                .is_ok()
            {
                return true;
            }
        }
        false
    }

    pub fn resolve(mut self) -> MoveVecWithScratch<T> {
        if self.parallel_moves.len() <= 1 {
            return MoveVecWithScratch::NoScratch(self.parallel_moves);
        }

        self.parallel_moves
            .sort_by_key(|&(src, dst, _)| u64_key(dst.bits(), src.bits()));

        self.parallel_moves.dedup();

        if cfg!(debug_assertions) {
            let mut last_dst = None;
            for &(_, dst, _) in &self.parallel_moves {
                if last_dst.is_some() {
                    debug_assert!(last_dst.unwrap() != dst);
                }
                last_dst = Some(dst);
            }
        }

        self.parallel_moves
            .retain(|&mut (src, dst, _)| src != dst);

        if !self.sources_overlap_dests() {
            return MoveVecWithScratch::NoScratch(self.parallel_moves);
        }

        const NONE: usize = usize::MAX;
        let must_come_before: SmallVec<[usize; 16]> = self
            .parallel_moves
            .iter()
            .map(|&(src, _, _)| {
                self.parallel_moves
                    .binary_search_by_key(&src, |&(_, dst, _)| dst)
                    .unwrap_or(NONE)
            })
            .collect();

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        enum State {
            ToDo,
            Pending,
            Done,
        }
        let mut ret: MoveVec<T> = smallvec![];
        let mut stack: SmallVec<[usize; 16]> = smallvec![];
        let mut state: SmallVec<[State; 16]> =
            smallvec![State::ToDo; self.parallel_moves.len()];
        let mut scratch_used = false;

        while let Some(next) = state.iter().position(|&state| state == State::ToDo) {
            stack.push(next);
            state[next] = State::Pending;

            while let Some(&top) = stack.last() {
                debug_assert_eq!(state[top], State::Pending);
                let next = must_come_before[top];
                if next == NONE || state[next] == State::Done {
                    ret.push(self.parallel_moves[top]);
                    state[top] = State::Done;
                    stack.pop();
                    while let Some(top) = stack.pop() {
                        ret.push(self.parallel_moves[top]);
                        state[top] = State::Done;
                    }
                } else if state[next] == State::ToDo {
                    stack.push(next);
                    state[next] = State::Pending;
                } else {
                    debug_assert_ne!(top, next);
                    state[top] = State::Done;
                    stack.pop();

                    let (scratch_src, dst, dst_t) = self.parallel_moves[top];
                    scratch_used = true;

                    ret.push((Allocation::none(), dst, dst_t));
                    while let Some(move_idx) = stack.pop() {
                        state[move_idx] = State::Done;
                        ret.push(self.parallel_moves[move_idx]);

                        if move_idx == next {
                            break;
                        }
                    }
                    ret.push((scratch_src, Allocation::none(), T::default()));
                }
            }
        }

        ret.reverse();

        if scratch_used {
            MoveVecWithScratch::Scratch(ret)
        } else {
            MoveVecWithScratch::NoScratch(ret)
        }
    }
}

impl<T> MoveVecWithScratch<T> {
    pub fn with_scratch(self, scratch: Allocation) -> MoveVec<T> {
        match self {
            MoveVecWithScratch::NoScratch(moves) => moves,
            MoveVecWithScratch::Scratch(mut moves) => {
                for (src, dst, _) in &mut moves {
                    debug_assert!(
                        *src != scratch && *dst != scratch,
                    );
                    debug_assert!(
                        !(src.is_none() && dst.is_none()),
                    );
                    if src.is_none() {
                        *src = scratch;
                    }
                    if dst.is_none() {
                        *dst = scratch;
                    }
                }
                moves
            }
        }
    }

    pub fn without_scratch(self) -> Option<MoveVec<T>> {
        match self {
            MoveVecWithScratch::NoScratch(moves) => Some(moves),
            MoveVecWithScratch::Scratch(..) => None,
        }
    }

    pub fn needs_scratch(&self) -> bool {
        match self {
            MoveVecWithScratch::NoScratch(..) => false,
            MoveVecWithScratch::Scratch(..) => true,
        }
    }
}

pub struct MoveAndScratchResolver<GetReg, GetStackSlot, IsStackAlloc>
where
    GetReg: FnMut() -> Option<Allocation>,
    GetStackSlot: FnMut() -> Allocation,
    IsStackAlloc: Fn(Allocation) -> bool,
{
    pub find_free_reg: GetReg,
    pub get_stackslot: GetStackSlot,
    pub is_stack_alloc: IsStackAlloc,
    pub borrowed_scratch_reg: PReg,
}

impl<GetReg, GetStackSlot, IsStackAlloc>
    MoveAndScratchResolver<GetReg, GetStackSlot, IsStackAlloc>
where
    GetReg: FnMut() -> Option<Allocation>,
    GetStackSlot: FnMut() -> Allocation,
    IsStackAlloc: Fn(Allocation) -> bool,
{
    pub fn compute<T: Debug + Default + Copy>(
        mut self,
        moves: MoveVecWithScratch<T>,
    ) -> MoveVec<T> {
        let moves = if moves.needs_scratch() {
            let scratch = (self.find_free_reg)().unwrap_or_else(|| (self.get_stackslot)());
            log::trace!("scratch resolver: scratch alloc {:?}", scratch);
            moves.with_scratch(scratch)
        } else {
            moves.without_scratch().unwrap()
        };

        let stack_to_stack = moves
            .iter()
            .any(|&(src, dst, _)| self.is_stack_to_stack_move(src, dst));
        if !stack_to_stack {
            return moves;
        }

        let (scratch_reg, save_slot) = if let Some(reg) = (self.find_free_reg)() {
            log::trace!(
                "scratch resolver: have free stack-to-stack scratch preg: {:?}",
                reg
            );
            (reg, None)
        } else {
            let reg = Allocation::reg(self.borrowed_scratch_reg);
            let save = (self.get_stackslot)();
            log::trace!(
                "scratch resolver: stack-to-stack borrowing {:?} with save stackslot {:?}",
                reg,
                save
            );
            (reg, Some(save))
        };

        let mut scratch_dirty = false;
        let mut save_dirty = true;

        let mut result = smallvec![];
        for &(src, dst, data) in &moves {
            if self.is_stack_to_stack_move(src, dst) {
                log::trace!("scratch resolver: stack to stack: {:?} -> {:?}", src, dst);

                if let Some(save_slot) = save_slot {
                    if save_dirty {
                        debug_assert!(!scratch_dirty);
                        result.push((scratch_reg, save_slot, T::default()));
                        save_dirty = false;
                    }
                }

                result.push((src, scratch_reg, data));
                result.push((scratch_reg, dst, data));
                scratch_dirty = true;
            } else {
                if src == scratch_reg && scratch_dirty {
                    debug_assert!(!save_dirty);
                    let save_slot = save_slot.expect("move source should not be a free register");
                    result.push((save_slot, scratch_reg, T::default()));
                    scratch_dirty = false;
                }
                if dst == scratch_reg {
                    scratch_dirty = false;
                    save_dirty = true;
                }
                result.push((src, dst, data));
            }
        }

        if let Some(save_slot) = save_slot {
            if scratch_dirty {
                debug_assert!(!save_dirty);
                result.push((save_slot, scratch_reg, T::default()));
            }
        }

        log::trace!("scratch resolver: got {:?}", result);
        result
    }

    fn is_stack_to_stack_move(&self, src: Allocation, dst: Allocation) -> bool {
        (self.is_stack_alloc)(src) && (self.is_stack_alloc)(dst)
    }
}

#[inline(always)]
fn u64_key(b: u32, a: u32) -> u64 {
    a as u64 | (b as u64) << 32
}
