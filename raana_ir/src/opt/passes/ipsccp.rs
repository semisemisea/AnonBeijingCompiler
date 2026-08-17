//! Implementation of *Interprocedural Sparse Condition Constant Propagation*
//!
//! ---
//!
//! # IPSCCP：过程间稀疏条件常量传播
//!
//! 在**整个程序**（而非单个函数）上做稀疏条件常量传播：沿调用边把常量实参
//! 传入被调函数的形参、沿返回边把常量返回值传回调用点，并据此折叠条件已知
//! 的分支。动机：函数内 SCCP（`opt/passes/const_prop.rs`，未注册、已被本模块
//! 取代）在 `Call` 处直接降 Bottom，跨函数边界的常量信息完全丢失；本 pass 用
//! ICFG + 内存摘要把传播打通到过程间。术语：SSA / block 参数（Phi）、ICFG、
//! 格 Top-Bottom、固定点、支配等见 `docs/offline-handbook/glossary.md`，不在此
//! 展开。
//!
//! ## 核心机制
//!
//! - **三值格**：`Lattice` = Top（未知，默认，最乐观）→ `Constant(i32)` →
//!   Bottom（非常量，最保守），只降不升；`merge` 是格 meet，`update` 返回是否
//!   变化。初始化：`InstKind::Integer` → Constant，`Float` → Bottom（`new_var`），
//!   其余缺席即 Top；零初始化全局先登记为零区间。
//! - **ICFG 边**（`opt/analysis_passes/icfg.rs` 的 `EdgeType`）：`Normal` /
//!   `Call` / `Return` / `CallToReturn` 四类。`Call` 边把调用点实参的格值写入
//!   被调函数形参（`Node::new(callee, param)`）；`Return` 边把返回值格值写回
//!   调用点的 `.call` 节点（`call_site_before` 解析）；一条虚设的 `start_edge`
//!   （`src == dst` 的 Normal 自环）触发 main 入口。
//! - **双工作队列**：`edge_worklist` 存边，目标节点首次到达时入队（`node_visited`
//!   去重），并记录所属块已访问（`block_visited`）；`node_worklist` 按指令种类
//!   求格值，变化时沿 `used_by` 扩散（`extend_affected_node_used_by`，只进入已
//!   访问块的指令）并重推出边——没被边到达的代码不参与，构成"稀疏"。
//! - **节点求值**：`Binary` 双操作数都是 Constant 才折叠
//!   （`mathematic_operation`，wrapping 语义）；`Select` 条件已知选一臂、Bottom
//!   时两臂 meet；`Cast` 仅 i32 目标且源是常量 `Float` 时可折叠（`fold_f32_to_i32`）；
//!   `GetElemPtr` / `Alloc`（地址不是 i32 常量）→ Bottom；vector 类指令 → Top；
//!   `Branch` 条件为 Constant 只推可达臂，Top 两臂都不推（保守，等后续迭代
//!   暴露），Bottom 两臂都推。
//! - **调用边传播**：`Call` / `TailCall` 的实参格值 zip 进形参（`is_decl` 的
//!   外部函数降 Bottom）。尾调用必须同样传播：否则递归尾调用间变化的实参会
//!   被误传播成常量（测试 `tail_call_args_prevent_constant_mispropagation`）。
//! - **返回值中继**：`Return` 沿出边把 `ret` 值写回调用点；尾调用是中继节点
//!   （故意不在 `callsite_by_continuation` 里），返回值先写入它，再靠
//!   `relay_targets` 重调度，让其 `TailCall` 臂沿 Return 边继续向上转发。
//! - **内存摘要**：`MemState` 模拟常量偏移内存。`CellKey` =（局部/全局对象 +
//!   字节偏移），`RootKey` = 整个对象；每 cell 按写者存格值，折叠值 = 写者的
//!   meet；`MemZero` 与零初始化全局用零区间回答 load。`EffectAnalysis`
//!   （`opt/analysis_passes/effects.rs`）提供基址/偏移解析（`BaseEnv` 的
//!   `constant_offset` / `base_of`）、不可解析地址的 points-to 目标
//!   （`targets_of`）与被调函数写集（`call_write_roots`）。
//! - **调用失效**：`invalidate_call` 按 `call_write_roots` 清掉被调函数可达的
//!   根并重调度受影响的 load；写集未知时清全部，并把根加入 `cleared_roots`
//!   ——未知写可能落在任意 store 与 load 之间，此后该根不再折叠 load。load 用
//!   `insert_or_replace`（快照语义）而非 merge，避免新旧快照 meet 错误塌缩成
//!   Bottom。
//!
//! ## 变换形态（IR 示例）
//!
//! ```text
//! // 传播前
//! main:   a = 10
//!         b = call inc(a)          // Call 边：实参 a 的格值 10 流入 inc 的形参 p
//!         br b != 0, L1, L2        // b 是 inc 的返回值
//! inc(p): q = p + 1                // p 格 = Constant(10)，q 折叠为 Constant(11)
//!         ret q                    // Return 边：11 写回调用点 → b 格 = Constant(11)
//!
//! // 传播后（常量替换 + 分支折叠）
//! main:   b = 11                   // 调用点被替换为立即数，inc 不再被调用
//!         jump L1                  // 11 != 0 恒真：br 折叠为无条件 jump
//! ```
//!
//! ## 正确性要点
//!
//! - 格值只降不升、工作队列覆盖所有可达节点，有限格上必然收敛到固定点；
//! - 只有格值为 `Constant` 的节点才被替换，Bottom 绝不替换；
//! - 替换在传播收敛后一次性进行：有使用者的 `Call` / `TailCall` /
//!   `BlockArgRef` 用 `visit_and_replace` 换成新建的立即数指令，其余用
//!   `replace_inst_with` 后脱离布局（`detach_layout_inst`）；
//! - 分支折叠只发生在条件被传播成 `InstKind::Integer` 的终结符上；块删除要求
//!   非入口且块内**每条指令**都无使用（防止删掉仍被可达块引用的 LICM 外提值）；
//! - 常量 `Div` / `Rem` 折叠断言除数非 0；`fold_f32_to_i32` 只折叠可表示的
//!   有限值（截断向零），饱和 `as` 转换与目标指令不符，其余保持运行时转换。
//!
//! ## 管线位置与门控
//!
//! - 注册：`opt/pass.rs` 的 `PassesManager::from_config`——initial 段（SSA →
//!   Specialize → [MulmodRecognize / RecursiveMemoize，仅 AArch64] → Inline →
//!   TCO → ColumnMajor → ScalarGlobalPromotion）之后，**fixpoint 段首位**
//!   （第一个 `register`，即 `fixed_point_start` 处），其后是 `simplify_cfg`、
//!   `loop_unroll`（`--loop-unroll` 门控）等。常量信息是后续 pass 的前提，故
//!   排在 fixpoint 最前；
//! - 无独立 config 开关、无目标门控（AArch64 / RISC-V 都跑）；`-O0` 时
//!   `from_config` 直接返回空管线，本 pass 不挂载；
//! - fixpoint 循环（`run_passes`）最多迭代 `MAX_PIPELINE_ITERATIONS`（100）轮
//!   直到整段无变化。
//!
//! ## 验证
//!
//! - 本文件 `#[cfg(test)] mod tests` 位于 `passes/ipsccp/tests.rs`（533 行）：
//!   f32→i32 折叠边界、尾调用实参防误传播、常量形参替换、全局 cell 的
//!   store/load 往返、零初始化全局 load、`MemZero`、调用写者失效、确定性写者
//!   折叠、不同偏移隔离、写者分歧 merge 为 Bottom 等；
//! - 端到端：`make test` 差分比对；静态指令数回归用 `scripts/perf_compare.sh`。
//!
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ir::inst_kind::mem_zero::MemZeroLen;
use crate::opt::{
    analysis_passes::{
        dom_tree::v2::DominanceTree,
        effects::{EffectAnalysis, WriteRoot},
        icfg::{Edge, EdgeType, ICFG},
        memory::{BaseEnv, MemObject},
    },
    prelude::*,
    utils::{cfg::CFG, visit_and_replace},
};

pub struct IPSCCP;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Lattice {
    #[default]
    Top,
    Constant(i32),
    Bottom,
}

impl Lattice {
    fn merge(self, new: Lattice) -> Lattice {
        match (self, new) {
            (Lattice::Top, v) => v,
            (Lattice::Constant(lhs), rhs) => match rhs {
                Lattice::Top => self,
                Lattice::Constant(rhs) if lhs == rhs => self,
                _ => Lattice::Bottom,
            },
            (Lattice::Bottom, _) => Lattice::Bottom,
        }
    }

    fn update(&mut self, new: Lattice) -> bool {
        match (*self, new) {
            (Lattice::Top, _) if new != Lattice::Top => {
                *self = new;
                true
            }
            (Lattice::Constant(old), Lattice::Constant(new)) if old != new => {
                *self = Lattice::Bottom;
                true
            }
            (Lattice::Constant(..), Lattice::Bottom) => {
                *self = Lattice::Bottom;
                true
            }
            _ => false,
        }
    }
}

#[derive(Debug, Default)]
struct LatticeMap(FxHashMap<Node, Lattice>);

impl LatticeMap {
    fn new_var(&mut self, node: Node) {
        self.0.insert(node, Lattice::Bottom);
    }

    fn new_const(&mut self, node: Node, val: i32) {
        self.0.insert(node, Lattice::Constant(val));
    }

    fn get(&self, node: Node) -> Lattice {
        *self.0.get(&node).unwrap_or(&Lattice::Top)
    }

    // Return whether the original status is changed.
    fn insert_or_update(&mut self, node: Node, status: Lattice) -> bool {
        match self.0.entry(node) {
            std::collections::hash_map::Entry::Occupied(mut e) => e.get_mut().update(status),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(status);
                status != Lattice::Top
            }
        }
    }

    /// Overwrite the lattice unconditionally, returning whether it changed.
    /// Loads use this: their value is a snapshot of the simulated memory at
    /// the time of (re)scheduling, and merging an outdated snapshot with the
    /// current one (e.g. 0 read before a store, 6 after) would collapse to
    /// Bottom instead of refining to the latest value.
    fn insert_or_replace(&mut self, node: Node, status: Lattice) -> bool {
        match self.0.entry(node) {
            std::collections::hash_map::Entry::Occupied(mut e) if *e.get() != status => {
                e.insert(status);
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(status);
                status != Lattice::Top
            }
        }
    }
}
type EdgeSet = FxHashSet<Edge>;
type NodeSet = FxHashSet<Node>;

/// A constant-offset memory cell: a local stack object or a global object
/// at a byte offset. Only i32-sized accesses are modeled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CellKey {
    Local(Function, Inst, i64),
    Global(Inst, i64),
}

/// A memory root: an entire local stack object or global object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RootKey {
    Local(Function, Inst),
    Global(Inst),
}

impl CellKey {
    fn offset(self) -> i64 {
        match self {
            CellKey::Local(_, _, off) | CellKey::Global(_, off) => off,
        }
    }

    fn root(self) -> RootKey {
        match self {
            CellKey::Local(func, inst, _) => RootKey::Local(func, inst),
            CellKey::Global(inst, _) => RootKey::Global(inst),
        }
    }
}

/// Simulation of the "main memory" for constant-offset accesses (see
/// `docs/memory_alias_analysis.md` §5.3). Per cell we keep the per-writer
/// contributions so a store whose source refines Top -> Constant over the
/// worklist can recover; the folded value is the lattice meet of the
/// writers. Zero ranges model `MemZero` and zero-initialized globals.
#[derive(Default)]
struct MemState {
    /// cell -> (writer 函数, writer store 指令) -> contribution lattice.
    /// writer 必须带函数：`Inst` 的 id 是**每函数独立 arena** 分配的
    /// （`ir/instruction.rs`，本地从 1 起），跨函数会撞 id——裸 `Inst`
    /// 不足以定位一条 store。
    cells: FxHashMap<CellKey, FxHashMap<(Function, Inst), Lattice>>,
    /// root -> merged zero byte intervals [from, to).
    zero: FxHashMap<RootKey, Vec<(i64, i64)>>,
    /// root -> cells currently tracked (for MemZero range removal).
    root_cells: FxHashMap<RootKey, Vec<CellKey>>,
    /// root -> loads that read it (re-scheduled on any root change).
    root_loaders: FxHashMap<RootKey, Vec<Node>>,
    /// every root ever modeled (for "may write anything" invalidation).
    all_roots: FxHashSet<RootKey>,
    /// roots invalidated by an unknown write (unresolvable store
    /// destination, may-write call). A root in this set never folds loads
    /// again: the unknown write may have happened at any program point,
    /// so a later store to the same cell (even a constant one) cannot be
    /// trusted to be visible to earlier loads re-scheduled after it.
    cleared_roots: FxHashSet<RootKey>,
}

impl MemState {
    /// Folded value of a cell **at load `load`'s program point**: the meet
    /// of the writer contributions whose store may precede the load
    /// (D∪R, see `docs/ipsccp_memory_order_fix.md` §4.1), plus the list of
    /// those writers `(function, block)` for `init_reachable`. Writers that
    /// execute *after* the load in program order (same-block later
    /// position, or disjoint paths) are excluded — a load must never read
    /// a value written later, which is exactly the bug class of case_0019
    /// (洞 1/2/3 share this single root cause).
    fn cell_fold(
        &self,
        key: CellKey,
        load: Node,
        ord: &OrderInfo,
    ) -> (Lattice, Vec<(Function, BasicBlock)>) {
        let mut folded = Lattice::Top;
        let mut prior = Vec::new();
        let Some(writers) = self.cells.get(&key) else {
            return (folded, prior);
        };
        for (&(wfunc, writer), &v) in writers {
            if ord.writer_relation(writer, wfunc, load) != 0 {
                folded = folded.merge(v);
                prior.push((wfunc, ord.block_of(wfunc, writer)));
            }
        }
        (folded, prior)
    }

    /// The value a load of `key` reads at its own program point: the cell
    /// contribution of the writers that precede it, merged with the initial
    /// zero when a store-free path to the load exists; else Bottom
    /// (unknown). Roots invalidated by an unknown write never fold again.
    fn read(&self, key: CellKey, load: Node, ord: &OrderInfo) -> Lattice {
        // A root invalidated by an unknown write (dynamic-index store,
        // may-write call) never folds again: the write may sit between
        // any store and any load of the root, and re-scheduled loads
        // would otherwise read a cell state that belongs to a different
        // program point.
        if self.cleared_roots.contains(&key.root()) {
            return Lattice::Bottom;
        }
        let (mut folded, prior) = self.cell_fold(key, load, ord);
        // Initial value: a zero-range covered cell answers 0 when a path
        // from the entry avoids every store preceding the load — otherwise
        // the load necessarily reads a stored value and folding 0 in would
        // be unsound (洞 2: single writer on one branch must not fold).
        let root = key.root();
        let zero_covered = self.zero.get(&root).is_some_and(|ranges| {
            ranges
                .iter()
                .any(|&(from, to)| key.offset() >= from && key.offset() < to)
        });
        if zero_covered && ord.init_reachable(load, &prior) {
            folded = folded.merge(Lattice::Constant(0));
        }
        if folded != Lattice::Top {
            folded
        } else {
            Lattice::Bottom
        }
    }

    /// Record a store `writer` (in function `func`) of `value` into `key`.
    /// A Top source writes an unknown value (Bottom contribution) but may
    /// recover when the source refines and the writer is re-visited. Returns
    /// whether the folded cell value changed.
    fn write(&mut self, key: CellKey, func: Function, writer: Inst, value: Lattice) -> bool {
        let contribution = if value == Lattice::Top {
            Lattice::Bottom
        } else {
            value
        };
        let root = key.root();
        self.all_roots.insert(root);
        let writers = self.cells.entry(key).or_default();
        let before = writers.values().fold(Lattice::Top, |acc, &v| acc.merge(v));
        if writers.is_empty() {
            self.root_cells.entry(root).or_default().push(key);
        }
        writers.insert((func, writer), contribution);
        let after = writers.values().fold(Lattice::Top, |acc, &v| acc.merge(v));
        before != after
    }

    /// Drop every cell and zero range of `root` (a call or an unknown
    /// destination may have overwritten anything).
    ///
    /// `unknown_write` marks a *value-unknown* invalidation (dynamic-index
    /// store, may-write-anything call): the write may sit between any
    /// store and any load of the root, so the root must never fold loads
    /// again (a re-scheduled load would read a cell state belonging to a
    /// different program point). A `false` invalidation (call to a callee
    /// with known write roots) only drops the current cells; the callee's
    /// stores are modeled separately and may repopulate them.
    fn clear(&mut self, root: RootKey, unknown_write: bool) -> bool {
        let mut changed = false;
        if let Some(keys) = self.root_cells.remove(&root) {
            for key in keys {
                self.cells.remove(&key);
            }
            changed = true;
        }
        changed |= self.zero.remove(&root).is_some();
        if unknown_write {
            self.cleared_roots.insert(root);
        }
        changed
    }

    /// A `MemZero` of `len` bytes at `off` of `root` zeroes the range.
    ///
    /// Cells already written by stores are NOT dropped: the worklist does
    /// not process a block's instructions in layout order, so a MemZero may
    /// be visited after the stores of the same initialization sequence. The
    /// frontend always emits MemZero before the stores, so a cell that
    /// exists must reflect a store that is semantically later; the zero
    /// range only answers loads when no store has written the cell.
    fn mem_zero(&mut self, root: RootKey, off: i64, len: i64) -> bool {
        self.all_roots.insert(root);
        let ranges = self.zero.entry(root).or_default();
        let before_len = ranges.len();
        merge_zero_interval(ranges, off, off + len);
        ranges.len() != before_len
    }

    /// The value a store through an unresolvable address may target: the
    /// concrete roots from the points-to analysis, or `None` for "any".
    fn possible_targets(
        &self,
        analysis: &EffectAnalysis,
        func: Function,
        addr: Inst,
        ctx: &ArenaContext<'_>,
    ) -> Option<Vec<RootKey>> {
        match analysis.targets_of(ctx, func, addr) {
            Some(objects) => {
                let mut roots = Vec::new();
                for o in objects {
                    match o {
                        crate::opt::analysis_passes::effects::AbstractObject::Global(g) => {
                            roots.push(RootKey::Global(g));
                        }
                        crate::opt::analysis_passes::effects::AbstractObject::Alloc(cf, a)
                            if cf == func =>
                        {
                            roots.push(RootKey::Local(func, a));
                        }
                        _ => {}
                    }
                }
                Some(roots)
            }
            None => None,
        }
        .or_else(|| {
            // Unresolvable address: conservatively everything modeled.
            Some(self.all_roots.iter().copied().collect())
        })
    }
}

/// 静态程序位置信息：一次构建，供 `MemState::read` 判定"writer（store 指令）
/// 相对 load 的程序顺序"。所有量只依赖指令位置与块结构（支配树、块可达闭包、
/// 块内 layout 序），不依赖 lattice 值，因此可以在 worklist 之外构建一次复用。
///
/// 位置分类（见 `docs/ipsccp_memory_order_fix.md` §4.1）：
/// - **D**（确定先于）：每条到 load 的执行路径都执行该 store——同函数
///   store 块支配 load 块（同块则 store 指令先于 load）；跨函数 caller 内
///   call 块支配 load 块（同块则 call 先于 load）且 callee 内 store 块
///   支配 callee 全部 return 块（callee 每次被调用必执行该 store）。
/// - **R**（可能先于）：部分路径执行该 store——同函数 store 块可达 load
///   块但非支配；跨函数 callee 内 store 块可达某 return 块且 caller 内
///   该 call 的 continuation 可达 load 块（同块则 continuation 先于 load）。
///
/// 其余 writer（store 在 load 之后执行、或与 load 路径不相交）对 load 不可
/// 见：load 绝不允许读到程序序上更晚写入的值——这正是 case_0019 的缺陷类
/// （洞 1/2/3 的同一个病根）。
struct OrderInfo<'a> {
    program: &'a Program,
    icfg: &'a ICFG,
    /// 每函数支配树（仅定义函数；声明函数无 CFG，不收录）。
    dom_trees: FxHashMap<Function, DominanceTree>,
    /// 每函数块级 CFG（可达块及其后继，终结符已解析）。
    cfgs: FxHashMap<Function, CFG>,
    /// 每函数块可达闭包：src 块 -> 可达块集合（含自身）。
    reach: FxHashMap<Function, FxHashMap<BasicBlock, FxHashSet<BasicBlock>>>,
    /// 每函数块内指令位置：inst -> layout 序 index。
    inst_pos: FxHashMap<Function, FxHashMap<Inst, usize>>,
    /// 每函数 return 块集合（终结符为 Return 的块）。
    return_blocks: FxHashMap<Function, FxHashSet<BasicBlock>>,
    /// 每函数 entry 块。
    entry_blocks: FxHashMap<Function, BasicBlock>,
}

impl<'a> OrderInfo<'a> {
    fn new(program: &'a Program, icfg: &'a ICFG) -> Self {
        let mut dom_trees = FxHashMap::default();
        let mut cfgs = FxHashMap::default();
        let mut reach = FxHashMap::default();
        let mut inst_pos = FxHashMap::default();
        let mut return_blocks: FxHashMap<Function, FxHashSet<BasicBlock>> = FxHashMap::default();
        let mut entry_blocks = FxHashMap::default();
        for &func in program.function_layout() {
            let data = program.func_data(func);
            if data.layout().is_decl() {
                continue;
            }
            let mut pos_map = FxHashMap::default();
            for bb_layout in data.layout().basicblocks() {
                let bb = bb_layout.bb();
                let terminator = *bb_layout.insts().get_last().unwrap();
                for (idx, &inst) in bb_layout.insts().iter().enumerate() {
                    pos_map.insert(inst, idx);
                }
                if matches!(data.inst_data(terminator).kind(), InstKind::Return(..)) {
                    return_blocks.entry(func).or_default().insert(bb);
                }
            }
            inst_pos.insert(func, pos_map);
            entry_blocks.insert(func, data.layout().entry_bb().unwrap().bb());
            let Some(cfg) = CFG::new(data) else {
                continue;
            };
            // 块可达闭包：逐块 BFS（函数块数小，BFS 足够）。
            let mut fn_reach: FxHashMap<BasicBlock, FxHashSet<BasicBlock>> = FxHashMap::default();
            for &src in cfg.blocks() {
                let mut visited = FxHashSet::default();
                let mut stack = vec![src];
                while let Some(bb) = stack.pop() {
                    if !visited.insert(bb) {
                        continue;
                    }
                    for &succ in cfg.successors_of(bb) {
                        stack.push(succ);
                    }
                }
                fn_reach.insert(src, visited);
            }
            reach.insert(func, fn_reach);
            dom_trees.insert(func, DominanceTree::from_cfg(&cfg));
            cfgs.insert(func, cfg);
        }
        Self {
            program,
            icfg,
            dom_trees,
            cfgs,
            reach,
            inst_pos,
            return_blocks,
            entry_blocks,
        }
    }

    fn block_of(&self, func: Function, inst: Inst) -> BasicBlock {
        self.program
            .func_data(func)
            .layout()
            .parent_bb(inst)
            .unwrap()
    }

    /// writer S 相对 load L 的程序顺序：
    /// `2` = 确定先于（D）；`1` = 可能先于（R）；`0` = 不可见（S 在 L 之后
    /// 执行，或与 L 的路径不相交）。
    fn writer_relation(&self, writer: Inst, writer_func: Function, load: Node) -> u8 {
        let load_bb = self.block_of(load.func, load.inst);
        let store_bb = self.block_of(writer_func, writer);
        if writer_func == load.func {
            self.same_func_relation(writer, store_bb, load, load_bb)
        } else {
            self.cross_func_relation(writer_func, store_bb, load, load_bb)
        }
    }

    fn same_func_relation(
        &self,
        writer: Inst,
        store_bb: BasicBlock,
        load: Node,
        load_bb: BasicBlock,
    ) -> u8 {
        if store_bb == load_bb {
            // 同块：只有 layout 序上 store 先于 load 才算确定先于；
            // store 在 load 之后（case_0019 形态）对 load 不可见。
            let sp = self.inst_pos[&load.func][&writer];
            let lp = self.inst_pos[&load.func][&load.inst];
            return if sp < lp { 2 } else { 0 };
        }
        let Some(tree) = self.dom_trees.get(&load.func) else {
            return 0;
        };
        // CFG/支配树只含可达块：不可达块上的 store 永不执行，不参与。
        if !tree.contains(store_bb) || !tree.contains(load_bb) {
            return 0;
        }
        if tree.dominates(store_bb, load_bb) {
            return 2;
        }
        if self.reach[&load.func][&store_bb].contains(&load_bb) {
            1
        } else {
            0
        }
    }

    fn cross_func_relation(
        &self,
        callee: Function,
        store_bb: BasicBlock,
        load: Node,
        load_bb: BasicBlock,
    ) -> u8 {
        let Some(callee_tree) = self.dom_trees.get(&callee) else {
            return 0;
        };
        if !callee_tree.contains(store_bb) {
            return 0;
        }
        let Some(caller_tree) = self.dom_trees.get(&load.func) else {
            return 0;
        };
        if !caller_tree.contains(load_bb) {
            return 0;
        }
        // S 支配 callee 全部 return 块：callee 每次被调用必执行 S。
        let store_dominates_all_returns = self
            .return_blocks
            .get(&callee)
            .is_some_and(|rbs| rbs.iter().all(|&rb| callee_tree.dominates(store_bb, rb)));
        // S 可达 callee 某 return 块：部分调用路径执行 S。
        let store_reaches_return = self.return_blocks.get(&callee).is_some_and(|rbs| {
            rbs.iter()
                .any(|&rb| self.reach[&callee][&store_bb].contains(&rb))
        });
        let mut dominates = false;
        let mut reaches = false;
        // `call_sites_of` 按 **caller** 索引：遍历 load 所在函数的所有调用点，
        // 筛出调用 `callee` 的。
        for cs in self.icfg.call_sites_of(load.func) {
            if cs.callee != callee {
                continue;
            }
            let call_bb = self.block_of(load.func, cs.call);
            if !caller_tree.contains(call_bb) {
                continue;
            }
            // caller 侧"必经"：call 块支配 load 块（同块则 call 指令先于 load）。
            let call_before_load = if call_bb == load_bb {
                self.inst_pos[&load.func][&cs.call] < self.inst_pos[&load.func][&load.inst]
            } else {
                caller_tree.dominates(call_bb, load_bb)
            };
            if call_before_load && store_dominates_all_returns {
                dominates = true;
            }
            // caller 侧"可达"：call 的 continuation 可达 load 块（同块则
            // continuation 先于 load）。
            if store_reaches_return {
                let cont_bb = self.block_of(load.func, cs.continuation);
                let cont_reaches_load = if cont_bb == load_bb {
                    self.inst_pos[&load.func][&cs.continuation]
                        < self.inst_pos[&load.func][&load.inst]
                } else {
                    self.reach[&load.func][&cont_bb].contains(&load_bb)
                };
                if cont_reaches_load {
                    reaches = true;
                }
            }
        }
        if dominates {
            2
        } else if reaches {
            1
        } else {
            0
        }
    }

    /// 是否存在从程序入口到 load、避开全部 `prior` writer 块的真实路径：
    /// 有则初始值（零区间）可能被读到；无则必经某个先于 load 的 store，
    /// 初始值不可达。同函数 writer 避开其所在块；跨函数 writer 拦截
    /// "必经 callee store"的 call 块（callee 内 entry→return 全路径都经过
    /// store 块时，调用该 callee 必然执行 store）。
    fn init_reachable(&self, load: Node, prior: &[(Function, BasicBlock)]) -> bool {
        let func = load.func;
        let load_bb = self.block_of(func, load.inst);
        let mut same_writer_blocks = FxHashSet::default();
        let mut cross_writers: FxHashMap<Function, FxHashSet<BasicBlock>> = FxHashMap::default();
        for &(f, bb) in prior {
            if f == func {
                same_writer_blocks.insert(bb);
            } else {
                cross_writers.entry(f).or_default().insert(bb);
            }
        }
        // 跨函数 writer：callee 内不存在避开其全部 store 块的 entry→return
        // 路径时，该 callee 的调用点视作路径拦截点。
        let mut blocked_calls = FxHashSet::default();
        for (callee, store_bbs) in &cross_writers {
            if self.callee_avoids(*callee, store_bbs) {
                continue;
            }
            // `call_sites_of` 按 caller 索引：筛出本函数内调用 `callee` 的。
            for cs in self.icfg.call_sites_of(func) {
                if cs.callee == *callee {
                    blocked_calls.insert(self.block_of(func, cs.call));
                }
            }
        }
        let entry = self.entry_blocks[&func];
        let mut stack = vec![entry];
        let mut visited = FxHashSet::default();
        while let Some(bb) = stack.pop() {
            if same_writer_blocks.contains(&bb) || blocked_calls.contains(&bb) {
                continue;
            }
            if bb == load_bb {
                return true;
            }
            for &succ in self.cfgs[&func].successors_of(bb) {
                if visited.insert(succ) {
                    stack.push(succ);
                }
            }
        }
        false
    }

    /// callee 内是否存在避开 `avoid` 全部块的 entry → 任一 return 块路径。
    fn callee_avoids(&self, callee: Function, avoid: &FxHashSet<BasicBlock>) -> bool {
        let Some(cfg) = self.cfgs.get(&callee) else {
            return false;
        };
        let entry = self.entry_blocks[&callee];
        let mut stack = vec![entry];
        let mut visited = FxHashSet::default();
        while let Some(bb) = stack.pop() {
            if avoid.contains(&bb) {
                continue;
            }
            if self
                .return_blocks
                .get(&callee)
                .is_some_and(|rbs| rbs.contains(&bb))
            {
                return true;
            }
            for &succ in cfg.successors_of(bb) {
                if visited.insert(succ) {
                    stack.push(succ);
                }
            }
        }
        false
    }
}

/// Merge `[from, to)` into a sorted, disjoint interval list.
fn merge_zero_interval(ranges: &mut Vec<(i64, i64)>, from: i64, to: i64) {
    let mut from = from;
    let mut to = to;
    let mut i = 0;
    while i < ranges.len() {
        let (l, r) = ranges[i];
        if r < from {
            i += 1;
            continue;
        }
        if l > to {
            break;
        }
        from = from.min(l);
        to = to.max(r);
        ranges.remove(i);
    }
    ranges.insert(i, (from, to));
}

/// Clear every cell the callee may write (in the caller's terms) and
/// re-schedule the affected loads. Unknown writers clear everything.
fn invalidate_call(
    analysis: &EffectAnalysis,
    state: &mut MemState,
    callee: Function,
    func: Function,
    mem_reschedule: &mut Vec<Node>,
) {
    let roots: Vec<RootKey> = match analysis.call_write_roots(callee, func) {
        Some(roots) => roots
            .into_iter()
            .map(|r| match r {
                WriteRoot::Global(g) => RootKey::Global(g),
                WriteRoot::Local(f, a) => RootKey::Local(f, a),
            })
            .collect(),
        None => state.all_roots.iter().copied().collect(),
    };
    let unknown = analysis.call_write_roots(callee, func).is_none();
    for root in roots {
        if state.clear(root, unknown) {
            if let Some(loaders) = state.root_loaders.get(&root) {
                mem_reschedule.extend(loaders.iter().copied());
            }
        }
    }
}

/// Resolve `addr` in `func` to a constant-offset cell on a modeled root,
/// using the base-object environment.
fn resolve_cell(
    env: &BaseEnv,
    ctx: &ArenaContext<'_>,
    func: Function,
    addr: Inst,
) -> Option<(CellKey, RootKey)> {
    let off = env.constant_offset(ctx, addr)?;
    match env.base_of(ctx, addr) {
        MemObject::Alloc(a) => {
            let key = CellKey::Local(func, a, off);
            Some((key, key.root()))
        }
        MemObject::Global(g) => {
            let key = CellKey::Global(g, off);
            Some((key, key.root()))
        }
        _ => None,
    }
}

impl Pass for IPSCCP {
    fn run(&mut self, program: &mut Program) -> bool {
        // Stage 0: Variables initialization.
        let mut edge_visited = EdgeSet::default();
        let mut node_visited = NodeSet::default();
        let mut edge_worklist: VecDeque<Edge> = VecDeque::default();
        let mut node_worklist: VecDeque<Node> = VecDeque::default();
        let mut lattice_map = LatticeMap::default();

        // Whole-program purity / alias analysis feeding the main-memory
        // simulation below (constant-offset cells on local and global
        // roots, zero ranges, per-call invalidation).
        let analysis = EffectAnalysis::new(program);
        let mut state = MemState::default();
        {
            // Zero-initialized globals answer constant-offset loads with 0
            // until a store overwrites them.
            let ctx = ArenaContext {
                program,
                curr_func: Some(program.get_main_function()),
            };
            for &g in program.global_inst_layout() {
                let InstKind::GlobalAlloc(global_alloc) = ctx.inst_data(g).kind() else {
                    continue;
                };
                if matches!(
                    ctx.inst_data(global_alloc.init()).kind(),
                    InstKind::ZeroInit
                ) {
                    let size = ctx.inst_data(g).ty().derefernce().size() as i64;
                    let root = RootKey::Global(g);
                    state.all_roots.insert(root);
                    merge_zero_interval(state.zero.entry(root).or_default(), 0, size);
                }
            }
        }

        // Stage 0.1
        // Default: Set every integer to const, float to bottom, else remain top.
        for &func in program.function_layout() {
            let arena = ArenaContext {
                program,
                curr_func: Some(func),
            };
            for (&inst, data) in arena.inst_datas() {
                let node = Node::new(func, inst);
                match data.kind() {
                    InstKind::Integer(int) => lattice_map.new_const(node, int.value()),
                    InstKind::Float(..) => lattice_map.new_var(node),
                    _ => {}
                }
            }
        }

        // TODO: What about global value?

        // Stage 0.2
        // Build ICFG. Ready to start the worklist algorithm.
        let icfg = icfg::ICFG::new(program);
        // 静态位置信息（支配树/块可达闭包/块内序/inst 属主）——worklist 阶段
        // 的 MemState::read 用它过滤"程序序上先于 load"的 writer。
        let ord = OrderInfo::new(program, &icfg);
        let main_func = program.get_main_function();
        let entry_bb_layout = program.func_data(main_func).layout().entry_bb().unwrap();
        let first_inst = *entry_bb_layout.insts().get_first().unwrap();
        let start_node = Node::new(main_func, first_inst);

        // A virtual edge to start the loop
        let start_edge = Edge {
            edge_type: EdgeType::Normal,
            src: start_node,
            dst: start_node,
        };
        edge_worklist.push_back(start_edge);
        edge_visited.insert(start_edge);
        let mut block_visited = FxHashSet::default();

        // Stage 1: Worklist algorithm.
        while !edge_worklist.is_empty() || !node_worklist.is_empty() {
            if let Some(edge) = edge_worklist.pop_front() {
                let Edge { dst, .. } = edge;
                let parent_bb = program
                    .func_data(dst.func)
                    .layout()
                    .parent_bb(dst.inst)
                    .unwrap();
                block_visited.insert(Block::new(dst.func, parent_bb));
                // first time visit the node
                if node_visited.insert(dst) {
                    node_worklist.push_back(dst);
                }
            }

            // Tail-call relay nodes whose lattice was updated by the Return arm
            // need re-scheduling so their TailCall arm can forward the callee's
            // return value upward. They cannot be pushed to `node_worklist`
            // directly inside the node-processing block because the closures
            // (`merge_and_extend` et al.) hold a mutable borrow of it; collect
            // them here and drain after those closures are dropped.
            let mut relay_targets: Vec<Node> = Vec::new();
            // Loads whose memory cells changed (stores, MemZero, calls) are
            // re-scheduled the same way.
            let mut mem_reschedule: Vec<Node> = Vec::new();

            if let Some(node) = node_worklist.pop_front() {
                let push_edge = |edge: Edge| {
                    edge_visited
                        .insert(edge)
                        .then(|| edge_worklist.push_back(edge));
                };
                let Node { func, inst } = node;
                let data = program.func_data(func);
                let mut extend_affected_node_used_by = |node: Node| {
                    let data = program.func_data(node.func);
                    node_worklist.extend(
                        data.inst_data(node.inst)
                            .used_by()
                            .iter()
                            .filter(|&&inst| {
                                data.layout().parent_bb(inst).is_some_and(|b| {
                                    block_visited.contains(&Block::new(node.func, b))
                                })
                            })
                            .map(|&inst| Node::new(node.func, inst)),
                    );
                };
                let mut merge_and_extend =
                    |node: Node, status: Lattice, lattice_map: &mut LatticeMap| -> bool {
                        if lattice_map.insert_or_update(node, status) {
                            extend_affected_node_used_by(node);
                            true
                        } else {
                            false
                        }
                    };
                match data.inst_data(inst).kind() {
                    // These instruction define a scalar and !never! appear in the layout
                    InstKind::Aggregate(..)
                    | InstKind::GlobalAlloc(..)
                    | InstKind::Undef
                    | InstKind::ZeroInit
                    | InstKind::Integer(..)
                    | InstKind::Float(..)
                    | InstKind::BlockArgRef(..) => unreachable!(
                        "instruction {:?} with data {:?} should never appear in the layout",
                        inst,
                        data.inst_data(inst)
                    ),
                    // Addresses are not i32 constants; keep them variable.
                    InstKind::GetElemPtr(..) | InstKind::Alloc => {
                        merge_and_extend(node, Lattice::Bottom, &mut lattice_map);
                    }
                    // Loads read the simulated memory: a constant-offset
                    // cell on a local/global root folds to the stored value
                    // (or 0 under a zero range); everything else is
                    // conservatively Bottom.
                    InstKind::Load(load) => {
                        let env = analysis.env_of(func);
                        let ctx = ArenaContext {
                            program,
                            curr_func: Some(func),
                        };
                        match resolve_cell(env, &ctx, func, load.src()) {
                            Some((key, root)) => {
                                // 位置感知读：只折叠程序序上先于本 load 的 writer。
                                let value = state.read(key, node, &ord);
                                // Overwrite: the load mirrors the current
                                // memory snapshot, not a meet of historical
                                // snapshots.
                                if lattice_map.insert_or_replace(node, value) {
                                    extend_affected_node_used_by(node);
                                }
                                // Register the load for re-scheduling when
                                // its root changes. Deduplicate: a load that
                                // is (re)processed many times must not grow
                                // the loader list unboundedly.
                                let loaders = state.root_loaders.entry(root).or_default();
                                if !loaders.contains(&node) {
                                    loaders.push(node);
                                }
                            }
                            None => {
                                merge_and_extend(node, Lattice::Bottom, &mut lattice_map);
                            }
                        }
                    }
                    InstKind::Binary(binary) => {
                        let status = match (
                            lattice_map.get(Node::new(func, binary.lhs())),
                            lattice_map.get(Node::new(func, binary.rhs())),
                        ) {
                            (Lattice::Bottom, _) | (_, Lattice::Bottom) => Lattice::Bottom,
                            (Lattice::Constant(lhs), Lattice::Constant(rhs)) => {
                                Lattice::Constant(mathematic_operation(binary.op(), lhs, rhs))
                            }
                            _ => Lattice::Top,
                        };
                        merge_and_extend(node, status, &mut lattice_map);
                    }
                    InstKind::Select(select) => {
                        let cond = select.cond();
                        let status_of = |node: Node| lattice_map.get(node);
                        let status = match status_of(Node::new(func, cond)) {
                            Lattice::Top => Lattice::Top,
                            Lattice::Constant(constant) => status_of(Node::new(
                                func,
                                if constant != 0 {
                                    select.if_true()
                                } else {
                                    select.if_false()
                                },
                            )),
                            Lattice::Bottom => status_of(Node::new(func, select.if_true()))
                                .merge(status_of(Node::new(func, select.if_false()))),
                        };
                        merge_and_extend(node, status, &mut lattice_map);
                    }
                    // Vector operations never carry an i32 constant lattice; the
                    // frontend (and any future vectorizer) emits them on vector
                    // operands, which are always Top here.
                    InstKind::Fma(..)
                    | InstKind::VectorSplat(..)
                    | InstKind::VectorExtractElement(..)
                    | InstKind::VectorInsertElement(..)
                    | InstKind::VectorReduce(..) => {
                        // These vector ops cannot be constant-folded, but
                        // their results are *definitely* not constants.
                        // Marking them Top (undef) lets the optimistic SCCP
                        // merge fold a loop block-param to the entry edge's
                        // constant (e.g. `sum` -> 0 for `sum += c[i][j]`
                        // nested loops), silently zeroing the accumulator.
                        // Bottom (overdefined) is the correct lattice value.
                        merge_and_extend(node, Lattice::Bottom, &mut lattice_map);
                    }
                    InstKind::Jump(jump) => {
                        let params = data.bb_data(jump.target()).params();
                        for (&arg, &param) in jump.args().iter().zip(params) {
                            let arg_status = lattice_map.get(Node::new(func, arg));
                            merge_and_extend(Node::new(func, param), arg_status, &mut lattice_map);
                        }
                    }
                    InstKind::Branch(branch) => {
                        let cond = branch.cond();
                        let cond_status = lattice_map.get(Node::new(func, cond));
                        let worklist = match cond_status {
                            Lattice::Top => [None, None],
                            Lattice::Bottom => [
                                Some((branch.t_target(), branch.t_args())),
                                Some((branch.f_target(), branch.f_args())),
                            ],
                            Lattice::Constant(c) => {
                                if c != 0 {
                                    [Some((branch.t_target(), branch.t_args())), None]
                                } else {
                                    [Some((branch.f_target(), branch.f_args())), None]
                                }
                            }
                        };
                        for (target, args) in worklist.into_iter().flatten() {
                            let params = data.bb_data(target).params();
                            for (&arg, &param) in args.iter().zip(params) {
                                let arg_status = lattice_map.get(Node::new(func, arg));
                                merge_and_extend(
                                    Node::new(func, param),
                                    arg_status,
                                    &mut lattice_map,
                                );
                            }
                        }
                    }
                    InstKind::Cast(cast) => {
                        let status = if data.inst_data(inst).ty().is_i32() {
                            match data.inst_data(cast.src()).kind() {
                                InstKind::Float(float) => fold_f32_to_i32(float.value())
                                    .map_or(Lattice::Bottom, Lattice::Constant),
                                _ => Lattice::Bottom,
                            }
                        } else {
                            Lattice::Bottom
                        };
                        merge_and_extend(node, status, &mut lattice_map);
                    }
                    // Stores write into the simulated memory: a resolvable
                    // constant-offset cell records the source lattice; an
                    // unresolvable destination clears everything it may
                    // target (via the points-to analysis).
                    InstKind::Store(store) => {
                        let env = analysis.env_of(func);
                        let ctx = ArenaContext {
                            program,
                            curr_func: Some(func),
                        };
                        let value = lattice_map.get(Node::new(func, store.src()));
                        match resolve_cell(env, &ctx, func, store.dest()) {
                            Some((key, root)) => {
                                if state.write(key, func, inst, value) {
                                    if let Some(loaders) = state.root_loaders.get(&root) {
                                        mem_reschedule.extend(loaders.iter().copied());
                                    }
                                }
                            }
                            None => {
                                let roots =
                                    state.possible_targets(&analysis, func, store.dest(), &ctx);
                                for root in roots.unwrap_or_default() {
                                    if state.clear(root, true) {
                                        if let Some(loaders) = state.root_loaders.get(&root) {
                                            mem_reschedule.extend(loaders.iter().copied());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    InstKind::MemZero(mem_zero) => {
                        let env = analysis.env_of(func);
                        let ctx = ArenaContext {
                            program,
                            curr_func: Some(func),
                        };
                        // A runtime-length MemZero (M53 zero-store loops)
                        // covers an unknown range: conservatively invalidate
                        // the whole root instead of folding a zero interval.
                        let len = match mem_zero.byte_len_len() {
                            MemZeroLen::Const(n) => Some(*n as i64),
                            MemZeroLen::Value(_) => None,
                        };
                        match (resolve_cell(env, &ctx, func, mem_zero.dest()), len) {
                            (Some((key, root)), Some(len)) => {
                                if state.mem_zero(root, key.offset(), len) {
                                    if let Some(loaders) = state.root_loaders.get(&root) {
                                        mem_reschedule.extend(loaders.iter().copied());
                                    }
                                }
                            }
                            (Some((_, root)), None) => {
                                if state.clear(root, true) {
                                    if let Some(loaders) = state.root_loaders.get(&root) {
                                        mem_reschedule.extend(loaders.iter().copied());
                                    }
                                }
                            }
                            (None, _) => {
                                let roots =
                                    state.possible_targets(&analysis, func, mem_zero.dest(), &ctx);
                                for root in roots.unwrap_or_default() {
                                    if state.clear(root, true) {
                                        if let Some(loaders) = state.root_loaders.get(&root) {
                                            mem_reschedule.extend(loaders.iter().copied());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    InstKind::TailCall(tail_call) => {
                        // A tail call transfers control to the callee just like a
                        // regular call, so its actual arguments must flow into the
                        // callee's formal parameters. Without this, the only
                        // argument source IPSCCP would see is the (non-tail) call
                        // site, causing parameters that vary across recursive tail
                        // calls to be mis-propagated as constants.
                        let callee = tail_call.callee();
                        let callee_data = program.func_data(callee);
                        if !callee_data.layout().is_decl() {
                            for (&arg, &param) in tail_call.args().iter().zip(callee_data.params())
                            {
                                let arg_status = lattice_map.get(Node::new(func, arg));
                                merge_and_extend(
                                    Node::new(callee, param),
                                    arg_status,
                                    &mut lattice_map,
                                );
                            }
                        }
                        // Relay: a tail call forwards the callee's return value
                        // directly to the current function's caller (the frame is
                        // reused). The callee's Return arm deposits its return value
                        // into *this* node's lattice (see the Return arm below);
                        // propagate it further along this node's outgoing Return
                        // edges, which connect to the caller's call site.
                        let node_status = lattice_map.get(node);
                        for Edge { dst, edge_type, .. } in icfg.outgoing_edges_of(node) {
                            if edge_type != EdgeType::Return {
                                continue;
                            }
                            if let Some(cs) = icfg.call_site_before(dst) {
                                merge_and_extend(
                                    Node::new(dst.func, cs.call),
                                    node_status,
                                    &mut lattice_map,
                                );
                            }
                        }
                        // The tail callee may write memory; invalidate the
                        // cells it can reach.
                        invalidate_call(&analysis, &mut state, callee, func, &mut mem_reschedule);
                    }
                    InstKind::Call(call) => {
                        let callee = call.callee();
                        let callee_data = program.func_data(callee);
                        if callee_data.layout().is_decl() {
                            merge_and_extend(node, Lattice::Bottom, &mut lattice_map);
                        } else {
                            for (&arg, &param) in call.args().iter().zip(callee_data.params()) {
                                let arg_status = lattice_map.get(Node::new(func, arg));
                                merge_and_extend(
                                    Node::new(callee, param),
                                    arg_status,
                                    &mut lattice_map,
                                );
                            }
                        }
                        // The callee may write memory; invalidate the cells
                        // it can reach (unknown writers clear everything).
                        invalidate_call(&analysis, &mut state, callee, func, &mut mem_reschedule);
                    }
                    InstKind::Return(ret) => {
                        if let Some(ret_val) = ret.value() {
                            let ret_val_status = lattice_map.get(Node::new(func, ret_val));
                            let outgoing_edges = icfg.outgoing_edges_of(node);
                            for Edge { dst, .. } in outgoing_edges {
                                // For a regular call, the Return edge lands at the
                                // call's continuation and `call_site_before` resolves
                                // the call site whose `.call` node receives the value.
                                // For a tail call, the edge lands at the tail-call
                                // instruction itself (the relay node), which is
                                // deliberately absent from `callsite_by_continuation`
                                // — deposit the value directly into that node.
                                let target = match icfg.call_site_before(dst) {
                                    Some(cs) => Node::new(dst.func, cs.call),
                                    None => dst,
                                };
                                if merge_and_extend(target, ret_val_status, &mut lattice_map) {
                                    // The relay node's lattice was set externally
                                    // (by us, not by its own evaluation), so it will
                                    // not be revisited through `used_by`. Re-schedule
                                    // it so its TailCall arm can forward the value
                                    // upward along the tail-call chain.
                                    if matches!(
                                        program
                                            .func_data(target.func)
                                            .inst_data(target.inst)
                                            .kind(),
                                        InstKind::TailCall(..)
                                    ) {
                                        relay_targets.push(target);
                                    }
                                }
                            }
                        }
                    }
                }
                // update edges.
                match data.inst_data(inst).kind() {
                    InstKind::Branch(branch) => {
                        let cond = branch.cond();
                        let cond_status = lattice_map.get(Node::new(func, cond));
                        let construct_edge = |block| Edge {
                            edge_type: EdgeType::Normal,
                            src: Node::new(func, inst),
                            dst: Node::new(
                                func,
                                *data.layout().basicblock(block).insts().get_first().unwrap(),
                            ),
                        };
                        let edges = match cond_status {
                            Lattice::Top => [None, None],
                            Lattice::Bottom => [Some(branch.t_target()), Some(branch.f_target())],
                            Lattice::Constant(constant) => {
                                if constant != 0 {
                                    [Some(branch.t_target()), None]
                                } else {
                                    [Some(branch.f_target()), None]
                                }
                            }
                        };
                        edges
                            .into_iter()
                            .flatten()
                            .map(construct_edge)
                            .for_each(push_edge);
                    }
                    InstKind::Return(..) => {}
                    InstKind::TailCall(..) => {
                        // Push the Call edge so the callee's entry becomes
                        // reachable. Return edges are consumed by the relay logic
                        // in the TailCall lattice arm, not pushed here.
                        icfg.outgoing_edges_of(Node::new(func, inst))
                            .filter(|e| e.edge_type == EdgeType::Call)
                            .for_each(push_edge);
                    }
                    _ => {
                        icfg.outgoing_edges_of(Node::new(func, inst))
                            .for_each(push_edge);
                    }
                }
            }
            // Closures borrowing `node_worklist` are now dropped; safe to extend.
            node_worklist.extend(relay_targets.drain(..));
            node_worklist.extend(mem_reschedule.drain(..));
        }

        let mut changed = false;

        let const_replace_list = lattice_map
            .0
            .iter()
            .filter_map(|(&node, &lattice)| {
                if let Lattice::Constant(c) = lattice {
                    Some((node, c))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        for (node, val) in const_replace_list {
            let mut arena = ArenaContextMut {
                program,
                curr_func: Some(node.func),
            };
            if matches!(
                arena.inst_data(node.inst).kind(),
                InstKind::Call(..) | InstKind::BlockArgRef(..) | InstKind::TailCall(..)
            ) {
                let has_uses = !arena.inst_data(node.inst).used_by().is_empty();
                if !has_uses {
                    continue;
                }
                let integer = arena.new_local_inst().integer(val);
                visit_and_replace(&mut arena, node.inst, integer);
                changed = true;
            } else {
                arena.replace_inst_with(node.inst).integer(val);
                let data = program.func_data_mut(node.func);
                let Some(parent_bb) = data.layout().parent_bb(node.inst) else {
                    continue;
                };
                data.detach_layout_inst(parent_bb, node.inst);
                changed = true;
            }
        }

        let mut useless_conditional_branch = vec![];
        for &func in program.function_layout() {
            let data = program.func_data(func);
            for bb_layout in program.func_data(func).layout().basicblocks() {
                let terminator = *bb_layout.insts().get_last().unwrap();
                if let InstKind::Branch(branch) = data.inst_data(terminator).kind() {
                    if let InstKind::Integer(..) = data.inst_data(branch.cond()).kind() {
                        useless_conditional_branch.push(Node::new(func, terminator));
                    }
                }
            }
        }

        for node in useless_conditional_branch {
            let Node { func, inst } = node;
            let data = program.func_data_mut(func);
            let InstKind::Branch(branch) = data.inst_data(inst).kind() else {
                unreachable!()
            };
            let InstKind::Integer(int) = data.inst_data(branch.cond()).kind() else {
                unreachable!()
            };
            let (target, args) = if int.value() == 0 {
                (branch.f_target(), branch.f_args().to_vec())
            } else {
                (branch.t_target(), branch.t_args().to_vec())
            };
            data.replace_inst_with(inst).jump(target, args);
            changed = true;
        }

        let mut remove_list = vec![];
        for &func in program.function_layout() {
            let data = program.func_data(func);
            remove_list.extend(
                data.layout()
                    .basicblocks()
                    .iter()
                    .map(|l| l.bb())
                    .filter(|&bb| {
                        bb != data.layout().entry_bb().unwrap().bb()
                            && data.bb_data(bb).used_by().is_empty()
                            // A block may have become unreachable while its
                            // non-terminator instructions still feed values
                            // into reachable blocks (e.g. LICM-hoisted GEPs
                            // used by a surviving loop body). Removing it then
                            // destroys live values and leaves dangling
                            // operands. Only remove blocks whose every
                            // instruction is itself unused.
                            && data
                                .layout()
                                .basicblock(bb)
                                .insts()
                                .iter()
                                .all(|&inst| data.inst_data(inst).used_by().is_empty())
                    })
                    .map(|bb| Block::new(func, bb))
                    .collect::<Vec<_>>(),
            )
        }
        for bb in remove_list {
            let data = program.func_data_mut(bb.func);
            data.remove_layout_basicblock(bb.block);
            changed = true;
        }

        changed
    }
}

fn mathematic_operation(op: BinaryOp, lhs: i32, rhs: i32) -> i32 {
    match op {
        BinaryOp::NotEq => (lhs != rhs) as i32,
        BinaryOp::Eq => (lhs == rhs) as i32,
        BinaryOp::Gt => (lhs > rhs) as i32,
        BinaryOp::Lt => (lhs < rhs) as i32,
        BinaryOp::Ge => (lhs >= rhs) as i32,
        BinaryOp::Le => (lhs <= rhs) as i32,
        BinaryOp::Add => lhs.wrapping_add(rhs),
        BinaryOp::Sub => lhs.wrapping_sub(rhs),
        BinaryOp::Mul => lhs.wrapping_mul(rhs),
        BinaryOp::Div => {
            assert_ne!(rhs, 0);
            lhs.wrapping_div(rhs)
        }
        BinaryOp::Rem => {
            assert_ne!(rhs, 0);
            lhs.wrapping_rem(rhs)
        }
        BinaryOp::And => lhs & rhs,
        BinaryOp::Or => lhs | rhs,
        BinaryOp::Xor => lhs ^ rhs,
        BinaryOp::Shl => lhs.wrapping_shl(rhs as u32),
        BinaryOp::Shr => (lhs as u32).wrapping_shr(rhs as u32) as i32,
        BinaryOp::Sar => lhs.wrapping_shr(rhs as u32),
        BinaryOp::Min => lhs.min(rhs),
        BinaryOp::Max => lhs.max(rhs),
    }
}

pub(super) fn fold_f32_to_i32(value: f32) -> Option<i32> {
    // The target conversions truncate toward zero for representable values.
    // Keep non-finite and out-of-range values as runtime casts because Rust's
    // saturating `as` conversion does not match the target instructions there.
    (value.is_finite() && value >= i32::MIN as f32 && value < i32::MAX as f32)
        .then_some(value as i32)
}

#[cfg(test)]
mod tests;
