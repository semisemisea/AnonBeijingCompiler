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
const CALL_SIZE_LIMIT: usize = 40;
/// Total budget for one callee across all of its callsites.
const TOTAL_SIZE_LIMIT: usize = 100;
/// Per-callsite clone budget when at least one callsite sits inside a
/// natural loop of its caller: the removed call overhead is paid once per
/// loop iteration, so a larger one-time clone is dynamically worthwhile
/// there (e.g. `read_bits_specialized_*` in huffman-01's decode loop).
/// Calibrated on real data: a loop-resident callee may itself grow through
/// chained in-loop inlining of its own helpers (`read_bits` 141 + `rotlN` 34
/// = 175), so the budget must cover the post-chain size, not the bare body.
const CALL_SIZE_LIMIT_LOOP: usize = 200;
/// Total budget across the callsites of an in-loop callee.
const TOTAL_SIZE_LIMIT_LOOP: usize = 300;

impl Pass for Inline {
    fn run(&mut self, program: &mut Program) -> bool {
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
    data.layout()
        .basicblocks()
        .iter()
        .map(|layout| layout.insts().len())
        .sum()
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
            if program.func_data(callee).layout().is_decl() {
                continue;
            }
            // A callee in a recursion cycle is never inlined: after the
            // first clone its in-cycle callsites live inside the caller,
            // where the reachability guard no longer excludes them, so the
            // pass would keep inlining the cycle forever. The conservative
            // cranelift rule (`does_not_inline_across_a_recursive_call_cycle`)
            // is to leave cyclic callees as calls.
            if call_graph.reaches(callee, callee) {
                continue;
            }
            let callee_data = program.func_data(callee);
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
            let size = estimate_size(callee_data);
            let total = size.saturating_mul(callsites.len());
            if size > CALL_SIZE_LIMIT || total > TOTAL_SIZE_LIMIT {
                let in_loop = Self::any_callsite_in_loop(program, &callsites);
                if !in_loop || size > CALL_SIZE_LIMIT_LOOP || total > TOTAL_SIZE_LIMIT_LOOP {
                    continue;
                }
            }
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

    /// True when any of the callsites' enclosing blocks sits inside a natural
    /// loop of its caller. Builds one `LoopAnalysis` per distinct caller; the
    /// analysis is a snapshot of the current CFG, which is valid because
    /// `find_candidate` runs before any mutation in `once`.
    fn any_callsite_in_loop(program: &Program, callsites: &[Node]) -> bool {
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
