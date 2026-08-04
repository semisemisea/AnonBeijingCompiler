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
            context.replace_inst_with(tail).jump(cloned.entry, tail_args);
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
            let tail = data
                .new_local_inst()
                .tail_call(fun, vec![half, dep_plus]);
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
        assert!(has_back_edge, "expected a loop back-edge jump to the header");
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
