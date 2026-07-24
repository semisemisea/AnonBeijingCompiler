/* Adapted from regalloc2 0.15.1 src/ion/reg_traversal.rs. Apache-2.0 WITH LLVM-exception. */

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
        let rotate = |set: PRegSet| {
            let mut regs: Vec<_> = set.into_iter().filter(|&reg| accept(reg)).collect();
            if !regs.is_empty() {
                let rotate_by = offset % regs.len();
                regs.rotate_left(rotate_by);
            }
            regs
        };
        let class_index = class as usize;
        let mut registers = Vec::new();
        if let Some(reg) = hint.filter(|&reg| accept(reg)) {
            registers.push(reg);
        }
        for reg in rotate(env.preferred_regs_by_class[class_index]) {
            if Some(reg) != hint {
                registers.push(reg);
            }
        }
        for reg in rotate(env.non_preferred_regs_by_class[class_index]) {
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
