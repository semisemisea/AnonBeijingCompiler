//! # Specialize：函数特化（按常量实参克隆）
//!
//! 对每个函数扫描全部调用点，把**带常量实参**的普通 `call` 重定向到按
//! (函数, 常量实参组合) 克隆出的专用版本（`{name}_specialized_{n}`）；同一
//! 常量组合的多个调用点共享同一份克隆（`specialized` 缓存），避免重复克隆。
//! 目标：克隆体随后被 `inline` 内联进各调用点后，其参数恒为同一组常量，
//! 后续常量传播 / 折叠（const_prop / IPSCCP）能比"一个函数体喂多种实参"
//! 更彻底地折叠。
//!
//! 这是经典的过程间优化（interprocedural specialization）。动机示例：
//!
//! ```text
//! f(a, n) { return a * n; }          // 通用版本保留
//! 调用点1: call f(3, n)              // → 改指 f_specialized_0
//! 调用点2: call f(3, m)              // → 复用 f_specialized_0
//! 调用点3: call f(7, k)              // → 新建 f_specialized_1
//! ```
//!
//! 内联后克隆体内的 `a` 恒为常量，`a * n` 可被强度削减 / 常量折叠处理。
//! 注意：克隆时**不**把参数替换成常量（`call` 仍传原实参），收益完全来自
//! "每个常量组合一个专用副本"这一形态，由后续 pass 兑现。
//!
//! ## 触发条件
//!
//! - 被调函数是**已定义**函数（跳过声明 `is_decl`）；
//! - 被调函数**不自递归**（`CallGraph::reaches(f, f)` 则跳过——自递归克隆
//!   的递归调用仍指向原函数，克隆是字节级重复，只多一层间接调用；自尾递归
//!   循环形态由 `tail_recursive_inline` pass 处理）；
//! - 调用点至少一个实参是编译期常量（`InstKind::Integer` / `Float`）；
//! - `BodyClonePlan::capture` 克隆成功；
//! - 只处理普通 `Call`（tailcall 暂跳过，代码有 TODO 注释）；
//! - 防无限递归：`created` 集合记录已克隆函数，每个候选函数只特化一轮
//!   （代码注释自认 "Very rough way"）。
//!
//! ## 正确性
//!
//! - 克隆体由 `BodyClonePlan` 完整复制原函数体，行为与原函数一致；调用点
//!   只是换了个"实参组合固定"的入口，语义不变；
//! - 不同常量组合 → 不同克隆（`Const::Int` / `Float` 参与缓存 key），
//!   互不干扰。
//!
//! ## 管线位置
//!
//! - 注册：`opt/pass.rs` 的 `from_config`，**initial 段**（一次性），`ssa`
//!   之后、`inline` 之前——特化必须先于内联，内联才能把专用克隆展开进
//!   调用点；
//! - `run` 循环执行 `run_on_one` 直到无变化（一次特化可能暴露新的候选）；
//! - 无目标门控、无 config 开关。
//!
//! ## 验证
//!
//! - 本文件 `mod tests`（138 行起）当前被整体注释（`// #[cfg(test)]`），
//!   如启用需先恢复；
//! - 端到端：`make test` 差分比对。

use rustc_hash::{FxHashMap, FxHashSet};

use crate::opt::{prelude::*, utils::body_clone::BodyClonePlan};

#[derive(Default)]
pub struct Specialize {
    /// If you have (candidate, [(1, const_1), (2, const_2), ...]) more the once, then reuse the function.
    ///
    /// 克隆缓存：键 = (原函数, 常量实参组合)，`(usize, Const)` 记录“第几个
    /// 参数是常量、值是多少”。同一常量组合的多个调用点复用同一克隆，避免重复克隆。
    specialized: FxHashMap<(Function, Vec<(usize, Const)>), Function>,
    /// 已创建过的克隆函数集合：新一轮扫描命中即跳过，防止“克隆体又被克隆”
    /// 造成的无限递归（见下方 TODO 注释）。
    created: FxHashSet<Function>,
}

/// 参与克隆缓存键的“常量实参”抽象。浮点常量按位模式（`u32`）保存：
/// `f32` 不实现 `Eq`/`Hash`，无法直接进哈希表；位模式不同即视为不同常量
/// 组合（如 ±0.0 编码不同会各得一个克隆）——多克隆无害，误合并才可能出错。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Const {
    Int(i32),
    Float(u32),
}

impl Specialize {
    /// 一轮特化：重建调用图 → 枚举候选函数 → 对每个候选收集带常量实参的
    /// 调用点，克隆函数体并改写调用目标。返回本轮是否发生了特化。
    fn run_on_one(&mut self, program: &mut Program) -> bool {
        // 每轮重建调用图：特化会新增函数、改写调用点，旧图快照已经失效。
        let call_graph = call_graph::CallGraph::new(program);
        // 候选 = 调用图中所有“被调用过”的函数；没人调用的函数克隆了也无收益。
        for candidate in call_graph.callees() {
            // TODO: Very rough way to prevent infinite recursion.
            // Maybe introduce attribute system to solve it.
            if self.created.contains(&candidate) || program.func_data(candidate).layout().is_decl()
            {
                continue;
            }
            // 跳过条件（命中任一即放弃该候选）：
            // - `created` 已含它：说明是上一轮产生的克隆体，不再对克隆体特化，
            //   否则“克隆体的常量调用点 → 下一代克隆”会无限递归（上方 TODO）；
            // - `is_decl()`：声明只有签名没有函数体，无内容可克隆。
            // A self-recursive candidate is never usefully specialized: the
            // clone keeps its recursion pointing at the original, so it is a
            // byte-for-byte duplicate that only adds a call indirection. The
            // self-tail-recursive loop case is handled by the
            // `tail_recursive_inline` pass instead.
            // 中文注：克隆只复制函数体、不重定向体内的调用目标，所以克隆里
            // 的递归调用仍指向原函数——克隆体与原件字节级相同，纯属浪费；
            // 自尾递归的循环形态交给 `tail_recursive_inline` pass。
            if call_graph.reaches(candidate, candidate) {
                continue;
            }
            // 枚举该候选的全部调用点（每个元素是一个 (调用方函数, 调用指令)
            // 对）。特化只改写这些调用点的目标，候选函数体本身不动。
            let callsites = call_graph.incoming_callsites_of(candidate);
            let mut specialize_count = 0;
            for Node { func, inst } in callsites {
                let data = program.func_data(func);
                let inst_data = data.inst_data(inst);
                let InstKind::Call(call) = inst_data.kind() else {
                    // TODO: Skip tailcall for now.
                    continue;
                };
                // 只有普通 `Call` 参与特化；`TailCall` 等其它指令形态落入
                // `else` 分支跳过（代码留有 TODO：暂不处理尾调用）。
                // 扫描实参，收集其中的编译期常量，记成 (实参下标, 常量值) 对：
                // 下标区分“哪个参数被固定”，常量值决定该调用点归属哪个克隆。
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
                // 没有任何常量实参的调用点：特化对它没有收益，保持原调用。
                if const_args.is_empty() {
                    continue;
                }
                // 缓存命中：该 (候选, 常量组合) 此前已克隆过，直接把本调用点
                // 改指已有克隆（`replace_inst_with` 原地替换该指令，实参不变），
                // 不再重复克隆。
                if let Some(&created) = self.specialized.get(&(candidate, const_args.clone())) {
                    let args = call.args().to_vec();
                    let mut ctx = ArenaContextMut {
                        program,
                        curr_func: Some(func),
                    };
                    ctx.replace_inst_with(inst).call(created, args);
                    continue;
                };
                // 首次遇到该常量组合：用 `BodyClonePlan` 捕获候选的完整函数体
                // （克隆工具见 `opt/utils/body_clone.rs`）；捕获失败则放弃该候选。
                let Ok(clone_plan) = BodyClonePlan::capture(program, candidate) else {
                    continue;
                };
                // 新建克隆函数：签名（返回类型 + 形参类型）与原函数完全一致，
                // 名字为 `{原函数名}_specialized_{序号}`。注意形参仍是普通参数、
                // 并不替换成常量——收益来自“每个常量组合一个专用副本”的形态，
                // 由后续 `inline` 内联 + 常量传播/折叠兑现（见模块文档）。
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
                // 先登记缓存再组装克隆体：同一轮内后续相同的常量组合直接复用，
                // 不会重复克隆。
                self.specialized.insert((candidate, const_args), new_func);
                // 组装克隆体骨架（新函数含两个基本块）：
                // 1. `entry_bb` 是新函数的入口块，其 block 参数即函数形参；
                // 2. `clone_into` 把原函数体整体搬进新函数（插在入口块之后），
                //    返回克隆体的入口块；
                // 3. 入口块末尾补一条 `jump` 到克隆体入口，把 entry 的 block
                //    参数原样转发——block 参数即本项目的 Phi，随边传参完成
                //    形参接线，克隆体内部无需任何修改。
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

                // 调用点改写：把原 `call candidate` 原地替换为 `call new_func`。
                // 实参仍传原值——克隆体形参不是常量，常量性由“该克隆只服务
                // 这一组常量调用点”保证，内联代入后即可折叠。
                let mut ctx = ArenaContextMut {
                    program,
                    curr_func: Some(func),
                };
                ctx.replace_inst_with(inst).call(new_func, args);
                // 累计本候选的克隆个数（决定命名序号），并把克隆体登记进
                // `created`，防止下一轮再对它特化。
                specialize_count += 1;
                self.created.insert(new_func);
            }
            // 该候选至少特化出一个克隆：提前结束本轮（外层 fixpoint 会重建
            // 调用图重扫，覆盖其余尚未处理的候选）。
            if specialize_count > 0 {
                return true;
            }
        }
        false
    }
}

impl Pass for Specialize {
    // fixpoint 外层：反复执行 `run_on_one` 直到一整轮没有任何特化发生。
    // `run_on_one` 一旦特化成功就提前返回，未处理的候选留到下一轮；每轮
    // 重建调用图，让改写后的调用点与新增克隆都被重新审视。克隆体被
    // `created` 集合拦截，循环必然终止。`changed_once` 汇总是否发生过改动。
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
