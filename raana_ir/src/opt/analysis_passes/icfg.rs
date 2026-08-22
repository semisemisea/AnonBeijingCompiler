//! ICFG implementation.
//! Better remove unused function before building the icfg.
//!
//! ---
//!
//! ## 补充说明（中文）
//!
//! ### 一句话定位
//!
//! **ICFG（Interprocedural Control-Flow Graph，过程间控制流图）**把整个程序所有
//! 函数的函数内 CFG「缝」成一张跨函数的有向图：节点是 `(func, inst)` 二元组
//! （`Node`），函数内的顺序执行、跳转、分支构成 `Normal` 边，函数之间用 `Call`
//! 边（调用点 → 被调函数入口）与 `Return` 边（被调函数返回点 → 调用点的继续点）
//! 连接，另用一条 `CallToReturn`「影子边」表示调用指令到其下一条指令的落回。
//! 过程间分析（如 `opt/passes/ipsccp.rs`）沿这张图跨函数传播信息，而无需关心各
//! 函数内部结构。术语：SSA / DFE / IPSCCP / TCO 等见
//! `docs/offline-handbook/glossary.md`。
//!
//! ### 与 `call_graph` 的区别
//!
//! `opt/analysis_passes/call_graph.rs` 只记录「函数 → 函数」的调用关系（点、边都
//! 是函数粒度）；本模块把粒度细化到**指令**：节点是单条指令，边带 `EdgeType`，
//! 并同时维护出边 / 入边索引，支持从任意指令出发做**稀疏**传播（只遍历被边到达
//! 的节点，见 ipsccp 的「稀疏」语义）。
//!
//! ### 数据结构
//!
//! - `EdgeType`：四类边枚举 `Normal` / `Call` / `Return` / `CallToReturn`，语义见
//!   下节「边类型」。
//! - `Node`（`opt::utils::global_handle`）：`(func: Function, inst: Inst)` 二元组，
//!   全局唯一标识一条指令；`Copy`。
//! - `Edge`：一条边的 **AoS 视图**（Array-of-Structs，结构体数组风格）——
//!   `edge_type` + `src: Node` + `dst: Node`，出 / 入边查询返回的就是它；
//!   `Copy` + `Hash`，可直接当 key 或比较。
//! - `Indices`：`SmallVec<[usize; 2]>`，一组边号。一个节点通常只有 1~2 条出边，
//!   用 SmallVec 免堆分配。
//! - `EdgeTable`：整张边表的 **SoA 存储**（Structure-of-Arrays，并行数组）——
//!   `edge_types` / `srcs` / `dsts` 三个等长 `Vec`，边号 `i` 即
//!   `(edge_types[i], srcs[i], dsts[i])`。`srcs`/`dsts` 只存 `Inst`，跨函数边另一
//!   端的函数由查询时按边类型推断（见 `outgoing_edges_of`）。
//! - `CallSite`（`opt::utils::call`）：一个调用点的四元组——`call`（调用指令）、
//!   `caller`、`callee`、`continuation`（调用指令的下一条指令；尾调用时等于
//!   `call` 本身，作哨兵）。
//! - `CallSiteTable`：调用点表的 SoA 存储（`calls`/`callers`/`callees`/
//!   `continuations` 并行数组）。
//! - `ICFG`：图本体，内部字段：
//!   - `edges: EdgeTable`——全部边（函数内 + 跨函数）；
//!   - `call_sites` + `call_sites_range: FxHashMap<Function, Range<usize>>`、
//!     `return_sites: Vec<Inst>` + `return_sites_range`——调用点与返回点的**全局
//!     连续存储**加「函数 → 区间」索引（同一函数的调用点、返回点在数组里连续）；
//!   - `cross_edge_callsite_map: FxHashMap<usize, usize>`——**边号 → 调用点号**
//!     稀疏索引：`Call`/`Return` 跨函数边自身不携带调用点信息，查询时靠它反查
//!     caller / callee；
//!   - `callsite_by_call` / `callsite_by_continuation`——`Node` → 调用点号，支撑
//!     `call_site_at` / `call_site_before` 两条反向查询；
//!   - `forward` / `backward`——`Node` → 出边 / 入边边号列表；
//!   - `reachable_functions`——第二阶段 BFS 从 `main` 出发到达的函数集合（含声明
//!     函数）。
//!
//! ### 边类型（`EdgeType`）
//!
//! - `Normal`：函数内控制流（顺序执行 / `Jump` / `Branch`），src、dst 同函数；
//! - `CallToReturn`：`Call` 指令 → 其下一条指令的影子边（先假装调用「立即返回」
//!   继续执行），src、dst 同函数，保证调用点之后仍有可达路径；
//! - `Call`：调用点 `(caller, call)` → 被调函数入口 `(callee, entry)`，跨函数；
//! - `Return`：被调函数每个返回点 `(callee, ret)` → 调用点的 `continuation`，
//!   跨函数；尾调用时指向调用指令自身（中继节点）。
//!
//! ### API 详解
//!
//! 公开方法都在 `impl ICFG` 上；除 `new` 外均为只读查询（`&self`），返回
//! `impl Iterator` 的方法都惰性求值、不分配新集合。
//!
//! - `new(program: &Program) -> ICFG`：从整个程序构建 ICFG（算法见下节），返回
//!   自持的 `ICFG` 值、不借用 `program`。构建后任何 IR 修改都会让图过期，必须
//!   重建（快照语义）。
//! - `entry_func(&self) -> Function`：入口函数，即 `program.get_main_function()`
//!   返回的 `main`。O(1)。
//! - `reachable_functions(&self) -> &FxHashSet<Function>`：BFS 从 `main` 到达的
//!   全部函数，**含外部声明函数**（被调用但无函数体）。死函数消除后它基本就是
//!   「活函数集合」。O(1) 取引用。
//! - `call_sites_of(&self, function) -> impl Iterator<Item = CallSite>`：`function`
//!   内全部调用点（`Call` + `TailCall`），按指令顺序。O(调用点数)。
//! - `callees_of(&self, function) -> &[Function]`：`function` 内全部调用点的
//!   callee 切片，与 `call_sites_of` 的 `callee` 字段一一对应。O(1) 取切片。
//! - `return_sites_of(&self, function) -> &[Inst]`：`function` 内全部返回指令
//!   （含尾调用指令——它同时是返回点）切片。O(1)。
//! - `outgoing_edges_of(&self, node) -> impl Iterator<Item = Edge>`：`node` 全部
//!   出边。`Normal`/`CallToReturn` 的 dst 取同函数指令；`Call`/`Return` 经
//!   `cross_edge_callsite_map` 反查调用点，把跨函数端解析为 callee（或 caller）。
//!   O(出度)；图中没有该节点（如死函数内指令）时返回空迭代器。
//! - `incoming_edges_of(&self, node) -> impl Iterator<Item = Edge>`：入边版本，
//!   与上者对称（`Call` 的 src 解析为 caller、`Return` 的 src 解析为 callee）。
//!   O(入度)。
//! - `call_site_at(&self, call: Node) -> Option<CallSite>`：给定**调用指令**节点
//!   返回它所属的调用点；不是调用点则 `None`。
//! - `call_site_before(&self, continuation: Node) -> Option<CallSite>`：给定
//!   **继续点**节点，返回「紧邻其前」的调用点。尾调用指令故意不在
//!   `callsite_by_continuation` 里（防与前置普通调用的 continuation 冲突），对它
//!   查询得 `None`。
//!
//! 私有 `call_site_by_index(index)` 是上述查询共用的「调用点号 → `CallSite`」取数
//! 函数。
//!
//! ### 构建算法（`ICFG::new`）
//!
//! 分两阶段：
//!
//! **第一阶段：函数内骨架（遍历 `program.function_layout()` 的全部函数）**
//!
//! 1. 跳过没有入口基本块的函数（外部声明，`layout().entry_bb().is_none()`）——
//!    它们没有指令，不产生边与索引；
//! 2. 对每个基本块：相邻指令两两建 `Normal` 边；`Call` 指令登记一个调用点
//!    （continuation = 下一条指令）并建 `CallToReturn` 边，同时写
//!    `callsite_by_call` / `callsite_by_continuation`；
//! 3. 终结符：`Return` → 记入 `return_sites`；`TailCall` → **既是调用点又是返回
//!    点**，以「哨兵 `continuation == call`」登记调用点，但不写
//!    `callsite_by_continuation`；`Jump`/`Branch` → 向目标块首指令建 `Normal` 边；
//! 4. 每个函数结束，把它的调用点 / 返回点区间记入 `call_sites_range` /
//!    `return_sites_range`。
//!
//! **第二阶段：跨函数边（从 `main` 出发的队列 BFS）**
//!
//! 1. 队列以 `main` 为根，`function_visited` 去重（每个函数只展开一次），最终
//!    即 `reachable_functions`；
//! 2. 弹出 `func`，逐个处理 `call_sites_range[func]` 里的调用点：callee 未访问
//!    且**非声明函数**才入队（声明函数进可达集但没有函数体可展开）；
//! 3. callee 是声明函数则跳过（不给它连跨函数边）；否则：
//!    - 建 `Call` 边：(caller, call) → (callee, entry)，「边号 → 调用点号」记入
//!      `cross_edge_callsite_map`；
//!    - 对 callee **每个**返回点建一条 `Return` 边：目标是普通调用的
//!      continuation（取该调用点 `CallToReturn` 边的 dst）；尾调用（哨兵
//!      `continuation == call`）则指向调用指令自身。每条 `Return` 边同样登记
//!      `cross_edge_callsite_map`；
//! 4. 队列清空即结束。可见每条 `Call` 边都连向 callee 的**全部**返回点（多对
//!    多），最坏边数 O(调用点总数 × 返回点总数)。
//!
//! 英文头注 *"Better remove unused function before building the icfg"* 的由来：
//! 第一阶段**扫描全部函数**——包括从 `main` 不可达的死函数，它们同样得到函数内
//! 边与索引区间；而第二阶段只从 `main` 展开，死函数不会获得任何跨函数边、也不
//! 进 `reachable_functions`。所以先跑 `opt/passes/dce.rs` 的死函数消除（基于
//! `call_graph` 判定可达性）再建图，可省掉对死代码的扫描、让边表与索引更紧凑；
//! 若保留死函数，图中会残留它们的函数内边（查询语义仍正确，属无害浪费，且
//! ipsccp 的边工作队列永远到不了那些节点）。
//!
//! ### 使用方清单
//!
//! grep 全仓库（`raana_ir/src` 及工作区其他 crate）实证，**当前唯一使用方**是
//! `opt/passes/ipsccp.rs`（过程间稀疏条件常量传播）：
//!
//! - 约 527 行：`icfg::ICFG::new(program)` 建图，供固定点工作队列使用；
//! - 约 840 / 878 / 946 / 951 行：`outgoing_edges_of(node)` 取节点出边，按
//!   `EdgeType` 分流——`Return` 边转发中继值（尾调用链 relay）、`Call` 边推送
//!   被调函数入口、其余边推入边工作队列；
//! - 约 844 / 887 行：`call_site_before(dst)` 把返回边落点解析回调用点（取其
//!   `.call` 节点收值）；对尾调用中继节点返回 `None`，值直接存入该节点；
//! - 约 108 行：直接引用 `icfg::{Edge, EdgeType}` 类型（ipsccp 自己也构造
//!   `EdgeType::Normal` 边来模拟分支）；
//! - 综上，ipsccp 只用到 `new` / `outgoing_edges_of` / `call_site_before` 三个
//!   API。
//!
//! 其余 API（`call_sites_of` / `callees_of` / `return_sites_of` /
//! `incoming_edges_of` / `call_site_at` / `entry_func` / `reachable_functions`）
//! 目前只出现在本模块自身的测试里，是为后续过程间 pass（如过程间死存储消除、
//! 常量参数特化）预留的通用查询。`opt.rs` 与 `cfg.rs` 文档中提及 ICFG 仅为说明
//! 性文字，非调用方。
//!
//! ### 正确性 / 边界
//!
//! - **快照**：构建后任何 IR 修改（增删指令 / 函数、改写调用）都会让图过期，
//!   必须重建；
//! - **覆盖范围**：跨函数边只覆盖 `main` 可达子图；死函数只有函数内边、无跨
//!   函数边、不在 `reachable_functions` 里，对其内部节点查询出 / 入边得空迭代器；
//! - **声明函数**：外部声明（无函数体）作为 callee 出现在 `reachable_functions`
//!   与调用点表里，但**没有** `Call`/`Return` 跨函数边——调用点只剩
//!   `CallToReturn` 边（测试 `external_call_is_reachable_without_cross_function_
//!   edges` 即验证此行为）；ipsccp 据此把声明函数调用处理为 Bottom；
//! - **尾调用**：`TailCall` 指令同时是返回点与调用点；其返回目标用
//!   「`continuation == call`」哨兵标记为**中继节点**（返回值先落在它上面，再由
//!   调用方继续向上转发），且故意不登记 `callsite_by_continuation`，避免与紧邻
//!   其前的普通 `Call` 的 continuation 映射冲突（见
//!   `tail_call_creates_call_and_return_edges` 测试）；
//! - **间接调用**：SysY 无函数指针，IR 里只有静态 `Call`/`TailCall`，图无需处理
//!   间接调用目标，因此是完备的；
//! - **多返回点**：callee 有多个 `Return` 时，每个返回点都向调用点的
//!   continuation 各连一条 `Return` 边。
//!
//! ### 验证
//!
//! - 本文件 `#[cfg(test)] mod tests`（约 386–584 行）共 3 个用例：
//!   `resolves_internal_call_and_return_edges`（普通调用：`Call` 边到 callee 入口、
//!   `Return` 边到 continuation、`call_site_at`/`call_site_before` 双向解析）、
//!   `external_call_is_reachable_without_cross_function_edges`（声明函数只进可达
//!   集、无跨函数边）、`tail_call_creates_call_and_return_edges`（尾调用哨兵
//!   continuation、中继节点的 `Call`/`Return` 双出边、`call_site_before` 返回
//!   `None`）；
//! - 使用方 ipsccp 的测试（`opt/passes/ipsccp/tests.rs`）间接覆盖 ICFG 的传播
//!   语义（常量实参沿 `Call` 边流入形参、返回值沿 `Return` 边流回调用点等）；
//! - 回归：`cargo test -p raana_ir`。
//!
use std::ops::Range;

use itertools::Itertools;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use crate::opt::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeType {
    Normal,
    Call,
    Return,
    CallToReturn,
}

pub type Indices = SmallVec<[usize; 2]>;

/// AoS version struct. Showing equivalent representation of a structure
#[derive(Debug, Clone, PartialEq, Eq, Copy, Hash)]
pub struct Edge {
    pub edge_type: EdgeType,
    pub src: Node,
    pub dst: Node,
}

#[derive(Debug, Default)]
pub struct EdgeTable {
    pub edge_types: Vec<EdgeType>,
    pub srcs: Vec<Inst>,
    pub dsts: Vec<Inst>,
}

impl EdgeTable {
    fn push(&mut self, edge_type: EdgeType, src: Inst, dst: Inst) {
        self.edge_types.push(edge_type);
        self.srcs.push(src);
        self.dsts.push(dst);
    }

    fn len(&self) -> usize {
        self.edge_types.len()
    }
}

pub struct ICFG {
    /// Entry function for a program. Usually `main` function.
    entry_func: Function,

    /// SoA form of Vec<Edge>,
    edges: EdgeTable,

    /// SoA form of Vec<CallSite>,
    call_sites: CallSiteTable,

    /// Continuous storage of return instruction.
    return_sites: Vec<Inst>,

    /// Sparse index used to resolve functions on cross-function edges.
    cross_edge_callsite_map: FxHashMap<usize, usize>,

    /// Index call and continuation nodes to their callsites.
    callsite_by_call: FxHashMap<Node, usize>,
    callsite_by_continuation: FxHashMap<Node, usize>,

    /// Index on continuous part of call sites.
    call_sites_range: FxHashMap<Function, Range<usize>>,

    /// Index on continuous part of return sites.
    return_sites_range: FxHashMap<Function, Range<usize>>,

    /// Store indices based on (Function, Inst) unique ID.
    /// The indices is used to index on edges.
    /// The forward one store all the indices of edges that have src == Inst
    /// The backward one store all the indices of edges that have dst == Inst
    forward: FxHashMap<Node, Indices>,
    backward: FxHashMap<Node, Indices>,

    /// All function reachable from main at comptime static estimate, including lib function.
    reachable_functions: FxHashSet<Function>,
}

impl ICFG {
    /// Get all callsites in a given function.
    pub fn call_sites_of(&self, function: Function) -> impl Iterator<Item = CallSite> {
        self.call_sites_range[&function]
            .clone()
            .map(|index| CallSite {
                call: self.call_sites.calls[index],
                caller: self.call_sites.callers[index],
                callee: self.call_sites.callees[index],
                continuation: self.call_sites.continuations[index],
            })
    }

    /// Get all callees of a given function.
    pub fn callees_of(&self, function: Function) -> &[Function] {
        &self.call_sites.callees[self.call_sites_range[&function].clone()]
    }

    /// Get all return instructions in a given function.
    pub fn return_sites_of(&self, function: Function) -> &[Inst] {
        &self.return_sites[self.return_sites_range[&function].clone()]
    }

    /// Get all outgoing edges of a node.
    pub fn outgoing_edges_of(&self, node: Node) -> impl Iterator<Item = Edge> {
        self.forward
            .get(&node)
            .into_iter()
            .flatten()
            .map(move |&index| {
                let edge_type = self.edges.edge_types[index];
                let dst_func = match edge_type {
                    EdgeType::Normal | EdgeType::CallToReturn => node.func,
                    EdgeType::Call => {
                        self.call_site_by_index(self.cross_edge_callsite_map[&index])
                            .callee
                    }
                    EdgeType::Return => {
                        self.call_site_by_index(self.cross_edge_callsite_map[&index])
                            .caller
                    }
                };
                Edge {
                    edge_type,
                    src: node,
                    dst: Node::new(dst_func, self.edges.dsts[index]),
                }
            })
    }

    /// Get all incoming edges of a node.
    pub fn incoming_edges_of(&self, node: Node) -> impl Iterator<Item = Edge> {
        self.backward
            .get(&node)
            .into_iter()
            .flatten()
            .map(move |&index| {
                let edge_type = self.edges.edge_types[index];
                let src_func = match edge_type {
                    EdgeType::Normal | EdgeType::CallToReturn => node.func,
                    EdgeType::Call => {
                        self.call_site_by_index(self.cross_edge_callsite_map[&index])
                            .caller
                    }
                    EdgeType::Return => {
                        self.call_site_by_index(self.cross_edge_callsite_map[&index])
                            .callee
                    }
                };
                Edge {
                    edge_type,
                    src: Node::new(src_func, self.edges.srcs[index]),
                    dst: node,
                }
            })
    }

    /// Get the callsite represented by a call node.
    pub fn call_site_at(&self, call: Node) -> Option<CallSite> {
        self.callsite_by_call
            .get(&call)
            .map(|&index| self.call_site_by_index(index))
    }

    /// Get the callsite immediately before a continuation node.
    pub fn call_site_before(&self, continuation: Node) -> Option<CallSite> {
        self.callsite_by_continuation
            .get(&continuation)
            .map(|&index| self.call_site_by_index(index))
    }

    fn call_site_by_index(&self, index: usize) -> CallSite {
        CallSite {
            call: self.call_sites.calls[index],
            caller: self.call_sites.callers[index],
            callee: self.call_sites.callees[index],
            continuation: self.call_sites.continuations[index],
        }
    }

    pub fn entry_func(&self) -> Function {
        self.entry_func
    }

    pub fn reachable_functions(&self) -> &FxHashSet<Function> {
        &self.reachable_functions
    }

    pub fn new(program: &Program) -> ICFG {
        let mut call_sites = CallSiteTable::default();
        let mut return_sites = vec![];
        let mut call_sites_range = FxHashMap::default();
        let mut return_sites_range = FxHashMap::default();
        let mut edges = EdgeTable::default();
        let mut forward: FxHashMap<Node, Indices> = FxHashMap::default();
        let mut backward: FxHashMap<Node, Indices> = FxHashMap::default();
        let mut entries = FxHashMap::default();
        let mut cross_edge_callsite_map = FxHashMap::default();
        let mut callsite_by_call = FxHashMap::default();
        let mut callsite_by_continuation = FxHashMap::default();

        fn insert_edge(
            ty: EdgeType,
            src: Node,
            dst: Node,
            edges: &mut EdgeTable,
            forward: &mut HashMap<Node, SmallVec<[usize; 2]>>,
            backward: &mut HashMap<Node, SmallVec<[usize; 2]>>,
        ) -> usize {
            let edge_len = edges.len();
            edges.push(ty, src.inst, dst.inst);

            forward.entry(src).or_default().push(edge_len);
            backward.entry(dst).or_default().push(edge_len);
            edge_len
        }

        for &func in program.function_layout() {
            let call_sites_start = call_sites.len();
            let return_sites_start = return_sites.len();
            let func_data = program.func_data(func);
            if func_data.layout().entry_bb().is_none() {
                continue;
            }
            for bb_layout in func_data.layout().basicblocks() {
                let first_inst = *bb_layout.insts().get_first().unwrap();
                entries.entry(func).or_insert(first_inst);
                for (&inst, &next) in bb_layout.insts().iter().tuple_windows() {
                    let inst_data = func_data.inst_data(inst);
                    let edge_type = match inst_data.kind() {
                        InstKind::Call(call) => {
                            let callsite_index = call_sites.len();
                            call_sites.push(inst, func, call.callee(), next);
                            callsite_by_call.insert(Node::new(func, inst), callsite_index);
                            callsite_by_continuation.insert(Node::new(func, next), callsite_index);
                            EdgeType::CallToReturn
                        }
                        _ => EdgeType::Normal,
                    };
                    insert_edge(
                        edge_type,
                        Node::new(func, inst),
                        Node::new(func, next),
                        &mut edges,
                        &mut forward,
                        &mut backward,
                    );
                }
                let last_inst = *bb_layout.insts().get_last().unwrap();
                let inst_data = func_data.inst_data(last_inst);

                let mut add_edge = |target| {
                    let jump_to = *func_data
                        .layout()
                        .basicblock(target)
                        .insts()
                        .get_first()
                        .unwrap();
                    insert_edge(
                        EdgeType::Normal,
                        Node::new(func, last_inst),
                        Node::new(func, jump_to),
                        &mut edges,
                        &mut forward,
                        &mut backward,
                    );
                };
                match inst_data.kind() {
                    InstKind::Return(..) => {
                        return_sites.push(last_inst);
                    }
                    InstKind::TailCall(tc) => {
                        // A tail call is simultaneously a return site (the caller's
                        // frame is gone) and a call site (control transfers to the
                        // callee). Record it as a call site with a sentinel
                        // `continuation == call` so the cross-function pass can
                        // distinguish it from a regular call (which has a real
                        // successor instruction as continuation).
                        return_sites.push(last_inst);
                        let callsite_index = call_sites.len();
                        call_sites.push(last_inst, func, tc.callee(), last_inst);
                        callsite_by_call.insert(Node::new(func, last_inst), callsite_index);
                        // Deliberately NOT inserted into `callsite_by_continuation`:
                        // a regular `Call` immediately preceding this terminator
                        // would already map `last_inst` as its continuation, and
                        // overwriting that entry would break the regular call's
                        // return-edge resolution.
                    }
                    InstKind::Jump(jump) => {
                        add_edge(jump.target());
                    }
                    InstKind::Branch(branch) => {
                        for target in [branch.t_target(), branch.f_target()] {
                            add_edge(target);
                        }
                    }
                    _ => unreachable!(),
                }
            }
            call_sites_range.insert(func, call_sites_start..call_sites.len());
            return_sites_range.insert(func, return_sites_start..return_sites.len());
        }

        let main_function = program.get_main_function();

        let mut function_queue = VecDeque::new();
        let mut function_visited = FxHashSet::default();

        function_queue.push_back(main_function);
        function_visited.insert(main_function);

        while !function_queue.is_empty() {
            let func = function_queue.pop_front().unwrap();
            let call_indices = &call_sites_range[&func];
            for call_index in call_indices.clone() {
                let call = call_sites.calls[call_index];
                let callee = call_sites.callees[call_index];
                // newly added entry.
                if function_visited.insert(callee) && !program.func_data(callee).layout().is_decl()
                {
                    function_queue.push_back(callee);
                }
                let callee_data = program.func_data(callee);
                if callee_data.layout().is_decl() {
                    continue;
                }
                let return_sites_indices = &return_sites_range[&callee];
                let returnsites = &return_sites[return_sites_indices.clone()];
                let entry = entries[&callee];
                let call_edge = insert_edge(
                    EdgeType::Call,
                    Node::new(func, call),
                    Node::new(callee, entry),
                    &mut edges,
                    &mut forward,
                    &mut backward,
                );
                cross_edge_callsite_map.insert(call_edge, call_index);
                // For a regular call, the continuation is the destination of its
                // CallToReturn edge (the instruction right after the call). For a
                // tail call there is no such edge — the sentinel
                // `continuation == call` marks it, and the callee's return value
                // lands at the tail-call instruction itself (the relay node).
                let return_to = if call_sites.continuations[call_index] == call {
                    call
                } else {
                    let call_to_return = *forward[&Node::new(func, call)].first().unwrap();
                    edges.dsts[call_to_return]
                };
                for &return_site in returnsites {
                    let edge_len = edges.len();
                    insert_edge(
                        EdgeType::Return,
                        Node::new(callee, return_site),
                        Node::new(func, return_to),
                        &mut edges,
                        &mut forward,
                        &mut backward,
                    );
                    cross_edge_callsite_map.insert(edge_len, call_index);
                }
            }
        }

        ICFG {
            entry_func: main_function,
            edges,
            call_sites,
            return_sites,
            cross_edge_callsite_map,
            callsite_by_call,
            callsite_by_continuation,
            call_sites_range,
            return_sites_range,
            forward,
            backward,
            reachable_functions: function_visited,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        Type,
        builder::{BasicBlockBuilder, LocalInstBuilder},
    };

    fn append_returning_block(program: &mut Program, function: Function, value: i32) -> Inst {
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let value = data.new_local_inst().integer(value);
        let ret = data.new_local_inst().ret(Some(value));
        data.layout_mut().insert_inst(entry, ret);
        ret
    }

    #[test]
    fn resolves_internal_call_and_return_edges() {
        let mut program = Program::new();
        let callee = program.new_function(Type::get_i32(), "callee".into(), vec![]);
        let callee_return = append_returning_block(&mut program, callee, 7);
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);

        let (call, continuation) = {
            let data = program.func_data_mut(main);
            let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
            data.layout_mut().push_bb_back(entry);
            let call = data
                .new_local_inst()
                .call_with_type(callee, vec![], Type::get_i32());
            let continuation = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(entry, call);
            data.layout_mut().insert_inst(entry, continuation);
            (call, continuation)
        };

        let icfg = ICFG::new(&program);
        let call_node = Node::new(main, call);
        let continuation_node = Node::new(main, continuation);
        let callsite = icfg.call_site_at(call_node).unwrap();
        assert_eq!(callsite.call, call);
        assert_eq!(callsite.caller, main);
        assert_eq!(callsite.callee, callee);
        assert_eq!(callsite.continuation, continuation);
        assert_eq!(icfg.call_site_before(continuation_node), Some(callsite));

        let outgoing = icfg.outgoing_edges_of(call_node).collect::<Vec<_>>();
        assert_eq!(outgoing.len(), 2);
        let call_edge = outgoing
            .iter()
            .copied()
            .find(|edge| edge.edge_type == EdgeType::Call)
            .unwrap();
        assert_eq!(call_edge.src, call_node);
        assert_eq!(call_edge.dst, Node::new(callee, callee_return));
        assert!(
            icfg.incoming_edges_of(Node::new(callee, callee_return))
                .any(|edge| edge == call_edge)
        );

        let return_edge = icfg
            .outgoing_edges_of(Node::new(callee, callee_return))
            .next()
            .unwrap();
        assert_eq!(return_edge.edge_type, EdgeType::Return);
        assert_eq!(return_edge.src, Node::new(callee, callee_return));
        assert_eq!(return_edge.dst, continuation_node);
        assert_eq!(icfg.call_site_before(return_edge.dst), Some(callsite));

        let incoming = icfg
            .incoming_edges_of(continuation_node)
            .collect::<Vec<_>>();
        assert_eq!(incoming.len(), 2);
        assert!(incoming.contains(&return_edge));
    }

    #[test]
    fn external_call_is_reachable_without_cross_function_edges() {
        let mut program = Program::new();
        let external = program.new_function(Type::get_unit(), "putint".into(), vec![]);
        let main = program.new_function(Type::get_unit(), "main".into(), vec![]);

        let call = {
            let data = program.func_data_mut(main);
            let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
            data.layout_mut().push_bb_back(entry);
            let call = data
                .new_local_inst()
                .call_with_type(external, vec![], Type::get_unit());
            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(entry, call);
            data.layout_mut().insert_inst(entry, ret);
            call
        };

        let icfg = ICFG::new(&program);
        assert!(icfg.reachable_functions().contains(&external));
        let edges = icfg
            .outgoing_edges_of(Node::new(main, call))
            .collect::<Vec<_>>();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].edge_type, EdgeType::CallToReturn);
        assert_eq!(edges[0].src, Node::new(main, call));
        assert_eq!(icfg.call_site_at(edges[0].src).unwrap().callee, external);
    }

    #[test]
    fn tail_call_creates_call_and_return_edges() {
        let mut program = Program::new();

        // g(x: i32) -> i32 — returns x
        let g = program.new_function(Type::get_i32(), "g".into(), vec![Type::get_i32()]);
        let (g_ret, g_entry) = {
            let data = program.func_data_mut(g);
            let entry = data.add_entry_block();
            let param = data.bb_data(entry).params()[0];
            let ret = data.new_local_inst().ret(Some(param));
            data.layout_mut().insert_inst(entry, ret);
            (
                ret,
                *data
                    .layout()
                    .entry_bb()
                    .unwrap()
                    .insts()
                    .get_first()
                    .unwrap(),
            )
        };

        // f(x: i32) -> i32 — tail-calls g(x)
        let f = program.new_function(Type::get_i32(), "f".into(), vec![Type::get_i32()]);
        let tail_call = {
            let data = program.func_data_mut(f);
            let entry = data.add_entry_block();
            let param = data.bb_data(entry).params()[0];
            let tc = data.new_local_inst().tail_call(g, vec![param]);
            data.layout_mut().insert_inst(entry, tc);
            tc
        };

        // main() -> i32 — calls f(42)
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let (call_f, main_cont) = {
            let data = program.func_data_mut(main);
            let entry = data.add_entry_block();
            let val = data.new_local_inst().integer(42);
            let call = data
                .new_local_inst()
                .call_with_type(f, vec![val], Type::get_i32());
            let ret = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(entry, call);
            data.layout_mut().insert_inst(entry, ret);
            (call, ret)
        };

        let icfg = ICFG::new(&program);

        // The tail call is recorded as a call site.
        let cs = icfg.call_site_at(Node::new(f, tail_call)).unwrap();
        assert_eq!(cs.caller, f);
        assert_eq!(cs.callee, g);

        // Call edge: (f, tail_call) -> (g, g_entry)
        let f_out = icfg
            .outgoing_edges_of(Node::new(f, tail_call))
            .collect::<Vec<_>>();
        let call_edge = f_out
            .iter()
            .find(|e| e.edge_type == EdgeType::Call)
            .unwrap();
        assert_eq!(call_edge.dst, Node::new(g, g_entry));

        // Return edge (relay input): (g, g_ret) -> (f, tail_call)
        let g_out = icfg
            .outgoing_edges_of(Node::new(g, g_ret))
            .collect::<Vec<_>>();
        let return_edge = g_out
            .iter()
            .find(|e| e.edge_type == EdgeType::Return)
            .unwrap();
        assert_eq!(return_edge.dst, Node::new(f, tail_call));

        // The tail-call node is deliberately absent from
        // callsite_by_continuation (collision avoidance with a preceding
        // regular Call whose continuation would be this terminator).
        assert_eq!(icfg.call_site_before(Node::new(f, tail_call)), None);

        // The tail-call node is also a return site of f, so f's caller sees a
        // Return edge from it (relay output).
        let relay = f_out
            .iter()
            .find(|e| e.edge_type == EdgeType::Return && e.dst.func == main)
            .unwrap();
        assert_eq!(relay.dst, Node::new(main, main_cont));
    }
}
