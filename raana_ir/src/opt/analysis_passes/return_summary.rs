//! Conditional non-negativity summaries for functions.
//!
//! Two related, interprocedural facts are computed over the call graph, both
//! feeding the M61 `guard_elimination` pass:
//!
//! 1. **Return summary** ([`nonneg_preserving_functions`]): a pure function is
//!    *non-negativity preserving* when its result is provably `>= 0` for every
//!    argument assignment in which all of its `i32` arguments are `>= 0`.
//! 2. **Parameter summary** ([`always_nonneg_params`]): which `i32` parameter
//!    of a function is provably `>= 0` at *every* call site.
//!
//! The `soyo_mulmod(a, b, p)` builtin (see the M60 `mulmod_recognize` pass) is
//! the base case for (1): `(i64)a * b % p` is non-negative whenever `a >= 0`
//! and `b >= 0` (a truncating remainder of a non-negative dividend never turns
//! negative). `multiply`/`power` in the fft0/fft1 NTT then inherit the
//! property, and their `a`/`b` parameters inherit (2).
//!
//! Both facts are *conditional*: a caller that feeds an input-derived value
//! (e.g. an array element that could be negative) stays conservative.
//!
//! The per-function non-negativity facts are computed by a small forward
//! dataflow over the SSA value graph (see [`nonneg_in_function`]): a value is
//! non-negative when it is a non-negative constant, a `>= 0` entry parameter, a
//! non-negative sum/product/remainder/select, a `soyo_mulmod` of two
//! non-negative operands, or a call to a non-negativity preserving function
//! with all non-negative arguments. Block parameters are the meet over their
//! incoming edges.
//!
//! ---
//!
//! ## 补充说明（中文）
//!
//! 本模块是**过程间非负性摘要**：回答两类跨函数问题——「哪些纯函数只要所有
//! `i32` 实参 `>= 0`，返回值就必然 `>= 0`」（返回摘要），以及「每个函数哪些
//! `i32` 形参在所有调用点都可证 `>= 0`」（参数摘要）。两个事实都是函数级
//! 摘要，一次算出、跨函数共享，主要喂给 M61 `guard_elimination`（守卫消除）
//! 把 `cmp; b.lt 0` 这类非负守卫折成无条件跳转。摘要都是**条件式**的：调用方
//! 喂入「输入派生值」（如可能为负的数组元素 load）时证明不了非负，就保守地
//! 不纳入——这是整个分析的保守边界。
//!
//! ## API 逐方法
//!
//! - `MODMUL_BUILTIN`（`pub const &str`）：内置取模乘法 `soyo_mulmod` 的名字，
//!   由 M60 `mulmod_recognize` pass 引入。`(i64)a * b % p` 在 `a >= 0` 且
//!   `b >= 0` 时非负（非负被除数截断取余不会变负），是返回摘要的基例。
//! - `nonneg_preserving_functions(&Program) -> FxHashSet<Function>`：返回摘要。
//!   求「纯函数且结果恒非负」的函数集合（算法见下）。调用方据此判定「调用
//!   `f(...)` 的结果是否可证 `>= 0`」。
//! - `always_nonneg_params(&Program, &FxHashSet<Function>) -> FxHashMap<Function,
//!   FxHashSet<Inst>>`：参数摘要。对每个有函数体的函数，给出在所有调用点都
//!   可证 `>= 0` 的形参集合（只考虑 `i32` 形参）；map 中每个函数都有键，值
//!   可能是空集。
//! - `nonneg_in_function(&Program, Function, &FxHashSet<Inst>, &FxHashSet<Function>)
//!   -> FxHashSet<Inst>`：函数内非负值集合。给定「已证明 `>= 0` 的入口参数」
//!   与「保非负函数集合」，在 SSA 值图上做前向数据流，求出该函数内所有可证
//!   `>= 0` 的值。它是前两个摘要共用的底层引擎，也是 `guard_elimination`
//!   判定守卫条件是否非负的唯一依据。
//!
//! 私有辅助：`returns_are_nonneg`（`f` 的所有 `ret` 值是否都可证 `>= 0`）、
//! `param_provable_at_all_sites`（`f` 第 `position` 个实参在**每个**调用点是否
//! 都可证 `>= 0`）、`value_is_nonneg`（单条值的规则表，见「正确性 / 边界」）、
//! `halving_base`（识别前端 `x / 2` 的 `sar(add(x, shr(x, 31)), 1)` 形态）。
//!
//! ## 算法
//!
//! 三个 fixpoint，自底向上：
//!
//! 1. `nonneg_in_function`（底层引擎）：以 `nonneg_params` 为种子，先扫本地
//!    指令把非负常量（`Integer >= 0`、`ZeroInit`）入集；然后反复迭代到不再
//!    变化——(a) **block 参数汇合**：非入口、可达的块参数，当且仅当**每条**
//!    入边传入的实参都可证非负时才标记。入口块参数必须排除：入口没有入边，
//!    空汇合（vacuous meet）会把所有入口参数误标为非负，它们只能由
//!    `always_nonneg_params` 的种子喂入；不可达块在 CFG 里没有边，跳过；
//!    (b) **沿定义传播**：对每个可达块按程序序逐条指令跑 `value_is_nonneg`
//!    规则表。集合单调增长，收敛即固定点。
//! 2. `nonneg_preserving_functions`：**co-inductive（最小）fixpoint**。候选集
//!    来自 `pure_function::pure_functions`（非纯函数直接排除；无函数体的声明
//!    也排除）。每轮对每个尚未入集的纯函数，**先假设它自己已在集合里**再
//!    检查——co-induction 的关键：自递归函数（如 NTT 的 `power` 调用自己）
//!    可以从自己的递归调用点证明非负，只要基例已成立。检查方式是
//!    `returns_are_nonneg`：把全部形参当作非负种子跑函数内分析，所有 `ret`
//!    值都可证非负才入集；有新增就再来一轮，直到不再变化。
//! 3. `always_nonneg_params`：**greatest fixpoint**。先把每个有函数体的函数的
//!    全部 `i32` 形参都当作候选，再反复淘汰：对每个形参扫描**全程序**所有
//!    调用方里的 `Call`/`TailCall` 指令（`param_provable_at_all_sites`），用
//!    调用方当前的参数摘要作种子跑函数内分析，只要存在一个调用点的实参证明
//!    不了非负就把该形参删掉；直到一轮内没有任何删除为止。
//!
//! 复杂度未刻意优化：`always_nonneg_params` 每轮对每个调用方重跑
//! `nonneg_in_function`；实际函数规模小、收敛快。
//!
//! ## 使用方清单
//!
//! - `opt/passes/guard_elimination.rs`（M61 `GuardElimination::run`，约 110 行）：
//!   三个公开 API 全用。每个 `run` 先算一遍程序级摘要（`nonneg_preserving_
//!   functions` → `always_nonneg_params`）跨函数共享，再逐函数用
//!   `nonneg_in_function` 求事实集；块的 `Branch` 终结符守卫条件在事实集里就
//!   折成 `Jump`。事实集里没有 `x` ⟹ 证明不了 ⟹ 守卫保留。
//! - `opt/analysis_passes/range.rs`（`RangeAnalysis`，约 311-318、364-369 行）：
//!   把 `nonneg_preserving_functions` 的结果存入 `nonneg_preserving` 字段、
//!   `always_nonneg_params` 的当前函数子集存入 `nonneg_params` 字段，作为值
//!   区间分析的下界种子；并用 `MODMUL_BUILTIN` 按名字定位 `soyo_mulmod`
//!   声明。
//! - `opt/passes/mod_fold.rs`（约 159-167 行）：调用 `nonneg_preserving_
//!   functions` + `always_nonneg_params`，把**本函数**的恒非负形参喂给 range
//!   分析，使被除数的下界推导能利用跨过程信息（如 `f(x)` 返回值包装中间值
//!   的场景）。
//! - `opt/passes/pointer_strength_reduction.rs`（约 248 行）：调用
//!   `nonneg_preserving_functions` 收紧仿射索引的取值区间，帮助
//!   `affine_range_fits_i32` 通过 i32 中间范围证明。
//! - `opt/analysis_passes/pure_function.rs`（约 186 行）：用 `MODMUL_BUILTIN`
//!   把 `soyo_mulmod` 判为纯函数——它是本模块的**上游**：
//!   `nonneg_preserving_functions` 的候选集正来自 `pure_functions`。
//!
//! 依赖链：`pure_function` → `return_summary` → `guard_elimination` / `range` /
//! `mod_fold` / `pointer_strength_reduction`。
//!
//! ## 正确性 / 边界
//!
//! - **i32 环绕语义下逐条规则 sound**：`x + y` / `x * y` **不**认作非负——两个
//!   大非负操作数相加/相乘会环绕成负值；`Rem`/`Shr` 保持符号；`Sar` 对非负值
//!   保持符号位为 0，且前端 `x / 2` 的 `sar(add(x, shr(x, 31)), 1)`（等于
//!   `floor(x / 2)`）经 `halving_base` 识别后对非负 `x` 成立；`Div` 只认**正
//!   的常量**除数（负除数翻转符号，变量除数可能为 0）；`And` 要求两操作数都
//!   非负；`Select` 要求两个分支值都非负。
//! - **条件式 / 保守**：两个程序级摘要都只对「所有 `i32` 实参 `>= 0`」作
//!   承诺；调用方喂输入派生值时证明不了非负，摘要不适用。`always_nonneg_params`
//!   要求**每个**调用点（含 `TailCall`）都可证，漏一个就淘汰该形参。
//! - **`soyo_mulmod` 基例**：要求恰好 3 个实参且前两个可证非负；模数 `p` 本身
//!   不参与判定（`(i64)a * b % p` 的符号只由被除数决定）。
//! - **快照**：与 `call_graph` 等分析一致，本模块是快照分析（见 `opt.rs` 模块
//!   文档）——结果只反映计算时的 IR；任何 pass 修改 IR 后必须重算
//!   （`guard_elimination` 每个 `run` 重算一次即是此约定）。
//! - **声明函数**：无函数体的声明（`is_decl`）既不会成为保非负候选，也不在
//!   `always_nonneg_params` 的结果里。
//!
//! ## 验证
//!
//! 本文件 `mod tests`（380 行起）4 个用例：`multiply_is_nonneg_preserving`
//! （`soyo_mulmod` 基例成立）、`pure_function_that_can_return_negative_is_not_
//! preserving`（`0 - x` 不得入集，负向用例）、`power_inherits_nonneg_preserving_
//! from_multiply`（`multiply` → 自递归 `power` 继承，co-induction 生效）、
//! `always_nonneg_params_requires_every_call_site`（`main` 用数组 load 喂参，
//! 形参不得入集）。使用方测试间接覆盖（如 `guard_elimination.rs` `mod tests`
//! 的 `folds_guard_on_constant_nonneg_and_keeps_param_guard`），全量
//! `cargo test -p raana_ir` 回归。
//!
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use crate::{
    ir::{BasicBlock, BinaryOp, Function, Inst, InstKind, Program, arena::Arena},
    opt::{
        analysis_passes::pure_function::pure_functions,
        utils::{cfg::CFG, logical_edge::incoming_edges},
    },
};

/// The compiler-provided modular-multiplication builtin introduced by the M60
/// `mulmod_recognize` pass. Its result is `(i64)a * b % p`, which is
/// non-negative when `a` and `b` are.
pub const MODMUL_BUILTIN: &str = "soyo_mulmod";

/// The pure functions whose result is `>= 0` whenever every `i32` argument is
/// `>= 0`. Co-inductive fixpoint: each candidate is checked under the
/// assumption that it is itself non-negativity preserving, so a self-recursive
/// function (`power` calling `power`) is provable from its own recursive call
/// site once its base cases are.
pub fn nonneg_preserving_functions(program: &Program) -> FxHashSet<Function> {
    let pure = pure_functions(program);
    let mut nonneg: FxHashSet<Function> = FxHashSet::default();
    loop {
        let mut changed = false;
        for &f in program.function_layout() {
            if pure.contains(&f) && !nonneg.contains(&f) && !program.func_data(f).layout().is_decl()
            {
                let mut assumed = nonneg.clone();
                assumed.insert(f);
                let all_params: FxHashSet<Inst> =
                    program.func_data(f).params().iter().copied().collect();
                if returns_are_nonneg(program, f, &all_params, &assumed) {
                    nonneg.insert(f);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    nonneg
}

/// For each function, the subset of its `i32` parameters that are provably
/// `>= 0` at every call site. Greatest fixpoint: start with every parameter as
/// a candidate and repeatedly drop any parameter some call site cannot prove.
pub fn always_nonneg_params(
    program: &Program,
    nonneg: &FxHashSet<Function>,
) -> FxHashMap<Function, FxHashSet<Inst>> {
    let mut result: FxHashMap<Function, FxHashSet<Inst>> = FxHashMap::default();
    for &f in program.function_layout() {
        if program.func_data(f).layout().is_decl() {
            continue;
        }
        result.insert(
            f,
            program
                .func_data(f)
                .params()
                .iter()
                .copied()
                .filter(|&param| program.func_data(f).inst_data(param).ty().is_i32())
                .collect(),
        );
    }
    loop {
        let mut removed = false;
        let current = result.clone();
        for &f in program.function_layout() {
            if program.func_data(f).layout().is_decl() {
                continue;
            }
            let candidate: Vec<Inst> = result[&f].iter().copied().collect();
            for param in candidate {
                let position = program
                    .func_data(f)
                    .params()
                    .iter()
                    .position(|&p| p == param)
                    .expect("parameter must be a formal parameter");
                if !param_provable_at_all_sites(program, f, position, &current, nonneg) {
                    result.get_mut(&f).expect("function key").remove(&param);
                    removed = true;
                }
            }
        }
        if !removed {
            break;
        }
    }
    result
}

/// Whether argument `position` of every `call f(...)` in the program is
/// provably `>= 0`, using the current parameter summaries `current`.
fn param_provable_at_all_sites(
    program: &Program,
    f: Function,
    position: usize,
    current: &FxHashMap<Function, FxHashSet<Inst>>,
    nonneg: &FxHashSet<Function>,
) -> bool {
    for &caller in program.function_layout() {
        let caller_data = program.func_data(caller);
        if caller_data.layout().is_decl() {
            continue;
        }
        let caller_params = current.get(&caller).cloned().unwrap_or_default();
        let facts = nonneg_in_function(program, caller, &caller_params, nonneg);
        for layout in caller_data.layout().basicblocks() {
            for &inst in layout.insts() {
                let args: SmallVec<[Inst; 8]> = match caller_data.inst_data(inst).kind() {
                    InstKind::Call(call) if call.callee() == f => {
                        call.args().iter().copied().collect()
                    }
                    InstKind::TailCall(tail) if tail.callee() == f => {
                        tail.args().iter().copied().collect()
                    }
                    _ => continue,
                };
                let Some(&arg) = args.get(position) else {
                    return false;
                };
                if !facts.contains(&arg) {
                    return false;
                }
            }
        }
    }
    true
}

/// Whether every `ret` value of `f` is provably `>= 0`, assuming `nonneg_params`
/// (all parameters) are `>= 0` and using `nonneg` for callee summaries.
fn returns_are_nonneg(
    program: &Program,
    f: Function,
    nonneg_params: &FxHashSet<Inst>,
    nonneg: &FxHashSet<Function>,
) -> bool {
    let data = program.func_data(f);
    let facts = nonneg_in_function(program, f, nonneg_params, nonneg);
    data.layout()
        .basicblocks()
        .iter()
        .flat_map(|layout| layout.insts().iter().copied())
        .filter_map(|inst| match data.inst_data(inst).kind() {
            InstKind::Return(ret) => ret.value(),
            _ => None,
        })
        .all(|value| facts.contains(&value))
}

/// The set of values of `f` provably `>= 0`, assuming `nonneg_params` (its
/// entry parameters known `>= 0`) and using `nonneg` return summaries for
/// callees. Forward worklist: block parameters join (AND) their incoming edge
/// arguments, and definitions propagate through the value graph until stable.
pub fn nonneg_in_function(
    program: &Program,
    f: Function,
    nonneg_params: &FxHashSet<Inst>,
    nonneg: &FxHashSet<Function>,
) -> FxHashSet<Inst> {
    let data = program.func_data(f);
    let mut facts: FxHashSet<Inst> = nonneg_params.clone();
    for (&inst, inst_data) in data.local().inst_arena().datas() {
        match inst_data.kind() {
            InstKind::Integer(int) if int.value() >= 0 => {
                facts.insert(inst);
            }
            InstKind::ZeroInit => {
                facts.insert(inst);
            }
            _ => {}
        }
    }

    let Some(cfg) = CFG::new(data) else {
        return facts;
    };
    // block parameter -> (owning block, position among its params)
    let mut param_blocks: FxHashMap<Inst, (BasicBlock, usize)> = FxHashMap::default();
    for layout in data.layout().basicblocks() {
        let block = layout.bb();
        for (index, &param) in data.bb_data(block).params().iter().enumerate() {
            if data.inst_data(param).ty().is_i32() {
                param_blocks.insert(param, (block, index));
            }
        }
    }

    let mut changed = true;
    while changed {
        changed = false;
        // Settle block parameters: non-negative iff every incoming edge passes
        // a non-negative argument. Entry-block parameters are excluded: the
        // entry has no incoming edges, so the vacuous meet would wrongly mark
        // every entry parameter non-negative. They are seeded only from
        // `nonneg_params` (the always-nonneg call-site analysis). Unreachable
        // blocks are skipped: the CFG has no edges for them.
        for (&param, &(block, position)) in &param_blocks {
            if facts.contains(&param) {
                continue;
            }
            if block == cfg.entry() || !cfg.is_reachable(block) {
                continue;
            }
            let all_nonneg = incoming_edges(data, &cfg, block).iter().all(|edge| {
                edge.args(data)
                    .get(position)
                    .is_some_and(|&arg| value_is_nonneg(program, data, arg, &facts, nonneg))
            });
            if all_nonneg {
                facts.insert(param);
                changed = true;
            }
        }
        // Propagate through definitions (per block, in program order).
        for layout in data.layout().basicblocks() {
            if !cfg.is_reachable(layout.bb()) {
                continue;
            }
            for &inst in layout.insts() {
                if !facts.contains(&inst) && value_is_nonneg(program, data, inst, &facts, nonneg) {
                    facts.insert(inst);
                    changed = true;
                }
            }
        }
    }
    facts
}

/// Whether `value` is provably `>= 0` given the current `facts`.
///
/// Every rule must be sound under i32 wrapping semantics:
/// `x + y`/`x * y` can wrap negative for large non-negative operands, so they
/// are *not* claimed here (the signed halving chain `sar(add(x, shr(x,31)),1)`
/// — the frontend's `x / 2` — is handled specially). A signed division keeps
/// the dividend's sign, so it is only claimed for a *positive* constant
/// divisor.
fn value_is_nonneg(
    program: &Program,
    data: &crate::ir::FunctionData,
    value: Inst,
    facts: &FxHashSet<Inst>,
    nonneg: &FxHashSet<Function>,
) -> bool {
    if facts.contains(&value) {
        return true;
    }
    match data.inst_data(value).kind() {
        InstKind::Integer(int) => int.value() >= 0,
        InstKind::ZeroInit => true,
        InstKind::Binary(binary) => match binary.op() {
            // A remainder of a non-negative dividend keeps the sign; an
            // arithmetic/logical shift of a non-negative value keeps the sign
            // bit clear.
            BinaryOp::Rem | BinaryOp::Shr => {
                value_is_nonneg(program, data, binary.lhs(), facts, nonneg)
            }
            BinaryOp::Sar => {
                // The frontend's signed halving chain
                // `sar(add(x, shr(x, 31)), 1)` equals `floor(x / 2)`, which is
                // non-negative for non-negative `x`. Any other arithmetic
                // shift of a non-negative value also keeps the sign bit clear.
                let mut base = None;
                if matches!(
                    data.inst_data(binary.rhs()).kind(),
                    InstKind::Integer(k) if k.value() == 1
                ) {
                    base = halving_base(data, binary.lhs());
                }
                let x = match base {
                    Some(x) => x,
                    None => binary.lhs(),
                };
                value_is_nonneg(program, data, x, facts, nonneg)
            }
            BinaryOp::And => {
                value_is_nonneg(program, data, binary.lhs(), facts, nonneg)
                    && value_is_nonneg(program, data, binary.rhs(), facts, nonneg)
            }
            BinaryOp::Div => {
                // `x / k` for `x >= 0` and a positive constant `k` stays
                // non-negative. A negative `k` would flip the sign; a variable
                // divisor could be zero.
                value_is_nonneg(program, data, binary.lhs(), facts, nonneg)
                    && matches!(
                        data.inst_data(binary.rhs()).kind(),
                        InstKind::Integer(k) if k.value() > 0
                    )
            }
            _ => false,
        },
        InstKind::Select(select) => {
            value_is_nonneg(program, data, select.if_true(), facts, nonneg)
                && value_is_nonneg(program, data, select.if_false(), facts, nonneg)
        }
        InstKind::Call(call) => {
            let callee = call.callee();
            let args = call.args();
            if program.func_data(callee).name() == MODMUL_BUILTIN {
                // `(i64)a*b % p` is non-negative for a non-negative product.
                args.len() == 3
                    && value_is_nonneg(program, data, args[0], facts, nonneg)
                    && value_is_nonneg(program, data, args[1], facts, nonneg)
            } else if nonneg.contains(&callee) {
                args.iter()
                    .all(|&arg| value_is_nonneg(program, data, arg, facts, nonneg))
            } else {
                false
            }
        }
        _ => false,
    }
}

/// If `value` is the frontend's signed halving chain `add(x, shr(x, 31))`
/// (either operand order), return `x`. Used by the `Sar` rule: the full
/// `sar(add(x, shr(x,31)), 1)` computes `floor(x / 2)`, which is non-negative
/// for non-negative `x`.
fn halving_base(data: &crate::ir::FunctionData, value: Inst) -> Option<Inst> {
    let (lhs, rhs) = match data.inst_data(value).kind() {
        InstKind::Binary(binary) if binary.op() == BinaryOp::Add => (binary.lhs(), binary.rhs()),
        _ => return None,
    };
    let shr_is = |inst: Inst| -> bool {
        matches!(
            data.inst_data(inst).kind(),
            InstKind::Binary(binary)
                if binary.op() == BinaryOp::Shr
                    && matches!(
                        data.inst_data(binary.rhs()).kind(),
                        InstKind::Integer(k) if k.value() == 31
                    )
        )
    };
    if shr_is(rhs) {
        Some(lhs)
    } else if shr_is(lhs) {
        Some(rhs)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Program, Type, arena::Arena, builder_trait::*};

    const P: i32 = 998244353;

    /// Declare the `soyo_mulmod(a, b, p)` builtin.
    fn declare_modmul(program: &mut Program) -> Function {
        program.new_function(
            Type::get_i32(),
            MODMUL_BUILTIN.into(),
            vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
        )
    }

    /// Build the M60-rewritten `multiply(a, b)`: `b < 0 ? 0 : soyo_mulmod(a,b,P)`.
    fn build_multiply(program: &mut Program, modmul: Function) -> Function {
        let f = program.new_function(
            Type::get_i32(),
            "multiply".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        {
            let data = program.func_data_mut(f);
            let entry = data.add_entry_block();
            let zero_block = data.new_basic_block().basic_block("zero".into(), vec![]);
            let fast = data.new_basic_block().basic_block("fast".into(), vec![]);
            data.layout_mut().push_bb_back(zero_block);
            data.layout_mut().push_bb_back(fast);
            let a = data.params()[0];
            let b = data.params()[1];
            let zero = data.new_local_inst().integer(0);
            let cond = data.new_local_inst().binary(BinaryOp::Lt, b, zero);
            let branch = data
                .new_local_inst()
                .branch(cond, zero_block, vec![], fast, vec![]);
            data.layout_mut().insert_inst(entry, cond);
            data.layout_mut().insert_inst(entry, branch);
            let ret_zero = data.new_local_inst().ret(Some(zero));
            data.layout_mut().insert_inst(zero_block, ret_zero);
            let p = data.new_local_inst().integer(P);
            let call = data
                .new_local_inst()
                .call_with_type(modmul, vec![a, b, p], Type::get_i32());
            let ret = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(fast, call);
            data.layout_mut().insert_inst(fast, ret);
        }
        f
    }

    #[test]
    fn multiply_is_nonneg_preserving() {
        let mut program = Program::new();
        let modmul = declare_modmul(&mut program);
        build_multiply(&mut program, modmul);
        let set = nonneg_preserving_functions(&program);
        assert_eq!(
            set.len(),
            1,
            "only multiply should preserve non-negativity: {set:?}"
        );
    }

    #[test]
    fn pure_function_that_can_return_negative_is_not_preserving() {
        // `f(x) = 0 - x` returns `<= 0` for non-negative `x`, so it must not
        // be claimed non-negativity preserving.
        let mut program = Program::new();
        let f = program.new_function(Type::get_i32(), "f".into(), vec![Type::get_i32()]);
        {
            let data = program.func_data_mut(f);
            let entry = data.add_entry_block();
            let x = data.params()[0];
            let zero = data.new_local_inst().integer(0);
            let neg = data.new_local_inst().binary(BinaryOp::Sub, zero, x);
            let ret = data.new_local_inst().ret(Some(neg));
            data.layout_mut().insert_inst(entry, neg);
            data.layout_mut().insert_inst(entry, ret);
        }
        let set = nonneg_preserving_functions(&program);
        assert!(!set.contains(&f), "f(x) = -x must not be preserving");
    }

    /// `power(a, b)` with the M60-inlined multiply: returns `soyo_mulmod` of the
    /// recursion result, with the frontend signed-halving `b/2`.
    fn build_power(program: &mut Program, modmul: Function) -> Function {
        let f = program.new_function(
            Type::get_i32(),
            "power".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        {
            let data = program.func_data_mut(f);
            let entry = data.add_entry_block();
            let even = data.new_basic_block().basic_block("even".into(), vec![]);
            let odd = data.new_basic_block().basic_block("odd".into(), vec![]);
            data.layout_mut().push_bb_back(even);
            data.layout_mut().push_bb_back(odd);
            let a = data.params()[0];
            let b = data.params()[1];

            let thirty_one = data.new_local_inst().integer(31);
            let one = data.new_local_inst().integer(1);
            let p = data.new_local_inst().integer(P);

            // half = sar(add(b, shr(b,31)), 1)
            let shr = data.new_local_inst().binary(BinaryOp::Shr, b, thirty_one);
            let add = data.new_local_inst().binary(BinaryOp::Add, b, shr);
            let half = data.new_local_inst().binary(BinaryOp::Sar, add, one);
            let rec = data
                .new_local_inst()
                .call_with_type(f, vec![a, half], Type::get_i32());
            // cur = soyo_mulmod(rec, rec, P)
            let cur =
                data.new_local_inst()
                    .call_with_type(modmul, vec![rec, rec, p], Type::get_i32());
            // b & 0x80000001 == 1 ? soyo_mulmod(cur, a, P) : cur
            let mask = data.new_local_inst().integer(-2147483647);
            let and = data.new_local_inst().binary(BinaryOp::And, b, mask);
            let eq = data.new_local_inst().binary(BinaryOp::Eq, and, one);
            let branch = data.new_local_inst().branch(eq, odd, vec![], even, vec![]);
            for inst in [shr, add, half, rec, cur, and, eq, branch] {
                data.layout_mut().insert_inst(entry, inst);
            }
            let ret_even = data.new_local_inst().ret(Some(cur));
            data.layout_mut().insert_inst(even, ret_even);
            let odd_call =
                data.new_local_inst()
                    .call_with_type(modmul, vec![cur, a, p], Type::get_i32());
            let ret_odd = data.new_local_inst().ret(Some(odd_call));
            data.layout_mut().insert_inst(odd, odd_call);
            data.layout_mut().insert_inst(odd, ret_odd);
        }
        f
    }

    #[test]
    fn power_inherits_nonneg_preserving_from_multiply() {
        let mut program = Program::new();
        let modmul = declare_modmul(&mut program);
        build_multiply(&mut program, modmul);
        build_power(&mut program, modmul);
        let set = nonneg_preserving_functions(&program);
        assert_eq!(set.len(), 2, "multiply and power preserve: {set:?}");
    }

    #[test]
    fn always_nonneg_params_requires_every_call_site() {
        let mut program = Program::new();
        let modmul = declare_modmul(&mut program);
        build_multiply(&mut program, modmul);
        let f = program.new_function(Type::get_i32(), "f".into(), vec![Type::get_i32()]);
        {
            let data = program.func_data_mut(f);
            let entry = data.add_entry_block();
            let x = data.params()[0];
            let p = data.new_local_inst().integer(P);
            let call = data
                .new_local_inst()
                .call_with_type(modmul, vec![x, x, p], Type::get_i32());
            let ret = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(entry, call);
            data.layout_mut().insert_inst(entry, ret);
        }
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        {
            let data = program.func_data_mut(main);
            let entry = data.add_entry_block();
            // arg is an array load: not provably >= 0.
            let ptr = data.new_local_inst().alloc(Type::get_i32());
            let load = data.new_local_inst().load(ptr);
            let call = data
                .new_local_inst()
                .call_with_type(f, vec![load], Type::get_i32());
            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(entry, load);
            data.layout_mut().insert_inst(entry, call);
            data.layout_mut().insert_inst(entry, ret);
        }
        let nonneg = nonneg_preserving_functions(&program);
        let params = always_nonneg_params(&program, &nonneg);
        assert!(
            params.get(&f).is_none_or(|set| set.is_empty()),
            "f's input-derived parameter must not be always-nonneg: {:?}",
            params.get(&f)
        );
    }
}
