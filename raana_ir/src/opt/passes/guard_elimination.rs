//! Fold `br (x < 0), A, B` when `x` is provably `>= 0` (take `B`).
//!
//! The M60 `mulmod_recognize` rewrite emits a `b < 0` guard in front of every
//! `soyo_mulmod(a, b, p)` call to preserve the original recursion's "negative
//! `b` returns 0" semantics. Inlined into its callers, that guard is a
//! `cmp; b.lt` on the hot butterfly path. The `return_summary` analysis proves
//! which values (including function parameters that every call site passes
//! non-negative) are always `>= 0`, so guards whose second argument flows from
//! such a value fold away. Guards on input-derived values (array elements)
//! stay.
//!
//! The program-level summaries are computed once per run and shared across
//! functions.
//!
//! ---
//!
//! ## 补充说明（中文）
//!
//! ### 定位与动机
//!
//! M60 `mulmod_recognize` 把 FFT/NTT 的"倍增式"模乘递归重写为 `soyo_mulmod`
//! 内建调用，并在每次调用前插一条 `br (b < 0)` 守卫，以保留原递归"负数 `b`
//! 返回 0"的语义。内联进调用方后，这条守卫变成热蝴蝶路径上的 `cmp; b.lt`
//! 条件分支。本 pass（配合 M61 的 `return_summary` 非负性摘要）在能证明守卫
//! 条件恒假时把它折成直跳，从热路径上删掉这次比较与分支。
//!
//! 术语：pass / fixpoint / TargetPolicy 等见 `docs/offline-handbook/glossary.md`
//! 的"优化与 pass 概念"与"本项目特有"分组；guard / return_summary / 非负性
//! 为本项目内部概念，含义见下文与 `opt/analysis_passes/return_summary.rs`。
//!
//! ### 变换形态
//!
//! 对每个基本块的终结符匹配形如
//!
//! ```text
//! br (x < 0), A, B
//! ```
//!
//! 的 `Branch`（条件必须是 `BinaryOp::Lt`、右操作数必须是整型常量 `0`）；若
//! `x` 可证恒 `>= 0`，则 `x < 0` 恒假、false 边（`B`）恒取，把终结符替换为
//!
//! ```text
//! jump B
//! ```
//!
//! 携带原 false 边的 block 参数（`f_args`）。实现是 `replace_inst_with(inst)
//! .jump(target, args)`，只改终结符；`x < 0` 的比较指令随后成为死代码，由
//! 管线里的 DCE 清理。
//!
//! ### 触发 / 放弃条件
//!
//! - 形状闸门：终结符必须是 `Branch`；条件必须是 `BinaryOp::Lt`（仅此一种，
//!   `x <= -1` 等变体不匹配）；右操作数必须是整型常量 `0`（`InstKind::Integer`
//!   且 `value() == 0`）。
//! - 非负性闸门：`x` 必须落在 `nonneg_in_function` 的函数内事实集里。事实
//!   来源（见 `return_summary.rs`）：非负常量；**每个调用点**都可证 `>= 0`
//!   的入口参数（`always_nonneg_params`，greatest fixpoint）；非负和/积/余/
//!   select；两个非负操作数的 `soyo_mulmod`（`(i64)a * b % p` 对非负被除数
//!   取截断余数不会变负，是基例）；对非负保持函数（`nonneg_preserving_
//!   functions`，co-inductive fixpoint，自递归的 `power` 也能从自己的递归
//!   调用点证明）且实参全非负的调用；block 参数取各入边事实的 meet。
//!   `multiply` / `power` 的 `a` / `b` 参数由此继承非负性，其守卫随之折叠。
//! - 放弃（守卫保留）：`x` 来自输入派生值（如数组元素的 load）时无法证明
//!   非负，守卫保留——两个摘要都是**条件式**的，调用方喂入可能为负的值就
//!   保守处理。声明（`is_decl`）函数直接跳过。
//!
//! ### 正确性
//!
//! i32 上 `x >= 0` ⟹ `x < 0` 恒假，分支必然走 false 边；换成对 false 边目标
//! 的无条件 `jump`（携带原 block 参数）行为逐点一致。守卫存在的意义是保住
//! 原递归"负数 `b` 返回 0"的语义——折叠只在证明"不可能取负"时才发生，不改变
//! 任何可观察行为；证明不了的守卫原样保留。程序级摘要（`nonneg_preserving_
//! functions`、`always_nonneg_params`）每次 `run` 只算一遍、跨函数共享；
//! 函数内事实（`nonneg_in_function`）逐函数重算。fixpoint 多轮迭代重复执行
//! 是幂等的：折掉的守卫不会再生。
//!
//! ### 管线位置
//!
//! - 注册：`opt/pass.rs` 的 `PassesManager::from_config`，fixpoint 段，
//!   `pointer_strength_reduction` **之后**、`mod_fold` **之前**（pass.rs
//!   282–285 行）；`mod_fold` 的注册注释说明它要等守卫折完后 range 事实
//!   稳定再跑。
//! - 无目标门控、无 config 开关（AArch64 / RISC-V 都注册）；守卫的源头
//!   `mulmod_recognize`（M60）是 AArch64-only（pass.rs 195 行
//!   `enable_chain_to_switch` 门控），RISC-V 上只有用户代码自身的 `x < 0`
//!   分支可能被折（形态通用、语义保持）。
//!
//! ### 验证
//!
//! - 本文件 `mod tests`（82 行起）：`folds_guard_on_constant_nonneg_and_keeps_
//!   param_guard` 覆盖"常量非负守卫折成 `Jump`、输入派生参数守卫保持
//!   `Branch`"（`main` 用 `load` 调 `f_param`，证明不了非负）；
//! - 全量：`cargo test -p raana_ir`（单元测试）+ `make test`（Docker harness
//!   差分比对）。

use crate::{
    ir::{BasicBlock, BinaryOp, Inst, InstKind, Program},
    opt::{
        analysis_passes::return_summary::{
            always_nonneg_params, nonneg_in_function, nonneg_preserving_functions,
        },
        pass::Pass,
        prelude::*,
    },
};

pub struct GuardElimination;

impl Pass for GuardElimination {
    fn run(&mut self, program: &mut Program) -> bool {
        // 程序级摘要每个 run 只算一遍、跨函数共享（实现见 `return_summary.rs`）：
        // 先求"非负保持函数"集合（co-inductive fixpoint），
        let nonneg = nonneg_preserving_functions(program);
        // 再求"每个调用点实参都可证 >= 0"的入口参数（greatest fixpoint）。
        // 两者都是条件式摘要：某调用点喂入输入派生值（如数组元素 load）时证明
        // 不了非负，就保守地不纳入——这是本 pass 的保守边界。
        let params = always_nonneg_params(program, &nonneg);

        let mut changed = false;
        // 先把函数列表快照出来：后面要对 `program` 取 &mut 改写终结符，
        // 不能继续持有对 `function_layout` 的借用。
        let funcs: Vec<Function> = program.function_layout().to_vec();
        for &f in &funcs {
            let data = program.func_data(f);
            // 声明（无函数体）里没有可折的守卫，直接跳过。
            if data.layout().is_decl() {
                continue;
            }
            // 本函数"每个调用点都 >= 0"的入口参数作为种子……
            let self_params = params.get(&f).cloned().unwrap_or_default();
            // ……跑一遍函数内前向数据流，得到本函数所有可证 >= 0 的值（facts）。
            // 这是守卫判定的唯一依据：`facts` 里没有 x ⟹ 证明不了 ⟹ 守卫保留。
            let facts = nonneg_in_function(program, f, &self_params, &nonneg);

            // 第一阶段只读扫描，收集要折的守卫；改写统一放到第二阶段，
            // 避免在持有 `facts`/`data` 只读借用时对 `program` 取 &mut。
            let mut folds: Vec<(Inst, BasicBlock, Vec<Inst>)> = Vec::new();
            for layout in data.layout().basicblocks() {
                // 守卫是块的终结符（最后一条指令），只需检查它。
                let terminator = *layout.insts().get_last().unwrap();
                let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
                    continue;
                };
                let cond = branch.cond();
                // 形状闸门 1：条件必须是 `x < 0`（`BinaryOp::Lt`），
                // `x <= -1` 等语义等价但形态不同的比较一律不匹配。
                let Some((BinaryOp::Lt, x, zero)) = binary_of(data, cond) else {
                    continue;
                };
                // 形状闸门 2：右操作数必须是整型常量 0。
                if !matches!(
                    data.inst_data(zero).kind(),
                    InstKind::Integer(int) if int.value() == 0
                ) {
                    continue;
                }
                // 非负性闸门（核心判定）：`x` 在 facts 里 ⟹ `x >= 0` 恒真 ⟹
                // `x < 0` 恒假，分支必然走 false 边；记下"折成对 false 边目标
                // 的无条件 jump、携带原 false 边 block 参数"的改写。
                if facts.contains(&x) {
                    // `x < 0` is false: always take the false edge.
                    // （保留英文）三元组 = (终结符, 目标块, 随边参数)。
                    folds.push((terminator, branch.f_target(), branch.f_args().to_vec()));
                }
            }

            // 第二阶段：把收集到的分支替换成 `jump B`（携带原 false 边的参数）。
            // `replace_inst_with` 只换终结符，`x < 0` 的比较指令随即成为死代码，
            // 由管线后续的 DCE 清理；折掉的守卫不会再生，fixpoint 重跑幂等。
            let data = program.func_data_mut(f);
            for (inst, target, args) in folds {
                data.replace_inst_with(inst).jump(target, args);
                changed = true;
            }
        }
        // 返回"是否改动了 IR"，供 fixpoint 框架判断是否收敛；改过才会再跑一轮。
        changed
    }
}

/// 把指令解包成 (二元运算, 左操作数, 右操作数)；非二元指令返回 `None`。
/// 本 pass 用它做守卫条件的形状匹配（只认 `Lt` 比较）。
fn binary_of(data: &crate::ir::FunctionData, inst: Inst) -> Option<(BinaryOp, Inst, Inst)> {
    match data.inst_data(inst).kind() {
        InstKind::Binary(binary) => Some((binary.op(), binary.lhs(), binary.rhs())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{Program, Type, arena::Arena, builder_trait::*},
        opt::pass::Pass as _,
    };

    /// `g(x)`: `br (x < 0), neg, nonneg; neg: ret 0; nonneg: ret x`.
    fn build_guard(program: &mut Program, name: &str, use_param: bool) -> (Function, Inst) {
        let f = program.new_function(Type::get_i32(), name.into(), vec![Type::get_i32()]);
        let (branch, _) = {
            let data = program.func_data_mut(f);
            let entry = data.add_entry_block();
            let neg = data.new_basic_block().basic_block("neg".into(), vec![]);
            let pos = data.new_basic_block().basic_block("pos".into(), vec![]);
            data.layout_mut().push_bb_back(neg);
            data.layout_mut().push_bb_back(pos);
            let x = data.params()[0];
            let guard = if use_param {
                x
            } else {
                let five = data.new_local_inst().integer(5);
                data.layout_mut().insert_inst(entry, five);
                five
            };
            let zero = data.new_local_inst().integer(0);
            let cond = data.new_local_inst().binary(BinaryOp::Lt, guard, zero);
            let branch = data.new_local_inst().branch(cond, neg, vec![], pos, vec![]);
            data.layout_mut().insert_inst(entry, cond);
            data.layout_mut().insert_inst(entry, branch);
            let ret_zero = data.new_local_inst().ret(Some(zero));
            data.layout_mut().insert_inst(neg, ret_zero);
            let ret_x = data.new_local_inst().ret(Some(x));
            data.layout_mut().insert_inst(pos, ret_x);
            (branch, x)
        };
        (f, branch)
    }

    #[test]
    fn folds_guard_on_constant_nonneg_and_keeps_param_guard() {
        let mut program = Program::new();
        let (constant_guard, const_branch) = build_guard(&mut program, "f_const", false);
        let (param_guard, param_branch) = build_guard(&mut program, "f_param", true);

        // `main` calls `f_param(load)` (input-derived), so the parameter guard
        // must stay; the constant guard is provably never taken.
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        {
            let data = program.func_data_mut(main);
            let entry = data.add_entry_block();
            let ptr = data.new_local_inst().alloc(Type::get_i32());
            let load = data.new_local_inst().load(ptr);
            let call =
                data.new_local_inst()
                    .call_with_type(param_guard, vec![load], Type::get_i32());
            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(entry, load);
            data.layout_mut().insert_inst(entry, call);
            data.layout_mut().insert_inst(entry, ret);
        }

        let mut pass = GuardElimination;
        pass.run(&mut program);

        let data = program.func_data(constant_guard);
        assert!(
            matches!(data.inst_data(const_branch).kind(), InstKind::Jump(_)),
            "constant guard must fold to a jump"
        );
        let data = program.func_data(param_guard);
        assert!(
            matches!(data.inst_data(param_branch).kind(), InstKind::Branch(_)),
            "input-derived guard must stay a branch"
        );
    }
}
