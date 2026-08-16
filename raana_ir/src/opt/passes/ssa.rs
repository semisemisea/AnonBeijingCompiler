//! # SSATransform：SSA 构造（内存槽提升，mem2reg 风格）
//!
//! 把函数内基于内存槽的标量局部变量（`Alloc` + `Store`/`Load`）提升为 SSA
//! 值：汇合点插入 block 参数（本项目的 Phi），读写直接折叠进 SSA 数据流，
//! 槽本身从 IR 中删除。这是优化流水线的**第一个** pass——后续 pass
//! （IPSCCP / GVN / LICM / 寄存器分配）都要求"每个值只定义一次、def-use
//! 链直接"的 SSA 形态；局部变量若不提升，就表现为内存读写，任何分析都要
//! 面对"可能别名"的不确定性。术语（SSA、block 参数/Phi、支配、backedge、
//! header/latch/preheader、固定点、归纳变量、trip count、test-at-top 等）：
//! 见 `docs/offline-handbook/glossary.md`，下文不逐一展开。
//!
//! ## 变换形态（IR 示例）
//!
//! 一个用局部变量 `x` 累加的循环，提升前（内存形态）：
//!
//! ```text
//! entry:   %slot = alloc i32            // 局部变量 x 的内存槽
//!          store 0, %slot               // x = 0
//!          jump header
//! header:  br cond, body, exit
//! body:    %t = load %slot              // 读 x
//!          %x1 = add %t, 1
//!          store %x1, %slot             // x = x + 1
//!          jump header
//! exit:    ret
//! ```
//!
//! 提升后（SSA 形态；block 参数即 Phi，不是指令）：
//!
//! ```text
//! entry:    jump header(0)              // 初值 store 折叠进参数
//! header(x): br cond, body(x), exit(x)  // 汇合点按参数合并各版本
//! body(x):  %x1 = add x, 1
//!           jump header(%x1)            // backedge 携带新版本
//! exit(x):  ret
//! ```
//!
//! `add_param` 在基本块上挂参数，`Jump`/`Branch` 的实参按目标块 `params()`
//! 顺序补齐（见 `dfs` 对终结符的处理）——实参列表必须与目标参数切片一一对应。
//!
//! 算法骨架（`run_on` 内顺序）：`cfg::build_cfg_both` 建 CFG 与反图 →
//! `cfg::rpo_path` 求 RPO（入口块编号必须为 0）→ `dom_tree::idom` 求立即
//! 支配者 → `dom_tree::build_dominance_tree` 建支配树 → `dominance_analysis`
//! 求支配边界 → `variable_analysis` 收集可提升槽 → 工作队列插参数 →
//! `dfs` 沿支配树重命名并改写指令。
//!
//! ## 触发 / 放弃条件
//!
//! - 触发：任何**有函数体**的函数（`run_on` 开头 `entry_bb().is_none()` 判定
//!   为函数声明，直接返回）；无 config 开关、无目标门控——O1/O2 无条件挂载
//!   （O0 在 `PassesManager::from_config` 早退，流水线为空）。
//! - 候选槽判定在 `variable_analysis`：`Alloc` 的槽类型必须是标量或指针
//!   （`ty.is_scalar() || ty.is_pointer()`），且地址不逃逸
//!   （`alloca_does_not_escape`：槽的所有使用者只能是 `Store`（dest == 该槽）
//!   或 `Load`）。`FUNC_ARG_OPT_ENABLE`（当前 `false`）打开时跳过前 N 个槽
//!   （对应函数形参的槽），属历史遗留开关。
//! - 放弃：聚合类型槽（数组等）、地址逃逸的槽（传给 `Call`、存入其它内存、
//!   `GetElemPtr` 取址、返回地址）保留在内存——地址一旦逃逸就可能被别名
//!   访问，提升会破坏语义。
//!
//! ## 正确性
//!
//! - **Phi 放置**：`dominance_analysis` 对入边 ≥ 2 的块沿前驱链走到 idom
//!   （Wikipedia 算法）求支配边界；插参数时再用工作队列迭代到**迭代支配
//!   边界（IDF）**——每在一个块插了参数，就继续向该块的支配边界传播，
//!   `worked` 去重保证每个 (vid, 块) 至多插一个参数。这正是"Phi 必须位于
//!   各定义块支配边界"的标准论证；
//! - **重命名**：`dfs` 沿支配树深度优先（显式栈 + Enter/Exit 事件），
//!   `ValStack` 维护每个 vid 沿当前路径的"最新版本"：进块先压入 block 参数
//!   （`insert_table` 记录 (vid, 参数下标)），`Store` 把源值压栈，`Load` 用
//!   栈顶值替换（`utils::visit_and_replace`），出块弹栈——支配关系保证任意
//!   使用点看到的栈顶就是唯一到达它的定义，不同路径互不串值；
//! - **未初始化读**：`Load` 时栈为空（该路径上尚无 `Store`）用 `undef`
//!   替换；`Jump`/`Branch` 补参数时栈为空同样补 `undef`（类型取目标参数
//!   类型）——对应 SysY 未初始化局部变量的未定义行为，同时保证每条入边都
//!   为目标参数提供实参、参数个数一致；
//! - **逃逸判定**：`alloca_does_not_escape` 是提升安全性的核心；指针类型槽
//!   同样可提升（测试 `promotes_pointer_slot_alloca`：GEP base 变为函数参数）；
//! - **清理**：被替换/删除的指令记入 `remove_list`，逆序
//!   `remove_layout_inst` 删除，`run` 最后对整程序跑 `DeadCodeElimination`
//!   清掉悬空指令（含生成的 `undef`）。
//!
//! ## 管线位置与门控
//!
//! - 注册：`opt/pass.rs` 的 `PassesManager::from_config`，
//!   `register_initial(ssa::SSATransform)`——initial 段（一次性，非固定点）
//!   的第一个 pass，在 `specialize` / `inline` / `tco` / `column_major` /
//!   `scalar_global_promotion` 之前，之后才是固定点段（IPSCCP、SimplifyCFG、
//!   LoopUnroll、…）。
//! - 为什么必须最先跑：`inline` 等 pass 会生成新代码，而所有依赖 def-use /
//!   block 参数的 pass 都假设 IR 已是 SSA 形态；内存形态下 IPSCCP / GVN 无法
//!   跟踪局部变量。
//! - 门控：无 config 开关、无 `TargetPolicy` 门控（AArch64 / RISC-V 都跑）；
//!   仅 `-O0` 例外（`from_config` 直接返回空 manager）。每个函数 `run_on`
//!   开头先跑 `dce::UnreachableBasicBlock` 清不可达块，整程序跑完再跑
//!   `DeadCodeElimination`。
//!
//! ## 验证
//!
//! - 本文件 `mod tests`：`promotes_pointer_slot_alloca` 构造"指针槽 + 逃逸
//!   判定通过"场景，断言提升后 `Alloc`/`Load` 计数归零、仅剩最终 `Store`、
//!   GEP base 变为函数参数；`count_kind` 辅助统计指令种类；
//! - 单测：`cargo test -p raana_ir`；端到端差分：`make test`
//!   （优化路径务必 `ARGS="-O 2"`）。

use crate::opt::prelude::*;

pub struct SSATransform;

const FUNC_ARG_OPT_ENABLE: bool = false;

// not all basic block has it's frontier so we can use HashMap instead of Vec
type Frontier = HashMap<BId, HashSet<BId>>;

type ValUsage = Vec<Vec<VId>>;

// variable(vid) is insert as basic block(bbid) at index(usize)
type Index = usize;
type InsertTable = Vec<Vec<(VId, Index)>>;

// Recording each variable version while doing SSA elimination.
type ValStack = Vec<Vec<Inst>>;

impl Pass for SSATransform {
    fn run(&mut self, program: &mut crate::ir::Program) -> bool {
        let func_layout = program.function_layout().to_vec();
        let mut arena_context = ArenaContextMut {
            program,
            curr_func: None,
        };
        let mut changed = false;
        for func in func_layout {
            arena_context.curr_func = Some(func);
            changed |= self.run_on(&mut arena_context);
        }
        let mut dce = super::dce::DeadCodeElimination;
        changed |= dce.run(program);
        changed
    }

    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        // function declaration. skip.
        if data.layout().entry_bb().is_none() {
            return false;
        }

        let mut ubb = super::dce::UnreachableBasicBlock;
        let mut changed = ubb.run_on(data);

        debug!("----------------------------------");
        debug!("function: {:?}", data.curr_func.unwrap());
        debug!("name: {}", data.name());

        // Discretization. Assign each unique basic block with natural number 0..n
        let mut bb_id = IDAllocator::new(1);

        debug!("showing");
        // get graph and reverse graph
        let (graph, prece) = cfg::build_cfg_both(data, &mut bb_id);
        debug!("graph: {graph:?}");
        debug!("prece: {prece:?}");

        // entry_bb must get 0 for id
        assert!(bb_id.get_id(&data.layout().entry_bb().unwrap().bb()) == 0);

        let rpo_path = cfg::rpo_path(&graph);
        // start from entry_bb so first element of RPO is zero
        assert!(rpo_path[0] == 0);
        debug!("rpo_path: {rpo_path:?}");

        // get immediate dominator of each block
        // dominance is a partial order.
        // immediate dominance means partial order coverage
        let idom_map = dom_tree::idom(&prece, &rpo_path);
        debug!("idom_map: {idom_map:?}");

        // for dominance, its hasse diagram is a tree
        let donimnace_tree = dom_tree::build_dominance_tree(&idom_map, rpo_path.len());
        debug!("dominance_tree: {donimnace_tree:?}");

        // then we can do frontier analysis
        let dom_frontier = dominance_analysis(&bb_id, &prece, &idom_map);
        debug!("dominance_frontier: {dom_frontier:?}");

        // find out where are varaibles defined.
        let mut val_id = IDAllocator::new(1);
        let val_usage = variable_analysis(&mut val_id, &mut bb_id, data);
        debug!("val_usage: {val_usage:?}");

        // variable(vid) is insert as basic block(bbid) at index(usize)
        let mut insert_table = vec![vec![]; bb_id.cnt()];

        let mut worked = vec![HashSet::default(); bb_id.cnt()];

        for (vid, frontiers) in val_usage.iter().enumerate().flat_map(|(vid, def_bbs)| {
            def_bbs
                .iter()
                .filter_map(|def_bb| dom_frontier.get(def_bb))
                .map(move |frontier| (vid, frontier))
        }) {
            // let mut worked = frontiers.clone();
            let mut work_queue = VecDeque::with_capacity(frontiers.len());
            for &front in frontiers.iter() {
                work_queue.push_back(front);
            }

            while !work_queue.is_empty() {
                let front = work_queue.pop_front().unwrap();
                if worked[front].contains(&vid) {
                    continue;
                }
                worked[front].insert(vid);
                let bb = bb_id.search_id(front);
                let index = data.bb_data(bb).params().len();

                let var_ty = utils::alloc_ty(val_id.search_id(vid as _), data).clone();

                let p = data.new_basic_block().add_param(bb, var_ty);
                changed = true;
                insert_table[front].push((vid, index));
                data.inst_data_mut(p).set_name(format!("vid_{}", vid));

                if let Some(sub_frontiers) = dom_frontier.get(&front) {
                    for &sub_front in sub_frontiers.iter() {
                        if !worked[sub_front].contains(&vid) {
                            work_queue.push_back(sub_front);
                        }
                    }
                }
            }
        }

        let mut val_stack = vec![vec![]; val_id.cnt()];
        let mut remove_list = Vec::new();

        dfs(
            0,
            &donimnace_tree,
            &mut val_stack,
            &val_id,
            &bb_id,
            data,
            &insert_table,
            &mut remove_list,
        );

        changed |= !remove_list.is_empty();
        remove_list.into_iter().rev().for_each(|(inst, bb)| {
            data.remove_layout_inst(bb, inst);
        });

        debug!("");
        debug!("----------------------------------");
        debug!("");
        changed
    }
}

#[allow(clippy::too_many_arguments)]
fn dfs(
    entry: BId,
    tree: &DomTree,
    st: &mut ValStack,
    val_id: &IDAllocator<Inst, VId>,
    bb_id: &IDAllocator<BasicBlock, BId>,
    data: &mut ArenaContextMut<'_>,
    insert_table: &InsertTable,
    remove_list: &mut Vec<(Inst, BasicBlock)>,
) {
    enum Visit {
        Enter(BId),
        Exit(Vec<VId>),
    }

    let mut visits = vec![Visit::Enter(entry)];
    while let Some(visit) = visits.pop() {
        let node = match visit {
            Visit::Enter(node) => node,
            Visit::Exit(history) => {
                for id in history {
                    st[id].pop();
                }
                continue;
            }
        };

        let mut history = Vec::new();
        // Step 1: Update `st` if block arguments update the value.
        let bb = bb_id.search_id(node);
        let bb_data = data.bb_data(bb);
        for &(vid, idx) in &insert_table[node] {
            st[vid].push(bb_data.params()[idx]);
            history.push(vid);
        }

        // Step 2: Traverse the instruction list and find `alloc`, `store` and `load`.
        let values = data
            .layout()
            .basicblock(bb)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for val in values {
            let val_data = data.inst_data(val);
            let ty = val_data.ty().clone();
            match val_data.kind() {
                InstKind::Alloc => {
                    if val_id.get_id_safe(&val).is_some() {
                        remove_list.push((val, bb));
                    }
                }
                InstKind::Store(store) => {
                    if let Some(&dest_id) = val_id.get_id_safe(&store.dest()) {
                        st[dest_id].push(store.src());
                        history.push(dest_id);
                        remove_list.push((val, bb));
                    }
                }
                InstKind::Load(load) => {
                    if let Some(&load_id) = val_id.get_id_safe(&load.src()) {
                        let rep_with = st[load_id]
                            .last()
                            .copied()
                            .unwrap_or_else(|| data.new_local_inst().undef(ty));
                        utils::visit_and_replace(data, val, rep_with);
                        remove_list.push((val, bb));
                    }
                }
                InstKind::Jump(jump) => {
                    let target = jump.target();
                    let target_id = bb_id.get_id(&target);
                    let mut args = jump.args().to_vec();
                    for (i, &(vid, _)) in (args.len()..).zip(&insert_table[target_id]) {
                        let item = match st[vid].last() {
                            Some(&val) => val,
                            None => {
                                let value = data.bb_data(target).params()[i];
                                let ty = data.inst_data(value).ty().clone();
                                data.new_local_inst().undef(ty)
                            }
                        };
                        args.push(item);
                    }
                    data.replace_inst_with(val).jump(target, args);
                }
                InstKind::Branch(branch) => {
                    let cond = branch.cond();
                    let t_target = branch.t_target();
                    let t_target_id = bb_id.get_id(&t_target);
                    let f_target = branch.f_target();
                    let f_target_id = bb_id.get_id(&f_target);
                    let mut f_args = branch.f_args().to_vec();
                    let mut t_args = branch.t_args().to_vec();
                    for (i, &(vid, _)) in (f_args.len()..).zip(&insert_table[f_target_id]) {
                        let item = match st[vid].last() {
                            Some(&val) => val,
                            None => {
                                let value = data.bb_data(f_target).params()[i];
                                let ty = data.inst_data(value).ty().clone();
                                data.new_local_inst().undef(ty)
                            }
                        };
                        f_args.push(item);
                    }
                    for (i, &(vid, _)) in (t_args.len()..).zip(&insert_table[t_target_id]) {
                        let item = match st[vid].last() {
                            Some(&val) => val,
                            None => {
                                let value = data.bb_data(t_target).params()[i];
                                let ty = data.inst_data(value).ty().clone();
                                data.new_local_inst().undef(ty)
                            }
                        };
                        t_args.push(item);
                    }
                    data.replace_inst_with(val)
                        .branch(cond, t_target, t_args, f_target, f_args);
                }
                _ => {}
            }
        }

        visits.push(Visit::Exit(history));
        for &child in tree[node].iter().rev() {
            visits.push(Visit::Enter(child));
        }
    }
}

/// An alloca is promotable only if it holds a single-machine-word value (scalar
/// or pointer) and its address never escapes: every use is either a `Store` that
/// writes *to* the slot or a `Load` that reads *from* it. Using the address in
/// any other way (passing it to a call, storing it into memory, address
/// arithmetic, returning it) forces the slot to stay in memory.
fn alloca_does_not_escape(val: Inst, data: &FunctionData) -> bool {
    data.inst_data(val)
        .used_by()
        .iter()
        .copied()
        .all(|user| match data.inst_data(user).kind() {
            InstKind::Store(store) => store.dest() == val,
            InstKind::Load(_) => true,
            _ => false,
        })
}

pub fn variable_analysis(
    val_id: &mut IDAllocator<Inst, VId>,
    bb_id: &mut IDAllocator<BasicBlock, BId>,
    data: &FunctionData,
) -> ValUsage {
    let mut skip_func_para = if FUNC_ARG_OPT_ENABLE {
        data.params().len()
    } else {
        0
    };
    let mut val_usage = ValUsage::new();

    // use iterator to get rid of nested for-loop
    // you don't have to care what does the iterator chain do.
    // only to know it return these things in tuple:
    //
    //  value handle     kind        which basic block it belongs to.
    for (val, val_kind, bb) in data
        .layout()
        .basicblocks()
        .iter()
        .flat_map(|layout| layout.insts().iter().zip(std::iter::repeat(layout.bb())))
        .map(|(&val, bb)| (val, data.inst_data(val).kind(), bb))
    {
        match val_kind {
            InstKind::Alloc => {
                if skip_func_para > 0 {
                    skip_func_para -= 1;
                } else {
                    let ty = utils::alloc_ty(val, data);
                    // Single-machine-word slot types (scalar or pointer) are
                    // promotable when the address does not escape.
                    if (ty.is_scalar() || ty.is_pointer()) && alloca_does_not_escape(val, data) {
                        val_id.check_or_alloc_id_same(val);
                        val_usage.push(Vec::new());
                    }
                }
            }
            InstKind::Store(store) => {
                if let Some(&vid) = val_id.get_id_safe(&store.dest()) {
                    let bbid = bb_id.get_id(&bb);
                    val_usage[vid].push(bbid);
                }
            }
            _ => {}
        }
    }

    val_usage
}

pub fn dominance_analysis(
    id_alloca: &IDAllocator<BasicBlock, BId>,
    prece: &CFGGraph,
    idom_map: &IDomMap,
) -> Frontier {
    let mut dominance_frontier = Frontier::default();

    // algorithm I looked up from wikipedia.
    for bb in 0..id_alloca.cnt() {
        if prece[&bb].len() >= 2 {
            for &pre in prece[&bb].iter() {
                let mut runner = pre;
                while runner != idom_map[bb] {
                    dominance_frontier.entry(runner).or_default().insert(bb);
                    runner = idom_map[runner];
                }
            }
        }
    }

    dominance_frontier
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(data: &mut ArenaContextMut<'_>) -> bool {
        SSATransform.run_on(data)
    }

    fn count_kind(data: &FunctionData, pred: impl Fn(&InstKind) -> bool) -> usize {
        data.layout()
            .basicblocks()
            .iter()
            .flat_map(|layout| layout.insts())
            .filter(|&&inst| pred(data.inst_data(inst).kind()))
            .count()
    }

    /// A pointer-typed alloca whose address is only used by `Store`/`Load` must
    /// be promoted: after the pass the slot is gone and the loaded pointer is
    /// replaced by the stored value directly.
    #[test]
    fn promotes_pointer_slot_alloca() {
        let mut program = Program::new();
        let ptr_ty = Type::get_pointer(Type::get_array(Type::get_i32(), 1024));
        let func =
            program.new_function(Type::get_unit(), "promote".to_owned(), vec![ptr_ty.clone()]);

        let _ = (|| {
            let data = program.func_data_mut(func);
            let entry = data
                .new_basic_block()
                .basic_block("entry".to_owned(), vec![ptr_ty.clone()]);
            let mid = data.new_basic_block().basic_block("mid".to_owned(), vec![]);
            data.layout_mut().push_bb_back(entry);
            data.layout_mut().push_bb_back(mid);

            let slot = data.new_local_inst().alloc(ptr_ty.clone());
            let param = data.bb_data(entry).params()[0];
            data.set_params(vec![param]);
            let store1 = data.new_local_inst().store(param, slot);
            let jump = data.new_local_inst().jump(mid, vec![]);
            data.layout_mut().insert_inst(entry, slot);
            data.layout_mut().insert_inst(entry, store1);
            data.layout_mut().insert_inst(entry, jump);

            let loaded = data.new_local_inst().load(slot);
            let zero = data.new_local_inst().integer(0);
            let elem = data.new_local_inst().get_elem_ptr(loaded, vec![zero]);
            let store2 = data.new_local_inst().store(zero, elem);
            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(mid, loaded);
            data.layout_mut().insert_inst(mid, zero);
            data.layout_mut().insert_inst(mid, elem);
            data.layout_mut().insert_inst(mid, store2);
            data.layout_mut().insert_inst(mid, ret);
        })();

        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(func),
        };
        assert!(run(&mut data));

        let data = data.curr_func_data();
        assert_eq!(
            count_kind(data, |k| matches!(k, InstKind::Alloc)),
            0,
            "slot should be promoted"
        );
        assert_eq!(
            count_kind(data, |k| matches!(k, InstKind::Store(..))),
            1,
            "only the final element store remains"
        );
        assert_eq!(
            count_kind(data, |k| matches!(k, InstKind::Load(..))),
            0,
            "slot load should be replaced"
        );

        // The GEP base must now be the function parameter, not a load of the slot.
        let gep = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|layout| layout.insts())
            .find(|&&inst| matches!(data.inst_data(inst).kind(), InstKind::GetElemPtr(..)))
            .copied()
            .unwrap();
        let InstKind::GetElemPtr(gep_data) = data.inst_data(gep).kind() else {
            unreachable!()
        };
        let param = data.params()[0];
        assert_eq!(
            gep_data.base(),
            param,
            "GEP base should be the promoted param"
        );
    }
}

// TEMP DEBUG
