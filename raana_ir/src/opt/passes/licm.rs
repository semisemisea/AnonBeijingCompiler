//! # LICM：循环不变代码外提
//!
//! 把循环内**每轮结果相同**的表达式移到 preheader，只算一次。经典 LICM 的
//! 保守实现 + 两个增强：**副作用感知**（`EffectAnalysis` 判定纯调用与
//! load 能否安全外提）与**外提规模限制**（地址计算型 load 批量外提会拉长
//! 活跃区间、引发 spill，见 `MAX_HOISTED_COMPUTED_LOADS`）。
//!
//! ## 变换形态（IR 示例）
//!
//! ```text
//! 前：                             后：
//! pre:  jump header(...)          pre:  t = a * 4            ← 外提
//! header(...):                          jump header(...)
//!   ...                             header(...):
//!   p = gep base, [i, a*4]            ...
//!   v = load p                        p = gep base, [i, t]    ← 引用外提值
//!   ...                               v = load p
//!                                    （p 的 GEP 若整条不变也可外提）
//! ```
//!
//! 外提的指令用 `substitute_header_params` 把 header/单前驱 body 参数替换成
//! preheader 边实参——外提指令落在 preheader，不能引用循环内定义的参数
//! （否则违反 SSA 支配；fft0/fft1 曾在此触发 VCode SSA 校验 panic）。
//!
//! ## 触发 / 放弃条件
//!
//! - 标量计算（Binary/Cast/Select/GEP）：操作数都是循环不变量
//!   （`Lattice::Invariant` 数据流标记，循环外定义/常量/全局/不随 IV 变）；
//! - `load`：仅当 `load_hoist_safe`——`EffectAnalysis` 证明循环内没有任何
//!   指令可能写该地址（按 `WriteRoot` 别名判定）；**直接标量 load** 保留
//!   资格，地址计算型 load 受限（`limit_computed_loads` 时 ≤ 8 条）；
//! - 纯 `call`（无副作用、无写内存）：可外提，实参需全部不变量；
//! - 放弃：副作用不确定、写内存可能命中、外提预算超限；无 `EffectAnalysis`
//!   的裸 `run_on` 调用（单测路径）不外提任何 load。
//!
//! ## 正确性
//!
//! - 外提指令每轮值相同（不变量）且无副作用 → 执行次数减少不改变语义；
//! - load 外提要求循环内无人写该地址（否则提前读会读到旧值）；
//! - 外提目标 preheader 支配循环内所有使用点，SSA 支配保持。
//!
//! ## 管线位置
//!
//! - 注册：`opt/pass.rs` 的 `from_config`，fixpoint 段，`chain_to_switch`
//!   之后、`gvn` 之前；
//! - 门控：`with_computed_load_limit(config.target.enable_chain_to_switch)`
//!   ——AArch64 限制地址计算型 load 外提，RISC-V 不限制；
//! - 依赖：`EffectAnalysis`（`analysis_passes/effects.rs`）在每次
//!   `Pass::run` 重建。
//!
//! ## 验证
//!
//! - 本文件 `mod tests` 覆盖不变量判定、load 安全性、参数替换与拒绝路径；
//! - 端到端：`make test` 差分比对。

use rustc_hash::{FxHashMap, FxHashSet};

use crate::opt::{
    analysis_passes::{
        dom_tree::v2::DominanceTree,
        effects::{AbstractObject, EffectAnalysis, WriteRoot},
        loop_analysis::Loop,
    },
    prelude::*,
    utils::{
        cfg::CFG,
        logical_edge::incoming_edges,
        preheader::{EnsurePreheader, ensure_preheader},
    },
};

/// Hoisting a large bank of address-computed loads extends all of their live
/// ranges across the loop and commonly costs more spill traffic than it saves.
/// Direct scalar loads remain eligible because they do not create the same
/// address/value register bank.
const MAX_HOISTED_COMPUTED_LOADS: usize = 8;

/// Loop invariant code motion
pub struct LICM {
    /// Whole-program purity / alias analysis, rebuilt on every `Pass::run`.
    /// Loads are only hoisted when no loop instruction may write the loaded
    /// address (see `load_hoist_safe`); without the analysis (direct
    /// `run_on` use) loads stay put.
    analysis: Option<EffectAnalysis>,
    limit_computed_loads: bool,
}

impl LICM {
    pub fn new() -> LICM {
        Self::with_computed_load_limit(true)
    }

    pub fn with_computed_load_limit(limit_computed_loads: bool) -> LICM {
        LICM {
            analysis: None,
            limit_computed_loads,
        }
    }
}

impl Default for LICM {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lattice {
    Variant,
    Invariant,
}

// 中文：不变性数据流格，仅两态：`Invariant`（循环不变量，每轮值相同）与
// `Variant`（每轮可能变化）。分析从全 `Variant` 出发，迭代到不动点后收敛。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopResult {
    Unchanged,
    Changed,
    CfgChanged,
}

// 中文：单次 `solve` 的结果：`Unchanged` 无事发生；`Changed` 有指令被外提；
// `CfgChanged` 表示外提需要新建 preheader（CFG 已变），外层须重建分析后重跑。

/// Rewrites `inst` in place, replacing loop-invariant header parameter operands
/// with their preheader edge arguments. Only the instruction kinds `LICM` hoists
/// can carry such operands.
fn substitute_header_params(
    data: &mut ArenaContextMut<'_>,
    inst: Inst,
    substitution: &FxHashMap<Inst, Inst>,
) {
    let substitute = |value: Inst| -> Inst { substitution.get(&value).copied().unwrap_or(value) };
    match data.inst_data(inst).kind().clone() {
        InstKind::GetElemPtr(gep) => {
            let base = substitute(gep.base());
            let offsets = gep
                .offsets()
                .iter()
                .map(|&offset| substitute(offset))
                .collect();
            data.replace_inst_with(inst).get_elem_ptr(base, offsets);
        }
        InstKind::Binary(binary) => {
            let lhs = substitute(binary.lhs());
            let rhs = substitute(binary.rhs());
            data.replace_inst_with(inst).binary(binary.op(), lhs, rhs);
        }
        InstKind::Cast(cast) => {
            let src = substitute(cast.src());
            let ty = data.inst_data(inst).ty().clone();
            data.replace_inst_with(inst).cast(src, ty);
        }
        InstKind::Select(select) => {
            let cond = substitute(select.cond());
            let if_true = substitute(select.if_true());
            let if_false = substitute(select.if_false());
            data.replace_inst_with(inst).select(cond, if_true, if_false);
        }
        // Hoisted pure calls must have their arguments rewritten too: a call
        // argument that is a block parameter (header or single-predecessor
        // body parameter) is only available inside the loop, while the hoisted
        // call lands in the preheader. Leaving the parameter in place made the
        // hoisted call use a value whose definition does not dominate it
        // (fft0/fft1/fft2 VCode SSA verification panic).
        InstKind::Call(call) => {
            let callee = call.callee();
            let args = call.args().iter().map(|&arg| substitute(arg)).collect();
            data.replace_inst_with(inst).call(callee, args);
        }
        InstKind::TailCall(tail) => {
            let callee = tail.callee();
            let args = tail.args().iter().map(|&arg| substitute(arg)).collect();
            data.replace_inst_with(inst).tail_call(callee, args);
        }
        InstKind::VectorSplat(splat) => {
            let src = substitute(splat.src());
            let ty = data.inst_data(inst).ty().clone();
            data.replace_inst_with(inst).vector_splat(src, ty);
        }
        _ => {}
    }
}

impl LICM {
    // 中文：LICM 主算法（solver）入口，对单个循环执行一轮"判定 + 改写"：
    //   1) 判定不变性：数据流格标记每条指令，纯调用/load 需 `EffectAnalysis` 背书；
    //   2) 筛选可外提指令：地址计算型 load 数量限制、GEP 部分外提候选；
    //   3) 改写：确保 preheader 存在，替换参数引用后把指令搬入 preheader。
    // 返回 `LoopResult` 汇报本轮效果；主文件 `Pass::run_on` 据其结果决定
    // 是否重建 CFG/支配/循环分析后重试。
    fn solve(
        looop: &Loop,
        analysis: &Option<EffectAnalysis>,
        data: &mut ArenaContextMut<'_>,
        cfg: &CFG,
        dom_tree: &DominanceTree,
        parameter_blocks: &FxHashMap<Inst, BasicBlock>,
        limit_computed_loads: bool,
    ) -> LoopResult {
        fn insts(looop: &Loop, data: &ArenaContextMut<'_>) -> impl Iterator<Item = Inst> {
            looop
                .body()
                .iter()
                .flat_map(|&bb| data.layout().basicblock(bb).insts())
                .copied()
        }

        // 中文：候选种类过滤：标量计算（整数/浮点/二元/转换/GEP/Select）与
        // `load` 天然可判不变；`call` 必须已进入 `hoistable_calls` 集合
        // （效果分析证明纯、且循环内无人写它读的对象）才有外提资格。
        fn can_be_invariant(
            kind: &InstKind,
            inst: Inst,
            hoistable_calls: &FxHashSet<Inst>,
        ) -> bool {
            matches!(
                kind,
                InstKind::Integer(..)
                    | InstKind::Float(..)
                    | InstKind::Binary(..)
                    | InstKind::Cast(..)
                    | InstKind::GetElemPtr(..)
                    | InstKind::Select(..)
                    | InstKind::Load(..)
                    | InstKind::VectorSplat(..)
            ) || (matches!(kind, InstKind::Call(..)) && hoistable_calls.contains(&inst))
        }

        fn is_integer_zero(data: &ArenaContextMut<'_>, inst: Inst) -> bool {
            matches!(data.inst_data(inst).kind(), InstKind::Integer(value) if value.value() == 0)
        }

        // 中文：循环体所有块（header + body）的参数集合。块参数没有定义指令，
        // 无法用"定义块支配 header"判定，需单独走回边 passthrough 与
        // 单前驱解析规则。
        let loop_params = looop
            .body()
            .iter()
            .flat_map(|&block| data.bb_data(block).params().iter().copied())
            .collect::<FxHashSet<_>>();

        // A header block parameter passed through unchanged on every backedge is
        // loop-invariant. When the loop has exactly one entry edge, hoisting can
        // substitute such a parameter with the entry edge's argument (which is
        // available in the preheader). With multiple entry edges the arguments
        // differ per edge, so hoisting operands that reference header params is
        // unsafe and they stay variant.
        // 中文：header 参数在每条回边都原样传回自己（passthrough）时，其值每轮
        // 不变（等于入口边实参）。但仅单入口边时该实参在 preheader 中才唯一
        // 可得；多入口边各边实参不同，引用这些参数的指令保持 `Variant`。
        let entry_edges = incoming_edges(data, cfg, looop.header())
            .into_iter()
            .filter(|edge| !looop.contains(edge.source()))
            .collect::<Vec<_>>();
        let backedges = incoming_edges(data, cfg, looop.header())
            .into_iter()
            .filter(|edge| looop.contains(edge.source()))
            .collect::<Vec<_>>();
        let header_params = data.bb_data(looop.header()).params().to_vec();
        let invariant_header_params = header_params
            .iter()
            .enumerate()
            .filter_map(|(position, &parameter)| {
                (entry_edges.len() == 1
                    && backedges
                        .iter()
                        .all(|edge| edge.args(data).get(position) == Some(&parameter)))
                .then_some(parameter)
            })
            .collect::<FxHashSet<_>>();
        // 中文：实际改写用的替换表：header 参数 → 入口边实参（值相同的项跳过）。
        // 仅单入口边时构造；多入口边为 `None`，外提时不做参数替换。
        let substitution = if let [edge] = entry_edges.as_slice() {
            let edge_args = edge.args(data);
            Some(
                header_params
                    .iter()
                    .zip(edge_args)
                    .filter(|(parameter, argument)| parameter != argument)
                    .map(|(parameter, argument)| (*parameter, *argument))
                    .collect::<FxHashMap<_, _>>(),
            )
        } else {
            None
        };

        // A block parameter whose block has a single logical incoming edge is
        // that edge's argument. Resolving such parameters (transitively, and
        // through the passthrough header-parameter substitution below)
        // exposes invariant computations that reference values forwarded
        // through single-predecessor chains — e.g. conv2d's inlined `idx`
        // address arithmetic, where the row index reaches the inner loop
        // through a chain of forwarding blocks.
        //
        // The seed contains only *passthrough* header parameters (whose
        // backedge passes the parameter itself): their entry-edge argument is
        // the parameter's value in every iteration and is available in the
        // preheader. Loop-carried header parameters (entry X, backedge Y)
        // must NOT resolve to their entry argument — that is how the c/r/kr/kc
        // induction variables of conv2d were once folded to their entry value
        // 0. They stay terminal values and are judged by `resolved_invariant`
        // (their defining block is inside the loop, so they remain variant).
        // 中文：把"单前驱转发链"上的块参数解析成最终值：body 块参数若只有一条
        // 逻辑入边，其值就是该边实参；实参仍是别的块参数则继续链式解析，
        // 环状依赖无法收敛时保持终值，由 `resolved_invariant` 按支配关系判定。
        // 种子只含 passthrough header 参数——回边携带循环值的 header 参数
        // （入口 X、回边 Y）绝不能解析成入口值（conv2d 的 c/kr/kc 曾因此被折成 0）。
        let mut param_resolutions: FxHashMap<Inst, Inst> = FxHashMap::default();
        if let Some(substitution) = &substitution {
            for &parameter in &invariant_header_params {
                if let Some(&arg) = substitution.get(&parameter) {
                    param_resolutions.insert(parameter, arg);
                }
            }
        }
        let mut deferred: Vec<(Inst, Inst)> = Vec::new();
        for &block in cfg.blocks() {
            let params = data.bb_data(block).params().to_vec();
            if params.is_empty() {
                continue;
            }
            let edges = incoming_edges(data, cfg, block);
            if edges.len() != 1 {
                continue; // real phis and function parameters stay terminal
            }
            let args = edges[0].args(data);
            for (position, &parameter) in params.iter().enumerate() {
                if param_resolutions.contains_key(&parameter) {
                    continue;
                }
                let Some(&arg) = args.get(position) else {
                    continue;
                };
                if arg == parameter {
                    continue; // self-passthrough: no information
                }
                if parameter_blocks.contains_key(&arg) {
                    // The argument is another block's parameter: chain
                    // through its resolution when available. Parameters of
                    // single-edge blocks resolve in a later fixpoint pass;
                    // terminal parameters (multi-edge blocks — e.g. the
                    // loop-carried IVs of enclosing loops) are kept as the
                    // final value and judged by `resolved_invariant`.
                    match param_resolutions.get(&arg) {
                        Some(&value) => {
                            param_resolutions.insert(parameter, value);
                        }
                        None => {
                            let arg_block = parameter_blocks[&arg];
                            if incoming_edges(data, cfg, arg_block).len() == 1 {
                                deferred.push((parameter, arg));
                            } else {
                                param_resolutions.insert(parameter, arg);
                            }
                        }
                    }
                } else {
                    param_resolutions.insert(parameter, arg);
                }
            }
        }
        // 中文：延迟解析项可能依赖另一参数的解析结果，逐轮扫描直到一轮无进展
        // （不动点）；环状依赖按终值落定，不会无限循环。
        while !deferred.is_empty() {
            let mut progress = false;
            let still_deferred = Vec::with_capacity(deferred.len());
            for (parameter, arg) in deferred.drain(..) {
                if let Some(&value) = param_resolutions.get(&arg) {
                    param_resolutions.insert(parameter, value);
                    progress = true;
                } else {
                    // The argument never resolved (cyclic chain): keep it as
                    // a terminal value; `resolved_invariant` judges it.
                    param_resolutions.insert(parameter, arg);
                    progress = true;
                }
            }
            deferred = still_deferred;
            if !progress {
                break; // defensive: should not happen with terminal handling
            }
        }

        // A resolved parameter value is invariant when it is a passthrough
        // header parameter or its defining block dominates the loop header.
        // Any value dominating the header lies outside the loop and is
        // available at the preheader insertion point, so hoisting with the
        // substituted operand is dominance-safe (the 8-04 operand-substitution
        // crash cannot recur).
        // 中文：解析后的值是否不变量：全局/常量恒真；passthrough header 参数
        // 恒真；其余看定义块——在循环外且支配 header（位于 preheader 插入点
        // 之前）即安全，替换后外提仍满足 SSA 支配。
        let resolved_invariant = |value: Inst| -> bool {
            if value.is_global() || data.inst_data(value).kind().is_const() {
                return true;
            }
            if invariant_header_params.contains(&value) {
                return true;
            }
            match data.layout().parent_bb(value) {
                Some(block) => !looop.contains(block) && dom_tree.dominates(block, looop.header()),
                None => parameter_blocks.get(&value).is_some_and(|&block| {
                    !looop.contains(block) && dom_tree.dominates(block, looop.header())
                }),
            }
        };

        // 中文：数据流分析初始化：循环内全部指令先标记 `Variant`，随后工作列表
        // 迭代松弛，直到不动点。
        let loop_insts = insts(looop, data).collect::<Vec<_>>();
        let mut map = loop_insts
            .iter()
            .copied()
            .map(|inst| (inst, Lattice::Variant))
            .collect::<FxHashMap<_, _>>();

        // Calls that may be hoisted: the callee must be read-only (no I/O,
        // no timer, no external write), and nothing in the loop may write
        // anything the callee reads (stores, memzeroes, or other calls).
        // 中文：纯调用可外提判定（`EffectAnalysis` 的核心用法），一个 call 须同时满足：
        //   1) `is_removable`：callee 无副作用（不写内存/外部），结果可丢弃；
        //   2) 循环内无写者命中其读集——store/MemZero 用 `targets_of` 求写目标后
        //      问 `call_may_read`；兄弟 call 先取 `call_read_roots`（读根集合），
        //      再问 `call_may_write` 它是否可能写这些根（读根未知时退化为
        //      `may_write_memory` 保守拒绝）。
        // 外提后 call 每轮只执行一次，若循环内有人改写它读的数据，提前执行就错。
        let hoistable_calls =
            loop_insts
                .iter()
                .copied()
                .filter(|&inst| {
                    let InstKind::Call(call) = data.inst_data(inst).kind() else {
                        return false;
                    };
                    let Some(analysis) = analysis else {
                        return false;
                    };
                    if !analysis.is_removable(call.callee()) {
                        return false;
                    }
                    let func = data.curr_func.unwrap();
                    let conflicts = loop_insts.iter().copied().any(|other| {
                        match data.inst_data(other).kind() {
                            InstKind::Store(store) => {
                                let targets = analysis.targets_of(data, func, store.dest());
                                analysis.call_may_read(call.callee(), targets.as_ref())
                            }
                            InstKind::MemZero(mem_zero) => {
                                let targets = analysis.targets_of(data, func, mem_zero.dest());
                                analysis.call_may_read(call.callee(), targets.as_ref())
                            }
                            InstKind::Call(other_call) => {
                                let sibling = other_call.callee();
                                match analysis.call_read_roots(call.callee(), func) {
                                    Some(reads) => {
                                        let reads = reads
                                            .iter()
                                            .map(|r| match r {
                                                WriteRoot::Global(g) => AbstractObject::Global(*g),
                                                WriteRoot::Local(f, a) => {
                                                    AbstractObject::Alloc(*f, *a)
                                                }
                                            })
                                            .collect::<FxHashSet<_>>();
                                        analysis.call_may_write(sibling, Some(&reads))
                                    }
                                    None => analysis.effects_of(sibling).may_write_memory(),
                                }
                            }
                            _ => false,
                        }
                    });
                    !conflicts
                })
                .collect::<FxHashSet<_>>();

        // 中文：单操作数不变性判定，按来源分四类：
        //   1) 全局/常量 → 不变；
        //   2) 循环块参数 → 仅 passthrough header 参数可信，或能解析成不变值的
        //      body 参数（回边携带循环值的 header 参数即使映射表会改写它仍是变体）；
        //   3) 循环内定义的指令 → 查数据流状态表；
        //   4) 循环外定义 → 定义块支配 header 即不变（preheader 处可得其值）。
        let operand_is_invariant = |operand: Inst, states: &FxHashMap<Inst, Lattice>| {
            if operand.is_global() || data.inst_data(operand).kind().is_const() {
                return true;
            }
            if loop_params.contains(&operand) {
                // Header parameters are invariant only through the
                // backedge-passthrough rule above: the substitution map
                // seeds header-param rewrites, but a loop-carried header
                // parameter (entry value X, backedge value Y) is variant
                // even though the map rewrites it. Body parameters resolve
                // through the single-predecessor chain.
                return invariant_header_params.contains(&operand)
                    || (!header_params.contains(&operand)
                        && param_resolutions
                            .get(&operand)
                            .is_some_and(|&value| resolved_invariant(value)));
            }
            match data.layout().parent_bb(operand) {
                Some(block) if looop.contains(block) => {
                    states.get(&operand) == Some(&Lattice::Invariant)
                }
                Some(block) => dom_tree.dominates(block, looop.header()),
                None if matches!(data.inst_data(operand).kind(), InstKind::BlockArgRef(..)) => {
                    parameter_blocks
                        .get(&operand)
                        .is_some_and(|&block| dom_tree.dominates(block, looop.header()))
                }
                None => false,
            }
        };

        // All memory-writing instructions in the loop body: any of them
        // that may alias a load's address blocks hoisting that load.
        // 中文：循环内全部可能写内存的指令（store/MemZero/call/TailCall），
        // 构成 load 外提的"写者清单"：任一个与 load 地址可能别名，load 就不能外提。
        let loop_writes = loop_insts
            .iter()
            .copied()
            .filter(|inst| {
                matches!(
                    data.inst_data(*inst).kind(),
                    InstKind::Store(..)
                        | InstKind::MemZero(..)
                        | InstKind::Call(..)
                        | InstKind::TailCall(..)
                )
            })
            .collect::<Vec<_>>();

        // 中文：主迭代：指令判为不变 ⇔ 种类可候选 ∧ 全部操作数不变 ∧ load 额外
        // 要求写者安全。某指令状态翻转时，把循环内使用它的指令重新入队（沿
        // def-use 前向传播），直到不动点；`invariant_order` 按发现顺序记录
        // 最终不变指令，供外提阶段依序处理。
        let mut invariant_order = vec![];
        let mut worklist = VecDeque::from_iter(loop_insts.iter().copied());
        while let Some(inst) = worklist.pop_front() {
            let inst_data = data.inst_data(inst);
            let status = if can_be_invariant(inst_data.kind(), inst, &hoistable_calls)
                && inst_data
                    .inst_usage()
                    .all(|operand| operand_is_invariant(operand, &map))
                && load_hoist_safe(analysis, inst, data.curr_func.unwrap(), data, &loop_writes)
            {
                Lattice::Invariant
            } else {
                Lattice::Variant
            };
            let orig = map
                .insert(inst, status)
                .expect("loop instruction was initialized");
            if orig != status {
                invariant_order.push(inst);
                worklist.extend(data.inst_data(inst).used_by().iter().filter_map(|&user| {
                    data.layout()
                        .parent_bb(user)
                        .filter(|&block| looop.contains(block))
                        .map(|_| user)
                }));
            }
        }

        // 中文：地址计算型 load：地址（`load.src`）本身也定义在循环内、需靠地址
        // 计算外提才能成立的 load。批量外提会拉长地址/值的活跃区间，AArch64 上
        // 常见 spill 得不偿失（上限 `MAX_HOISTED_COMPUTED_LOADS` = 8）。超限时把
        // 最早外提的这类 load 降回 `Variant`，并沿操作数链级联降级依赖它的表达式。
        let computed_loads = invariant_order
            .iter()
            .copied()
            .filter(|&inst| {
                let InstKind::Load(load) = data.inst_data(inst).kind() else {
                    return false;
                };
                data.layout()
                    .parent_bb(load.src())
                    .is_some_and(|block| looop.contains(block))
            })
            .collect::<Vec<_>>();
        if limit_computed_loads && computed_loads.len() > MAX_HOISTED_COMPUTED_LOADS {
            let mut retained = VecDeque::from(computed_loads);
            while let Some(inst) = retained.pop_front() {
                if map.insert(inst, Lattice::Variant) != Some(Lattice::Invariant) {
                    continue;
                }
                if let InstKind::Load(load) = data.inst_data(inst).kind() {
                    retained.push_back(load.src());
                } else {
                    retained.extend(data.inst_data(inst).inst_usage().filter(|operand| {
                        data.layout()
                            .parent_bb(*operand)
                            .is_some_and(|block| looop.contains(block))
                    }));
                }
            }
            // Any invariant expression depending on a retained load must stay
            // with it. Iterate to a fixed point because dependency chains may
            // contain address casts or arithmetic before their final use.
            loop {
                let mut downgraded = false;
                for &inst in &invariant_order {
                    if map[&inst] != Lattice::Invariant {
                        continue;
                    }
                    if data.inst_data(inst).inst_usage().any(|operand| {
                        data.layout().parent_bb(operand).is_some_and(|block| {
                            looop.contains(block) && map.get(&operand) == Some(&Lattice::Variant)
                        })
                    }) {
                        map.insert(inst, Lattice::Variant);
                        downgraded = true;
                    }
                }
                if !downgraded {
                    break;
                }
            }
            invariant_order.retain(|inst| map[inst] == Lattice::Invariant);
        }

        // 中文：GEP 部分外提候选：只有 offsets 的不变前缀（`base` + 前 k 个偏移）
        // 是不变量，其余偏移随 IV 变化。此时把不变前缀做成 preheader 里的新 GEP、
        // 循环内原 GEP 接剩余后缀——地址语义不变而前缀只算一次。跳过前缀为空/
        // 全不变（全不变走常规外提）及前缀仅为 [0] 的无意义情形。
        let partial_geps = loop_insts
            .into_iter()
            .filter_map(|inst| {
                if map.get(&inst) == Some(&Lattice::Invariant) {
                    return None;
                }
                let InstKind::GetElemPtr(gep) = data.inst_data(inst).kind() else {
                    return None;
                };
                if !operand_is_invariant(gep.base(), &map) {
                    return None;
                }
                let prefix_len = gep
                    .offsets()
                    .iter()
                    .take_while(|&&offset| operand_is_invariant(offset, &map))
                    .count();
                if prefix_len == 0 || prefix_len == gep.offsets().len() {
                    return None;
                }
                if prefix_len == 1 && is_integer_zero(data, gep.offsets()[0]) {
                    return None;
                }
                Some((
                    inst,
                    gep.base(),
                    gep.offsets()[..prefix_len].to_vec(),
                    gep.offsets()[prefix_len..].to_vec(),
                    data.inst_data(inst).ty().clone(),
                ))
            })
            .collect::<Vec<_>>();

        // 中文：无外提内容（常量除外——常量无需搬动）则本轮无事可做。
        let has_invariant_insts = invariant_order
            .iter()
            .any(|&inst| !data.inst_data(inst).kind().is_const());
        // 中文：外提需要一个真正的 preheader 作插入点；若需新建（CFG 被改），
        // 返回 `CfgChanged`，外层重建 CFG/支配/循环分析后重跑本循环。
        if !has_invariant_insts && partial_geps.is_empty() {
            return LoopResult::Unchanged;
        }

        let Some(preheader) = ensure_preheader(data, cfg, looop) else {
            return LoopResult::Unchanged;
        };
        let preheader = match preheader {
            EnsurePreheader::Existing(preheader) => preheader,
            EnsurePreheader::Created(..) => return LoopResult::CfgChanged,
        };

        // 中文：外提改写（核心步骤），逐条搬移不变指令：
        //   1) `substitute_header_params` 先替换循环内参数引用——搬走后指令落在
        //      preheader，不能引用循环内定义，否则违反 SSA 支配；
        //   2) 从原块指令序列摘除；
        //   3) 插入 preheader 的 terminator 之前：该位置每轮循环必经且只执行一次，
        //      并支配循环内所有使用点。
        let mut changed = false;
        for inst in invariant_order {
            let inst_data = data.inst_data(inst);
            if inst_data.kind().is_const() {
                continue;
            }
            changed = true;
            if !param_resolutions.is_empty() {
                substitute_header_params(data, inst, &param_resolutions);
            }
            let bb = data
                .layout()
                .parent_bb(inst)
                .expect("invariant inst must be in the layout, constants is excluded");
            data.layout_mut().remove_inst(bb, inst);
            data.layout_mut().insert_before_terminator(preheader, inst);
        }

        // 中文：部分 GEP 的实际改写：preheader 里生成前缀 GEP（操作数同样先做
        // 参数替换），循环内原 GEP 改为 [前缀, 0, ...剩余偏移]——头部补 0 保持
        // 与完整 GEP 相同的指针运算语义与结果类型（下方 assert 校验类型不变）。
        for (inst, base, prefix_offsets, remaining_offsets, original_ty) in partial_geps {
            let substitute =
                |value: Inst| -> Inst { param_resolutions.get(&value).copied().unwrap_or(value) };
            let prefix = data.new_local_value().get_elem_ptr(
                substitute(base),
                prefix_offsets
                    .iter()
                    .map(|&offset| substitute(offset))
                    .collect(),
            );
            data.layout_mut()
                .insert_before_terminator(preheader, prefix);

            let zero = data.new_local_value().integer(0);
            let mut suffix_offsets = Vec::with_capacity(remaining_offsets.len() + 1);
            suffix_offsets.push(zero);
            suffix_offsets.extend(remaining_offsets);
            data.replace_inst_with(inst)
                .get_elem_ptr(prefix, suffix_offsets);
            assert_eq!(
                data.inst_data(inst).ty(),
                &original_ty,
                "splitting GEP must preserve its result type"
            );
            changed = true;
        }

        if changed {
            LoopResult::Changed
        } else {
            LoopResult::Unchanged
        }
    }
}

/// Whether hoisting `inst` out of the loop is safe with respect to memory
/// ordering. Non-loads are always safe; a load is safe only when no loop
/// write (store, MemZero, or call) may write its address. Without the
/// whole-program analysis, loads are conservatively not hoisted.
// 中文：load 外提安全性（`EffectAnalysis` 的第二处使用）：非 load 恒安全；
// load 须与每个写者核对——store/MemZero 用 `alias`（跨过程 points-to 别名）
// 比对地址；call/TailCall 用 `targets_of` 取 load 地址指向的对象集，再问
// `call_may_write` 被调者是否可能写这些对象。无全程序分析时保守不外提。
fn load_hoist_safe(
    analysis: &Option<EffectAnalysis>,
    inst: Inst,
    func: Function,
    data: &ArenaContextMut<'_>,
    loop_writes: &[Inst],
) -> bool {
    let InstKind::Load(load) = data.inst_data(inst).kind() else {
        return true;
    };
    let Some(analysis) = analysis else {
        return false;
    };
    let addr = load.src();
    loop_writes
        .iter()
        .all(|&write| match data.inst_data(write).kind() {
            InstKind::Store(store) => !analysis.alias(data, func, addr, store.dest()).may_alias(),
            InstKind::MemZero(mem_zero) => !analysis
                .alias(data, func, addr, mem_zero.dest())
                .may_alias(),
            InstKind::Call(call) => {
                let targets = analysis.targets_of(data, func, addr);
                !analysis.call_may_write(call.callee(), targets.as_ref())
            }
            InstKind::TailCall(tail_call) => {
                let targets = analysis.targets_of(data, func, addr);
                !analysis.call_may_write(tail_call.callee(), targets.as_ref())
            }
            _ => true,
        })
}

impl Pass for LICM {
    fn run(&mut self, program: &mut Program) -> bool {
        // Rebuild the whole-program purity / alias analysis: the fixed-point
        // manager re-invokes run() after every IR change, so a stale
        // snapshot must never be reused.
        // 中文：`Pass` 入口：每次 `run` 都重建全程序 `EffectAnalysis`——fixpoint
        // 管理器在每轮 IR 变化后都会重调 `run`，陈旧快照不可复用。随后逐函数
        // 执行 `run_on`，任一函数有改动即返回 true。
        self.analysis = Some(EffectAnalysis::new(program));
        let func_layout = program.function_layout().to_vec();
        let mut changed = false;
        for func in func_layout {
            let mut arena_context = ArenaContextMut {
                program,
                curr_func: Some(func),
            };
            changed |= self.run_on(&mut arena_context);
        }
        changed
    }

    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().is_decl() {
            return false;
        }
        // Direct invocations (unit tests, pre-run_on callers) have no
        // analysis yet; build one locally. Function-level side effects do
        // not change while this pass runs (LICM only relocates
        // instructions), so analyzing once per invocation is enough.
        // 中文：直接调用路径（单测、`run_on` 前置调用者）没有分析快照，就地
        // 构建一次即可：LICM 只搬动指令、不改函数副作用，一次分析足够。
        if self.analysis.is_none() {
            self.analysis = Some(EffectAnalysis::new(data.program));
        }
        let mut changed = false;
        // 中文：外层循环：每轮重建 CFG/支配树/循环分析（三者都是快照，IR 一改
        // 即失效）；CFG 无环则没有循环可外提，直接返回。
        loop {
            let Some(cfg) = CFG::new(data) else {
                return changed;
            };
            if cfg.is_acyclic() {
                return changed;
            }
            // 中文：`parameter_blocks`：块参数 → 所属块 的索引。块参数没有定义
            // 指令，参数解析与支配判定都靠它定位所属块。
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
            let (cfg, dom_tree, loop_analysis) = loop_analysis::LoopAnalysis::from_cfg(cfg);
            let mut rebuild = false;

            // Loops are ordered from small to big. This lets an instruction
            // hoisted from an inner loop be considered by its outer loop.
            // 中文：循环按小到大排序处理：内层外提出的指令现在定义在外层循环内、
            // 仍可能是不变量，会被外层循环接着考察。
            for looop in loop_analysis.loops() {
                match Self::solve(
                    looop,
                    &self.analysis,
                    data,
                    &cfg,
                    &dom_tree,
                    &parameter_blocks,
                    self.limit_computed_loads,
                ) {
                    LoopResult::Unchanged => {}
                    LoopResult::Changed => changed = true,
                    LoopResult::CfgChanged => {
                        changed = true;
                        rebuild = true;
                        break;
                    }
                }
            }

            // 中文：某循环外提需要新建 preheader 时 CFG 已变，快照型分析全部
            // 失效：置 `rebuild` 跳出，回到外层循环开头重建全套分析后重来，
            // 直到一整轮不再改动 CFG。
            if !rebuild {
                return changed;
            }
        }
    }
}

#[cfg(test)]
mod tests;

