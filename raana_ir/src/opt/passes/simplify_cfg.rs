//! # SimplifyCFG：CFG 化简
//!
//! 三个互相独立的形状化简，循环执行直到没有变化：
//!
//! 1. **常数条件分支折叠**（`fold_const_condition_branch`）：条件已知的
//!    `br` 退化为 `jump`。
//! 2. **同目标分支折叠**（`fold_branch_same_target_and_args`）：两臂目标与
//!    实参完全相同的 `br` 退化为 `jump`。
//! 3. **平凡跳转块删除**（`remove_trivial_jump_block`）：删除"只有一条无参
//!    `jump`、自身无参数"的块，前驱直接指向目标。
//!
//! ## 变换形态（IR 示例）
//!
//! ```text
//! ① br 1, A, xs, B, ys        →   jump A, xs
//!    br 0, A, xs, B, ys        →   jump B, ys
//! ② br c, A, xs, A, xs        →   jump A, xs
//! ③ pre:  jump mid            →   pre:  jump exit
//!    mid:  jump exit           （mid 被删除）
//! ```
//!
//! ③ 的跳转链会**一次解析到底**（`mid → exit` 若 exit 也是平凡块则继续追）：
//! `pre → mid1 → mid2 → exit` 时 pre 直接指向 exit，中间块全部删除；链上出现
//! 环（平凡块自环或互环）则整条链放弃。
//!
//! ## 触发 / 放弃条件
//!
//! - ①只认 `InstKind::Integer` 条件（非零 → 真臂）；非整型条件不动。
//! - ②要求目标**和实参列表**都相同（`insts_equal`）。
//! - ③的候选块必须：非 entry、只有一条指令（终结符）、是无参 `jump`、块
//!   自身无参数、且不自跳（自跳块删除会让前驱指向不存在的块）。
//! - 幂等：三种化简都不产生新的同类形状，循环必然收敛。
//!
//! ## 正确性
//!
//! - ①②是直接等价替换；③中平凡块无参数、无副作用，删掉只缩短路径；
//! - 跳转链环检测保证不会删除形成环的块集（那会变成死循环或悬空前驱）。
//!
//! ## 管线位置
//!
//! - 注册：`opt/pass.rs` 的 `from_config`，fixpoint 段，`ipsccp` 之后、
//!   `loop_unroll` 之前；其它 pass 产生的死块/冗余分支由它在下一轮 fixpoint
//!   迭代里清掉；
//! - 无目标门控、无 config 开关。
//!
//! ## 验证
//!
//! - 本文件 `mod tests`（188 行起）覆盖三种化简及跳转链/自跳/entry 边界；
//! - 端到端：`make test` 差分比对。

use crate::opt::prelude::*;
use crate::opt::utils::logical_edge::{LogicalEdgeRewriter, outgoing_edges};

pub struct SimplifyCFG;

impl Pass for SimplifyCFG {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().entry_bb().is_none() {
            // 没有入口块（如空函数体）时不存在可化简的 CFG，直接返回未改动
            return false;
        }
        let mut changed_any = false;
        loop {
            let mut changed = false;

            // 三种化简每轮都执行：`|=` 不会短路，前面的化简即使改动了 IR，
            // 后面的化简仍会跑——同轮内后一个能看到前一个的结果，例如 ③ 删掉
            // 平凡块后，某 branch 两臂可能变成同一目标，从而新触发 ②。
            changed |= SimplifyCFG::fold_const_condition_branch(data);
            changed |= SimplifyCFG::fold_branch_same_target_and_args(data);
            changed |= SimplifyCFG::remove_trivial_jump_block(data);

            if !changed {
                // 一轮内三种化简都无改动：达到不动点。每轮要么删除若干块、
                // 要么把 branch 换成 jump（未取臂的实参变成死值），总量单调下降，
                // 因此循环必然终止
                break;
            }
            changed_any = true;
        }
        changed_any
    }
}

impl SimplifyCFG {
    /// br 1, t_target, t_args, f_target, f_args => jump t_target, t_args
    /// br 0, t_target, t_args, f_target, f_args => jump f_target, f_args
    pub fn fold_const_condition_branch(data: &mut ArenaContextMut<'_>) -> bool {
        struct Edit {
            branch: Inst,
            target: BasicBlock,
            args: Vec<Inst>,
        }
        let mut edits = Vec::new();
        for bb_layout in data.layout().basicblocks() {
            let terminator = bb_layout.terminator();
            let inst_data = data.inst_data(terminator);
            if let InstKind::Branch(branch) = inst_data.kind() {
                let cond = branch.cond();
                // 只认整型常量条件：非零即真（SysY/C 的 if 语义）；
                // 浮点/指针等其他类型条件无法判定真假，保持不动
                if let InstKind::Integer(int) = data.inst_data(cond).kind() {
                    let cond_eval = int.value() != 0;
                    let (target, args) = if cond_eval {
                        (branch.t_target(), branch.t_args().to_vec())
                    } else {
                        (branch.f_target(), branch.f_args().to_vec())
                    };
                    edits.push(Edit {
                        branch: terminator,
                        target,
                        args,
                    });
                }
            }
        }
        let changed = !edits.is_empty();
        // 两阶段：先遍历 layout 把改写全部收集进 `edits`，再统一应用——
        // 遍历期间不修改 IR，句柄与迭代器都保持有效
        for Edit {
            branch,
            target,
            args,
        } in edits
        {
            // 把 branch 原地替换成 jump，被选中那臂的实参向量原样搬到 jump 上：
            // 目标块的参数必须仍由跳转实参一一提供；未取臂的实参随之成为死值，
            // 留给后续 DCE 清理
            data.replace_inst_with(branch).jump(target, args);
        }

        changed
    }

    pub fn fold_branch_same_target_and_args(data: &mut ArenaContextMut<'_>) -> bool {
        struct Edit {
            branch: Inst,
            target: BasicBlock,
            args: Vec<Inst>,
        }
        let mut edits = Vec::new();
        for bb_layout in data.layout().basicblocks() {
            let terminator = bb_layout.terminator();
            let inst_data = data.inst_data(terminator);
            if let InstKind::Branch(branch) = inst_data.kind() {
                // 两臂目标相同还不够：实参也必须逐条等价（`insts_equal` 是带缓存
                // 的结构等价比较）。若两臂给目标块传入的值不同，合并会改变块参数
                // 的实际取值，语义不等价
                if branch.t_target() == branch.f_target()
                    && data.insts_equal(branch.t_args(), branch.f_args())
                {
                    edits.push(Edit {
                        branch: terminator,
                        target: branch.t_target(),
                        args: branch.t_args().to_vec(),
                    });
                };
            }
        }
        let changed = !edits.is_empty();
        // 应用阶段与①相同：branch → jump，实参原样搬运（两臂等价，取哪一臂都行）
        for Edit {
            branch,
            target,
            args,
        } in edits
        {
            data.replace_inst_with(branch).jump(target, args);
        }

        changed
    }

    pub fn remove_trivial_jump_block(data: &mut ArenaContextMut<'_>) -> bool {
        // —— 阶段一：收集"平凡跳转块"候选 ——
        // 平凡块 = 只有一条指令（终结符）、终结符是无参 jump、块自身无参数。
        // 前两条保证删除时无需搬运边参数，第三条保证前驱传入的实参可以直接
        // 丢弃而不改变目标块参数的值——这是整个化简不需要做参数匹配的前提
        let mut candidates = HashMap::default();
        // Skip entry bb
        // entry 是 layout 的第一块，删除它会失去函数唯一入口，因此从第二块开始扫
        for bb_layout in data.layout().basicblocks().iter().skip(1) {
            // 块里不止终结符一条指令：删掉会连累中间的普通指令，不能作为跳板
            if bb_layout.insts().len() != 1 {
                continue;
            }
            let first = *bb_layout.insts().get_first().unwrap();
            // Find a block that only have a jump instruction with no argument.
            // 终结符必须是 jump；branch/ret 有真实控制流语义，不能当跳板删除
            let InstKind::Jump(jump) = data.inst_data(first).kind() else {
                continue;
            };
            // 跳转带实参、或块自身带参数：任一成立，前驱的边参数就必须重定向到
            // 新目标，而新目标的参数表未必匹配，参数搬运会出错，直接放弃该块
            if !(jump.args().is_empty() && data.bb_data(bb_layout.bb()).params().is_empty()) {
                continue;
            }
            // Removing a self-jumping block would leave its predecessors
            // targeting a block that is no longer in the function layout.
            // 自跳块删除后，原指向它的前驱将失去合法目标（悬空），因此排除；
            // 自环的平凡块本来也无法"缩短路径"，删它没有意义
            if jump.target() == bb_layout.bb() {
                continue;
            }
            // 候选表：平凡块 → 它的跳转目标，阶段二沿着这张表解析跳转链
            candidates.insert(bb_layout.bb(), jump.target());
        }

        // —— 阶段二：把跳转链一次解析到底 ——
        // 沿 candidates 从起点追到链尾：`pre → mid1 → mid2 → exit` 时 pre 直接
        // 指向 exit，中间块全部可删；链上出现环（自环或互环）则整条链放弃——
        // 删除成环的块集会留下死循环或悬空前驱（与阶段一排除自跳同理）
        let mut final_targets: HashMap<BasicBlock, Option<BasicBlock>> = HashMap::default();
        for &start in candidates.keys() {
            // final_targets 兼作记忆化：解析过的起点（含判环失败的 None）直接复用，
            // 保证每条链只被走一次，总代价线性于候选数
            if final_targets.contains_key(&start) {
                continue;
            }
            let mut path = Vec::new();
            let mut path_indices = HashMap::default();
            let mut current = start;
            let final_target = loop {
                // 追到了已解析过的块：直接借用它的结论（可能是 None）
                if let Some(&resolved) = final_targets.get(&current) {
                    break resolved;
                }
                // 当前块已在本条路径上出现过 ⇒ 形成环，整条链放弃
                if path_indices.insert(current, path.len()).is_some() {
                    break None;
                }
                path.push(current);
                let target = candidates[&current];
                // 链尾：目标不是平凡块，它就是最终要保留的目标
                if !candidates.contains_key(&target) {
                    break Some(target);
                }
                current = target;
            };
            // 路径上所有块共享同一个结论（最终目标或放弃），一次性写入记忆表
            for block in path {
                final_targets.insert(block, final_target);
            }
        }

        // 只有解析出 Some 最终目标的块才是真正可删除的；None（成环）的块保留
        let removable = final_targets
            .iter()
            .filter_map(|(&block, target)| target.map(|target| (block, target)))
            .collect::<HashMap<_, _>>();
        if removable.is_empty() {
            return false;
        }

        // —— 阶段三：先把所有指向平凡块的边重定向到最终目标，再删块 ——
        let blocks = data
            .layout()
            .basicblocks()
            .iter()
            .map(|layout| layout.bb())
            .collect::<Vec<_>>();
        let mut rewrites = LogicalEdgeRewriter::new();
        let mut skipped_targets = std::collections::HashSet::new();
        for source in blocks {
            // 用逻辑边而不是结构边遍历：branch 两臂都指向同一平凡块时存在两条
            // 逻辑边，必须分别重定向；结构边（唯一 (源, 目标) 对）会漏掉一臂
            for edge in outgoing_edges(data.curr_func_data(), source) {
                if let Some(&target) = removable.get(&edge.target(data.curr_func_data())) {
                    // 边参数原样搬运到新目标：重定向不改这条边提供的实参，
                    // 最终目标的块参数仍由它一一喂入，参数语义保持一致
                    let args = edge.args(data.curr_func_data()).to_vec();
                    // The final target may carry parameters (the trivial
                    // jump block itself is parameterless, but its target
                    // need not be); retargeting with mismatched args would
                    // panic in the rewriter. Skip the edge conservatively —
                    // the candidate jump block stays in place (and is not
                    // removed).
                    if args.len() != data.bb_data(target).params().len() {
                        skipped_targets.insert(edge.target(data.curr_func_data()));
                        continue;
                    }
                    rewrites.retarget(data.curr_func_data(), edge, target, args);
                }
            }
        }
        // 所有终结符改写先收集进 LogicalEdgeRewriter、再一次性 apply，且 apply
        // 发生在删块之前——任何时刻 CFG 中都不存在指向已删除块的边，终结符不会
        // 悬空。这正是"不悬空终结符"的保证所在
        rewrites.apply(data);
        let mut removed_any = false;
        for &bb in removable.keys() {
            if skipped_targets.contains(&bb) {
                continue;
            }
            // 删块时同时 detach 块内指令的 def-use（remove_layout_basicblock），
            // 不留指向孤儿指令的引用
            data.curr_func_data_mut().remove_layout_basicblock(bb);
            removed_any = true;
        }
        removed_any
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{Program, Type, builder_trait::*},
        opt::utils::cfg::CFG,
    };

    #[test]
    fn keeps_trivial_self_loop_in_layout() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "self_loop".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let loop_block = data.new_basic_block().basic_block("loop".into(), vec![]);
        data.layout_mut().push_bb_back(loop_block);

        let enter = data.new_local_inst().jump(loop_block, vec![]);
        data.layout_mut().insert_inst(entry, enter);
        let backedge = data.new_local_inst().jump(loop_block, vec![]);
        data.layout_mut().insert_inst(loop_block, backedge);

        let mut context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        assert!(!SimplifyCFG::remove_trivial_jump_block(&mut context));
        assert!(CFG::new(context.curr_func_data()).is_some());
        assert_eq!(
            context
                .curr_func_data()
                .layout()
                .basicblock(loop_block)
                .terminator(),
            backedge
        );
    }

    #[test]
    fn redirects_both_same_target_branch_arms() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "same_target".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let trivial = data.new_basic_block().basic_block("trivial".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        data.layout_mut().push_bb_back(trivial);
        data.layout_mut().push_bb_back(exit);

        let cond = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(cond, trivial, vec![], trivial, vec![]);
        data.layout_mut().insert_inst(entry, branch);
        let bypass = data.new_local_inst().jump(exit, vec![]);
        data.layout_mut().insert_inst(trivial, bypass);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        let mut context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        assert!(SimplifyCFG::remove_trivial_jump_block(&mut context));
        let InstKind::Branch(branch_data) = context.inst_data(branch).kind() else {
            panic!("entry terminator must remain a branch");
        };
        assert_eq!(branch_data.t_target(), exit);
        assert_eq!(branch_data.f_target(), exit);
        assert!(!context.bb_data(exit).used_by().contains(&bypass));
        assert!(CFG::new(context.curr_func_data()).is_some());
    }

    #[test]
    fn removes_a_chain_of_trivial_blocks_in_one_run() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "trivial_chain".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let mut blocks = Vec::new();
        for index in 0..1_000 {
            let block = data
                .new_basic_block()
                .basic_block(format!("trivial_{index}"), vec![]);
            data.layout_mut().push_bb_back(block);
            blocks.push(block);
        }
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        data.layout_mut().push_bb_back(exit);

        let enter = data.new_local_inst().jump(blocks[0], vec![]);
        data.layout_mut().insert_inst(entry, enter);
        for (index, &block) in blocks.iter().enumerate() {
            let target = blocks.get(index + 1).copied().unwrap_or(exit);
            let jump = data.new_local_inst().jump(target, vec![]);
            data.layout_mut().insert_inst(block, jump);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        let mut context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        assert!(SimplifyCFG::remove_trivial_jump_block(&mut context));
        assert_eq!(context.curr_func_data().layout().basicblocks().len(), 2);
        assert!(matches!(
            context.inst_data(enter).kind(),
            InstKind::Jump(jump) if jump.target() == exit
        ));
        assert!(CFG::new(context.curr_func_data()).is_some());
    }

    #[test]
    fn keeps_a_trivial_block_cycle() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "trivial_cycle".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let first = data.new_basic_block().basic_block("first".into(), vec![]);
        let second = data.new_basic_block().basic_block("second".into(), vec![]);
        data.layout_mut().push_bb_back(first);
        data.layout_mut().push_bb_back(second);

        let enter = data.new_local_inst().jump(first, vec![]);
        data.layout_mut().insert_inst(entry, enter);
        let to_second = data.new_local_inst().jump(second, vec![]);
        data.layout_mut().insert_inst(first, to_second);
        let to_first = data.new_local_inst().jump(first, vec![]);
        data.layout_mut().insert_inst(second, to_first);

        let mut context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        assert!(!SimplifyCFG::remove_trivial_jump_block(&mut context));
        assert_eq!(context.curr_func_data().layout().basicblocks().len(), 3);
        assert!(CFG::new(context.curr_func_data()).is_some());
    }
}
