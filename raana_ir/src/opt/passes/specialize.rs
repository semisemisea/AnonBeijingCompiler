use rustc_hash::{FxHashMap, FxHashSet};

use crate::opt::{prelude::*, utils::body_clone::BodyClonePlan};

#[derive(Default)]
pub struct Specialize {
    /// If you have (candidate, [(1, const_1), (2, const_2), ...]) more the once, then reuse the function.
    specialized: FxHashMap<(Function, Vec<(usize, Const)>), Function>,
    created: FxHashSet<Function>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Const {
    Int(i32),
    Float(u32),
}

impl Specialize {
    fn run_on_one(&mut self, program: &mut Program) -> bool {
        let call_graph = call_graph::CallGraph::new(program);
        for candidate in call_graph.callees() {
            // TODO: Very rough way to prevent infinite recursion.
            // Maybe introduce attribute system to solve it.
            if self.created.contains(&candidate) || program.func_data(candidate).layout().is_decl()
            {
                continue;
            }
            let callsites = call_graph.incoming_callsites_of(candidate);
            let mut specialize_count = 0;
            for Node { func, inst } in callsites {
                let data = program.func_data(func);
                let inst_data = data.inst_data(inst);
                let InstKind::Call(call) = inst_data.kind() else {
                    // TODO: Skip tailcall for now.
                    continue;
                };
                let mut const_args = vec![];
                for (i, &arg) in call.args().iter().enumerate() {
                    let arg_data = data.inst_data(arg);
                    match arg_data.kind() {
                        InstKind::Integer(int) => {
                            const_args.push((i, Const::Int(int.value())));
                        }
                        InstKind::Float(float) => {
                            const_args.push((i, Const::Float(float.value() as u32)));
                        }
                        _ => {}
                    }
                }
                if const_args.is_empty() {
                    continue;
                }
                if let Some(&created) = self.specialized.get(&(candidate, const_args.clone())) {
                    let args = call.args().to_vec();
                    let mut ctx = ArenaContextMut {
                        program,
                        curr_func: Some(func),
                    };
                    ctx.replace_inst_with(inst).call(created, args);
                    continue;
                };
                let Ok(clone_plan) = BodyClonePlan::capture(program, candidate) else {
                    continue;
                };
                let args = call.args().to_vec();
                let candidate_data = program.func_data(candidate);
                let ret_ty = candidate_data.ret_ty().clone();
                let params_ty = candidate_data.params_ty().to_vec();
                let name = candidate_data.name();
                let new_func = program.new_function(
                    ret_ty,
                    format!("{name}_specialized_{specialize_count}"),
                    params_ty,
                );
                self.specialized.insert((candidate, const_args), new_func);
                let entry_bb = program.func_data_mut(new_func).add_entry_block();

                let clone_body = clone_plan
                    .clone_into(program, new_func, entry_bb)
                    .expect("await to have a perfect reason for not failing.");

                let entry_param = program
                    .func_data(new_func)
                    .bb_data(entry_bb)
                    .params()
                    .to_vec();

                let jump_inst = program
                    .func_data_mut(new_func)
                    .new_local_inst()
                    .jump(clone_body.entry, entry_param);

                program
                    .func_data_mut(new_func)
                    .layout_mut()
                    .insert_inst(entry_bb, jump_inst);

                let mut ctx = ArenaContextMut {
                    program,
                    curr_func: Some(func),
                };
                ctx.replace_inst_with(inst).call(new_func, args);
                specialize_count += 1;
                self.created.insert(new_func);
            }
            if specialize_count > 0 {
                return true;
            }
        }
        false
    }
}

impl Pass for Specialize {
    fn run(&mut self, program: &mut Program) -> bool {
        let mut changed_once = false;
        loop {
            let mut changed = false;
            changed |= self.run_on_one(program);
            if !changed {
                break;
            }
            changed_once = true;
        }
        changed_once
    }
}

// #[cfg(test)]
// mod tests {
//     use super::Specialize;
//     use crate::{
//         ir::{BinaryOp, Function, Inst, InstKind, Program, Type, arena::Arena, builder_trait::*},
//         opt::pass::Pass,
//     };
//
//     /// `f(x) = x + 1`: a one-block callee with a single i32 parameter.
//     fn build_f() -> Program {
//         let mut program = Program::new();
//         let callee = program.new_function(Type::get_i32(), "f".into(), vec![Type::get_i32()]);
//         {
//             let data = program.func_data_mut(callee);
//             let entry = data.add_entry_block();
//             let param = data.params()[0];
//             let one = data.new_local_inst().integer(1);
//             let add = data.new_local_inst().binary(BinaryOp::Add, param, one);
//             let ret = data.new_local_inst().ret(Some(add));
//             data.layout_mut().insert_inst(entry, add);
//             data.layout_mut().insert_inst(entry, ret);
//         }
//         program
//     }
//
//     fn find_specialized(program: &Program, prefix: &str) -> Function {
//         program
//             .function_layout()
//             .iter()
//             .copied()
//             .find(|&f| program.func_data(f).name().starts_with(prefix))
//             .expect("a specialized clone must exist")
//     }
//
//     fn calls_in(program: &Program, func: Function) -> Vec<(Function, Vec<Inst>)> {
//         let data = program.func_data(func);
//         data.layout()
//             .basicblocks()
//             .iter()
//             .flat_map(|block| block.insts().iter())
//             .filter_map(|&inst| match data.inst_data(inst).kind() {
//                 InstKind::Call(call) => Some((call.callee(), call.args().to_vec())),
//                 _ => None,
//             })
//             .collect()
//     }
//
//     #[test]
//     fn redirects_constant_callsite_to_a_cloned_function() {
//         let mut program = build_f();
//         let f = program.function_layout()[0];
//         let main = program.new_function(Type::get_i32(), "main".into(), vec![Type::get_i32()]);
//         {
//             let data = program.func_data_mut(main);
//             let entry = data.add_entry_block();
//             let runtime = data.params()[0];
//             let forty_two = data.new_local_inst().integer(42);
//             let call_const =
//                 data.new_local_inst()
//                     .call_with_type(f, vec![forty_two], Type::get_i32());
//             let call_runtime =
//                 data.new_local_inst()
//                     .call_with_type(f, vec![runtime], Type::get_i32());
//             let add = data
//                 .new_local_inst()
//                 .binary(BinaryOp::Add, call_const, call_runtime);
//             let ret = data.new_local_inst().ret(Some(add));
//             for inst in [call_const, call_runtime, add, ret] {
//                 data.layout_mut().insert_inst(entry, inst);
//             }
//         }
//
//         let changed = Specialize.run(&mut program);
//
//         let new_func = find_specialized(&program, "f_specialized");
//         assert_ne!(new_func, f);
//
//         // The clone keeps the full signature: an empty entry block plus the
//         // cloned body, wired by an entry jump forwarding the entry parameters.
//         let data = program.func_data(new_func);
//         assert_eq!(data.layout().basicblocks().len(), 2);
//         let entry = data.layout().basicblocks().get_first().unwrap().bb();
//         let clone_entry = data.layout().basicblocks().get_last().unwrap().bb();
//         let terminator = data.layout().basicblock(entry).terminator();
//         let InstKind::Jump(jump) = data.inst_data(terminator).kind() else {
//             panic!("specialized entry must end with a jump to the cloned body");
//         };
//         assert_eq!(jump.target(), clone_entry);
//         assert_eq!(jump.args().to_vec(), data.bb_data(entry).params().to_vec());
//
//         // Only the constant callsite is redirected; the runtime one stays.
//         let calls = calls_in(&program, main);
//         assert_eq!(calls.len(), 2);
//         for (callee, args) in &calls {
//             if matches!(
//                 program.func_data(main).inst_data(args[0]).kind(),
//                 InstKind::Integer(..)
//             ) {
//                 assert_eq!(*callee, new_func);
//             } else {
//                 assert_eq!(*callee, f);
//             }
//         }
//
//         // The pass must report the change so the pipeline fixed point reruns
//         // the folding passes over the fresh clone.
//         assert!(changed);
//     }
//
//     #[test]
//     fn does_not_specialize_without_constant_arguments() {
//         let mut program = build_f();
//         let f = program.function_layout()[0];
//         let main = program.new_function(Type::get_i32(), "main".into(), vec![Type::get_i32()]);
//         {
//             let data = program.func_data_mut(main);
//             let entry = data.add_entry_block();
//             let runtime = data.params()[0];
//             let call = data
//                 .new_local_inst()
//                 .call_with_type(f, vec![runtime], Type::get_i32());
//             let ret = data.new_local_inst().ret(Some(call));
//             data.layout_mut().insert_inst(entry, call);
//             data.layout_mut().insert_inst(entry, ret);
//         }
//
//         assert!(!Specialize.run(&mut program));
//         assert_eq!(program.function_layout().len(), 2);
//         assert!(
//             program
//                 .function_layout()
//                 .iter()
//                 .all(|&f| !program.func_data(f).name().contains("specialized"))
//         );
//     }
//
//     #[test]
//     fn creates_one_clone_per_distinct_constant() {
//         let mut program = build_f();
//         let f = program.function_layout()[0];
//         let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
//         {
//             let data = program.func_data_mut(main);
//             let entry = data.add_entry_block();
//             let one = data.new_local_inst().integer(1);
//             let two = data.new_local_inst().integer(2);
//             let call_one = data
//                 .new_local_inst()
//                 .call_with_type(f, vec![one], Type::get_i32());
//             let call_two = data
//                 .new_local_inst()
//                 .call_with_type(f, vec![two], Type::get_i32());
//             let add = data
//                 .new_local_inst()
//                 .binary(BinaryOp::Add, call_one, call_two);
//             let ret = data.new_local_inst().ret(Some(add));
//             for inst in [call_one, call_two, add, ret] {
//                 data.layout_mut().insert_inst(entry, inst);
//             }
//         }
//
//         let changed = Specialize.run(&mut program);
//
//         assert_eq!(program.function_layout().len(), 4); // f, main, and two clones
//         let calls = calls_in(&program, main);
//         assert_eq!(calls.len(), 2);
//         for (callee, _) in &calls {
//             assert_ne!(*callee, f);
//             assert!(
//                 program
//                     .func_data(*callee)
//                     .name()
//                     .starts_with("f_specialized")
//             );
//         }
//         assert!(changed);
//     }
//
//     #[test]
//     fn reuses_one_clone_for_repeated_identical_constants() {
//         let mut program = build_f();
//         let f = program.function_layout()[0];
//         let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
//         {
//             let data = program.func_data_mut(main);
//             let entry = data.add_entry_block();
//             let one = data.new_local_inst().integer(1);
//             let two = data.new_local_inst().integer(1);
//             let call_one = data
//                 .new_local_inst()
//                 .call_with_type(f, vec![one], Type::get_i32());
//             let call_two = data
//                 .new_local_inst()
//                 .call_with_type(f, vec![two], Type::get_i32());
//             let add = data
//                 .new_local_inst()
//                 .binary(BinaryOp::Add, call_one, call_two);
//             let ret = data.new_local_inst().ret(Some(add));
//             for inst in [call_one, call_two, add, ret] {
//                 data.layout_mut().insert_inst(entry, inst);
//             }
//         }
//
//         let changed = Specialize.run(&mut program);
//
//         // Identical specialization keys must share one clone; the two
//         // callsites both redirect to it.
//         assert_eq!(program.function_layout().len(), 3); // f, main, and one clone
//         let new_func = find_specialized(&program, "f_specialized");
//         for (callee, _) in calls_in(&program, main) {
//             assert_eq!(callee, new_func);
//         }
//         assert!(changed);
//     }
//
//     #[test]
//     fn skips_declaration_callees_without_creating_a_function() {
//         let mut program = Program::new();
//         let decl = program.new_function(Type::get_i32(), "getval".into(), vec![Type::get_i32()]);
//         // No `add_entry_block` call: this is a declaration without a body.
//         let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
//         {
//             let data = program.func_data_mut(main);
//             let entry = data.add_entry_block();
//             let seven = data.new_local_inst().integer(7);
//             let call = data
//                 .new_local_inst()
//                 .call_with_type(decl, vec![seven], Type::get_i32());
//             let ret = data.new_local_inst().ret(Some(call));
//             data.layout_mut().insert_inst(entry, call);
//             data.layout_mut().insert_inst(entry, ret);
//         }
//
//         let changed = Specialize.run(&mut program);
//
//         // A declaration has no body to clone; nothing may be created, so the
//         // leaked half-built function from a failed capture must not exist.
//         assert_eq!(program.function_layout().len(), 2);
//         assert!(!changed);
//     }
//
//     #[test]
//     fn does_not_panic_on_tail_call_callsites() {
//         let mut program = build_f();
//         let f = program.function_layout()[0];
//         let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
//         {
//             let data = program.func_data_mut(main);
//             let entry = data.add_entry_block();
//             let forty_two = data.new_local_inst().integer(42);
//             let tail = data.new_local_inst().tail_call(f, vec![forty_two]);
//             data.layout_mut().insert_inst(entry, tail);
//         }
//
//         let changed = Specialize.run(&mut program);
//         assert!(!changed);
//     }
// }
