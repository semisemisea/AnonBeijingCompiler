use crate::{
    ir::arena::Arena,
    opt::{prelude::*, utils::body_clone::BodyClonePlan},
};

/// Inline functions with one statically reachable ordinary callsite.
pub struct Inline;

impl Pass for Inline {
    fn run(&self, program: &mut Program) -> bool {
        let mut changed = false;
        while Self::once(program) {
            changed = true;
        }
        changed
    }
}

struct Candidate {
    caller: Function,
    call_inst: Inst,
    call_block: BasicBlock,
    args: Vec<Inst>,
    ret_ty: Type,
    plan: BodyClonePlan,
}

impl Inline {
    fn once(program: &mut Program) -> bool {
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

        let cloned = plan
            .clone_into(program, caller, call_block)
            .expect("preflighted function body must clone successfully");
        debug_assert!(cloned.tail_calls.is_empty());
        debug_assert!(!cloned.blocks.is_empty());

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
        true
    }

    fn find_candidate(program: &Program) -> Option<Candidate> {
        let call_graph = call_graph::CallGraph::new(program);
        for &callee in program.function_layout() {
            if call_graph.in_degree_of(callee) != 1 || program.func_data(callee).layout().is_decl()
            {
                continue;
            }
            let Some(callsite) = call_graph.be_called_at(callee).next() else {
                continue;
            };
            if callsite.func == callee || call_graph.reaches(callee, callsite.func) {
                continue;
            }

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

            let callee_data = program.func_data(callee);
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
            if caller_data.inst_data(callsite.inst).ty().is_unit()
                && !caller_data.inst_data(callsite.inst).used_by().is_empty()
            {
                continue;
            }

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
}

#[cfg(test)]
mod tests {
    use super::Inline;
    use crate::{
        ir::{BinaryOp, InstKind, Program, Type, arena::Arena, builder_trait::*},
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
    fn does_not_inline_a_function_with_multiple_callsites() {
        let mut program = Program::new();
        let callee = program.new_function(Type::get_i32(), "value".into(), vec![]);
        {
            let data = program.func_data_mut(callee);
            let entry = data.add_entry_block();
            let one = data.new_local_inst().integer(1);
            let ret = data.new_local_inst().ret(Some(one));
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
}
