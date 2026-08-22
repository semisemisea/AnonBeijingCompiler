//! # Inline：预算控制的函数内联
//!
//! 在调用点把被调函数体克隆进调用方，消除 call/ret 的调用开销（参数搬运、
//! 保存恢复、跳转），并把小函数的代码并入调用方的基本块，给后续标量 / 循环
//! pass 提供更大的优化窗口。触发对象是**成本模型内的小函数**：估计克隆大小
//! × 静态调用点数不超过预算即内联，让热内核里的小工具函数（如 huffman-01 的
//! `rotlN` / `rotrN`）在每个调用点都被展开，同时不让"多调用点的大叶子函数"
//! 把程序撑爆。CLI：无独立开关，`-O1` / `-O2` 下始终运行。术语（SSA、
//! block 参数（Phi）、支配、自然循环 / header / 回边、固定点）见
//! `docs/offline-handbook/glossary.md` 的"内联"条目，本模块不展开。
//!
//! ## 变换形态（IR 示例）
//!
//! 内联前：`main` 的 entry 调用 `choose`，返回值直接 `ret`；`choose` 有两条
//! 返回路径（if / else）：
//!
//! ```text
//! main:
//!   entry:        v = call choose(cond); ret v
//!
//! choose(c):
//!   entry:        br c, then, else
//!   then:         ret 1
//!   else:         ret 2
//! ```
//!
//! 内联后（`choose` 本体保留；`main` 里出现克隆体与续体块；块名为示例，
//! 真实命名见下）：
//!
//! ```text
//! main:
//!   entry:              jump entry_choose_inline(cond)   // 原 call 换成进入克隆体的 jump
//!   entry_choose_inline(c):                             // 克隆的 entry
//!                       br c, then_choose_inline, else_choose_inline
//!   then_choose_inline: jump entry_inline_cont(1)        // 克隆的 ret → 跳续体，携带返回值
//!   else_choose_inline: jump entry_inline_cont(2)
//!   entry_inline_cont(v): ret v                         // 续体：v 是块参数（返回值）
//! ```
//!
//! 结构上：`once` 先 `split_block_after` 把含 call 的块切成两半，后半成为带
//! 一个返回类型参数的续体块（`{原块名}_inline_cont`；unit 返回则无参数）；
//! `BodyClonePlan::clone_into` 把被调函数全部块克隆到 call 块之后（克隆块名
//! `{原块名}_{被调函数名}_inline`）；每条克隆的 `ret` 重写为带返回值的
//! `jump` 汇入续体；call 指令的所有使用点替换为续体参数后删除；最后在 call
//! 位置插入带原实参的 `jump` 进入克隆入口。多条返回路径通过续体的块参数
//! 合并——正是本项目 Phi 的形态。
//!
//! ## 触发 / 放弃条件
//!
//! 候选筛选在 `find_candidate` 逐条进行，任一不满足即放弃该被调函数：
//!
//! - 有函数体：`layout().is_decl()` 的声明跳过；
//! - **不在递归环中**：`call_graph.reaches(callee, callee)` 为真则放弃。
//!   原因（代码注释）：克隆一次后环内调用点就位于调用方内部，可达性 guard
//!   不再排除它，会无限内联下去；保守规则沿用 cranelift 的
//!   `does_not_inline_across_a_recursive_call_cycle`；
//! - 有静态调用点：`call_graph.incoming_callsites_of(callee)` 非空；
//! - **成本模型**（`estimate_size` + 四个预算常量）：`size` = 被调函数各
//!   基本块指令数之和（当前实现即 `layout().basicblocks()` 各块
//!   `insts().len()` 求和），`total = size × 调用点数`（`saturating_mul`）。
//!   `size ≤ CALL_SIZE_LIMIT`（40）且 `total ≤ TOTAL_SIZE_LIMIT`（100）即
//!   接受；超限时仅当 `any_callsite_in_loop` 为真且
//!   `size ≤ CALL_SIZE_LIMIT_LOOP`（200）、`total ≤ TOTAL_SIZE_LIMIT_LOOP`
//!   （300）才接受，否则放弃；
//! - 选中的调用点满足 caller ≠ callee 且 `!call_graph.reaches(callee,
//!   callsite.func)`——caller 不可从被调函数到达，防止把调用点内联进"被调
//!   函数能到达的函数"形成爆炸；
//! - 类型匹配：call 结果类型 == `callee_data.ret_ty()`、实参数 == 形参数
//!   （`call.args().len() == params_ty().len()`）、逐个实参类型 == 形参类型；
//! - unit 型 call 必须 `used_by` 为空（unit 值不能有使用者）；
//! - 克隆预检：`BodyClonePlan::capture` 成功（放弃原因见下）且克隆体不含
//!   尾调用（`plan.contains_tail_call()` 为假）。
//!
//! 放弃（`BodyClonePlan::capture` 的 `CloneError` 各变体）：`Declaration`
//! （无函数体）/ `MissingEntry` / `EmptyBlock` / `InvalidTerminator` /
//! `MissingTargetBlock`（控制流目标逃出被调函数体）/ `MissingLocalOperand` /
//! `LocalGlobalAlloc`（函数体含 `GlobalAlloc`：克隆会产生多份全局分配，破坏
//! "只分配一次"语义）；另有克隆体含 `TailCall` 时放弃（尾调用有自己的处理
//! 路径，见 fixpoint 里的 `tail_recursive_inline`）。
//!
//! 循环内放宽的理由（`CALL_SIZE_LIMIT_LOOP` 常量注释）：调用点位于调用方
//! 自然循环内时，被移除的调用开销每轮迭代都要付一次，一次性更大的克隆动态上
//! 划算（如 huffman-01 decode 循环里的 `read_bits_specialized_*`）；且循环内
//! 被调函数可能通过链式内联继续变大（`read_bits` 141 + `rotlN` 34 = 175），
//! 预算须覆盖链后尺寸而非裸函数体。
//!
//! ## 正确性
//!
//! - 返回值路由：续体块参数即返回类型；每个克隆 `ret` 的返回值作为 jump 实参
//!   传入续体（多条返回路径汇入同一续体，由块参数按入边合并）；
//! - call 的**所有**使用点替换为续体参数（`utils::visit_and_replace`），删除
//!   call 前断言 `used_by` 为空（"call must have no users before removal"）；
//! - 实参按**位置**传给克隆入口（`jump(cloned.entry, args)`），不重排——回归
//!   测试 `preserves_argument_order_for_many_params_with_array_parameters`
//!   （`functional/88_many_params2.sy`：多参数含数组指针参数时位置必须保持）；
//! - 克隆只重映射局部值，全局值共享（`CloneMapper` 对 `is_global()` 原样
//!   保留）；块参数与块间跳转目标同步重映射，克隆体自洽；
//! - 类型匹配在克隆前完成，保证实参 / 形参、结果 / 返回类型一一对应；
//! - 终止性：递归环 guard、调用点可达性 guard 与预算上限三者共同保证
//!   `run` 的 `while Self::once(program)`（每次只内联一个调用点）一定收敛。
//!
//! ## 管线位置与门控
//!
//! - 注册：`opt/pass.rs` 的 `PassesManager::from_config`，**initial 段**
//!   （`register_initial`，整条管线只跑一次、不进 fixpoint），无条件挂载
//!   （`-O0` 在 `from_config` 开头早退，故实际仅 `-O1` / `-O2` 生效）；
//!   无目标门控（AArch64 / RISC-V 都跑）。
//! - 前驱：`ssa`（SSATransform）、`specialize`，以及 AArch64 门控的
//!   `mulmod_recognize` 与 `recursive_memoize`——注册注释明确它们必须跑在
//!   inline 之前：前者把递归模乘改写为内置调用后，缩小的 callee 会被本 pass
//!   内联；后者把自递归函数改写为带缓存循环后，inline 不会把"已冗余的函数
//!   体"拍平。
//! - 后继：initial 的 `tco`（TailCallElim）、`column_major`、`gsp`
//!   （ScalarGlobalPromotion）——gsp 的注册注释要求跑在 inline 之后
//!   （"so the callee-touch analysis sees the final call graph"：全局标量
//!   提升的 callee 触达分析需要看到内联后的最终调用图）；之后进入 fixpoint
//!   （IPSCCP 起），fixpoint 内的 `TailRecursiveInline` 在 TCO 之后把纯自尾
//!   递归转成循环，与本 pass 的普通调用内联互补。
//!
//! ## 验证
//!
//! - 本文件 `mod tests`（228 行起）9 个用例：单调用内联与续体路由、多参数
//!   （含数组参数）位置保持、多返回值合并、超预算 leaf 拒绝、递归环拒绝、
//!   循环内放宽（超 `CALL_SIZE_LIMIT` 的 callee 在自然循环内内联、循环外
//!   拒绝；34 × 3 = 102 > `TOTAL_SIZE_LIMIT`（100）的跨线形状在循环内通过
//!   合计预算 300 内联、循环外拒绝）；
//! - 端到端：`make test` 差分（性能用例用 `ARGS="-O 2"`），静态指令数对照
//!   `scripts/perf_compare.sh`——huffman-01 的 `rotlN` / `rotrN` 等热辅助
//!   函数在每个调用点展开；也可用 `--emit ir` dump 直接观察内联后的 IR。

use crate::{
    ir::arena::Arena,
    opt::{
        analysis_passes::loop_analysis::LoopAnalysis, prelude::*, utils::body_clone::BodyClonePlan,
    },
};

/// Inline functions with a bounded total cloning cost. A callee is inlined
/// when its estimated instruction count times its number of statically
/// reachable callsites stays within a budget, so hot helpers (e.g.
/// `rotlN`/`rotrN` in huffman-01) are inlined at every callsite without
/// letting a many-callsite leaf blow the program up.
pub struct Inline;

/// Per-callsite clone budget (estimated instructions of the callee body).
/// 单个调用点的克隆体大小上限：函数体超过它说明"太肥"，展开不划算。
const CALL_SIZE_LIMIT: usize = 40;
/// Total budget for one callee across all of its callsites.
/// 一个被调函数在所有调用点上的克隆总量上限：防止"多调用点的叶子函数"把程序
/// 撑爆——单个调用点再小，调用点一多总量也会失控。
const TOTAL_SIZE_LIMIT: usize = 100;
/// Per-callsite clone budget when at least one callsite sits inside a
/// natural loop of its caller: the removed call overhead is paid once per
/// loop iteration, so a larger one-time clone is dynamically worthwhile
/// there (e.g. `read_bits_specialized_*` in huffman-01's decode loop).
/// Calibrated on real data: a loop-resident callee may itself grow through
/// chained in-loop inlining of its own helpers (`read_bits` 141 + `rotlN` 34
/// = 175), so the budget must cover the post-chain size, not the bare body.
/// 循环内放宽后的单调用点上限：循环内的调用开销每轮迭代都要付一次，一次性
/// 更大的克隆动态上更划算（放宽理由与校准数据见上方英文注释）。
const CALL_SIZE_LIMIT_LOOP: usize = 200;
/// Total budget across the callsites of an in-loop callee.
/// 循环内放宽后的总量上限。
const TOTAL_SIZE_LIMIT_LOOP: usize = 300;

impl Pass for Inline {
    fn run(&mut self, program: &mut Program) -> bool {
        // 主循环入口：反复调用 `once`，每轮只内联一个调用点，直到找不到候选。
        // 每次只改一处，保证下一轮 `find_candidate` 看到的调用图与 CFG 都是
        // 最新快照；终止性由递归环守卫、调用点可达性守卫与成本预算共同保证。
        let mut changed = false;
        while Self::once(program) {
            changed = true;
        }
        changed
    }
}

/// Estimated clone size: ordinary instructions plus a small constant for the
/// block-argument routing the inliner has to splice around each edge.
fn estimate_size(data: &FunctionData) -> usize {
    // 实现：把被调函数每个基本块的指令数直接相加，得到"克隆一份函数体"的
    // 指令量估计。不区分指令种类，也不含加权系数，只做粗略的代码量度量。
    data.layout()
        .basicblocks()
        .iter()
        .map(|layout| layout.insts().len())
        .sum()
}

/// 一个待内联调用点的完整描述：调用方、调用指令与所在基本块、实参列表、
/// 返回类型，以及 `find_candidate` 阶段预检好的克隆预案（`BodyClonePlan`）。
/// `once` 拿到它即可直接改写，无需再做任何检查。
struct Candidate {
    caller: Function,
    call_inst: Inst,
    call_block: BasicBlock,
    args: Vec<Inst>,
    ret_ty: Type,
    plan: BodyClonePlan,
}

impl Inline {
    /// 单次内联：取出 `find_candidate` 找到的一个候选调用点，完成
    /// "切续体 → 克隆函数体 → 重写返回 → 替换使用点并接跳转"四步改写。
    /// 返回 `true` 表示发生了内联；`run` 循环调用它直至返回 `false`。
    fn once(program: &mut Program) -> bool {
        // 没有候选即无事可做，主循环收敛。
        let Some(candidate) = Self::find_candidate(program) else {
            return false;
        };

        let Candidate {
            caller,
            call_inst,
            call_block,
            args,
            ret_ty,
            plan,
        } = candidate;

        // 第一步（切续体）：把含 call 的块在 call 之后切成两半，后半成为
        // 续体块 `{块名}_inline_cont`，带一个返回类型参数。所有克隆返回路径
        // 最终都汇入这里，块参数即"返回值"——多条返回路径由此合并（Phi 形态）；
        // unit 返回无需携带值，参数列表为空。
        let continuation = {
            let data = program.func_data_mut(caller);
            let block_name = data.bb_data(call_block).name().to_owned();
            let params = if ret_ty.is_unit() {
                vec![]
            } else {
                vec![ret_ty]
            };
            data.split_block_after(call_inst, format!("{block_name}_inline_cont"), params)
        };

        // 第二步（克隆）：把被调函数全部基本块克隆到 call 块之后，克隆块命名
        // `{原块名}_{被调函数名}_inline`。克隆体自洽：局部值（含块参数）由
        // `CloneMapper` 重映射为全新指令，全局值原样共享；`cloned.returns` /
        // `cloned.tail_calls` 分别收集克隆体内的返回与尾调用指令。
        let cloned = plan
            .clone_into(program, caller, call_block)
            .expect("preflighted function body must clone successfully");
        // 候选筛选已排除含尾调用的函数体，且 `capture` 预检过结构，可安全断言。
        debug_assert!(cloned.tail_calls.is_empty());
        debug_assert!(!cloned.blocks.is_empty());

        let mut context = ArenaContextMut {
            program,
            curr_func: Some(caller),
        };
        // 第三步（重写返回）：克隆体内的 `ret` 不能原样保留——它本意是结束
        // 被调函数，留在调用方里会错误地结束整个调用方函数。把每条克隆 `ret`
        // 改写为携带返回值的 `jump` 汇入续体，多条返回路径在此合并。
        for return_inst in cloned.returns {
            let return_value = match context.inst_data(return_inst).kind() {
                InstKind::Return(ret) => ret.value(),
                _ => unreachable!("cloner returned a non-return instruction"),
            };
            let jump_args = return_value.into_iter().collect();
            context
                .replace_inst_with(return_inst)
                .jump(continuation, jump_args);
        }

        // 第四步：先把 call 的所有使用点替换为续体参数（`visit_and_replace`
        // 遍历 call 的全部使用者改写引用）；unit 型 call 没有使用者，无需替换。
        if !context.inst_data(call_inst).ty().is_unit() {
            let continuation_result = context.bb_data(continuation).params()[0];
            utils::visit_and_replace(&mut context, call_inst, continuation_result);
        }
        // 使用点清空后 call 成为死指令：删除前断言无使用者，防止悬空引用。
        assert!(
            context.inst_data(call_inst).used_by().is_empty(),
            "call must have no users before removal"
        );
        // 收尾：删除原 call，并在其位置插入进入克隆入口的 jump。实参列表即原
        // call 的实参，按位置对应克隆入口形参（不重排——多参数用例的回归测试
        // 专门盯着这一点）。
        context.remove_layout_inst(call_block, call_inst);
        let entry_jump = context.new_local_value().jump(cloned.entry, args);
        context.layout_mut().insert_inst(call_block, entry_jump);
        true
    }

    /// 遍历程序全部函数，返回第一个满足全部条件的调用点候选（含克隆预案）。
    /// 筛选顺序：函数级守卫（有函数体、不在递归环、有调用点）→ 成本预算 →
    /// 调用点级守卫（调用方可达性）→ 类型匹配 → 克隆预检；任一不满足即放弃
    /// 该被调函数。`None` 表示没有可内联的调用点，主循环收敛。
    fn find_candidate(program: &Program) -> Option<Candidate> {
        // 调用图是一次性快照：本轮 `once` 尚未改写任何代码，用它做可达性
        // 与调用点查询是有效的；内联一旦发生，下一轮会重建。
        let call_graph = call_graph::CallGraph::new(program);
        for &callee in program.function_layout() {
            if program.func_data(callee).layout().is_decl() {
                continue;
            }
            // A callee in a recursion cycle is never inlined: after the
            // first clone its in-cycle callsites live inside the caller,
            // where the reachability guard no longer excludes them, so the
            // pass would keep inlining the cycle forever. The conservative
            // cranelift rule (`does_not_inline_across_a_recursive_call_cycle`)
            // is to leave cyclic callees as calls.
            // 中文注：这正是防无限增长的第一个守卫——对环上函数一律保持调用。
            // 关键在"克隆一次后"：环内调用点会搬进调用方内部，下一轮可达性
            // 守卫不再排除它，于是会无限内联下去，所以必须在入口处直接拒绝。
            if call_graph.reaches(callee, callee) {
                continue;
            }
            let callee_data = program.func_data(callee);
            // 调用点收集：`incoming_callsites_of` 枚举 callee 的全部静态调用点
            // （调用指令 + 所在函数）。没有调用点的函数（孤立 / 外部入口）没有
            // 可展开的位置，直接跳过。
            let callsites: Vec<_> = call_graph.incoming_callsites_of(callee).collect();
            if callsites.is_empty() {
                continue;
            }
            // Cost model: the estimated clone size times the number of
            // callsites must stay within budget. A single-callsite helper of
            // any reasonable size is always inlined; a many-callsite leaf is
            // only inlined when the total stays small. A callee whose callsite
            // sits inside a natural loop removes its call overhead once per
            // iteration, so the strict budget is relaxed for it (see
            // `any_callsite_in_loop`).
            // 成本模型（防增长守卫之二，对应上方英文注释）：`size` 是单份克隆
            // 的指令数，`total` 是所有调用点克隆的总量——用 `saturating_mul`
            // 防极端情况下溢出。超预算时仅当存在位于自然循环内的调用点才放宽
            // （`any_callsite_in_loop`），否则放弃该 callee。
            let size = estimate_size(callee_data);
            let total = size.saturating_mul(callsites.len());
            if size > CALL_SIZE_LIMIT || total > TOTAL_SIZE_LIMIT {
                let in_loop = Self::any_callsite_in_loop(program, &callsites);
                if !in_loop || size > CALL_SIZE_LIMIT_LOOP || total > TOTAL_SIZE_LIMIT_LOOP {
                    continue;
                }
            }
            // 调用点级守卫（防增长守卫之三）：跳过 callee 对自身的调用点，并
            // 排除"调用方可从 callee 到达"的调用点——若被调函数能（直接或
            // 间接）调用到调用方，把该调用点内联进去会让被调函数体再次出现在
            // 它自己能到达的函数里，形成调用图层面的爆炸式增长。`find` 取首个
            // 通过守卫的调用点作为本轮候选。
            let Some(&callsite) = callsites.iter().find(|callsite| {
                callsite.func != callee && !call_graph.reaches(callee, callsite.func)
            }) else {
                continue;
            };
            let caller_data = program.func_data(callsite.func);
            let Some(call_block) = caller_data.layout().parent_bb(callsite.inst) else {
                continue;
            };
            let InstKind::Call(call) = caller_data.inst_data(callsite.inst).kind() else {
                continue;
            };
            if call.callee() != callee {
                continue;
            }

            // 类型匹配（在克隆前完成）：结果类型 == 返回类型、实参数 == 形参数、
            // 且每个实参类型 == 对应形参类型。克隆是按位置把实参绑到形参的，
            // 类型错位会在 IR 里留下类型非法的块参数，所以先在这里挡掉。
            if caller_data.inst_data(callsite.inst).ty() != callee_data.ret_ty()
                || call.args().len() != callee_data.params_ty().len()
                || call
                    .args()
                    .iter()
                    .zip(callee_data.params_ty())
                    .any(|(&arg, param_ty)| caller_data.inst_data(arg).ty() != param_ty)
            {
                continue;
            }
            // unit 型 call 不允许有使用者：unit 值没有类型，被使用说明 IR 里
            // 有非法引用（或残留坏值），此时放弃该候选。
            if caller_data.inst_data(callsite.inst).ty().is_unit()
                && !caller_data.inst_data(callsite.inst).used_by().is_empty()
            {
                continue;
            }

            // 克隆预检：`capture` 在克隆前把函数体扫描一遍（结构合法性、控制流
            // 目标不逃逸、无 `GlobalAlloc` 等，失败原因见模块文档），失败即放弃；
            // 含尾调用的函数体也排除——尾调用由 fixpoint 里的 `tail_recursive_inline`
            // 专门处理，不在此内联。
            let Ok(plan) = BodyClonePlan::capture(program, callee) else {
                continue;
            };
            if plan.contains_tail_call() {
                continue;
            }
            return Some(Candidate {
                caller: callsite.func,
                call_inst: callsite.inst,
                call_block,
                args: call.args().to_vec(),
                ret_ty: callee_data.ret_ty().clone(),
                plan,
            });
        }
        None
    }

    /// True when any of the callsites' enclosing blocks sits inside a natural
    /// loop of its caller. Builds one `LoopAnalysis` per distinct caller; the
    /// analysis is a snapshot of the current CFG, which is valid because
    /// `find_candidate` runs before any mutation in `once`.
    fn any_callsite_in_loop(program: &Program, callsites: &[Node]) -> bool {
        // 循环分析是当前 CFG 的快照且有一定代价，同一调用方下的多个调用点
        // 共用一次分析（按调用方缓存）。任一调用点所在基本块被某个自然循环
        // 包含（`min_loop_contain` 命中）即返回 `true`。
        let mut loops_by_caller: HashMap<Function, LoopAnalysis> = HashMap::default();
        callsites.iter().any(|callsite| {
            let analysis = loops_by_caller.entry(callsite.func).or_insert_with(|| {
                let (_, _, loops) = LoopAnalysis::new(program.func_data(callsite.func));
                loops
            });
            let Some(call_block) = program
                .func_data(callsite.func)
                .layout()
                .parent_bb(callsite.inst)
            else {
                return false;
            };
            analysis.min_loop_contain(call_block).is_some()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Inline;
    use crate::{
        ir::{
            BasicBlock, BinaryOp, Function, Inst, InstKind, Program, Type, arena::Arena,
            builder_trait::*,
        },
        opt::pass::Pass,
    };

    #[test]
    fn inlines_a_single_call_and_routes_the_result_through_a_continuation() {
        let mut program = Program::new();
        let callee = program.new_function(Type::get_i32(), "add_one".into(), vec![Type::get_i32()]);
        {
            let data = program.func_data_mut(callee);
            let entry = data.add_entry_block();
            let param = data.params()[0];
            let one = data.new_local_inst().integer(1);
            let add = data.new_local_inst().binary(BinaryOp::Add, param, one);
            let ret = data.new_local_inst().ret(Some(add));
            data.layout_mut().insert_inst(entry, add);
            data.layout_mut().insert_inst(entry, ret);
        }

        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        {
            let ret_ty = program.func_data(callee).ret_ty().clone();
            let data = program.func_data_mut(main);
            let entry = data.add_entry_block();
            let forty_one = data.new_local_inst().integer(41);
            let call = data
                .new_local_inst()
                .call_with_type(callee, vec![forty_one], ret_ty);
            let ret = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(entry, call);
            data.layout_mut().insert_inst(entry, ret);
        }

        assert!(Inline.run(&mut program));
        assert!(!Inline.run(&mut program));

        let data = program.func_data(main);
        assert_eq!(data.layout().basicblocks().len(), 3);
        assert!(data.layout().basicblocks().iter().all(|block| {
            block
                .insts()
                .iter()
                .all(|&inst| !matches!(data.inst_data(inst).kind(), InstKind::Call(..)))
        }));
        let continuation = data.layout().basicblocks().get_last().unwrap().bb();
        assert_eq!(data.bb_data(continuation).params().len(), 1);
        let ret = data.layout().basicblock(continuation).terminator();
        let InstKind::Return(ret) = data.inst_data(ret).kind() else {
            panic!("expected caller return");
        };
        assert_eq!(ret.value(), Some(data.bb_data(continuation).params()[0]));
    }

    #[test]
    fn preserves_argument_order_for_many_params_with_array_parameters() {
        // Regression for functional/88_many_params2.sy: inlining a callee
        // with many (incl. array) parameters must keep jump arguments in
        // positional order.
        let mut program = Program::new();
        let arr_ty = Type::get_array(Type::get_i32(), 2);
        let callee = program.new_function(
            Type::get_i32(),
            "func".into(),
            vec![
                Type::get_i32(),
                Type::get_pointer(arr_ty.clone()),
                Type::get_i32(),
                Type::get_pointer(Type::get_i32()),
                Type::get_i32(),
                Type::get_i32(),
                Type::get_pointer(Type::get_i32()),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        {
            let data = program.func_data_mut(callee);
            let entry = data.add_entry_block();
            let params: Vec<Inst> = data.params().to_vec();
            let sum = data
                .new_local_inst()
                .binary(BinaryOp::Add, params[0], params[2]);
            let sum = data.new_local_inst().binary(BinaryOp::Add, sum, params[4]);
            let sum = data.new_local_inst().binary(BinaryOp::Add, sum, params[5]);
            let sum = data.new_local_inst().binary(BinaryOp::Add, sum, params[7]);
            let sum = data.new_local_inst().binary(BinaryOp::Add, sum, params[8]);
            data.layout_mut().insert_inst(entry, sum);
            let ret = data.new_local_inst().ret(Some(sum));
            data.layout_mut().insert_inst(entry, ret);
        }

        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let (call, args) = {
            let ret_ty = program.func_data(callee).ret_ty().clone();
            let data = program.func_data_mut(main);
            let entry = data.add_entry_block();
            let args: Vec<_> = (1..=9).map(|v| data.new_local_inst().integer(v)).collect();
            let mut call_args = args.clone();
            // Array/pointer parameters receive distinct pointer values so a
            // positional shuffle is detectable; the scalar constants stay at
            // their slots.
            let arr_ptr = data.new_local_inst().alloc(arr_ty.clone());
            let int_ptr = data.new_local_inst().alloc(Type::get_i32());
            call_args[1] = arr_ptr;
            call_args[3] = int_ptr;
            call_args[6] = int_ptr;
            let call = data
                .new_local_inst()
                .call_with_type(callee, call_args, ret_ty);
            data.layout_mut().insert_inst(entry, call);
            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(entry, ret);
            (call, args)
        };
        let _ = call;
        let _ = args;

        assert!(Inline.run(&mut program));
        let data = program.func_data(main);
        // Find the jump into the cloned entry and check positional mapping.
        let mut found = false;
        for block in data.layout().basicblocks() {
            for &inst in block.insts() {
                if let InstKind::Jump(jump) = data.inst_data(inst).kind() {
                    let params = data.bb_data(jump.target()).params();
                    assert_eq!(params.len(), jump.args().len());
                    for (arg, param) in jump.args().iter().zip(params.iter()) {
                        assert_eq!(
                            data.inst_data(*arg).ty(),
                            data.inst_data(*param).ty(),
                            "arg/param type mismatch at position"
                        );
                    }
                    // The argument constants must keep their positional
                    // values (1..=9), i.e. no shuffle by the cloner.
                    for (i, arg) in jump.args().iter().enumerate() {
                        if let InstKind::Integer(int) = data.inst_data(*arg).kind() {
                            assert_eq!(int.value(), (i + 1) as i32, "argument {i} shuffled");
                        }
                    }
                    found = true;
                }
            }
        }
        assert!(found, "expected an inlined entry jump");
    }

    #[test]
    fn does_not_inline_a_leaf_with_a_large_total_callsite_cost() {
        // A leaf with several callsites is only inlined while the total
        // estimated size (size x callsites) stays within the budget. This
        // callee's body exceeds the per-call limit, so it stays a call.
        let mut program = Program::new();
        let callee = program.new_function(Type::get_i32(), "value".into(), vec![]);
        {
            let data = program.func_data_mut(callee);
            let entry = data.add_entry_block();
            let mut acc: Inst = data.new_local_inst().integer(0);
            for i in 1..=super::CALL_SIZE_LIMIT + 1 {
                let int = data.new_local_inst().integer(i as i32);
                let add = data.new_local_inst().binary(BinaryOp::Add, acc, int);
                data.layout_mut().insert_inst(entry, add);
                acc = add;
            }
            let ret = data.new_local_inst().ret(Some(acc));
            data.layout_mut().insert_inst(entry, ret);
        }
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        {
            let data = program.func_data_mut(main);
            let entry = data.add_entry_block();
            let first = data
                .new_local_inst()
                .call_with_type(callee, vec![], Type::get_i32());
            let second = data
                .new_local_inst()
                .call_with_type(callee, vec![], Type::get_i32());
            let add = data.new_local_inst().binary(BinaryOp::Add, first, second);
            let ret = data.new_local_inst().ret(Some(add));
            for inst in [first, second, add, ret] {
                data.layout_mut().insert_inst(entry, inst);
            }
        }

        assert!(!Inline.run(&mut program));
    }

    #[test]
    fn merges_multiple_returns_through_one_continuation() {
        let mut program = Program::new();
        let callee = program.new_function(Type::get_i32(), "choose".into(), vec![Type::get_i32()]);
        {
            let data = program.func_data_mut(callee);
            let entry = data.add_entry_block();
            let then_block = data.new_basic_block().basic_block("then".into(), vec![]);
            let else_block = data.new_basic_block().basic_block("else".into(), vec![]);
            data.layout_mut().push_bb_back(then_block);
            data.layout_mut().push_bb_back(else_block);
            let one = data.new_local_inst().integer(1);
            let two = data.new_local_inst().integer(2);
            let condition = data.params()[0];
            let branch =
                data.new_local_inst()
                    .branch(condition, then_block, vec![], else_block, vec![]);
            let then_ret = data.new_local_inst().ret(Some(one));
            let else_ret = data.new_local_inst().ret(Some(two));
            data.layout_mut().insert_inst(entry, branch);
            data.layout_mut().insert_inst(then_block, then_ret);
            data.layout_mut().insert_inst(else_block, else_ret);
        }
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        {
            let data = program.func_data_mut(main);
            let entry = data.add_entry_block();
            let condition = data.new_local_inst().integer(1);
            let call =
                data.new_local_inst()
                    .call_with_type(callee, vec![condition], Type::get_i32());
            let ret = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(entry, call);
            data.layout_mut().insert_inst(entry, ret);
        }

        assert!(Inline.run(&mut program));

        let data = program.func_data(main);
        let continuation = data.layout().basicblocks().get_last().unwrap().bb();
        let incoming_returns = data
            .layout()
            .basicblocks()
            .iter()
            .filter(|block| {
                matches!(
                    data.inst_data(block.terminator()).kind(),
                    InstKind::Jump(jump) if jump.target() == continuation && jump.args().len() == 1
                )
            })
            .count();
        assert_eq!(incoming_returns, 2);
    }

    #[test]
    fn does_not_inline_across_a_recursive_call_cycle() {
        let mut program = Program::new();
        let a = program.new_function(Type::get_i32(), "a".into(), vec![]);
        let b = program.new_function(Type::get_i32(), "b".into(), vec![]);
        {
            let data = program.func_data_mut(a);
            let entry = data.add_entry_block();
            let call = data
                .new_local_inst()
                .call_with_type(b, vec![], Type::get_i32());
            let ret = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(entry, call);
            data.layout_mut().insert_inst(entry, ret);
        }
        {
            let data = program.func_data_mut(b);
            let entry = data.add_entry_block();
            let call = data
                .new_local_inst()
                .call_with_type(a, vec![], Type::get_i32());
            let ret = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(entry, call);
            data.layout_mut().insert_inst(entry, ret);
        }
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        {
            let data = program.func_data_mut(main);
            let entry = data.add_entry_block();
            let call = data
                .new_local_inst()
                .call_with_type(a, vec![], Type::get_i32());
            let ret = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(entry, call);
            data.layout_mut().insert_inst(entry, ret);
        }

        assert!(!Inline.run(&mut program));
    }

    /// Build a leaf callee whose layout holds exactly `insts` instructions
    /// (`insts - 1` chained adds plus one return), so `estimate_size`
    /// reports `insts`.
    fn build_leaf(program: &mut Program, name: &str, insts: usize) -> Function {
        let callee = program.new_function(Type::get_i32(), name.into(), vec![]);
        let data = program.func_data_mut(callee);
        let entry = data.add_entry_block();
        let mut acc: Inst = data.new_local_inst().integer(0);
        for i in 1..insts {
            let int = data.new_local_inst().integer(i as i32);
            let add = data.new_local_inst().binary(BinaryOp::Add, acc, int);
            data.layout_mut().insert_inst(entry, add);
            acc = add;
        }
        let ret = data.new_local_inst().ret(Some(acc));
        data.layout_mut().insert_inst(entry, ret);
        callee
    }

    /// Build a `main` with a natural loop
    /// `entry -> header(i) -> body -> header` (branch on `i < 10`, `exit`
    /// returns 0). `body` is left empty for the caller to fill and must end
    /// with a jump back to `header`. Returns `(main, header, body)`.
    fn build_loop_main(program: &mut Program) -> (Function, BasicBlock, BasicBlock) {
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let data = program.func_data_mut(main);
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        data.layout_mut().push_bb_back(header);
        data.layout_mut().push_bb_back(body);
        data.layout_mut().push_bb_back(exit);

        let one = data.new_local_inst().integer(1);
        let ten = data.new_local_inst().integer(10);
        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![one]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let i = data.bb_data(header).params()[0];
        let cond = data.new_local_inst().binary(BinaryOp::Lt, i, ten);
        let branch = data
            .new_local_inst()
            .branch(cond, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, cond);
        data.layout_mut().insert_inst(header, branch);

        let ret = data.new_local_inst().ret(Some(zero));
        data.layout_mut().insert_inst(exit, ret);
        (main, header, body)
    }

    /// Finish a `body` produced by `build_loop_main`: append the given
    /// instructions, then increment the header induction variable and jump
    /// back to the header.
    fn finish_loop_body(
        program: &mut Program,
        main: Function,
        header: BasicBlock,
        body: BasicBlock,
        insts: &[Inst],
    ) {
        let data = program.func_data_mut(main);
        let i = data.bb_data(header).params()[0];
        let one = data.new_local_inst().integer(1);
        let increment = data.new_local_inst().binary(BinaryOp::Add, i, one);
        let backedge = data.new_local_inst().jump(header, vec![increment]);
        for &inst in insts {
            data.layout_mut().insert_inst(body, inst);
        }
        data.layout_mut().insert_inst(body, increment);
        data.layout_mut().insert_inst(body, backedge);
    }

    fn assert_main_has_no_calls(program: &Program, main: Function) {
        let data = program.func_data(main);
        assert!(data.layout().basicblocks().iter().all(|block| {
            block
                .insts()
                .iter()
                .all(|&inst| !matches!(data.inst_data(inst).kind(), InstKind::Call(..)))
        }));
    }

    #[test]
    fn inlines_an_oversized_callee_when_the_call_sits_in_a_natural_loop() {
        // A callee above CALL_SIZE_LIMIT is still inlined when its callsite
        // runs inside a natural loop: the removed call overhead is paid once
        // per iteration.
        let mut program = Program::new();
        let callee = build_leaf(&mut program, "big_leaf", super::CALL_SIZE_LIMIT + 1);
        let (main, header, body) = build_loop_main(&mut program);
        let call = program.func_data_mut(main).new_local_inst().call_with_type(
            callee,
            vec![],
            Type::get_i32(),
        );
        finish_loop_body(&mut program, main, header, body, &[call]);

        assert!(Inline.run(&mut program));
        assert!(!Inline.run(&mut program));
        assert_main_has_no_calls(&program, main);
    }

    #[test]
    fn does_not_inline_an_oversized_callee_outside_a_loop() {
        // The same oversized callee stays a call when its callsite is not
        // loop-resident: the relaxed budget must only apply to in-loop calls.
        let mut program = Program::new();
        let callee = build_leaf(&mut program, "big_leaf", super::CALL_SIZE_LIMIT + 1);
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        {
            let data = program.func_data_mut(main);
            let entry = data.add_entry_block();
            let call = data
                .new_local_inst()
                .call_with_type(callee, vec![], Type::get_i32());
            let ret = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(entry, call);
            data.layout_mut().insert_inst(entry, ret);
        }

        assert!(!Inline.run(&mut program));
    }

    #[test]
    fn inlines_a_multi_callsite_callee_when_its_callsites_sit_in_a_loop() {
        // 34 insts x 3 callsites = 102 exceeds TOTAL_SIZE_LIMIT (100), so
        // the strict model rejects it; the in-loop total budget covers it.
        let mut program = Program::new();
        let callee = build_leaf(&mut program, "leaf", 34);
        let (main, header, body) = build_loop_main(&mut program);
        let data = program.func_data_mut(main);
        let calls: Vec<_> = (0..3)
            .map(|_| {
                data.new_local_inst()
                    .call_with_type(callee, vec![], Type::get_i32())
            })
            .collect();
        finish_loop_body(&mut program, main, header, body, &calls);

        assert!(Inline.run(&mut program));
        assert!(!Inline.run(&mut program));
        assert_main_has_no_calls(&program, main);
    }

    #[test]
    fn does_not_inline_a_multi_callsite_callee_outside_a_loop() {
        // The same 34-inst x 3-callsite shape outside any loop stays a call
        // because 102 > TOTAL_SIZE_LIMIT and no loop relaxes the budget.
        let mut program = Program::new();
        let callee = build_leaf(&mut program, "leaf", 34);
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        {
            let data = program.func_data_mut(main);
            let entry = data.add_entry_block();
            let first = data
                .new_local_inst()
                .call_with_type(callee, vec![], Type::get_i32());
            let second = data
                .new_local_inst()
                .call_with_type(callee, vec![], Type::get_i32());
            let third = data
                .new_local_inst()
                .call_with_type(callee, vec![], Type::get_i32());
            let add = data.new_local_inst().binary(BinaryOp::Add, first, second);
            let add2 = data.new_local_inst().binary(BinaryOp::Add, add, third);
            let ret = data.new_local_inst().ret(Some(add2));
            for inst in [first, second, third, add, add2, ret] {
                data.layout_mut().insert_inst(entry, inst);
            }
        }

        assert!(!Inline.run(&mut program));
    }
}
