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
//! 按候选优先级遍历可用物理寄存器：Ion 分配器主循环（process.rs 的逐 bundle
//! 分配）为某个 bundle 物色寄存器时，就按这里给出的顺序逐个尝试。顺序即优先级，
//! 直接影响分配质量——能否命中提示寄存器、寄存器压力是否均匀。

use crate::reg_alloc::reg::{MachineEnv, PReg, PRegSet, RegClass};

/// Candidate allocatable-register order: fixed, hint, preferred, then non-preferred.
/// 候选物理寄存器的顺序（优先级从高到低）：
/// 1. fixed —— 被硬约束（如 ABI 调用约定）锁死的寄存器；
/// 2. hint —— 分配提示给出的寄存器（值已在那里，命中可省一条 move）；
/// 3. preferred —— 该类首选寄存器（如调用方保存、分配/使用成本低的）；
/// 4. non-preferred —— 其余可用寄存器（如调用者保存的）。
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
        // accept：只接受硬件编码 hw_enc < limit 的寄存器。limit 来自
        // Requirement::Limit（例如“只许用该类的前 N 个寄存器”），None 表示不限。
        let accept = |reg: PReg| reg.hw_enc() < limit.unwrap_or(usize::MAX);
        // 固定寄存器分支：候选集只含这一个寄存器。若它超出 limit 则候选集为空，
        // 调用方会把这次分配视为失败，转而走驱逐/分裂/溢出等备选路径。
        if let Some(reg) = fixed {
            return Self {
                registers: accept(reg).then_some(reg).into_iter().collect(),
                next: 0,
            };
        }
        // traverse：把集合里的寄存器按“从 offset 处旋转”后的升序排列。
        // mask 收集所有 hw_enc < offset % PReg::MAX 的寄存器；先取 mask 之外
        // （编码 ≥ 起点那段）再链回 mask 之内（编码 < 起点那段），实现绕一圈：
        // 第一个候选是编码 ≥ offset % PReg::MAX 的最小寄存器，扫到顶后回绕到 0。
        // offset 由 process.rs 以“指令位置 + bundle 序号”算出：不同位置、不同
        // bundle 从不同起点开扫，把分配压力均匀摊到各寄存器上，避免总抢低编号的。
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
        // hint 排最前：优先尝试提示寄存器（如合并 move 两端共同偏好的寄存器），
        // 命中即可省掉后续的 move 指令。注意：hint 若被 accept 拒绝（超出 limit），
        // 后面两个循环仍会按值相等跳过它，即该寄存器在整条候选链中都不出现。
        if let Some(reg) = hint.filter(|&reg| accept(reg)) {
            registers.push(reg);
        }
        // 其次遍历首选寄存器（旋转序）；hint 已插在最前，这里跳过以免重复。
        for reg in traverse(env.preferred_regs_by_class[class_index]) {
            if Some(reg) != hint {
                registers.push(reg);
            }
        }
        // 最后才是非首选寄存器：它们使用成本更高（如调用者保存的，进出调用都要
        // 保存/恢复），只在前两类都不合适时才轮到。
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
        // 顺序吐出预排好的候选；只有确实取到寄存器时 next 才 +1，
        // 耗尽后下标停在末尾（不会越界），此后反复调用都返回 None。
        let reg = self.registers.get(self.next).copied();
        self.next += usize::from(reg.is_some());
        reg
    }
}
