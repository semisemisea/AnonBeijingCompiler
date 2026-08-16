//! # TailCallElim：尾调用消除（TCO）
//!
//! 把满足条件的尾调用改写成 `TailCall` 指令，后端将其降为**复用调用者栈帧
//! 的跳转**（`b callee`）——不压栈、不保存返回地址，无限尾递归变成循环。
//! 详情见 `TailCallElim` 的英文文档；中文要点如下。
//!
//! ## 触发条件（全部满足才改写）
//!
//! 1. 块的**最后一条**非终结符指令是 `call`，紧跟 `ret`（邻接性保证 call
//!    的副作用在 ret 前无人观察）；
//! 2. **签名完全一致**：调用者与被调用者的返回类型、参数类型列表相同，
//!    call 的结果类型 = 被调用者返回类型、实参数目与类型逐一匹配——后端
//!    才能安全复用调用者的入参位置；
//! 3. 尾调用形态二选一：
//!    - **值尾调用**：`%t = call callee(args); ret %t`（ret 值就是 call
//!      结果）；
//!    - **空尾调用**：`call callee(args); ret`（call 结果类型为 unit 且
//!      无人使用）。
//!
//! 改写方式：移除 call 与 ret，插入 `tail_call callee(args)`。
//!
//! ## 正确性
//!
//! - 签名一致 → 参数位置可直接复用，无需搬移；
//! - 邻接性 → call 与 ret 之间没有观察副作用的机会；
//! - 值尾调用要求 ret 值恰好是 call 结果，杜绝"还要处理返回值"的路径。
//!
//! ## 管线位置
//!
//! - 注册两次：`opt/pass.rs` 的 `from_config`——**initial 段**一次
//!   （inline 之后、column_major 之前）+ **fixpoint 段**一次（
//!   `if_conversion` 之后、`tail_recursive_inline` 之前，捕获取消尾调用
//!   的 pass 新暴露的尾调用）；
//! - 与 `tail_recursive_inline` 衔接：TCO 把自尾递归变成回边循环形态，
//!   后者再内联成调用者里的循环；
//! - 无目标门控、无 config 开关。
//!
//! ## 验证
//!
//! - 本文件 `mod tests`（100 行起）覆盖值/空尾调用、签名不匹配、非邻接
//!   拒绝等；
//! - 端到端：`make test` 差分比对 + 汇编检查（`b callee` 而非 `bl`）。

use crate::opt::prelude::*;

/// Rewrites ABI-compatible tail calls into a [`crate::ir::inst_kind::TailCall`], which the backend
/// lowers to a frame-reusing jump (`b callee`).
///
/// The caller and callee must currently have identical signatures so the
/// backend can safely reuse the caller's incoming argument locations. The call
/// must also be immediately before the `ret`; adjacency guarantees nothing
/// observes the call's side effects between it and the return.
///
/// 1. **Value tail call**: `%t = call callee(args); ret %t`.
/// 2. **Void tail call**: `call callee(args); ret`
///    (call result is unit, no other users).
pub struct TailCallElim;

impl Pass for TailCallElim {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        let curr_func = data.curr_func.unwrap();
        if data.layout().entry_bb().is_none() {
            return false;
        }

        let blocks: Vec<BasicBlock> = data
            .layout()
            .basicblocks()
            .iter()
            .map(|layout| layout.bb())
            .collect();

        // Collect rewrites before mutating so iteration stays borrow-clean.
        let mut rewrites: Vec<(BasicBlock, Inst, Vec<Inst>)> = Vec::new();
        for bb in blocks {
            let insts = data.layout().basicblock(bb).insts();
            let Some(&terminator) = insts.get_last() else {
                continue;
            };
            let InstKind::Return(ret) = data.inst_data(terminator).kind() else {
                continue;
            };

            // Anything between the call and return could observe the call's
            // side effects, so requiring adjacency makes the rewrite safe.
            let mut rev = insts.iter().rev();
            rev.next(); // skip the terminator
            let Some(&call_inst) = rev.next() else {
                continue;
            };
            let InstKind::Call(call) = data.inst_data(call_inst).kind() else {
                continue;
            };

            let callee = call.callee();
            let args = call.args().to_vec();
            let call_ty = data.inst_data(call_inst).ty().clone();
            let caller_data = data.program.func_data(curr_func);
            let callee_data = data.program.func_data(callee);
            if caller_data.ret_ty() != callee_data.ret_ty()
                || caller_data.params_ty() != callee_data.params_ty()
                || call_ty != *callee_data.ret_ty()
                || args.len() != callee_data.params_ty().len()
                || args
                    .iter()
                    .zip(callee_data.params_ty())
                    .any(|(&arg, param_ty)| data.inst_data(arg).ty() != param_ty)
            {
                continue;
            }

            match ret.value() {
                // Value tail call: `ret call_result`.
                Some(value) if value == call_inst => {}
                // Void tail call: the unit-typed call result must have no users.
                None if data.inst_data(call_inst).used_by().is_empty() => {}
                _ => continue,
            }

            rewrites.push((bb, call_inst, args));
        }

        let changed = !rewrites.is_empty();
        for (bb, call_inst, args) in rewrites {
            let callee = match data.inst_data(call_inst).kind() {
                InstKind::Call(call) => call.callee(),
                _ => unreachable!("collected rewrite must be a Call"),
            };
            // Detach the call first. Its arguments are released; the ret still
            // nominally references the call result, which `detach_inst_usage`
            // tolerates because the call's instruction data is already gone.
            data.remove_layout_inst(bb, call_inst);
            let terminator = utils::get_terminator_inst(data, bb);
            data.remove_layout_inst(bb, terminator);
            let tail_call = data.new_local_inst().tail_call(callee, args);
            data.layout_mut().insert_inst(bb, tail_call);
        }

        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        BinaryOp, Program,
        arena::Arena,
        builder_trait::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
    };
    use crate::opt::pass::Pass;

    fn run(program: &mut Program) {
        TailCallElim.run(program);
    }

    /// Build a function whose entry block carries one block parameter per
    /// signature type, mirroring the construction convention used by the
    /// frontend. Returns the entry block and its block-parameter values.
    fn add_entry(data: &mut FunctionData, params_ty: Vec<Type>) -> (BasicBlock, Vec<Inst>) {
        let entry = data
            .new_basic_block()
            .basic_block("entry".into(), params_ty);
        data.layout_mut().push_bb_back(entry);
        let params = data.bb_data(entry).params().to_vec();
        (entry, params)
    }

    fn term_is_tail_call_to(data: &FunctionData, bb: BasicBlock, callee: Function) -> bool {
        let term = utils::get_terminator_inst(data, bb);
        matches!(
            data.inst_data(term).kind(),
            InstKind::TailCall(tc) if tc.callee() == callee
        )
    }

    #[test]
    fn converts_value_tail_call_into_loop() {
        // int sum(int n, int acc) {
        //     if (n == 0) return acc;
        //     return sum(n - 1, acc + n);   // self tail call
        // }
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "sum".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let (entry, params) = add_entry(data, vec![Type::get_i32(), Type::get_i32()]);
        let then_bb = data.new_basic_block().basic_block("then".into(), vec![]);
        let recurse_bb = data.new_basic_block().basic_block("recurse".into(), vec![]);
        for bb in [then_bb, recurse_bb] {
            data.layout_mut().push_bb_back(bb);
        }
        let n = params[0];
        let acc = params[1];
        let zero = data.new_local_inst().integer(0);
        let cond = data.new_local_inst().binary(BinaryOp::Eq, n, zero);
        let entry_term = data
            .new_local_inst()
            .branch(cond, then_bb, vec![], recurse_bb, vec![]);
        data.layout_mut().insert_inst(entry, entry_term);
        let then_ret = data.new_local_inst().ret(Some(acc));
        data.layout_mut().insert_inst(then_bb, then_ret);
        let one = data.new_local_inst().integer(1);
        let n_minus_one = data.new_local_inst().binary(BinaryOp::Sub, n, one);
        let acc_plus_n = data.new_local_inst().binary(BinaryOp::Add, acc, n);
        let call = data.new_local_inst().call_with_type(
            function,
            vec![n_minus_one, acc_plus_n],
            Type::get_i32(),
        );
        data.layout_mut().insert_inst(recurse_bb, call);
        let recurse_ret = data.new_local_inst().ret(Some(call));
        data.layout_mut().insert_inst(recurse_bb, recurse_ret);

        run(&mut program);
        let data = program.func_data(function);

        assert!(term_is_tail_call_to(data, recurse_bb, function));
        // The `then` arm keeps its real return.
        let then_term = utils::get_terminator_inst(data, then_bb);
        assert!(matches!(
            data.inst_data(then_term).kind(),
            InstKind::Return(..)
        ));
        // No (non-tail) calls to self remain.
        let has_self_call = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .any(|inst| matches!(data.inst_data(inst).kind(), InstKind::Call(c) if c.callee() == function));
        assert!(!has_self_call);
    }

    #[test]
    fn ignores_non_tail_self_call() {
        // int f(int n) { int t = f(n); return t + 1; }  -- not a tail call.
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "f".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let (entry, params) = add_entry(data, vec![Type::get_i32()]);
        let n = params[0];
        let call = data
            .new_local_inst()
            .call_with_type(function, vec![n], Type::get_i32());
        data.layout_mut().insert_inst(entry, call);
        let one = data.new_local_inst().integer(1);
        let plus = data.new_local_inst().binary(BinaryOp::Add, call, one);
        data.layout_mut().insert_inst(entry, plus);
        let ret = data.new_local_inst().ret(Some(plus));
        data.layout_mut().insert_inst(entry, ret);

        run(&mut program);
        let data = program.func_data(function);
        // Still a real return, not a jump to entry.
        let term = utils::get_terminator_inst(data, entry);
        assert!(matches!(data.inst_data(term).kind(), InstKind::Return(..)));
    }

    #[test]
    fn converts_abi_compatible_tail_call_to_other_function() {
        let mut program = Program::new();
        let other = program.new_function(Type::get_i32(), "g".into(), vec![Type::get_i32()]);
        let function = program.new_function(Type::get_i32(), "f".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let (entry, params) = add_entry(data, vec![Type::get_i32()]);
        let n = params[0];
        let call = data
            .new_local_inst()
            .call_with_type(other, vec![n], Type::get_i32());
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_inst().ret(Some(call));
        data.layout_mut().insert_inst(entry, ret);

        run(&mut program);
        let data = program.func_data(function);
        assert!(term_is_tail_call_to(data, entry, other));
    }

    #[test]
    fn converts_abi_compatible_void_tail_call() {
        let mut program = Program::new();
        let other = program.new_function(Type::get_unit(), "g".into(), vec![Type::get_i32()]);
        let function = program.new_function(Type::get_unit(), "f".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let (entry, params) = add_entry(data, vec![Type::get_i32()]);
        let call = data
            .new_local_inst()
            .call_with_type(other, vec![params[0]], Type::get_unit());
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        run(&mut program);
        let data = program.func_data(function);
        assert!(term_is_tail_call_to(data, entry, other));
    }

    #[test]
    fn ignores_tail_call_with_incompatible_parameter_signature() {
        let mut program = Program::new();
        let other = program.new_function(Type::get_i32(), "g".into(), vec![]);
        let function = program.new_function(Type::get_i32(), "f".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let (entry, _) = add_entry(data, vec![Type::get_i32()]);
        let call = data
            .new_local_inst()
            .call_with_type(other, vec![], Type::get_i32());
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_inst().ret(Some(call));
        data.layout_mut().insert_inst(entry, ret);

        run(&mut program);
        let data = program.func_data(function);
        let term = utils::get_terminator_inst(data, entry);
        assert!(matches!(data.inst_data(term).kind(), InstKind::Return(..)));
    }

    #[test]
    fn ignores_tail_call_with_incompatible_return_signature() {
        let mut program = Program::new();
        let other = program.new_function(Type::get_unit(), "g".into(), vec![Type::get_i32()]);
        let function = program.new_function(Type::get_i32(), "f".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let (entry, params) = add_entry(data, vec![Type::get_i32()]);
        // Keep the caller IR type-correct while simulating a mismatched call
        // declaration that must not be turned into a tail call.
        let call = data
            .new_local_inst()
            .call_with_type(other, vec![params[0]], Type::get_i32());
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_inst().ret(Some(call));
        data.layout_mut().insert_inst(entry, ret);

        run(&mut program);
        let data = program.func_data(function);
        let term = utils::get_terminator_inst(data, entry);
        assert!(matches!(data.inst_data(term).kind(), InstKind::Return(..)));
    }

    #[test]
    fn leaves_paramless_non_recursive_function_untouched() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let zero = data.new_local_inst().integer(0);
        let ret = data.new_local_inst().ret(Some(zero));
        data.layout_mut().insert_inst(entry, ret);

        run(&mut program);
        let data = program.func_data(function);
        let term = utils::get_terminator_inst(data, entry);
        assert!(matches!(data.inst_data(term).kind(), InstKind::Return(..)));
    }
}
