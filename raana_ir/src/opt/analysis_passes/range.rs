//! Function-local signed i32 range analysis.
//!
//! The abstract domain is one closed interval with an optional hole at zero.
//! Arithmetic that may wrap for a non-singleton input is deliberately mapped
//! to `full`; singleton operations use RaanaIR's wrapping integer semantics.
//!
//! ---
//!
//! ## 补充说明（中文）
//!
//! 术语：区间 / 抽象域 / 数据流分析（不动点）/ 回边 / 归纳变量（IV）/ 强度削减
//! （SR）/ PSR 等见 `docs/offline-handbook/glossary.md` 的「IR 与 SSA 基础」
//! 「循环」「强度削减」分组；「结构边 vs 逻辑边」「Block 参数（Phi）」见
//! 「IR 与 SSA 基础」分组。
//!
//! ### 定位
//!
//! 本模块是**函数内**（intra-procedural）的 i32 区间分析基础设施：对当前函数里
//! 每条 i32 值，计算它在各个程序点（函数级 / 块入口 / 逻辑边 / 指令前）可能取到
//! 的取值区间，供改写 pass 做「这个运算不会回绕」「这个值非负」「这个值落在
//! `[0, 2P)`」这类证明。结果是**保守上近似**：证明不了的值一律给最宽区间
//! （`full`），宁漏勿错。
//!
//! 与 `cfg` / `dom_tree` / `loop_analysis` / `induction_variable` 等分析一致，
//! 它是**快照**：`RangeAnalysis::new` 一次性建好（构造时已跑完全部迭代），之后
//! 只读查询；任何 IR 修改（增删 / 改写指令）都会让结果过期，使用方必须重建。
//!
//! ### 抽象域：闭区间 + 零点空洞
//!
//! `IntRange` 是带 bottom / top 的三值格（bottom < 具体区间 < top）：
//!
//! - `Empty`：空集（bottom），表示「该程序点不可达」或「假设自相矛盾」；
//! - `Bounded { min, max, contains_zero }`：闭区间 `[min, max]`，外加布尔标记
//!   `contains_zero`。**零点空洞**：`contains_zero == false` 表示区间**排除 0**
//!   （如 `assume_ne(0)` 之后），而 `min <= 0 <= max` 只表示「数值上覆盖 0 这个
//!   点」。这样「除 0 外的区间」不必拆成两段，代价是表达能力弱于「两个闭区间的
//!   并」——这是本设计的取舍：够用，且 join / intersect 都是 O(1)。
//!
//! `impl IntRange` 上的方法：
//!
//! - `empty()` / `full()` / `constant(value)` / `bounded(min, max)`：四种构造
//!   （`bounded` 按 `min <= 0 <= max` 自动决定 `contains_zero`，`min > max`
//!   时归一化为 `Empty`）；
//! - `min()` / `max()` / `singleton()`：`Empty` 时返回 `None`；`singleton` 在
//!   `min == max` 时返回唯一确定值；
//! - `contains(value)` / `contains_zero()` / `excludes_zero()`：成员查询，
//!   `contains` 同时检查闭区间与零点空洞（`value == 0` 时要求 `contains_zero`）；
//! - `join`（并，取上确界）/ `intersect`（交，取下确界）/ `is_subset_of`
//!   （子集判定，别名 `subset`）：格运算；
//! - `assume_eq` / `assume_ne` / `assume_lt` / `assume_le` / `assume_gt` /
//!   `assume_ge`：在「该条件为真」的假设下收紧自身——`assume_eq` 就是
//!   `intersect`；`assume_ne(0)` 只清掉零点空洞（对单点 `{0}` 给 `Empty`）；
//!   大小比较与对应半边区间求交，边界处 `checked_add` / `checked_sub` 溢出
//!   视为 `Empty`（如 `assume_lt` 对 `other.max() == i32::MIN` 的情形）；
//! - `widen(next)`：**宽化算子**（见「算法」第 4 步）：`next` 越过自身边界时
//!   直接把该侧放宽到 `i32::MIN` / `i32::MAX`，保证循环迭代收敛。
//!
//! ### wrap 处理：单点走包装语义，非常量走数学区间
//!
//! 编译器里 `+` / `-` / `*` 等是 **i32 包装运算**（RaanaIR 语义，见英文文档）。
//! `transfer_binary` 的策略：
//!
//! - 两个操作数都是**单点**：用 `fold_binary` 按包装语义精确折叠（如
//!   `i32::MAX + 1` → `i32::MIN`，与运行时一致）；除 / 余除数为 0 时例外 →
//!   `full`；
//! - 操作数不全是单点：用 `mathematical_binary` 在 **i64 上**做端点运算
//!   （加 / 减 / 乘 / 常量移位 / `min` / `max`），结果能落回 i32 才给区间；端点
//!   越出 i32 范围（可能回绕）时返回 `None` → `full`。**非常量操作数的算术要么
//!   给精确区间、要么给 `full`，绝不给出会漏掉回绕值的假区间**；
//! - 比较指令：`comparison_range` 证明恒真 / 恒假时给 `1` / `0`，否则给
//!   `[0, 1]`；
//! - 按位与：单点掩码 ≥ 0 时给 `[0, mask]`；`Or` / `Xor` 一边为 0 时给另一边；
//!   逻辑右移 `Shr` 给 `[0, i32::MAX]`；算术右移 `Sar` 在移位量为单点且
//!   `[0, 32)` 时按端点移位给区间；
//! - 除 / 余只对**单点除数**精化：除数为 0、或 `i32::MIN / -1`（回绕）→
//!   `full`；`transfer_rem` 在**被除数可证非负**时把余数压到 `[0, |divisor|-1]`
//!   （截断余数符号跟随被除数，见测试 `remainder_of_non_negative_dividend_
//!   is_non_negative`），否则给 `[-m, m]`（`m = |divisor|-1` 封顶，防
//!   `i32::MIN` 的绝对值溢出）——这是 `mod_fold` 链式 `%` 折叠能继续的前提；
//! - `Select`：条件恒 0 / 恒非 0 时只算对应分支，否则两分支 `join`；`Cast`：
//!   源是常量 `f32`（有限且在 i32 范围内，`fold_f32_to_i32`）时精确折叠，源是
//!   整型常量时返回源区间，其余 `full`；
//! - 调用（`call_range`）：`soyo_mulmod(a, b, p)`（`return_summary::MODMUL_
//!   BUILTIN`）在 `a`、`b` 都可证非负时给 `[0, p-1]`（`p` 为正常量）否则
//!   `[0, i32::MAX]`；`return_summary::nonneg_preserving_functions` 里的纯函数
//!   在**所有实参**都可证非负时给 `[0, i32::MAX]`；其余调用一律 `full`。
//!
//! ### 核心 API（`impl RangeAnalysis`）
//!
//! 构造：`new(&ArenaContext, &CFG, &LoopAnalysis, &BasicInductionVariableAnalysis,
//! &nonneg_preserving, &nonneg_params) -> Self`。后两个 `FxHashSet` 来自
//! `return_summary`（跨过程非负摘要，见「算法」）。构造时跑完整轮前向数据流
//! （必要时重解一遍），之后的查询全是只读。
//!
//! 查询（`&self`，统一经 `range_in_context(value, context)` 分发）：
//!
//! - `range_of(value)`：函数级区间——所有上下文 join 后的整体范围，最粗；
//! - `range_at_block_entry(block, value)`：`block` 入口处（各入边状态 join 后）
//!   的区间；无记录时回退 `base_range`；
//! - `range_on_edge(edge, value)`：沿某条**逻辑边**的区间——分支条件沿该边的
//!   假设已精化（true 臂上 `cond != 0`，false 臂上 `cond == 0`）；该边不可达
//!   （状态缺失）时返回 `empty`；
//! - `range_before(instruction, value)`：**指令前**程序点的区间——当前块内
//!   该指令之前所有已定值事实（含此前分支精化）。`mod_fold` / `pointer_strength_
//!   reduction` 的核心查询；
//! - `loop_header_range(header, parameter)`：循环头块参数的入口区间
//!   （`range_at_block_entry` 的别名），测试里查 IV 范围用；
//! - `range_of_fresh(value)`：忽略 `solve` 记录在 `ranges` 里的该值自身区间，
//!   沿 def-use 链**重推**一遍。动机：`solve` 处理某块时就把块内指令的区间记进
//!   `ranges`，而入口参数的传播值要到 worklist 收尾才 join 进来，所以「两个入口
//!   参数的运算结果」这类值在 `ranges` 里可能是过期的 `full`。注释点名给
//!   return summary / guard folding 类调用方用稳定答案；目前仓库内无外部调用点
//!   （见「使用方清单」）；
//! - `proves_binary_no_signed_wrap(op, lhs, rhs, context) -> bool`：证明 `Add` /
//!   `Sub` / `Mul` / `Shl` 在给定程序点**不发生有符号回绕**——对两个操作数在
//!   `context` 下求区间后跑 `mathematical_binary`（i64 端点运算不越界即无回绕）。
//!   PSR 用它保证仿射地址算术的健全性。
//!
//! `RangeContext` 是查询上下文枚举，四个变体 `Function` / `BlockEntry(block)` /
//! `Edge(edge)` / `Before(inst)` 与上述查询一一对应。
//!
//! ### 算法：带分支精化的前向数据流（worklist 不动点）
//!
//! 1. **定义收集**：`new` 先扫全函数指令，把每条 i32 值登记成 `ValueDef`
//!    （`Integer` / `ZeroInit` / `Undef` / `Binary(op, lhs, rhs)` / `Select` /
//!    `Cast` / `Call(callee, args)` / `BlockParameter` / `Unknown`；非 i32 一律
//!    `Unknown`，查询得 `full`）；
//! 2. **入口初始化**：入口块参数给 `full`；`return_summary::always_nonneg_params`
//!    里本函数的参数给 `[0, i32::MAX]`；
//! 3. **worklist 迭代**（`solve`）：块出队后按指令顺序求值。回边携带的值是旧
//!    观察：处理块内每条 i32 指令前先从状态里删掉它（SSA 定义先于使用），把
//!    当前状态存入 `before[inst]`，算出区间后写回状态并 `join` 进函数级
//!    `ranges`。块尾沿每条出边克隆状态，`refine_edge` 用分支条件精化（见上；
//!    条件是整数比较时再对两个操作数做 `refine_comparison` 双向 `assume_*`；
//!    精化结果为空 → 该边不可达，整条边跳过）。边状态变化就把它 `join` 进目标
//!    块的 `block_entries`，变化则重新入队；`join_incoming` 只对目标**块参数**
//!    求各入边实参的区间（其余值靠 `evaluate_contextual` 沿 def-use 链惰性重推，
//!    避免 O(入边 × 状态) 的拷贝开销）；
//! 4. **循环头宽化**：循环头参数每次经回边变化时计数，变化 ≥ `WIDEN_AFTER`(3)
//!    次后改用 `widen` 直接放宽到边界方向——保证含未知递推（如 `x *= 2`）的
//!    循环也能收敛（见测试 `unrecognized_loop_recurrence_widens_to_terminate`）；
//! 5. **归纳变量封顶**（`add_induction_caps`，消费 `loop_analysis` +
//!    `induction_variable`）：对形如 `iv < bound`（步长 > 0）或 `iv > bound`
//!    （步长 < 0）的**严格 exit** 测试，若步长为常量且不会回绕（`|step| ≠ 1`
//!    时用 i64 检查终端值不越界），按初值 `join` 终端值给 IV 算出封顶区间
//!    `loop_caps`；只要产生过 cap 就**清空全部状态重解一遍**，让封顶参与后续
//!    迭代（测试 `loop_cap_includes_failing_header_visit`）；
//! 6. **按需求值**（`evaluate_contextual` / `evaluate_def`）：查询时若当前 `facts`
//!    里没有该值，沿 `ValueDef` 递归重推（深度上限 `CONTEXT_DEPTH_LIMIT`(32) +
//!    visiting 集合防深链 / 环上爆炸），兜底 `base_range`。
//!
//! ### 使用方清单（`raana_ir/src` grep 确认）
//!
//! - `opt/passes/mod_fold.rs`（`ModFold`）：`RangeAnalysis::new` 建分析，对每个
//!   「除数为非 2 幂正整数常量」的 `Rem` 候选调 `range_before(inst, dividend)`，
//!   `foldable` 证明被除数在 `rem` 处满足 `0 <= x < 2P` 后把 `x % P` 折成
//!   `select(x >= P, x - P, x)`（一条 `sub; cmp; csel`）。喂给分析的 `nonneg`
//!   集合来自 `return_summary`（含本函数恒非负入口参数）。管线位置：fixpoint
//!   段 `guard_elimination` 之后、`sr` 之前（pass.rs 287–292 行）。
//! - `opt/passes/pointer_strength_reduction.rs`（`PointerStrengthReduction`）：
//!   `RangeAnalysis::new` 建分析（非负参数集合传空）后把 `&ranges` 传进
//!   `find_candidate`（经 `pointer_strength_reduction/candidate.rs` 到
//!   `analysis.rs`）：`range_before(gep, value)` 求仿射表达式里循环不变量偏移的
//!   区间（`I64Range::from_i32`），`proves_binary_no_signed_wrap(..., RangeContext::
//!   Before(gep))` 证明循环依赖的 add / sub / mul / shl 在 GEP 处不回绕——回绕
//!   或区间越出 i32 就放弃该候选。管线位置：fixpoint 段 `dse` 之后、
//!   `guard_elimination` 之前（pass.rs 279–285 行）。
//! - 间接上游（被消费的依赖，不是使用方）：`opt/analysis_passes/return_summary.rs`
//!   提供 `nonneg_preserving_functions` / `always_nonneg_params` / `MODMUL_BUILTIN`
//!   （`soyo_mulmod`）；`loop_analysis` / `induction_variable` 提供循环与 IV
//!   结构（`add_induction_caps` 消费，见 `induction_variable.rs` 文档的依赖表）。
//! - 管线邻居（不直接调用本模块）：`opt/passes/guard_elimination.rs` 用
//!   `return_summary` 的摘要折叠 modmul 的 `br (x < 0)` 守卫，注册在 `mod_fold`
//!   **之前**（pass.rs 282–285 行）——守卫折完后 range 事实稳定，`mod_fold`
//!   才做区间证明（见 `guard_elimination.rs` 文档 81–82 行与 pass.rs 注册注释）。
//! - `range_of_fresh` / `loop_header_range` 目前只在测试里出现，仓库内无其他
//!   外部调用点。
//!
//! ### 正确性 / 边界
//!
//! - **保守上近似**：证明不了 → `full`（`base_range` 兜底），宁漏勿错；除 / 余
//!   对非常量除数直接 `full`；
//! - **包装语义**：单点运算按 i32 包装折叠（`fold_binary`），非单点只在 i64
//!   端点运算不越界时给区间，越界 → `full`——不会声称「不回绕」而实际回绕；
//! - **零点空洞**：`contains_zero` 是区间形状的一部分，`join` / `intersect` /
//!   `is_subset_of` 都参与比较；`assume_ne(0)` 只清空洞、不拆区间（表达力取舍：
//!   无法表示「`[−4, 8]` 里排除 0 之外某点」这类形状，此类查询退化为两侧并集
//!   的近似）；
//! - **快照**：任何 IR 修改都会使结果过期，使用方改写前必须重建；
//! - **函数内**：不做跨函数区间——调用结果除 mulmod 与非负保持纯函数两个特例
//!   外一律 `full`；`BlockParameter` 的求值回退到函数级 `ranges`；
//! - **不可达边**：`refine_edge` 证明某臂不可达时该边没有状态，`range_on_edge`
//!   返回 `empty`；
//! - **循环**：无法识别的递推靠宽化收敛为 `full`（正确但丢精度），宽度上限
//!   3 次变化（`WIDEN_AFTER`）。
//!
//! ### 验证
//!
//! 本文件内联 `#[cfg(test)] mod tests`（1117 行起，共 10 个用例）：
//! `lattice_and_assumptions`（格运算与 assume 族）、`wrapping_singletons_and_
//! interval_overflow`（单点包装 / 区间越界 → full）、`remainder_of_non_negative_
//! dividend_is_non_negative`（非负被除数余数）、`diamond_refines_branch_condition_
//! and_parameter_join`（分支精化 + 参数 join）、`same_target_arms_keep_distinct_
//! arguments`（同目标双臂实参区分）、`float_comparison_does_not_refine_float_
//! operands_as_integers`（浮点比较不按整数精化）、`joins_block_parameters_from_
//! distinct_predecessors`（多前驱参数 join）、`loop_cap_includes_failing_header_
//! visit`（IV 封顶）、`unrecognized_loop_recurrence_widens_to_terminate`（未知
//! 递推宽化收敛）、`proves_no_signed_wrap_in_context`（无回绕证明）。端到端由
//! 使用方 pass 的测试（`mod_fold.rs` / `pointer_strength_reduction` 的 `mod
//! tests`）与 `cargo test -p raana_ir` 全量回归覆盖。

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    ir::{BasicBlock, BinaryOp, Inst, InstKind, arena::Arena},
    opt::{
        analysis_passes::{
            induction_variable::{
                BasicInductionVariable, BasicInductionVariableAnalysis, InductionDirection,
                InductionStep,
            },
            loop_analysis::{Loop, LoopAnalysis},
            return_summary,
        },
        pass::ArenaContext,
        utils::{
            cfg::CFG,
            logical_edge::{LogicalEdge, LogicalEdgeArm, incoming_edges, outgoing_edges},
        },
    },
};

const WIDEN_AFTER: usize = 3;
const CONTEXT_DEPTH_LIMIT: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntRange {
    Empty,
    Bounded {
        min: i32,
        max: i32,
        contains_zero: bool,
    },
}

impl IntRange {
    pub const fn empty() -> Self {
        Self::Empty
    }

    pub const fn full() -> Self {
        Self::Bounded {
            min: i32::MIN,
            max: i32::MAX,
            contains_zero: true,
        }
    }

    pub const fn constant(value: i32) -> Self {
        Self::Bounded {
            min: value,
            max: value,
            contains_zero: value == 0,
        }
    }

    pub fn bounded(min: i32, max: i32) -> Self {
        Self::normalized(min, max, min <= 0 && max >= 0)
    }

    fn normalized(mut min: i32, mut max: i32, contains_zero: bool) -> Self {
        if min > max {
            return Self::Empty;
        }
        if !contains_zero {
            if min == 0 {
                min = 1;
            }
            if max == 0 {
                max = -1;
            }
            if min > max {
                return Self::Empty;
            }
        }
        Self::Bounded {
            min,
            max,
            contains_zero: contains_zero && min <= 0 && max >= 0,
        }
    }

    pub const fn min(self) -> Option<i32> {
        match self {
            Self::Empty => None,
            Self::Bounded { min, .. } => Some(min),
        }
    }

    pub const fn max(self) -> Option<i32> {
        match self {
            Self::Empty => None,
            Self::Bounded { max, .. } => Some(max),
        }
    }

    pub const fn singleton(self) -> Option<i32> {
        match self {
            Self::Bounded { min, max, .. } if min == max => Some(min),
            _ => None,
        }
    }

    pub const fn contains(self, value: i32) -> bool {
        match self {
            Self::Empty => false,
            Self::Bounded {
                min,
                max,
                contains_zero,
            } => min <= value && value <= max && (value != 0 || contains_zero),
        }
    }

    pub const fn contains_zero(self) -> bool {
        self.contains(0)
    }

    pub const fn excludes_zero(self) -> bool {
        !self.contains_zero()
    }

    pub fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Empty, range) | (range, Self::Empty) => range,
            (
                Self::Bounded {
                    min: lhs_min,
                    max: lhs_max,
                    contains_zero: lhs_zero,
                },
                Self::Bounded {
                    min: rhs_min,
                    max: rhs_max,
                    contains_zero: rhs_zero,
                },
            ) => Self::normalized(
                lhs_min.min(rhs_min),
                lhs_max.max(rhs_max),
                lhs_zero || rhs_zero,
            ),
        }
    }

    pub fn intersect(self, other: Self) -> Self {
        match (self, other) {
            (Self::Empty, _) | (_, Self::Empty) => Self::Empty,
            (
                Self::Bounded {
                    min: lhs_min,
                    max: lhs_max,
                    contains_zero: lhs_zero,
                },
                Self::Bounded {
                    min: rhs_min,
                    max: rhs_max,
                    contains_zero: rhs_zero,
                },
            ) => Self::normalized(
                lhs_min.max(rhs_min),
                lhs_max.min(rhs_max),
                lhs_zero && rhs_zero,
            ),
        }
    }

    pub fn is_subset_of(self, other: Self) -> bool {
        match (self, other) {
            (Self::Empty, _) => true,
            (_, Self::Empty) => false,
            (
                Self::Bounded {
                    min: lhs_min,
                    max: lhs_max,
                    contains_zero: lhs_zero,
                },
                Self::Bounded {
                    min: rhs_min,
                    max: rhs_max,
                    contains_zero: rhs_zero,
                },
            ) => {
                rhs_min <= lhs_min
                    && lhs_max <= rhs_max
                    && (!lhs_zero || rhs_zero)
                    && !(lhs_min == 0 && !lhs_zero && lhs_max == 0)
            }
        }
    }

    pub fn subset(self, other: Self) -> bool {
        self.is_subset_of(other)
    }

    pub fn assume_eq(self, other: Self) -> Self {
        self.intersect(other)
    }

    pub fn assume_ne(self, other: Self) -> Self {
        match other.singleton() {
            Some(0) => match self {
                Self::Bounded { min, max, .. } if min == 0 && max == 0 => Self::Empty,
                Self::Bounded { min, max, .. } => Self::normalized(min, max, false),
                Self::Empty => Self::Empty,
            },
            Some(value) if self.singleton() == Some(value) => Self::Empty,
            _ => self,
        }
    }

    pub fn assume_lt(self, other: Self) -> Self {
        let Some(other_max) = other.max() else {
            return Self::Empty;
        };
        let Some(max) = other_max.checked_sub(1) else {
            return Self::Empty;
        };
        self.intersect(Self::bounded(i32::MIN, max))
    }

    pub fn assume_le(self, other: Self) -> Self {
        let Some(other_max) = other.max() else {
            return Self::Empty;
        };
        self.intersect(Self::bounded(i32::MIN, other_max))
    }

    pub fn assume_gt(self, other: Self) -> Self {
        let Some(other_min) = other.min() else {
            return Self::Empty;
        };
        let Some(min) = other_min.checked_add(1) else {
            return Self::Empty;
        };
        self.intersect(Self::bounded(min, i32::MAX))
    }

    pub fn assume_ge(self, other: Self) -> Self {
        let Some(other_min) = other.min() else {
            return Self::Empty;
        };
        self.intersect(Self::bounded(other_min, i32::MAX))
    }

    pub fn widen(self, next: Self) -> Self {
        match (self, next) {
            (Self::Empty, range) => range,
            (range, Self::Empty) => range,
            (
                Self::Bounded {
                    min: old_min,
                    max: old_max,
                    contains_zero: old_zero,
                },
                Self::Bounded {
                    min: new_min,
                    max: new_max,
                    contains_zero: new_zero,
                },
            ) => Self::normalized(
                if new_min < old_min { i32::MIN } else { old_min },
                if new_max > old_max { i32::MAX } else { old_max },
                old_zero || new_zero,
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RangeContext {
    Function,
    BlockEntry(BasicBlock),
    Edge(LogicalEdge),
    Before(Inst),
}

type State = FxHashMap<Inst, IntRange>;

#[derive(Debug, Clone)]
enum ValueDef {
    Integer(i32),
    ZeroInit,
    Undef,
    Binary(BinaryOp, Inst, Inst),
    Select(Inst, Inst, Inst),
    Cast {
        src: Inst,
        source_float: Option<f32>,
    },
    Call {
        callee: crate::ir::Function,
        args: Vec<Inst>,
    },
    BlockParameter,
    Unknown,
}

pub struct RangeAnalysis {
    definitions: FxHashMap<Inst, ValueDef>,
    ranges: State,
    block_entries: FxHashMap<BasicBlock, State>,
    edge_states: FxHashMap<LogicalEdge, State>,
    before: FxHashMap<Inst, State>,
    loop_caps: FxHashMap<Inst, IntRange>,
    loop_headers: FxHashSet<BasicBlock>,
    /// Pure functions whose result is `>= 0` whenever every argument is
    /// `>= 0` (see `return_summary::nonneg_preserving_functions`).
    nonneg_preserving: FxHashSet<crate::ir::Function>,
    /// The `soyo_mulmod` builtin declaration, if present.
    modmul_builtin: Option<crate::ir::Function>,
    /// Entry-block parameters known `>= 0` at every call site (see
    /// `return_summary::always_nonneg_params`).
    nonneg_params: FxHashSet<Inst>,
}

impl RangeAnalysis {
    pub fn new(
        arena: &ArenaContext<'_>,
        cfg: &CFG,
        loops: &LoopAnalysis,
        induction: &BasicInductionVariableAnalysis,
        nonneg_preserving: &FxHashSet<crate::ir::Function>,
        nonneg_params: &FxHashSet<Inst>,
    ) -> Self {
        let data = arena.curr_func_data();
        let mut definitions = FxHashMap::default();
        for (&inst, inst_data) in arena.inst_datas() {
            let definition = if !inst_data.ty().is_i32() {
                ValueDef::Unknown
            } else {
                match inst_data.kind() {
                    InstKind::Integer(value) => ValueDef::Integer(value.value()),
                    InstKind::ZeroInit => ValueDef::ZeroInit,
                    InstKind::Undef | InstKind::Load(_) => ValueDef::Undef,
                    InstKind::Call(call) => ValueDef::Call {
                        callee: call.callee(),
                        args: call.args().to_vec(),
                    },
                    InstKind::Binary(binary) => {
                        ValueDef::Binary(binary.op(), binary.lhs(), binary.rhs())
                    }
                    InstKind::Select(select) => {
                        ValueDef::Select(select.cond(), select.if_true(), select.if_false())
                    }
                    InstKind::Cast(cast) => ValueDef::Cast {
                        src: cast.src(),
                        source_float: match arena.inst_data(cast.src()).kind() {
                            InstKind::Float(value) => Some(value.value()),
                            _ => None,
                        },
                    },
                    InstKind::BlockArgRef(_) => ValueDef::BlockParameter,
                    _ => ValueDef::Unknown,
                }
            };
            definitions.insert(inst, definition);
        }

        let modmul_builtin = arena
            .program
            .function_layout()
            .iter()
            .copied()
            .find(|&f| arena.program.func_data(f).name() == return_summary::MODMUL_BUILTIN);
        let loop_headers = loops.loops().iter().map(Loop::header).collect();
        let mut analysis = Self {
            definitions,
            ranges: State::default(),
            block_entries: FxHashMap::default(),
            edge_states: FxHashMap::default(),
            before: FxHashMap::default(),
            loop_caps: FxHashMap::default(),
            loop_headers,
            nonneg_preserving: nonneg_preserving.clone(),
            modmul_builtin,
            nonneg_params: nonneg_params.clone(),
        };
        analysis.solve(data, cfg);
        analysis.add_induction_caps(arena, loops, induction);
        if !analysis.loop_caps.is_empty() {
            analysis.ranges.clear();
            analysis.block_entries.clear();
            analysis.edge_states.clear();
            analysis.before.clear();
            analysis.solve(data, cfg);
        }
        analysis
    }

    pub fn range_of(&self, value: Inst) -> IntRange {
        self.range_in_context(value, RangeContext::Function)
    }

    pub fn range_at_block_entry(&self, block: BasicBlock, value: Inst) -> IntRange {
        self.range_in_context(value, RangeContext::BlockEntry(block))
    }

    pub fn range_on_edge(&self, edge: LogicalEdge, value: Inst) -> IntRange {
        self.range_in_context(value, RangeContext::Edge(edge))
    }

    pub fn range_before(&self, instruction: Inst, value: Inst) -> IntRange {
        self.range_in_context(value, RangeContext::Before(instruction))
    }

    pub fn loop_header_range(&self, header: BasicBlock, parameter: Inst) -> IntRange {
        self.range_at_block_entry(header, parameter)
    }

    pub fn range_in_context(&self, value: Inst, context: RangeContext) -> IntRange {
        let facts = match context {
            RangeContext::Function => &self.ranges,
            RangeContext::BlockEntry(block) => match self.block_entries.get(&block) {
                Some(state) => state,
                None => return self.base_range(value),
            },
            RangeContext::Edge(edge) => match self.edge_states.get(&edge) {
                Some(state) => state,
                None => return IntRange::empty(),
            },
            RangeContext::Before(inst) => match self.before.get(&inst) {
                Some(state) => state,
                None => return self.base_range(value),
            },
        };
        self.evaluate_contextual(value, facts, &mut FxHashSet::default(), 0)
    }

    pub fn proves_binary_no_signed_wrap(
        &self,
        op: BinaryOp,
        lhs: Inst,
        rhs: Inst,
        context: RangeContext,
    ) -> bool {
        if !matches!(
            op,
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Shl
        ) {
            return false;
        }
        let lhs = self.range_in_context(lhs, context);
        let rhs = self.range_in_context(rhs, context);
        mathematical_binary(op, lhs, rhs).is_some()
    }

    fn solve(&mut self, data: &crate::ir::FunctionData, cfg: &CFG) {
        let entry = cfg.entry();
        let mut initial = State::default();
        for &parameter in data.bb_data(entry).params() {
            if data.inst_data(parameter).ty().is_i32() {
                if self.nonneg_params.contains(&parameter) {
                    initial.insert(parameter, IntRange::bounded(0, i32::MAX));
                } else {
                    initial.insert(parameter, IntRange::full());
                }
            }
        }
        self.block_entries.insert(entry, initial);

        let mut worklist = std::collections::VecDeque::from([entry]);
        let mut queued = FxHashSet::from_iter([entry]);
        let mut header_changes: FxHashMap<(BasicBlock, Inst), usize> = FxHashMap::default();
        while let Some(block) = worklist.pop_front() {
            queued.remove(&block);
            let Some(mut state) = self.block_entries.get(&block).cloned() else {
                continue;
            };
            for &inst in data.layout().basicblock(block).insts() {
                if data.inst_data(inst).ty().is_i32() {
                    // A value carried around a backedge is an old observation,
                    // not the result of this execution of its defining instruction.
                    state.remove(&inst);
                    self.before.insert(inst, state.clone());
                    let range = self.eval_with_state(inst, &state);
                    state.insert(inst, range);
                    self.ranges
                        .entry(inst)
                        .and_modify(|old| *old = old.join(range))
                        .or_insert(range);
                } else {
                    self.before.insert(inst, state.clone());
                }
            }

            for edge in outgoing_edges(data, block) {
                let mut edge_state = state.clone();
                if !self.refine_edge(data, edge, &mut edge_state) {
                    continue;
                }
                let changed_edge = match self.edge_states.get_mut(&edge) {
                    Some(old) => join_state(old, &edge_state),
                    None => {
                        self.edge_states.insert(edge, edge_state);
                        true
                    }
                };
                if !changed_edge {
                    continue;
                }
                let target = edge.target(data);
                let mut incoming = self.join_incoming(data, cfg, target);
                if self.loop_headers.contains(&target) {
                    for &parameter in data.bb_data(target).params() {
                        let Some(next) = incoming.get(&parameter).copied() else {
                            continue;
                        };
                        let old = self
                            .block_entries
                            .get(&target)
                            .and_then(|state| state.get(&parameter))
                            .copied()
                            .unwrap_or(IntRange::empty());
                        let changes = header_changes.entry((target, parameter)).or_default();
                        let mut value = if *changes >= WIDEN_AFTER {
                            old.widen(next)
                        } else {
                            next
                        };
                        if value != old {
                            *changes += 1;
                        }
                        if let Some(cap) = self.loop_caps.get(&parameter) {
                            value = value.intersect(*cap);
                        }
                        incoming.insert(parameter, value);
                    }
                }
                let changed = match self.block_entries.get_mut(&target) {
                    Some(old) => join_state(old, &incoming),
                    None => {
                        self.block_entries.insert(target, incoming);
                        true
                    }
                };
                if changed && queued.insert(target) {
                    worklist.push_back(target);
                }
            }
        }

        for state in self.block_entries.values().chain(self.edge_states.values()) {
            for (&value, &range) in state {
                self.ranges
                    .entry(value)
                    .and_modify(|old| *old = old.join(range))
                    .or_insert(range);
            }
        }
    }

    /// Compute the incoming range state for `target`'s block parameters.
    ///
    /// Only the parameters are returned.  Non-parameter values reachable from
    /// an edge are recovered lazily by `evaluate_contextual` along the def-use
    /// chain (falling back to the global `self.ranges` join), and each block's
    /// own definitions are re-derived when the block is processed.  Copying
    /// every incoming edge's full state here is O(incoming × state) and
    /// dominates the cost of range analysis on deeply nested loops; the join
    /// loop in `solve` keeps `block_entries[target]` monotonic without it.
    fn join_incoming(
        &self,
        data: &crate::ir::FunctionData,
        cfg: &CFG,
        target: BasicBlock,
    ) -> State {
        let mut result = State::default();
        let params = data.bb_data(target).params();
        for edge in incoming_edges(data, cfg, target) {
            let Some(state) = self.edge_states.get(&edge) else {
                continue;
            };
            for (&parameter, &argument) in params.iter().zip(edge.args(data)) {
                if !data.inst_data(parameter).ty().is_i32() {
                    continue;
                }
                let range = self.evaluate_contextual(argument, state, &mut FxHashSet::default(), 0);
                result
                    .entry(parameter)
                    .and_modify(|old| *old = old.join(range))
                    .or_insert(range);
            }
        }
        result
    }

    fn refine_edge(
        &self,
        data: &crate::ir::FunctionData,
        edge: LogicalEdge,
        state: &mut State,
    ) -> bool {
        if !matches!(edge.arm(), LogicalEdgeArm::True | LogicalEdgeArm::False) {
            return true;
        }
        let InstKind::Branch(branch) = data.inst_data(edge.terminator()).kind() else {
            return true;
        };
        let truth = edge.arm() == LogicalEdgeArm::True;
        let condition = branch.cond();
        let condition_range = self.eval_with_state(condition, state);
        if (truth && condition_range.singleton() == Some(0))
            || (!truth && condition_range.excludes_zero())
        {
            return false;
        }
        let refined_condition = if truth {
            condition_range.assume_ne(IntRange::constant(0))
        } else {
            condition_range.assume_eq(IntRange::constant(0))
        };
        if refined_condition == IntRange::empty() {
            return false;
        }
        state.insert(condition, refined_condition);

        let Some(ValueDef::Binary(mut op, lhs, rhs)) = self.definitions.get(&condition).cloned()
        else {
            return true;
        };
        if !op.is_compare() {
            return true;
        }
        if !data.inst_data(lhs).ty().is_i32() || !data.inst_data(rhs).ty().is_i32() {
            return true;
        }
        if !truth {
            op = op.complement_integer_compare().unwrap();
        }
        let lhs_range = self.eval_with_state(lhs, state);
        let rhs_range = self.eval_with_state(rhs, state);
        let (new_lhs, new_rhs) = refine_comparison(op, lhs_range, rhs_range);
        if new_lhs == IntRange::empty() || new_rhs == IntRange::empty() {
            return false;
        }
        state.insert(lhs, new_lhs);
        state.insert(rhs, new_rhs);
        true
    }

    fn add_induction_caps(
        &mut self,
        arena: &ArenaContext<'_>,
        loops: &LoopAnalysis,
        induction: &BasicInductionVariableAnalysis,
    ) {
        let data = arena.curr_func_data();
        for looop in loops.loops() {
            let terminator = data.layout().basicblock(looop.header()).terminator();
            let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
                continue;
            };
            let true_inside = looop.contains(branch.t_target());
            let false_inside = looop.contains(branch.f_target());
            if true_inside == false_inside {
                continue;
            }
            let InstKind::Binary(compare) = data.inst_data(branch.cond()).kind() else {
                continue;
            };
            for iv in induction.for_loop(looop) {
                let Some(step) = self.constant_step(iv.step()) else {
                    continue;
                };
                if step == 0 {
                    continue;
                }
                let mut op = compare.op();
                if !true_inside {
                    let Some(complement) = op.complement_integer_compare() else {
                        continue;
                    };
                    op = complement;
                }
                let bound_value = if compare.lhs() == iv.parameter() {
                    compare.rhs()
                } else if compare.rhs() == iv.parameter() {
                    let Some(swapped) = op.swap_compare_args() else {
                        continue;
                    };
                    op = swapped;
                    compare.lhs()
                } else {
                    continue;
                };
                let direction = if step > 0 {
                    InductionDirection::Forward
                } else {
                    InductionDirection::Backward
                };
                if !matches!(
                    (direction, op),
                    (InductionDirection::Forward, BinaryOp::Lt)
                        | (InductionDirection::Backward, BinaryOp::Gt)
                ) {
                    continue;
                }
                if step.unsigned_abs() != 1 {
                    let Some(bound) = self.constant_value(bound_value) else {
                        continue;
                    };
                    let no_wrap = match direction {
                        InductionDirection::Forward => {
                            i64::from(bound) + i64::from(step) - 1 <= i64::from(i32::MAX)
                        }
                        InductionDirection::Backward => {
                            i64::from(bound) + i64::from(step) + 1 >= i64::from(i32::MIN)
                        }
                    };
                    if !no_wrap {
                        continue;
                    }
                }
                let Some(cap) = self.constant_induction_cap(iv, direction, step, bound_value)
                else {
                    continue;
                };
                self.loop_caps
                    .entry(iv.parameter())
                    .and_modify(|old| *old = old.join(cap))
                    .or_insert(cap);
            }
        }
    }

    fn constant_induction_cap(
        &self,
        iv: &BasicInductionVariable,
        direction: InductionDirection,
        signed_step: i32,
        bound: Inst,
    ) -> Option<IntRange> {
        let bound = self.constant_value(bound)?;
        let initial_values = iv
            .initial_values()
            .iter()
            .map(|&value| self.constant_value(value))
            .collect::<Option<Vec<_>>>()?;
        let initial_min = initial_values.iter().copied().min()?;
        let initial_max = initial_values.iter().copied().max()?;

        match direction {
            InductionDirection::Forward => {
                let terminal = i64::from(bound)
                    .checked_sub(1)?
                    .checked_add(i64::from(signed_step))?;
                Some(IntRange::bounded(
                    initial_min,
                    initial_max.max(i32::try_from(terminal).ok()?),
                ))
            }
            InductionDirection::Backward => {
                let terminal = i64::from(bound)
                    .checked_add(1)?
                    .checked_add(i64::from(signed_step))?;
                Some(IntRange::bounded(
                    initial_min.min(i32::try_from(terminal).ok()?),
                    initial_max,
                ))
            }
        }
    }

    fn constant_step(&self, step: InductionStep) -> Option<i32> {
        match step {
            InductionStep::Add(value) => self.constant_value(value),
            InductionStep::Sub(value) => self.constant_value(value)?.checked_neg(),
        }
    }

    fn constant_value(&self, value: Inst) -> Option<i32> {
        match self.definitions.get(&value) {
            Some(ValueDef::Integer(value)) => Some(*value),
            Some(ValueDef::ZeroInit) => Some(0),
            _ => None,
        }
    }

    fn base_range(&self, value: Inst) -> IntRange {
        match self.definitions.get(&value) {
            Some(ValueDef::Integer(value)) => IntRange::constant(*value),
            Some(ValueDef::ZeroInit) => IntRange::constant(0),
            Some(ValueDef::Unknown) | Some(ValueDef::Undef) | None => IntRange::full(),
            _ => self.ranges.get(&value).copied().unwrap_or(IntRange::full()),
        }
    }

    fn eval_with_state(&self, value: Inst, state: &State) -> IntRange {
        self.evaluate_contextual(value, state, &mut FxHashSet::default(), 0)
    }

    /// Re-derive `value`'s range from its definition against the settled
    /// `self.ranges`, ignoring the range `solve` recorded for `value` itself.
    ///
    /// `solve` records a value's range when its block is first processed, which
    /// happens before entry-parameter ranges have propagated through
    /// `self.ranges` (they are joined in only at the end). A call of two entry
    /// parameters therefore records `full` and stays stale. Callers that need
    /// the settled answer (return summaries, guard folding) use this.
    pub fn range_of_fresh(&self, value: Inst) -> IntRange {
        self.evaluate_def(value, &self.ranges, &mut FxHashSet::default(), 0)
    }

    fn evaluate_contextual(
        &self,
        value: Inst,
        facts: &State,
        visiting: &mut FxHashSet<Inst>,
        depth: usize,
    ) -> IntRange {
        if let Some(range) = facts.get(&value) {
            return *range;
        }
        if depth >= CONTEXT_DEPTH_LIMIT || !visiting.insert(value) {
            return self.base_range(value);
        }
        let range = self.evaluate_def(value, facts, visiting, depth);
        visiting.remove(&value);
        range
    }

    fn evaluate_def(
        &self,
        value: Inst,
        facts: &State,
        visiting: &mut FxHashSet<Inst>,
        depth: usize,
    ) -> IntRange {
        match self.definitions.get(&value) {
            Some(ValueDef::Integer(value)) => IntRange::constant(*value),
            Some(ValueDef::ZeroInit) => IntRange::constant(0),
            Some(ValueDef::Binary(op, lhs, rhs)) => transfer_binary(
                *op,
                self.evaluate_contextual(*lhs, facts, visiting, depth + 1),
                self.evaluate_contextual(*rhs, facts, visiting, depth + 1),
            ),
            Some(ValueDef::Select(condition, if_true, if_false)) => {
                let condition = self.evaluate_contextual(*condition, facts, visiting, depth + 1);
                if condition.singleton() == Some(0) {
                    self.evaluate_contextual(*if_false, facts, visiting, depth + 1)
                } else if condition.excludes_zero() {
                    self.evaluate_contextual(*if_true, facts, visiting, depth + 1)
                } else {
                    self.evaluate_contextual(*if_true, facts, visiting, depth + 1)
                        .join(self.evaluate_contextual(*if_false, facts, visiting, depth + 1))
                }
            }
            Some(ValueDef::Cast { src, source_float }) => {
                if let Some(value) = source_float.and_then(fold_f32_to_i32) {
                    IntRange::constant(value)
                } else if matches!(self.definitions.get(src), Some(ValueDef::Integer(_))) {
                    self.evaluate_contextual(*src, facts, visiting, depth + 1)
                } else {
                    IntRange::full()
                }
            }
            Some(ValueDef::BlockParameter) => {
                self.ranges.get(&value).copied().unwrap_or(IntRange::full())
            }
            Some(ValueDef::Call { callee, args }) => {
                self.call_range(*callee, args, facts, visiting, depth)
            }
            Some(ValueDef::Undef) | Some(ValueDef::Unknown) | None => IntRange::full(),
        }
    }

    /// Range of a call result. The `soyo_mulmod(a, b, p)` builtin returns
    /// `[0, p)` when both multiplicands are provably non-negative (a remainder
    /// of a non-negative product never turns negative); a non-negativity
    /// preserving pure function returns `[0, i32::MAX]` when every argument is
    /// provably non-negative. Everything else is full range.
    fn call_range(
        &self,
        callee: crate::ir::Function,
        args: &[Inst],
        facts: &State,
        visiting: &mut FxHashSet<Inst>,
        depth: usize,
    ) -> IntRange {
        if self.modmul_builtin == Some(callee) && args.len() == 3 {
            let a = self.evaluate_contextual(args[0], facts, visiting, depth + 1);
            let b = self.evaluate_contextual(args[1], facts, visiting, depth + 1);
            if a.min().is_some_and(|min| min >= 0) && b.min().is_some_and(|min| min >= 0) {
                let p = match self.definitions.get(&args[2]) {
                    Some(ValueDef::Integer(value)) => *value,
                    _ => 0,
                };
                if p > 0 {
                    return IntRange::bounded(0, p - 1);
                }
                return IntRange::bounded(0, i32::MAX);
            }
            return IntRange::full();
        }
        if self.nonneg_preserving.contains(&callee)
            && args.iter().all(|&arg| {
                self.evaluate_contextual(arg, facts, visiting, depth + 1)
                    .min()
                    .is_some_and(|min| min >= 0)
            })
        {
            return IntRange::bounded(0, i32::MAX);
        }
        IntRange::full()
    }
}

fn join_state(target: &mut State, incoming: &State) -> bool {
    let mut changed = false;
    for (&value, &range) in incoming {
        match target.get_mut(&value) {
            Some(old) => {
                let joined = old.join(range);
                if joined != *old {
                    *old = joined;
                    changed = true;
                }
            }
            None => {
                target.insert(value, range);
                changed = true;
            }
        }
    }
    changed
}

fn refine_comparison(op: BinaryOp, lhs: IntRange, rhs: IntRange) -> (IntRange, IntRange) {
    match op {
        BinaryOp::Eq => (lhs.assume_eq(rhs), rhs.assume_eq(lhs)),
        BinaryOp::NotEq => (lhs.assume_ne(rhs), rhs.assume_ne(lhs)),
        BinaryOp::Lt => (lhs.assume_lt(rhs), rhs.assume_gt(lhs)),
        BinaryOp::Le => (lhs.assume_le(rhs), rhs.assume_ge(lhs)),
        BinaryOp::Gt => (lhs.assume_gt(rhs), rhs.assume_lt(lhs)),
        BinaryOp::Ge => (lhs.assume_ge(rhs), rhs.assume_le(lhs)),
        _ => (lhs, rhs),
    }
}

fn transfer_binary(op: BinaryOp, lhs: IntRange, rhs: IntRange) -> IntRange {
    if lhs == IntRange::empty() || rhs == IntRange::empty() {
        return IntRange::empty();
    }
    if let (Some(lhs), Some(rhs)) = (lhs.singleton(), rhs.singleton()) {
        if matches!(op, BinaryOp::Div | BinaryOp::Rem) && rhs == 0 {
            return IntRange::full();
        }
        return IntRange::constant(fold_binary(op, lhs, rhs));
    }
    if op.is_compare() {
        return comparison_range(op, lhs, rhs);
    }
    if let Some(range) = mathematical_binary(op, lhs, rhs) {
        return range;
    }
    match op {
        BinaryOp::And if rhs.singleton() == Some(0) || lhs.singleton() == Some(0) => {
            IntRange::constant(0)
        }
        BinaryOp::And => {
            let mask = lhs.singleton().or_else(|| rhs.singleton());
            match mask {
                Some(mask) if mask >= 0 => IntRange::bounded(0, mask),
                _ => IntRange::full(),
            }
        }
        BinaryOp::Or | BinaryOp::Xor if rhs.singleton() == Some(0) => lhs,
        BinaryOp::Or | BinaryOp::Xor if lhs.singleton() == Some(0) => rhs,
        BinaryOp::Div => transfer_div(lhs, rhs),
        BinaryOp::Rem => transfer_rem(lhs, rhs),
        BinaryOp::Shr => IntRange::bounded(0, i32::MAX),
        BinaryOp::Sar => match rhs.singleton().filter(|shift| (0..32).contains(shift)) {
            Some(shift) => {
                let shift = shift as u32;
                IntRange::bounded(lhs.min().unwrap() >> shift, lhs.max().unwrap() >> shift)
            }
            None => IntRange::full(),
        },
        _ => IntRange::full(),
    }
}

fn mathematical_binary(op: BinaryOp, lhs: IntRange, rhs: IntRange) -> Option<IntRange> {
    let (lhs_min, lhs_max, rhs_min, rhs_max) = (
        i64::from(lhs.min()?),
        i64::from(lhs.max()?),
        i64::from(rhs.min()?),
        i64::from(rhs.max()?),
    );
    let (min, max) = match op {
        BinaryOp::Add => (lhs_min + rhs_min, lhs_max + rhs_max),
        BinaryOp::Sub => (lhs_min - rhs_max, lhs_max - rhs_min),
        BinaryOp::Mul => {
            let values = [
                lhs_min * rhs_min,
                lhs_min * rhs_max,
                lhs_max * rhs_min,
                lhs_max * rhs_max,
            ];
            (*values.iter().min().unwrap(), *values.iter().max().unwrap())
        }
        BinaryOp::Shl if rhs_min == rhs_max && (0..32).contains(&rhs_min) => {
            let factor = 1_i64 << rhs_min;
            let values = [lhs_min * factor, lhs_max * factor];
            (values[0].min(values[1]), values[0].max(values[1]))
        }
        BinaryOp::Min => (lhs_min.min(rhs_min), lhs_max.min(rhs_max)),
        BinaryOp::Max => (lhs_min.max(rhs_min), lhs_max.max(rhs_max)),
        _ => return None,
    };
    if min < i64::from(i32::MIN) || max > i64::from(i32::MAX) {
        return None;
    }
    Some(IntRange::bounded(min as i32, max as i32))
}

fn comparison_range(op: BinaryOp, lhs: IntRange, rhs: IntRange) -> IntRange {
    let (lhs_min, lhs_max, rhs_min, rhs_max) = (
        lhs.min().unwrap(),
        lhs.max().unwrap(),
        rhs.min().unwrap(),
        rhs.max().unwrap(),
    );
    let proven_true = match op {
        BinaryOp::Eq => lhs.singleton().is_some() && lhs.singleton() == rhs.singleton(),
        BinaryOp::NotEq => lhs_max < rhs_min || rhs_max < lhs_min,
        BinaryOp::Lt => lhs_max < rhs_min,
        BinaryOp::Le => lhs_max <= rhs_min,
        BinaryOp::Gt => lhs_min > rhs_max,
        BinaryOp::Ge => lhs_min >= rhs_max,
        _ => false,
    };
    let proven_false = match op {
        BinaryOp::Eq => lhs_max < rhs_min || rhs_max < lhs_min,
        BinaryOp::NotEq => lhs.singleton().is_some() && lhs.singleton() == rhs.singleton(),
        BinaryOp::Lt => lhs_min >= rhs_max,
        BinaryOp::Le => lhs_min > rhs_max,
        BinaryOp::Gt => lhs_max <= rhs_min,
        BinaryOp::Ge => lhs_max < rhs_min,
        _ => false,
    };
    if proven_true {
        IntRange::constant(1)
    } else if proven_false {
        IntRange::constant(0)
    } else {
        IntRange::bounded(0, 1)
    }
}

fn transfer_div(lhs: IntRange, rhs: IntRange) -> IntRange {
    let Some(divisor) = rhs.singleton() else {
        return IntRange::full();
    };
    if divisor == 0 || (divisor == -1 && lhs.contains(i32::MIN)) {
        return IntRange::full();
    }
    let values = [lhs.min().unwrap() / divisor, lhs.max().unwrap() / divisor];
    IntRange::bounded(values[0].min(values[1]), values[0].max(values[1]))
}

fn transfer_rem(lhs: IntRange, rhs: IntRange) -> IntRange {
    let Some(divisor) = rhs.singleton() else {
        return IntRange::full();
    };
    if divisor == 0 {
        return IntRange::full();
    }
    let magnitude = divisor
        .unsigned_abs()
        .saturating_sub(1)
        .min(i32::MAX as u32) as i32;
    if lhs.min().is_some_and(|min| min >= 0) {
        // A truncating remainder of a non-negative dividend is never negative
        // and stays below |divisor|, so the sign can be dropped.
        IntRange::bounded(0, magnitude)
    } else {
        IntRange::bounded(-magnitude, magnitude)
    }
}

fn fold_binary(op: BinaryOp, lhs: i32, rhs: i32) -> i32 {
    match op {
        BinaryOp::Add => lhs.wrapping_add(rhs),
        BinaryOp::Sub => lhs.wrapping_sub(rhs),
        BinaryOp::Mul => lhs.wrapping_mul(rhs),
        BinaryOp::Div => lhs.wrapping_div(rhs),
        BinaryOp::Rem => lhs.wrapping_rem(rhs),
        BinaryOp::NotEq => (lhs != rhs) as i32,
        BinaryOp::Eq => (lhs == rhs) as i32,
        BinaryOp::Gt => (lhs > rhs) as i32,
        BinaryOp::Lt => (lhs < rhs) as i32,
        BinaryOp::Ge => (lhs >= rhs) as i32,
        BinaryOp::Le => (lhs <= rhs) as i32,
        BinaryOp::And => lhs & rhs,
        BinaryOp::Or => lhs | rhs,
        BinaryOp::Xor => lhs ^ rhs,
        BinaryOp::Shl => lhs.wrapping_shl(rhs as u32),
        BinaryOp::Shr => (lhs as u32).wrapping_shr(rhs as u32) as i32,
        BinaryOp::Sar => lhs.wrapping_shr(rhs as u32),
        BinaryOp::Min => lhs.min(rhs),
        BinaryOp::Max => lhs.max(rhs),
        BinaryOp::MatMul => unreachable!("tensor type should not reach here."),
    }
}

fn fold_f32_to_i32(value: f32) -> Option<i32> {
    (value.is_finite() && value >= i32::MIN as f32 && value < i32::MAX as f32)
        .then_some(value as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{Program, Type, builder_trait::*},
        opt::analysis_passes::{
            induction_variable::BasicInductionVariableAnalysis, loop_analysis::LoopAnalysis,
        },
    };

    fn analyze(program: &Program, function: crate::ir::Function) -> RangeAnalysis {
        let data = program.func_data(function);
        let (cfg, _, loops) = LoopAnalysis::new(data);
        let ivs = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        let arena = ArenaContext {
            program,
            curr_func: Some(function),
        };
        RangeAnalysis::new(
            &arena,
            &cfg,
            &loops,
            &ivs,
            &FxHashSet::default(),
            &FxHashSet::default(),
        )
    }

    #[test]
    fn lattice_and_assumptions() {
        let a = IntRange::bounded(-4, 8);
        let b = IntRange::bounded(2, 12);
        assert_eq!(a.join(b), b.join(a));
        assert_eq!(a.intersect(b), b.intersect(a));
        assert!(a.intersect(b).is_subset_of(a));
        assert_eq!(a.assume_eq(b), IntRange::bounded(2, 8));
        assert!(a.assume_ne(IntRange::constant(0)).excludes_zero());
        assert_eq!(a.assume_lt(IntRange::constant(3)).max(), Some(2));
        assert_eq!(a.assume_gt(IntRange::constant(3)).min(), Some(4));
        assert_eq!(
            IntRange::bounded(0, 1)
                .assume_ne(IntRange::constant(0))
                .singleton(),
            Some(1)
        );
    }

    #[test]
    fn wrapping_singletons_and_interval_overflow() {
        assert_eq!(
            transfer_binary(
                BinaryOp::Add,
                IntRange::constant(i32::MAX),
                IntRange::constant(1),
            ),
            IntRange::constant(i32::MIN)
        );
        assert_eq!(
            transfer_binary(
                BinaryOp::Add,
                IntRange::bounded(0, i32::MAX),
                IntRange::constant(1)
            ),
            IntRange::full()
        );
        assert_eq!(
            transfer_binary(
                BinaryOp::Mul,
                IntRange::bounded(-2, 3),
                IntRange::constant(4)
            ),
            IntRange::bounded(-8, 12)
        );
    }

    #[test]
    fn remainder_of_non_negative_dividend_is_non_negative() {
        // `x % 7` with x ∈ [0, 100) is in [0, 6], not [-6, 6].
        assert_eq!(
            transfer_binary(
                BinaryOp::Rem,
                IntRange::bounded(0, 100),
                IntRange::constant(7),
            ),
            IntRange::bounded(0, 6)
        );
        // A possibly-negative dividend keeps the symmetric range.
        assert_eq!(
            transfer_binary(
                BinaryOp::Rem,
                IntRange::bounded(-5, 100),
                IntRange::constant(7),
            ),
            IntRange::bounded(-6, 6)
        );
        // `i32::MIN % -1` wraps to 0; the magnitude cap must not overflow.
        assert_eq!(
            transfer_binary(
                BinaryOp::Rem,
                IntRange::bounded(0, i32::MAX),
                IntRange::constant(i32::MIN),
            ),
            IntRange::bounded(0, i32::MAX)
        );
    }

    #[test]
    fn diamond_refines_branch_condition_and_parameter_join() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "diamond".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        for block in [left, right, merge] {
            data.layout_mut().push_bb_back(block);
        }
        let x = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let compare = data.new_local_inst().binary(BinaryOp::Gt, x, zero);
        let branch = data
            .new_local_inst()
            .branch(compare, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(entry, compare);
        data.layout_mut().insert_inst(entry, branch);
        let one = data.new_local_inst().integer(1);
        let left_jump = data.new_local_inst().jump(merge, vec![one]);
        data.layout_mut().insert_inst(left, left_jump);
        let minus_one = data.new_local_inst().integer(-1);
        let right_jump = data.new_local_inst().jump(merge, vec![minus_one]);
        data.layout_mut().insert_inst(right, right_jump);
        let parameter = data.bb_data(merge).params()[0];
        let ret = data.new_local_inst().ret(Some(parameter));
        data.layout_mut().insert_inst(merge, ret);

        let analysis = analyze(&program, function);
        let data = program.func_data(function);
        let true_edge = outgoing_edges(data, entry)[0];
        let false_edge = outgoing_edges(data, entry)[1];
        assert_eq!(analysis.range_on_edge(true_edge, x).min(), Some(1));
        assert_eq!(analysis.range_on_edge(false_edge, x).max(), Some(0));
        assert_eq!(
            analysis.range_at_block_entry(merge, parameter),
            IntRange::bounded(-1, 1).assume_ne(IntRange::constant(0))
        );
    }

    #[test]
    fn same_target_arms_keep_distinct_arguments() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "arms".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        data.layout_mut().push_bb_back(merge);
        let condition = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let seven = data.new_local_inst().integer(7);
        let branch = data
            .new_local_inst()
            .branch(condition, merge, vec![zero], merge, vec![seven]);
        data.layout_mut().insert_inst(entry, branch);
        let parameter = data.bb_data(merge).params()[0];
        let ret = data.new_local_inst().ret(Some(parameter));
        data.layout_mut().insert_inst(merge, ret);
        let analysis = analyze(&program, function);
        let edges = outgoing_edges(program.func_data(function), entry);
        assert_eq!(
            analysis.range_on_edge(edges[0], condition).excludes_zero(),
            true
        );
        assert_eq!(
            analysis.range_on_edge(edges[1], condition),
            IntRange::constant(0)
        );
        assert_eq!(
            analysis.range_at_block_entry(merge, parameter),
            IntRange::bounded(0, 7)
        );
    }

    #[test]
    fn float_comparison_does_not_refine_float_operands_as_integers() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "float_branch".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let on_true = data.new_basic_block().basic_block("true".into(), vec![]);
        let on_false = data.new_basic_block().basic_block("false".into(), vec![]);
        data.layout_mut().push_bb_back(on_true);
        data.layout_mut().push_bb_back(on_false);

        let nan = data.new_local_inst().float(f32::NAN);
        let zero = data.new_local_inst().float(0.0);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, nan, zero);
        let branch = data
            .new_local_inst()
            .branch(compare, on_true, vec![], on_false, vec![]);
        data.layout_mut().insert_inst(entry, compare);
        data.layout_mut().insert_inst(entry, branch);
        let one = data.new_local_inst().integer(1);
        let true_ret = data.new_local_inst().ret(Some(one));
        data.layout_mut().insert_inst(on_true, true_ret);
        let zero_int = data.new_local_inst().integer(0);
        let false_ret = data.new_local_inst().ret(Some(zero_int));
        data.layout_mut().insert_inst(on_false, false_ret);

        let analysis = analyze(&program, function);
        let edges = outgoing_edges(program.func_data(function), entry);
        assert_eq!(analysis.range_on_edge(edges[0], nan), IntRange::full());
        assert_eq!(analysis.range_on_edge(edges[1], nan), IntRange::full());
    }

    #[test]
    fn joins_block_parameters_from_distinct_predecessors() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "join".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        for block in [left, right, merge] {
            data.layout_mut().push_bb_back(block);
        }
        let condition = data.params()[0];
        let branch = data
            .new_local_inst()
            .branch(condition, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(entry, branch);
        let three = data.new_local_inst().integer(3);
        let left_jump = data.new_local_inst().jump(merge, vec![three]);
        data.layout_mut().insert_inst(left, left_jump);
        let nine = data.new_local_inst().integer(9);
        let right_jump = data.new_local_inst().jump(merge, vec![nine]);
        data.layout_mut().insert_inst(right, right_jump);
        let parameter = data.bb_data(merge).params()[0];
        let ret = data.new_local_inst().ret(Some(parameter));
        data.layout_mut().insert_inst(merge, ret);

        let analysis = analyze(&program, function);
        assert_eq!(
            analysis.range_at_block_entry(merge, parameter),
            IntRange::bounded(3, 9)
        );
    }

    #[test]
    fn loop_cap_includes_failing_header_visit() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "loop".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }
        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let ten = data.new_local_inst().integer(10);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, ten);
        let branch = data
            .new_local_inst()
            .branch(compare, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, compare);
        data.layout_mut().insert_inst(header, branch);
        let one = data.new_local_inst().integer(1);
        let next = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let back = data.new_local_inst().jump(header, vec![next]);
        data.layout_mut().insert_inst(body, next);
        data.layout_mut().insert_inst(body, back);
        let ret = data.new_local_inst().ret(Some(iv));
        data.layout_mut().insert_inst(exit, ret);
        let analysis = analyze(&program, function);
        assert_eq!(
            analysis.loop_header_range(header, iv),
            IntRange::bounded(0, 10)
        );
    }

    #[test]
    fn unrecognized_loop_recurrence_widens_to_terminate() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "widen".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        data.layout_mut().push_bb_back(header);
        let one = data.new_local_inst().integer(1);
        let start = data.new_local_inst().jump(header, vec![one]);
        data.layout_mut().insert_inst(entry, start);
        let value = data.bb_data(header).params()[0];
        let two = data.new_local_inst().integer(2);
        let next = data.new_local_inst().binary(BinaryOp::Mul, value, two);
        let back = data.new_local_inst().jump(header, vec![next]);
        data.layout_mut().insert_inst(header, next);
        data.layout_mut().insert_inst(header, back);
        let analysis = analyze(&program, function);
        assert_eq!(
            analysis.loop_header_range(header, value).max(),
            Some(i32::MAX)
        );
    }

    #[test]
    fn proves_no_signed_wrap_in_context() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "proof".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let ten = data.new_local_inst().integer(10);
        let twenty = data.new_local_inst().integer(20);
        let max = data.new_local_inst().integer(i32::MAX);
        let add = data.new_local_inst().binary(BinaryOp::Add, ten, twenty);
        let ret = data.new_local_inst().ret(Some(add));
        data.layout_mut().insert_inst(entry, add);
        data.layout_mut().insert_inst(entry, ret);
        let analysis = analyze(&program, function);
        assert!(analysis.proves_binary_no_signed_wrap(
            BinaryOp::Add,
            ten,
            twenty,
            RangeContext::Before(add)
        ));
        assert!(!analysis.proves_binary_no_signed_wrap(
            BinaryOp::Add,
            max,
            twenty,
            RangeContext::Before(add)
        ));
    }
}
