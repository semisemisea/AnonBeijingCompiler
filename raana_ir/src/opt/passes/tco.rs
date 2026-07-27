use crate::opt::prelude::*;

/// Rewrites self-recursive tail calls into a [`TailCall`], which the backend
/// lowers to a frame-reusing jump (`b callee`) so the recursion runs in
/// constant stack space.
///
/// Two tail shapes are recognized, both only when the callee is the current
/// function and the call is the instruction immediately before the `ret`
/// (adjacency guarantees nothing observes the call's side effects between it
/// and the return):
///
/// 1. **Value tail call**: `%t = call self(args); ret %t`.
/// 2. **Void tail call**: `call self(args); ret`
///    (call result is unit, no other users).
///
/// Both become `tail_call self(args)`.
pub struct TailCallElim;

impl Pass for TailCallElim {
    fn run_on(&self, data: &mut ArenaContext<'_>) -> bool {
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

            // The self-call must be the instruction immediately before the
            // `ret`. Anything between them (loads, stores, ...) could observe
            // the call's side effects, so the call would not be in tail
            // position. Requiring adjacency makes the transformation safe.
            let mut rev = insts.iter().rev();
            rev.next(); // skip the terminator
            let Some(&call_inst) = rev.next() else {
                continue;
            };
            let InstKind::Call(call) = data.inst_data(call_inst).kind() else {
                continue;
            };
            if call.callee() != curr_func {
                continue;
            }

            match ret.value() {
                // Value tail call: `ret call_result`.
                Some(value) if value == call_inst => {}
                // Void tail call: the unit-typed call result must have no users.
                None if data.inst_data(call_inst).used_by().is_empty() => {}
                _ => continue,
            }

            rewrites.push((bb, call_inst, call.args().to_vec()));
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
    fn ignores_tail_call_to_other_function() {
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
