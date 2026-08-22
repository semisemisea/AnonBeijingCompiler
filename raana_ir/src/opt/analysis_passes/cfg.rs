//! # CFG 分析：数据驱动的控制流图基础设施
//!
//! 本模块为优化管线提供**控制流图（CFG）的构建与遍历顺序**基础设施：
//! `build_cfg_both` 从函数 layout 出发同时构建前向图与反图（前驱图），
//! `rpo_path` 在图上求逆后序（RPO）遍历。它是**数据驱动**的——图完全由
//! 每条基本块的终结指令（terminator）推导而来，自身不持有任何与 IR 的
//! 联动状态；支配树、自然循环等更上层的分析都建立在这套图之上。
//!
//! **结构边 vs 逻辑边**（约定见 `docs/Convention.md` 第二节）：**结构边**
//! 是唯一的 `(源, 目标)` 二元组，用于图的遍历、支配与自然循环发现；
//! **逻辑边**是一条 `Jump` 边或 `Branch` 的某一个 arm（每个 arm 携带各自
//! 的按位参数向量）。当一条分支的两个 arm 指向同一块时，结构边只有一条，
//! 但逻辑边仍有两条——Phi 与 block 参数分析必须逐 arm 检查。本模块只
//! 处理结构边。
//!
//! ## 与 `opt::utils::cfg::CFG` 的关系（重要）
//!
//! 本模块是**遗留（legacy）CFG 助手**：图以稠密的 `BId`（`usize` 块编号）
//! 为节点键，编号由调用方传入的 `IDAllocator` 分配。新写的控制流分析
//! 应改用 `opt::utils::cfg::CFG`（`raana_ir/src/opt/utils/cfg.rs`）——它
//! 以真实的 `BasicBlock` 句柄为节点身份，直接暴露 `entry` / `blocks`
//! （RPO 序）/ `is_acyclic` / `successors_of` / `predecessors_of` /
//! `edges` 等查询，并在 debug 构建下自校验（`verify`）。按
//! `docs/Convention.md` 第一节，遗留稠密块编号只是 SSA 等遗留客户的实现
//! 细节，新 API 不得暴露或混用这些编号。
//!
//! ## 核心 API 详解
//!
//! - `build_cfg_both(data, bb_alloc) -> (CFGGraph, CFGGraph)`：构建函数的
//!   **前向图**与**反图**（第二个返回值即前驱图）。`CFGGraph` 是
//!   `HashMap<BId, Vec<BId>>`，键为源块编号、值为后继（或前驱）块编号
//!   列表。算法从 `data.layout().entry_bb().unwrap()` 出发做迭代 DFS
//!   （`Visit::Enter` / `Visit::FalseArm` 双事件栈），对每个首次访问的块
//!   用 `get_terminator_inst` 取终结指令并按种类分发：
//!   - `Jump`：登记一条 `(源, 目标)` 结构边，继续 DFS 目标；
//!   - `Branch`：立即登记 `(源, 真目标)` 边并 DFS 真目标；假目标连同源块
//!     编号经 `FalseArm` 事件延迟处理（两个 arm 共用同一源块，边统一登记
//!     进 `graph[源]`，真 arm 在前、假 arm 在后）；
//!   - `Return` / `TailCall`：块无后继，仅登记空邻接表；
//!   - 其余终结指令 `unreachable!()`（IR 不变量：可达块只以这四类收尾）。
//!   图**只包含从入口可达的块**，不可达的 layout 块不进图、不参与支配/
//!   循环分析（`docs/Convention.md`）；反图保证入口块 0 的邻接表一定存在
//!   （`prece.entry(0).or_default()`），供支配计算读取。块编号由
//!   `bb_alloc.check_or_alloc_id_same` 惰性分配、从 0 递增——入口块是
//!   第一个访问的块，故必得编号 0（`ssa.rs` 断言 `rpo_path[0] == 0`）；
//!   同一 `IDAllocator` 可在多次调用间复用，保证同一块编号稳定。文件内
//!   的两个 TODO 提示：结果可以缓存；可按前向/后向分离以支持更多优化。
//! - `rpo_path(g: &CFGGraph) -> GPath`：求图的**逆后序**（RPO）。用
//!   `Enter` / `Exit` 双事件栈模拟 DFS：`Enter` 首次访问节点并入 `Set`，
//!   `Exit` 在全部后继处理完后把节点压入 `path`，最后整体 `reverse()`。
//!   RPO 的关键性质是**每个节点的前驱都排在它之前**（无环子图严格成立），
//!   入口块因此位于路径首（下标 0）；`Set` 去重使带环图也能安全遍历。
//!   返回的 `GPath = Vec<BId>` 常作为支配计算（`dom_tree::idom`）与
//!   值编号（GVN）的访问顺序。
//! - 相关类型（定义在 `opt::utils::type_alias`）：`CFGGraph`、
//!   `GPath = Vec<BId>`、`BId = usize`、`Set = HashSet<BId>`
//!   （`rustc_hash::FxHashSet`）。
//!
//! ## 下游分析入口（配套基础设施）
//!
//! 以下模块与本模块共同构成 CFG 之上的分析栈（定义在别处，不属本文件）：
//!
//! - `opt::analysis_passes::dom_tree`：`idom(prece, rpo)` 以反图 + RPO
//!   求立即支配者映射（`IDomMap`），`build_dominance_tree` 把它展开成
//!   支配树（`DomTree`）；新实现 `dom_tree::v2::DominanceTree`
//!   （`from_cfg` / `new`）以 `BasicBlock` 为身份，`dominates` 查询 O(1)；
//! - `opt::analysis_passes::loop_analysis`：自然循环发现。入口
//!   `LoopAnalysis::new(data)`（内部先 `CFG::new`）或
//!   `LoopAnalysis::from_cfg(cfg)`，返回 `(CFG, DominanceTree,
//!   LoopAnalysis)` 三元组；循环由支配回边识别，`loops()` 按尺寸从小到大
//!   排序，`min_loop_contain` / `containing_loops` / `parent_loop` 支持从
//!   内到外遍历嵌套循环，debug 构建下断言图可归约（`assert_reducible`）；
//! - `opt::utils::cfg::CFG`：见上文"关系"一节，新代码的首选。
//!
//! ## 快照语义（必须遵守）
//!
//! **CFG、支配、自然循环、IV 分析全部是快照**（`docs/Convention.md`
//! "CFG Mutation" 一节）：本模块返回的图以及下游的支配树/循环分析都只是
//! 构建时刻对 IR 的一次性拷贝/推导，**不随 IR 更新**。任何对块、终结指令、
//! 跳转目标、逻辑边参数或可达性的修改都会使全部依赖快照失效——pass 改写
//! CFG 之后必须整体重建 CFG 与支配/循环分析，严禁继续使用旧快照。arena +
//! 下标句柄的设计正是为了让"重建"廉价：快照可以随时整体重算。
//!
//! ## 使用方清单（grep 全仓库确认，`raana_ir/src`）
//!
//! 直接使用本文件两个函数的遗留客户：
//! - `opt/passes/ssa.rs`：`build_cfg_both` + `rpo_path` → `idom` →
//!   `build_dominance_tree`，构造支配树/支配前沿以做 SSA 构造；
//! - `opt/passes/dce.rs`：`build_cfg_both` 取反图，反复删除"没有前驱"的
//!   不可达块直到删不动（另用 `CFG::new` 快照做参数转发）；
//! - `opt/passes/gvn.rs`：`build_cfg_both` + `rpo_path`，RPO 序做值编号；
//! - `opt/passes/scalar_global_promotion.rs`：`build_cfg_both` + `rpo_path`
//!   + `idom` + `build_dominance_tree`，按支配关系决定全局变量提升范围。
//!
//! 使用新 CFG/支配/循环分析栈的 pass（`LoopAnalysis::from_cfg` /
//! `LoopAnalysis::new` / `CFG::new` / `DominanceTree`）：
//! `passes/loop_unroll`、`passes/licm`、`passes/pointer_strength_reduction`
//! （PSR）、`passes/column_major`、`passes/blocked_reduction`、
//! `passes/reduction_unroll`、`passes/matmul_interchange`、`passes/mod_fold`、
//! `passes/invariant_reduction_hoisting`（以上经 `from_cfg`）、
//! `passes/gvn_pre` 与 `passes/if_conversion`（支配树）、`passes/inline`、
//! `passes/recursive_memoize`、`analysis_passes/induction_variable`、
//! `analysis_passes/range`、`opt/utils/preheader`（以上经 `new`）；
//! `passes/simplify_cfg` 在测试里用 `CFG::new` 做健全性断言。
//! 注意：`passes/rotate_loops` **不**依赖本分析栈（只做 layout 旋转，
//! grep 无命中）；`passes/ipsccp` 用的是过程间 `ICFG`，与函数内 CFG 无关。
//!
//! ## 验证
//!
//! 本文件自身没有 `#[cfg(test)]` 测试（纯函数小工具）；正确性由下游覆盖：
//! `analysis_passes/dom_tree.rs` 与 `analysis_passes/loop_analysis.rs` 的
//! inline 测试（支配关系、回边、可归约性、嵌套循环），以及各使用方 pass
//! 的 inline 单测（如 `passes/licm/tests.rs`、`passes/pointer_strength_
//! reduction/tests.rs` 直接构造 `LoopAnalysis::new`）。回归入口：
//! `cargo test -p raana_ir`。debug 构建下 `CFG::new` 还会运行 `verify`
//! 自校验（RPO 首块为入口、邻接表与边集一致等），`LoopAnalysis::from_cfg`
//! 会断言可归约性。
//!
use rustc_hash::FxHashSet as HashSet;

use log::debug;

use crate::{
    ir::{BasicBlock, FunctionData, InstKind, arena::Arena},
    opt::utils::{IDAllocator, get_terminator_inst, type_alias::*},
};

pub fn rpo_path(g: &CFGGraph) -> GPath {
    #[derive(Clone, Copy)]
    enum Visit {
        Enter(BId),
        Exit(BId),
    }

    let mut path = Vec::new();
    let mut visited = Set::default();
    let mut stack = vec![Visit::Enter(0)];
    while let Some(visit) = stack.pop() {
        match visit {
            Visit::Enter(node) => {
                if !visited.insert(node) {
                    continue;
                }
                stack.push(Visit::Exit(node));
                for &successor in g[&node].iter().rev() {
                    if !visited.contains(&successor) {
                        stack.push(Visit::Enter(successor));
                    }
                }
            }
            Visit::Exit(node) => path.push(node),
        }
    }
    path.reverse();
    debug!("Graph/Path: {:?} {:?}", g, path);
    path
}

// TODO: We can do cache there.
// TODO: Do forward/backward seperation to allow further more optimization.
pub fn build_cfg_both(
    data: &FunctionData,
    bb_alloc: &mut IDAllocator<BasicBlock, BId>,
) -> (CFGGraph, CFGGraph) {
    #[derive(Clone, Copy)]
    enum Visit {
        Enter(BasicBlock),
        FalseArm(BId, BasicBlock),
    }

    // <a,b> in set E when a can directly jump to b
    let mut graph = CFGGraph::default();
    // reverse graph
    let mut prece = CFGGraph::default();
    prece.entry(0).or_default();
    let mut visited = HashSet::default();
    let mut stack = vec![Visit::Enter(data.layout().entry_bb().unwrap().bb())];
    while let Some(visit) = stack.pop() {
        match visit {
            Visit::Enter(node) => {
                if !visited.insert(node) {
                    continue;
                }
                let id = bb_alloc.check_or_alloc_id_same(node);
                let terminator = get_terminator_inst(data, node);
                match data.inst_data(terminator).kind() {
                    InstKind::Jump(jump) => {
                        let target = jump.target();
                        let target_id = bb_alloc.check_or_alloc_id_same(target);
                        graph.entry(id).or_default().push(target_id);
                        prece.entry(target_id).or_default().push(id);
                        stack.push(Visit::Enter(target));
                    }
                    InstKind::Branch(branch) => {
                        let true_target = branch.t_target();
                        let true_id = bb_alloc.check_or_alloc_id_same(true_target);
                        graph.entry(id).or_default().push(true_id);
                        prece.entry(true_id).or_default().push(id);
                        stack.push(Visit::FalseArm(id, branch.f_target()));
                        stack.push(Visit::Enter(true_target));
                    }
                    InstKind::Return(..) | InstKind::TailCall(..) => {
                        graph.entry(id).or_default();
                    }
                    _ => unreachable!(),
                }
            }
            Visit::FalseArm(source_id, target) => {
                let target_id = bb_alloc.check_or_alloc_id_same(target);
                graph.entry(source_id).or_default().push(target_id);
                prece.entry(target_id).or_default().push(source_id);
                stack.push(Visit::Enter(target));
            }
        }
    }
    (graph, prece)
}
