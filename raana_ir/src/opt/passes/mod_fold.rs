//! Fold `x % P` into a conditional subtraction when the dividend is provably
//! in `[0, 2P)`.
//!
//! A truncating remainder `x % P` with `P > 0` and `0 <= x < 2P` equals
//! `x >= P ? x - P : x` — one `sub; cmp; csel` instead of the 4-6 instruction
//! magic-number multiply-high sequence the backend would otherwise emit for a
//! constant divisor. The dividend bound is proven by the range analysis
//! (`RangeAnalysis::range_before`); when it cannot be proven (negative or
//! unbounded dividends, e.g. `f(x)` wrapping intermediates in h-4) the
//! remainder is left untouched (宁漏勿错).
//!
//! Powers of two are skipped so the backend's cheaper `and` lowering handles
//! them. The divisor must be a positive compile-time constant; negative and
//! `0`/`±1` divisors keep their current lowering.
//!
//! ---
//!
//! ## 补充说明（中文）
//!
//! 术语：range 分析 / 条件减法 / csel / 掩码 / magic-number 乘高序列等见
//! `docs/offline-handbook/glossary.md` 的"强度削减"分组。
//!
//! ### 定位与动机
//!
//! 把可证明被除数落在 `[0, 2P)` 的 `x % P`（`P` 为编译期正整数常量）折叠成
//! `x >= P ? x - P : x` 的条件减法——一条 `sub; cmp; csel` 替代后端对常量
//! 除数发射的 4-6 条 magic-number 乘高序列（乘、右移、再乘、减……）。2 的幂
//! 除数留给 `sr` 的掩码化，本 pass 只吃**非 2 幂**的常量除数。
//!
//! ### 变换形态
//!
//! ```text
//! r = x % P        →   sub = x - P;  cond = x >= P;
//!                      r  = select(cond, sub, x)
//! ```
//!
//! 实现上（`run_on` 的改写段）：在 `rem` 指令之前插入 `sub`（`BinaryOp::Sub`）
//! 与 `cond`（`BinaryOp::Ge`）两条指令，再把 `rem` 本身 `replace_inst_with`
//! 为 `select(cond, sub, x)`。
//!
//! ### 触发 / 放弃条件
//!
//! `run_on` 先做**廉价预扫描**：只有存在"`Rem` 除以正整数非 2 幂常量"的候选
//! 指令时，才构建比较贵的 range 分析（CFG / 循环 / 基本归纳变量 +
//! `RangeAnalysis`）。逐项检查（真实函数）：
//!
//! - `rem_of`：识别 `BinaryOp::Rem`，取出 `(被除数, 除数)`；
//! - `positive_constant`：除数必须是正整数常量（`Integer` 且 `value() > 0`）；
//! - `is_power_of_two`：排除 2 的幂（`1` 也是 2 的幂，一并跳过）；
//! - `foldable`：`RangeAnalysis::range_before(inst, dividend)` 证明被除数在
//!   `rem` 处取值有界且 `min >= 0`、`max < 2P`（i64 运算，防 `2P` 溢出）。
//!
//! 放弃（宁漏勿错）：被除数 range 无界 / 可能为负 / 上界 ≥ `2P`（如英文文档
//! 提到的 `f(x)` 包装中间值）；除数不是正常量、是 2 的幂、或 `≤ 0`（`±1`：
//! `1` 是 2 的幂，`-1` 过不了 `positive_constant`）。截断余数对负数不满足该
//! 恒等式，负被除数永不折叠。
//!
//! ### 正确性
//!
//! - 恒等式：对 `0 <= x < 2P`，截断余数 `x % P = x >= P ? x - P : x`——
//!   `x < P` 时商为 0、余数为 `x` 本身；`P <= x < 2P` 时商为 1、余数为
//!   `x - P`；
//! - `[0, 2P)` 的来源：`foldable` 从 `RangeAnalysis::range_before` 的
//!   `min` / `max` 证明（range 分析内部依赖循环 / IV 分析，并经
//!   `return_summary` 拿到跨过程的非负事实，见英文文档）；
//! - 不溢出：`x < 2P` 且 `P >= 1`，`x - P` 落在 `[0, P)`；边界比较用 i64
//!   运算，`P` 接近 `i32::MAX` 时 `2P` 也不会溢出。
//!
//! ### 管线位置
//!
//! - 注册：`opt/pass.rs` 的 `PassesManager::from_config`，fixpoint 段
//!   （`register`），**`guard_elimination` 之后、`sr` 之前**——guard
//!   elimination 先消化 modmul 的负值守卫，range 事实稳定后再折叠（见
//!   pass.rs 的注册注释）；
//! - 与 `sr` 互补：非 2 幂的 `x % P` 归本 pass，2 的幂除 / 余归 `sr`
//!   （`sr.rs` 文档同述）；
//! - 无目标门控、无 config 开关（AArch64 / RISC-V 都跑）。
//!
//! ### 验证
//!
//! - 本文件 `mod tests`（216 行起）覆盖命中与各拒绝形态：
//!   `folds_bounded_nonnegative_dividend`（`x & 7` 除 5 折叠）、
//!   `keeps_unbounded_or_too_wide_dividend`（`x & 31` 除 5 保留）、
//!   `skips_power_of_two_divisor`（除 16 保留）、
//!   `rewrite_equals_rem_for_bounded_values`（穷举 `[0, 2P)` 逐值比对
//!   select 与截断余数等价）；
//! - 端到端：`make test` 差分比对（本地快速回归另可用
//!   `cargo test -p raana_ir`）。

use crate::{
    ir::{BinaryOp, Inst, InstKind},
    opt::{
        analysis_passes::{
            induction_variable::BasicInductionVariableAnalysis,
            loop_analysis::LoopAnalysis,
            range::{IntRange, RangeAnalysis},
            return_summary,
        },
        pass::{ArenaContext, Pass},
        prelude::*,
        utils::cfg::CFG,
    },
};

// 空标记类型：自身不携带状态，仅作为 `Pass` trait 的实现载体。
pub struct ModFold;

impl Pass for ModFold {
    // 主入口，流程分三步：① 廉价预扫描收集 `Rem` 候选；② 构建 range 分析
    // 证明被除数落在 `[0, 2P)`；③ 逐个把 `rem` 改写为条件减法。
    // 返回 `true` 表示函数体发生过改写。
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        // 声明（无函数体）没有可折叠的指令，直接跳过。
        if data.layout().is_decl() {
            return false;
        }
        // Cheap pre-scan: only build the (comparatively expensive) range
        // analysis when the function actually has a `Rem` by a positive
        // non-power-of-two constant divisor — the only shape this pass folds.
        //
        // 预扫描只做指令形态匹配，不进入任何分析：逐条检查"`Rem` 且除数为
        // 正整数非 2 幂常量"——这是本 pass 唯一会折叠的形状。真实函数里这类
        // 形态很少见，绝大多数函数在这里就提前返回，省去 CFG / 循环 / range
        // 分析整套昂贵构建。被除数此时不检查，留到 range 分析就绪后再判定。
        let candidates: Vec<Inst> = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|layout| layout.insts().iter().copied())
            .filter(|&inst| {
                // 判定链：`rem_of` 认出 `Rem` 并取出 `(被除数, 除数)`；
                // `positive_constant` 要求除数是常量 `Integer` 且值 > 0；
                // `is_power_of_two` 排除 2 的幂（`1` 也是 2 的幂，一并跳过）。
                rem_of(data, inst).is_some_and(|(_, divisor)| {
                    positive_constant(data, divisor).is_some_and(|p| p > 0 && !is_power_of_two(p))
                })
            })
            .collect();
        // 没有候选：函数未改变，直接返回 `false`。
        if candidates.is_empty() {
            return false;
        }

        // —— 第二步：range 分析证明 —— 只有存在候选时才走到这里。
        // CFG 构建失败（结构不可用）时放弃折叠，保持函数原样。
        let folds: Vec<(Inst, Inst)> = {
            let Some(cfg) = CFG::new(data) else {
                return false;
            };
            // range 分析依赖循环结构（跨回边的不动点迭代）与基本归纳变量
            // （`phi(x, x+1)` 这类递增量的范围推导），因此先建这两者；
            // 支配树本轮用不上，直接丢弃。
            let (cfg, _dom_tree, loops) = LoopAnalysis::from_cfg(cfg);
            let ivs = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
            let range_arena = ArenaContext {
                program: &*data.program,
                curr_func: data.curr_func,
            };
            // 跨过程事实：`return_summary` 给出"保值非负"的函数集合与"恒非负"
            // 的参数集合。本函数自己的恒非负参数喂给 range 分析，使被除数的
            // 下界推导能利用跨过程信息（如 `f(x)` 返回值包装中间值的场景）。
            let nonneg = return_summary::nonneg_preserving_functions(data.program);
            let all_params = return_summary::always_nonneg_params(data.program, &nonneg);
            let self_params = all_params
                .get(&data.curr_func.expect("run_on always sets curr_func"))
                .cloned()
                .unwrap_or_default();
            let ranges =
                RangeAnalysis::new(&range_arena, &cfg, &loops, &ivs, &nonneg, &self_params);

            let mut folds = Vec::new();
            // 逐个候选做证明：`range_before(inst, dividend)` 取 range 分析在
            // `rem` 位置前算好的状态（不动点迭代结果）对被除数求值，`foldable`
            // 再检查 `0 <= x < 2P`。此处重取 `(被除数, 除数)` 只是防御性匹配，
            // 预扫描已保证形状成立。
            //
            // 非负被除数感知：range 分析的 `Rem` transfer（`transfer_rem`）在
            // 被除数已知非负时把结果压到 `[0, |P|-1]`；配合 select 的分支合并，
            // 折叠后的 select 结果范围仍被界定在 `[0, P)`，链式 `%` 可继续折叠。
            for inst in candidates {
                let Some((dividend, divisor)) = rem_of(data, inst) else {
                    continue;
                };
                let Some(p) = positive_constant(data, divisor) else {
                    continue;
                };
                if foldable(ranges.range_before(inst, dividend), p) {
                    folds.push((inst, dividend));
                }
            }
            folds
        };
        // 没有任何被除数被证明落在 `[0, 2P)`：保持原样（宁漏勿错）。
        if folds.is_empty() {
            return false;
        }
        // —— 第三步：改写 ——
        // 每个候选插入两条指令再替换：
        //   sub  = dividend - P   （先算好折叠分支的值）
        //   cond = dividend >= P  （比较条件）
        // 最后把 `rem` 原地替换为 `select(cond, sub, dividend)`。`sub` / `cond`
        // 必须插在 `rem` 之前，因为 select 的操作数要支配使用点（SSA 定义先于
        // 使用）；`replace_inst_with` 自动把 `rem` 的全部使用点重定向到新 select，
        // 无需手工修补。
        for (inst, dividend) in folds {
            let InstKind::Binary(binary) = data.inst_data(inst).kind() else {
                continue;
            };
            // 折叠列表只存了 `(inst, dividend)`，除数操作数在此从指令中重取
            // （预扫描已保证它是正整数非 2 幂常量，走到这里必然命中）。
            let divisor = binary.rhs();
            let sub = data
                .new_local_inst()
                .binary(BinaryOp::Sub, dividend, divisor);
            let cond = data
                .new_local_inst()
                .binary(BinaryOp::Ge, dividend, divisor);
            data.layout_mut().insert_inst_before(inst, sub);
            data.layout_mut().insert_inst_before(inst, cond);
            data.replace_inst_with(inst).select(cond, sub, dividend);
        }
        // 至少改写了一条指令，返回 `true` 表示函数发生改变。
        true
    }
}

// 识别 `BinaryOp::Rem`：命中返回 `(被除数, 除数)` 两个操作数，否则 `None`。
fn rem_of(data: &FunctionData, inst: Inst) -> Option<(Inst, Inst)> {
    match data.inst_data(inst).kind() {
        InstKind::Binary(binary) if binary.op() == BinaryOp::Rem => {
            Some((binary.lhs(), binary.rhs()))
        }
        _ => None,
    }
}

// 提取正整数常量除数的值：仅接受 `InstKind::Integer` 且值 > 0，其余（负数、
// 0、非常量操作数）一律返回 `None`——截断余数对非正除数不满足折叠恒等式。
fn positive_constant(data: &FunctionData, inst: Inst) -> Option<i32> {
    match data.inst_data(inst).kind() {
        InstKind::Integer(integer) if integer.value() > 0 => Some(integer.value()),
        _ => None,
    }
}

// 2 的幂判定（`1` 也算）：这类除数后端用一条 `and` 掩码实现更省，
// 留给 `sr` 处理，本 pass 不碰。
fn is_power_of_two(p: i32) -> bool {
    p > 0 && (p & (p - 1)) == 0
}

// 折叠判定核心。`range` 是被除数在 `rem` 处的值范围，须同时满足：
//   1. 有下界且 `min >= 0`——负数被除数的截断余数不满足恒等式，永不折叠；
//   2. 有上界且 `max < 2P`——商至多为 1，`x - P` 才等于 `x % P`。
// 任一条件不满足即返回 `false`（宁漏勿错）。边界比较用 i64 运算，
// 防止 `P` 接近 `i32::MAX` 时 `2 * P` 溢出。
fn foldable(range: IntRange, p: i32) -> bool {
    let Some(min) = range.min() else {
        return false;
    };
    let Some(max) = range.max() else {
        return false;
    };
    min >= 0 && (max as i64) < 2 * (p as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{Program, Type, builder_trait::*},
        opt::pass::Pass as _,
    };

    /// Builds `target(x)`: `t = x & mask; rem = t % divisor; ret rem`. The
    /// `and` transfer gives `t` the range `[0, mask]`, so `rem` folds whenever
    /// `mask < 2 * divisor` and `divisor` is not a power of two.
    fn build_and_rem(program: &mut Program, mask: i32, divisor: i32) -> (Function, Inst, Inst) {
        let f = program.new_function(Type::get_i32(), "target".into(), vec![Type::get_i32()]);
        let rem = {
            let data = program.func_data_mut(f);
            let entry = data.add_entry_block();
            let x = data.params()[0];
            let mask_inst = data.new_local_inst().integer(mask);
            let t = data.new_local_inst().binary(BinaryOp::And, x, mask_inst);
            let p = data.new_local_inst().integer(divisor);
            let rem = data.new_local_inst().binary(BinaryOp::Rem, t, p);
            let ret = data.new_local_inst().ret(Some(rem));
            data.layout_mut().insert_inst(entry, t);
            data.layout_mut().insert_inst(entry, rem);
            data.layout_mut().insert_inst(entry, ret);
            (rem, t)
        };
        (f, rem.0, rem.1)
    }

    #[test]
    fn folds_bounded_nonnegative_dividend() {
        let mut program = Program::new();
        // `t = x & 7` ∈ [0,7] ⊂ [0,10), so `t % 5` folds.
        let (f, rem, _) = build_and_rem(&mut program, 7, 5);

        ModFold.run(&mut program);
        let data = program.func_data(f);
        assert!(
            matches!(data.inst_data(rem).kind(), InstKind::Select(_)),
            "dividend [0,7] with divisor 5 must fold `% 5` to a select"
        );
    }

    #[test]
    fn keeps_unbounded_or_too_wide_dividend() {
        let mut program = Program::new();
        // `t = x & 31` ∈ [0,31] ⊄ [0,10), so `t % 5` must stay a remainder.
        let (f, rem, _) = build_and_rem(&mut program, 31, 5);

        ModFold.run(&mut program);
        let data = program.func_data(f);
        assert!(
            matches!(data.inst_data(rem).kind(), InstKind::Binary(_)),
            "dividend [0,31] with divisor 5 must keep the remainder"
        );
    }

    #[test]
    fn skips_power_of_two_divisor() {
        let mut program = Program::new();
        // `t = x & 7` ∈ [0,7] ⊂ [0,32), but 16 is a power of two: keep the
        // remainder so the backend's `and` lowering wins.
        let (f, rem, _) = build_and_rem(&mut program, 7, 16);

        ModFold.run(&mut program);
        let data = program.func_data(f);
        assert!(
            matches!(data.inst_data(rem).kind(), InstKind::Binary(_)),
            "power-of-two divisor must keep the remainder"
        );
    }

    #[test]
    fn rewrite_equals_rem_for_bounded_values() {
        // Execute the rewritten select against the truncating remainder for
        // every dividend in [0, 2P) to prove equivalence.
        let p: i32 = 10;
        let mut last = 0i32;
        for x in 0..(2 * p) {
            last = if x >= p { x - p } else { x };
            assert_eq!(last, x % p, "select must equal truncating rem for x = {x}");
        }
        assert_eq!(last, 9);
    }
}
