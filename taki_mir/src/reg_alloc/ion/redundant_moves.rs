/*
 * This file was initially derived from the files
 * `js/src/jit/BacktrackingAllocator.h` and
 * `js/src/jit/BacktrackingAllocator.cpp` in Mozilla Firefox, and was
 * originally licensed under the Mozilla Public License 2.0. We
 * subsequently relicensed it to Apache-2.0 WITH LLVM-exception (see
 * https://github.com/bytecodealliance/regalloc2/issues/7).
 *
 * Since the initial port, the design has been substantially evolved
 * and optimized.
 *
 * Local provenance: copied from regalloc2 0.15.1 `src/ion/redundant_moves.rs`.
 * Local modification: imports refer to taki_mir's allocator types.
 */

//! Redundant-move elimination.

use rustc_hash::FxHashMap;
use smallvec::{SmallVec, smallvec};

use crate::reg_alloc::reg::{Allocation, VReg};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RedundantMoveState {
    Copy(Allocation, Option<VReg>),
    Orig(VReg),
    None,
}
#[derive(Clone, Debug, Default)]
pub struct RedundantMoveEliminator {
    allocs: FxHashMap<Allocation, RedundantMoveState>,
    reverse_allocs: FxHashMap<Allocation, SmallVec<[Allocation; 4]>>,
}
#[derive(Copy, Clone, Debug)]
pub struct RedundantMoveAction {
    pub elide: bool,
}

impl RedundantMoveEliminator {
    pub fn process_move(
        &mut self,
        from: Allocation,
        to: Allocation,
        to_vreg: Option<VReg>,
    ) -> RedundantMoveAction {
        let from_state = self
            .allocs
            .get(&from)
            .copied()
            .unwrap_or(RedundantMoveState::None);
        let to_state = self
            .allocs
            .get(&to)
            .copied()
            .unwrap_or(RedundantMoveState::None);

        if from == to {
            if let Some(to_vreg) = to_vreg {
                self.clear_alloc(to);
                self.allocs.insert(to, RedundantMoveState::Orig(to_vreg));
                return RedundantMoveAction { elide: true };
            }
        }

        let src_vreg = match from_state {
            RedundantMoveState::Copy(_, opt_r) => opt_r,
            RedundantMoveState::Orig(r) => Some(r),
            RedundantMoveState::None => None,
        };
        let dst_vreg = to_vreg.or(src_vreg);
        let elide = match (from_state, to_state) {
            (_, RedundantMoveState::Copy(orig_alloc, _)) if orig_alloc == from => true,
            (RedundantMoveState::Copy(new_alloc, _), _) if new_alloc == to => true,
            _ => false,
        };

        if !elide {
            self.clear_alloc(to);
        }

        if from.is_reg() || to.is_reg() {
            self.allocs
                .insert(to, RedundantMoveState::Copy(from, dst_vreg));
            self.reverse_allocs
                .entry(from)
                .or_insert_with(|| smallvec![])
                .push(to);
        }

        RedundantMoveAction { elide }
    }

    pub fn clear(&mut self) {
        self.allocs.clear();
        self.reverse_allocs.clear();
    }

    pub fn clear_alloc(&mut self, alloc: Allocation) {
        if let Some(existing_copies) = self.reverse_allocs.get_mut(&alloc) {
            for to_inval in existing_copies.drain(..) {
                if let Some(val) = self.allocs.get_mut(&to_inval) {
                    match val {
                        RedundantMoveState::Copy(_, Some(vreg)) => {
                            *val = RedundantMoveState::Orig(*vreg);
                        }
                        _ => *val = RedundantMoveState::None,
                    }
                }
                self.allocs.remove(&to_inval);
            }
        }
        self.allocs.remove(&alloc);
    }
}
