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
 * Local provenance: copied from regalloc2 0.15.1 `src/ion/reg_traversal.rs`.
 * Local modification: materializes the traversal because taki_mir's PRegSet
 * iterator differs from regalloc2's PRegSetIter export.
 */

//! Iterate over available registers.

use crate::reg_alloc::reg::{MachineEnv, PReg, PRegSet, RegClass};

/// Candidate allocatable-register order: fixed, hint, preferred, then non-preferred.
pub struct RegTraversalIter {
    registers: Vec<PReg>,
    next: usize,
}
impl RegTraversalIter {
    pub fn new(
        env: &MachineEnv,
        class: RegClass,
        fixed: Option<PReg>,
        hint: Option<PReg>,
        offset: usize,
        limit: Option<usize>,
    ) -> Self {
        let accept = |reg: PReg| reg.hw_enc() < limit.unwrap_or(usize::MAX);
        if let Some(reg) = fixed {
            return Self {
                registers: accept(reg).then_some(reg).into_iter().collect(),
                next: 0,
            };
        }
        let traverse = |set: PRegSet| {
            let mut mask = PRegSet::empty();
            mask.add_up_to(PReg::new(offset % PReg::MAX, class));
            let mut regs: Vec<_> = (set & mask.invert())
                .into_iter()
                .chain(set & mask)
                .filter(|&reg| accept(reg))
                .collect();
            regs.shrink_to_fit();
            regs
        };
        let class_index = class as usize;
        let mut registers = Vec::new();
        if let Some(reg) = hint.filter(|&reg| accept(reg)) {
            registers.push(reg);
        }
        for reg in traverse(env.preferred_regs_by_class[class_index]) {
            if Some(reg) != hint {
                registers.push(reg);
            }
        }
        for reg in traverse(env.non_preferred_regs_by_class[class_index]) {
            if Some(reg) != hint {
                registers.push(reg);
            }
        }
        Self { registers, next: 0 }
    }
}
impl Iterator for RegTraversalIter {
    type Item = PReg;
    fn next(&mut self) -> Option<PReg> {
        let reg = self.registers.get(self.next).copied();
        self.next += usize::from(reg.is_some());
        reg
    }
}
