//! # 纯函数判定：`pure_function` 分析
//!
//! 一句话定位：判定程序里哪些函数是**纯函数**——行为完全由实参决定、不产生任何
//! 可观察副作用（无 I/O、不写调用方可见的内存、不取全局地址）。纯函数是经典
//! 优化的前提：LICM 把循环不变的纯调用外提到 preheader、公共子表达式消除折叠
//! 相同实参的重复调用、DCE 删除结果未使用的调用。本模块是 `analysis_passes`
//! 里的基础设施分析：它只回答「这个函数纯不纯」，怎么利用纯性由各 pass 决定。
//! 输出是一个全程序范围内的纯函数集合（`Function`/`Inst` 都是 arena 下标句柄，
//! 见 `ir` 模块文档），是快照：程序一旦被改写即失效，需重新计算。
//!
//! 与姊妹分析 `effects`（`EffectAnalysis`，`analysis_passes/effects.rs`）的分工：
//! `effects` 做更细的**效果摘要**（读写对象集合、points-to、stdin/stdout 标志），
//! LICM 外提与 DCE 删调用实际使用的是 `EffectAnalysis`；本模块做**函数粒度**的
//! 纯性判定，直接消费方是 `return_summary`（非负性摘要），并作为 M60
//! `mulmod_recognize` 的下游约定（`soyo_mulmod` 视为纯函数）。两者并存、口径
//! 不同（见「正确性与边界」）。
//!
//! ## API 一览
//!
//! - `pure_functions(program: &Program) -> HashSet<Function>`（公开，唯一入口）：
//!   返回程序里全部纯函数的下标集合。算法 = 局部纯度初选 + 跨过程最小不动点
//!   剔除（见下）。成本是一次全程序遍历，调用方应只算一遍并复用结果。
//! - `is_library_function(name: &str) -> bool`（公开）：`name` 是否是 SysY 运行时
//!   库的入口名——`getint`/`getch`/`getfloat`/`getarray`/`getfarray`（读 stdin）
//!   与 `putint`/`putch`/`putfloat`/`putarray`/`putf`（写 stdout）。这些入口有
//!   可观察 I/O，**永远不可能纯**。名单与 `soyo_compiler` 前端在下降开始时
//!   声明的运行时函数一致（`frontend/utils.rs` 的 `decl_library_functions`）。
//! - `locally_pure(program, func) -> bool`（私有）：只看函数**自身**函数体的
//!   局部纯度，不检查被调方；`pure_functions` 用它筛初选集合。
//! - `caller_visible_ptr(data, params, ptr) -> bool`（私有）：判定一个指针是否
//!   **调用方可见**（是全局、是函数参数、或由二者经 GEP/转换派生）；局部纯度
//!   检查的公共工具。
//!
//! ## 算法：两步走
//!
//! ### 第一步：局部纯度 `locally_pure`
//!
//! 对每个函数依次检查：
//!
//! 1. 名字命中 `is_library_function` → 不纯（I/O 副作用）；
//! 2. 名字是 `soyo_mulmod`（即 `return_summary::MODMUL_BUILTIN`）→ 纯：这是 M60
//!    `mulmod_recognize` 引入的**编译器内置声明**，没有函数体，但 AArch64 后端把
//!    每次调用展开成 `smull; sxtw; sdiv; msub` 纯算术序列，无副作用；
//! 3. 没有入口基本块（`layout().entry_bb()` 为 `None` 的声明）→ 不纯：无体的
//!    外部声明行为未知，绝不视为纯（唯一例外是上面的 `soyo_mulmod`）；
//! 4. 逐基本块、逐指令扫描函数体：
//!    - `Load`：源指针调用方可见 → 不纯。理由：读到的值依赖外部内存状态，行为
//!      不再仅由实参决定；
//!    - `Store`：**源或目标**指针任一调用方可见 → 不纯（写可见内存是副作用）；
//!    - `MemZero`：目标指针调用方可见 → 不纯；
//!    - `GetElemPtr`：`base` 是全局 → 不纯（把全局地址交给别人就是可观察的）。
//!    其余指令（算术、经本地 `Alloc` 派生的读写等）不构成副作用，可留在纯函数
//!    里——本地 `Alloc` 上的标量读写对程序其余部分不可见。
//!
//! ### 指针可见性 `caller_visible_ptr`
//!
//! worklist + 去重集合，沿 def 链回溯指针来源：
//! - 是全局或函数参数 → **可见**；
//! - 是 `GetElemPtr`/`Cast` → 继续回溯其 `base()`/`src()` 操作数（GEP/转换只改
//!   地址、不改对象身份）；
//! - **其他一切**（`Load` 的结果、block 参数指针、`Select` …）→ 保守判为可见。
//!
//! 只有能一路追到**本地 `Alloc`** 的指针才被当作调用方不可见。
//!
//! ### 第二步：跨过程最小不动点 `pure_functions`
//!
//! 1. 初选：把全部 `locally_pure` 的函数放入集合；
//! 2. 迭代：对集合内每个函数，扫描其全部 `Call`/`TailCall` 指令，只要存在一个
//!    被调方**不在**集合里，就把它移出集合；直到某轮没有任何移除为止。
//!
//! 这是**最小不动点**（least fixpoint）：局部纯但（传递地）调用不纯函数的函数
//! 会被逐轮剔除。递归与相互递归天然正确：自递归函数只调用自己，初选时自己已在
//! 集合内，只要局部纯就能留下；相互递归的强连通分量同理。
//!
//! ## 使用方清单（全仓库 grep 确认）
//!
//! - **直接调用**：`analysis_passes/return_summary.rs` 是唯一直接使用方——
//!   `nonneg_preserving_functions`（第 51 行）以 `pure_functions` 的结果为候选
//!   集合，再求「纯函数结果非负」与「参数恒非负」（`always_nonneg_params`，
//!   共归纳/最大不动点），供范围分析使用。
//! - **间接下游**（经 `return_summary` 摘要）：
//!   - `analysis_passes/range.rs`：`RangeAnalysis` 持有 nonneg_preserving 集合与
//!     nonneg_params，给纯函数调用赋 `[0, i32::MAX]` 范围（`a`、`b` 非负时
//!     `soyo_mulmod` 得 `[0, P)`）；
//!   - `passes/guard_elimination.rs`：range 证明 modmul guard `br (x < 0)` 永不
//!     命中时折叠（`pass.rs` 注释称之为 M61 pure non-negativity summaries）；
//!   - `passes/mod_fold.rs`：dividend 落在 `[0, 2P)` 时把 `x % P` 折叠成条件减法
//!     （一条 `sub; cmp; csel`，替代 4–6 条魔数序列）；
//!   - `passes/pointer_strength_reduction.rs`：用非负性做指针强度削减。
//! - **姊妹分析**：`analysis_passes/effects.rs` 的 `EffectAnalysis` 是 LICM 外提
//!   与 DCE 删调用的实际纯度来源（`is_removable`/`is_pure`/`may_write_memory`），
//!   与本模块不同实现、不同粒度，两者并存互补。
//! - **下游约定**：`passes/mulmod_recognize.rs` 的模块文档（第 86–88 行）明确
//!   「`soyo_mulmod` 是无体声明，`pure_function.rs` 视其为纯函数」——改写 pass
//!   依赖本模块的这条特判。
//!
//! ## 正确性与边界
//!
//! - **声明函数**：运行时库入口按名单拒绝；其余无体声明（如 `_sysy_starttime`
//!   等，`entry_bb` 为 `None`）也一律拒绝。宁可保守不错放。
//! - **`soyo_mulmod`**：唯一被当作纯函数的无体声明。安全前提是 AArch64 后端把
//!   调用展开成无副作用的纯算术；RISC-V 门控（`enable_chain_to_switch`）下该
//!   声明不会出现，因此这条特判不跨目标泄漏。
//! - **递归 / 相互递归**：最小不动点保证收敛且结果正确（见算法第二步）。
//! - **保守默认**：指针来源不可证时一律按「调用方可见」处理；对可见内存的
//!   `Load` 也判不纯（行为依赖外部状态）。注意这与 `effects` 的口径不同——
//!   那里纯读可见内存仍是 `is_pure`（`effects.rs` 测试
//!   `load_of_global_is_read_only_but_pure`）。两个分析各按各的口径服务不同的
//!   优化，改动本文件时不要把它们混为一谈。
//! - **`TailCall` 与 `Call` 同等对待**：第二步的剔除检查两者都查，不漏尾调用。
//!
//! ## 验证
//!
//! 本文件没有内联 `mod tests`；纯性行为由下游测试间接覆盖：
//! - `return_summary.rs` `mod tests`：`pure_function_that_can_return_negative_is_not_preserving`
//!   等（经 `pure_functions` 参与计算）；
//! - `dce.rs` `mod tests`：`removes_pure_unused_calls_but_keeps_io_calls` /
//!   `keeps_pure_calls_whose_result_is_used` 等（`EffectAnalysis` 口径的删调用行为）；
//! - `effects.rs` `mod tests`：`pure_function_without_memory_ops` 等（姊妹分析）；
//! - 全量回归：`cargo test -p raana_ir`（236 个单元测试）；性能门禁
//!   `make test ARGS="-O 2"`。
//!
use crate::opt::prelude::*;

/// Names of the runtime-library entry points. These always have observable
/// side effects (I/O), so they can never be treated as pure.
pub fn is_library_function(name: &str) -> bool {
    matches!(
        name,
        "getint"
            | "getch"
            | "getfloat"
            | "getarray"
            | "getfarray"
            | "putint"
            | "putch"
            | "putfloat"
            | "putarray"
            | "putfarray"
            | "putf"
    )
}

/// A pointer is *caller-visible* when its target may be reachable by the
/// caller: it is a global, a function parameter, or is derived from either via
/// GEP/casts. Reading or writing through such a pointer is observable by the
/// rest of the program, so it makes a function impure. Pointers that trace back
/// to a local `Alloc` (through GEP/cast only) are invisible to the caller.
fn caller_visible_ptr(data: &FunctionData, params: &[Inst], ptr: Inst) -> bool {
    let mut worklist = vec![ptr];
    let mut seen = HashSet::default();
    while let Some(inst) = worklist.pop() {
        if !seen.insert(inst) {
            continue;
        }
        if inst.is_global() || params.contains(&inst) {
            return true;
        }
        match data.inst_data(inst).kind() {
            InstKind::GetElemPtr(gep) => worklist.push(gep.base()),
            InstKind::Cast(cast) => worklist.push(cast.src()),
            // Anything not provably derived from a local Alloc (a load, a
            // block-arg pointer, a select, ...) is treated as caller-visible.
            _ => return true,
        }
    }
    false
}

/// A function is *locally pure* when its body performs no observable memory
/// operation: it may not load or store through a caller-visible address
/// (global or pointer parameter), write memory with `MemZero`, or take the
/// address of a global. Scalar reads/writes through local `Alloc` pointers are
/// invisible to the rest of the program, so they do not make a function impure.
///
/// Callers must still check that every callee is pure (see
/// [`pure_functions`]); this only inspects the function's own body.
fn locally_pure(program: &Program, func: Function) -> bool {
    let data = program.func_data(func);
    if is_library_function(data.name()) {
        return false;
    }
    // The M60 `soyo_mulmod` modmul builtin is a compiler-provided declaration
    // with no body, but it is pure: the AArch64 backend expands every call to
    // `smull; sxtw; sdiv; msub` arithmetic with no side effects.
    if data.name() == super::return_summary::MODMUL_BUILTIN {
        return true;
    }
    // Declarations (library-style entries with no body) have unknown
    // behaviour; never treat them as pure.
    if data.layout().entry_bb().is_none() {
        return false;
    }
    let params = data.params();
    for bb in data.layout().basicblocks() {
        for &inst in bb.insts() {
            match data.inst_data(inst).kind() {
                InstKind::Load(load) => {
                    if caller_visible_ptr(data, params, load.src()) {
                        return false;
                    }
                }
                InstKind::Store(store) => {
                    if caller_visible_ptr(data, params, store.src())
                        || caller_visible_ptr(data, params, store.dest())
                    {
                        return false;
                    }
                }
                InstKind::MemZero(mem_zero) => {
                    if caller_visible_ptr(data, params, mem_zero.dest()) {
                        return false;
                    }
                }
                InstKind::GetElemPtr(gep) => {
                    if gep.base().is_global() {
                        return false;
                    }
                }
                _ => {}
            }
        }
    }
    true
}

/// The set of pure functions in the program: functions whose behaviour is
/// fully determined by their arguments, so that duplicate calls with identical
/// arguments may be CSE'd. Computed as a least fixpoint over the call graph so
/// that recursion and mutual recursion are handled correctly.
pub fn pure_functions(program: &Program) -> HashSet<Function> {
    let all: Vec<Function> = program.function_layout().to_vec();
    // Start from the locally-pure candidates, then repeatedly remove any
    // function that calls a (transitively) impure function until stable.
    let mut pure: HashSet<Function> = all
        .iter()
        .copied()
        .filter(|&f| locally_pure(program, f))
        .collect();
    loop {
        let mut removed = false;
        let snapshot: Vec<Function> = pure.iter().copied().collect();
        for func in snapshot {
            let data = program.func_data(func);
            let calls_impure = data.layout().basicblocks().iter().any(|bb| {
                bb.insts()
                    .iter()
                    .any(|&inst| match data.inst_data(inst).kind() {
                        InstKind::Call(call) => !pure.contains(&call.callee()),
                        InstKind::TailCall(tail_call) => !pure.contains(&tail_call.callee()),
                        _ => false,
                    })
            });
            if calls_impure {
                pure.remove(&func);
                removed = true;
            }
        }
        if !removed {
            break;
        }
    }
    pure
}
