//! Memoize pure self-recursive functions (M68).
//!
//! A pure function `f(K, C)` where `C` is an additive accumulator is rewritten
//! into a memoized form `f_memo(K, C, cache, size)`. The cache stores, per
//! `K`, the residual `h(K) = f(K, 0)` together with a one-bit classification:
//! either `f(K, C) = C + h(K)` (leaf residual) or `f(K, C) = v(K)` (fixed,
//! independent of `C` — the "barrier" of h-1's `return 7`). A cache hit
//! returns the stored value directly; a miss runs the original body with its
//! self-recursion replaced by `f_memo` calls and then backfills the entry.
//!
//! The h-1 hotspot is `fun(n, dep)`: a single-path chain where `n` only ever
//! increases (guarded) or halves, so every `n` in `[1, lim]` appears as a
//! distinct node. Naive recursion visits ~3.16G nodes; memoizing the `n`-keyed
//! residual collapses the work to one call per `n` (50M) plus one chain-step
//! per miss (~33M) — a 63x reduction (measured: QEMU 22.79s -> ~3.7s).
//!
//! # Trigger (structural, input-independent)
//!
//! 1. `f` is pure self-recursive: no stores, no calls other than `f` itself,
//!    loads only from globals, exactly two `i32` parameters.
//! 2. One parameter `C` is an additive accumulator: every use of `C` is
//!    `C + k` (with `k` not `C`-derived), a direct self-call argument at the
//!    accumulator position, or a returned `C`. Non-accumulator arguments of
//!    every self-call are `C`-independent. Every self-call result is used
//!    only as a return value.
//! 3. The single external callsite lives in a natural loop whose header
//!    parameter (the callsite's key argument) is a forward induction variable
//!    compared against a loop-invariant bound. The cache size is `bound + 1`,
//!    allocated at runtime via the compiler-provided `soyo_calloc` builtin.
//!
//! Everything outside this shape is left untouched (宁漏勿错). The cache is a
//! packed `i32` array: `entry = (val << 2) | tag`, `tag ∈ {1, 2}` (1 = leaf
//! residual, 2 = fixed), `0` = empty. `val` must fit in the low 30 bits; a
//! runtime guard skips caching a value that does not. Out-of-bounds `K` and
//! out-of-range residuals degrade to the uncached computation (correctness is
//! preserved for every input).
//!
//! ---
//!
//! ## 补充说明（中文）
//!
//! 术语：记忆化（memoization）/ 缓存 / 累加器（accumulator）/ 自递归
//! （self-recursion）/ 运行时大小缓存 等见 `docs/offline-handbook/glossary.md`
//! 的"优化与 pass 概念"分组，本模块只给最小解释。
//!
//! ### 一句话定位
//!
//! **M68**：把「纯自递归 + 加性累加器」形态的函数 `f(K, C)` 改写成记忆化版本
//! `f_memo(K, C, cache, size)`——命中直接返回、未命中跑原函数体并回填缓存。
//! 动机是 h-1 家族热点 `fun(n, dep)`：单链递归（`n` 只增或减半，`[1, lim]`
//! 每个 `n` 都是独立节点），朴素递归访问约 3.16G 节点；按 `n` 记忆化残差后
//! 塌缩为每 `n` 一次调用（50M）+ 每次未命中一次链步（约 33M），约 63x
//! 加速（实测 QEMU 22.79s → 约 3.7s）。
//!
//! ### 变换形态
//!
//! 缓存按 `K` 存残差 `h(K) = f(K, 0)` 外加 1 bit 分类：
//!
//! - **leaf**：`f(K, C) = C + h(K)`（h-1 的 `fun(n, dep)` 叶返回 `dep`）；
//! - **fixed**：`f(K, C) = v(K)`，与 `C` 无关（h-1 的 `return 7` 屏障）。
//!
//! 缓存是打包 `i32` 数组：`entry = (val << 2) | tag`，`tag ∈ {1, 2}`
//! （`TAG_LEAF` = 1 = leaf 残差，`TAG_FIXED` = 2 = fixed 值），`0` = 空槽；
//! `val` 必须落在低 30 位（`val & FIT_MASK == 0`）才缓存，否则跳过回填
//! （正确性不依赖回填是否发生）。
//!
//! `f_memo` 结构（`build_memoized` 用 `BodyClonePlan` 克隆原函数体，新函数名
//! 为 `{原函数名}_memo`，形参 `(K, C, cache, size)`）：
//! 前导块 `entry`（`1 <= key < size` 越界检查）→ `memo_in_bounds`（取槽、
//! 算 tag）→ `memo_hit`（leaf → `C + (val >> 2)`，fixed → `val >> 2`）／
//! `memo_miss`（跳进克隆体）。`(K, cache, size)` 作为块参数穿线（thread）
//! 过每个克隆块；克隆体内的自调用重定向为 `f_memo`（多传 `cache, size`）；
//! 每个 `ret` 改为「受保护的 store 回填 + ret」（`emit_store`；`ReturnKind`
//! 三分类 Leaf / Fixed / Rec，`Rec` 的父残差由子节点缓存项派生）。
//!
//! ### 触发 / 放弃条件
//!
//! 全部是结构性、与输入无关的判定（`detect` → `detect_callsite`，任一不满足
//! 即放弃，宁漏勿错）：
//!
//! - **函数形态**（`detect` / `classify` / `derived_set`）：恰好两个 `i32`
//!   参数、返回 `i32`、有入口块；无副作用（无 store / `MemZero` / `Alloc`）、
//!   无外部调用、load 只来自全局；非入口块无块参数（保证 `derived_set` 精确）；
//!   每个自调用恰好两个实参、结果只作返回值；两参中一个必须是**加性累加器**
//!   `C`：`C` 的每个使用只能是 `C + k`（`k` 不依赖 `C`）、自调用的累加器
//!   实参、或原样返回；key 实参不依赖 `C`；返回值必须同时含 `Rec`（返回
//!   自调用结果）与 `Leaf`（`C` 派生值）两类。
//! - **调用点**（`detect_callsite` / `classify_bound`）：唯一外部调用点在自然
//!   循环中，key 实参是前向归纳变量（步长为正常数），header 终结符为
//!   `key <= bound` 或 `key < bound`；`bound` 只接受常量 / 全局 load /
//!   调用者入口块参数；循环体无 store、无其它调用（否则 `f` 读的全局与缓存
//!   结果可能跨迭代变化）；存在 preheader。
//! - **内置名保护**：源码里存在带入口块、名为 `CALLOO_NAME`（`soyo_calloc`）
//!   的函数时整个 pass 放弃，避免遮蔽编译器提供的分配器
//!   （`find_or_declare_calloc` 只声明、不重复定义）。
//!
//! 缓存大小 = `bound + 1`（运行时才知道；`bound` 为负时 clamp 到 0），在
//! preheader 里经 `soyo_calloc(size, 4)` 分配（`rewrite_callsite`），并把
//! 调用点改写为 `f_memo(key, acc, cache, size)`。
//!
//! ### 正确性要点
//!
//! - 纯性由结构保证：`f` 无副作用、load 仅全局、循环体无 store / 无调用 →
//!   循环期间 `f` 的输入与读到的全局稳定，`f(K, C)` 只由 `(K, C)` 决定；
//! - 缓存不变量：槽内要么空（`0`），要么 `(h(K), tag)` 且满足
//!   `f(K, 0) = h(K)`；命中分支由 tag 恢复 `f(K, C)`（leaf → `C + h`，
//!   fixed → `v`），与定义一致；
//! - `Rec` 返回：父残差由子节点缓存项派生（`select(is_leaf, child_val + inc,
//!   child_val)`，`inc` 是递归累加器实参 `C' = C + inc` 的非 `C` 部分，
//!   `residual_of` 求取），回填附加 `ok` 守卫（子 `K` 在界内且子项非空）——
//!   子槽为空说明子调用走了降级路径，父项也不缓存；
//! - 降级路径：`K` 越界 / 值不 fit（`FIT_MASK`）/ 守卫失败 → 槽保持 `0`
//!   （`emit_store` 写回 `select(ok, packed, 0)`，失败即写 `0` 不变），下次
//!   探测仍 miss、重算原计算，任意输入结果与未记忆化时一致；
//! - 幂等：改写后原函数不再有外部调用点，第二轮 `run` 无候选
//!   （`the_pass_is_idempotent` 测试）。
//!
//! ### 管线位置与门控
//!
//! - 注册：`opt/pass.rs` 的 `PassesManager::from_config`，**initial 段**
//!   （`register_initial`），`specialize` / `mulmod_recognize` 之后、`inline`
//!   **之前**——必须先于 `inline`（inline 会把已冗余的原函数体拍平）；
//! - 门控：`config.memoize && config.target.enable_chain_to_switch`
//!   ——AArch64 专属（`soyo_calloc` 由 AArch64 后端展开为「零扩展两个 32 位
//!   实参后 tail-call glibc `calloc`」的内嵌汇编包装，零初始化让空槽免费），
//!   RISC-V 保持原递归；`config.rs` 的 `memoize` 默认开启，可 A/B 测量关闭。
//!
//! ### 验证
//!
//! - 本文件 `mod tests`（1054 行起）：识别 / 拒绝 / 改写 / 幂等共 6 个测试
//!   （`detects_the_accumulator_shape`、`rejects_an_accumulator_used_in_a_
//!   comparison`、`rewrites_the_recursion_and_the_callsite` 等）；
//! - 端到端：`make test` 差分比对（AArch64 `-O2`，性能对照
//!   `scripts/perf_compare.sh`）。
//!
use crate::{
    ir::{BasicBlock, BinaryOp, Function, Inst, InstKind, Program, Type, builder_trait::*},
    opt::{
        analysis_passes::{
            induction_variable::{BasicInductionVariableAnalysis, InductionStep},
            loop_analysis::LoopAnalysis,
        },
        pass::{ArenaContextMut, Pass},
        prelude::*,
        utils::body_clone::BodyClonePlan,
    },
};

/// Compiler-provided runtime cache allocator. The AArch64 backend lowers every
/// call to this declared function to an embedded assembly wrapper that
/// zero-extends its two 32-bit arguments and tail-calls glibc `calloc`
/// (zero-initialized, so tag `0` = empty for free). AArch64-only: the memoize
/// pass is gated behind `TargetPolicy::enable_chain_to_switch`.
pub const CALLOO_NAME: &str = "soyo_calloc";

/// The packed cache: `entry = (val << 2) | tag`.
const TAG_LEAF: i32 = 1;
const TAG_FIXED: i32 = 2;

/// Mask of the top two bits: a value fits the packed 30-bit field iff
/// `value & FIT_MASK == 0`.
const FIT_MASK: i32 = -0x40000000;

pub struct RecursiveMemoize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReturnKind {
    /// `ret v` with `v = C + k`: stores residual `k`.
    Leaf,
    /// `ret v` with `v` independent of `C`: fixed value.
    Fixed,
    /// `ret f(child, C')`: the parent's residual derives from the child's.
    Rec,
}

struct MemoInfo {
    func: Function,
    key_pos: usize,
    acc_pos: usize,
    /// Return classification in layout order (the clone preserves the order).
    kinds: Vec<ReturnKind>,
}

enum BoundKind {
    Const(i32),
    GlobalLoad(Inst),
    /// A caller entry-block parameter (available everywhere).
    Param(Inst),
}

struct CallsiteInfo {
    caller: Function,
    call: Inst,
    preheader: BasicBlock,
    bound: BoundKind,
}

impl Pass for RecursiveMemoize {
    fn run(&mut self, program: &mut Program) -> bool {
        // A source function that owns the builtin name must never be shadowed
        // by the compiler-provided allocator.
        if program.function_layout().iter().any(|&f| {
            program.func_data(f).name() == CALLOO_NAME
                && program.func_data(f).layout().entry_bb().is_some()
        }) {
            return false;
        }

        let candidates = program
            .function_layout()
            .iter()
            .copied()
            .filter_map(|f| {
                let memo = detect(program, f)?;
                let callsite = detect_callsite(program, f, memo.key_pos, memo.acc_pos)?;
                Some((memo, callsite))
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return false;
        }

        let calloc = find_or_declare_calloc(program);
        let mut changed = false;
        for (memo, callsite) in candidates {
            let Some(f_memo) = build_memoized(program, &memo) else {
                continue;
            };
            rewrite_callsite(
                program,
                &callsite,
                f_memo,
                memo.key_pos,
                memo.acc_pos,
                calloc,
            );
            changed = true;
        }
        changed
    }
}

fn find_or_declare_calloc(program: &mut Program) -> Function {
    for &f in program.function_layout() {
        if program.func_data(f).name() == CALLOO_NAME {
            return f;
        }
    }
    program.new_function(
        Type::get_pointer(Type::get_i32()),
        CALLOO_NAME.into(),
        vec![Type::get_i32(), Type::get_i32()],
    )
}

fn const_i32(data: &FunctionData, inst: Inst) -> Option<i32> {
    match data.inst_data(inst).kind() {
        InstKind::Integer(integer) => Some(integer.value()),
        _ => None,
    }
}

/// `C`-derived values: the fixpoint of `{C} ∪ {v : an operand of v is derived}`.
/// Only exact when no non-entry block has block parameters, which `detect`
/// enforces (after SSA, values then only flow through layout instructions).
fn derived_set(data: &FunctionData, acc: Inst) -> HashSet<Inst> {
    // "依赖累加参数 C 的值"闭包：C 本身，以及任一操作数已被标记的值。
    // 用途：区分"残差"（不依赖 C，如 ret C+k 里的 k）与 C 派生的值——
    // 只有 C 派生值才是残差/增量的一部分。SSA 后值只经布局指令流动，
    // 且 detect 已保证非 entry 块无块参数，故这个闭包是精确的（而非近似）。
    let mut derived: HashSet<Inst> = HashSet::from_iter([acc]);
    loop {
        let mut changed = false;
        for layout in data.layout().basicblocks() {
            for &inst in layout.insts() {
                if derived.contains(&inst) {
                    continue;
                }
                if data
                    .inst_data(inst)
                    .inst_usage()
                    .any(|operand| derived.contains(&operand))
                {
                    derived.insert(inst);
                    changed = true;
                }
            }
        }
        if !changed {
            return derived;
        }
    }
}

fn detect(program: &Program, f: Function) -> Option<MemoInfo> {
    let data = program.func_data(f);
    if data.layout().entry_bb().is_none() || !data.ret_ty().is_i32() {
        return None;
    }
    let params = data.params().to_vec();
    if params.len() != 2 || params.iter().any(|p| !data.inst_data(*p).ty().is_i32()) {
        return None;
    }

    // 结构性识别（M68）：函数必须恰有两个 i32 参数（key 与累加参数 C），
    // 只调用自己、无外部调用、无 Store/MemZero/Alloc 副作用，Load 只读
    // 全局（全局在循环内不变，缓存结果才有效）。这些都是纯结构判定，
    // 不匹配函数名/输入值（合规要求）。
    let mut self_calls = Vec::new();
    let mut returns = Vec::new();
    let mut has_foreign_call = false;
    let mut has_side_effect = false;
    for layout in data.layout().basicblocks() {
        for &inst in layout.insts() {
            match data.inst_data(inst).kind() {
                InstKind::Call(call) => {
                    if call.callee() == f {
                        self_calls.push(inst);
                    } else {
                        has_foreign_call = true;
                    }
                }
                InstKind::TailCall(..) => has_foreign_call = true,
                InstKind::Return(ret) => returns.push((inst, ret.value())),
                InstKind::Store(_) | InstKind::MemZero(_) | InstKind::Alloc => {
                    has_side_effect = true
                }
                InstKind::Load(load) => {
                    if !load.src().is_global() {
                        return None;
                    }
                }
                _ => {}
            }
        }
    }
    if self_calls.is_empty() || has_foreign_call || has_side_effect {
        return None;
    }

    // Every self-call must have exactly two arguments and its result must be
    // used only as a return value.
    // 每个自调用的实参必须恰好 2 个、结果只被 Return 消费（递归调用的
    // 结果直接作为返回值，无其他使用点）；非 entry 块必须无块参数
    // （保证 derived_set 闭包精确）。随后对两个参数位置各试一次
    // （key, acc）与（acc, key）的分配，找到能通过 classify 的组合。
    for &call in &self_calls {
        let call_data = data.inst_data(call);
        let InstKind::Call(call_kind) = call_data.kind() else {
            unreachable!()
        };
        if call_kind.args().len() != 2 || call_data.used_by().is_empty() {
            return None;
        }
        if call_data
            .used_by()
            .iter()
            .any(|&user| !matches!(data.inst_data(user).kind(), InstKind::Return(_)))
        {
            return None;
        }
    }

    // Non-entry blocks must be parameterless so `derived_set` is exact.
    let entry = data.layout().entry_bb().unwrap().bb();
    if data
        .layout()
        .basicblocks()
        .iter()
        .any(|layout| layout.bb() != entry && !data.bb_data(layout.bb()).params().is_empty())
    {
        return None;
    }

    for acc_pos in 0..2 {
        let key_pos = 1 - acc_pos;
        if let Some(kinds) = classify(program, f, &params, key_pos, acc_pos, &self_calls, &returns)
        {
            return Some(MemoInfo {
                func: f,
                key_pos,
                acc_pos,
                kinds,
            });
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn classify(
    program: &Program,
    f: Function,
    params: &[Inst],
    key_pos: usize,
    acc_pos: usize,
    self_calls: &[Inst],
    returns: &[(Inst, Option<Inst>)],
) -> Option<Vec<ReturnKind>> {
    let data = program.func_data(f);
    let acc = params[acc_pos];
    let derived = derived_set(data, acc);

    // 自调用实参校验：累加槽必须是 `C` 或 `C + k`（k 不依赖 C——即
    // 递归步的增量是"常数"而非 C 的函数）；key 槽必须与 C 无关。
    // 这保证记忆化的键（key）与增量（inc）在结构上可分离。
    // Self-call arguments: the accumulator slot is `C` or `C + k` with `k`
    // not `C`-derived; the key slot is `C`-independent.
    for &call in self_calls {
        let InstKind::Call(call_kind) = data.inst_data(call).kind() else {
            unreachable!()
        };
        let args = call_kind.args();
        let acc_arg = args[acc_pos];
        let acc_ok = acc_arg == acc
            || matches!(
                data.inst_data(acc_arg).kind(),
                InstKind::Binary(bin)
                    if bin.op() == BinaryOp::Add
                        && ((bin.lhs() == acc && !derived.contains(&bin.rhs()))
                            || (bin.rhs() == acc && !derived.contains(&bin.lhs())))
            );
        if !acc_ok || derived.contains(&args[key_pos]) {
            return None;
        }
    }

    // Every use of `C` must be a self-call accumulator argument equal to `C`,
    // a returned `C`, or an `Add(C, k)` whose result is itself only used as a
    // self-call accumulator argument or a return value.
    for &user in data.inst_data(acc).used_by() {
        let ok = match data.inst_data(user).kind() {
            InstKind::Call(call) => call.callee() == f && call.args()[acc_pos] == acc,
            InstKind::Return(ret) => ret.value() == Some(acc),
            InstKind::Binary(bin) => {
                let is_add = bin.op() == BinaryOp::Add
                    && ((bin.lhs() == acc && !derived.contains(&bin.rhs()))
                        || (bin.rhs() == acc && !derived.contains(&bin.lhs())));
                is_add
                    && data.inst_data(user).used_by().iter().all(|&u| {
                        match data.inst_data(u).kind() {
                            InstKind::Call(call) => {
                                call.callee() == f && call.args()[acc_pos] == user
                            }
                            InstKind::Return(ret) => ret.value() == Some(user),
                            _ => false,
                        }
                    })
            }
            _ => false,
        };
        if !ok {
            return None;
        }
    }

    // 返回值分类（ReturnKind）：rec 直接返回自调用结果（父残差由子残差
    // 递推）；leaf 返回 C 派生值（残差 = C + k 里的 k，可缓存为"叶残差"）；
    // fixed 返回与 C 无关的常量（固定值，如屏障语义的常量 7）。
    // 必须同时出现 rec 与 leaf：累加参数既要驱动递归又要落到叶上，
    // 否则缓存语义不成立（没有残差可存/没有增量可加）。
    // Classify returns.
    let mut kinds = Vec::with_capacity(returns.len());
    let mut has_rec = false;
    let mut uses_acc = false;
    for (_, value) in returns {
        let value = (*value)?;
        if matches!(
            data.inst_data(value).kind(),
            InstKind::Call(call) if call.callee() == f
        ) {
            kinds.push(ReturnKind::Rec);
            has_rec = true;
        } else if derived.contains(&value) {
            kinds.push(ReturnKind::Leaf);
            uses_acc = true;
        } else {
            kinds.push(ReturnKind::Fixed);
        }
    }
    // The accumulator must genuinely drive the recursion or a leaf.
    if !has_rec || !uses_acc {
        return None;
    }
    Some(kinds)
}

fn detect_callsite(
    program: &Program,
    f: Function,
    key_pos: usize,
    acc_pos: usize,
) -> Option<CallsiteInfo> {
    let mut callsite: Option<(Function, Inst, BasicBlock)> = None;
    for &caller in program.function_layout() {
        if caller == f {
            continue;
        }
        let data = program.func_data(caller);
        for layout in data.layout().basicblocks() {
            for &inst in layout.insts() {
                if let InstKind::Call(call) = data.inst_data(inst).kind() {
                    if call.callee() == f {
                        if callsite.is_some() {
                            return None;
                        }
                        callsite = Some((caller, inst, layout.bb()));
                    }
                }
            }
        }
    }
    let (caller, call, call_bb) = callsite?;
    let data = program.func_data(caller);
    let InstKind::Call(call_kind) = data.inst_data(call).kind() else {
        return None;
    };
    if call_kind.args().len() != 2 {
        return None;
    }
    let key_arg = call_kind.args()[key_pos];
    let acc_arg = call_kind.args()[acc_pos];
    if !data.inst_data(key_arg).ty().is_i32() || !data.inst_data(acc_arg).ty().is_i32() {
        return None;
    }

    let (cfg, _dom, loops) = LoopAnalysis::new(data);
    // 调用点必须在一个循环内，且 key 实参是该循环的**前向 IV**（步长
    // 为正的 Add）——循环每轮调用 f 的 key 单调递增，保证每个 key 在
    // 循环内只出现一次，记忆化才有意义（同 key 不重复计算）。
    let loop_index = loops.min_loop_contain_index(call_bb)?;
    let looop = &loops.loops()[loop_index];
    let ivs = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
    let iv = ivs.find(looop, key_arg)?;
    let step = match iv.step() {
        InductionStep::Add(step) => const_i32(data, step)?,
        InductionStep::Sub(_) => return None,
    };
    if step <= 0 {
        return None;
    }

    let header = looop.header();
    let terminator = data.layout().basicblock(header).terminator();
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
    if !compare.op().is_compare() {
        return None;
    }
    let (op, bound) = if compare.lhs() == key_arg {
        (compare.op(), compare.rhs())
    } else if compare.rhs() == key_arg {
        (compare.op().swap_compare_args()?, compare.lhs())
    } else {
        return None;
    };
    let mut op = op;
    if !true_inside {
        op = op.complement_integer_compare()?;
    }
    // Forward bounded induction: `key_arg <= bound` (or `< bound`).
    if op != BinaryOp::Le && op != BinaryOp::Lt {
        return None;
    }
    let bound = classify_bound(data, bound)?;

    // 循环体必须无 Store/MemZero/其他调用：否则 f 读的全局（及已缓存
    // 的结果）可能在迭代间变化，缓存失效。这是记忆化正确性的关键约束。
    // The loop body must be free of stores and of other calls, otherwise the
    // globals `f` reads (and the memoized results) could change across
    // iterations.
    for block in looop.body() {
        for &inst in data.layout().basicblock(*block).insts() {
            if inst == call {
                continue;
            }
            match data.inst_data(inst).kind() {
                InstKind::Store(_)
                | InstKind::MemZero(_)
                | InstKind::Call(_)
                | InstKind::TailCall(_) => return None,
                _ => {}
            }
        }
    }

    let preheader = looop.get_preheader(&cfg)?;
    Some(CallsiteInfo {
        caller,
        call,
        preheader,
        bound,
    })
}

fn classify_bound(data: &FunctionData, bound: Inst) -> Option<BoundKind> {
    match data.inst_data(bound).kind() {
        InstKind::Integer(integer) => Some(BoundKind::Const(integer.value())),
        InstKind::Load(load) if load.src().is_global() => Some(BoundKind::GlobalLoad(load.src())),
        InstKind::BlockArgRef(_) if data.params().contains(&bound) => Some(BoundKind::Param(bound)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Rewrite
// ---------------------------------------------------------------------------

fn build_memoized(program: &mut Program, memo: &MemoInfo) -> Option<Function> {
    let plan = BodyClonePlan::capture(program, memo.func).ok()?;
    let key_ty = Type::get_i32();
    let acc_ty = Type::get_i32();
    let cache_ty = Type::get_pointer(Type::get_i32());
    let name = program.func_data(memo.func).name().to_owned();
    // 新函数签名：f_memo(K, C, cache, size)。key（K）与累加参数（C）之外
    // 多了缓存指针与缓存大小——调用方在 preheader 里 calloc 分配后传入。
    // 缓存是运行时按 bound 推导的尺寸，不写死任何常量（合规要求）。
    let f_memo = program.new_function(
        Type::get_i32(),
        format!("{name}_memo"),
        vec![
            key_ty.clone(),
            acc_ty.clone(),
            cache_ty.clone(),
            Type::get_i32(),
        ],
    );

    // Scope A: build the prologue (entry / in_bounds / hit / miss).
    // Prologue 四块：entry 做 1<=K<size 边界检查；in_bounds 读打包条目
    // 算 tag；hit 按 tag 分支（LEAF→C+val / FIXED→val）；miss 落回克隆体。
    // 边界检查先行：OOB 的 K 直接走 miss（值仍正确计算，只是不缓存）。
    let miss_block = {
        let mut builder = ArenaContextMut {
            program,
            curr_func: Some(f_memo),
        };
        let param_tys = vec![
            key_ty.clone(),
            acc_ty.clone(),
            cache_ty.clone(),
            Type::get_i32(),
        ];
        let entry = builder
            .new_basic_block()
            .basic_block("entry".into(), param_tys.clone());
        let in_bounds_block = builder
            .new_basic_block()
            .basic_block("memo_in_bounds".into(), param_tys.clone());
        let hit_block = builder.new_basic_block().basic_block(
            "memo_hit".into(),
            vec![
                key_ty,
                acc_ty,
                cache_ty.clone(),
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let miss_block = builder
            .new_basic_block()
            .basic_block("memo_miss".into(), param_tys);
        for block in [entry, in_bounds_block, hit_block, miss_block] {
            builder.layout_mut().push_bb_back(block);
        }
        let (key, acc, cache, size) = (
            builder.bb_data(entry).params()[0],
            builder.bb_data(entry).params()[1],
            builder.bb_data(entry).params()[2],
            builder.bb_data(entry).params()[3],
        );
        builder.set_params(vec![key, acc, cache, size]);

        // entry: in_bounds = (key >= 1) && (key < size); probe the cache if
        // in bounds, otherwise fall straight to the miss path.
        let one = builder.new_local_inst().integer(1);
        let ge = builder.new_local_inst().binary(BinaryOp::Ge, key, one);
        let lt = builder.new_local_inst().binary(BinaryOp::Lt, key, size);
        let in_bounds = builder.new_local_inst().binary(BinaryOp::And, ge, lt);
        let entry_branch = builder.new_local_inst().branch(
            in_bounds,
            in_bounds_block,
            vec![key, acc, cache, size],
            miss_block,
            vec![key, acc, cache, size],
        );
        for inst in [ge, lt, in_bounds, entry_branch] {
            builder.layout_mut().insert_inst(entry, inst);
        }

        // in_bounds_block: load the packed entry, compute the tag, branch.
        let (ib_key, ib_acc, ib_cache, ib_size) = (
            builder.bb_data(in_bounds_block).params()[0],
            builder.bb_data(in_bounds_block).params()[1],
            builder.bb_data(in_bounds_block).params()[2],
            builder.bb_data(in_bounds_block).params()[3],
        );
        let three = builder.new_local_inst().integer(3);
        let k_ptr = builder
            .new_local_inst()
            .get_elem_ptr(ib_cache, vec![ib_key]);
        let entry_val = builder.new_local_inst().load(k_ptr);
        let tag = builder
            .new_local_inst()
            .binary(BinaryOp::And, entry_val, three);
        let zero = builder.new_local_inst().integer(0);
        let hit = builder.new_local_inst().binary(BinaryOp::NotEq, tag, zero);
        let ib_branch = builder.new_local_inst().branch(
            hit,
            hit_block,
            vec![ib_key, ib_acc, ib_cache, ib_size, entry_val, tag],
            miss_block,
            vec![ib_key, ib_acc, ib_cache, ib_size],
        );
        for inst in [k_ptr, entry_val, tag, hit, ib_branch] {
            builder.layout_mut().insert_inst(in_bounds_block, inst);
        }

        // hit_block: tag is 1 (leaf) -> C + val, or 2 (fixed) -> val.
        // 命中分支：val = 打包值 >> 2（右移 2 等价去 tag）；tag==1（叶残差）
        // 时结果 = C + val（残差累加回当前 C），tag==2（固定值）时结果 = val。
        // 用 select 一次选出，避免再开一个分支块。
        let (_, h_acc, _, _, h_val, h_tag) = (
            builder.bb_data(hit_block).params()[0],
            builder.bb_data(hit_block).params()[1],
            builder.bb_data(hit_block).params()[2],
            builder.bb_data(hit_block).params()[3],
            builder.bb_data(hit_block).params()[4],
            builder.bb_data(hit_block).params()[5],
        );
        let two = builder.new_local_inst().integer(2);
        let val = builder.new_local_inst().binary(BinaryOp::Shr, h_val, two);
        let is_leaf = builder.new_local_inst().binary(BinaryOp::Eq, h_tag, one);
        let c_plus = builder.new_local_inst().binary(BinaryOp::Add, h_acc, val);
        let res = builder.new_local_inst().select(is_leaf, c_plus, val);
        let h_ret = builder.new_local_inst().ret(Some(res));
        for inst in [val, is_leaf, c_plus, res, h_ret] {
            builder.layout_mut().insert_inst(hit_block, inst);
        }
        miss_block
    };

    let cloned = plan.clone_into(program, f_memo, miss_block).ok()?;

    // Scope B: thread (K, cache, size) through every cloned block, redirect
    // self-calls, and turn each return into a guarded store + ret. The cloned
    // entry's own `C` parameter dominates the whole body, so the leaf residual
    // and recursion increment are derived structurally from it (no `C`
    // threading).
    // 第二阶段：把 (K, cache, size) 三个值穿线（thread）进克隆体的每个
    // 块参数——克隆体是从原函数复制来的，原本不知道缓存的存在，所有
    // 跳转/分支边都要补上这三个实参，递归调用也要重定向到 f_memo 并
    // 传缓存。累加参数 C 不穿线：克隆入口的 C 参数支配整个函数体，
    // 叶残差与递归增量都从它结构化推导（见 residual_of）。
    let mut builder = ArenaContextMut {
        program,
        curr_func: Some(f_memo),
    };

    let mut threaded: HashMap<BasicBlock, (Inst, Inst, Inst)> = HashMap::default();
    for &block in &cloned.blocks {
        let k_p = builder.new_basic_block().add_param(block, Type::get_i32());
        let cache_p = builder.new_basic_block().add_param(block, cache_ty.clone());
        let size_p = builder.new_basic_block().add_param(block, Type::get_i32());
        threaded.insert(block, (k_p, cache_p, size_p));
    }

    // The miss block jumps into the cloned entry, forwarding the invocation's
    // (K, C) plus the threaded (K, cache, size).
    let (m_key, m_acc, m_cache, m_size) = (
        builder.bb_data(miss_block).params()[0],
        builder.bb_data(miss_block).params()[1],
        builder.bb_data(miss_block).params()[2],
        builder.bb_data(miss_block).params()[3],
    );
    let miss_jump = builder
        .new_local_inst()
        .jump(cloned.entry, vec![m_key, m_acc, m_key, m_cache, m_size]);
    builder.layout_mut().insert_inst(miss_block, miss_jump);

    // Append (K, cache, size) to every outgoing logical edge.
    enum TerminatorData {
        Jump(BasicBlock, Vec<Inst>),
        Branch(Inst, BasicBlock, Vec<Inst>, BasicBlock, Vec<Inst>),
    }
    for &block in &cloned.blocks {
        let (k_p, cache_p, size_p) = threaded[&block];
        let terminator = builder.layout().basicblock(block).terminator();
        let data = match builder.inst_data(terminator).kind() {
            InstKind::Jump(jump) => TerminatorData::Jump(jump.target(), jump.args().to_vec()),
            InstKind::Branch(branch) => TerminatorData::Branch(
                branch.cond(),
                branch.t_target(),
                branch.t_args().to_vec(),
                branch.f_target(),
                branch.f_args().to_vec(),
            ),
            _ => continue,
        };
        match data {
            TerminatorData::Jump(target, mut args) => {
                args.extend([k_p, cache_p, size_p]);
                builder.replace_inst_with(terminator).jump(target, args);
            }
            TerminatorData::Branch(cond, t_target, mut t_args, f_target, mut f_args) => {
                t_args.extend([k_p, cache_p, size_p]);
                f_args.extend([k_p, cache_p, size_p]);
                builder
                    .replace_inst_with(terminator)
                    .branch(cond, t_target, t_args, f_target, f_args);
            }
        }
    }

    // Redirect every cloned self-call to f_memo with the threaded cache args.
    for &block in &cloned.blocks {
        let (_, cache_p, size_p) = threaded[&block];
        let insts: Vec<Inst> = builder
            .layout()
            .basicblock(block)
            .insts()
            .iter()
            .copied()
            .collect();
        for inst in insts {
            let InstKind::Call(call) = builder.inst_data(inst).kind() else {
                continue;
            };
            if call.callee() != memo.func {
                continue;
            }
            let mut args = call.args().to_vec();
            args.push(cache_p);
            args.push(size_p);
            builder
                .replace_inst_with(inst)
                .call_with_type(f_memo, args, Type::get_i32());
        }
    }

    // The cloned entry's accumulator parameter, from which the leaf residual
    // and recursion increment are derived. It dominates every block.
    let acc_param = builder.bb_data(cloned.entry).params()[memo.acc_pos];

    // Transform every cloned return into a guarded store followed by the ret.
    // 每个克隆的 return 改写为：缓存回填（带守卫）+ 原 ret。
    //   Leaf:  存残差 k（值 = C + k 中不依赖 C 的部分），tag=LEAF；
    //   Fixed: 存固定值，tag=FIXED；
    //   Rec:   递归返回路径——先读子节点缓存条目，若子节点是 LEAF，
    //          新值 = 子残差 + 本步增量 inc（inc 从递归实参 C' = C + inc
    //          结构化推导），tag 继承子节点；子节点 OOB/未命中/宽度越界
    //          （extra_ok=false）则不回填，值仍正确返回。
    for (i, &ret) in cloned.returns.iter().enumerate() {
        let kind = memo.kinds[i];
        let block = builder.layout().parent_bb(ret).expect("return is laid out");
        let (k_p, cache_p, size_p) = threaded[&block];
        let value = match builder.inst_data(ret).kind() {
            InstKind::Return(return_kind) => return_kind.value(),
            _ => unreachable!(),
        };

        let (cache_val, tag, extra_ok) = match kind {
            ReturnKind::Leaf => {
                let value = value.unwrap();
                // Residual: `C + k` with `k` not `C`-derived.
                let res = residual_of(&mut builder, acc_param, value);
                insert_fresh(&mut builder, block, res);
                (res, builder.new_local_inst().integer(TAG_LEAF), None)
            }
            ReturnKind::Fixed => {
                let value = value.unwrap();
                (value, builder.new_local_inst().integer(TAG_FIXED), None)
            }
            ReturnKind::Rec => {
                let value = value.unwrap();
                let InstKind::Call(call) = builder.inst_data(value).kind() else {
                    unreachable!("recursion return value must be the call result");
                };
                let child = call.args()[0];
                let c_prime = call.args()[1];
                let zero = builder.new_local_inst().integer(0);
                let one_i = builder.new_local_inst().integer(1);
                let two = builder.new_local_inst().integer(2);
                let three = builder.new_local_inst().integer(3);
                let c_ge = builder.new_local_inst().binary(BinaryOp::Ge, child, one_i);
                let c_lt = builder.new_local_inst().binary(BinaryOp::Lt, child, size_p);
                let child_ib = builder.new_local_inst().binary(BinaryOp::And, c_ge, c_lt);
                let child_ptr = builder.new_local_inst().get_elem_ptr(cache_p, vec![child]);
                let child_entry = builder.new_local_inst().load(child_ptr);
                let child_ne0 = builder
                    .new_local_inst()
                    .binary(BinaryOp::NotEq, child_entry, zero);
                let child_tag = builder
                    .new_local_inst()
                    .binary(BinaryOp::And, child_entry, three);
                let child_val = builder
                    .new_local_inst()
                    .binary(BinaryOp::Shr, child_entry, two);
                // Increment: the non-`C` part of the recursion's accumulator
                // argument `C' = C + inc`.
                let inc = residual_of(&mut builder, acc_param, c_prime);
                let is_leaf = builder
                    .new_local_inst()
                    .binary(BinaryOp::Eq, child_tag, one_i);
                let bumped = builder
                    .new_local_inst()
                    .binary(BinaryOp::Add, child_val, inc);
                let newval = builder.new_local_inst().select(is_leaf, bumped, child_val);
                let ok = builder
                    .new_local_inst()
                    .binary(BinaryOp::And, child_ib, child_ne0);
                // `inc` may be a constant or an already-laid-out operand.
                insert_fresh(&mut builder, block, inc);
                for inst in [
                    c_ge,
                    c_lt,
                    child_ib,
                    child_ptr,
                    child_entry,
                    child_ne0,
                    child_tag,
                    child_val,
                    is_leaf,
                    bumped,
                    newval,
                    ok,
                ] {
                    builder.layout_mut().insert_before_terminator(block, inst);
                }
                (newval, child_tag, Some(ok))
            }
        };

        emit_store(
            &mut builder,
            block,
            k_p,
            cache_p,
            size_p,
            cache_val,
            tag,
            extra_ok,
        );
    }

    Some(f_memo)
}

/// The residual `k` of an accumulator expression `C + k` (or `k + C`): the
/// operand that does not depend on `C`. Returns `0` for a bare `C`. The
/// defensive fallback recomputes `value - C`, which is exact because `C` is
/// the cloned entry's accumulator parameter dominating every block.
fn residual_of(builder: &mut ArenaContextMut<'_>, acc: Inst, value: Inst) -> Inst {
    // 残差提取：C + k 中"不是 C 的那一半"。直接形态是 Add(C, k)/Add(k, C)
    // 返回另一半；裸 C 返回 0；兜底用 value - C 重算（C 支配全函数体，
    // 减法精确）。防御性兜底保证识别不依赖前端恰好生成 Add 形态。
    if value == acc {
        return builder.new_local_inst().integer(0);
    }
    match builder.inst_data(value).kind() {
        InstKind::Binary(bin) if bin.op() == BinaryOp::Add => {
            if bin.lhs() == acc {
                return bin.rhs();
            }
            if bin.rhs() == acc {
                return bin.lhs();
            }
        }
        _ => {}
    }
    builder.new_local_inst().binary(BinaryOp::Sub, value, acc)
}

/// Insert `inst` into `block` only when it is a freshly created, non-constant
/// value. Constants (integers) and values already owned by a layout block must
/// stay out of the block layout.
fn insert_fresh(builder: &mut ArenaContextMut<'_>, block: BasicBlock, inst: Inst) {
    if builder.inst_data(inst).kind().is_const() {
        return;
    }
    if builder.layout().parent_bb(inst).is_some() {
        return;
    }
    builder.layout_mut().insert_before_terminator(block, inst);
}

fn emit_store(
    builder: &mut ArenaContextMut<'_>,
    block: BasicBlock,
    k: Inst,
    cache: Inst,
    size: Inst,
    val: Inst,
    tag: Inst,
    extra_ok: Option<Inst>,
) {
    // 回填守卫（emit_store 的全部逻辑）：只有 1<=K<size 且 val 能装进
    // 30 位（FIT_MASK 检查，val 需 < 2^30）才写缓存；rec 路径额外要求
    // 子节点条目有效（extra_ok）。守卫失败存 0（= 空条目，等价于不缓存）。
    // 正确性不受影响：不缓存只是下次重算；OOB/越界全部退化为原逻辑。
    let zero = builder.new_local_inst().integer(0);
    let one = builder.new_local_inst().integer(1);
    let two = builder.new_local_inst().integer(2);
    let mask = builder.new_local_inst().integer(FIT_MASK);
    let ge = builder.new_local_inst().binary(BinaryOp::Ge, k, one);
    let lt = builder.new_local_inst().binary(BinaryOp::Lt, k, size);
    let k_ib = builder.new_local_inst().binary(BinaryOp::And, ge, lt);
    // 打包：val << 2 | tag。低 2 位是 tag（1=叶残差 / 2=固定值），0=空。
    let shl = builder.new_local_inst().binary(BinaryOp::Shl, val, two);
    let packed = builder.new_local_inst().binary(BinaryOp::Or, shl, tag);
    let andv = builder.new_local_inst().binary(BinaryOp::And, val, mask);
    let fits = builder.new_local_inst().binary(BinaryOp::Eq, andv, zero);
    let mut ok = builder.new_local_inst().binary(BinaryOp::And, k_ib, fits);
    let mut to_insert = vec![ge, lt, k_ib, shl, packed, andv, fits, ok];
    if let Some(extra) = extra_ok {
        let combined = builder.new_local_inst().binary(BinaryOp::And, ok, extra);
        to_insert.push(combined);
        ok = combined;
    }
    let sel = builder.new_local_inst().select(ok, packed, zero);
    let ptr = builder.new_local_inst().get_elem_ptr(cache, vec![k]);
    let store = builder.new_local_inst().store(sel, ptr);
    to_insert.extend([sel, ptr, store]);
    for inst in to_insert {
        builder.layout_mut().insert_before_terminator(block, inst);
    }
}

fn rewrite_callsite(
    program: &mut Program,
    callsite: &CallsiteInfo,
    f_memo: Function,
    key_pos: usize,
    acc_pos: usize,
    calloc: Function,
) {
    let mut builder = ArenaContextMut {
        program,
        curr_func: Some(callsite.caller),
    };

    // 调用点改写：在调用方 preheader 里重新发射循环上界（bound 可能是
    // 常量/全局 load/调用方参数三种形态）并推导缓存尺寸 size = bound+1
    // （key 从 1 走到 bound，条目下标 1..=bound；size 需 ≥0，用 select
    // 钳制），然后 calloc(size, 4) 分配缓存，把调用换成
    // f_memo(key, acc, cache, size)。
    // Re-emit the loop bound in the preheader and derive the cache size.
    let bound = match &callsite.bound {
        BoundKind::Const(value) => builder.new_local_inst().integer(*value),
        BoundKind::GlobalLoad(global) => {
            // FunctionData's Arena cannot address global instructions; build
            // the re-emitted load through a LocalBuilder rooted at the
            // ArenaContextMut (which can).
            let pointee = builder.inst_data(*global).ty().derefernce();
            crate::ir::builder::LocalBuilder {
                arena: &mut builder,
            }
            .raw(crate::ir::inst_kind::Load::new_data(*global, pointee))
        }
        BoundKind::Param(param) => *param,
    };
    let zero = builder.new_local_inst().integer(0);
    let one = builder.new_local_inst().integer(1);
    let four = builder.new_local_inst().integer(4);
    let size_raw = builder.new_local_inst().binary(BinaryOp::Add, bound, one);
    let ge0 = builder
        .new_local_inst()
        .binary(BinaryOp::Ge, size_raw, zero);
    let size = builder.new_local_inst().select(ge0, size_raw, zero);
    let calloc_call = builder.new_local_inst().call_with_type(
        calloc,
        vec![size, four],
        Type::get_pointer(Type::get_i32()),
    );
    // `bound` is a re-emitted global load (laid out) or an inline operand
    // (constant / block parameter); only lay out the former, and always before
    // the size arithmetic that reads it.
    if matches!(callsite.bound, BoundKind::GlobalLoad(_)) {
        builder
            .layout_mut()
            .insert_before_terminator(callsite.preheader, bound);
    }
    for inst in [size_raw, ge0, size, calloc_call] {
        builder
            .layout_mut()
            .insert_before_terminator(callsite.preheader, inst);
    }

    // Rewrite the callsite to pass the cache pointer and size.
    let InstKind::Call(call_kind) = builder.inst_data(callsite.call).kind() else {
        return;
    };
    let key_arg = call_kind.args()[key_pos];
    let acc_arg = call_kind.args()[acc_pos];
    builder.replace_inst_with(callsite.call).call_with_type(
        f_memo,
        vec![key_arg, acc_arg, calloc_call, size],
        Type::get_i32(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Program, Type, builder_trait::*};
    use crate::opt::pass::Pass as _;

    /// Build the h-1-shaped recursion `fun(n, dep)`:
    /// `n == 1 -> dep`, otherwise `fun(n / 2, dep + 1)`.
    fn build_fun(program: &mut Program) -> Function {
        let f = program.new_function(
            Type::get_i32(),
            "fun".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        {
            let data = program.func_data_mut(f);
            let entry = data.add_entry_block();
            let leaf = data.new_basic_block().basic_block("leaf".into(), vec![]);
            let rec = data.new_basic_block().basic_block("rec".into(), vec![]);
            data.layout_mut().push_bb_back(leaf);
            data.layout_mut().push_bb_back(rec);
            let (n, dep) = (data.params()[0], data.params()[1]);
            let one = data.new_local_inst().integer(1);
            let eq = data.new_local_inst().binary(BinaryOp::Eq, n, one);
            let br = data.new_local_inst().branch(eq, leaf, vec![], rec, vec![]);
            data.layout_mut().insert_inst(entry, eq);
            data.layout_mut().insert_inst(entry, br);

            let ret_dep = data.new_local_inst().ret(Some(dep));
            data.layout_mut().insert_inst(leaf, ret_dep);

            let two = data.new_local_inst().integer(2);
            let half = data.new_local_inst().binary(BinaryOp::Div, n, two);
            let c1 = data.new_local_inst().binary(BinaryOp::Add, dep, one);
            let call = data
                .new_local_inst()
                .call_with_type(f, vec![half, c1], Type::get_i32());
            let ret_call = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(rec, half);
            data.layout_mut().insert_inst(rec, c1);
            data.layout_mut().insert_inst(rec, call);
            data.layout_mut().insert_inst(rec, ret_call);
        }
        f
    }

    /// Build `main(bound)`: a forward `i <= bound` loop calling `fun(i, 0)`.
    fn build_main(program: &mut Program, f: Function) {
        let main = program.new_function(Type::get_i32(), "main".into(), vec![Type::get_i32()]);
        {
            let data = program.func_data_mut(main);
            let entry = data.add_entry_block();
            let header = data
                .new_basic_block()
                .basic_block("header".into(), vec![Type::get_i32()]);
            let body = data.new_basic_block().basic_block("body".into(), vec![]);
            let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
            for block in [header, body, exit] {
                data.layout_mut().push_bb_back(block);
            }
            let bound = data.params()[0];
            let zero = data.new_local_inst().integer(0);
            let entry_jump = data.new_local_inst().jump(header, vec![zero]);
            data.layout_mut().insert_inst(entry, entry_jump);

            let iv = data.bb_data(header).params()[0];
            let le = data.new_local_inst().binary(BinaryOp::Le, iv, bound);
            let header_br = data.new_local_inst().branch(le, body, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, le);
            data.layout_mut().insert_inst(header, header_br);

            let zero_c = data.new_local_inst().integer(0);
            let call = data
                .new_local_inst()
                .call_with_type(f, vec![iv, zero_c], Type::get_i32());
            let one = data.new_local_inst().integer(1);
            let next = data.new_local_inst().binary(BinaryOp::Add, iv, one);
            let body_jump = data.new_local_inst().jump(header, vec![next]);
            data.layout_mut().insert_inst(body, call);
            data.layout_mut().insert_inst(body, one);
            data.layout_mut().insert_inst(body, next);
            data.layout_mut().insert_inst(body, body_jump);

            let ret = data.new_local_inst().ret(Some(zero));
            data.layout_mut().insert_inst(exit, ret);
        }
    }

    #[test]
    fn detects_the_accumulator_shape() {
        let mut program = Program::new();
        let f = build_fun(&mut program);
        let info = detect(&program, f).expect("fun(n, dep) must be recognized");
        assert_eq!(info.key_pos, 0);
        assert_eq!(info.acc_pos, 1);
    }

    #[test]
    fn detects_the_loop_callsite() {
        let mut program = Program::new();
        let f = build_fun(&mut program);
        build_main(&mut program, f);
        let info = detect(&program, f).unwrap();
        let callsite =
            detect_callsite(&program, f, info.key_pos, info.acc_pos).expect("loop callsite");
        assert_eq!(program.func_data(callsite.caller).name(), "main");
        assert!(matches!(callsite.bound, BoundKind::Param(_)));
    }

    #[test]
    fn rejects_an_accumulator_used_in_a_comparison() {
        let mut program = Program::new();
        let f = program.new_function(
            Type::get_i32(),
            "bad".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        {
            let data = program.func_data_mut(f);
            let entry = data.add_entry_block();
            let leaf = data.new_basic_block().basic_block("leaf".into(), vec![]);
            let rec = data.new_basic_block().basic_block("rec".into(), vec![]);
            data.layout_mut().push_bb_back(leaf);
            data.layout_mut().push_bb_back(rec);
            let (n, dep) = (data.params()[0], data.params()[1]);
            let one = data.new_local_inst().integer(1);
            // `dep < 0` compares the accumulator: not a pure accumulator.
            let zero = data.new_local_inst().integer(0);
            let cmp = data.new_local_inst().binary(BinaryOp::Lt, dep, zero);
            let br = data.new_local_inst().branch(cmp, leaf, vec![], rec, vec![]);
            data.layout_mut().insert_inst(entry, cmp);
            data.layout_mut().insert_inst(entry, br);
            let ret_dep = data.new_local_inst().ret(Some(dep));
            data.layout_mut().insert_inst(leaf, ret_dep);
            let two = data.new_local_inst().integer(2);
            let half = data.new_local_inst().binary(BinaryOp::Div, n, two);
            let c1 = data.new_local_inst().binary(BinaryOp::Add, dep, one);
            let call = data
                .new_local_inst()
                .call_with_type(f, vec![half, c1], Type::get_i32());
            let ret_call = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(rec, half);
            data.layout_mut().insert_inst(rec, c1);
            data.layout_mut().insert_inst(rec, call);
            data.layout_mut().insert_inst(rec, ret_call);
        }
        assert!(detect(&program, f).is_none());
    }

    #[test]
    fn rejects_when_the_key_argument_depends_on_the_accumulator() {
        let mut program = Program::new();
        let f = program.new_function(
            Type::get_i32(),
            "bad_key".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        {
            let data = program.func_data_mut(f);
            let entry = data.add_entry_block();
            let leaf = data.new_basic_block().basic_block("leaf".into(), vec![]);
            let rec = data.new_basic_block().basic_block("rec".into(), vec![]);
            data.layout_mut().push_bb_back(leaf);
            data.layout_mut().push_bb_back(rec);
            let (n, dep) = (data.params()[0], data.params()[1]);
            let one = data.new_local_inst().integer(1);
            let eq = data.new_local_inst().binary(BinaryOp::Eq, n, one);
            let br = data.new_local_inst().branch(eq, leaf, vec![], rec, vec![]);
            data.layout_mut().insert_inst(entry, eq);
            data.layout_mut().insert_inst(entry, br);
            let ret_dep = data.new_local_inst().ret(Some(dep));
            data.layout_mut().insert_inst(leaf, ret_dep);
            // Recursion key depends on `dep`: `fun(n + dep, dep + 1)`.
            let key = data.new_local_inst().binary(BinaryOp::Add, n, dep);
            let c1 = data.new_local_inst().binary(BinaryOp::Add, dep, one);
            let call = data
                .new_local_inst()
                .call_with_type(f, vec![key, c1], Type::get_i32());
            let ret_call = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(rec, key);
            data.layout_mut().insert_inst(rec, c1);
            data.layout_mut().insert_inst(rec, call);
            data.layout_mut().insert_inst(rec, ret_call);
        }
        assert!(detect(&program, f).is_none());
    }

    #[test]
    fn rewrites_the_recursion_and_the_callsite() {
        let mut program = Program::new();
        let f = build_fun(&mut program);
        build_main(&mut program, f);

        assert!(RecursiveMemoize.run(&mut program));

        // A memoized clone exists and the original callsite now targets it.
        let memo_func = program
            .function_layout()
            .iter()
            .copied()
            .find(|&g| program.func_data(g).name() == "fun_memo")
            .expect("fun_memo must be created");
        let mut calls_fun_memo = 0;
        let mut calls_original = 0;
        for &g in program.function_layout() {
            let data = program.func_data(g);
            for layout in data.layout().basicblocks() {
                for &inst in layout.insts() {
                    if let InstKind::Call(call) = data.inst_data(inst).kind() {
                        if call.callee() == memo_func {
                            calls_fun_memo += 1;
                        }
                        // Only the dead original may still recurse to itself.
                        if call.callee() == f && g != f {
                            calls_original += 1;
                        }
                    }
                }
            }
        }
        assert!(calls_fun_memo >= 2, "self-recursion plus the callsite");
        // No live caller references the original function.
        assert_eq!(calls_original, 0);
        // The allocator is declared.
        assert!(
            program
                .function_layout()
                .iter()
                .any(|&g| program.func_data(g).name() == CALLOO_NAME)
        );
    }

    #[test]
    fn the_pass_is_idempotent() {
        let mut program = Program::new();
        let f = build_fun(&mut program);
        build_main(&mut program, f);
        assert!(RecursiveMemoize.run(&mut program));
        // The original `fun` is no longer a candidate (no external callsite).
        let again = RecursiveMemoize.run(&mut program);
        assert!(!again);
    }
}
