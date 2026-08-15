/*
 * Portions of this module are adapted from regalloc2 0.15.1,
 * https://github.com/bytecodealliance/regalloc2/tree/v0.15.1.
 *
 * Released under the Apache License 2.0 with LLVM Exception. See the
 * repository LICENSE for the full license text. Local modifications adapt
 * regalloc2's interfaces and allocation utilities to taki_mir.
 */

//! Backtracking register allocator adapted from regalloc2's Ion allocator.
//!
//! The public entry point is deliberately separate from the production
//! allocator while this port is validated.  In particular, it normalizes the
//! client's VRegs into allocator-local dense VRegs before constructing Ion's
//! VReg-indexed state.
//!
//! ## 为什么叫"回溯"（backtracking）
//!
//! 分配不是一遍过的：主循环按 **bundle 优先级**（其活跃区间长度之和，
//! `compute_bundle_prio`）从大到小处理活跃区间（bundle），尝试为每个
//! bundle 找寄存器；找不到时不是立刻放弃，而是**驱逐**（evict）权重更低的
//! 已有 occupant，把被驱逐者重新入队。被驱逐者若仍无处可去，就**分裂**
//! （split）成更小的区间或**溢出**（spill）到栈。**溢出权重（SpillWeight）
//! 用于驱逐博弈与 spill 排序，不决定处理顺序**。这是 Firefox IonMonkey 的
//! BacktrackingAllocator 血统（经 regalloc2 移植）。
//!
//! ## 子模块职责
//!
//! | 模块 | 职责 |
//! |------|------|
//! | [`cfg`](cfg) | CFG 与支配信息（`CFGInfo`），供 liveness/merge 使用 |
//! | [`data_structures`] | 分配器核心数据结构：`LiveRange`/`LiveBundle`/`Use`/`VRegIndex`/`SpillSlotData`/`Ctx`/`Env` |
//! | [`domtree`] | 支配树（Cooper–Harvey–Kennedy 算法） |
//! | [`function`] | `DenseVRegFunction`：把客户端 VReg 归一化为稠密 VReg 的函数视图 |
//! | [`indexset`] | 稀疏无界索引集合（分配器内部集合） |
//! | [`liveranges`] | 活跃区间计算（`Liveness`）与溢出权重（`SpillWeight`，见下） |
//! | [`merge`] | 把同一 VReg 的多个 LiveRange 合并成 LiveBundle |
//! | [`moves`] | 移动解析：跨 block 边界的值搬运（blockparam in/out） |
//! | [`postorder`] | 迭代式后序遍历（CFG 分析用） |
//! | [`process`] | **主分配循环**：逐 bundle 分配/驱逐/分裂/溢出（本模块核心） |
//! | [`redundant_moves`] | 冗余 move 消除（`RedundantMoveEliminator`） |
//! | [`reg_traversal`] | 可用寄存器遍历迭代器（`RegTraversalIter`） |
//! | [`requirement`] | 固定寄存器约束（`Requirement`，如 ABI 参数必须进特定寄存器） |
//! | [`spill`] | 溢出槽分配（`SpillSlotData` 的布局） |
//!
//! ## 主流程（[`run`] 内部）
//!
//! 1. **归一化**：`DenseVRegFunction` 把客户端 VReg 重编号为 `0..n` 稠密区间；
//! 2. **liveness**（liveranges）：逐指令扫描，为每个 VReg 计算活跃区间；
//! 3. **merge**：同一 VReg 的所有区间合成一个 bundle（若区间被约束拆开则
//!    多个 bundle 共享 spill slot）；
//! 4. **process**（主循环，process.rs）：按权重处理 bundle——
//!    `try_to_allocate_bundle_to_reg` → 冲突则 `evict_bundle` /
//!    `split_and_requeue_bundle` / `get_or_create_spill_bundle`；
//! 5. **解析**（moves）：为 block 参数与跨边界的活跃值插入 move，
//!    并跑 `RedundantMoveEliminator` 清理；
//! 6. **溢出**（spill）：给每个溢出 bundle 分配栈槽。
//!
//! 结果以 [`Output`](crate::reg_alloc::reg::Output) 返回：每个 VReg 的物理
//! 寄存器或栈槽 + 需要插入的 move 列表，由 `taki_mir` 回写进 VCode。
//!
//! ## 最小示例（分配前后）
//!
//! 三条指令的玩具函数（寄存器不足导致 v3 被溢出）：
//!
//! ```text
//! 分配前（虚拟寄存器）            分配后（物理寄存器/栈）
//!   v0 = v1 + v2            →    x0 = x1 + x2
//!   v3 = v0 * v4            →    str x0, \[sp, #8\]    ← v3 溢出：定义点写栈
//!   v5 = v3 + 1             →    ldr x3, \[sp, #8\]    ← 使用点读栈
//!                                 add x5, x3, #1
//! ```
//!
//! 溢出发生在寄存器耗尽且权重博弈失败时：低权重的 bundle 让位给高权重者，
//! 自己的值在**定义点 store 到栈、使用点 load 回来**（上例 v3 在 def 处
//! 写 `\[sp,#8\]`、在 use 处读回）。
//!
//! ## 常见修改点（调策略动哪里）
//!
//! - **调溢出权重** → `liveranges.rs::spill_weight_from_constraint` 的 bonus 常量；
//! - **加寄存器类** → `reg.rs::RegClass` + `reg_traversal.rs` 的遍历顺序；
//! - **改 ABI 固定寄存器约束** → `requirement.rs`；
//! - **调栈槽复用** → `spill.rs`；
//! - 上游参考：本模块移植自 regalloc2 0.15.1（文件头 license 注明来源），
//!   结构性改动先对照上游同结构。

mod cfg;
mod data_structures;
mod domtree;
mod function;
mod indexset;
mod liveranges;
mod merge;
mod moves;
mod postorder;
mod process;
// Kept crate-visible while `spill.rs` remains on the upstream module layout.
pub(crate) use process::AllocRegResult;
mod redundant_moves;
mod reg_traversal;
mod requirement;
mod spill;

pub use cfg::{CFGInfo, CFGInfoCtx};
pub use data_structures::{
    BlockparamIn as BlockParamIn, BlockparamOut as BlockParamOut, CodeRange, Ctx, Env,
    FixedRegFixupLevel, InsertMovePrio, InsertedMove, InsertedMoves, LiveBundle, LiveBundleIndex,
    LiveBundleVec, LiveRange, LiveRangeFlag, LiveRangeIndex, LiveRangeKey, LiveRangeList,
    LiveRangeListEntry, PRegIndex, SpillSetIndex, SpillSlotData, SpillSlotIndex, Use, UseList,
    VRegIndex,
};
pub use function::DenseVRegFunction;
pub use indexset::IndexSet;
pub use liveranges::{Liveness, SpillWeight, spill_weight_from_constraint};
pub use redundant_moves::{RedundantMoveAction, RedundantMoveEliminator, RedundantMoveState};
pub use reg_traversal::RegTraversalIter;
pub use requirement::{Requirement, RequirementConflict, RequirementConflictAt};

use crate::{
    VecExt,
    reg_alloc::{
        function::Function,
        reg::{Edit, MachineEnv, Output, RegClass, VReg},
    },
};

impl<'a, F: Function> Env<'a, F> {
    /// Initialize an Ion allocation context for one dense function view.
    ///
    /// The local port has no annotations or allocator statistics in `Output`;
    /// those upstream-only facilities are intentionally reset and omitted here.
    fn new(func: &'a F, env: &'a MachineEnv, ctx: &'a mut Ctx) -> Self {
        let ninstrs = func.num_insts();
        let nblocks = func.num_blocks();

        ctx.liveins.preallocate(nblocks);
        ctx.liveouts.preallocate(nblocks);
        ctx.ranges.preallocate(4 * ninstrs);
        ctx.bundles.preallocate(ninstrs);
        ctx.spillsets.preallocate(ninstrs);
        ctx.vregs.preallocate(func.num_vregs());
        ctx.output.allocs.preallocate(4 * ninstrs);

        // A context is reusable between invocations. Clear every allocation
        // product; retain capacities to avoid churn on repeated compilations.
        ctx.liveins.clear();
        ctx.liveouts.clear();
        ctx.blockparam_ins.clear();
        ctx.blockparam_outs.clear();
        ctx.ranges.storage.clear();
        ctx.bundles.storage.clear();
        ctx.spillsets.storage.clear();
        ctx.vregs.storage.clear();
        ctx.allocation_queue.heap.clear();
        ctx.spilled_bundles.clear();
        ctx.scratch_spillset_pool
            .extend(ctx.spillslots.drain(..).map(|mut slot| {
                slot.ranges.btree.clear();
                slot.ranges
            }));
        ctx.slots_by_class = Default::default();
        ctx.extra_spillslots_by_class = Default::default();
        ctx.preferred_victim_by_class = [crate::reg_alloc::reg::PReg::invalid(); 3];
        ctx.multi_fixed_reg_fixups.clear();
        ctx.allocated_bundle_count = 0;
        ctx.debug_annotations.clear();
        ctx.conflict_set.clear();
        ctx.scratch_conflicts.clear();
        ctx.scratch_bundle.clear();
        ctx.scratch_vreg_ranges.clear();
        ctx.scratch_workqueue.clear();
        ctx.scratch_operand_rewrites.clear();
        ctx.scratch_removed_lrs.clear();
        ctx.scratch_removed_lrs_vregs.clear();
        ctx.scratch_workqueue_set.clear();
        ctx.output = Output::default();

        for preg in &mut ctx.pregs {
            preg.is_stack = false;
            preg.allocations.btree.clear();
        }

        Self { func, env, ctx }
    }

    fn init(&mut self) -> Result<(), String> {
        self.create_pregs_and_vregs();
        self.compute_liveness()?;
        self.build_liveranges()?;
        self.fixup_multi_fixed_vregs();
        self.merge_vreg_bundles();
        self.queue_bundles();
        Ok(())
    }

    fn run(&mut self) -> Result<data_structures::Edits, String> {
        self.process_bundles()?;
        self.try_allocating_regs_for_spilled_bundles();
        self.allocate_spillslots();
        let moves = self.apply_allocations_and_insert_moves();
        Ok(self.resolve_inserted_moves(moves))
    }
}

/// Allocate registers with the Ion backtracking allocator.
///
/// Ion indexes its analysis state directly by VReg number. `DenseVRegFunction`
/// therefore provides a private dense view even when the source function uses
/// sparse VRegs (as VCode does for pinned physical-register values).
pub fn run<F: Function>(func: &F, mach_env: &MachineEnv) -> Result<Output, String> {
    let dense_func = DenseVRegFunction::new(func);
    let mut ctx = Ctx::default();
    ctx.cfginfo.init(&dense_func, &mut ctx.cfginfo_ctx)?;

    let mut edits = {
        let mut env = Env::new(&dense_func, mach_env, &mut ctx);
        env.init()?;
        env.run()?
    };
    // Map allocator-local vreg indices in the edits back to the original
    // function's vregs so the emitter can size moves from real value types.
    let remapped = edits.drain_edits().map(|(point, edit)| {
        let edit = match edit {
            Edit::Move {
                from,
                to,
                class,
                vreg: Some(idx),
            } => {
                let local = VReg::new(idx as usize, RegClass::Int);
                let original = dense_func
                    .original_vreg(local)
                    .map(|v| v.vreg() as u32)
                    .unwrap_or(idx);
                Edit::Move {
                    from,
                    to,
                    class,
                    vreg: Some(original),
                }
            }
            other => other,
        };
        (point, edit)
    });
    ctx.output.edits.extend(remapped);
    Ok(core::mem::take(&mut ctx.output))
}

#[cfg(test)]
mod tests;


