//! # LoopUnroll：精确小循环全展开
//!
//! 把**迭代次数在编译期可精确求出**的规范化小循环整体展开成直线代码，消除
//! 循环测试与回边跳转的开销，并给后续 pass（GVN / DSE / 寄存器分配）暴露更大
//! 的无分支基本块。典型受益对象是 stencil / 小矩阵类固定迭代内核。对应 CLI
//! 开关 `--loop-unroll on|off|dry-run`。
//!
//! ## 变换形态（IR 示例）
//!
//! 一个 test-at-top 的规范化循环（测试在顶部、单 latch、计数向上）：
//!
//! ```text
//! pre:        jump header(0)
//! header(j):  br j < 3, body, exit     // 测试在顶部
//! body:       a[j] = j * 2; j' = j + 1; jump header(j')
//! exit:       ret
//! ```
//!
//! `constant_trip_count` 从严格 exit（`j < 3` 且 `j` 是基本归纳变量）推出
//! trip = 3。展开后：
//!
//! ```text
//! pre:          jump header(0)
//! header(j):    jump body              // 首测必过（trip>0），测试被删除
//! body:         a[0] = 0*2; j1 = 1; jump header_1(j1)
//! header_1(j):  jump body_1            // 克隆的 header：参数原样传递
//! body_1:       a[1] = 1*2; j2 = 2; jump header_2(j2)
//! header_2(j):  jump exit(2)           // 最后一次迭代：测试替换为直跳 exit
//! exit:         ret
//! ```
//!
//! 结构上：原 header 保留但终结符改为无条件进入第一次迭代；随后按 trip 数克隆
//! header + 循环体（`clone_region`），最后一个克隆 header 直跳 exit。原 header
//! 与克隆 header 共 trip+1 个、循环体共 trip 份——与大小预算公式一致。
//!
//! 循环外对 header 参数/指令值的引用会被重映射到最后一次迭代的对应值
//! （`remap_external_header_uses`）：循环退出时这些值恰好等于最后一次迭代
//! 的值，语义保持。
//!
//! ## 触发条件
//!
//! 候选循环必须同时满足：
//!
//! - **规范化形状**（`analyze_candidate` 逐项检查，任一不满足即拒绝）：
//!   header 不是函数入口；循环体 ≥ 2 块；恰好 1 个 latch（latch ≠ header）；
//!   无嵌套循环；header 恰好 2 条出边（1 条 continue 指向循环内、1 条 exit
//!   指向循环外）；latch 的 backedge 唯一且指向 header；header 恰好 2 条入边
//!   （1 条来自 preheader + 1 条 backedge）；各边参数个数与 header 参数个数
//!   一致。
//! - **可精确计数**：有基本归纳变量（`BasicInductionVariableAnalysis`）、有
//!   严格 exit（`normalize_strict_exit`，测试形如 `iv < bound`）、
//!   `constant_trip_count` 能求出**精确**迭代次数。
//! - **大小预算**：trip_count ≤ `MAX_FULL_UNROLL_TRIPS`（8），且投影大小
//!   （header 指令数 ×(trip+1) + 循环体指令数 ×trip，`projected_size`）
//!   ≤ `MAX_UNROLLED_NON_TERMINATORS`（1536）。预算放宽到 1536 的理由
//!   （见常量注释）：小的精确循环展开后常会暴露外层精确循环（例如 5×5 二维
//!   模板内核的两层），预算要容纳这种连续展开；而 trip 上限挡住了长循环，
//!   预算不会因此被滥用。
//!
//! ## 放弃（拒绝）条件
//!
//! 拒绝原因见
//! [`LoopUnrollRejectReason`](crate::opt::stats::LoopUnrollRejectReason)
//! （`--pass-stats` 可看到各原因的计数分布）：`HeaderIsEntry` /
//! `UnsupportedLoopShape` / `NestedLoop` / `UnsupportedHeaderEdges` /
//! `UnsupportedBackedge` / `NonCanonicalEntry` / `EdgeArgumentMismatch` /
//! `NoBasicInductionVariable` / `NoSupportedStrictExit` /
//! `NonConstantTripCount` / `TripCountTooLarge` / `ProjectedSizeTooLarge` /
//! `ContainsAlloc` / `BodyValueEscapesLoop`。两个语义闸门：
//!
//! - `ContainsAlloc`：循环体含 `Alloc` 时拒绝——展开会把 `Alloc` 克隆成多份
//!   栈分配，破坏"整个循环只分配一次"的语义（推断：代码未注释此原因，理由
//!   从拒绝点位置推得）；
//! - `BodyValueEscapesLoop`：循环体产生的值被循环外使用时拒绝——克隆会产生
//!   多份定义，外部引用无法唯一对应。
//!
//! ## 正确性
//!
//! - trip 数是**精确**迭代次数（不是上界），来自严格 exit 的常量推导；
//! - trip = 0：原 header 测试第一次就失败，直接把 header 终结符替换为直跳
//!   `exit`（携带原 exit 参数），等价于零次迭代；
//! - trip > 0：进入循环时测试必然通过（否则迭代数为 0），所以移除 header
//!   测试、无条件进入第一次迭代是安全的——即"head test 只 gate 第一次迭代"
//!   的论证（与 `rotate_loops` 的文档同款）；
//! - 每次迭代的 header 参数通过 `remap_values` 按 SSA 值流重映射，克隆指令
//!   引用本迭代的定义，不跨迭代串值。
//!
//! ## 管线位置与门控
//!
//! - 注册：`opt/pass.rs` 的 `PassesManager::from_config`，fixpoint 段，
//!   `config.loop_unroll != Disabled` 时挂载；运行在 `simplify_cfg` 之后、
//!   `rotate_loops` **之前**——必须在循环仍是 test-at-top 形态时展开；
//!   `rotate_loops` 会把测试移到 latch（countdown 形式），届时 IV / 严格
//!   exit 分析无法再识别。
//! - 模式：`LoopUnrollMode::Enabled`（默认，实际改写）/ `DryRun`（只跑候选
//!   分析并输出统计，不改写 IR，配合 `--pass-stats` 做语料调查）/
//!   `Disabled`。CLI：`--loop-unroll on|off|dry-run`，仅 `-O1`/`-O2` 接受，
//!   `-O0` 拒绝（`soyo_compiler/src/cli.rs`）。
//! - 统计：`stats.rs` 的 `LoopUnrollStats` 记录观察/接受/拒绝直方图；
//!   本 pass 的 `observed` 表按 (函数, header) 去重，避免 fixpoint 多轮
//!   迭代里同一循环被重复计数。
//!
//! ## 验证
//!
//! - 本文件 `mod tests`（917 行起）覆盖候选判定与展开重写；
//! - 全量语料：`docs/Loop_unroll_corpus_report.md`（U1 报告）——214 例在
//!   `on` 模式下 AArch64 / RISC-V QEMU 全过（214/214），展开后汇编增量
//!   < 0.8%；⚠ 该报告的 `MAX_UNROLLED_NON_TERMINATORS = 64` 是旧值，
//!   代码已放宽到 1536（为 5×5 stencil 内核），报告未同步。

use std::sync::{Arc, Mutex};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    ir::remap::EntityMapper,
    opt::{
        analysis_passes::{
            induction_variable::{
                BasicInductionVariableAnalysis, constant_trip_count, normalize_strict_exit,
            },
            loop_analysis::{Loop, LoopAnalysis},
        },
        config::LoopUnrollMode,
        prelude::*,
        stats::{LoopUnrollEvent, LoopUnrollOutcome, LoopUnrollRejectReason, PassesRunStats},
        utils::{
            cfg::CFG,
            logical_edge::{LogicalEdge, incoming_edges, outgoing_edges},
        },
    },
};

const MAX_FULL_UNROLL_TRIPS: usize = 8;
// Small exact loops often expose an outer exact loop after the first unroll.
// Keep enough room for two-dimensional stencil kernels (for example 5 x 5),
// while the trip-count cap prevents this budget from applying to long loops.
const MAX_UNROLLED_NON_TERMINATORS: usize = 1_536;

pub struct LoopUnroll {
    mode: LoopUnrollMode,
    stats: Arc<Mutex<PassesRunStats>>,
    collect_stats: bool,
    observed: FxHashMap<(Function, BasicBlock), ObservedLoop>,
}

impl LoopUnroll {
    pub fn new(
        mode: LoopUnrollMode,
        stats: Arc<Mutex<PassesRunStats>>,
        collect_stats: bool,
    ) -> Self {
        Self {
            mode,
            stats,
            collect_stats,
            observed: FxHashMap::default(),
        }
    }
}

#[derive(Debug, Clone)]
struct UnrollCandidate {
    header: BasicBlock,
    blocks: Vec<BasicBlock>,
    latch: BasicBlock,
    exit: BasicBlock,
    continue_edge: LogicalEdge,
    backedge: LogicalEdge,
    exit_edge: LogicalEdge,
    trip_count: usize,
    loop_values: FxHashSet<Inst>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloneError {
    MissingLoopValue(Inst),
}

#[derive(Debug, Clone)]
struct CandidateRejection {
    reason: LoopUnrollRejectReason,
    trip_count: Option<usize>,
    header_size: usize,
    body_size: usize,
    projected_size: Option<usize>,
    shape_candidate: bool,
    exact_trip_candidate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ObservedLoop {
    outcome: LoopUnrollOutcome,
    trip_count: Option<usize>,
    body_size: usize,
    projected_size: Option<usize>,
    shape_candidate: bool,
    exact_trip_candidate: bool,
}

impl CandidateRejection {
    fn new(reason: LoopUnrollRejectReason) -> Self {
        Self {
            reason,
            trip_count: None,
            header_size: 0,
            body_size: 0,
            projected_size: None,
            shape_candidate: false,
            exact_trip_candidate: false,
        }
    }
}

impl Pass for LoopUnroll {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().is_decl() {
            return false;
        }
        if self.collect_stats {
            self.stats.lock().unwrap().loop_unroll.pass_invocations += 1;
        }
        let mut changed = false;
        loop {
            let Some(cfg) = CFG::new(data) else {
                return changed;
            };
            if cfg.is_acyclic() {
                return changed;
            }
            let (cfg, _dom_tree, loops) = LoopAnalysis::from_cfg(cfg);
            let ivs = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
            let mut candidate = None;
            for looop in loops.loops() {
                match analyze_candidate(data, &cfg, &loops, &ivs, looop) {
                    Ok(found) => {
                        let outcome = match self.mode {
                            LoopUnrollMode::Enabled => LoopUnrollOutcome::Applied,
                            LoopUnrollMode::DryRun => LoopUnrollOutcome::WouldApply,
                            LoopUnrollMode::Disabled => unreachable!(),
                        };
                        self.record(
                            data,
                            looop,
                            outcome,
                            Some(found.trip_count),
                            non_terminators(data, found.header).len(),
                            body_size(data, &found),
                            Some(projected_size(data, &found).unwrap()),
                            true,
                            true,
                        );
                        if self.mode == LoopUnrollMode::Enabled {
                            candidate = Some(found);
                            break;
                        }
                    }
                    Err(rejection) => self.record(
                        data,
                        looop,
                        LoopUnrollOutcome::Rejected(rejection.reason),
                        rejection.trip_count,
                        rejection.header_size,
                        rejection.body_size,
                        rejection.projected_size,
                        rejection.shape_candidate,
                        rejection.exact_trip_candidate,
                    ),
                }
            }
            if self.mode == LoopUnrollMode::DryRun {
                return changed;
            }
            let Some(candidate) = candidate else {
                return changed;
            };
            apply_candidate(data, &candidate);
            changed = true;
        }
    }
}

impl LoopUnroll {
    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        data: &ArenaContextMut<'_>,
        looop: &Loop,
        outcome: LoopUnrollOutcome,
        trip_count: Option<usize>,
        header_size: usize,
        body_size: usize,
        projected_size: Option<usize>,
        shape_candidate: bool,
        exact_trip_candidate: bool,
    ) {
        if !self.collect_stats {
            return;
        }
        let key = (data.curr_func.unwrap(), looop.header());
        let current = ObservedLoop {
            outcome,
            trip_count,
            body_size,
            projected_size,
            shape_candidate,
            exact_trip_candidate,
        };
        let previous = self.observed.insert(key, current);
        let mut run_stats = self.stats.lock().unwrap();
        let stats = &mut run_stats.loop_unroll;
        stats.loop_observations += 1;
        if previous.is_none() {
            stats.unique_loops_seen += 1;
        } else if previous == Some(current) {
            return;
        }
        if let Some(previous) = previous {
            remove_observation(stats, previous);
        }
        add_observation(stats, current);
        match outcome {
            LoopUnrollOutcome::Applied
            | LoopUnrollOutcome::WouldApply
            | LoopUnrollOutcome::Rejected(_) => {}
        }
        stats.events.push(LoopUnrollEvent {
            function: data.name().to_owned(),
            header: data.bb_data(looop.header()).name().to_owned(),
            outcome,
            trip_count,
            header_size,
            body_size,
            projected_size,
        });
    }
}

fn add_observation(stats: &mut crate::opt::stats::LoopUnrollStats, observation: ObservedLoop) {
    if observation.shape_candidate {
        stats.shape_candidates += 1;
    }
    if observation.exact_trip_candidate {
        stats.exact_trip_candidates += 1;
    }
    if let Some(trip_count) = observation.trip_count {
        *stats.trip_count_histogram.entry(trip_count).or_default() += 1;
    }
    *stats
        .body_size_histogram
        .entry(observation.body_size)
        .or_default() += 1;
    if let Some(projected_size) = observation.projected_size {
        *stats
            .projected_size_histogram
            .entry(projected_size)
            .or_default() += 1;
    }
    match observation.outcome {
        LoopUnrollOutcome::Applied => {
            stats.applied += 1;
            if let Some(trip_count) = observation.trip_count {
                *stats
                    .accepted_trip_count_histogram
                    .entry(trip_count)
                    .or_default() += 1;
            }
        }
        LoopUnrollOutcome::WouldApply => {
            stats.would_apply += 1;
            if let Some(trip_count) = observation.trip_count {
                *stats
                    .accepted_trip_count_histogram
                    .entry(trip_count)
                    .or_default() += 1;
            }
        }
        LoopUnrollOutcome::Rejected(reason) => {
            *stats.reject_reasons.entry(reason).or_default() += 1;
        }
    }
}

fn remove_observation(stats: &mut crate::opt::stats::LoopUnrollStats, observation: ObservedLoop) {
    if observation.shape_candidate {
        stats.shape_candidates -= 1;
    }
    if observation.exact_trip_candidate {
        stats.exact_trip_candidates -= 1;
    }
    if let Some(trip_count) = observation.trip_count {
        decrement_histogram(&mut stats.trip_count_histogram, trip_count);
    }
    decrement_histogram(&mut stats.body_size_histogram, observation.body_size);
    if let Some(projected_size) = observation.projected_size {
        decrement_histogram(&mut stats.projected_size_histogram, projected_size);
    }
    match observation.outcome {
        LoopUnrollOutcome::Applied => {
            stats.applied -= 1;
            if let Some(trip_count) = observation.trip_count {
                decrement_histogram(&mut stats.accepted_trip_count_histogram, trip_count);
            }
        }
        LoopUnrollOutcome::WouldApply => {
            stats.would_apply -= 1;
            if let Some(trip_count) = observation.trip_count {
                decrement_histogram(&mut stats.accepted_trip_count_histogram, trip_count);
            }
        }
        LoopUnrollOutcome::Rejected(reason) => {
            let count = stats.reject_reasons.get_mut(&reason).unwrap();
            *count -= 1;
            if *count == 0 {
                stats.reject_reasons.remove(&reason);
            }
        }
    }
}

fn decrement_histogram(histogram: &mut std::collections::BTreeMap<usize, u64>, key: usize) {
    let count = histogram.get_mut(&key).unwrap();
    *count -= 1;
    if *count == 0 {
        histogram.remove(&key);
    }
}

fn analyze_candidate(
    data: &ArenaContextMut<'_>,
    cfg: &CFG,
    loops: &LoopAnalysis,
    ivs: &BasicInductionVariableAnalysis,
    looop: &Loop,
) -> Result<UnrollCandidate, CandidateRejection> {
    let header = looop.header();
    if Some(header) == data.layout().entry_bb().map(|block| block.bb()) {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::HeaderIsEntry,
        ));
    }
    if looop.body().len() < 2 || looop.latches().len() != 1 {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::UnsupportedLoopShape,
        ));
    }
    let latch = looop.latches()[0];
    if latch == header || !looop.contains(latch) {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::UnsupportedLoopShape,
        ));
    }
    if loops
        .loops()
        .iter()
        .any(|nested| nested.header() != header && looop.contains(nested.header()))
    {
        return Err(CandidateRejection::new(LoopUnrollRejectReason::NestedLoop));
    }

    let header_edges = outgoing_edges(data, header);
    if header_edges.len() != 2 {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::UnsupportedHeaderEdges,
        ));
    }
    let continue_edges = header_edges
        .iter()
        .copied()
        .filter(|edge| looop.contains(edge.target(data)))
        .collect::<Vec<_>>();
    let exit_edges = header_edges
        .iter()
        .copied()
        .filter(|edge| !looop.contains(edge.target(data)))
        .collect::<Vec<_>>();
    let [continue_edge] = continue_edges.as_slice() else {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::UnsupportedHeaderEdges,
        ));
    };
    let [exit_edge] = exit_edges.as_slice() else {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::UnsupportedHeaderEdges,
        ));
    };
    let exit = exit_edge.target(data);

    let latch_edges = outgoing_edges(data, latch);
    let [backedge] = latch_edges.as_slice() else {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::UnsupportedBackedge,
        ));
    };
    if backedge.target(data) != header {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::UnsupportedBackedge,
        ));
    }

    let incoming = incoming_edges(data, cfg, header);
    if incoming.len() != 2 {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::NonCanonicalEntry,
        ));
    }
    let entries = incoming
        .iter()
        .copied()
        .filter(|edge| !looop.contains(edge.source()))
        .collect::<Vec<_>>();
    let backedges = incoming
        .iter()
        .copied()
        .filter(|edge| looop.contains(edge.source()))
        .collect::<Vec<_>>();
    let [entry_edge] = entries.as_slice() else {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::NonCanonicalEntry,
        ));
    };
    let [incoming_backedge] = backedges.as_slice() else {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::NonCanonicalEntry,
        ));
    };
    if incoming_backedge != backedge || looop.get_preheader(cfg) != Some(entry_edge.source()) {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::NonCanonicalEntry,
        ));
    }
    if entry_edge.args(data).len() != data.bb_data(header).params().len()
        || backedge.args(data).len() != data.bb_data(header).params().len()
    {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::EdgeArgumentMismatch,
        ));
    }

    let variables = ivs.for_loop(looop);
    if variables.is_empty() {
        return Err(CandidateRejection {
            shape_candidate: true,
            ..CandidateRejection::new(LoopUnrollRejectReason::NoBasicInductionVariable)
        });
    }
    let exits = variables
        .iter()
        .filter_map(|iv| normalize_strict_exit(data, looop, iv).map(|exit| (iv, exit)))
        .collect::<Vec<_>>();
    if exits.is_empty() {
        return Err(CandidateRejection {
            shape_candidate: true,
            ..CandidateRejection::new(LoopUnrollRejectReason::NoSupportedStrictExit)
        });
    }
    let trip_count = exits
        .into_iter()
        .find_map(|(iv, exit)| constant_trip_count(data, iv, exit).map(|trip| trip.iterations()))
        .ok_or_else(|| CandidateRejection {
            shape_candidate: true,
            ..CandidateRejection::new(LoopUnrollRejectReason::NonConstantTripCount)
        })?;
    let blocks = data
        .layout()
        .basicblocks()
        .iter()
        .map(|block| block.bb())
        .filter(|&block| block != header && looop.contains(block))
        .collect::<Vec<_>>();
    if blocks.is_empty() || !blocks.contains(&continue_edge.target(data)) {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::UnsupportedLoopShape,
        ));
    }
    // Full unrolling can clone arbitrary internal control flow, but only when the
    // header owns the sole loop exit and the latch owns the sole backedge.
    for &block in &blocks {
        for edge in outgoing_edges(data, block) {
            if block == latch && edge == *backedge {
                continue;
            }
            if !looop.contains(edge.target(data)) {
                return Err(CandidateRejection::new(
                    LoopUnrollRejectReason::UnsupportedLoopShape,
                ));
            }
        }
    }
    let header_insts = non_terminators(data, header);
    let body_insts = blocks
        .iter()
        .flat_map(|&block| non_terminators(data, block))
        .collect::<Vec<_>>();
    let total = header_insts
        .len()
        .checked_mul(trip_count.saturating_add(1))
        .and_then(|header| {
            body_insts
                .len()
                .checked_mul(trip_count)
                .and_then(|body| header.checked_add(body))
        })
        .ok_or_else(|| CandidateRejection {
            trip_count: Some(trip_count),
            header_size: header_insts.len(),
            body_size: body_insts.len(),
            shape_candidate: true,
            exact_trip_candidate: true,
            ..CandidateRejection::new(LoopUnrollRejectReason::ProjectedSizeTooLarge)
        })?;
    if trip_count > MAX_FULL_UNROLL_TRIPS {
        return Err(CandidateRejection {
            trip_count: Some(trip_count),
            header_size: header_insts.len(),
            body_size: body_insts.len(),
            projected_size: Some(total),
            shape_candidate: true,
            exact_trip_candidate: true,
            ..CandidateRejection::new(LoopUnrollRejectReason::TripCountTooLarge)
        });
    }
    if total > MAX_UNROLLED_NON_TERMINATORS {
        return Err(CandidateRejection {
            trip_count: Some(trip_count),
            header_size: header_insts.len(),
            body_size: body_insts.len(),
            projected_size: Some(total),
            shape_candidate: true,
            exact_trip_candidate: true,
            ..CandidateRejection::new(LoopUnrollRejectReason::ProjectedSizeTooLarge)
        });
    }
    if header_insts
        .iter()
        .chain(&body_insts)
        .any(|&inst| matches!(data.inst_data(inst).kind(), InstKind::Alloc))
    {
        return Err(CandidateRejection {
            trip_count: Some(trip_count),
            header_size: header_insts.len(),
            body_size: body_insts.len(),
            projected_size: Some(total),
            shape_candidate: true,
            exact_trip_candidate: true,
            ..CandidateRejection::new(LoopUnrollRejectReason::ContainsAlloc)
        });
    }

    let header_values = data
        .bb_data(header)
        .params()
        .iter()
        .copied()
        .chain(data.layout().basicblock(header).insts().iter().copied())
        .collect::<FxHashSet<_>>();
    let body_values = blocks
        .iter()
        .flat_map(|&block| {
            data.bb_data(block)
                .params()
                .iter()
                .copied()
                .chain(data.layout().basicblock(block).insts().iter().copied())
        })
        .collect::<FxHashSet<_>>();
    if body_values.iter().any(|&value| {
        data.inst_data(value).used_by().iter().any(|&user| {
            data.layout()
                .parent_bb(user)
                .is_none_or(|block| !looop.contains(block))
        })
    }) {
        return Err(CandidateRejection {
            trip_count: Some(trip_count),
            header_size: header_insts.len(),
            body_size: body_insts.len(),
            projected_size: Some(total),
            shape_candidate: true,
            exact_trip_candidate: true,
            ..CandidateRejection::new(LoopUnrollRejectReason::BodyValueEscapesLoop)
        });
    }
    let loop_values = header_values.union(&body_values).copied().collect();

    Ok(UnrollCandidate {
        header,
        blocks,
        latch,
        exit,
        continue_edge: *continue_edge,
        backedge: *backedge,
        exit_edge: *exit_edge,
        trip_count,
        loop_values,
    })
}

fn projected_size(data: &FunctionData, candidate: &UnrollCandidate) -> Option<usize> {
    non_terminators(data, candidate.header)
        .len()
        .checked_mul(candidate.trip_count.checked_add(1)?)?
        .checked_add(body_size(data, candidate).checked_mul(candidate.trip_count)?)
}

fn body_size(data: &FunctionData, candidate: &UnrollCandidate) -> usize {
    candidate
        .blocks
        .iter()
        .map(|&block| non_terminators(data, block).len())
        .sum()
}

fn apply_candidate(data: &mut ArenaContextMut<'_>, candidate: &UnrollCandidate) {
    let header_source = non_terminators(data, candidate.header);
    let header_params = data.bb_data(candidate.header).params().to_vec();
    let backedge_args = candidate.backedge.args(data).to_vec();
    let exit_args = candidate.exit_edge.args(data).to_vec();
    let continue_target = candidate.continue_edge.target(data);
    let continue_args = candidate.continue_edge.args(data).to_vec();

    if candidate.trip_count == 0 {
        let values = header_params
            .iter()
            .copied()
            .map(|param| (param, param))
            .collect();
        let final_args = remap_values(&exit_args, &values, &candidate.loop_values)
            .expect("zero-trip exit arguments must be available in the original header");
        data.replace_inst_with(data.layout().basicblock(candidate.header).terminator())
            .jump(candidate.exit, final_args);
        return;
    }

    let mut current_values = FxHashMap::default();
    for &param in &header_params {
        current_values.insert(param, param);
    }
    for &inst in &header_source {
        current_values.insert(inst, inst);
    }
    for &block in &candidate.blocks {
        for &param in data.bb_data(block).params() {
            current_values.insert(param, param);
        }
        for &inst in data.layout().basicblock(block).insts() {
            current_values.insert(inst, inst);
        }
    }
    let enter_args = remap_values(&continue_args, &current_values, &candidate.loop_values)
        .expect("validated continue arguments must be remappable");
    data.replace_inst_with(data.layout().basicblock(candidate.header).terminator())
        .jump(continue_target, enter_args);

    let mut anchor = *candidate.blocks.last().unwrap();
    let mut current_latch = candidate.latch;
    for iteration in 0..candidate.trip_count {
        let next_header = new_header(data, candidate.header, iteration + 1, &mut anchor);
        let next_params = data.bb_data(next_header).params().to_vec();
        let next_args = remap_values(&backedge_args, &current_values, &candidate.loop_values)
            .expect("validated backedge arguments must be remappable");
        data.replace_inst_with(data.layout().basicblock(current_latch).terminator())
            .jump(next_header, next_args);

        let mut next_values = header_params
            .iter()
            .copied()
            .zip(next_params.iter().copied())
            .collect::<FxHashMap<_, _>>();
        clone_non_terminators(
            data,
            &header_source,
            next_header,
            &mut next_values,
            &candidate.loop_values,
        )
        .expect("validated header instructions must be remappable");

        if iteration + 1 == candidate.trip_count {
            let final_args = remap_values(&exit_args, &next_values, &candidate.loop_values)
                .expect("validated exit arguments must be remappable");
            remap_external_header_uses(
                data,
                candidate,
                &header_params,
                &header_source,
                &next_values,
            );
            let jump = data.new_local_value().jump(candidate.exit, final_args);
            data.layout_mut().insert_inst(next_header, jump);
            break;
        }

        let block_map = clone_region(
            data,
            &candidate.blocks,
            iteration + 1,
            &mut anchor,
            &mut next_values,
            &candidate.loop_values,
        )
        .expect("validated loop region must be remappable");
        let enter_args = remap_values(&continue_args, &next_values, &candidate.loop_values)
            .expect("validated continue arguments must be remappable");
        let enter_body = data
            .new_local_value()
            .jump(block_map[&continue_target], enter_args);
        data.layout_mut().insert_inst(next_header, enter_body);
        current_latch = block_map[&candidate.latch];
        current_values = next_values;
    }
}

fn remap_external_header_uses(
    data: &mut ArenaContextMut<'_>,
    candidate: &UnrollCandidate,
    header_params: &[Inst],
    header_source: &[Inst],
    final_values: &FxHashMap<Inst, Inst>,
) {
    let mut users = FxHashSet::default();
    for &value in header_params.iter().chain(header_source) {
        users.extend(
            data.inst_data(value)
                .used_by()
                .iter()
                .copied()
                .filter(|&user| {
                    data.layout().parent_bb(user).is_some_and(|block| {
                        block != candidate.header && !candidate.blocks.contains(&block)
                    })
                }),
        );
    }
    let mut mapper = IterationMapper {
        values: final_values,
        loop_values: &candidate.loop_values,
        blocks: None,
    };
    let rewrites = users
        .into_iter()
        .map(|user| {
            let mapped = data
                .inst_data(user)
                .remap_refs(&mut mapper)
                .expect("validated external header use must be remappable");
            (user, mapped)
        })
        .collect::<Vec<_>>();
    for (user, mapped) in rewrites {
        data.replace_inst_with(user).raw(mapped);
    }
}

fn new_header(
    data: &mut ArenaContextMut<'_>,
    source: BasicBlock,
    iteration: usize,
    anchor: &mut BasicBlock,
) -> BasicBlock {
    let name = format!("{}_unroll_{iteration}", data.bb_data(source).name());
    let param_types = data
        .bb_data(source)
        .params()
        .iter()
        .map(|&param| data.inst_data(param).ty().clone())
        .collect();
    let block = data.new_basic_block().basic_block(name, param_types);
    data.layout_mut().insert_bb_after(*anchor, block);
    *anchor = block;
    block
}

fn clone_region(
    data: &mut ArenaContextMut<'_>,
    sources: &[BasicBlock],
    iteration: usize,
    anchor: &mut BasicBlock,
    values: &mut FxHashMap<Inst, Inst>,
    loop_values: &FxHashSet<Inst>,
) -> Result<FxHashMap<BasicBlock, BasicBlock>, CloneError> {
    let mut blocks = FxHashMap::default();
    for &source in sources {
        let name = format!("{}_unroll_{iteration}", data.bb_data(source).name());
        let param_types = data
            .bb_data(source)
            .params()
            .iter()
            .map(|&param| data.inst_data(param).ty().clone())
            .collect();
        let block = data.new_basic_block().basic_block(name, param_types);
        data.layout_mut().insert_bb_after(*anchor, block);
        *anchor = block;
        blocks.insert(source, block);
        for (&source_param, &cloned_param) in data
            .bb_data(source)
            .params()
            .iter()
            .zip(data.bb_data(block).params())
        {
            values.insert(source_param, cloned_param);
        }
    }
    for &source in sources {
        let insts = data
            .layout()
            .basicblock(source)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for inst in insts {
            let ty = data.inst_data(inst).ty().clone();
            let shell = data.new_local_value().undef(ty);
            values.insert(inst, shell);
        }
    }
    let mut mapper = IterationMapper {
        values,
        loop_values,
        blocks: Some(&blocks),
    };
    for &source in sources {
        let destination = blocks[&source];
        let insts = data
            .layout()
            .basicblock(source)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for inst in insts {
            let mapped = data.inst_data(inst).remap_refs(&mut mapper)?;
            let cloned = mapper.values[&inst];
            data.replace_inst_with(cloned).raw(mapped);
            data.layout_mut().insert_inst(destination, cloned);
        }
    }
    Ok(blocks)
}

fn clone_non_terminators(
    data: &mut ArenaContextMut<'_>,
    source: &[Inst],
    destination: BasicBlock,
    values: &mut FxHashMap<Inst, Inst>,
    loop_values: &FxHashSet<Inst>,
) -> Result<(), CloneError> {
    for &inst in source {
        let ty = data.inst_data(inst).ty().clone();
        let shell = data.new_local_value().undef(ty);
        values.insert(inst, shell);
    }
    let mut mapper = IterationMapper {
        values,
        loop_values,
        blocks: None,
    };
    for &inst in source {
        let mapped = data.inst_data(inst).remap_refs(&mut mapper)?;
        let cloned = mapper.values[&inst];
        data.replace_inst_with(cloned).raw(mapped);
        data.layout_mut().insert_inst(destination, cloned);
    }
    Ok(())
}

fn remap_values(
    source: &[Inst],
    values: &FxHashMap<Inst, Inst>,
    loop_values: &FxHashSet<Inst>,
) -> Result<Vec<Inst>, CloneError> {
    source
        .iter()
        .map(|&value| map_value(value, values, loop_values))
        .collect()
}

fn map_value(
    value: Inst,
    values: &FxHashMap<Inst, Inst>,
    loop_values: &FxHashSet<Inst>,
) -> Result<Inst, CloneError> {
    if value.is_global() {
        return Ok(value);
    }
    if let Some(&mapped) = values.get(&value) {
        return Ok(mapped);
    }
    if loop_values.contains(&value) {
        return Err(CloneError::MissingLoopValue(value));
    }
    Ok(value)
}

struct IterationMapper<'a> {
    values: &'a FxHashMap<Inst, Inst>,
    loop_values: &'a FxHashSet<Inst>,
    blocks: Option<&'a FxHashMap<BasicBlock, BasicBlock>>,
}

impl EntityMapper for IterationMapper<'_> {
    type Error = CloneError;

    fn map_inst(&mut self, inst: Inst) -> Result<Inst, Self::Error> {
        map_value(inst, self.values, self.loop_values)
    }

    fn map_block(&mut self, block: BasicBlock) -> Result<BasicBlock, Self::Error> {
        Ok(self
            .blocks
            .and_then(|blocks| blocks.get(&block).copied())
            .unwrap_or(block))
    }
}

fn non_terminators(data: &FunctionData, block: BasicBlock) -> Vec<Inst> {
    let insts = data.layout().basicblock(block).insts();
    insts
        .iter()
        .copied()
        .take(insts.len().saturating_sub(1))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opt::utils::logical_edge::outgoing_edges;

    struct Fixture {
        program: Program,
        function: Function,
        header: BasicBlock,
        body: BasicBlock,
        exit: BasicBlock,
    }

    fn fixture(initial: i32, bound: i32, step: i32) -> Fixture {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "unroll".into(), vec![]);
        let (header, body, exit) = {
            let data = program.func_data_mut(function);
            let entry = data.add_entry_block();
            let header = data
                .new_basic_block()
                .basic_block("header".into(), vec![Type::get_i32(), Type::get_i32()]);
            let body = data.new_basic_block().basic_block("body".into(), vec![]);
            let exit = data
                .new_basic_block()
                .basic_block("exit".into(), vec![Type::get_i32()]);
            for block in [header, body, exit] {
                data.layout_mut().push_bb_back(block);
            }

            let initial = data.new_local_value().integer(initial);
            let zero = data.new_local_value().integer(0);
            let entry_jump = data.new_local_value().jump(header, vec![initial, zero]);
            data.layout_mut().insert_inst(entry, entry_jump);

            let params = data.bb_data(header).params().to_vec();
            let iv = params[0];
            let acc = params[1];
            let bound = data.new_local_value().integer(bound);
            let compare = if step > 0 {
                data.new_local_value().binary(BinaryOp::Lt, iv, bound)
            } else {
                data.new_local_value().binary(BinaryOp::Gt, iv, bound)
            };
            data.layout_mut().insert_inst(header, compare);
            let branch = data
                .new_local_value()
                .branch(compare, body, vec![], exit, vec![acc]);
            data.layout_mut().insert_inst(header, branch);

            let next_acc = data.new_local_value().binary(BinaryOp::Add, acc, iv);
            let step_value = data.new_local_value().integer(step.unsigned_abs() as i32);
            let next_iv = if step > 0 {
                data.new_local_value().binary(BinaryOp::Add, iv, step_value)
            } else {
                data.new_local_value().binary(BinaryOp::Sub, iv, step_value)
            };
            for inst in [next_acc, next_iv] {
                data.layout_mut().insert_inst(body, inst);
            }
            let backedge = data.new_local_value().jump(header, vec![next_iv, next_acc]);
            data.layout_mut().insert_inst(body, backedge);

            let result = data.bb_data(exit).params()[0];
            let ret = data.new_local_value().ret(Some(result));
            data.layout_mut().insert_inst(exit, ret);
            (header, body, exit)
        };
        Fixture {
            program,
            function,
            header,
            body,
            exit,
        }
    }

    fn run(fixture: &mut Fixture) -> bool {
        let mut data = ArenaContextMut {
            program: &mut fixture.program,
            curr_func: Some(fixture.function),
        };
        LoopUnroll::new(
            LoopUnrollMode::Enabled,
            Arc::new(Mutex::new(PassesRunStats::default())),
            false,
        )
        .run_on(&mut data)
    }

    fn assert_edge_arguments_well_typed(data: &FunctionData) {
        let cfg = CFG::new(data).unwrap();
        for &source in cfg.blocks() {
            for edge in outgoing_edges(data, source) {
                let params = data.bb_data(edge.target(data)).params();
                let args = edge.args(data);
                assert_eq!(args.len(), params.len());
                for (&arg, &param) in args.iter().zip(params) {
                    assert_eq!(data.inst_data(arg).ty(), data.inst_data(param).ty());
                }
            }
        }
    }

    #[test]
    fn fully_unrolls_a_small_forward_loop_and_is_idempotent() {
        let mut fixture = fixture(0, 4, 1);
        assert!(run(&mut fixture));
        assert!(!run(&mut fixture));

        let data = fixture.program.func_data(fixture.function);
        let (_cfg, _dom, loops) = LoopAnalysis::new(data);
        assert!(loops.loops().is_empty());
        assert_edge_arguments_well_typed(data);
        let header_blocks = data
            .layout()
            .basicblocks()
            .iter()
            .filter(|block| data.bb_data(block.bb()).name().contains("header"))
            .count();
        let body_blocks = data
            .layout()
            .basicblocks()
            .iter()
            .filter(|block| data.bb_data(block.bb()).name().contains("body"))
            .count();
        assert_eq!(header_blocks, 5);
        assert_eq!(body_blocks, 4);

        let final_header = data
            .layout()
            .basicblocks()
            .iter()
            .find(|block| {
                let terminator = block.terminator();
                matches!(data.inst_data(terminator).kind(), InstKind::Jump(jump)
                    if jump.target() == fixture.exit)
            })
            .unwrap()
            .bb();
        let final_params = data.bb_data(final_header).params();
        let InstKind::Jump(exit_jump) = data
            .inst_data(data.layout().basicblock(final_header).terminator())
            .kind()
        else {
            unreachable!();
        };
        assert_eq!(exit_jump.args(), [final_params[1]]);
    }

    #[test]
    fn preserves_the_final_failed_header_visit_for_zero_trip_loops() {
        let mut fixture = fixture(0, 0, 1);
        assert!(run(&mut fixture));
        let data = fixture.program.func_data(fixture.function);
        let InstKind::Jump(jump) = data
            .inst_data(data.layout().basicblock(fixture.header).terminator())
            .kind()
        else {
            panic!("zero-trip header must jump directly to exit");
        };
        assert_eq!(jump.target(), fixture.exit);
        assert_eq!(jump.args().len(), 1);
        let (_cfg, _dom, loops) = LoopAnalysis::new(data);
        assert!(loops.loops().is_empty());
        assert_edge_arguments_well_typed(data);
    }

    #[test]
    fn unrolls_backward_and_non_unit_loops() {
        for (initial, bound, step, expected_headers) in [(7, 0, -2, 5), (0, 7, 2, 5)] {
            let mut fixture = fixture(initial, bound, step);
            assert!(run(&mut fixture));
            let data = fixture.program.func_data(fixture.function);
            let headers = data
                .layout()
                .basicblocks()
                .iter()
                .filter(|block| data.bb_data(block.bb()).name().contains("header"))
                .count();
            assert_eq!(headers, expected_headers);
            assert_edge_arguments_well_typed(data);
        }
    }

    #[test]
    fn rewrites_external_header_parameter_uses_to_the_final_iteration() {
        let mut fixture = fixture(0, 4, 1);
        let original_acc = fixture
            .program
            .func_data(fixture.function)
            .bb_data(fixture.header)
            .params()[1];
        let ret = fixture
            .program
            .func_data(fixture.function)
            .layout()
            .basicblock(fixture.exit)
            .terminator();
        fixture
            .program
            .func_data_mut(fixture.function)
            .replace_inst_with(ret)
            .ret(Some(original_acc));

        assert!(run(&mut fixture));
        let data = fixture.program.func_data(fixture.function);
        let InstKind::Return(ret) = data.inst_data(ret).kind() else {
            unreachable!();
        };
        let final_value = ret.value().unwrap();
        assert_ne!(final_value, original_acc);
        let final_header = data.layout().parent_bb(final_value).or_else(|| {
            data.layout().basicblocks().iter().find_map(|block| {
                data.bb_data(block.bb())
                    .params()
                    .contains(&final_value)
                    .then_some(block.bb())
            })
        });
        assert!(final_header.is_some());
        assert!(
            data.bb_data(final_header.unwrap())
                .name()
                .contains("unroll_4")
        );
    }

    #[test]
    fn rejects_large_trip_counts() {
        let mut large = fixture(0, 9, 1);
        assert!(!run(&mut large));
    }

    #[test]
    fn clones_body_block_parameters() {
        let mut parameterized = fixture(0, 4, 1);
        let function = parameterized.function;
        let header = parameterized.header;
        let body = parameterized.body;
        {
            let data = parameterized.program.func_data_mut(function);
            let _parameter = data.new_basic_block().add_param(body, Type::get_i32());
            let zero = data.new_local_value().integer(0);
            let terminator = data.layout().basicblock(header).terminator();
            let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
                unreachable!();
            };
            let cond = branch.cond();
            let false_target = branch.f_target();
            let false_args = branch.f_args().to_vec();
            data.replace_inst_with(terminator).branch(
                cond,
                body,
                vec![zero],
                false_target,
                false_args,
            );
        }
        assert!(run(&mut parameterized));
        let data = parameterized.program.func_data(function);
        assert_edge_arguments_well_typed(data);
        let (_cfg, _dom, loops) = LoopAnalysis::new(data);
        assert!(loops.loops().is_empty());
    }
}
