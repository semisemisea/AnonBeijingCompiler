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
enum IndexEvolution {
    Invariant,
    Direct,
    Affine(AffineI32Expr),
}

#[derive(Clone)]
struct AffineI32Expr {
    value: Inst,
    coefficient: i64,
    offset_range: I64Range,
    chain: SmallVec<[Inst; 4]>,
    invariants: SmallVec<[Inst; 4]>,
}

struct FlattenedAddressEvolution {
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
struct BackedgeGroup {
    source: BasicBlock,
    edges: SmallVec<[LogicalEdge; 2]>,
}

enum ApplyResult {
    Unchanged,
    Changed,
}

mod analysis;
mod candidate;
mod rewrite;

impl Pass for PointerStrengthReduction {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().is_decl() {
            return false;
        }
        let mut changed = false;
        loop {
            let Some(cfg) = CFG::new(data) else {
                return changed;
            };
            if cfg.is_acyclic() {
                return changed;
            }
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
            let range_arena = ArenaContext {
                program: &*data.program,
                curr_func: data.curr_func,
            };
            let nonneg = crate::opt::analysis_passes::return_summary::nonneg_preserving_functions(
                data.program,
            );
            let no_params = FxHashSet::default();
            let ranges = RangeAnalysis::new(&range_arena, &cfg, &loops, &ivs, &nonneg, &no_params);
            let mut transformed = false;
            for looop in loops.loops() {
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
                    continue;
                };
                match Self::apply_candidate(data, &cfg, looop, candidate) {
                    ApplyResult::Unchanged => continue,
                    ApplyResult::Changed => {
                        changed = true;
                        transformed = true;
                        break;
                    }
                }
            }
            if !transformed {
                return changed;
            }
        }
    }
}

#[cfg(test)]
mod tests;
