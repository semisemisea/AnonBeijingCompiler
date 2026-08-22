//! # DCE 家族：指令级 / Phi 级 / 块级 / 函数级死代码消除
//!
//! 本模块清掉四类"不会被程序观察到的"IR 成分——结果未使用的指令
//! （`DeadCodeElimination`）、死 / 转发 Phi 参数（`DeadPhiElimination`）、从入口
//! 不可达的基本块（`UnreachableBasicBlock`）、从 `main` 沿调用图不可达的函数
//! （`DeadFunctionElimination`）——另有一个已声明但实现被注释掉、尚未启用的跳转块
//! 消除 `JumpOnlyElimination`。它们共同把其它 pass 留下的"垃圾"收敛干净，缩小后续
//! pass 与后端的工作面。
//!
//! 术语：SSA、block 参数（本项目的 Phi）、used_by / def-use 链、结构边 vs 逻辑边、
//! 固定点（fixpoint）、DCE / DFE 等见 `docs/offline-handbook/glossary.md`，这里不
//! 展开。文件内已有的英文行注释描述各实现的局部细节，本文档给整体视图。
//!
//! ## DeadCodeElimination：指令级死代码消除（mark-and-sweep）
//!
//! 经典的标记-清扫（mark and sweep）：先标记所有"关键"指令，再沿 def-use 反向
//! 传播把关键指令的操作数也标记为存活，最后清扫未标记指令。
//!
//! ```text
//! // 变换前                                   // 变换后
//! entry:                                    entry:
//!   %a = Integer 1                           %a = Integer 1
//!   %b = Binary Add %a, %a    // 结果未使用   →（删除）
//!   %c = Call pure_fn()        // 纯函数、结果未使用 →（删除）
//!   ret %a                                    ret %a
//! ```
//!
//! - 标记种子：`is_critical` 判定——`Branch` / `Jump` / `Store` / `MemZero` /
//!   `Return` / `TailCall` 恒为关键（控制流与副作用不能删）；`Call` 只有在全程序
//!   纯度分析（`EffectAnalysis::new` + `is_removable`）确认被调者无副作用且结果
//!   未用时才不关键（注释 "rdf is not ready"：按需 def-use 尚未就绪，调用默认按
//!   关键处理）；纯计算类（`Cast` / `Load` / `Binary` / `Select` / `Fma` /
//!   `GetElemPtr` / `Vector*`）不是种子，只靠被关键指令使用才存活；`Integer` /
//!   `Aggregate` / `BlockArgRef` / `Undef` / `ZeroInit` / `GlobalAlloc` 在
//!   `is_critical` 里是 `unreachable!()`——它们永远不会是种子，靠使用者存活。
//! - 生长：从 worklist 弹出后按指令 kind 逐个标记操作数（`mark_live!` 宏；全局值
//!   `is_global` 跳过，全局由 `GlobalAlloc` 语义单独管理）。弹出时先查
//!   `has_inst_data`：前面的 pass（如过程间 SCCP）可能替换指令留下悬空操作数，
//!   直接跳过。
//! - 清扫：收集所有未标记指令（连同所在块），`remove_layout_inst` 逐条删除，
//!   返回是否发生变化。
//!
//! 入口有两个：
//! - `run`（整程序）：先做全程序纯度分析，把对无副作用被调者且结果未用的调用收进
//!   `removable_calls`，再逐函数跑 `run_on_func`；
//! - `run_on`（单函数）：不带纯度分析，`removable_calls` 为空——历史保守行为，
//!   任何 `Call` 都不删。
//!
//! 触发 / 放弃条件：
//! - 触发：指令不在存活集合中（结果未被任何关键指令传递引用）；
//! - 放弃：I/O 类调用（`getint` 等经 I/O 标志保留）、会写全局变量的函数调用
//!   （经全局 / 指针改写内存使其不纯）、结果仍被使用的调用——分别由测试
//!   `keeps_call_to_io_function` / `keeps_call_to_function_writing_a_global` /
//!   `keeps_call_whose_result_is_used` 锁定；`has_side_effect` 是预留的副作用规则
//!   占位（`#[allow(dead_code)]`，恒返回 true，尚未接入判定）。
//!
//! 正确性要点：控制流与副作用指令作为种子必然存活；存活沿 def-use 反向传播，被
//! 使用的值不会被误删；`MemZero` 的分配与长度（`MemZeroLen::Value` 形态）和
//! `Store` 一样被标记（测试 `preserves_mem_zero_and_its_allocation`）。
//!
//! ## DeadPhiElimination：死 Phi / 转发 Phi 清理
//!
//! block 参数即本项目的 Phi（见术语表）。本 pass 清理两类"死"参数：①转发参数——
//! 所有逻辑入边喂进来的是同一个值，参数只是拷贝，直接替换为源值；②未使用参数——
//! `used_by` 为空，连同每个前驱终结符对应位置的实参一起删掉。
//!
//! ① 转发参数（单值转发）：
//! ```text
//! // 变换前                                  // 变换后
//! entry:      jump merge(%v)                entry:      jump merge
//! merge(%p):  %r = ret %p                   merge:       %r = ret %v
//! ```
//! ② 未使用参数（死参数，`%p` 无人引用）：
//! ```text
//! // 变换前                                       // 变换后
//! entry:      br %cond, merge(%z), merge(%o)      entry:      br %cond, merge, merge
//! merge(%p):  ret                                 merge:      ret
//! ```
//!
//! 流程（都在 `run_on` 里）：
//! - 转发：`CFG::new` 建快照 → `forwarded_block_params` 找"每条逻辑入边都喂同一
//!   SSA 值"的参数（注意结构边 vs 逻辑边：branch 两臂指向同一块时仍按两臂分别
//!   比对，双臂值不同时不误报）→ `resolve_forwarded_params` 把转发链解到终点值
//!   （成环的链放弃）→ `utils::visit_and_replace` 在函数内替换所有使用点；
//! - 死参数：按 `params[i].used_by().is_empty()` 找索引（**降序**收集，`remove`
//!   保持其余参数的相对顺序——`swap_remove` 会打乱尾部，而前驱的实参向量必须做
//!   同样的位置置换才能与块参数保持对齐）；
//! - 重写前驱：`bb_data(bb).used_by()` 找到跳进本块的终结符，`Jump` 删对应位置
//!   实参、`Branch` 两臂**分别**删（同一块可能同时是 t_target 与 f_target），
//!   `replace_inst_with` 重建终结符。
//!
//! 触发 / 放弃条件：
//! - 触发：转发参数（所有逻辑入边值相同且不等于参数自身）或死参数（`used_by`
//!   为空）；
//! - 放弃：**entry 块参数永不处理**——它们是函数 ABI 参数，由 `FunctionData::params`
//!   独立跟踪，删除会使两处失同步留下悬空引用；回边也不转发 entry 参数（测试
//!   `never_forwards_entry_abi_parameters_from_backedges`）。
//!
//! 正确性要点：删除的是无使用者的参数，替换后无悬空 def-use；实参删除与参数删除
//! 索引一一对应，SSA 边参数对齐跨多轮运行保持（`jump_args_stay_aligned_when_
//! trailing_params_are_dead`：9 个参数只死尾部两个，跳转实参仍精确剩 7 个）。
//!
//! ## UnreachableBasicBlock：不可达块删除
//!
//! 删除从函数入口不可达的基本块。固定点循环：每轮从零重建 CFG 快照
//! （`cfg::build_cfg_both`），找出没有前驱的块删除，直到删不动为止。
//!
//! ```text
//! // 变换前                                    // 变换后
//! entry:      br %cond, live, dead             entry:      br %cond, live, dead
//! dead:       %x = Integer 1; ret              （%x 无 used_by → 整块删除）
//! live:       ret                              live:       ret
//! ```
//!
//! 触发 / 放弃条件：
//! - 候选：CFG 快照里入度为零的块（`prece[id].is_empty()`）或根本不在快照里的块
//!   （`get_id_safe` 为 None）；
//! - **安全闸**：块内所有指令 `used_by` 全空才删——不可达块仍可能向可达块供值
//!   （例如 LICM 外提到死块里的 GEP 被幸存循环体使用），删掉会留下悬空操作数；
//! - 无入口块直接返回 false；一轮删不动（候选都有活值）就停止——后续迭代不可能
//!   再进步；`removed_any` 为真才继续下一轮（删块可能让更多块变不可达，连锁效应
//!   靠下一轮的新快照发现）。
//!
//! 正确性要点：只在整块指令都无使用者时删除，不破坏 def-use；本 pass 只从 layout
//! 移除块（`remove_layout_basicblock`），不重写指向已删块的终结符——每轮重建快照
//! 时它们自然消失，悬空终结符留给调用方 / 后续 pass。
//!
//! 管线位置：**不在** `from_config` 主管线里注册——被 `ssa.rs`（SSATransform
//! 收尾）和 `const_prop.rs` 直接调用（`super::dce::UnreachableBasicBlock`），作为
//! 这两个 pass 的清理步骤。
//!
//! ## DeadFunctionElimination：死函数消除
//!
//! 从 `main` 出发沿调用图做可达性闭包，把不可达的**已定义**函数移出函数布局。
//! 动机：指令级 DCE 只删指令，会留下整段无用函数，其内部调用点随之被拖进汇编
//! （例如一个未被使用的 crypto helper 保留着它的 rotl / and 调用）。
//!
//! ```text
//! // 变换前（main 出发的调用图）                 // 变换后
//! main:   call a; ret         main:   call a; ret
//! a:      call b; ret         a:      call b; ret
//! b:      ret                 b:      ret
//! c:      call d; ret         （c、d 从 main 不可达 → 移出函数布局）
//! d:      ret
//! ```
//!
//! 流程（`run`，整程序）：`call_graph::CallGraph::new` 建调用图 →
//! `get_main_function` 为根，队列 BFS（`callees_in` 展开）得 `reachable` 集合 →
//! 布局中不在集合内且非声明桩（`!layout().is_decl()`）的函数判死 →
//! `program.remove_function` 逐个移除。
//!
//! 触发 / 放弃条件：
//! - 触发：从 `main` 不可达（传递闭包）且不是声明桩；
//! - 可达性判定是闭包而非调用点计数——自递归且无外部调用者的函数也被删
//!   （`removes_a_self_recursive_function`，计数法会误判为"被调用过"）；
//! - 放弃：声明桩（运行时库接口，`preserves_decl_and_main`）与 `main` 本身
//!   （闭包根）恒保留。
//!
//! 正确性要点：只从函数布局移除，`FunctionData` 仍留在 arena——函数句柄是下标，
//! arena 级删除会失效所有后续句柄；调用链上传递保留（`removes_transitively_dead_
//! functions`：d → e 一起被删）。
//!
//! ## JumpOnlyElimination：跳转块消除（仅声明，未启用）
//!
//! 消除"纯转发"块：块内只有一条无实参 `jump`、目标块也没有参数（不携带任何 Phi
//! 值），把前驱终结符直接重定向到目标块后删除该块。目前**仅声明**了结构体
//! （本文件 `pub struct JumpOnlyElimination;`），`impl Pass` 整体被注释掉（文件
//! 底部，TODO 注明：这是 SimplifyCFG，应迁到独立的 pass 文件）——未注册、未运行。
//! 以下形态与条件取自注释掉的实现意图：
//!
//! ```text
//! // 变换前                // 变换后
//! a:  jump b              a:  jump c
//! b:  jump c              （b 被删除，a 的终结符直接指向 c）
//! c:  ret                 c:  ret
//! ```
//!
//! - 候选：块首条指令是 `jump` 且 `jump.args()` 与目标块 `params()` 均为空；
//! - 重写：经 `bb.used_by()` 找前驱终结符——`Jump` 改 `target`、`Branch` 改对应臂
//!   （`true_bb_mut` / `false_bb_mut`）；删除 entry 块的分支被注释掉（不删入口）。
//!
//! ## 管线位置与门控
//!
//! 注册点都在 `opt/pass.rs` 的 `PassesManager::from_config`，fixpoint 段**末尾**
//! （`gvn_pre::GVNPRE` 之后）：
//!
//! - `DeadPhiElimination` → `DeadCodeElimination` 紧邻注册（`p.register`）；
//! - `DeadFunctionElimination` 受 `config.dead_function_elimination` 门控，为 true
//!   才挂载——ABI 观察测试走不带它的管线（它们断言"优化了但不可达的 helper"的
//!   参数绑定）；
//! - `UnreachableBasicBlock` 不在 from_config（见上，由 `ssa.rs` / `const_prop.rs`
//!   直接调用）；`JumpOnlyElimination` 未注册。
//!
//! fixpoint 语义（见术语表）：整段反复执行到一轮内无任何 pass 改变 IR（上限 100
//! 轮）——前面的 pass 每轮新造的死代码，由本模块在后续轮次收敛干净。
//!
//! ## 验证
//!
//! - 本文件 `mod tests`（242 行起）：DCE 的 7 个用例——`preserves_mem_zero_and_
//!   its_allocation` / `removes_call_to_pure_function_with_unused_result` /
//!   `keeps_call_to_io_function` / `keeps_call_to_function_writing_a_global` /
//!   `keeps_call_whose_result_is_used` / `removes_pure_unused_calls_but_keeps_io_
//!   calls` / `keeps_pure_calls_whose_result_is_used`；
//! - `mod dead_phi_tests`（580 行起）：DeadPhiElimination 的 4 例
//!   （`never_forwards_entry_abi_parameters_from_backedges` /
//!   `removes_dead_param_from_both_same_target_branch_arms` /
//!   `eliminates_a_block_parameter_forwarding_one_value` /
//!   `jump_args_stay_aligned_when_trailing_params_are_dead`）+ DeadFunctionElimination
//!   的 3 例（`removes_transitively_dead_functions` / `removes_a_self_recursive_
//!   function` / `preserves_decl_and_main`）；
//! - 端到端：`make test` 差分比对（stdout + 退出码 vs `.out`）；本地快速回归：
//!   `cargo test -p raana_ir`。
//!
use crate::opt::{
    analysis_passes::effects::EffectAnalysis,
    prelude::*,
    utils::{
        self,
        cfg::CFG,
        logical_edge::{forwarded_block_params, resolve_forwarded_params},
    },
};

pub struct DeadPhiElimination;
pub struct DeadCodeElimination;
pub struct UnreachableBasicBlock;
pub struct JumpOnlyElimination;

/// Mark and sweep algorithm
/// To start the process, we mark all the useful instructions, including:
/// I/O
/// Function (call to function)
/// Branches and Return
impl Pass for DeadCodeElimination {
    fn run(&mut self, program: &mut Program) -> bool {
        // Whole-program purity analysis lets the mark phase drop calls to
        // effect-free callees whose result is unused (getint and friends
        // are preserved through the I/O flags).
        let analysis = EffectAnalysis::new(program);
        let mut changed = false;
        for func in program.function_layout().to_vec() {
            let mut removable_calls = HashSet::default();
            let data = program.func_data(func);
            for bb_layout in data.layout().basicblocks() {
                for &inst in bb_layout.insts() {
                    if let InstKind::Call(call) = data.inst_data(inst).kind() {
                        if analysis.is_removable(call.callee()) {
                            removable_calls.insert(inst);
                        }
                    }
                }
            }
            let mut arena_context = ArenaContextMut {
                program,
                curr_func: Some(func),
            };
            changed |= DeadCodeElimination::run_on_func(self, &mut arena_context, &removable_calls);
        }
        changed
    }

    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        // Direct per-function invocation without the purity analysis: no
        // call is removable (the historical conservative behavior).
        self.run_on_func(data, &HashSet::default())
    }
}

// TODO: side-effet function rules.
#[allow(dead_code)]
fn has_side_effect(_func: Function) -> bool {
    true
}

#[inline]
fn is_critical(value: Inst, data: &FunctionData, removable_calls: &HashSet<Inst>) -> bool {
    match data.inst_data(value).kind() {
        InstKind::Branch(..)
        | InstKind::Jump(..)
        | InstKind::Store(..)
        | InstKind::MemZero(..)
        | InstKind::Return(..)
        | InstKind::TailCall(..) => true,
        InstKind::GlobalAlloc(..)
        | InstKind::BlockArgRef(..)
        | InstKind::Aggregate(..)
        | InstKind::Undef
        | InstKind::ZeroInit
        | InstKind::Integer(..) => unreachable!(),
        InstKind::Cast(..)
        | InstKind::Float(..)
        | InstKind::Alloc
        | InstKind::Load(..)
        | InstKind::GetElemPtr(..)
        | InstKind::Binary(..)
        | InstKind::Select(..)
        | InstKind::Fma(..)
        | InstKind::VectorSplat(..)
        | InstKind::VectorExtractElement(..)
        | InstKind::VectorInsertElement(..)
        | InstKind::VectorReduce(..) => false,
        // rdf is not ready
        InstKind::Call(..) => !removable_calls.contains(&value),
    }
}

impl DeadCodeElimination {
    pub(crate) fn run_on_func(
        &self,
        data: &mut ArenaContextMut<'_>,
        removable_calls: &HashSet<Inst>,
    ) -> bool {
        let mut worklist = VecDeque::new();
        let mut live_inst = HashSet::default();

        macro_rules! mark_live {
            ($inst: expr) => {
                if !live_inst.contains(&$inst) && !$inst.is_global() {
                    worklist.push_back($inst);
                    live_inst.insert($inst);
                }
            };
        }

        // 1.1 Mark: Initiate
        for layout in data.layout().basicblocks() {
            for &inst in layout.insts() {
                if is_critical(inst, data, removable_calls) {
                    mark_live!(inst);
                }
            }
        }

        // 1.2 Mark: Grow
        while let Some(inst) = worklist.pop_front() {
            // An earlier pass (e.g. interprocedural SCCP) may have replaced
            // an instruction and left a dangling reference in another inst's
            // operand list. Skip insts that no longer exist.
            if !data.has_inst_data(inst) && !inst.is_global() {
                continue;
            }
            match data.inst_data(inst).kind() {
                InstKind::GlobalAlloc(..)
                | InstKind::Alloc
                | InstKind::BlockArgRef(..)
                | InstKind::Undef
                | InstKind::ZeroInit
                | InstKind::Float(..)
                | InstKind::Integer(..) => continue,
                InstKind::Aggregate(agg) => {
                    for &elem in agg.value() {
                        mark_live!(elem);
                    }
                }
                InstKind::Cast(cast) => mark_live!(cast.src()),
                InstKind::Return(ret) => {
                    if let Some(inst) = ret.value() {
                        mark_live!(inst);
                    }
                }
                InstKind::Store(store) => {
                    mark_live!(store.src());
                    mark_live!(store.dest());
                }
                InstKind::MemZero(mem_zero) => {
                    mark_live!(mem_zero.dest());
                    if let crate::ir::inst_kind::mem_zero::MemZeroLen::Value(byte_len) =
                        mem_zero.byte_len_len()
                    {
                        mark_live!(*byte_len);
                    }
                }
                InstKind::Load(load) => {
                    mark_live!(load.src());
                }
                InstKind::GetElemPtr(get_elem_ptr) => {
                    mark_live!(get_elem_ptr.base());
                    get_elem_ptr
                        .offsets()
                        .iter()
                        .for_each(|&inst| mark_live!(inst));
                }
                InstKind::Binary(binary) => {
                    mark_live!(binary.lhs());
                    mark_live!(binary.rhs());
                }
                InstKind::Select(select) => {
                    mark_live!(select.cond());
                    mark_live!(select.if_true());
                    mark_live!(select.if_false());
                }
                InstKind::Fma(fma) => {
                    mark_live!(fma.acc());
                    mark_live!(fma.lhs());
                    mark_live!(fma.rhs());
                }
                InstKind::VectorSplat(splat) => mark_live!(splat.src()),
                InstKind::VectorExtractElement(extract) => {
                    mark_live!(extract.src());
                    mark_live!(extract.index());
                }
                InstKind::VectorInsertElement(insert) => {
                    mark_live!(insert.vector());
                    mark_live!(insert.element());
                    mark_live!(insert.index());
                }
                InstKind::VectorReduce(reduce) => mark_live!(reduce.src()),
                InstKind::Branch(branch) => {
                    mark_live!(branch.cond());
                    for &ta in branch.t_args() {
                        mark_live!(ta);
                    }
                    for &fa in branch.f_args() {
                        mark_live!(fa);
                    }
                }
                InstKind::Jump(jump) => {
                    for &ja in jump.args() {
                        mark_live!(ja)
                    }
                }
                InstKind::Call(call) => {
                    for &ca in call.args() {
                        mark_live!(ca)
                    }
                }
                InstKind::TailCall(tail_call) => {
                    for &arg in tail_call.args() {
                        mark_live!(arg)
                    }
                }
            }
        }

        // 2 Sweep
        let mut rename_list = Vec::new();
        for layout in data.layout().basicblocks() {
            rename_list.extend(
                layout
                    .insts()
                    .iter()
                    .copied()
                    .filter(|inst| !live_inst.contains(inst))
                    .zip(std::iter::repeat(layout.bb())),
            );
        }
        let changed = !rename_list.is_empty();
        for (inst, bb) in rename_list {
            data.remove_layout_inst(bb, inst);
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::{DeadCodeElimination, Pass};
    use crate::{
        ir::{Program, Type, arena::Arena, builder_trait::*},
        opt::pass::ArenaContextMut,
    };

    /// A function with an entry block and a bare `ret`.
    fn empty_function(program: &mut Program, name: &str) -> crate::ir::Function {
        let function = program.new_function(Type::get_unit(), name.into(), vec![]);
        let mut data = ArenaContextMut {
            program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);
        function
    }

    #[test]
    fn preserves_mem_zero_and_its_allocation() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "clear".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let alloc = data
            .new_local_inst()
            .alloc(Type::get_array(Type::get_i32(), 4));
        let clear = data.new_local_inst().mem_zero(alloc, 16);
        data.layout_mut().insert_inst(entry, clear);
        let one = data.new_local_inst().integer(1);
        let dead = data
            .new_local_inst()
            .binary(crate::ir::BinaryOp::Add, one, one);
        data.layout_mut().insert_inst(entry, dead);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        assert!(DeadCodeElimination.run(&mut program));
        let data = program.func_data(function);
        let insts = data.layout().basicblock(entry).insts();
        assert!(insts.iter().any(|&inst| inst == clear));
        assert!(!insts.iter().any(|&inst| inst == dead));
        assert!(data.inst_data(alloc).used_by().contains(&clear));
    }

    #[test]
    fn removes_call_to_pure_function_with_unused_result() {
        let mut program = Program::new();
        let callee = empty_function(&mut program, "callee");
        let caller = program.new_function(Type::get_unit(), "caller".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(caller),
        };
        let entry = data.add_entry_block();
        let call = data.new_local_value().call(callee, vec![]);
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        assert!(DeadCodeElimination.run(&mut program));
        let data = program.func_data(caller);
        let insts = data.layout().basicblock(entry).insts();
        assert!(!insts.iter().any(|&inst| inst == call));
        assert_eq!(insts.len(), 1); // only the ret remains
    }

    #[test]
    fn keeps_call_to_io_function() {
        let mut program = Program::new();
        // A declaration is enough: the analysis keys on the callee name.
        let getint = program.new_function(Type::get_i32(), "getint".into(), vec![]);
        let caller = program.new_function(Type::get_unit(), "caller".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(caller),
        };
        let entry = data.add_entry_block();
        let call = data.new_local_value().call(getint, vec![]);
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        assert!(!DeadCodeElimination.run(&mut program));
        let data = program.func_data(caller);
        assert!(
            data.layout()
                .basicblock(entry)
                .insts()
                .iter()
                .any(|&inst| inst == call)
        );
    }

    #[test]
    fn keeps_call_to_function_writing_a_global() {
        let mut program = Program::new();
        let init = program.new_value().zero_init(Type::get_i32());
        let global = program.new_value().global_alloc(init);
        let writer = program.new_function(Type::get_unit(), "writer".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(writer),
        };
        let entry = data.add_entry_block();
        let one = data.new_local_value().integer(1);
        let store = data.new_local_value().store(one, global);
        data.layout_mut().insert_inst(entry, store);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let caller = program.new_function(Type::get_unit(), "caller".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(caller),
        };
        let entry = data.add_entry_block();
        let call = data.new_local_value().call(writer, vec![]);
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        assert!(!DeadCodeElimination.run(&mut program));
        let data = program.func_data(caller);
        assert!(
            data.layout()
                .basicblock(entry)
                .insts()
                .iter()
                .any(|&inst| inst == call)
        );
    }

    #[test]
    fn keeps_call_whose_result_is_used() {
        let mut program = Program::new();
        let callee = program.new_function(Type::get_i32(), "callee".into(), vec![]);
        {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(callee),
            };
            let entry = data.add_entry_block();
            let one = data.new_local_value().integer(1);
            let ret = data.new_local_value().ret(Some(one));
            data.layout_mut().insert_inst(entry, ret);
        }
        let caller = program.new_function(Type::get_i32(), "caller".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(caller),
        };
        let entry = data.add_entry_block();
        let call = data.new_local_value().call(callee, vec![]);
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_value().ret(Some(call));
        data.layout_mut().insert_inst(entry, ret);

        assert!(!DeadCodeElimination.run(&mut program));
        let data = program.func_data(caller);
        assert!(
            data.layout()
                .basicblock(entry)
                .insts()
                .iter()
                .any(|&inst| inst == call)
        );
    }

    #[test]
    fn removes_pure_unused_calls_but_keeps_io_calls() {
        let mut program = Program::new();
        // A strictly pure callee with no body at all.
        let pure_callee = program.new_function(Type::get_i32(), "pure".into(), vec![]);
        let pure_data = program.func_data_mut(pure_callee);
        let pure_entry = pure_data.add_entry_block();
        // Constants stay outside the block layout; only the return is laid out.
        let one = pure_data.new_local_inst().integer(1);
        let pure_ret = pure_data.new_local_inst().ret(Some(one));
        pure_data.layout_mut().insert_inst(pure_entry, pure_ret);

        // A library I/O function (no body; identified by name).
        let io_callee = program.new_function(Type::get_i32(), "getint".into(), vec![]);

        let function = program.new_function(Type::get_unit(), "caller".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let pure_call = data
            .new_local_inst()
            .call_with_type(pure_callee, vec![], Type::get_i32());
        data.layout_mut().insert_inst(entry, pure_call);
        let io_call = data
            .new_local_inst()
            .call_with_type(io_callee, vec![], Type::get_i32());
        data.layout_mut().insert_inst(entry, io_call);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        assert!(DeadCodeElimination.run(&mut program));
        let data = program.func_data(function);
        let insts = data.layout().basicblock(entry).insts();
        assert!(
            !insts.iter().any(|&inst| inst == pure_call),
            "unused pure call must be removed"
        );
        assert!(
            insts.iter().any(|&inst| inst == io_call),
            "I/O call must be kept"
        );
    }

    #[test]
    fn keeps_pure_calls_whose_result_is_used() {
        let mut program = Program::new();
        let pure_callee = program.new_function(Type::get_i32(), "pure".into(), vec![]);
        let pure_data = program.func_data_mut(pure_callee);
        let pure_entry = pure_data.add_entry_block();
        let one = pure_data.new_local_inst().integer(1);
        let pure_ret = pure_data.new_local_inst().ret(Some(one));
        pure_data.layout_mut().insert_inst(pure_entry, pure_ret);

        let function = program.new_function(Type::get_i32(), "caller".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let pure_call = data
            .new_local_inst()
            .call_with_type(pure_callee, vec![], Type::get_i32());
        data.layout_mut().insert_inst(entry, pure_call);
        let ret = data.new_local_inst().ret(Some(pure_call));
        data.layout_mut().insert_inst(entry, ret);

        assert!(!DeadCodeElimination.run(&mut program));
        let data = program.func_data(function);
        let insts = data.layout().basicblock(entry).insts();
        assert!(insts.iter().any(|&inst| inst == pure_call));
    }
}

impl Pass for DeadPhiElimination {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        let forwarded = CFG::new(data)
            .map(|cfg| forwarded_block_params(data, &cfg))
            .unwrap_or_default();
        let resolved = resolve_forwarded_params(&forwarded);
        for (&parameter, &replacement) in &resolved {
            utils::visit_and_replace(data, parameter, replacement);
        }

        let mut bb_allocator: IDAllocator<BasicBlock, BId> = IDAllocator::new(1);
        let mut unused_params_indices = Vec::with_capacity(data.layout().basicblocks().len());

        let entry_bb = data.layout().entry_bb().map(|l| l.bb());
        for (assert_id, layout) in data.layout().basicblocks().iter().enumerate() {
            assert_eq!(bb_allocator.check_or_alloc_id_same(layout.bb()), assert_id);
            // Entry block parameters are the function's ABI parameters and are
            // tracked by FunctionData::params independently of the block's
            // params list. Removing one here would desynchronize the two,
            // leaving a dangling reference. Keep them all.
            if Some(layout.bb()) == entry_bb {
                unused_params_indices.push(Vec::new());
                continue;
            }
            let params = data.bb_data(layout.bb()).params();
            let unused_params_index = (0..params.len())
                .filter(|&index| data.inst_data(params[index]).used_by().is_empty())
                .rev()
                .collect::<Vec<_>>();
            unused_params_indices.push(unused_params_index);
        }
        let mut changed = !resolved.is_empty();
        for (i, unused_params_index) in unused_params_indices.into_iter().enumerate() {
            if unused_params_index.is_empty() {
                continue;
            }
            changed = true;
            let bb = bb_allocator.search_id(i);

            // `unused_params_index` is descending, so positional `remove`
            // keeps the remaining parameters in their original relative
            // order. `swap_remove` would also work positionally, but it
            // reorders the tail and every predecessor's argument vector
            // must be permuted identically for the block's phi values to
            // stay aligned across repeated runs.
            for &index in unused_params_index.iter() {
                let _val = data.bb_data_mut(bb).params_mut().remove(index);
            }

            let jump_inst = data
                .bb_data(bb)
                .used_by()
                .iter()
                .copied()
                // TODO: You can remove this if you correctly implement inst data remove.
                .filter(|&inst| data.layout().parent_bb(inst).is_some())
                .collect::<Vec<_>>();

            for inst in jump_inst {
                match data.inst_data(inst).kind() {
                    InstKind::Jump(jump) => {
                        let t = jump.target();
                        let mut a = jump.args().to_vec();
                        for &index in unused_params_index.iter() {
                            a.remove(index);
                        }
                        data.replace_inst_with(inst).jump(t, a);
                    }
                    InstKind::Branch(branch) => {
                        let c = branch.cond();
                        let tt = branch.t_target();
                        let ft = branch.f_target();
                        let mut ta = branch.t_args().to_vec();
                        let mut fa = branch.f_args().to_vec();
                        // The same block may be both the true and the false
                        // target of a branch; trim each side independently so
                        // the rebuilt branch keeps args aligned with params.
                        if bb == branch.t_target() {
                            for &index in unused_params_index.iter() {
                                ta.remove(index);
                            }
                        }
                        if bb == branch.f_target() {
                            for &index in unused_params_index.iter() {
                                fa.remove(index);
                            }
                        }
                        data.replace_inst_with(inst).branch(c, tt, ta, ft, fa);
                    }
                    _ => unreachable!(),
                }
            }
        }
        changed
    }
}

#[cfg(test)]
mod dead_phi_tests {
    use super::{DeadFunctionElimination, DeadPhiElimination, Pass};
    use crate::{
        ir::{BinaryOp, InstKind, Program, Type, arena::Arena, builder_trait::*},
        opt::pass::ArenaContextMut,
    };

    #[test]
    fn never_forwards_entry_abi_parameters_from_backedges() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "entry_loop".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        data.layout_mut().push_bb_back(exit);
        let parameter = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let branch = data
            .new_local_inst()
            .branch(parameter, entry, vec![zero], exit, vec![]);
        data.layout_mut().insert_inst(entry, branch);
        let ret = data.new_local_inst().ret(Some(parameter));
        data.layout_mut().insert_inst(exit, ret);

        assert!(!DeadPhiElimination.run(&mut program));
        let data = program.func_data(function);
        let InstKind::Branch(branch) = data.inst_data(branch).kind() else {
            panic!("entry terminator must remain a branch");
        };
        assert_eq!(branch.cond(), parameter);
        let InstKind::Return(ret) = data.inst_data(ret).kind() else {
            panic!("exit must remain a return");
        };
        assert_eq!(ret.value(), Some(parameter));
    }

    #[test]
    fn removes_dead_param_from_both_same_target_branch_arms() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_unit(), "dead_phi".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        data.layout_mut().push_bb_back(merge);

        let cond = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let one = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(cond, merge, vec![zero], merge, vec![one]);
        data.layout_mut().insert_inst(entry, branch);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(merge, ret);

        assert!(DeadPhiElimination.run(&mut program));
        let data = program.func_data(function);
        assert!(data.bb_data(merge).params().is_empty());
        let InstKind::Branch(branch_data) = data.inst_data(branch).kind() else {
            panic!("entry terminator must remain a branch");
        };
        assert!(branch_data.t_args().is_empty());
        assert!(branch_data.f_args().is_empty());
    }

    #[test]
    fn eliminates_a_block_parameter_forwarding_one_value() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "forwarded_phi".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        data.layout_mut().push_bb_back(merge);
        let value = data.new_local_inst().integer(7);
        let jump = data.new_local_inst().jump(merge, vec![value]);
        data.layout_mut().insert_inst(entry, jump);
        let parameter = data.bb_data(merge).params()[0];
        let ret = data.new_local_inst().ret(Some(parameter));
        data.layout_mut().insert_inst(merge, ret);

        assert!(DeadPhiElimination.run(&mut program));
        let data = program.func_data(function);
        assert!(data.bb_data(merge).params().is_empty());
        let InstKind::Jump(jump) = data.inst_data(jump).kind() else {
            panic!("entry must still jump to merge");
        };
        assert!(jump.args().is_empty());
        let InstKind::Return(ret) = data.inst_data(ret).kind() else {
            panic!("merge must still return");
        };
        assert_eq!(ret.value(), Some(value));
    }

    #[test]
    fn jump_args_stay_aligned_when_trailing_params_are_dead() {
        // A block whose *trailing* parameters are dead: the jump arguments
        // must drop the same positions, keeping earlier args aligned.
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_unit(), "dead_tail".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let merge = data.new_basic_block().basic_block(
            "merge".into(),
            vec![
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        data.layout_mut().push_bb_back(merge);
        let cond = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let one = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(cond, merge, vec![one; 9], merge, vec![zero; 9]);
        data.layout_mut().insert_inst(entry, branch);
        // Only params 7 and 8 are unused; use the others so only the tail
        // two get removed.
        for i in 0..7 {
            let p = data.bb_data(merge).params()[i];
            let _use = data.new_local_inst().binary(BinaryOp::Add, p, one);
            data.layout_mut().insert_inst(merge, _use);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(merge, ret);

        assert!(DeadPhiElimination.run(&mut program));
        let data = program.func_data(function);
        assert_eq!(data.bb_data(merge).params().len(), 7);
        // The jump into merge must still carry exactly 7 args.
        let mut found = false;
        for block in data.layout().basicblocks() {
            for &inst in block.insts() {
                if let InstKind::Branch(branch) = data.inst_data(inst).kind() {
                    if branch.t_target() == merge && branch.f_target() == merge {
                        assert_eq!(branch.t_args().len(), 7);
                        assert_eq!(branch.f_args().len(), 7);
                        found = true;
                    }
                }
            }
        }
        assert!(found);
    }

    fn removes_an_unreferenced_function() {
        let mut program = Program::new();
        let main = program.new_function(Type::get_unit(), "main".into(), vec![]);
        let a = program.new_function(Type::get_unit(), "a".into(), vec![]);
        let b = program.new_function(Type::get_unit(), "b".into(), vec![]);
        let dead = program.new_function(Type::get_unit(), "dead".into(), vec![]);

        for (func, calls) in [(main, vec![a]), (a, vec![b]), (b, vec![]), (dead, vec![])] {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(func),
            };
            let entry = data.add_entry_block();
            for &callee in &calls {
                let call = data.new_local_value().call(callee, vec![]);
                data.layout_mut().insert_inst(entry, call);
            }
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(entry, ret);
        }

        assert!(DeadFunctionElimination.run(&mut program));
        let layout = program.function_layout();
        assert!(layout.contains(&main));
        assert!(layout.contains(&a));
        assert!(layout.contains(&b));
        assert!(!layout.contains(&dead));
    }

    #[test]
    fn removes_transitively_dead_functions() {
        let mut program = Program::new();
        let main = program.new_function(Type::get_unit(), "main".into(), vec![]);
        let d = program.new_function(Type::get_unit(), "d".into(), vec![]);
        let e = program.new_function(Type::get_unit(), "e".into(), vec![]);

        for (func, calls) in [(main, vec![]), (d, vec![e]), (e, vec![])] {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(func),
            };
            let entry = data.add_entry_block();
            for &callee in &calls {
                let call = data.new_local_value().call(callee, vec![]);
                data.layout_mut().insert_inst(entry, call);
            }
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(entry, ret);
        }

        assert!(DeadFunctionElimination.run(&mut program));
        let layout = program.function_layout();
        assert!(layout.contains(&main));
        assert!(!layout.contains(&d));
        assert!(!layout.contains(&e));
    }

    #[test]
    fn removes_a_self_recursive_function() {
        let mut program = Program::new();
        let main = program.new_function(Type::get_unit(), "main".into(), vec![]);
        let f = program.new_function(Type::get_unit(), "f".into(), vec![]);

        for (func, calls) in [(main, vec![]), (f, vec![f])] {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(func),
            };
            let entry = data.add_entry_block();
            for &callee in &calls {
                let call = data.new_local_value().call(callee, vec![]);
                data.layout_mut().insert_inst(entry, call);
            }
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(entry, ret);
        }

        assert!(DeadFunctionElimination.run(&mut program));
        assert!(!program.function_layout().contains(&f));
    }

    #[test]
    fn preserves_decl_and_main() {
        let mut program = Program::new();
        let main = program.new_function(Type::get_unit(), "main".into(), vec![]);
        let unused_decl = program.new_function(Type::get_unit(), "unused_decl".into(), vec![]);

        // `unused_decl` is a declaration stub (no basic blocks) and is never
        // called: it must be preserved, as must `main`.
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(main),
        };
        let entry = data.add_entry_block();
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        assert!(!DeadFunctionElimination.run(&mut program));
        let layout = program.function_layout();
        assert!(layout.contains(&main));
        assert!(layout.contains(&unused_decl));
    }
}

/// Remove functions that are unreachable from `main` through the call graph
/// (dead functions). Instruction-level DCE keeps whole unused functions,
/// which then drag their internal call sites into the emitted assembly
/// (e.g. an unused crypto helper retaining its rotl/and calls). Reachability
/// is the transitive closure from `main`, so a self-recursive function with
/// no external caller is removed as well, which a plain callsite-count
/// check would misclassify. Declaration stubs (runtime library interfaces)
/// are preserved; `main` is always reachable as the closure root. Removing
/// a function only drops it from the program's function layout: the
/// `FunctionData` stays in the arena because function handles are
/// index-based and arena removal would invalidate every later handle.
pub struct DeadFunctionElimination;

impl Pass for DeadFunctionElimination {
    fn run(&mut self, program: &mut Program) -> bool {
        let call_graph = call_graph::CallGraph::new(program);
        let main = program.get_main_function();
        let mut reachable = HashSet::default();
        let mut queue = vec![main];
        let mut head = 0;
        while head < queue.len() {
            let func = queue[head];
            head += 1;
            if reachable.insert(func) {
                queue.extend(call_graph.callees_in(func));
            }
        }
        let dead = program
            .function_layout()
            .iter()
            .copied()
            .filter(|&func| {
                !reachable.contains(&func) && !program.func_data(func).layout().is_decl()
            })
            .collect::<Vec<_>>();
        if dead.is_empty() {
            return false;
        }
        for func in dead {
            program.remove_function(func);
        }
        true
    }
}

impl Pass for UnreachableBasicBlock {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().entry_bb().is_none() {
            return false;
        }
        let mut changed = false;
        loop {
            let mut id_allocator = IDAllocator::new(1);
            let (g, prece) = cfg::build_cfg_both(data, &mut id_allocator);

            let unreachable_bb = (1..id_allocator.cnt())
                // in-degree is zero.
                .filter(|id| prece[id].is_empty())
                .collect::<Vec<_>>();

            debug!("g:{:?}", g);
            debug!("prece:{:?}", prece);
            debug!("unreachable_bb:{:?}", unreachable_bb);

            if unreachable_bb.is_empty() && id_allocator.cnt() == data.layout().basicblocks().len()
            {
                return changed;
            }

            let mut island = Vec::new();
            for layout in data.layout().basicblocks() {
                if id_allocator.get_id_safe(&layout.bb()).is_none() {
                    // Only remove blocks whose instructions are all unused:
                    // an unreachable block may still feed values into
                    // reachable blocks (e.g. LICM-hoisted GEPs used by a
                    // surviving loop body), and removing it would leave
                    // dangling operands.
                    let all_unused = layout
                        .insts()
                        .iter()
                        .all(|&inst| data.inst_data(inst).used_by().is_empty());
                    if all_unused {
                        island.push(layout.bb());
                    }
                }
            }

            let mut removed_any = false;
            for bb in island {
                data.remove_layout_basicblock(bb);
                removed_any = true;
            }

            for id in unreachable_bb {
                let bb = id_allocator.search_id(id);
                let all_unused = data
                    .layout()
                    .basicblock(bb)
                    .insts()
                    .iter()
                    .all(|&inst| data.inst_data(inst).used_by().is_empty());
                if all_unused {
                    data.remove_layout_basicblock(bb);
                    removed_any = true;
                }
            }
            if removed_any {
                changed = true;
            } else {
                // Unreachable blocks remain but none are removable (their
                // values are still live). Further iterations cannot make
                // progress, so stop.
                return changed;
            }
        }
    }
}

// fn dfs_remove(val: Inst, data: &mut FunctionData, bb: BasicBlock) {
//     let mut remove_list = Vec::new();
//     _dfs_remove(val, data, &mut remove_list);
//     for val in remove_list.into_iter().rev() {
//         eprintln!("remove:{val:?}");
//         data.layout_mut().bb_mut(bb).insts_mut().remove(&val);
//         data.dfg_mut().remove_value(val);
//     }
// }
//
// fn _dfs_remove(val: Inst, data: &FunctionData, remove_list: &mut Vec<Inst>) {
//     let vd = data.dfg().value(val);
//     remove_list.push(val);
//     for &child in vd.used_by().iter() {
//         _dfs_remove(child, data, remove_list);
//     }
// }
//
// #[inline]
// fn is_jump_inst(val: Inst, data: &FunctionData) -> bool {
//     matches!(data.dfg().value(val).kind(), InstKind::Jump(..))
// }

// TODO: This is SimplifyCFG. Please move to a single pass file.
//
// impl Pass for JumpOnlyElimination {
//     fn run_on(&mut self, func: Function, data: &mut FunctionData) {
//         let Some(entry_bb) = data.layout().entry_bb() else {
//             return;
//         };
//         // let virtual_entry_bb = data
//         //     .dfg_mut()
//         //     .new_bb()
//         //     .basic_block(Some("%v_entry".to_string()));
//         // data.layout_mut().bbs_mut().push_key_front(virtual_entry_bb);
//         // let jump = data.dfg_mut().new_value().jump(entry_bb);
//         // data.layout_mut()
//         //     .bb_mut(virtual_entry_bb)
//         //     .insts_mut()
//         //     .push_key_back(jump)
//         //     .unwrap();
//         let worklist = data
//             .layout()
//             .bbs()
//             .iter()
//             .filter(|&(&bb, node)| {
//                 eprintln!("{:?}", data.dfg().bb(bb).name());
//                 let val = *node.insts().front_key().unwrap();
//                 if let InstKind::Jump(jump) = data.dfg().value(val).kind() {
//                     eprintln!("1");
//                     eprintln!("{:?} {:?}", jump.args(), data.dfg().bb(bb).params());
//                     jump.args().is_empty() && data.dfg().bb(bb).params().is_empty()
//                     // jump.args().iter().eq(data.dfg().bb(bb).params())
//                 } else {
//                     false
//                 }
//             })
//             .map(|(&bb, node)| bb)
//             .collect::<Vec<_>>();
//
//         for bb in worklist.into_iter().rev() {
//             eprintln!("{:?}", data.dfg().bb(bb).name());
//             let node = data.layout().bbs().node(&bb).unwrap();
//             let prev_jump_insts = data
//                 .dfg()
//                 .bb(bb)
//                 .used_by()
//                 .iter()
//                 .copied()
//                 .collect::<Vec<_>>();
//
//             let target_bb = if let InstKind::Jump(jump) =
//                 data.dfg().value(*node.insts().front_key().unwrap()).kind()
//             {
//                 jump.target()
//             } else {
//                 unreachable!()
//             };
//
//             for prev_jump_inst in prev_jump_insts {
//                 match data.dfg_mut().value_mut(prev_jump_inst).kind_mut() {
//                     InstKind::Jump(jump) => *jump.target_mut() = target_bb,
//                     InstKind::Branch(branch) => {
//                         if branch.true_bb() == bb {
//                             *branch.true_bb_mut() = target_bb;
//                         } else {
//                             *branch.false_bb_mut() = target_bb;
//                         }
//                     }
//                     _ => unreachable!(),
//                 }
//             }
//
//             // if data.layout().entry_bb().unwrap() != bb {
//             data.layout_mut().bbs_mut().remove(bb);
//             // } else {
//             //     let (key, node) = data.layout_mut().bbs_mut().remove(&target_bb).unwrap();
//             //     data.layout_mut().bbs_mut().remove(&bb);
//             //     data.layout_mut().bbs_mut().push_front(key, node);
//             // }
//         }
//         // data.layout_mut().bbs_mut().pop_front();
//         // for (&bb, data) in data.layout().
//         // data.layout_mut().bbs_mut().pusf
//     }
// }
