//! # 调用图分析（CallGraph）
//!
//! 调用图是一张「函数 → 函数」的有向图：节点是 `Function`，每条边代表一条
//! **调用关系**——caller 内部某条 `Call`/`TailCall` 指令调用了 callee。它是
//! 过程间分析（interprocedural analysis）的基础设施：死函数消除要知道「从
//! `main` 出发能到达哪些函数」，特化（specialize）与内联（inline）要回答「这个
//! 函数被谁调用、调用点在哪、是否在递归环里」。本模块把这张图建成**可随机访问
//! 的边表**（三个哈希表），并提供度数查询与可达性查询（BFS）。
//!
//! 与 `cfg`/`dom_tree` 等分析一致，调用图是**快照**：它只反映构建时 IR 的调用
//! 关系，任何 pass 增删调用指令或函数之后，都必须用 `CallGraph::new` 重建。
//!
//! ## 数据结构
//!
//! - `CallGraph`：整张调用图，内部三张表：
//!   - `call_edges`（`CallEdgeTable`，私有）：**并行数组**式边表——`callsites`
//!     与 `callees` 两个 `Vec` 等长，边号 `i` 即 `(callsites[i], callees[i])`，
//!     边号全局唯一；
//!   - `call_site_indices`：`Function → Vec<边号>`，按**调用方**索引，`func`
//!     的所有出边；
//!   - `callee_indices`：`Function → Vec<边号>`，按**被调方**索引，所有指向
//!     `func` 的入边。
//! - `CallEdge`：一条边的视图（`callsite: Node` + `callee: Function`），由
//!   `CallEdgeTable::get` 按边号取出。
//! - `Node`（`opt::utils::global_handle`）：`(func: Function, inst: Inst)`
//!   二元组，唯一标识一条指令——在这里用来标识**调用点**。
//!
//! ## API 详解
//!
//! 全部方法都在 `impl CallGraph` 上，均为只读查询（`&self`）；返回 `impl
//! Iterator` 的方法都**惰性求值**，不分配新集合。
//!
//! - `new(p: &Program) -> CallGraph`：从程序构建调用图（算法见下节）。复杂度
//!   O(可达函数的指令总数)，每条调用指令的处理为 O(1)。
//! - `callees_in(&self, func) -> impl Iterator<Item = Function>`：`func` 内部
//!   全部调用点（`Call` + `TailCall`）的被调函数，按指令顺序迭代；同一 callee
//!   被多处调用会出现**多次**。复杂度 O(出度)。`func` 不在图中时返回空迭代器。
//! - `callsites_in(&self, func) -> impl Iterator<Item = Node>`：`func` 内全部
//!   调用点的位置（`Node`），与 `callees_in` 一一对应（第 i 个调用点调用第 i
//!   个 callee）。
//! - `incoming_callsites_of(&self, func) -> impl Iterator<Item = Node>`：所有
//!   调用 `func` 的调用点（入边上的 `Node`），跨 caller 按建图顺序。复杂度
//!   O(入度)。
//! - `in_degree_of(&self, func) -> usize`：入度 = 指向 `func` 的调用点个数
//!   （同一个 caller 多处调用会计多次）。O(1)。
//! - `out_degree_of(&self, func) -> usize`：出度 = `func` 内调用点个数。O(1)。
//! - `out_degrees_of_all(&self) -> impl Iterator<Item = (Function, usize)>`：
//!   所有**至少有一条出边**的函数的出度（叶子函数不出现）。
//! - `in_degrees_of_all(&self) -> impl Iterator<Item = (Function, usize)>`：
//!   所有**至少被调用过一次**的函数的入度。
//! - `callees(&self) -> impl Iterator<Item = Function>`：所有充当过被调方的函数
//!   （`callee_indices` 的键，即入度 ≥ 1 的集合）——常用来枚举「值得特化」的
//!   候选（见 `specialize` 的使用）。
//! - `reaches(&self, from, target) -> bool`：`from` 能否沿调用边（长度 ≥ 1
//!   的路径）到达 `target`。BFS 实现，最坏 O(V + E)。
//!
//! ## 构建算法
//!
//! `CallGraph::new` 并非把整程序所有函数之间的调用全部建出来，而是从
//! `p.get_main_function()` 出发做**队列 BFS**：
//!
//! 1. 队列以 `main` 为根，`func_visited` 记录已入队函数（每个函数只展开一次）；
//! 2. 弹出 `func`，遍历 `p.func_data(func)` 的每个基本块的每条指令；只认
//!    `InstKind::Call(call) => call.callee()` 与
//!    `InstKind::TailCall(tailcall) => tailcall.callee()`，其余指令跳过；
//! 3. 每命中一条调用指令就登记一条边（`Node::new(func, inst)` → callee），
//!    并往两个索引表各追加边号；若 callee 尚未访问则入队；
//! 4. 队列清空即结束。边号按登记顺序分配。
//!
//! 因此**图中只包含 `main` 可达子图**——从 `main` 永远调不到的函数（包括互相
//! 调用但不经 `main` 的死代码）不会出现在图里，对它们的任何查询都返回
//! 「空 / 0 / false」。死函数消除正是利用这一点（见使用方清单）。
//!
//! ## 使用方清单
//!
//! - `opt/passes/dce.rs`（`DeadFunctionElimination::run`，约 1072 行）：
//!   `CallGraph::new` 建图后，以 `get_main_function` 为根做 BFS（用
//!   `callees_in` 展开出边）收集 `reachable` 集合，不在其中的函数判死删除。
//! - `opt/passes/specialize.rs`（`Specialize`，约 75 行）：`CallGraph::new`
//!   建图；用 `callees()` 枚举特化候选；`reaches(candidate, candidate)` 为真
//!   说明候选在递归环里，跳过（自递归特化会无限克隆）；用
//!   `incoming_callsites_of(candidate)` 定位所有待替换的调用点。
//! - `opt/passes/inline.rs`（`Inline`，约 251 行）：`CallGraph::new` 建图；
//!   `reaches(callee, callee)` 排除递归函数；`incoming_callsites_of(callee)`
//!   收集内联候选调用点；再用 `reaches(callee, callsite.func)` 排除「把
//!   `callee` 内联进会被 `callee` 调用的函数」的情形（否则内联会无限增长）。
//!
//! ## 正确性 / 边界
//!
//! - **快照**：任何 IR 修改（增删函数、增删或改写调用指令）都会让图过期，必须
//!   重建；
//! - **覆盖范围**：只含 `main` 可达子图（见「构建算法」）；
//! - **声明函数**：只有声明没有 body 的函数（`FunctionLayout::is_decl()`，即
//!   无基本块）仍会作为 callee 入图（出现在 `callees()`/`callees_in`/
//!   `incoming_callsites_of` 里），但它没有 body，展开时零指令、无出边；
//! - **尾调用**：`InstKind::TailCall` 与 `InstKind::Call` 一样计为一条边；
//! - **自递归 / 互递归**：`reaches(f, f)` 为真 ⇔ `f` 在某个递归环中（直接自
//!   调用或经别的函数绕回来）。注意 `reaches` 不检查起点本身：`f` 没有任何
//!   出边时 `reaches(f, f)` 为 `false`；
//! - **多条边**：同一函数内多处调用同一 callee 产生多条边，`callees_in` 会
//!   重复返回，度数按调用点计数而非按 caller 计数；
//! - **间接调用**：SysY 无函数指针，IR 里只有静态调用，本图无需处理间接调用
//!   目标，因此是完备的。
//!
//! ## 验证
//!
//! 本文件没有独立单元测试；正确性由使用方 pass 的测试间接覆盖（如 `dce.rs`
//! 的 `mod tests` 中 `DeadFunctionElimination` 用例），以及
//! `cargo test -p raana_ir` 全量回归。
//!
use rustc_hash::{FxBuildHasher, FxHashMap};

use crate::opt::prelude::*;

pub struct CallGraph {
    call_edges: CallEdgeTable,
    call_site_indices: FxHashMap<Function, Vec<usize>>,
    callee_indices: FxHashMap<Function, Vec<usize>>,
}

pub struct CallEdge {
    callsite: Node,
    callee: Function,
}

#[derive(Default)]
struct CallEdgeTable {
    callsites: Vec<Node>,
    callees: Vec<Function>,
}

impl CallEdgeTable {
    fn len(&self) -> usize {
        self.callsites.len()
    }

    fn push(&mut self, call_site: Node, callee: Function) {
        self.callsites.push(call_site);
        self.callees.push(callee);
    }

    fn get(&self, i: usize) -> CallEdge {
        CallEdge {
            callsite: self.callsites[i],
            callee: self.callees[i],
        }
    }
}

impl CallGraph {
    /// Return all the callees of a function
    /// a.k.a all the functions that would be called inside the given function
    pub fn callees_in(&self, func: Function) -> impl Iterator<Item = Function> + '_ {
        self.call_site_indices
            .get(&func)
            .into_iter()
            .flatten()
            .map(|&idx| self.call_edges.get(idx).callee)
    }

    /// Return all the callsites of a function
    /// a.k.a all the instruction of `InstKind::Call` inside the given function
    pub fn callsites_in(&self, func: Function) -> impl Iterator<Item = Node> + '_ {
        self.call_site_indices
            .get(&func)
            .into_iter()
            .flatten()
            .map(|&idx| self.call_edges.get(idx).callsite)
    }

    /// Return all the callsites that call the given function.
    /// a.k.a all the call instruction which callee is given function.
    pub fn incoming_callsites_of(&self, func: Function) -> impl Iterator<Item = Node> + '_ {
        self.callee_indices
            .get(&func)
            .into_iter()
            .flatten()
            .map(|&idx| self.call_edges.get(idx).callsite)
    }

    pub fn in_degree_of(&self, func: Function) -> usize {
        self.callee_indices.get(&func).map_or(0, Vec::len)
    }

    pub fn out_degree_of(&self, func: Function) -> usize {
        self.call_site_indices.get(&func).map_or(0, Vec::len)
    }

    pub fn out_degrees_of_all(&self) -> impl Iterator<Item = (Function, usize)> {
        self.call_site_indices
            .iter()
            .map(|(&func, indices)| (func, indices.len()))
    }

    pub fn callees(&self) -> impl Iterator<Item = Function> {
        self.callee_indices.keys().copied()
    }

    pub fn in_degrees_of_all(&self) -> impl Iterator<Item = (Function, usize)> {
        self.callee_indices
            .iter()
            .map(|(&func, indices)| (func, indices.len()))
    }

    pub fn reaches(&self, from: Function, target: Function) -> bool {
        let mut queue = VecDeque::from([from]);
        let mut visited = HashSet::from_iter([from]);
        while let Some(function) = queue.pop_front() {
            for callee in self.callees_in(function) {
                if callee == target {
                    return true;
                }
                if visited.insert(callee) {
                    queue.push_back(callee);
                }
            }
        }
        false
    }

    pub fn new(p: &Program) -> CallGraph {
        let main_function = p.get_main_function();
        let mut func_queue = VecDeque::with_capacity(16);
        let mut func_visited =
        // idk why FxHashSet do not provide FxHashSet::with_capacity() so:
            HashSet::with_capacity_and_hasher(p.function_layout().len(), FxBuildHasher);
        func_queue.push_back(main_function);
        func_visited.insert(main_function);
        let mut call_edges = CallEdgeTable::default();
        let mut call_site_indices: FxHashMap<Function, Vec<usize>> = FxHashMap::default();
        let mut callee_indices: FxHashMap<Function, Vec<usize>> = FxHashMap::default();

        let mut push_edge = |call_site: Node, callee| {
            call_site_indices
                .entry(call_site.func)
                .or_default()
                .push(call_edges.len());
            callee_indices
                .entry(callee)
                .or_default()
                .push(call_edges.len());
            call_edges.push(call_site, callee);
        };

        while !func_queue.is_empty() {
            let func = func_queue.pop_front().unwrap();
            let data = p.func_data(func);
            for bb_layout in data.layout().basicblocks() {
                for &inst in bb_layout.insts() {
                    let callee = match data.inst_data(inst).kind() {
                        InstKind::Call(call) => call.callee(),
                        InstKind::TailCall(tailcall) => tailcall.callee(),
                        _ => {
                            continue;
                        }
                    };
                    push_edge(Node::new(func, inst), callee);
                    if func_visited.insert(callee) {
                        func_queue.push_back(callee);
                    }
                }
            }
        }

        CallGraph {
            call_edges,
            call_site_indices,
            callee_indices,
        }
    }
}
