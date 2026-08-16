//! Structured statistics produced by MIR passes and code generation.
//!
//! ---
//!
//! ## 中文说明：`CodegenStats`——MIR pass 与代码生成的统计输出
//!
//! 定位链：SysY 源码 → RaanaIR（平台无关 SSA，`raana_ir`）→ MIR pass、寄存器
//! 分配与汇编发射（`taki_mir` + `anon_armv8` / `uika_riscv` 后端）。本文件只
//! 定义**统计输出的数据结构**：后端编译每个函数时，把各 pass 的命中次数、ABI
//! 参数绑定方式、寄存器分配代价、调度器行为写进 `FunctionCodegenStats`，最终
//! 由 `CodegenStats::aggregate` 汇总成整个编译单元的统计，随 `CompileOutput`
//! 的 `stats` 字段返回给调用方。统计本身不影响生成的代码，只用于观测与验证。
//!
//! ### 统计了什么
//!
//! `CodegenStats` 顶层只有两个字段：`functions: Vec<FunctionCodegenStats>`
//! （逐函数记录）+ `total: FunctionCodegenStats`（`aggregate` 把各函数计数
//! 逐项累加出的总和，`total.function` 为空字符串）。每个函数一份的
//! `FunctionCodegenStats` 按阶段分组：
//!
//! - **pass 是否运行/是否改动**：`DceStats` / `PeepholeStats` /
//!   `PairCombineStats` / `SchedulerStats` / `BranchOptStats` 都带 `ran` 与
//!   `changed` 两个布尔位；`ChainFusionStats` 只有 `changed`。
//! - **pass 命中计数**：`DceStats::instructions_removed`；`PeepholeStats` 的
//!   `mac_pairs_formed` / `flag_fusions_formed`；`ChainFusionStats::fusions`；
//!   `PairCombineStats` 的 `load_pairs_formed` / `store_pairs_formed` /
//!   `tombstone_removed_created`；`BranchOptStats` 五类：`fallthrough_removed`
//!   （R1）、`labels_threaded`（R2）、`dead_jumps_removed`（R3）、
//!   `branches_inverted`（R4）、`veneers_inserted`（M27 长跳转 veneer）。
//! - **ABI 参数绑定**：`AbiArgStats` 的 `register_args_bound`（经 entry `Args`
//!   伪指令绑定）、`unused_register_args_skipped`（从未用到而跳过）、
//!   `incoming_stack_args_loaded`（incoming-arg load 物化栈参数）。
//! - **寄存器分配**：`RegallocStats` 的 `spill_slots`（单位 `spill_size`）与
//!   `reg_to_reg_edits` / `reg_to_stack_edits` / `stack_to_reg_edits`（分配器
//!   编辑次数，来自 `lib.rs` 对分配输出 `edits` 的分类计数）。
//! - **调度器**：`SchedulerStats` 的块级覆盖（`blocks_total` /
//!   `blocks_checked` / `blocks_skipped_short`）、`identity_schedules` /
//!   `estimator_rejections`、`fallbacks`（回退原因
//!   `SchedulerFallbackReason::StallBudgetExhausted`，带块号）；周期估算
//!   `original` / `scheduled` 各一份 `CycleEstimateStats`
//!   （`completion_cycles` / `stall_cycles` / `single_issue_cycles` /
//!   `dual_issue_cycles` / `idle_cycles`，外加 `ResourceUseStats` 的 `lsu` /
//!   `alu` / `mac_div` / `fp_other` / `branch`）；DAG 形态 `DagStats`
//!   （`nodes` / `edges` / `edges_by_kind: BTreeMap<String, u64>` /
//!   `max_block_nodes`，以及 `MemoryDagStats` 的 `known_root_accesses` /
//!   `unknown_root_accesses` / `disjoint_comparisons` / `may_alias_comparisons`
//!   / `max_history_len`）。
//! - **观测计时**：`SchedulerTimings` 的 `dag_build_ns` / `schedule_ns` /
//!   `estimate_ns`。注意它是**非确定性**的（每次运行可能不同），不能用于
//!   逐字节对比的报告，见其字段注释。
//!
//! ### 谁在写、谁在读
//!
//! - **写入方**：`taki_mir/src/passes.rs` 把 `&mut FunctionCodegenStats` 传给
//!   各 MIR pass；`anon_armv8/src/passes/` 下的 dce、peephole_combine、
//!   chain_fusion、pair_combine、list_scheduler 逐个填数；
//!   `taki_mir/src/lib.rs` 的 `compile_with_config` 填 `AbiArgStats`
//!   （`vcode.abi.arg_stats()`）与 `RegallocStats`；`taki_mir/src/emit.rs` 的
//!   `AsmWriter` 在发射时填 `BranchOptStats`（EmitBuffer 分支优化规则）。
//! - **读取方**：`CodegenStats` 经 `CompileOutput` 的 `stats` 字段随汇编文本
//!   一起返回，`soyo_compiler/src/abi_matrix.rs` 的测试从中取
//!   `FunctionCodegenStats` 验证 ABI 绑定行为。注意 CLI 的 `--pass-stats`
//!   开关只打印 **raana_ir 层**的统计（`PassesRunStats`），不包含本文件的
//!   MIR 层数据；MIR 层统计目前主要供测试与程序化分析使用。
//!
//! ### 与 `raana_ir/src/opt/stats.rs` 的区别
//!
//! - **层**：`raana_ir` 统计平台无关 SSA IR 上的优化 pass（如
//!   `LoopUnrollStats` 的观察/接受/拒绝计数、`reject_reasons` 直方图与
//!   `events` 事件流）；本文件统计 MIR/代码生成阶段（lower 后的 VCode 上跑的
//!   pass、寄存器分配、发射）。
//! - **组织**：raana_ir 是"每 pass 一份结构 + 直方图 + 事件日志"；本文件是
//!   "每函数一份、按阶段嵌套，`accumulate` 逐项合并出 `total`"。
//! - **输出**：raana_ir 由 `--pass-stats` 打到 stderr 做语料调查；MIR 层随
//!   `CompileOutput` 的 `stats` 字段返回。
//!
//! ### 验证
//!
//! - `soyo_compiler/src/abi_matrix.rs` 的测试读编译输出的
//!   `FunctionCodegenStats` 断言寄存器/栈参数绑定计数；
//!   `anon_armv8/src/sched/dag/tests.rs` 等断言 `DagStats` 的 `nodes` /
//!   `edges` 等字段；
//! - 汇总正确性由 `accumulate` 实现保证：布尔位 `|=` 合并、计数 `+=`、最大值
//!   字段（`max_block_nodes` / `max_history_len`）取 `max`；`SchedulerTimings`
//!   除外，不做确定性保证。
//!

use std::collections::BTreeMap;

/// Complete statistics for one compilation unit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodegenStats {
    pub functions: Vec<FunctionCodegenStats>,
    pub total: FunctionCodegenStats,
}

impl CodegenStats {
    pub fn aggregate(functions: Vec<FunctionCodegenStats>) -> Self {
        let mut total = FunctionCodegenStats::default();
        for function in &functions {
            total.accumulate(function);
        }
        Self { functions, total }
    }
}

/// Statistics collected while compiling one function.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FunctionCodegenStats {
    pub function: String,
    pub dce: DceStats,
    pub peephole: PeepholeStats,
    pub chain_fusion: ChainFusionStats,
    pub pair: PairCombineStats,
    pub scheduler: SchedulerStats,
    pub abi: AbiArgStats,
    pub regalloc: RegallocStats,
    pub branch_opt: BranchOptStats,
}

impl FunctionCodegenStats {
    fn accumulate(&mut self, other: &Self) {
        self.dce.accumulate(&other.dce);
        self.peephole.accumulate(&other.peephole);
        self.chain_fusion.accumulate(&other.chain_fusion);
        self.pair.accumulate(&other.pair);
        self.scheduler.accumulate(&other.scheduler);
        self.abi.accumulate(&other.abi);
        self.regalloc.accumulate(&other.regalloc);
        self.branch_opt.accumulate(&other.branch_opt);
    }
}

/// Emission-time branch optimization statistics (EmitBuffer rules).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BranchOptStats {
    pub ran: bool,
    pub changed: bool,
    /// Branches removed because their target was the fallthrough (R1).
    pub fallthrough_removed: u64,
    /// Conditional branches inverted to eliminate a following jump (R4).
    pub branches_inverted: u64,
    /// Labels redirected through a tail unconditional branch (R2).
    pub labels_threaded: u64,
    /// Unreachable unconditional branches removed after another uncond (R3).
    pub dead_jumps_removed: u64,
    /// Long-branch veneers inserted by range relaxation (M27).
    pub veneers_inserted: u64,
}

impl BranchOptStats {
    fn accumulate(&mut self, other: &Self) {
        self.ran |= other.ran;
        self.changed |= other.changed;
        self.fallthrough_removed += other.fallthrough_removed;
        self.branches_inverted += other.branches_inverted;
        self.labels_threaded += other.labels_threaded;
        self.dead_jumps_removed += other.dead_jumps_removed;
        self.veneers_inserted += other.veneers_inserted;
    }
}

/// Incoming-argument binding statistics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AbiArgStats {
    /// Register parameters bound through the entry `Args` pseudo.
    pub register_args_bound: u64,
    /// Register parameters that were never needed and skipped entirely.
    pub unused_register_args_skipped: u64,
    /// Stack parameters materialized with an incoming-argument load.
    pub incoming_stack_args_loaded: u64,
}

impl AbiArgStats {
    fn accumulate(&mut self, other: &Self) {
        self.register_args_bound += other.register_args_bound;
        self.unused_register_args_skipped += other.unused_register_args_skipped;
        self.incoming_stack_args_loaded += other.incoming_stack_args_loaded;
    }
}

/// Register-allocation outcome statistics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegallocStats {
    /// Number of allocator spill slots (`spill_size` units) used.
    pub spill_slots: u64,
    /// Allocator edits moving a value between two registers.
    pub reg_to_reg_edits: u64,
    /// Allocator edits spilling a value to the stack.
    pub reg_to_stack_edits: u64,
    /// Allocator edits reloading a value from the stack.
    pub stack_to_reg_edits: u64,
}

impl RegallocStats {
    fn accumulate(&mut self, other: &Self) {
        self.spill_slots += other.spill_slots;
        self.reg_to_reg_edits += other.reg_to_reg_edits;
        self.reg_to_stack_edits += other.reg_to_stack_edits;
        self.stack_to_reg_edits += other.stack_to_reg_edits;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DceStats {
    pub ran: bool,
    pub changed: bool,
    pub instructions_removed: u64,
}

impl DceStats {
    fn accumulate(&mut self, other: &Self) {
        self.ran |= other.ran;
        self.changed |= other.changed;
        self.instructions_removed += other.instructions_removed;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeepholeStats {
    pub ran: bool,
    pub changed: bool,
    pub mac_pairs_formed: u64,
    pub flag_fusions_formed: u64,
}

/// Chain fusion (AArch64 `chain_fusion`): compares removed from the second
/// block of a (check, split) decision-tree node pair.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChainFusionStats {
    pub fusions: u64,
    pub changed: bool,
}

impl ChainFusionStats {
    fn accumulate(&mut self, other: &Self) {
        self.fusions += other.fusions;
        self.changed |= other.changed;
    }
}

impl PeepholeStats {
    fn accumulate(&mut self, other: &Self) {
        self.ran |= other.ran;
        self.changed |= other.changed;
        self.mac_pairs_formed += other.mac_pairs_formed;
        self.flag_fusions_formed += other.flag_fusions_formed;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PairCombineStats {
    pub ran: bool,
    pub changed: bool,
    pub load_pairs_formed: u64,
    pub store_pairs_formed: u64,
    pub tombstone_removed_created: u64,
}

impl PairCombineStats {
    fn accumulate(&mut self, other: &Self) {
        self.ran |= other.ran;
        self.changed |= other.changed;
        self.load_pairs_formed += other.load_pairs_formed;
        self.store_pairs_formed += other.store_pairs_formed;
        self.tombstone_removed_created += other.tombstone_removed_created;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchedulerStats {
    pub ran: bool,
    pub blocks_total: u64,
    pub blocks_checked: u64,
    pub blocks_skipped_short: u64,
    pub identity_schedules: u64,
    pub estimator_rejections: u64,
    pub fallbacks: Vec<SchedulerFallback>,
    pub original: CycleEstimateStats,
    pub scheduled: CycleEstimateStats,
    pub dag: DagStats,
    pub timings: SchedulerTimings,
}

impl SchedulerStats {
    fn accumulate(&mut self, other: &Self) {
        self.ran |= other.ran;
        self.blocks_total += other.blocks_total;
        self.blocks_checked += other.blocks_checked;
        self.blocks_skipped_short += other.blocks_skipped_short;
        self.identity_schedules += other.identity_schedules;
        self.estimator_rejections += other.estimator_rejections;
        self.fallbacks.extend(other.fallbacks.iter().cloned());
        self.original.accumulate(&other.original);
        self.scheduled.accumulate(&other.scheduled);
        self.dag.accumulate(&other.dag);
        self.timings.accumulate(&other.timings);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchedulerFallback {
    pub block: usize,
    pub reason: SchedulerFallbackReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedulerFallbackReason {
    StallBudgetExhausted,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CycleEstimateStats {
    pub samples: u64,
    pub completion_cycles: u64,
    pub stall_cycles: u64,
    pub single_issue_cycles: u64,
    pub dual_issue_cycles: u64,
    pub idle_cycles: u64,
    pub resources: ResourceUseStats,
}

impl CycleEstimateStats {
    fn accumulate(&mut self, other: &Self) {
        self.samples += other.samples;
        self.completion_cycles += other.completion_cycles;
        self.stall_cycles += other.stall_cycles;
        self.single_issue_cycles += other.single_issue_cycles;
        self.dual_issue_cycles += other.dual_issue_cycles;
        self.idle_cycles += other.idle_cycles;
        self.resources.accumulate(&other.resources);
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResourceUseStats {
    pub lsu: u64,
    pub alu: u64,
    pub mac_div: u64,
    pub fp_other: u64,
    pub branch: u64,
}

impl ResourceUseStats {
    fn accumulate(&mut self, other: &Self) {
        self.lsu += other.lsu;
        self.alu += other.alu;
        self.mac_div += other.mac_div;
        self.fp_other += other.fp_other;
        self.branch += other.branch;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DagStats {
    pub nodes: u64,
    pub edges: u64,
    pub edges_by_kind: BTreeMap<String, u64>,
    pub memory: MemoryDagStats,
    pub max_block_nodes: u64,
}

impl DagStats {
    fn accumulate(&mut self, other: &Self) {
        self.nodes += other.nodes;
        self.edges += other.edges;
        for (kind, count) in &other.edges_by_kind {
            *self.edges_by_kind.entry(kind.clone()).or_default() += count;
        }
        self.memory.accumulate(&other.memory);
        self.max_block_nodes = self.max_block_nodes.max(other.max_block_nodes);
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryDagStats {
    pub accesses: u64,
    pub known_root_accesses: u64,
    pub unknown_root_accesses: u64,
    pub comparisons: u64,
    pub disjoint_comparisons: u64,
    pub may_alias_comparisons: u64,
    pub max_history_len: u64,
}

impl MemoryDagStats {
    fn accumulate(&mut self, other: &Self) {
        self.accesses += other.accesses;
        self.known_root_accesses += other.known_root_accesses;
        self.unknown_root_accesses += other.unknown_root_accesses;
        self.comparisons += other.comparisons;
        self.disjoint_comparisons += other.disjoint_comparisons;
        self.may_alias_comparisons += other.may_alias_comparisons;
        self.max_history_len = self.max_history_len.max(other.max_history_len);
    }
}

/// Observational timings; unlike deterministic counters these may differ
/// between runs and should not be used for byte-for-byte report comparisons.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchedulerTimings {
    pub dag_build_ns: u64,
    pub schedule_ns: u64,
    pub estimate_ns: u64,
}

impl SchedulerTimings {
    fn accumulate(&mut self, other: &Self) {
        self.dag_build_ns += other.dag_build_ns;
        self.schedule_ns += other.schedule_ns;
        self.estimate_ns += other.estimate_ns;
    }
}
