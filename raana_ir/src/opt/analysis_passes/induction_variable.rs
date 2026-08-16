//! # 基本归纳变量分析与循环 trip count（`induction_variable`）
//!
//! 本模块是循环优化的**基础设施分析**：给定循环结构（`LoopAnalysis`）与 CFG，
//! 识别循环中每轮按固定步长变化的**基本归纳变量**（basic induction variable,
//! BIV），把 header 处的分支 exit 规范化为 `iv < bound` / `iv > bound` 的**严格
//! 形式**，并在此之上推导**迭代次数（trip count）**、**归纳变量的常量取值区间**
//! 与**派生线性表达式**。它不修改 IR，只产出供其他 pass 消费的分析结果，是
//! `loop_unroll`、`pointer_strength_reduction`、`column_major` 等循环优化共用的
//! 底座。
//!
//! 典型消费链：`BasicInductionVariableAnalysis::new` 识别 IV →
//! `normalize_strict_exit` 确认循环以"严格比较 + 常量步长"退出 →
//! `constant_trip_count` / `induction_trip_count` 求迭代次数（全展开、循环互换、
//! 访问重排的前提）→ `constant_induction_range` / `classify_derived_induction_variable`
//! 求 IV 及其线性组合的取值区间（强度削减、列主序变换的前提）。
//!
//! ## 术语速查
//!
//! - **基本归纳变量（BIV）**：`i32` 值、每轮按同一常量（或循环不变量）步长变化、
//!   初值来自循环外的变量。本例中它**一定是循环 header 的一个 block 参数**
//!   （Phi）：从外部入口边取初值、从回边（latch）取更新值；
//! - **严格 exit**：header 终结分支的条件形如 `iv < bound`（递增）或
//!   `iv > bound`（递减），且边界不含等号。`<=` / `>=` 只有在能取补集化为严格
//!   形式时才接受（见 `normalize_strict_exit`）；
//! - **trip count**：循环总执行轮数。已知初值、边界、步长与方向时，
//!   轮数 = `ceil(|bound - initial| / |step|)`（见 `constant_trip_count`）；
//! - **规范化（normalize）**：把方向、比较符号、IV 在比较式左右的位置统一成
//!   一种标准形态，下游 pass 只需处理一种模式；
//! - **派生（线性）归纳变量**：IV 的仿射组合 `coefficient * iv + offset`，如
//!   数组下标 `i * 4 + base`（见 `classify_derived_induction_variable`）。
//!
//! ## 核心 API 详解
//!
//! ### 分析入口：`BasicInductionVariableAnalysis`
//!
//! 整个分析的一等公民：一次构造、按 header 索引的全部 BIV 集合。
//!
//! ```rust,ignore
//! pub fn new(data: &FunctionData, cfg: &CFG, loops: &LoopAnalysis) -> Self
//! ```
//!
//! **语义**：快照分析。遍历 `loops.loops()`，对每个循环收集 header 的所有 block
//! 参数，逐一调用内部 `analyze_parameter` 判定是否为 BIV，结果存入
//! `by_header: FxHashMap<BasicBlock, Vec<BasicInductionVariable>>`（以 header 块为
//! 键）。**注意**：这是纯只读快照，任何 IR 修改都会使其失效，pass 必须先做完
//! 分析再改 IR（与 CFG / 支配 / 循环分析同约定）。构造输入恰好是
//! `LoopAnalysis::new(data)` 的三元组，三者配套使用。
//!
//! ```rust,ignore
//! pub fn for_loop(&self, looop: &Loop) -> &[BasicInductionVariable]
//! ```
//!
//! **语义**：返回 `looop` 的 header 上识别出的全部 BIV（无则空切片）。**使用方**：
//! `column_major`（枚举每个循环的 IV 做访问 delta 与执行次数分析）、`range`
//! 分析（`add_induction_caps` 给循环携带值加封顶）。
//!
//! ```rust,ignore
//! pub fn find(&self, looop: &Loop, parameter: Inst) -> Option<&BasicInductionVariable>
//! ```
//!
//! **语义**：按 header 参数精确查找某个 IV，是 `for_loop` 的线性查找版。
//! **使用方**：`recursive_memoize`（确认 memo 键参数是前向步进的 IV）、
//! `loop_unroll` / `reduction_unroll` / `pointer_strength_reduction`（锁定
//! "那一个"循环变量）、本模块测试。
//!
//! ### 基本归纳变量：`BasicInductionVariable`
//!
//! ```rust,ignore
//! pub fn parameter(&self) -> Inst              // header 里的 block 参数（Phi）
//! pub fn initial_values(&self) -> &[Inst]      // 外部入口边传入的初值（≥1 个）
//! pub fn update_values(&self) -> &[Inst]       // 回边传入的更新值（≥1 个）
//! pub fn step(&self) -> InductionStep          // 归一化后的更新形态 Add/Sub
//! ```
//!
//! **语义**：一个 BIV 由四件事刻画——它是哪个 header 参数（`parameter`）、从哪些
//! 初值起步（`initial_values`）、每轮由哪些更新值推进（`update_values`）、更新是
//! 加还是减（`step`）。`initial_values` 与 `update_values` 都是 `SmallVec`：自然
//! 循环里各 1 个，但多入口 / 多 latch 时可以有多个。**使用方**：一切消费 IV 的
//! pass（下方各 API 均以它为第一参数）；`range` 分析读 `step()` 判断方向。
//!
//! ### 更新形态与方向：`InductionStep` / `InductionDirection`
//!
//! ```rust,ignore
//! pub enum InductionStep { Add(Inst), Sub(Inst) }   // value() 取步长指令
//! pub enum InductionDirection { Forward, Backward }
//! ```
//!
//! **语义**：`InductionStep` 是 BIV 更新的**归一化形态**——识别时把
//! `Add(param, step)`、`Add(step, param)`（交换律）统一为 `Add`，把
//! `Sub(param, step)` 记为 `Sub`，步长指令本身在 `value()`。`InductionDirection`
//! 描述 IV 随迭代增大（`Forward`）还是减小（`Backward`），由步长常量符号决定。
//! **使用方**：`recursive_memoize` 只接受 `Add` 步进；`column_major` 按方向计算
//! 访问 delta；`normalize_strict_exit` 产出方向，trip count / range 推导消费之。
//!
//! ### 严格 exit 规范化：`normalize_strict_exit` / `NormalizedInductionExit`
//!
//! ```rust,ignore
//! pub fn normalize_strict_exit(
//!     data: &ArenaContextMut<'_>, looop: &Loop, iv: &BasicInductionVariable,
//! ) -> Option<NormalizedInductionExit>
//! ```
//!
//! **语义**：检查 `looop` 是否以"严格比较 + 常量步长"退出，是就产出规范形态
//! `NormalizedInductionExit { direction, signed_step: i32, bound: Inst }`（访问器
//! `direction()` / `signed_step()` / `bound()`）。具体把 header 终结分支重写成两种
//! 标准模式之一：
//!
//! - `Forward`（`signed_step > 0`）：留在循环内的条件是 `iv < bound`；
//! - `Backward`（`signed_step < 0`）：留在循环内的条件是 `iv > bound`。
//!
//! IV 在比较式左右任一侧均可，分支"真臂在循环内 / 假臂在循环内"两种情况通过
//! 取补集（`complement_integer_compare`）与交换操作数（`swap_compare_args`）统一。
//! **非单位步长**（`|signed_step| != 1`）额外要求 bound 是常量且能证明最后一次
//! 更新不会让 `i32` 回绕（前向要求 `bound + step - 1 <= i32::MAX`，反向要求
//! `bound + step + 1 >= i32::MIN`）——步长不是 ±1 时最后一次"继续迭代的更新"可能
//! 越过 `i32` 边界，必须显式排除。失败（`None`）情形见"正确性与边界情况"。
//! **使用方**：几乎所有循环 pass——`loop_unroll`、`reduction_unroll`、
//! `pointer_strength_reduction`、`column_major`、`matmul_interchange` 都在拿它
//! 确认"这个循环我们能数清楚"。
//!
//! ### trip count 推导：`constant_trip_count` / `induction_trip_count` /
//! ### `ConstantTripCount` / `TripCountEstimate`
//!
//! ```rust,ignore
//! pub fn constant_trip_count(
//!     data: &ArenaContextMut<'_>, iv: &BasicInductionVariable, exit: NormalizedInductionExit,
//! ) -> Option<ConstantTripCount>
//! pub fn induction_trip_count(
//!     data: &ArenaContextMut<'_>, iv: &BasicInductionVariable, exit: NormalizedInductionExit,
//! ) -> Option<TripCountEstimate>
//! ```
//!
//! **语义**：两者都要求初值与 bound 是**整数常量**，从
//! `ceil(|bound - initial| / |step|)` 求迭代次数（`distance.div_ceil(step)`，全程
//! `i64` checked 算术；初值已越过边界时为 0；方向与步长符号矛盾时为 `None`）。
//! 区别在初值个数：
//!
//! - `constant_trip_count` 要求**恰好一个**初值（`let [initial] = ...`），产出
//!   `ConstantTripCount { iterations: usize, initial: i32, bound: i32,
//!   signed_step: i32 }`（访问器同名）——`iterations()` 是精确值；
//! - `induction_trip_count` 允许**多个**初值（多入口），逐初值求轮数后取最大：
//!   全部相同 → `TripCountEstimate::Exact(n)`，否则保守地给
//!   `TripCountEstimate::UpperBound(n)`。`exact()` 只在 `Exact` 时返回
//!   `Some(n)`，`upper_bound()` 恒返回轮数上限——消费方想证明"恰好 N 轮"必须
//!   `exact()` 成功。
//!
//! **使用方**：`loop_unroll` 用 `constant_trip_count` 的 `iterations()` 决定能否
//! 精确全展开；`column_major` 用 `induction_trip_count` 求循环嵌套执行次数
//! （`loop_nest_executions`，逐层乘 trip count 得访问总执行量，只把 `UpperBound`
//! 当上限用）。
//!
//! ### 常量归纳范围：`constant_induction_range` / `ConstantInductionRange`
//!
//! ```rust,ignore
//! pub fn constant_induction_range(
//!     data: &ArenaContextMut<'_>, iv: &BasicInductionVariable, exit: NormalizedInductionExit,
//! ) -> Option<ConstantInductionRange>
//! ```
//!
//! **语义**：IV 在循环存活期内取值的**闭区间** `ConstantInductionRange { min: i32,
//! max: i32 }`（访问器 `min()` / `max()`）。要求 bound 与全部初值都是整数常量；
//! 区间 = 所有初值的 min/max，再与**末轮终值**取并：前向时末轮最大值为
//! `bound - 1 + step`（最后一次更新后的值，未越界是 `normalize_strict_exit`
//! 保证的），反向时末轮最小值为 `bound + 1 + step`。**使用方**：
//! `pointer_strength_reduction`（证明指针每轮偏移在范围内、可安全转为下标算术）、
//! `column_major`（把访问下标归一到 `[min, max]` 上计算 delta，见
//! `classify_access_delta` / `index_coefficient`）。
//!
//! ### 派生（线性）归纳变量：`classify_derived_induction_variable` /
//! ### `DerivedInductionVariable`
//!
//! ```rust,ignore
//! pub fn classify_derived_induction_variable(
//!     data: &ArenaContextMut<'_>, looop: &Loop, base: Inst, value: Inst,
//!     range: ConstantInductionRange,
//! ) -> Option<DerivedInductionVariable>
//! ```
//!
//! **语义**：判定 `value` 是否为 `base`（通常是 IV 的 `parameter()`）的**仿射
//! 组合**，即 `value == coefficient * base + offset`。成功产出
//! `DerivedInductionVariable { value, base, coefficient: i64, offset: i64, chain }`：
//! `coefficient()` / `offset()` 是仿射系数，`chain` 是参与计算的指令链。递归
//! 分类规则：`value == base` → `(1, 0)`；整数常量 → `(0, c)`；`Add` / `Sub` →
//! 系数与偏移逐项加减；`Mul` 其中一侧为常量 → 整体缩放；`Shl` 常量移位
//! （0..32）→ 乘 `2^shift`。每次组合后做 `range_fits` 检查：系数 × 区间端点 +
//! 偏移必须仍落在 `i32` 内（保证在循环任何一轮都不溢出）；要求 `value` 定义在
//! 循环内、`i32` 类型、最终 `coefficient != 0`。辅助方法：
//!
//! - `evaluate(base_value: i32) -> Option<i32>`：代入 `base_value` 求具体值
//!   （checked 算术，溢出返回 `None`）；
//! - `removable_chain_cost(&self, data: &FunctionData, consumer: Inst) -> usize`：
//!   若 `chain` 中每条指令都只被 `consumer` 或链内指令使用，返回链长（把派生
//!   计算内联进消费点后可删除的指令数），否则 0——供调用方评估改写是否划算。
//!
//! **使用方**：`column_major` 经 `classify_derived_induction_variable` 把数组访问
//! 的地址表达式写成 `coefficient * iv + offset` 形式（`index_coefficient`），
//! 再据此算跨迭代的访问 delta 以决定列主序改写。
//!
//! ## 算法：五步从循环头到 trip count
//!
//! 1. **收集候选**：对每个循环取 header 的全部 block 参数，只保留 `i32` 类型者；
//!    经 `incoming_edges`（结构边 → 逻辑边）收集 header 的所有入边，按参数位置
//!    对齐每条边传入的值（`header_incoming`）。
//! 2. **区分初值与更新**：入边按源块是否在循环内分类——源块在循环外的边是
//!    入口边，其值进入 `initial_values`；源块在循环内的边是回边（latch），其值
//!    进入 `update_values`（`analyze_parameter`）。
//! 3. **匹配更新形态**：对每个更新值调 `match_update`，必须是
//!    `Add(param, step)`、`Add(step, param)` 或 `Sub(param, step)`；`step` 还得
//!    非零（字面量 0 拒绝）且循环不变量（`is_loop_invariant_step`：全局值 / 常量
//!    / 定义在循环外的指令）。所有回边的步长必须一致（`same_step`，常量按值
//!    比较，允许全局常量）。初值 ≥ 1 且更新 ≥ 1 才成 IV——纯直通（passthrough）
//!    不算。
//! 4. **规范化严格 exit**：`normalize_strict_exit` 从步长常量得 `signed_step` 与
//!    方向；要求 header 终结是指向"一臂在循环内、一臂在循环外"的分支、条件是
//!    `i32` 二元比较且一边是 IV；经补集 / 交换统一为 `iv < bound`（前向）或
//!    `iv > bound`（反向）；非单位步长补 no-wrap 证明。
//! 5. **推导 trip count / 范围**：`constant_trip_count` /
//!    `induction_trip_count` 按 `ceil(distance / |step|)` 求轮数（多初值取
//!    max、同则 Exact）；`constant_induction_range` 用初值与末轮终值并出闭区间；
//!    `classify_derived_induction_variable` 在区间上做仿射分类并验证不溢出。
//!
//! ## 使用方清单（grep 全仓库确认）
//!
//! | 使用方 | 使用的 API | 用途 |
//! |--------|-----------|------|
//! | `passes/loop_unroll.rs` | `new` + `normalize_strict_exit` + `constant_trip_count` | 求精确迭代次数，可精确计数才做全展开 |
//! | `passes/reduction_unroll.rs` | `new` + `normalize_strict_exit` | 要求恰好 1 个 BIV、步长 +1、严格前向 exit，才做规约展开 |
//! | `passes/pointer_strength_reduction.rs`（含 `candidate.rs`） | `new` + `normalize_strict_exit` + `constant_induction_range` | 指针→下标强度削减：证明无回绕并取得常量范围 |
//! | `passes/column_major.rs` | `new` + `for_loop` + `normalize_strict_exit` + `constant_induction_range` + `induction_trip_count` + `classify_derived_induction_variable` | 访问 delta 分析、循环嵌套执行次数、列主序改写（最大用户） |
//! | `passes/matmul_interchange.rs` | `new` + `normalize_strict_exit` | 检查 k / j 循环 exit 形态，判定循环互换合法性 |
//! | `analysis_passes/range.rs`（`RangeAnalysis`） | `for_loop` + `step()` | `add_induction_caps` 给循环携带值加取值封顶 |
//! | `passes/recursive_memoize.rs` | `new` + `find` + `InductionStep` | 证明 memo 键参数是前向 `Add` 步进的 IV 才安全 |
//! | `passes/mod_fold.rs` | `new`（转交 `RangeAnalysis`） | `%` 折叠所需的范围证据 |
//!
//! 注：`rotate_loops` 与 `blocked_reduction` 经 grep 确认**不**消费本分析（其测试
//! 里"induction variable"只是泛指循环变量）。
//!
//! ## 正确性与边界情况
//!
//! - **非严格 exit**：`Le` / `Ge` 且"留在循环内的臂"对应等号（无法补成严格
//!   比较）、或分支两臂同在/同不在循环内、或终结不是分支——一律 `None`。
//!   理由：`iv <= bound` 的精确轮数依赖步长整除关系，保守起见本模块只认严格
//!   形式（`Le`/`Ge` 在"假臂在循环内"时可经补集化为严格形式，这是允许的）；
//! - **非常量步长**：识别阶段只收 `Add`/`Sub` 且步长循环不变量；
//!   `normalize_strict_exit` 进一步要求步长是整数常量（否则 `signed_step` 无从
//!   谈起）；符号不定的步长、方向与步长符号矛盾（如前向 exit 配负步长）→ `None`；
//! - **多 latch / 多入口**：多回边允许，但所有更新步长必须一致（
//!   `rejects_inconsistent_same_target_backedge_arms` 覆盖不一致场景）；多入口
//!   产生多个初值——`constant_trip_count` 要求恰好一个，`induction_trip_count`
//!   取最大并降级为 `UpperBound`；
//! - **回绕（wrap）**：全部距离 / 步长运算走 `i64` checked 算术；
//!   非单位步长的 exit 必须满足 no-wrap 不等式（`bound + step ∓ 1` 仍在 `i32`
//!   内）；派生 IV 的仿射组合经 `range_fits` 验证不溢出——任何一步溢出都返回
//!   `None`，宁可保守；
//! - **形态拒绝**：零步长 / 直通更新、浮点或非 `i32` 循环参数、更新是其他二元
//!   运算（`Mul`、`Shl` 等直接作用于参数）都不算 BIV——派生组合只发生在
//!   `classify_derived_induction_variable` 里；
//! - **快照语义**：分析结果与 CFG / 循环分析同为快照，IR 一旦修改必须整体重建
//!   （见 `docs/Convention.md`）。
//!
//! ## 验证
//!
//! 本模块 `#[cfg(test)] mod tests` 含 16 个单元测试（`cargo test -p raana_ir`）：
//!
//! - trip count：`computes_exact_constant_trip_counts`（含 `i32` 极值边界与方向/
//!   步长矛盾）、`computes_exact_forward_backward_non_unit_and_zero_trip_counts`、
//!   `estimates_multiple_initial_values_conservatively`（多初值 → `UpperBound`）；
//! - exit 规范化：`normalizes_forward_and_backward_strict_unit_exits`、
//!   `normalizes_non_unit_steps_with_a_constant_no_wrap_bound`（含 `i32::MAX-1`
//!   边界）、`rejects_non_unit_steps_without_a_no_wrap_proof`、
//!   `rejects_non_strict_mismatched_and_non_unit_exits`、
//!   `normalizes_an_exit_with_a_global_unit_step`（全局常量步长）；
//! - IV 识别：`recognizes_add_sub_and_commuted_add`（含交换律）、
//!   `accepts_symbolic_outer_value_as_inner_loop_step`、
//!   `preserves_multiple_entry_and_same_target_backedge_arms`、
//!   `accepts_consistent_multiple_latches`、
//!   `rejects_passthrough_zero_variant_and_non_affine_updates`、
//!   `rejects_inconsistent_same_target_backedge_arms`、
//!   `rejects_float_recurrence`、`rejects_entry_header_without_an_outside_initial_value`
//!   （缺外部初值 → 非 IV）。
//!
//! 集成层面：`loop_unroll`、`pointer_strength_reduction`、`column_major`、
//! `reduction_unroll` 的 pass 测试都构造真实循环再消费本分析（如 PSR 的
//! `tests.rs` 直接调 `normalize_strict_exit` + `constant_induction_range` 验证
//! 改写后的范围正确性）。
//!
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use crate::opt::{
    analysis_passes::loop_analysis::{Loop, LoopAnalysis},
    prelude::*,
    utils::{cfg::CFG, logical_edge::incoming_edges},
};

/// The normalized update of a basic induction variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InductionStep {
    Add(Inst),
    Sub(Inst),
}

impl InductionStep {
    pub fn value(self) -> Inst {
        match self {
            Self::Add(value) | Self::Sub(value) => value,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BasicInductionVariable {
    parameter: Inst,
    initial_values: SmallVec<[Inst; 2]>,
    update_values: SmallVec<[Inst; 2]>,
    step: InductionStep,
}

impl BasicInductionVariable {
    pub fn parameter(&self) -> Inst {
        self.parameter
    }

    pub fn initial_values(&self) -> &[Inst] {
        &self.initial_values
    }

    pub fn update_values(&self) -> &[Inst] {
        &self.update_values
    }

    pub fn step(&self) -> InductionStep {
        self.step
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InductionDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NormalizedInductionExit {
    direction: InductionDirection,
    signed_step: i32,
    bound: Inst,
}

impl NormalizedInductionExit {
    pub fn direction(self) -> InductionDirection {
        self.direction
    }

    pub fn signed_step(self) -> i32 {
        self.signed_step
    }

    pub fn bound(self) -> Inst {
        self.bound
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TripCountEstimate {
    Exact(u64),
    UpperBound(u64),
}

impl TripCountEstimate {
    pub fn exact(self) -> Option<u64> {
        match self {
            Self::Exact(iterations) => Some(iterations),
            Self::UpperBound(_) => None,
        }
    }

    pub fn upper_bound(self) -> u64 {
        match self {
            Self::Exact(iterations) | Self::UpperBound(iterations) => iterations,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConstantInductionRange {
    min: i32,
    max: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConstantTripCount {
    iterations: usize,
    initial: i32,
    bound: i32,
    signed_step: i32,
}

impl ConstantTripCount {
    pub fn iterations(self) -> usize {
        self.iterations
    }

    pub fn initial(self) -> i32 {
        self.initial
    }

    pub fn bound(self) -> i32 {
        self.bound
    }

    pub fn signed_step(self) -> i32 {
        self.signed_step
    }
}

impl ConstantInductionRange {
    pub fn min(self) -> i32 {
        self.min
    }

    pub fn max(self) -> i32 {
        self.max
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedInductionVariable {
    value: Inst,
    base: Inst,
    coefficient: i64,
    offset: i64,
    chain: SmallVec<[Inst; 4]>,
}

impl DerivedInductionVariable {
    pub fn value(&self) -> Inst {
        self.value
    }

    pub fn base(&self) -> Inst {
        self.base
    }

    pub fn coefficient(&self) -> i64 {
        self.coefficient
    }

    pub fn offset(&self) -> i64 {
        self.offset
    }

    pub fn evaluate(&self, base_value: i32) -> Option<i32> {
        i64::from(base_value)
            .checked_mul(self.coefficient)?
            .checked_add(self.offset)?
            .try_into()
            .ok()
    }

    pub fn removable_chain_cost(&self, data: &FunctionData, consumer: Inst) -> usize {
        let chain = self.chain.iter().copied().collect::<FxHashSet<_>>();
        if self.chain.iter().all(|&inst| {
            data.inst_data(inst)
                .used_by()
                .iter()
                .all(|user| *user == consumer || chain.contains(user))
        }) {
            self.chain.len()
        } else {
            0
        }
    }
}

pub struct BasicInductionVariableAnalysis {
    by_header: FxHashMap<BasicBlock, Vec<BasicInductionVariable>>,
}

struct HeaderIncoming {
    source: BasicBlock,
    args: Vec<Inst>,
}

impl BasicInductionVariableAnalysis {
    pub fn new(data: &FunctionData, cfg: &CFG, loops: &LoopAnalysis) -> Self {
        let mut by_header = FxHashMap::default();
        for looop in loops.loops() {
            let header = looop.header();
            let header_params = data.bb_data(header).params();
            let incoming = header_incoming(data, cfg, looop, header_params.len());
            let loop_params = looop
                .body()
                .iter()
                .flat_map(|&block| data.bb_data(block).params().iter().copied())
                .collect::<FxHashSet<_>>();

            let variables = header_params
                .iter()
                .copied()
                .enumerate()
                .filter_map(|(position, parameter)| {
                    analyze_parameter(data, looop, &loop_params, &incoming, position, parameter)
                })
                .collect();
            by_header.insert(header, variables);
        }
        Self { by_header }
    }

    pub fn for_loop(&self, looop: &Loop) -> &[BasicInductionVariable] {
        self.by_header
            .get(&looop.header())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn find(&self, looop: &Loop, parameter: Inst) -> Option<&BasicInductionVariable> {
        self.for_loop(looop)
            .iter()
            .find(|variable| variable.parameter == parameter)
    }
}

/// Normalize a constant-step header exit to `iv < bound` or `iv > bound` while
/// the selected branch arm remains inside the loop. Non-unit steps require a
/// constant bound that proves the continuing update cannot wrap `i32`.
pub fn normalize_strict_exit(
    data: &ArenaContextMut<'_>,
    looop: &Loop,
    iv: &BasicInductionVariable,
) -> Option<NormalizedInductionExit> {
    let signed_step = match iv.step() {
        InductionStep::Add(step) => integer_constant(data, step)?,
        InductionStep::Sub(step) => integer_constant(data, step)?.checked_neg()?,
    };
    let direction = match signed_step.cmp(&0) {
        std::cmp::Ordering::Greater => InductionDirection::Forward,
        std::cmp::Ordering::Less => InductionDirection::Backward,
        std::cmp::Ordering::Equal => return None,
    };

    let terminator = data.layout().basicblock(looop.header()).terminator();
    let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
        return None;
    };
    let true_inside = looop.contains(branch.t_target());
    let false_inside = looop.contains(branch.f_target());
    if true_inside == false_inside {
        return None;
    }

    let InstKind::Binary(compare) = data.inst_data(branch.cond()).kind() else {
        return None;
    };
    if !data.inst_data(compare.lhs()).ty().is_i32() || !data.inst_data(compare.rhs()).ty().is_i32()
    {
        return None;
    }

    let mut op = compare.op();
    if !true_inside {
        op = op.complement_integer_compare()?;
    }
    let bound = if compare.lhs() == iv.parameter() {
        compare.rhs()
    } else if compare.rhs() == iv.parameter() {
        op = op.swap_compare_args()?;
        compare.lhs()
    } else {
        return None;
    };

    match (direction, op) {
        (InductionDirection::Forward, BinaryOp::Lt)
        | (InductionDirection::Backward, BinaryOp::Gt) => {}
        _ => return None,
    };

    if signed_step.unsigned_abs() != 1 {
        let bound = i64::from(integer_constant(data, bound)?);
        let signed_step = i64::from(signed_step);
        let no_wrap = match direction {
            InductionDirection::Forward => bound + signed_step - 1 <= i64::from(i32::MAX),
            InductionDirection::Backward => bound + signed_step + 1 >= i64::from(i32::MIN),
        };
        if !no_wrap {
            return None;
        }
    }

    Some(NormalizedInductionExit {
        direction,
        signed_step,
        bound,
    })
}

pub fn constant_induction_range(
    data: &ArenaContextMut<'_>,
    iv: &BasicInductionVariable,
    exit: NormalizedInductionExit,
) -> Option<ConstantInductionRange> {
    let bound = integer_constant(data, exit.bound())?;
    let initial_values = iv
        .initial_values()
        .iter()
        .map(|&value| integer_constant(data, value))
        .collect::<Option<Vec<_>>>()?;
    let initial_min = initial_values.iter().copied().min()?;
    let initial_max = initial_values.iter().copied().max()?;

    match exit.direction() {
        InductionDirection::Forward => {
            let terminal = i64::from(bound)
                .checked_sub(1)?
                .checked_add(i64::from(exit.signed_step()))?;
            Some(ConstantInductionRange {
                min: initial_min,
                max: initial_max.max(i32::try_from(terminal).ok()?),
            })
        }
        InductionDirection::Backward => {
            let terminal = i64::from(bound)
                .checked_add(1)?
                .checked_add(i64::from(exit.signed_step()))?;
            Some(ConstantInductionRange {
                min: initial_min.min(i32::try_from(terminal).ok()?),
                max: initial_max,
            })
        }
    }
}

/// Compute a constant trip count for a normalized strict induction exit.
/// Multiple entry values produce an upper bound unless they all have the same
/// trip count.
pub fn induction_trip_count(
    data: &ArenaContextMut<'_>,
    iv: &BasicInductionVariable,
    exit: NormalizedInductionExit,
) -> Option<TripCountEstimate> {
    let bound = integer_constant(data, exit.bound())?;
    let mut counts = iv.initial_values().iter().map(|&initial| {
        estimated_trip_count_values(
            integer_constant(data, initial)?,
            bound,
            exit.signed_step(),
            exit.direction(),
        )
    });
    let first = counts.next()??;
    let mut upper_bound = first;
    let mut exact = true;
    for count in counts {
        let count = count?;
        exact &= count == first;
        upper_bound = upper_bound.max(count);
    }
    Some(if exact {
        TripCountEstimate::Exact(first)
    } else {
        TripCountEstimate::UpperBound(upper_bound)
    })
}

fn estimated_trip_count_values(
    initial: i32,
    bound: i32,
    signed_step: i32,
    direction: InductionDirection,
) -> Option<u64> {
    let (distance, step) = match direction {
        InductionDirection::Forward => {
            if signed_step <= 0 {
                return None;
            }
            if initial >= bound {
                return Some(0);
            }
            (
                i64::from(bound).checked_sub(i64::from(initial))?,
                i64::from(signed_step),
            )
        }
        InductionDirection::Backward => {
            if signed_step >= 0 {
                return None;
            }
            if initial <= bound {
                return Some(0);
            }
            (
                i64::from(initial).checked_sub(i64::from(bound))?,
                i64::from(signed_step).checked_neg()?,
            )
        }
    };
    let distance = u64::try_from(distance).ok()?;
    let step = u64::try_from(step).ok()?;
    Some(distance.div_ceil(step))
}

pub fn constant_trip_count(
    data: &ArenaContextMut<'_>,
    iv: &BasicInductionVariable,
    exit: NormalizedInductionExit,
) -> Option<ConstantTripCount> {
    let [initial] = iv.initial_values() else {
        return None;
    };
    let initial = integer_constant(data, *initial)?;
    let bound = integer_constant(data, exit.bound())?;
    let iterations =
        constant_trip_count_values(initial, bound, exit.signed_step(), exit.direction())?;
    Some(ConstantTripCount {
        iterations,
        initial,
        bound,
        signed_step: exit.signed_step(),
    })
}

fn constant_trip_count_values(
    initial: i32,
    bound: i32,
    signed_step: i32,
    direction: InductionDirection,
) -> Option<usize> {
    let (distance, step) = match direction {
        InductionDirection::Forward => {
            if signed_step <= 0 {
                return None;
            }
            if initial >= bound {
                return Some(0);
            }
            (
                i64::from(bound).checked_sub(i64::from(initial))?,
                i64::from(signed_step),
            )
        }
        InductionDirection::Backward => {
            if signed_step >= 0 {
                return None;
            }
            if initial <= bound {
                return Some(0);
            }
            (
                i64::from(initial).checked_sub(i64::from(bound))?,
                i64::from(signed_step).checked_neg()?,
            )
        }
    };
    let rounded = distance.checked_add(step.checked_sub(1)?)?;
    usize::try_from(rounded.checked_div(step)?).ok()
}

pub fn classify_derived_induction_variable(
    data: &ArenaContextMut<'_>,
    looop: &Loop,
    base: Inst,
    value: Inst,
    range: ConstantInductionRange,
) -> Option<DerivedInductionVariable> {
    fn range_fits(coefficient: i64, offset: i64, range: ConstantInductionRange) -> bool {
        let at_min = coefficient
            .checked_mul(i64::from(range.min()))
            .and_then(|value| value.checked_add(offset));
        let at_max = coefficient
            .checked_mul(i64::from(range.max()))
            .and_then(|value| value.checked_add(offset));
        let (Some(at_min), Some(at_max)) = (at_min, at_max) else {
            return false;
        };
        let min = at_min.min(at_max);
        let max = at_min.max(at_max);
        min >= i64::from(i32::MIN) && max <= i64::from(i32::MAX)
    }

    fn classify(
        data: &ArenaContextMut<'_>,
        looop: &Loop,
        base: Inst,
        value: Inst,
        range: ConstantInductionRange,
    ) -> Option<(i64, i64, SmallVec<[Inst; 4]>)> {
        if value == base {
            return Some((1, 0, SmallVec::new()));
        }
        if let Some(constant) = integer_constant(data, value) {
            return Some((0, i64::from(constant), SmallVec::new()));
        }
        if data
            .layout()
            .parent_bb(value)
            .is_none_or(|block| !looop.contains(block))
            || !data.inst_data(value).ty().is_i32()
        {
            return None;
        }
        let InstKind::Binary(binary) = data.inst_data(value).kind() else {
            return None;
        };
        let (lhs_coefficient, lhs_offset, mut lhs_chain) =
            classify(data, looop, base, binary.lhs(), range)?;
        let (rhs_coefficient, rhs_offset, rhs_chain) =
            classify(data, looop, base, binary.rhs(), range)?;
        let (coefficient, offset) = match binary.op() {
            BinaryOp::Add => (
                lhs_coefficient.checked_add(rhs_coefficient)?,
                lhs_offset.checked_add(rhs_offset)?,
            ),
            BinaryOp::Sub => (
                lhs_coefficient.checked_sub(rhs_coefficient)?,
                lhs_offset.checked_sub(rhs_offset)?,
            ),
            BinaryOp::Mul if lhs_coefficient == 0 => (
                rhs_coefficient.checked_mul(lhs_offset)?,
                rhs_offset.checked_mul(lhs_offset)?,
            ),
            BinaryOp::Mul if rhs_coefficient == 0 => (
                lhs_coefficient.checked_mul(rhs_offset)?,
                lhs_offset.checked_mul(rhs_offset)?,
            ),
            BinaryOp::Shl if rhs_coefficient == 0 && (0..32).contains(&rhs_offset) => {
                let factor = 1_i64.checked_shl(u32::try_from(rhs_offset).ok()?)?;
                (
                    lhs_coefficient.checked_mul(factor)?,
                    lhs_offset.checked_mul(factor)?,
                )
            }
            _ => return None,
        };
        if !range_fits(coefficient, offset, range) {
            return None;
        }
        for inst in rhs_chain {
            if !lhs_chain.contains(&inst) {
                lhs_chain.push(inst);
            }
        }
        if !lhs_chain.contains(&value) {
            lhs_chain.push(value);
        }
        Some((coefficient, offset, lhs_chain))
    }

    let (coefficient, offset, chain) = classify(data, looop, base, value, range)?;
    if coefficient == 0
        || chain.is_empty()
        || i32::try_from(coefficient).is_err()
        || i32::try_from(offset).is_err()
    {
        return None;
    }
    Some(DerivedInductionVariable {
        value,
        base,
        coefficient,
        offset,
        chain,
    })
}

fn header_incoming(
    data: &FunctionData,
    cfg: &CFG,
    looop: &Loop,
    parameter_count: usize,
) -> Vec<HeaderIncoming> {
    let header = looop.header();
    let mut incoming = Vec::new();
    let mut push = |source: BasicBlock, args: &[Inst]| {
        assert_eq!(
            args.len(),
            parameter_count,
            "header edge arguments must match header parameters"
        );
        incoming.push(HeaderIncoming {
            source,
            args: args.to_vec(),
        });
    };

    for edge in incoming_edges(data, cfg, header) {
        push(edge.source(), edge.args(data));
    }
    incoming
}

fn analyze_parameter(
    data: &FunctionData,
    looop: &Loop,
    loop_params: &FxHashSet<Inst>,
    incoming: &[HeaderIncoming],
    position: usize,
    parameter: Inst,
) -> Option<BasicInductionVariable> {
    if !data.inst_data(parameter).ty().is_i32() {
        return None;
    }

    let mut initial_values = SmallVec::new();
    let mut update_values = SmallVec::new();
    let mut common_step = None;

    for edge in incoming {
        let value = edge.args[position];
        if !looop.contains(edge.source) {
            initial_values.push(value);
            continue;
        }

        let step = match_update(data, parameter, value)?;
        if is_literal_zero(data, step.value())
            || !is_loop_invariant_step(data, looop, loop_params, step.value())
        {
            return None;
        }
        if let Some(existing) = common_step {
            if !same_step(data, existing, step) {
                return None;
            }
        } else {
            common_step = Some(step);
        }
        update_values.push(value);
    }

    if initial_values.is_empty() || update_values.is_empty() {
        return None;
    }
    Some(BasicInductionVariable {
        parameter,
        initial_values,
        update_values,
        step: common_step?,
    })
}

fn match_update(data: &FunctionData, parameter: Inst, update: Inst) -> Option<InductionStep> {
    let InstKind::Binary(binary) = data.inst_data(update).kind() else {
        return None;
    };
    match binary.op() {
        BinaryOp::Add if binary.lhs() == parameter => Some(InductionStep::Add(binary.rhs())),
        BinaryOp::Add if binary.rhs() == parameter => Some(InductionStep::Add(binary.lhs())),
        BinaryOp::Sub if binary.lhs() == parameter => Some(InductionStep::Sub(binary.rhs())),
        _ => None,
    }
}

fn is_loop_invariant_step(
    data: &FunctionData,
    looop: &Loop,
    loop_params: &FxHashSet<Inst>,
    step: Inst,
) -> bool {
    if step.is_global() {
        return true;
    }
    if data.inst_data(step).kind().is_const() {
        return true;
    }
    if loop_params.contains(&step) {
        return false;
    }
    match data.layout().parent_bb(step) {
        Some(block) => !looop.contains(block),
        None => matches!(data.inst_data(step).kind(), InstKind::BlockArgRef(..)),
    }
}

fn is_literal_zero(data: &FunctionData, value: Inst) -> bool {
    !value.is_global()
        && matches!(data.inst_data(value).kind(), InstKind::Integer(integer) if integer.value() == 0)
}

fn integer_constant(data: &ArenaContextMut<'_>, value: Inst) -> Option<i32> {
    match data.inst_data(value).kind() {
        InstKind::Integer(integer) => Some(integer.value()),
        _ => None,
    }
}

fn same_step(data: &FunctionData, lhs: InductionStep, rhs: InductionStep) -> bool {
    match (lhs, rhs) {
        (InductionStep::Add(lhs), InductionStep::Add(rhs))
        | (InductionStep::Sub(lhs), InductionStep::Sub(rhs)) => same_step_value(data, lhs, rhs),
        _ => false,
    }
}

fn same_step_value(data: &FunctionData, lhs: Inst, rhs: Inst) -> bool {
    if lhs == rhs {
        return true;
    }
    if lhs.is_global() || rhs.is_global() {
        return false;
    }
    matches!(
        (data.inst_data(lhs).kind(), data.inst_data(rhs).kind()),
        (InstKind::Integer(lhs), InstKind::Integer(rhs)) if lhs.value() == rhs.value()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        Program, Type,
        builder_trait::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
    };

    fn only_loop(loops: &LoopAnalysis) -> &Loop {
        assert_eq!(loops.loops().len(), 1);
        &loops.loops()[0]
    }

    #[test]
    fn computes_exact_constant_trip_counts() {
        for (initial, bound, step, direction, expected) in [
            (0, 0, 1, InductionDirection::Forward, Some(0)),
            (0, 1, 1, InductionDirection::Forward, Some(1)),
            (0, 7, 2, InductionDirection::Forward, Some(4)),
            (5, 0, -1, InductionDirection::Backward, Some(5)),
            (7, 0, -2, InductionDirection::Backward, Some(4)),
            (
                i32::MIN,
                i32::MIN + 1,
                1,
                InductionDirection::Forward,
                Some(1),
            ),
            (
                i32::MAX,
                i32::MAX - 1,
                -1,
                InductionDirection::Backward,
                Some(1),
            ),
            (0, 4, -1, InductionDirection::Forward, None),
            (4, 0, 1, InductionDirection::Backward, None),
        ] {
            assert_eq!(
                constant_trip_count_values(initial, bound, step, direction),
                expected
            );
        }
    }

    fn trip_count(
        initial_values: &[i32],
        bound: i32,
        signed_step: i32,
        direction: InductionDirection,
    ) -> Option<TripCountEstimate> {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "trip_count".into(), vec![]);
        let data = program.func_data_mut(function);
        let initial_values = initial_values
            .iter()
            .map(|&value| data.new_local_inst().integer(value))
            .collect();
        let bound = data.new_local_inst().integer(bound);
        let step = data.new_local_inst().integer(signed_step);
        let iv = BasicInductionVariable {
            parameter: bound,
            initial_values,
            update_values: SmallVec::new(),
            step: InductionStep::Add(step),
        };
        let exit = NormalizedInductionExit {
            direction,
            signed_step,
            bound,
        };
        let context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        induction_trip_count(&context, &iv, exit)
    }

    #[test]
    fn computes_exact_forward_backward_non_unit_and_zero_trip_counts() {
        assert_eq!(
            trip_count(&[0], 7, 2, InductionDirection::Forward),
            Some(TripCountEstimate::Exact(4))
        );
        assert_eq!(
            trip_count(&[7], 0, -2, InductionDirection::Backward),
            Some(TripCountEstimate::Exact(4))
        );
        assert_eq!(
            trip_count(&[7], 7, 1, InductionDirection::Forward),
            Some(TripCountEstimate::Exact(0))
        );
    }

    #[test]
    fn estimates_multiple_initial_values_conservatively() {
        let estimate = trip_count(&[0, 3, 8], 8, 2, InductionDirection::Forward).unwrap();
        assert_eq!(estimate, TripCountEstimate::UpperBound(4));
        assert_eq!(estimate.exact(), None);
        assert_eq!(estimate.upper_bound(), 4);

        let exact = trip_count(&[0, -1], 7, 2, InductionDirection::Forward).unwrap();
        assert_eq!(exact, TripCountEstimate::Exact(4));
        assert_eq!(exact.exact(), Some(4));
        assert_eq!(exact.upper_bound(), 4);
    }

    fn assert_single_iv(
        program: &Program,
        function: Function,
        parameter: Inst,
    ) -> BasicInductionVariable {
        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let analysis = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        let variables = analysis.for_loop(only_loop(&loops));
        assert_eq!(variables.len(), 1);
        assert_eq!(variables[0].parameter(), parameter);
        variables[0].clone()
    }

    fn normalized_step(
        update_op: BinaryOp,
        step_value: i32,
        compare_op: BinaryOp,
        iv_on_left: bool,
        continue_on_true: bool,
        constant_bound: Option<i32>,
    ) -> Option<(InductionDirection, i32)> {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "normalized_exit".into(),
            if constant_bound.is_some() {
                vec![]
            } else {
                vec![Type::get_i32()]
            },
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let bound = match constant_bound {
            Some(value) => data.new_local_inst().integer(value),
            None => data.params()[0],
        };
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, latch, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let step = data.new_local_inst().integer(step_value);
        let update = data.new_local_inst().binary(update_op, iv, step);
        data.layout_mut().insert_inst(header, update);
        let (lhs, rhs) = if iv_on_left { (iv, bound) } else { (bound, iv) };
        let compare = data.new_local_inst().binary(compare_op, lhs, rhs);
        data.layout_mut().insert_inst(header, compare);
        let (true_target, false_target) = if continue_on_true {
            (latch, exit)
        } else {
            (exit, latch)
        };
        let branch =
            data.new_local_inst()
                .branch(compare, true_target, vec![], false_target, vec![]);
        data.layout_mut().insert_inst(header, branch);
        let backedge = data.new_local_inst().jump(header, vec![update]);
        data.layout_mut().insert_inst(latch, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let looop = only_loop(&loops);
        let analysis = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        let iv = analysis.find(looop, iv)?;
        let context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        normalize_strict_exit(&context, looop, iv)
            .map(|exit| (exit.direction(), exit.signed_step()))
    }

    #[test]
    fn normalizes_forward_and_backward_strict_unit_exits() {
        assert_eq!(
            normalized_step(BinaryOp::Add, 1, BinaryOp::Lt, true, true, None),
            Some((InductionDirection::Forward, 1))
        );
        assert_eq!(
            normalized_step(BinaryOp::Sub, -1, BinaryOp::Ge, true, false, None),
            Some((InductionDirection::Forward, 1))
        );
        assert_eq!(
            normalized_step(BinaryOp::Sub, 1, BinaryOp::Gt, true, true, None),
            Some((InductionDirection::Backward, -1))
        );
        assert_eq!(
            normalized_step(BinaryOp::Add, -1, BinaryOp::Le, true, false, None),
            Some((InductionDirection::Backward, -1))
        );
    }

    #[test]
    fn normalizes_non_unit_steps_with_a_constant_no_wrap_bound() {
        assert_eq!(
            normalized_step(BinaryOp::Add, 2, BinaryOp::Lt, true, true, Some(100)),
            Some((InductionDirection::Forward, 2))
        );
        assert_eq!(
            normalized_step(BinaryOp::Sub, 2, BinaryOp::Gt, true, true, Some(-100)),
            Some((InductionDirection::Backward, -2))
        );
        assert_eq!(
            normalized_step(BinaryOp::Add, -3, BinaryOp::Gt, true, true, Some(-100)),
            Some((InductionDirection::Backward, -3))
        );
        assert_eq!(
            normalized_step(BinaryOp::Sub, -3, BinaryOp::Lt, true, true, Some(100)),
            Some((InductionDirection::Forward, 3))
        );
        assert_eq!(
            normalized_step(BinaryOp::Add, 2, BinaryOp::Gt, false, true, Some(100)),
            Some((InductionDirection::Forward, 2))
        );
        assert_eq!(
            normalized_step(BinaryOp::Add, 2, BinaryOp::Ge, true, false, Some(100)),
            Some((InductionDirection::Forward, 2))
        );
        assert_eq!(
            normalized_step(BinaryOp::Add, 2, BinaryOp::Le, false, false, Some(100)),
            Some((InductionDirection::Forward, 2))
        );
        assert_eq!(
            normalized_step(
                BinaryOp::Add,
                2,
                BinaryOp::Lt,
                true,
                true,
                Some(i32::MAX - 1),
            ),
            Some((InductionDirection::Forward, 2))
        );
        assert_eq!(
            normalized_step(
                BinaryOp::Sub,
                2,
                BinaryOp::Gt,
                true,
                true,
                Some(i32::MIN + 1),
            ),
            Some((InductionDirection::Backward, -2))
        );
    }

    #[test]
    fn rejects_non_unit_steps_without_a_no_wrap_proof() {
        assert_eq!(
            normalized_step(BinaryOp::Add, 2, BinaryOp::Lt, true, true, None),
            None
        );
        assert_eq!(
            normalized_step(BinaryOp::Add, 2, BinaryOp::Lt, true, true, Some(i32::MAX),),
            None
        );
        assert_eq!(
            normalized_step(BinaryOp::Sub, 2, BinaryOp::Gt, true, true, Some(i32::MIN),),
            None
        );
        assert_eq!(
            normalized_step(BinaryOp::Sub, i32::MIN, BinaryOp::Lt, true, true, Some(0),),
            None
        );
    }

    #[test]
    fn rejects_non_strict_mismatched_and_non_unit_exits() {
        assert_eq!(
            normalized_step(BinaryOp::Add, 1, BinaryOp::Le, true, true, None),
            None
        );
        assert_eq!(
            normalized_step(BinaryOp::Add, 1, BinaryOp::Gt, true, true, None),
            None
        );
    }

    #[test]
    fn normalizes_an_exit_with_a_global_unit_step() {
        let mut program = Program::new();
        let step = program.new_value().integer(1);
        let function = program.new_function(
            Type::get_unit(),
            "global_step_exit".into(),
            vec![Type::get_i32()],
        );
        let (header, iv) = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(function),
            };
            let entry = data.add_entry_block();
            let bound = data.params()[0];
            let header = data
                .new_basic_block()
                .basic_block("header".into(), vec![Type::get_i32()]);
            let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
            let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
            for block in [header, latch, exit] {
                data.layout_mut().push_bb_back(block);
            }

            let zero = data.new_local_value().integer(0);
            let entry_jump = data.new_local_value().jump(header, vec![zero]);
            data.layout_mut().insert_inst(entry, entry_jump);
            let iv = data.bb_data(header).params()[0];
            let update = data.new_local_value().binary(BinaryOp::Add, iv, step);
            let compare = data.new_local_value().binary(BinaryOp::Lt, iv, bound);
            for inst in [update, compare] {
                data.layout_mut().insert_inst(header, inst);
            }
            let branch = data
                .new_local_value()
                .branch(compare, latch, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, branch);
            let backedge = data.new_local_value().jump(header, vec![update]);
            data.layout_mut().insert_inst(latch, backedge);
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(exit, ret);
            (header, iv)
        };

        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let looop = only_loop(&loops);
        assert_eq!(looop.header(), header);
        let analysis = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        let iv = analysis.find(looop, iv).unwrap();
        let context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        assert_eq!(
            normalize_strict_exit(&context, looop, iv)
                .map(|exit| (exit.direction(), exit.signed_step())),
            Some((InductionDirection::Forward, 1))
        );
    }

    #[test]
    fn recognizes_add_sub_and_commuted_add() {
        for (op, parameter_on_left, subtract) in [
            (BinaryOp::Add, true, false),
            (BinaryOp::Add, false, false),
            (BinaryOp::Sub, true, true),
        ] {
            let mut program = Program::new();
            let function = program.new_function(Type::get_unit(), "iv".into(), vec![]);
            let data = program.func_data_mut(function);
            let entry = data.add_entry_block();
            let header = data
                .new_basic_block()
                .basic_block("header".into(), vec![Type::get_i32()]);
            let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
            let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
            for block in [header, latch, exit] {
                data.layout_mut().push_bb_back(block);
            }

            let zero = data.new_local_inst().integer(0);
            let entry_jump = data.new_local_inst().jump(header, vec![zero]);
            data.layout_mut().insert_inst(entry, entry_jump);
            let parameter = data.bb_data(header).params()[0];
            let one = data.new_local_inst().integer(1);
            let (lhs, rhs) = if parameter_on_left {
                (parameter, one)
            } else {
                (one, parameter)
            };
            let update = data.new_local_inst().binary(op, lhs, rhs);
            data.layout_mut().insert_inst(header, update);
            let condition = data.new_local_inst().integer(1);
            let branch = data
                .new_local_inst()
                .branch(condition, latch, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, branch);
            let backedge = data.new_local_inst().jump(header, vec![update]);
            data.layout_mut().insert_inst(latch, backedge);
            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(exit, ret);

            let variable = assert_single_iv(&program, function, parameter);
            assert_eq!(variable.initial_values(), &[zero]);
            assert_eq!(variable.update_values(), &[update]);
            assert_eq!(
                variable.step(),
                if subtract {
                    InductionStep::Sub(one)
                } else {
                    InductionStep::Add(one)
                }
            );
        }
    }

    #[test]
    fn accepts_symbolic_outer_value_as_inner_loop_step() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "nested_iv".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let outer_header = data
            .new_basic_block()
            .basic_block("outer_header".into(), vec![Type::get_i32()]);
        let inner_header = data
            .new_basic_block()
            .basic_block("inner_header".into(), vec![Type::get_i32()]);
        let inner_latch = data
            .new_basic_block()
            .basic_block("inner_latch".into(), vec![]);
        let outer_latch = data
            .new_basic_block()
            .basic_block("outer_latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [outer_header, inner_header, inner_latch, outer_latch, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(outer_header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let outer = data.bb_data(outer_header).params()[0];
        let enter_inner = data.new_local_inst().jump(inner_header, vec![zero]);
        data.layout_mut().insert_inst(outer_header, enter_inner);
        let inner = data.bb_data(inner_header).params()[0];
        let inner_update = data.new_local_inst().binary(BinaryOp::Add, inner, outer);
        data.layout_mut().insert_inst(inner_header, inner_update);
        let condition = data.new_local_inst().integer(1);
        let inner_branch =
            data.new_local_inst()
                .branch(condition, inner_latch, vec![], outer_latch, vec![]);
        data.layout_mut().insert_inst(inner_header, inner_branch);
        let inner_backedge = data.new_local_inst().jump(inner_header, vec![inner_update]);
        data.layout_mut().insert_inst(inner_latch, inner_backedge);
        let one = data.new_local_inst().integer(1);
        let outer_update = data.new_local_inst().binary(BinaryOp::Add, outer, one);
        data.layout_mut().insert_inst(outer_latch, outer_update);
        let outer_branch =
            data.new_local_inst()
                .branch(condition, outer_header, vec![outer_update], exit, vec![]);
        data.layout_mut().insert_inst(outer_latch, outer_branch);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        assert_eq!(loops.loops().len(), 2);
        let analysis = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        let inner_loop = loops
            .loops()
            .iter()
            .find(|looop| looop.header() == inner_header)
            .unwrap();
        let variable = analysis.find(inner_loop, inner).unwrap();
        assert_eq!(variable.step(), InductionStep::Add(outer));
    }

    #[test]
    fn preserves_multiple_entry_and_same_target_backedge_arms() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "parallel_iv".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        for block in [header, latch] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let five = data.new_local_inst().integer(5);
        let condition = data.new_local_inst().integer(1);
        let entry_branch =
            data.new_local_inst()
                .branch(condition, header, vec![zero], header, vec![five]);
        data.layout_mut().insert_inst(entry, entry_branch);
        let parameter = data.bb_data(header).params()[0];
        let one_a = data.new_local_inst().integer(1);
        let one_b = data.new_local_inst().integer(1);
        let update_a = data
            .new_local_inst()
            .binary(BinaryOp::Add, parameter, one_a);
        let update_b = data
            .new_local_inst()
            .binary(BinaryOp::Add, parameter, one_b);
        for update in [update_a, update_b] {
            data.layout_mut().insert_inst(header, update);
        }
        let to_latch = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, to_latch);
        let backedge =
            data.new_local_inst()
                .branch(condition, header, vec![update_a], header, vec![update_b]);
        data.layout_mut().insert_inst(latch, backedge);

        let variable = assert_single_iv(&program, function, parameter);
        assert_eq!(
            variable
                .initial_values()
                .iter()
                .copied()
                .collect::<FxHashSet<_>>(),
            FxHashSet::from_iter([zero, five])
        );
        assert_eq!(
            variable
                .update_values()
                .iter()
                .copied()
                .collect::<FxHashSet<_>>(),
            FxHashSet::from_iter([update_a, update_b])
        );
    }

    #[test]
    fn accepts_consistent_multiple_latches() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "multi_latch_iv".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let left_latch = data
            .new_basic_block()
            .basic_block("left_latch".into(), vec![]);
        let right_latch = data
            .new_basic_block()
            .basic_block("right_latch".into(), vec![]);
        for block in [header, left_latch, right_latch] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let parameter = data.bb_data(header).params()[0];
        let condition = data.new_local_inst().integer(1);
        let choose_latch =
            data.new_local_inst()
                .branch(condition, left_latch, vec![], right_latch, vec![]);
        data.layout_mut().insert_inst(header, choose_latch);

        let one_a = data.new_local_inst().integer(1);
        let left_update = data
            .new_local_inst()
            .binary(BinaryOp::Add, parameter, one_a);
        data.layout_mut().insert_inst(left_latch, left_update);
        let left_backedge = data.new_local_inst().jump(header, vec![left_update]);
        data.layout_mut().insert_inst(left_latch, left_backedge);

        let one_b = data.new_local_inst().integer(1);
        let right_update = data
            .new_local_inst()
            .binary(BinaryOp::Add, one_b, parameter);
        data.layout_mut().insert_inst(right_latch, right_update);
        let right_backedge = data.new_local_inst().jump(header, vec![right_update]);
        data.layout_mut().insert_inst(right_latch, right_backedge);

        let variable = assert_single_iv(&program, function, parameter);
        assert_eq!(variable.update_values().len(), 2);
        assert_eq!(variable.step(), InductionStep::Add(one_a));
    }

    #[test]
    fn rejects_passthrough_zero_variant_and_non_affine_updates() {
        enum UpdateKind {
            Passthrough,
            AddZero,
            ReverseSub,
            Mul,
            VariantStep,
        }

        for update_kind in [
            UpdateKind::Passthrough,
            UpdateKind::AddZero,
            UpdateKind::ReverseSub,
            UpdateKind::Mul,
            UpdateKind::VariantStep,
        ] {
            let mut program = Program::new();
            let function = program.new_function(Type::get_unit(), "not_iv".into(), vec![]);
            let data = program.func_data_mut(function);
            let entry = data.add_entry_block();
            let header = data
                .new_basic_block()
                .basic_block("header".into(), vec![Type::get_i32()]);
            let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
            for block in [header, latch] {
                data.layout_mut().push_bb_back(block);
            }
            let zero = data.new_local_inst().integer(0);
            let entry_jump = data.new_local_inst().jump(header, vec![zero]);
            data.layout_mut().insert_inst(entry, entry_jump);
            let parameter = data.bb_data(header).params()[0];
            let one = data.new_local_inst().integer(1);
            let update = match update_kind {
                UpdateKind::Passthrough => parameter,
                UpdateKind::AddZero => data.new_local_inst().binary(BinaryOp::Add, parameter, zero),
                UpdateKind::ReverseSub => {
                    data.new_local_inst().binary(BinaryOp::Sub, one, parameter)
                }
                UpdateKind::Mul => data.new_local_inst().binary(BinaryOp::Mul, parameter, one),
                UpdateKind::VariantStep => {
                    let step = data.new_local_inst().binary(BinaryOp::Add, one, one);
                    data.layout_mut().insert_inst(header, step);
                    data.new_local_inst().binary(BinaryOp::Add, parameter, step)
                }
            };
            if update != parameter {
                data.layout_mut().insert_inst(header, update);
            }
            let to_latch = data.new_local_inst().jump(latch, vec![]);
            data.layout_mut().insert_inst(header, to_latch);
            let backedge = data.new_local_inst().jump(header, vec![update]);
            data.layout_mut().insert_inst(latch, backedge);

            let data = program.func_data(function);
            let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
            let analysis = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
            assert!(analysis.for_loop(only_loop(&loops)).is_empty());
        }
    }

    #[test]
    fn rejects_inconsistent_same_target_backedge_arms() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "mixed_iv".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        for block in [header, latch] {
            data.layout_mut().push_bb_back(block);
        }
        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let parameter = data.bb_data(header).params()[0];
        let one = data.new_local_inst().integer(1);
        let update = data.new_local_inst().binary(BinaryOp::Add, parameter, one);
        data.layout_mut().insert_inst(header, update);
        let to_latch = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, to_latch);
        let condition = data.new_local_inst().integer(1);
        let backedge =
            data.new_local_inst()
                .branch(condition, header, vec![update], header, vec![parameter]);
        data.layout_mut().insert_inst(latch, backedge);

        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let analysis = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        assert!(analysis.for_loop(only_loop(&loops)).is_empty());
    }

    #[test]
    fn rejects_float_recurrence() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "float_iv".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_f32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        for block in [header, latch] {
            data.layout_mut().push_bb_back(block);
        }
        let zero = data.new_local_inst().float(0.0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let parameter = data.bb_data(header).params()[0];
        let one = data.new_local_inst().float(1.0);
        let update = data.new_local_inst().binary(BinaryOp::Add, parameter, one);
        data.layout_mut().insert_inst(header, update);
        let to_latch = data.new_local_inst().jump(latch, vec![]);
        data.layout_mut().insert_inst(header, to_latch);
        let backedge = data.new_local_inst().jump(header, vec![update]);
        data.layout_mut().insert_inst(latch, backedge);

        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let analysis = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        assert!(analysis.for_loop(only_loop(&loops)).is_empty());
    }

    #[test]
    fn rejects_entry_header_without_an_outside_initial_value() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "entry_header".into(),
            vec![Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let header = data.add_entry_block();
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        data.layout_mut().push_bb_back(exit);

        let parameter = data.params()[0];
        let one = data.new_local_inst().integer(1);
        let update = data.new_local_inst().binary(BinaryOp::Add, parameter, one);
        data.layout_mut().insert_inst(header, update);
        let condition = data.new_local_inst().integer(1);
        let backedge = data
            .new_local_inst()
            .branch(condition, header, vec![update], exit, vec![]);
        data.layout_mut().insert_inst(header, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let analysis = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        assert!(analysis.for_loop(only_loop(&loops)).is_empty());
    }
}
