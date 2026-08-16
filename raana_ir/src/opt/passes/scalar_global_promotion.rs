//! Scalar global promotion (GSP).
//!
//! A scalar global that is only ever loaded/stored through its own address
//! (never address-taken, never passed to a call) behaves like a local
//! variable as long as no called function can observe it. Promotion threads
//! the global's value through the function in SSA form (load once at entry,
//! stores become defs, one write-back before each return or tail call), so the backend
//! keeps it in registers instead of reloading/re-storing on every access —
//! clang's shape for the `bits`/`pos`/`size` globals in huffman-01.
//!
//! Eligibility is deliberately conservative (`宁漏勿错`):
//!
//! - The global must have scalar type and every use across the whole program
//!   must be a direct `load`/`store` of the global's address (no
//!   `getelemptr`, no escaping into call arguments or returns).
//! - A function may only be rewritten when none of its callees (transitively)
//!   touches the global. Runtime/declared functions never touch user globals
//!   and therefore never block promotion.
//!
//! ---
//!
//! ## 补充说明（中文）
//!
//! 一句话定位：把**不可观察的标量全局**提升为函数内的 SSA 值，让后端把它全程
//! 维护在寄存器里（入口 load 一次、store 就地变成新的 SSA 定义、每个返回 / 尾
//! 调用前写回一次），而不是每次访问都读写内存。典型收益是 huffman-01 的
//! `bits` / `pos` / `size` 三个全局（英文原文所述 clang 的形态）。本 pass 简称
//! GSP。
//!
//! 术语：全局变量、地址逃逸（address-taken）、SSA、块参数（本项目的 Phi）、
//! 写回（write-back）等见 `docs/offline-handbook/glossary.md` 的"IR 与 SSA
//! 基础"分组。
//!
//! ### 变换形态（IR 示例）
//!
//! 对每个满足条件的（函数, 全局）对，`promote` 的流程：先算 def 块（含
//! `store` 的块），在其**支配边界**上插入块参数（即 SSA 的 Phi 放置，算法同
//! `ssa` pass 的支配边界）；再在入口插入初始 `load`；最后按支配树前序
//! `thread` 值流。效果示意：
//!
//! ```text
//! entry:      g0 = load @g           // ① 入口 load 一次：初始 SSA 值
//!             br c, a, b
//! a:          x = load @g            // ② 其余 load @g 替换为当前值（此处 g0）
//!             x' = x + 1
//!             store x', @g           // ③ store 变 def：指令删除，x' 压入值栈
//!             jump join(x')          //    join 是支配边界 → 新增块参数，边上传参
//! b:          jump join(g0)
//! join(v'):   store v', @g           // ④ 每个 return / tail call 之前写回一次
//!             ret
//! ```
//!
//! `thread` 的规则：`load` 替换为栈顶值（延迟到各边参数补齐后统一替换，避免
//! 终结符参数未加长时就重建）、`store` 把值压栈、`Return` / `TailCall` 前用栈
//! 顶值插写回、跳转 / 分支的目标块若有新增参数则补传参。
//!
//! ### 触发 / 放弃条件
//!
//! 三层把关，宁可漏过也不做错（`宁漏勿错`）：
//!
//! - `eligible_globals`（全程序扫描）：全局必须是指向标量的指针
//!   （`ty.is_pointer() && ty.derefernce().is_scalar()`），且全程序每个使用点
//!   都必须是**直接**以全局地址为源 / 为目的的 `Load` / `Store`；任何其它用
//!   法（`getelemptr`、调用实参、`select`、分支条件……）都取消资格——即地址
//!   绝不逃逸进其它指令形态；
//! - `call_analysis`：统计每个函数直接 load / store 的全局，再沿直接调用与
//!   尾调用（`callee_of`）做传递闭包，得到 `may_touch`（函数可能触碰的全局
//!   集合）；声明（无函数体）的运行时函数触碰集为空，永不阻拦；
//! - `promotable_in`：当前函数的（传递可达）被调函数若触碰到该全局 → 放弃；
//! - 函数有入口块（`entry_bb` 存在）且确实用到了该全局
//!   （`function_uses_global`），才进 `promote` 改写。
//!
//! ### 正确性要点
//!
//! - 提升 = 把全局当成函数内局部值维护：入口读一次、SSA 值流逐条对应、出口
//!   写回，与原 load / store 序列语义一致（SSA 值不跨路径串值）；
//! - 关键前提是 **callee 触达分析**：被调函数绝不能观察到该全局。提升期间
//!   全局内存里保留的是入口读入后的旧值（store 已被删除），若 callee 读写它，
//!   调用前后会看到不一致的值；运行时 / 声明函数不碰用户全局，因此不构成
//!   障碍；
//! - 写回插在 `Return` / `TailCall` 终结符**之前**，保证任意出口路径（含尾
//!   调用）都把最新值写回内存；
//! - 块参数只插在支配边界（汇合点），符合 SSA 合法性；`thread` 按支配树前序
//!   推进，保证每个使用点拿到的是支配它的最近定义。
//!
//! ### 管线位置
//!
//! - 注册：`opt/pass.rs` 的 `PassesManager::from_config`，`register_initial`
//!   挂在 **initial（规范化）段**——只跑一轮、不进 fixpoint；顺序在 `inline`
//!   （及其后的 `tco_initial`、`column_major`）**之后**、第一个 fixpoint pass
//!   `ipsccp` **之前**；
//! - 必须在 `inline` 之后：callee 触达分析要看**最终**调用图，内联后再跑才
//!   准确；
//! - 无目标门控、无 config 开关；`-O0` 在 `from_config` 提前返回，本 pass
//!   只在 `-O1` / `-O2` 注册。
//!
//! ### 验证
//!
//! - 本文件 `mod tests`（443 行起）：覆盖尾调用前写回、被调函数触碰全局时
//!   拒绝、`may_touch` 沿传递尾调用传播、对声明（外部）函数尾调用前写回；
//! - 端到端：`make test` 差分比对（harness 默认 `-O0`，要跑到本 pass 需加
//!   `ARGS="-O 2"`）。
//!
use std::collections::{HashMap, HashSet, VecDeque};

use crate::ir::inst_kind;
use crate::opt::prelude::*;

pub struct ScalarGlobalPromotion;

impl Pass for ScalarGlobalPromotion {
    fn run(&mut self, program: &mut Program) -> bool {
        // 主流程三步：全程序候选扫描 → 调用图触碰分析 → 逐函数改写。
        // 与 LICM 的分工（两个 pass 同为 M34 引入）：GSP 把"值只在函数内部
        // 流转（被调函数不可见）、无地址逃逸"的全局彻底变成 SSA 值，函数内
        // 对它的 load/store 只剩入口一次读与出口写回；多函数共享、无法提升的
        // 全局则留给 fixpoint 里的 LICM 外提循环不变访问。
        let may_touch = call_analysis(program);
        let eligible = eligible_globals(program);
        let mut changed = false;
        for func in program.function_layout().to_vec() {
            let mut arena_context = ArenaContextMut {
                program,
                curr_func: Some(func),
            };
            changed |= promote_function(&mut arena_context, &may_touch, &eligible);
        }
        changed
    }
}

/// Program-wide eligibility: the global must be a scalar pointer whose every
/// use in every function is a direct load or store of the global address.
fn eligible_globals(program: &Program) -> HashSet<Inst> {
    // 候选判定第一步（全程序级）：先按类型粗筛——全局声明必须是指向标量的指针
    // （聚合类型如数组 / 结构体按值穿流没有寄存器收益，直接排除），再在下方
    // 循环里对每个使用点做"无地址逃逸"验证；全部通过才算候选。
    let mut eligible: HashMap<Inst, bool> = program
        .global_arena()
        .inst_arena()
        .datas()
        .map(|(inst, data)| {
            let ty = data.ty();
            (*inst, ty.is_pointer() && ty.derefernce().is_scalar())
        })
        .collect();
    for func in program.function_layout() {
        let func_data = program.func_data(*func);
        for layout in func_data.layout().basicblocks() {
            for &inst in layout.insts() {
                let kind = func_data.inst_data(inst).kind();
                // A load/store is fine only when it accesses the global
                // through the global's own address; every other use
                // (getelemptr, call, select, branch condition, ...)
                // disqualifies the global.
                // 中文：这是"无逃逸"判定——合法使用只有两种：`Load` 的源 /
                // `Store` 的目的恰好是该全局。`getelemptr`、调用实参、`select`
                // 等任何其它用法都意味着地址逃逸出"直接访问"形态，一旦发现
                // 就把该全局永久标记为不合格。
                for used in func_data.inst_data(inst).inst_usage() {
                    if let Some(ok) = eligible.get_mut(&used) {
                        let direct = matches!(kind, InstKind::Load(load) if load.src() == used)
                            || matches!(kind, InstKind::Store(store) if store.dest() == used);
                        if !direct {
                            *ok = false;
                        }
                    }
                }
            }
        }
    }
    eligible
        .into_iter()
        .filter(|(_, ok)| *ok)
        .map(|(g, _)| g)
        .collect()
}

/// Function -> set of globals it may load or store, transitively through
/// direct calls and tail calls.
fn call_analysis(program: &Program) -> HashMap<Function, HashSet<Inst>> {
    // 候选判定第二步（调用图侧）：算出每个函数"可能触碰"哪些全局——自己直接
    // load/store 的，并上沿直接调用与尾调用（`callee_of`）的传递闭包。声明
    // （无函数体）的运行时函数没有指令，触碰集恒为空，天然不阻拦提升。
    let mut direct: HashMap<Function, HashSet<Inst>> = HashMap::new();
    for func in program.function_layout() {
        let func_data = program.func_data(*func);
        let mut touched = HashSet::new();
        for layout in func_data.layout().basicblocks() {
            for &inst in layout.insts() {
                match func_data.inst_data(inst).kind() {
                    InstKind::Load(load) => {
                        touched.insert(load.src());
                    }
                    InstKind::Store(store) => {
                        touched.insert(store.dest());
                    }
                    _ => {}
                }
            }
        }
        direct.insert(*func, touched);
    }

    // Transitive closure over direct calls and tail calls.
    // 中文：不动点迭代——把被调函数的触碰集并入调用方，直到任何集合都不再
    // 增长。结果是"传递可达被调函数可能触碰的全局"上界，供 `promotable_in`
    // 直接查表，避免逐调用点递归。
    let mut may_touch = direct.clone();
    let mut changed = true;
    while changed {
        changed = false;
        for func in program.function_layout() {
            let func_data = program.func_data(*func);
            let callees: Vec<Function> = func_data
                .layout()
                .basicblocks()
                .iter()
                .flat_map(|layout| layout.insts().iter())
                .filter_map(|&inst| callee_of(func_data.inst_data(inst).kind()))
                .collect();
            let own: HashSet<Inst> = may_touch[func].clone();
            let mut extra = Vec::new();
            for callee in &callees {
                if let Some(touched) = may_touch.get(callee) {
                    extra.extend(touched.iter().copied().filter(|x| !own.contains(x)));
                }
            }
            if !extra.is_empty() {
                may_touch.get_mut(func).unwrap().extend(extra);
                changed = true;
            }
        }
    }
    may_touch
}

fn callee_of(kind: &InstKind) -> Option<Function> {
    // 直接调用与尾调用统一算作"被调"：触碰分析（`call_analysis`）与 callee
    // 检查（`promotable_in`）共用这一个判定。
    match kind {
        InstKind::Call(call) => Some(call.callee()),
        InstKind::TailCall(call) => Some(call.callee()),
        _ => None,
    }
}

/// Promote every eligible global inside the current function.
// 中文：候选判定第三步（函数级）+ 改写驱动：先确认有入口块（声明函数没有
// 函数体，直接放弃），再对每个候选全局依次过三道闸，全过才进 `promote`。
fn promote_function(
    data: &mut ArenaContextMut<'_>,
    may_touch: &HashMap<Function, HashSet<Inst>>,
    eligible: &HashSet<Inst>,
) -> bool {
    if data.layout().entry_bb().is_none() {
        return false;
    }
    let mut changed = false;
    let globals: Vec<Inst> = data
        .global()
        .inst_arena()
        .datas()
        .map(|(i, _)| *i)
        .collect();
    for global in globals {
        if !eligible.contains(&global) {
            continue; // 闸①：全程序资格（标量指针 + 无地址逃逸）
        }
        if !function_uses_global(data, global) {
            continue; // 闸②：本函数确实 load/store 过它，没有使用就没有提升意义
        }
        if !promotable_in(data, global, may_touch) {
            continue; // 闸③：有（传递可达的）被调函数可能触碰 → 放弃
        }
        changed |= promote(data, global);
    }
    changed
}

fn function_uses_global(data: &ArenaContextMut<'_>, global: Inst) -> bool {
    // 任一基本块里有以该全局为源 / 目的的 Load / Store，就算"本函数用到"。
    data.layout().basicblocks().iter().any(|layout| {
        layout.insts().iter().any(|&inst| {
            matches!(data.inst_data(inst).kind(), InstKind::Load(load) if load.src() == global)
                || matches!(data.inst_data(inst).kind(), InstKind::Store(store) if store.dest() == global)
        })
    })
}

/// The current function may be rewritten only when none of its
/// transitively-reachable callees touches the global.
fn promotable_in(
    data: &ArenaContextMut<'_>,
    global: Inst,
    may_touch: &HashMap<Function, HashSet<Inst>>,
) -> bool {
    // 正确性关键闸门：提升期间全局内存里保留的是入口读入后的旧值（store 已被
    // 吸收），若任一被调函数（含尾调用目标）可能读写它，调用前后会看到不一致
    // 的值，因此必须拒绝。触碰信息直接查 `may_touch` 闭包结果，无需递归。
    for layout in data.layout().basicblocks() {
        for &inst in layout.insts() {
            if let Some(callee) = callee_of(data.inst_data(inst).kind()) {
                if may_touch.get(&callee).is_some_and(|g| g.contains(&global)) {
                    return false;
                }
            }
        }
    }
    true
}

/// Thread `global` through the CFG in SSA form inside the current function.
fn promote(data: &mut ArenaContextMut<'_>, global: Inst) -> bool {
    // 核心改写，四步：① 建 CFG 快照与支配信息；② 在定义块（含 store 的块）的
    // 支配边界上插块参数（Phi 放置）；③ 入口块插入初始 load；④ 按支配树前序
    // 把值流穿遍整个函数，最后统一替换 / 删除 / 插写回。
    let entry_bb = data.layout().entry_bb().unwrap().bb();
    let pointee = data.inst_data(global).ty().derefernce().clone();

    let mut bb_id = utils::IDAllocator::new(1);
    let (graph, prece) = cfg::build_cfg_both(data, &mut bb_id);
    let rpo_path = cfg::rpo_path(&graph);
    let idom_map = dom_tree::idom(&prece, &rpo_path);
    let dom_frontier = dominance_frontier(&bb_id, &prece, &idom_map);
    // CFG / 支配信息是后续步骤共用的快照——Phi 只插在支配边界上、值流按支配
    // 树前序推进，都由这里算出的 idom 与支配边界决定。

    // Blocks that store to the global define a new value.
    // 中文：定义块 = 含有该全局 `store` 的块。SSA 语义下每个 `store` 都是全局
    // 值的一次新定义，Phi 要插在"这些定义的支配边界"上（汇合点）。
    let def_blocks: HashSet<usize> = data
        .layout()
        .basicblocks()
        .iter()
        .filter(|layout| {
            layout.insts().iter().any(|&inst| {
                matches!(data.inst_data(inst).kind(), InstKind::Store(store) if store.dest() == global)
            })
        })
        .map(|layout| bb_id.get_id(&layout.bb()))
        .collect();

    // Insert a block parameter for the global value at every dominance
    // frontier of a defining block.
    // 中文：把全局变成 SSA 值的关键一步——在定义块的支配边界上放块参数（本项目
    // 的 Phi，见术语表）。队列 BFS 处理嵌套情形：新插入的块参数本身也是一次
    // "定义"，它的支配边界还要继续插参数，直到闭包完成。
    let mut param_of_block: HashMap<usize, Inst> = HashMap::new();
    let mut worked: HashSet<usize> = HashSet::new();
    for &def_id in &def_blocks {
        let mut work_queue: VecDeque<usize> = dom_frontier
            .get(&def_id)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        while let Some(front) = work_queue.pop_front() {
            if !worked.insert(front) {
                continue;
            }
            let bb = bb_id.search_id(front);
            let param = data.new_basic_block().add_param(bb, pointee.clone());
            param_of_block.insert(front, param);
            if let Some(sub) = dom_frontier.get(&front) {
                for &s in sub {
                    work_queue.push_back(s);
                }
            }
        }
    }

    // Entry load: the initial value of the global. Built through a builder
    // whose arena can address global instructions (FunctionData's Arena
    // cannot).
    // 中文：入口 load 把"进入函数时的全局内存值"读成初始 SSA 值，作为值流的
    // 起点（等价于给局部变量赋初值）。必须用 `builder`（`LocalBuilder`）构造，
    // 因为普通 `FunctionData` 的 arena 无法新建引用全局指令的指令。
    let entry_load = builder(data).insert_inst(inst_kind::Load::new_data(global, pointee.clone()));
    let entry_first = *data
        .layout()
        .basicblock(entry_bb)
        .insts()
        .get_first()
        .unwrap();
    data.layout_mut()
        .insert_inst_before(entry_first, entry_load);

    // Thread the value through the CFG in dominator-tree pre-order.
    // 中文：按支配树前序 DFS 穿值流（见 `thread`），保证每个使用点拿到的都是
    // 支配它的最近定义。`stack` 初始只有入口 load 值。
    let dom_tree = dom_tree::build_dominance_tree(&idom_map, rpo_path.len());
    let mut stack: Vec<Inst> = vec![entry_load];
    let mut remove_list: Vec<(Inst, BasicBlock)> = Vec::new();
    let mut write_backs: Vec<(Inst, BasicBlock)> = Vec::new();
    // (load, replacement) pairs deferred until after the DFS has extended
    // every jump/branch args list to match the freshly inserted block
    // parameters. Running `visit_and_replace` inline trips on terminators
    // whose args have not yet been lengthened.
    // 中文：load 替换延迟到 DFS 结束、所有跳转 / 分支实参都补齐到新块参数长度
    // 之后统一执行——DFS 中途替换会重建"实参尚未加长"的终结符而触发断言。
    let mut load_replacements: Vec<(Inst, Inst)> = Vec::new();
    thread(
        0,
        &dom_tree,
        &bb_id,
        data,
        global,
        entry_load,
        &param_of_block,
        &mut stack,
        &mut remove_list,
        &mut write_backs,
        &mut load_replacements,
    );

    let mut changed = false;
    // 收尾三件事，顺序有讲究：先把所有 load 的 use 重定向到新值（替换必须在
    // 删除之前，否则残留的 use 会指向已删除指令），再删掉被替换的 load 与被
    // 吸收的 store，最后在每个 `Return` / `TailCall` 终结符之前插入写回 store，
    // 保证任意出口路径（含尾调用）都把最新值写回内存。
    for (load, rep) in load_replacements {
        utils::visit_and_replace(data, load, rep);
        changed = true;
    }
    for (inst, bb) in remove_list {
        data.remove_layout_inst(bb, inst);
        changed = true;
    }
    for (value, bb) in write_backs {
        let store = builder(data).insert_inst(inst_kind::Store::new_data(value, global));
        let terminator = data.layout().basicblock(bb).terminator();
        data.layout_mut().insert_inst_before(terminator, store);
        changed = true;
    }
    // 至此函数内对该全局的普通 load/store 已全部消失（只保留入口一次 load 与
    // 出口写回），后续 LICM 无需再处理它；无法提升的全局则留在内存里，交给
    // LICM 外提循环不变访问（两个 pass 同为 M34 引入）。
    changed
}

/// A local-instruction builder whose arena can also address global
/// instructions.
// 中文：`LocalBuilder` 的 arena 指向整个 `Program`（经 `ArenaContextMut`），
// 因此能新建以全局指令为操作数的 `Load` / `Store`；普通函数内 builder 不行。
fn builder<'a>(data: &'a mut ArenaContextMut<'_>) -> crate::ir::builder::LocalBuilder<'a> {
    crate::ir::builder::LocalBuilder { arena: data }
}

/// Dom-frontier computation (Wikipedia algorithm, as in the SSA pass).
// 中文：经典支配边界算法（与 `ssa` pass 同款）：对每个多前驱块，从每个前驱
// 沿 idom 链向上爬到该块的直接支配者为止，沿途把该块记入各块的支配边界。
fn dominance_frontier(
    id_alloca: &utils::IDAllocator<BasicBlock, usize>,
    prece: &CFGGraph,
    idom_map: &IDomMap,
) -> HashMap<usize, HashSet<usize>> {
    let mut frontier: HashMap<usize, HashSet<usize>> = HashMap::new();
    for bb in 0..id_alloca.cnt() {
        if let Some(preds) = prece.get(&bb) {
            if preds.len() >= 2 {
                for &pre in preds {
                    let mut runner = pre;
                    while runner != idom_map[bb] {
                        frontier.entry(runner).or_default().insert(bb);
                        runner = idom_map[runner];
                    }
                }
            }
        }
    }
    frontier
}

#[allow(clippy::too_many_arguments)]
fn thread(
    node: usize,
    tree: &DomTree,
    bb_id: &utils::IDAllocator<BasicBlock, usize>,
    data: &mut ArenaContextMut<'_>,
    global: Inst,
    entry_load: Inst,
    param_of_block: &HashMap<usize, Inst>,
    stack: &mut Vec<Inst>,
    remove_list: &mut Vec<(Inst, BasicBlock)>,
    write_backs: &mut Vec<(Inst, BasicBlock)>,
    load_replacements: &mut Vec<(Inst, Inst)>,
) {
    // 值流推进核心（支配树前序 DFS）：进入块时若有块参数则压栈；块内 `load`
    // 用栈顶值替换、`store` 把新值压栈、`Return` / `TailCall` 记录写回、跳转 /
    // 分支给带新参数的目标块补传当前值；离开前弹出本块压入的值，兄弟子树之间
    // 栈状态互不污染。`stack[0]` 恒为入口 load 值，作兜底。
    let bb = bb_id.search_id(node);
    let mut pushes = 0;
    if let Some(&param) = param_of_block.get(&node) {
        // 本块是汇合点：块参数就是"进入本块时的当前值"，压栈作为初值。
        stack.push(param);
        pushes += 1;
    }

    let insts: Vec<Inst> = data
        .layout()
        .basicblock(bb)
        .insts()
        .iter()
        .copied()
        .collect();
    for inst in insts {
        if inst == entry_load {
            continue;
        }
        match data.inst_data(inst).kind() {
            InstKind::Load(load) if load.src() == global => {
                // 提升的本质：`load` 不再读内存，直接换成当前 SSA 值（栈顶）。
                let rep = stack.last().copied().unwrap_or(stack[0]);
                // Defer the visit_and_replace: jumping into it now would
                // rebuild terminators whose args have not yet been extended
                // to match the freshly inserted block parameters.
                load_replacements.push((inst, rep));
                remove_list.push((inst, bb));
            }
            InstKind::Store(store) if store.dest() == global => {
                // `store` 就地变成新定义：被存的值成为新的当前值压栈，原 store
                // 指令记入删除列表（不再写内存，出口统一写回）。
                stack.push(store.src());
                pushes += 1;
                remove_list.push((inst, bb));
            }
            InstKind::Return(..) | InstKind::TailCall(..) => {
                // 出口路径：用最新值（栈顶）记录一次写回，插在该终结符之前，
                // 保证返回 / 尾调用后调用方与后续函数看到的是最新值。
                if let Some(&last) = stack.last() {
                    write_backs.push((last, bb));
                }
            }
            InstKind::Jump(jump) => {
                // 目标块若因 Phi 放置新增了块参数，这条边就要补传当前值——实参
                // 向量必须与目标块 `params()` 对齐（见 `docs/Convention.md`）。
                let target = jump.target();
                let mut args = jump.args().to_vec();
                let has_param = param_of_block.contains_key(&bb_id.get_id(&target));
                if has_param {
                    args.push(stack.last().copied().unwrap_or(stack[0]));
                    data.replace_inst_with(inst).jump(target, args);
                }
            }
            InstKind::Branch(branch) => {
                // 条件分支同理，但两臂目标可能不同，需要分别判断、分别补参。
                let cond = branch.cond();
                let t_target = branch.t_target();
                let f_target = branch.f_target();
                let mut t_args = branch.t_args().to_vec();
                let mut f_args = branch.f_args().to_vec();
                let t_has = param_of_block.contains_key(&bb_id.get_id(&t_target));
                let f_has = param_of_block.contains_key(&bb_id.get_id(&f_target));
                if t_has {
                    t_args.push(stack.last().copied().unwrap_or(stack[0]));
                }
                if f_has {
                    f_args.push(stack.last().copied().unwrap_or(stack[0]));
                }
                if t_has || f_has {
                    data.replace_inst_with(inst)
                        .branch(cond, t_target, t_args, f_target, f_args);
                }
            }
            _ => {}
        }
    }

    if let Some(children) = tree.get(node) {
        for &child in children {
            thread(
                child,
                tree,
                bb_id,
                data,
                global,
                entry_load,
                param_of_block,
                stack,
                remove_list,
                write_backs,
                load_replacements,
            );
        }
    }

    for _ in 0..pushes {
        // 离开本块：弹出本块压入的参数与 store 值，恢复进入前的栈状态，
        // 使兄弟子树从相同的值流起点继续。
        stack.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::builder_trait::{GlobalInstBuilder, LocalInstBuilder, ScalarInstBuilder};

    fn scalar_global(program: &mut Program) -> Inst {
        let zero = program.new_value().integer(0);
        program.new_value().global_alloc(zero)
    }

    fn add_unit_function(program: &mut Program, name: &str) -> (Function, BasicBlock) {
        let function = program.new_function(Type::get_unit(), name.into(), vec![]);
        let entry = program.func_data_mut(function).add_entry_block();
        (function, entry)
    }

    fn stores_to(data: &FunctionData, global: Inst) -> Vec<Inst> {
        data.layout()
            .basicblocks()
            .iter()
            .flat_map(|layout| layout.insts().iter().copied())
            .filter(|&inst| {
                matches!(data.inst_data(inst).kind(), InstKind::Store(store) if store.dest() == global)
            })
            .collect()
    }

    #[test]
    fn writes_back_before_tail_call_to_non_touching_callee() {
        let mut program = Program::new();
        let global = scalar_global(&mut program);
        let callee = program.new_function(Type::get_unit(), "callee".into(), vec![]);
        let (caller, entry) = add_unit_function(&mut program, "caller");
        let original_store = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(caller),
            };
            let value = data.new_local_value().integer(42);
            let store = data.new_local_value().store(value, global);
            let tail_call = data.new_local_value().tail_call(callee, vec![]);
            data.layout_mut().insert_inst(entry, store);
            data.layout_mut().insert_inst(entry, tail_call);
            store
        };

        assert!(ScalarGlobalPromotion.run(&mut program));

        let data = program.func_data(caller);
        assert_eq!(data.layout().parent_bb(original_store), None);
        let insts = data
            .layout()
            .basicblock(entry)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let terminator = *insts.last().unwrap();
        assert!(matches!(
            data.inst_data(terminator).kind(),
            InstKind::TailCall(call) if call.callee() == callee
        ));
        let write_back = insts[insts.len() - 2];
        assert!(matches!(
            data.inst_data(write_back).kind(),
            InstKind::Store(store) if store.dest() == global
        ));
    }

    #[test]
    fn rejects_promotion_when_tail_callee_touches_global() {
        let mut program = Program::new();
        let global = scalar_global(&mut program);
        let (callee, callee_entry) = add_unit_function(&mut program, "callee");
        {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(callee),
            };
            let load = data.new_local_value().load(global);
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(callee_entry, load);
            data.layout_mut().insert_inst(callee_entry, ret);
        }
        let (caller, caller_entry) = add_unit_function(&mut program, "caller");
        let original_store = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(caller),
            };
            let value = data.new_local_value().integer(42);
            let store = data.new_local_value().store(value, global);
            let tail_call = data.new_local_value().tail_call(callee, vec![]);
            data.layout_mut().insert_inst(caller_entry, store);
            data.layout_mut().insert_inst(caller_entry, tail_call);
            store
        };

        ScalarGlobalPromotion.run(&mut program);

        assert_eq!(
            program.func_data(caller).layout().parent_bb(original_store),
            Some(caller_entry)
        );
    }

    #[test]
    fn may_touch_follows_transitive_tail_calls() {
        let mut program = Program::new();
        let global = scalar_global(&mut program);
        let (leaf, leaf_entry) = add_unit_function(&mut program, "leaf");
        {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(leaf),
            };
            let load = data.new_local_value().load(global);
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(leaf_entry, load);
            data.layout_mut().insert_inst(leaf_entry, ret);
        }
        let (middle, middle_entry) = add_unit_function(&mut program, "middle");
        {
            let data = program.func_data_mut(middle);
            let tail_call = data.new_local_inst().tail_call(leaf, vec![]);
            data.layout_mut().insert_inst(middle_entry, tail_call);
        }
        let (caller, caller_entry) = add_unit_function(&mut program, "caller");
        let original_store = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(caller),
            };
            let value = data.new_local_value().integer(42);
            let store = data.new_local_value().store(value, global);
            let call = data
                .new_local_value()
                .call_with_type(middle, vec![], Type::get_unit());
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(caller_entry, store);
            data.layout_mut().insert_inst(caller_entry, call);
            data.layout_mut().insert_inst(caller_entry, ret);
            store
        };

        ScalarGlobalPromotion.run(&mut program);

        assert_eq!(
            program.func_data(caller).layout().parent_bb(original_store),
            Some(caller_entry)
        );
    }

    #[test]
    fn writes_back_before_tail_call_to_declaration() {
        let mut program = Program::new();
        let global = scalar_global(&mut program);
        let declaration = program.new_function(Type::get_unit(), "external".into(), vec![]);
        let (caller, entry) = add_unit_function(&mut program, "caller");
        {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(caller),
            };
            let value = data.new_local_value().integer(42);
            let store = data.new_local_value().store(value, global);
            let tail_call = data.new_local_value().tail_call(declaration, vec![]);
            data.layout_mut().insert_inst(entry, store);
            data.layout_mut().insert_inst(entry, tail_call);
        }

        assert!(ScalarGlobalPromotion.run(&mut program));

        let data = program.func_data(caller);
        let stores = stores_to(data, global);
        assert_eq!(stores.len(), 1);
        let insts = data
            .layout()
            .basicblock(entry)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(stores[0], insts[insts.len() - 2]);
        assert!(matches!(
            data.inst_data(*insts.last().unwrap()).kind(),
            InstKind::TailCall(call) if call.callee() == declaration
        ));
    }
}
