//! # TailRecursiveInline：自尾递归函数内联（把 `call F` 变成调用者内的循环）
//!
//! 一句话定位：**纯自尾递归函数 F 的调用点就地内联**——把 F 的循环体克隆进
//! 调用者，F 体内的自 `tail_call` 变成指向克隆入口的回边，原 `call` 消失，
//! 与 clang 的做法一致（clang 把循环内联、深度计数留在寄存器里，热循环内零
//! 调用）。动机来自 h-1 系列：`fun` 这类纯自尾递归函数经 TCO 后形如
//! "分支繁多的回边循环 + 每个调用点一次 `bl F`"，内联后调用点成本降到零。
//! 术语（SSA、Phi（块参数）、TCO/尾调用、header/latch/preheader/backedge、
//! 固定点、支配 等）：见 `docs/offline-handbook/glossary.md`，不在本文展开。
//!
//! ## 变换形态（IR 示例）
//!
//! 变换前：被调用者 F 已是 TCO 后的自尾递归（`tail_call` 在 AArch64 后端降为
//! 帧复用跳转 `b F`），调用者里是一次普通 `call`：
//!
//! ```text
//! F(n, dep):                        // h-1 家族形态（示意图）
//!   entry:  br n == 1, base, odd
//!   odd:    br n % 2 == 0, even, base
//!   base:   ret dep
//!   even:   j = n / 2; d = dep + 1; tail_call F(j, d)   // 自尾调用
//!
//! main:
//!   entry:  v = call F(7, 0); ret v
//! ```
//!
//! 内联后（`apply`）：`split_block_after` 在 call 之后切出 continuation（原
//! call 之后的指令全部移入）；克隆的 F 体接到 call 块之后，原 `call` 删除、
//! call 块末尾改为跳进克隆入口；克隆里的 `ret` 跳 continuation、`tail_call`
//! 跳回克隆入口（回边）：
//!
//! ```text
//! main:
//!   entry:  jump F_loop(7, 0)       // 原 call 的参数原样传入
//!   F_loop(n, dep):                 // 克隆的 F 入口：块参数即循环头 Phi
//!     br n == 1, base, odd
//!   odd:    br n % 2 == 0, even, base
//!   base:   jump main_tailrec_cont(dep)       // 克隆的 ret → continuation
//!   even:   j = n / 2; d = dep + 1; jump F_loop(j, d)   // 自尾调用 → 回边
//!   main_tailrec_cont(v):           // 携带返回值的 continuation
//!     ret v                         // 原 call 之后的指令都留在这里
//! ```
//!
//! 变换后调用者内不再有任何对 F 的调用；F 若不再被引用，后续
//! dead-function-elimination 会整函数删掉它。
//!
//! ## 触发 / 放弃条件（`find_candidate`，宁漏勿错）
//!
//! 扫描所有函数所有基本块的 `Call`（`callee == caller` 的自调用直接跳过，
//! 防止自身无限内联），以下全部满足才内联，任一不满足保留原 `call`：
//!
//! - **纯自尾递归循环**（`is_self_tail_recursive_loop`）：被调用者必须有
//!   entry；体内所有 `TailCall` 都指向 F 自身且至少一个；出现**任何**非尾
//!   `Call`（无论调用 F 自己还是别的函数）立即拒绝——这一条保证克隆体内
//!   的自调用全部变成回边、不残留对 F 的调用，内联必然终止；
//! - **类型匹配**：call 结果类型 == F 的返回类型，实参数目 ==
//!   `params_ty()` 长度，实参类型逐一等于形参类型；
//! - **unit 结果必须无人使用**：结果类型是 unit 时 `used_by()` 必须为空
//!   （否则返回值无法通过 continuation 回传，只能放弃）；
//! - **大小预算**：`estimate_size`（各基本块指令数之和）≤ `CALL_SIZE_LIMIT`
//!   （40，与通用 inline pass 同款的按调用点克隆预算）；
//! - **克隆预检**：`BodyClonePlan::capture` 成功且 `plan.contains_tail_call()`。
//!
//! ## 正确性
//!
//! - 纯自循环保证 F 不调用任何其他函数 → 克隆副本除自身参数派生的 load 外
//!   无副作用，可安全复制进调用者；
//! - 返回路径：克隆的 `ret v` → `jump continuation(v)`，原 call 之后的所有
//!   指令原封不动留在 continuation 里继续执行；continuation 的块参数携带
//!   返回值（SSA 块参数即 Phi，值流唯一），非 unit 结果时用
//!   `visit_and_replace` 把 call 的所有使用者替换成该参数；
//! - unit 结果在触发阶段已保证无人使用，删除 call 前再 `assert` 一遍
//!   `used_by` 为空；
//! - `run` 每轮只内联第一个候选（`find_candidate` 返回首个），内联完重新
//!   扫描直到无候选；call 消失后第二轮 `run` 返回 false（幂等，测试断言）。
//!
//! ## 管线位置与门控
//!
//! - 注册：`opt/pass.rs` 的 `PassesManager::from_config`，**fixpoint 段**，
//!   `if_conversion` → 第二次 `tco`（`TailCallElim`）之后、
//!   `boolean_simplification` 之前；
//! - 与 TCO 衔接：TCO 把尾递归降为 `TailCall`（后端降为帧复用跳转 `b F`），
//!   本 pass 依赖候选体已是"纯 `TailCall` 自循环"形态——
//!   `is_self_tail_recursive_loop` 只认 `TailCall`，所以必须排在 TCO 之后
//!   （注册处备注 "runs after TCO so the loop form is visible"；该 TCO 是
//!   第二次，捕获取消前面 simplify pass 新暴露的尾调用）；initial 段还有一次
//!   TCO（inline 之后、column_major 之前），保证早期尾调用先变成 `TailCall`；
//! - 无 config 开关、无目标门控（注册处没有 `enable_chain_to_switch` 条件，
//!   AArch64 / RISC-V 都跑）。
//!
//! ## 验证
//!
//! - 本文件 `mod tests`（218 行起）：`build_loop_function` 构造 h-1 家族的
//!   `fun(n, dep)`（`n==1` 返回 `dep`，`n` 为偶数时自尾调用
//!   `fun(n/2, dep+1)`，否则返回 `dep`）；
//!   `inlines_self_tail_recursive_call_into_loop` 断言第一轮 `run` 返回 true、
//!   第二轮 false，调用方内无任何 `Call`，且存在一个"双参数（n/dep）循环头"
//!   被回边 `Jump` 指向；`refuses_a_function_with_a_non_tail_call` 断言含非尾
//!   `Call`（调用 `other`）的候选被拒绝；
//! - 端到端：`make test` 差分比对（h-1 家族用例在 `-O2` 下调用点消失、回边
//!   循环留在调用者内）；单元测试用 `cargo test -p raana_ir`。

use crate::{
    ir::arena::Arena,
    opt::{prelude::*, utils::body_clone::BodyClonePlan},
};

/// Inline a call to a pure self-tail-recursive function, turning the callee's
/// self `tail_call` into a loop back-edge inside the caller.
///
/// A self-tail-recursive function F (whose body is a pure loop after TCO, e.g.
/// the `fun` in h-1-01) is lowered to a branch-heavy loop with a `bl F` per
/// call site. clang instead inlines the loop and keeps the depth count in
/// registers, so the caller's hot loop contains the whole recursion with zero
/// calls. This pass does the same: it clones F's body at `call F(args)` and
/// rewrites
///
///   * every cloned `ret v`            -> `jump continuation(v)`
///   * every cloned `tail_call F(args)` -> `jump cloned_entry(args)` (back-edge)
///   * the original call               -> `jump cloned_entry(args)`
///
/// Guards (宁漏勿错):
///   * F must be a pure self-loop: every `Call`/`TailCall` in its body targets
///     F itself, at least one is a `TailCall`, and none is a non-tail `Call`.
///     That makes the clone's self-calls all become back-edges, so the inline
///     terminates (no call to F remains in the clone).
///   * F must not call any other function (keeps the clone side-effect free
///     apart from its own argument-derived loads).
///   * F is small enough to clone (bounded static cost).
pub struct TailRecursiveInline;

/// Per-callsite clone budget (estimated instructions of the callee body),
/// matching the general inline pass.
const CALL_SIZE_LIMIT: usize = 40;

fn estimate_size(data: &FunctionData) -> usize {
    data.layout()
        .basicblocks()
        .iter()
        .map(|layout| layout.insts().len())
        .sum()
}

/// True when `f` is a pure self-tail-recursive loop: its body only calls `f`
/// itself, all such calls are `TailCall`, and there is at least one.
fn is_self_tail_recursive_loop(program: &Program, f: Function) -> bool {
    let data = program.func_data(f);
    if data.layout().entry_bb().is_none() {
        return false;
    }
    let mut has_self_tail_call = false;
    for bb in data.layout().basicblocks() {
        for &inst in bb.insts() {
            match data.inst_data(inst).kind() {
                InstKind::TailCall(tc) => {
                    if tc.callee() != f {
                        return false;
                    }
                    has_self_tail_call = true;
                }
                InstKind::Call(c) => {
                    // A non-tail self call (or any call to another function)
                    // disqualifies the pure-loop shape.
                    let _ = c;
                    return false;
                }
                _ => {}
            }
        }
    }
    has_self_tail_call
}

struct Candidate {
    caller: Function,
    call_inst: Inst,
    call_block: BasicBlock,
    args: Vec<Inst>,
    ret_ty: Type,
    plan: BodyClonePlan,
}

impl Pass for TailRecursiveInline {
    fn run(&mut self, program: &mut Program) -> bool {
        let mut changed = false;
        while let Some(candidate) = Self::find_candidate(program) {
            Self::apply(program, candidate);
            changed = true;
        }
        changed
    }
}

impl TailRecursiveInline {
    fn find_candidate(program: &Program) -> Option<Candidate> {
        let funcs: Vec<Function> = program.function_layout().to_vec();
        for &caller in &funcs {
            let caller_data = program.func_data(caller);
            if caller_data.layout().entry_bb().is_none() {
                continue;
            }
            for layout in caller_data.layout().basicblocks() {
                let call_block = layout.bb();
                for &inst in layout.insts() {
                    let InstKind::Call(call) = caller_data.inst_data(inst).kind() else {
                        continue;
                    };
                    let callee = call.callee();
                    if callee == caller {
                        continue;
                    }
                    if !is_self_tail_recursive_loop(program, callee) {
                        continue;
                    }
                    let callee_data = program.func_data(callee);
                    if *caller_data.inst_data(inst).ty() != *callee_data.ret_ty()
                        || call.args().len() != callee_data.params_ty().len()
                        || call
                            .args()
                            .iter()
                            .zip(callee_data.params_ty())
                            .any(|(&arg, param_ty)| caller_data.inst_data(arg).ty() != param_ty)
                    {
                        continue;
                    }
                    if caller_data.inst_data(inst).ty().is_unit()
                        && !caller_data.inst_data(inst).used_by().is_empty()
                    {
                        continue;
                    }
                    if estimate_size(callee_data) > CALL_SIZE_LIMIT {
                        continue;
                    }
                    let Ok(plan) = BodyClonePlan::capture(program, callee) else {
                        continue;
                    };
                    if !plan.contains_tail_call() {
                        continue;
                    }
                    return Some(Candidate {
                        caller,
                        call_inst: inst,
                        call_block,
                        args: call.args().to_vec(),
                        ret_ty: callee_data.ret_ty().clone(),
                        plan,
                    });
                }
            }
        }
        None
    }

    fn apply(program: &mut Program, candidate: Candidate) {
        let Candidate {
            caller,
            call_inst,
            call_block,
            args,
            ret_ty,
            plan,
        } = candidate;

        let continuation = {
            let data = program.func_data_mut(caller);
            let block_name = data.bb_data(call_block).name().to_owned();
            let params = if ret_ty.is_unit() {
                vec![]
            } else {
                vec![ret_ty]
            };
            data.split_block_after(call_inst, format!("{block_name}_tailrec_cont"), params)
        };

        let cloned = plan
            .clone_into(program, caller, call_block)
            .expect("preflighted self-tail-recursive body must clone successfully");

        let mut context = ArenaContextMut {
            program,
            curr_func: Some(caller),
        };

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

        // Self tail-calls become back-edges to the inlined loop entry.
        for tail in cloned.tail_calls {
            let tail_args = match context.inst_data(tail).kind() {
                InstKind::TailCall(tc) => tc.args().to_vec(),
                _ => unreachable!("cloner returned a non-tail-call instruction"),
            };
            context
                .replace_inst_with(tail)
                .jump(cloned.entry, tail_args);
        }

        if !context.inst_data(call_inst).ty().is_unit() {
            let continuation_result = context.bb_data(continuation).params()[0];
            utils::visit_and_replace(&mut context, call_inst, continuation_result);
        }
        assert!(
            context.inst_data(call_inst).used_by().is_empty(),
            "call must have no users before removal"
        );
        context.remove_layout_inst(call_block, call_inst);
        let entry_jump = context.new_local_value().jump(cloned.entry, args);
        context.layout_mut().insert_inst(call_block, entry_jump);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{BinaryOp, InstKind, Program, Type, arena::Arena, builder_trait::*},
        opt::pass::Pass,
    };

    /// Build the h-1 `fun(n, dep)` shape in `program`: `if n==1 return dep;
    /// else if n even return fun(n/2, dep+1); else return 7`, with a self
    /// tail call. Returns the `fun` function handle.
    fn build_loop_function(program: &mut Program) -> Function {
        let fun = program.new_function(
            Type::get_i32(),
            "fun".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        {
            let data = program.func_data_mut(fun);
            let entry = data.add_entry_block();
            let then_bb = data.new_basic_block().basic_block("then".into(), vec![]);
            let else_bb = data.new_basic_block().basic_block("else".into(), vec![]);
            let even_bb = data.new_basic_block().basic_block("even".into(), vec![]);
            for bb in [then_bb, else_bb, even_bb] {
                data.layout_mut().push_bb_back(bb);
            }
            let params = data.params().to_vec();
            let n = params[0];
            let dep = params[1];
            let one = data.new_local_inst().integer(1);
            let eq_one = data.new_local_inst().binary(BinaryOp::Eq, n, one);
            let entry_term = data
                .new_local_inst()
                .branch(eq_one, then_bb, vec![], else_bb, vec![]);
            data.layout_mut().insert_inst(entry, entry_term);
            let ret_dep = data.new_local_inst().ret(Some(dep));
            data.layout_mut().insert_inst(then_bb, ret_dep);

            let two = data.new_local_inst().integer(2);
            let rem = data.new_local_inst().binary(BinaryOp::Rem, n, two);
            let rem_zero = data.new_local_inst().integer(0);
            let is_even = data.new_local_inst().binary(BinaryOp::Eq, rem, rem_zero);
            let else_term = data
                .new_local_inst()
                .branch(is_even, even_bb, vec![], then_bb, vec![]);
            data.layout_mut().insert_inst(else_bb, else_term);

            let half = data.new_local_inst().binary(BinaryOp::Div, n, two);
            let dep_plus = data.new_local_inst().binary(BinaryOp::Add, dep, one);
            let tail = data.new_local_inst().tail_call(fun, vec![half, dep_plus]);
            data.layout_mut().insert_inst(even_bb, tail);
        }
        fun
    }

    /// A caller that calls `fun(n, 0)` once and returns the result.
    fn add_caller(program: &mut Program, fun: Function) {
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let data = program.func_data_mut(main);
        let entry = data.add_entry_block();
        let seven = data.new_local_inst().integer(7);
        let zero = data.new_local_inst().integer(0);
        let call = data
            .new_local_inst()
            .call_with_type(fun, vec![seven, zero], Type::get_i32());
        let ret = data.new_local_inst().ret(Some(call));
        data.layout_mut().insert_inst(entry, call);
        data.layout_mut().insert_inst(entry, ret);
    }

    #[test]
    fn inlines_self_tail_recursive_call_into_loop() {
        let mut program = Program::new();
        let fun = build_loop_function(&mut program);
        add_caller(&mut program, fun);

        assert!(TailRecursiveInline.run(&mut program));
        assert!(!TailRecursiveInline.run(&mut program));

        let data = program.func_data(program.function_layout()[1]);
        // The call is gone; a continuation block and the cloned loop remain.
        assert!(
            data.layout()
                .basicblocks()
                .iter()
                .flat_map(|l| l.insts().iter().copied())
                .all(|inst| !matches!(data.inst_data(inst).kind(), InstKind::Call(..)))
        );
        // The cloned self tail-call must now be a back-edge jump to the
        // cloned entry block: a jump whose target block takes the function's
        // two parameters (n, dep), i.e. a loop header.
        let loop_header = data
            .layout()
            .basicblocks()
            .iter()
            .find(|l| data.bb_data(l.bb()).params().len() == 2)
            .map(|l| l.bb());
        assert!(loop_header.is_some(), "expected a two-param loop header");
        let header = loop_header.unwrap();
        let has_back_edge = data.layout().basicblocks().iter().any(|l| {
            l.insts().iter().any(|&inst| {
                matches!(data.inst_data(inst).kind(), InstKind::Jump(j) if j.target() == header)
            })
        });
        assert!(
            has_back_edge,
            "expected a loop back-edge jump to the header"
        );
    }

    #[test]
    fn refuses_a_function_with_a_non_tail_call() {
        let mut program = Program::new();
        let other = program.new_function(Type::get_i32(), "other".into(), vec![]);
        {
            let data = program.func_data_mut(other);
            let entry = data.add_entry_block();
            let zero = data.new_local_inst().integer(0);
            let ret = data.new_local_inst().ret(Some(zero));
            data.layout_mut().insert_inst(entry, ret);
        }
        // fun tail-calls itself but also contains a regular call to `other`,
        // so it is not a pure self-loop and must be left as a call.
        let fun = program.new_function(
            Type::get_i32(),
            "fun".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        {
            let data = program.func_data_mut(fun);
            let entry = data.add_entry_block();
            let params = data.params().to_vec();
            let other_call = data
                .new_local_inst()
                .call_with_type(other, vec![], Type::get_i32());
            let tail = data
                .new_local_inst()
                .tail_call(fun, vec![params[0], other_call]);
            data.layout_mut().insert_inst(entry, other_call);
            data.layout_mut().insert_inst(entry, tail);
        }
        add_caller(&mut program, fun);

        assert!(!TailRecursiveInline.run(&mut program));
    }
}
