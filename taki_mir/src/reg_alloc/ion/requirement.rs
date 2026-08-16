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
 * Local adaptation note: interfaces are mapped to `taki_mir::reg_alloc`.
 */

//! Requirements computation.
//!
//! `Requirement` 描述一个使用点对操作数的**固定约束**：必须落在某个具体物理
//! 寄存器（如 ABI 参数寄存器、返回寄存器、架构要求如乘法的固定输入），或必须
//! 是寄存器/栈槽。liveness 阶段为每个 use 计算 requirement，process 阶段据此
//! 限制候选寄存器集合；约束冲突（`RequirementConflict`）会导致区间分裂或溢出。
//!
//! 本文件共三块内容：`Requirement` 约束枚举与合并规则（`merge` 取交集）、
//! `RequirementConflictAt` 冲突报告（附建议切点，供分裂使用）、以及 `Env` 上
//! 从操作数 / 束计算约束的三个方法。

use super::data_structures::{Env, LiveBundleIndex};
use crate::reg_alloc::{
    function::Function,
    reg::{Operand, OperandConstraint, PReg, ProgPoint},
};
use log::trace;

/// 约束冲突信号：两个约束的交集为空（例如同时要求两个不同的固定寄存器）。
pub struct RequirementConflict;

#[derive(Clone, Copy, Debug)]
pub enum RequirementConflictAt {
    /// A transition from a stack-constrained to a reg-constrained
    /// segment. The suggested split point is late, to keep the
    /// intervening region with the stackslot (which is cheaper).
    // 从"必须栈槽"过渡到"必须寄存器"。建议切点取**晚**（紧贴冲突的寄存器
    // 使用点之前），让中间那段继续留在栈槽上——栈比寄存器便宜。
    StackToReg(ProgPoint),
    /// A transition from a reg-constraint to a stack-constrained
    /// segment. Mirror of above: the suggested split point is early
    /// (just after the last register use).
    // 镜像情形：从"必须寄存器"过渡到"必须栈槽"。建议切点取**早**（紧贴
    // 最后一次寄存器使用之后），尽早把值挪回栈上。
    RegToStack(ProgPoint),
    /// Any other transition. The suggested split point is late (just
    /// before the conflicting use), but the split will also trim the
    /// ends and create a split bundle, so the intervening region will
    /// not appear with either side. This is probably for the best
    /// when e.g. the two sides of the split are both constrained to
    /// different physical registers: the part in the middle should be
    /// constrained to neither.
    // 其余一切冲突（典型：两侧分别约束到两个不同的固定寄存器）。建议切点取
    // 晚，但分裂会**修剪两端**、生成 split bundle，中间区域不会归属任何一侧
    // ——这通常是最优的：中间那段既不该被左边约束、也不该被右边约束。
    Other(ProgPoint),
}

impl RequirementConflictAt {
    #[inline(always)]
    pub fn should_trim_edges_around_split(self) -> bool {
        // 栈↔寄存器类转变不需要修剪：中间区域保留给较便宜的那一侧（栈）；
        // 其它冲突需要修剪，让中间区域不属于任何一侧。
        match self {
            RequirementConflictAt::RegToStack(..) | RequirementConflictAt::StackToReg(..) => false,
            RequirementConflictAt::Other(..) => true,
        }
    }

    #[inline(always)]
    pub fn suggested_split_point(self) -> ProgPoint {
        // 取出建议切点，交给 `split_and_requeue_bundle` 执行分裂。
        match self {
            RequirementConflictAt::RegToStack(pt)
            | RequirementConflictAt::StackToReg(pt)
            | RequirementConflictAt::Other(pt) => pt,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requirement {
    /// 无约束：寄存器或栈槽均可。
    Any,
    /// 必须是某个寄存器（不限定具体哪个）。
    Register,
    /// 必须是这个具体的物理寄存器（如 ABI 参数寄存器、返回寄存器）。
    FixedReg(PReg),
    /// 必须是编号落在 `0..n` 范围内的寄存器
    /// （指令编码的寄存器字段宽度限制，对应 `OperandConstraint::Limit`）。
    Limit(usize),
    /// 必须是栈槽（任意槽位）。
    Stack,
    /// 必须是这个具体的栈槽（栈槽在 PReg 表中同样占一个编号）。
    FixedStack(PReg),
}
impl Requirement {
    #[inline(always)]
    pub fn from_constraint(constraint: OperandConstraint, is_stack: impl Fn(PReg) -> bool) -> Self {
        // 把操作数约束翻译成 Requirement。注意 `Reuse(_)`（定义点复用某个使用
        // 点的寄存器）在这里只当作 `Reg` 处理：同寄存器复用由 defs 层面的
        // "复用关系"交给分配器其它机制满足，requirement 层面只要求"必须在
        // 寄存器"即可。`is_stack` 回调用来分辨 FixedReg 到底是真寄存器还是
        // 栈槽（二者共用 PReg 编号空间）。
        match constraint {
            OperandConstraint::Any => Self::Any,
            OperandConstraint::Reg | OperandConstraint::Reuse(_) => Self::Register,
            OperandConstraint::Stack => Self::Stack,
            OperandConstraint::Limit(n) => Self::Limit(n),
            OperandConstraint::FixedReg(reg) if is_stack(reg) => Self::FixedStack(reg),
            OperandConstraint::FixedReg(reg) => Self::FixedReg(reg),
        }
    }
    #[inline(always)]
    pub fn merge(self, other: Requirement) -> Result<Requirement, RequirementConflict> {
        use Requirement::*;

        // 合并 = 取两个约束的**交集**（更严格者胜出）；交集为空则返回冲突。
        // `compute_requirement` 用它在束内逐个累加使用点约束，
        // `merge_bundle_requirements` 用它判断两个束能否合并。
        match (self, other) {
            // `Any` matches anything.
            // Any 与任何约束合并 = 另一个约束本身。
            (other, Any) | (Any, other) => Ok(other),
            // Same kinds match.
            // 同类约束合并 = 自身。
            (Register, Register) => Ok(self),
            (Stack, Stack) => Ok(self),
            // 两个寄存器范围上限取较小者——交集。
            (Limit(a), Limit(b)) => Ok(Limit(a.min(b))),
            // 同一个固定寄存器（栈槽）才可合并，不同则落入下方失败分支。
            (FixedReg(a), FixedReg(b)) if a == b => Ok(self),
            (FixedStack(a), FixedStack(b)) if a == b => Ok(self),
            // Limit a 'Register|FixedReg`.
            // "范围上限"比"任意寄存器"更严格，合并结果保留上限。
            (Limit(a), Register) | (Register, Limit(a)) => Ok(Limit(a)),
            // 若固定寄存器的硬件编码落在这个上限范围内，则固定寄存器更严格
            // （交集就是它）；否则交集为空，落入失败分支。
            (Limit(a), FixedReg(b)) | (FixedReg(b), Limit(a)) if a > b.hw_enc() => Ok(FixedReg(b)),
            // Constrain `Register|Stack` to `Fixed{Reg|Stack}`.
            // 任意寄存器 / 任意栈槽收窄为具体的固定寄存器 / 固定栈槽。
            (Register, FixedReg(preg)) | (FixedReg(preg), Register) => Ok(FixedReg(preg)),
            (Stack, FixedStack(preg)) | (FixedStack(preg), Stack) => Ok(FixedStack(preg)),
            // Fail otherwise.
            // 其余组合（两个不同的固定寄存器、栈↔寄存器等）交集为空 → 冲突。
            _ => Err(RequirementConflict),
        }
    }

    #[inline(always)]
    pub fn is_stack(self) -> bool {
        // 是否为"栈类"约束（`Stack` / `FixedStack`）。
        match self {
            Requirement::Stack | Requirement::FixedStack(..) => true,
            Requirement::Register | Requirement::FixedReg(..) | Requirement::Limit(..) => false,
            Requirement::Any => false,
        }
    }

    #[inline(always)]
    pub fn is_reg(self) -> bool {
        // 是否为"寄存器类"约束（`Register` / `FixedReg` / `Limit`）。
        // 这两个分类判断在 `compute_requirement` 里用来决定冲突方向。
        match self {
            Requirement::Register | Requirement::FixedReg(..) | Requirement::Limit(..) => true,
            Requirement::Stack | Requirement::FixedStack(..) => false,
            Requirement::Any => false,
        }
    }
}

impl<F: Function> Env<'_, F> {
    #[inline(always)]
    pub fn requirement_from_operand(&self, op: Operand) -> Requirement {
        // 与 `Requirement::from_constraint` 等价，但用 Env 的 PReg 表判断
        // 固定寄存器是否为栈槽（栈槽也占用 PReg 编号，靠 `is_stack` 标记区分）。
        match op.constraint() {
            OperandConstraint::FixedReg(preg) => {
                if self.pregs[preg.index()].is_stack {
                    Requirement::FixedStack(preg)
                } else {
                    Requirement::FixedReg(preg)
                }
            }
            OperandConstraint::Reg | OperandConstraint::Reuse(_) => Requirement::Register,
            OperandConstraint::Limit(max) => Requirement::Limit(max),
            OperandConstraint::Stack => Requirement::Stack,
            OperandConstraint::Any => Requirement::Any,
        }
    }

    pub fn compute_requirement(
        &self,
        bundle: LiveBundleIndex,
    ) -> Result<Requirement, RequirementConflictAt> {
        // 计算一个束（bundle）的**统一约束**：遍历束内所有活跃区间（live
        // range）的所有使用点，把每个使用点的约束逐个 `merge` 进来（求交集）。
        // 某次 merge 失败说明束内存在互斥约束（如既要求寄存器又要求栈、或
        // 要求两个不同的固定寄存器），此时返回 `RequirementConflictAt`——它
        // 携带一个**建议切点**，`process.rs` 的 `process_bundle` 会立刻据此
        // `split_and_requeue_bundle` 把束切开重排，而不是放弃分配。
        //
        // 冲突方向的判定：已累加的约束是栈、新约束是寄存器 → 建议晚切（在
        // 冲突的寄存器使用点处切）；反过来 → 建议早切（在**上一个**使用点
        // `last_pos` 处切，让中间那段继续留在寄存器里）。
        let mut req = Requirement::Any;
        // 从"无约束"开始累加；`last_pos` 记录上一个使用点，供"早切"建议用。
        let mut last_pos = ProgPoint::before(0);
        trace!("compute_requirement: {:?}", bundle);
        let ranges = &self.bundles[bundle].ranges;
        for entry in ranges {
            trace!(" -> LR {:?}: {:?}", entry.index, entry.range);
            for u in &self.ranges[entry.index].uses {
                trace!("  -> use {:?}", u);
                let r = self.requirement_from_operand(u.operand);
                req = req.merge(r).map_err(|_| {
                    trace!("     -> conflict");
                    if req.is_stack() && r.is_reg() {
                        // Suggested split point just before the reg (i.e., late split).
                        // 栈→寄存器：切点取冲突使用点之前（晚切）。
                        RequirementConflictAt::StackToReg(u.pos)
                    } else if req.is_reg() && r.is_stack() {
                        // Suggested split point just after the stack
                        // (i.e., early split). Note that splitting
                        // with a use *right* at the beginning is
                        // interpreted by `split_and_requeue_bundle`
                        // as splitting off the first use.
                        // 寄存器→栈：切点取上一个使用点之后（早切）。注意若
                        // 切点恰好落在某个使用点上，`split_and_requeue_bundle`
                        // 会把该使用点当作"第一个使用点"切到新束里。
                        RequirementConflictAt::RegToStack(last_pos)
                    } else {
                        // 其它冲突：切点取冲突使用点之前。
                        RequirementConflictAt::Other(u.pos)
                    }
                })?;
                last_pos = u.pos;
                trace!("     -> req {:?}", req);
            }
        }
        trace!(" -> final: {:?}", req);
        Ok(req)
    }

    pub fn merge_bundle_requirements(
        &self,
        a: LiveBundleIndex,
        b: LiveBundleIndex,
    ) -> Result<Requirement, RequirementConflict> {
        // 合并两个束的约束：先分别算出各自的 requirement，再 `merge` 求交集。
        // `merge.rs` 在尝试把两个束并成一个之前调用它——两边约束冲突（如
        // 一个必须固定寄存器、另一个必须固定栈槽）就放弃这次合并。
        let req_a = self
            .compute_requirement(a)
            .map_err(|_| RequirementConflict)?;
        let req_b = self
            .compute_requirement(b)
            .map_err(|_| RequirementConflict)?;
        req_a.merge(req_b)
    }
}
