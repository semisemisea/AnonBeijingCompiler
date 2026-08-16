//! # PointerStrengthReduction：指针强度削减（PSR）
//!
//! 把循环内**仿射索引**的 GEP 地址计算改写为指针递进：循环里每轮"重新
//! 计算 `base + i*c + off`"变成"前一轮地址 + 常量步长"，把乘法/加法链
//! 变成一条地址增量。本文件是 pass 骨架（`run_on` 主循环 + 数据结构）；
//! 实现按阶段拆在三个子模块里：
//!
//! - `analysis` 子模块：仿射索引演化分类与范围分析
//!   （`classify_index_evolution`：索引是循环不变量 / 直接 IV / 仿射
//!   IV 表达式，系数 + 偏移范围，溢出检查 `affine_range_fits_i32`）；
//! - `candidate` 子模块：候选发现与可达性/安全性检查
//!   （`find_candidate`：GEP 必须只被循环内 load/store 使用、仿射步长
//!   每轮恒定、不逃逸等）；
//! - `rewrite` 子模块：改写实施（preheader 克隆初值、
//!   `apply_candidate`：header 增加携带指针的参数，backedge 传
//!   "指针 + 步长"）。
//!
//! ## 变换形态（IR 示例）
//!
//! ```text
//! 前（每轮重算地址）：
//!   header(i):  p = gep base, [i, 0]        // i 是 IV，步长 = c 字节
//!               v = load p
//!               i' = i + 1; jump header(i')
//! 后（指针递进，preheader 克隆初值）：
//!   pre:  p0 = gep base, [i0, 0];  jump header(i0, p0)
//!   header(i, p):  v = load p
//!                  p' = p + c;  i' = i + 1;  jump header(i', p')
//! ```
//!
//! `p' = p + c` 是每轮恒定步长的地址加法，后端可融合进访存寻址
//! （`ldr w0, [p], #c` 后变址），省掉每轮的 GEP 乘法。
//!
//! ## 触发 / 放弃条件
//!
//! - 候选 GEP 的索引对最内层循环的 IV 呈**仿射**（`IndexEvolution::Affine`
//!   带系数与偏移；不变量 `Invariant` 与直接 IV `Direct` 也可处理）；
//! - GEP 只能被循环内 load/store 使用（`has_only_loop_memory_users`），
//!   地址不逃逸出循环；
//! - 仿射范围必须能安全折叠进 i32（`affine_range_fits_i32`——系数 ×
//!   迭代范围不溢出，指针步长 `pointer_step` 可精确表示）；
//! - 多组 backedge 来源（`BackedgeGroup`）需逐一验证参数一致性；
//! - 放弃：函数无循环（`cfg.is_acyclic()` 直接返回）、声明函数、索引演化
//!   含未知成分（非仿射）。
//!
//! ## 正确性
//!
//! - 指针递进由 IV 的仿射闭式（`evaluate_affine_initial` + 每轮 `+ step`）
//!   推导，与每轮重算的 GEP 地址**逐轮相等**（数学归纳：初值相等、步长
//!   相等）；
//! - 改写要求地址只在循环内使用，避免循环外引用到中间指针；
//! - 溢出防护：系数/范围/步长全程 i64 计算并检查 i32 可表示，防地址运算
//!   回绕语义被破坏。
//!
//! ## 管线位置
//!
//! - 注册：`opt/pass.rs` 的 `from_config`，fixpoint 段，`dse` 之后、
//!   `guard_elimination` 之前（内存画像稳定后改写地址）；
//! - 无目标门控、无 config 开关；
//! - 成本预估（`utils/pointer_strength_reduction_cost.rs`，AArch64 专用
//!   `estimate_aarch64_pointer_strength_reduction`）在候选阶段参与收益判断。
//!
//! ## 验证
//!
//! - 本文件 `mod tests` 委托 `tests.rs`（候选/分析/改写三件套的单测）；
//! - 端到端：`make test` 差分比对。

use crate::opt::{
    analysis_passes::{
        dom_tree::v2::DominanceTree,
        induction_variable::{
            BasicInductionVariableAnalysis, ConstantInductionRange, InductionStep,
            constant_induction_range, normalize_strict_exit,
        },
        loop_analysis::{Loop, LoopAnalysis},
        range::{IntRange, RangeAnalysis, RangeContext},
    },
    prelude::*,
    utils::{
        cfg::CFG,
        gep::gep_index_stride,
        logical_edge::{
            LogicalEdge, LogicalEdgeRewriter, forwarded_block_params, incoming_edges,
            outgoing_edges, resolve_forwarded_params,
        },
        pointer_strength_reduction_cost::estimate_aarch64_pointer_strength_reduction,
        preheader::{EnsurePreheader, ensure_preheader},
    },
};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

pub struct PointerStrengthReduction;

// 一个可改写的候选：循环内某条 GEP（`gep`）连同驱动它的 IV（`iv`）、
// IV 在 header 参数里的位置、backedge 分组与扁平化地址演化等。
// 由 `candidate::find_candidate` 产出，交给 `rewrite::apply_candidate` 实施。
struct Candidate {
    gep: Inst,
    iv: Inst,
    header_iv_position: usize,
    backedge_groups: SmallVec<[BackedgeGroup; 2]>,
    base: Inst,
    offsets: Vec<Inst>,
    pointer_ty: Type,
    address_evolution: FlattenedAddressEvolution,
    forwarded_params: FxHashMap<Inst, Inst>,
}

#[derive(Clone)]
// GEP 某一维索引对循环 IV 的演化分类：不变量 / 直接跟随 IV / 仿射表达式。
// 分类由 `analysis::classify_index_evolution` 完成，是步长推导
// （`FlattenedAddressEvolution`）与可达性检查的共同基础。
enum IndexEvolution {
    Invariant,
    Direct,
    Affine(AffineI32Expr),
}

#[derive(Clone)]
// 仿射索引 `coefficient * iv + offset` 的展开形态：除系数与偏移范围外，
// 还记录推导它的指令链（`chain`，改写后成为死代码，是可删收益的来源）
// 与链上用到的不变量（`invariants`，改写时须在 header 处可得）。
struct AffineI32Expr {
    value: Inst,
    coefficient: i64,
    offset_range: I64Range,
    chain: SmallVec<[Inst; 4]>,
    invariants: SmallVec<[Inst; 4]>,
}

struct FlattenedAddressEvolution {
// 把 GEP 各维索引按 `IndexEvolution` 扁平化后的"整条地址"演化：
// `coefficient` 是各维贡献按 i64 checked 累加的合并系数，
// `pointer_step` 是每轮指针前进的常量字节步长（i32，溢出检查保证可表示）。
    indices: Vec<IndexEvolution>,
    coefficient: i64,
    pointer_step: i32,
}

#[derive(Clone, Copy)]
struct I64Range {
    min: i64,
    max: i64,
}

impl I64Range {
// 全程在 64 位区间上做 checked 算术：仿射范围/步长推导中任何一步溢出
// 都返回 `None` 放弃候选，防止地址运算回绕语义被破坏。
    fn from_i32(range: IntRange) -> Option<Self> {
        Some(Self {
            min: i64::from(range.min()?),
            max: i64::from(range.max()?),
        })
    }

    fn add(self, other: Self) -> Option<Self> {
        Some(Self {
            min: self.min.checked_add(other.min)?,
            max: self.max.checked_add(other.max)?,
        })
    }

    fn sub(self, other: Self) -> Option<Self> {
        Some(Self {
            min: self.min.checked_sub(other.max)?,
            max: self.max.checked_sub(other.min)?,
        })
    }

    fn mul(self, other: Self) -> Option<Self> {
        let values = [
            self.min.checked_mul(other.min)?,
            self.min.checked_mul(other.max)?,
            self.max.checked_mul(other.min)?,
            self.max.checked_mul(other.max)?,
        ];
        Some(Self {
            min: *values.iter().min().unwrap(),
            max: *values.iter().max().unwrap(),
        })
    }
}

#[derive(Clone)]
// 来自同一个 latch 的一组 backedge 共享同一条指针增量指令
// （`rewrite::apply_candidate` 按组只插入一次 `gep pointer, [step]`）。
struct BackedgeGroup {
    source: BasicBlock,
    edges: SmallVec<[LogicalEdge; 2]>,
}

enum ApplyResult {
// `apply_candidate` 的结果：`Changed` 表示改写落地；`Unchanged` 表示
// 因条件不满足（如缺 preheader）放弃，本循环跳过。
    Unchanged,
    Changed,
}

mod analysis;
mod candidate;
mod rewrite;

impl Pass for PointerStrengthReduction {
    // PSR 入口（fixpoint 驱动）。每轮分三步：重建全套分析快照 →
    // 逐循环收集候选（含成本判定）→ 改写**一个**候选后整体重跑。
    // 因为任何改写都会使快照分析失效，同一轮内不能连续改写，
    // 只能"改一个 → 重建 → 再找"，直到某轮不再有任何变换。
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        // 声明函数没有函数体，无循环可处理，直接返回。
        if data.layout().is_decl() {
            return false;
        }
        // `changed` 汇总整个 pass 运行期间是否发生过改写，
        // 最终作为返回值交给上层 pass 管理器的 fixpoint 判定。
        let mut changed = false;
        loop {
            // 每轮 fixpoint 从重建 CFG 开始：上轮若改写成功，旧快照已不可用。
            let Some(cfg) = CFG::new(data) else {
                return changed;
            };
            // 无环函数不存在可递进指针的循环，PSR 无事可做。
            if cfg.is_acyclic() {
                return changed;
            }
            // 收集"block 参数 → 定义块"的映射：候选检查里遇到 BlockArgRef
            // 形式的值时，要据此找到它的定义块再判定是否在 header 处可得。
            let parameter_blocks = cfg
                .blocks()
                .iter()
                .flat_map(|&block| {
                    data.bb_data(block)
                        .params()
                        .iter()
                        .copied()
                        .map(move |parameter| (parameter, block))
                })
                .collect::<FxHashMap<_, _>>();
            let (cfg, dom_tree, loops) = LoopAnalysis::from_cfg(cfg);
            let ivs = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
            // 以下分析全是当前 IR 的只读快照：循环结构、支配树、IV 与
            // 整数取值范围。IV 的无回绕证明依赖 `constant_induction_range`，
            // 取值范围分析又需要函数级非负摘要辅助（见下）。
            let range_arena = ArenaContext {
                program: &*data.program,
                curr_func: data.curr_func,
            };
            let nonneg = crate::opt::analysis_passes::return_summary::nonneg_preserving_functions(
                data.program,
            );
            // 保非负函数清单用于收紧仿射索引的取值区间，帮助
            // `affine_range_fits_i32` 通过 i32 中间范围证明。
            let no_params = FxHashSet::default();
            let ranges = RangeAnalysis::new(&range_arena, &cfg, &loops, &ivs, &nonneg, &no_params);
            let mut transformed = false;
            // 逐循环找候选。每个循环只扫描"恰好属于它"的块
            // （`min_loop_contain` == 本循环），嵌套循环体内的 GEP 归内层
            // 循环自己的迭代处理，天然避免父子循环重复改写。
            for looop in loops.loops() {
                // 候选收集 + 安全性检查 + 成本判定：AArch64 成本模型
                // （`estimate_aarch64_pointer_strength_reduction`）不盈利即
                // 放弃；同一循环多个候选按 `is_better_than` 只留一个最佳。
                let Some(candidate) = Self::find_candidate(
                    data,
                    &cfg,
                    &dom_tree,
                    &loops,
                    &ivs,
                    &ranges,
                    looop,
                    &parameter_blocks,
                ) else {
                    // 本循环无合格候选，看下一个循环。
                    continue;
                };
                // 改写调度：命中候选就尝试改写；失败（如无 preheader、
                // 初值偏移越界）返回 `Unchanged`，不影响其他循环。
                match Self::apply_candidate(data, &cfg, looop, candidate) {
                    ApplyResult::Unchanged => continue,
                    ApplyResult::Changed => {
                        // 改写落地：记录后立即 break，外层重建全部分析
                        // 再来一轮（快照已失效，继续用会基于过期数据改写）。
                        changed = true;
                        transformed = true;
                        break;
                    }
                }
            }
            // 整轮没有任何变换：fixpoint 收敛，带着累计的 `changed` 退出。
            if !transformed {
                return changed;
            }
        }
    }
}

#[cfg(test)]
mod tests;
