//! # 支配树分析（dom_tree）：CFG 上的支配关系基础设施
//!
//! 支配树（dominance tree）是优化管线最底层的控制流分析之一：它回答"从函数入口
//! 出发的所有执行路径都经过某个基本块吗"这类问题，是 SSA 构造（phi 插入）、循环
//! 识别（回边判定）、GVN/GVN-PRE（值可用性）、LICM（循环不变代码）、
//! if-conversion（菱形合并）、PSR（指针强度削减）等大量 pass 的共同地基。本模块
//! 提供两代实现：**v1** 是早期基于裸下标（`BId`）的函数式 API（`idom` /
//! `build_dominance_tree`），**v2**（`pub mod v2`）是基于 `BasicBlock` 句柄的
//! 结构体 API（`v2::DominanceTree`），新代码一律用 v2。
//!
//! ## 概念速览
//!
//! - **支配（dominates）**：若从入口到块 B 的每条路径都经过块 A，则 A 支配 B
//!   （A 也支配自己）；入口块支配一切可达块；
//! - **严格支配（strictly dominates）**：A 支配 B 且 A != B；
//! - **立即支配者（immediate dominator, idom）**：严格支配 B 且被 B 的所有严格
//!   支配者支配（即"最接近 B 的支配者"）——每个非入口块恰有一个 idom；
//! - **支配树**：把每个非入口块挂在它的 idom 之下得到的树（支配关系是偏序，
//!   其 Hasse 图恰为一棵树）；A 支配 B 当且仅当 A 是 B 在树上的祖先。
//!
//! ## v1：`idom` / `build_dominance_tree`（函数式、按 BId 下标）
//!
//! v1 工作在图构建期：先用 `utils::cfg::rpo_path` 对前驱图求逆后序（RPO），再把
//! 前驱图与 RPO 交给本模块。相关类型来自 `utils::type_alias`（经 `opt::prelude`
//! 再导出）：`BId = usize`（块编号）、`CFGGraph = HashMap<BId, Vec<BId>>`、
//! `IDomMap = Vec<BId>`、`DomTree = Vec<Vec<BId>>`。
//!
//! - `idom(prece: &CFGGraph, rpo: &[BId]) -> IDomMap`：求每个块的立即支配者。
//!   `prece` 是**前驱**图（`prece[bb]` 列出 bb 的所有前驱），`rpo` 是入口在前的
//!   逆后序（约定入口块编号为 0）。返回 `IDomMap`，`map[bb]` 即 bb 的 idom
//!   编号；**入口块取 `map[0] = 0`（自指）作为算法哨兵**——这不是合法的树边，
//!   消费方必须记得跳过。算法细节见下文。
//! - `build_dominance_tree(idom_map: &IDomMap, rpo_len: usize) -> DomTree`
//!   （`#[must_use]`）：把 idom 表反转为**子表**邻接形式（`tree[idom]` 列出所有
//!   以 idom 为立即支配者的块），即支配树的父→子邻接表；实现跳过 0 号哨兵
//!   （`idom_map[0] = 0` 不允许构成自环/环）。`rpo_len` 给出块总数。
//! - `DominanceTree`（v1 结构体，第 57 行）：遗留空壳，仅存 `old_id_alloc:
//!   BIDAlloc` 与 `idom_edges: Vec<BasicBlock>` 两个字段，全仓库无任何使用点，
//!   不要在新代码里引用它——用 `v2::DominanceTree`。
//!
//! v1 使用方（见"使用方清单"）：`opt/passes/ssa.rs`（建树后做支配边界分析）、
//! `opt/passes/gvn.rs`；`scalar_global_promotion.rs` 则以 `IDomMap`/`DomTree`
//! 为参数接收。
//!
//! ## v2：`v2::DominanceTree`（推荐，按 BasicBlock 句柄）
//!
//! v2 直接吃 IR 层的 `CFG`（`utils::cfg`），全程用 `BasicBlock` 句柄，不需要先做
//! "块 → 编号"映射，也不依赖"入口块编号为 0"的约定。类型别名
//! `DomTreeChildren = SmallVec<[BasicBlock; 4]>`：子块列表，小扇出免堆分配。
//!
//! 结构体字段（`FxHashMap` 均按块索引）：`entry`（入口块）、
//! `immediate_dominators: Block → Option<Block>`（入口为 `None`）、
//! `children: Block → DomTreeChildren`、`depths`（深度，入口 0）、
//! `dfs_in`/`dfs_out`（先序进入/后序离开时间戳，供 O(1) 支配查询）。
//!
//! ### 构造
//!
//! - `new(data: &FunctionData) -> Option<Self>`：直接分析一个函数定义；**函数
//!   声明没有 CFG，返回 `None`**（内部是 `CFG::new(data)` + `from_cfg`）。
//! - `from_cfg(cfg: &CFG) -> Self`：分析一个已构建好的 CFG，用
//!   Cooper-Harvey-Kennedy 迭代算法求 idom（详见下文）。断言 RPO 首元素是入口；
//!   只覆盖**可达块**，不可达块不在树里（`contains` 为 `false`）。
//!
//! ### 查询方法（对不在树中的块一律 panic 报错，而不是静默返回错误结果）
//!
//! - `entry() -> BasicBlock`：入口块。
//! - `contains(block) -> bool`：块是否在树中（等价于是否可达）。
//! - `immediate_dominator(block) -> Option<BasicBlock>`：立即支配者；仅入口块为
//!   `None`（公开表示里入口没有 idom）。
//! - `children_of(block) -> &[BasicBlock]`：支配树上的直接子块列表（按 RPO 序
//!   收集）。GVN-PRE 用它做"按支配树自顶向下"的遍历。
//! - `depth_of(block) -> usize`：块在支配树上的深度（入口为 0）。
//! - `dominates(dominator, block) -> bool`：**O(1)** 支配查询——利用 DFS 区间：
//!   `dfs_in[dominator] <= dfs_in[block] && dfs_out[block] <= dfs_out[dominator]`，
//!   即 block 的 [in, out] 区间被 dominator 的区间包含（子树关系）。
//! - `strictly_dominates(dominator, block) -> bool`：`dominator != block` 且
//!   `dominates`。
//! - `#[cfg(debug_assertions)] verify(&self, cfg: &CFG)`：构造后自动执行的自检——
//!   各表尺寸与 `cfg.block_count()` 一致、入口 idom 为 `None`、每个非入口块满足
//!   `strictly_dominates(idom, block)`、`depth_of(block) == depth_of(idom) + 1`、
//!   idom 链沿祖先爬有限步必达入口等。
//!
//! 模块内两个私有辅助：`intersect(lhs, rhs, idoms, rpo_indices)`（CHK 算法的
//! LCA 相交函数：两个节点沿 idom 链交替上爬，每次抬升 RPO 序号更大的一方，直到
//! 相遇）与 `number_tree(entry, children, ...)`（**迭代** DFS 编号——显式栈
//! Enter/Exit，不递归，对 2 万块深的链式 CFG 也不会爆栈）。
//!
//! ## 算法：Cooper-Harvey-Kennedy 迭代求立即支配者
//!
//! 两代实现共用同一算法。对 RPO 序（入口除外）反复扫描：
//!
//! 1. 入口以自身为 idom 哨兵（v1：`map[0] = 0`；v2：`algorithm_idoms[entry] =
//!    entry`，公开结果里再转成 `None`）；
//! 2. 每轮对每个块：取其"idom 已经算出"的前驱集合，第一个作种子，其余逐个与
//!    种子做 `intersect`（LCA）折叠，得到 `new_idom`（等价于对所有已处理前驱的
//!    idom 链取交——idom 是"所有支配者的交"里离块最近的那个）；
//! 3. 只要任何块的 idom 发生变化就再来一轮，直到整轮无变化（不动点收敛）。
//!
//! 正确性要点：RPO 保证每个非入口块至少有一个前驱（DFS 树父边）的 RPO 序号更
//! 小、因而在当轮更早被处理，所以"找第一个 idom 已知的前驱"（v1 的
//! `find(...).unwrap()`）不会落空；回边（RPO 序号更大的前驱）在收敛后才稳定。
//! 收敛后每个可达非入口块的 idom 必已求出（v2 用断言 `algorithm_idoms.len() ==
//! cfg.block_count()` 兜底）。v1 的 lca 内部用"按块号索引的 RPO 数组"比较先后
//! （依赖块编号与 RPO 位置一致的假设，`rpo_idx` 表虽构建却未被传入使用）——这是
//! v1 的脆弱点，也是 v2 改显式 `rpo_indices` 映射的原因之一。
//!
//! ## v2 相对 v1 的改进
//!
//! - **键类型**：`BasicBlock` 句柄直用，免去块→编号映射，且不依赖"入口编号 0"；
//! - **查询能力**：v1 只给 idom 表 + 子表，`dominates` 要自己沿树爬；v2 提供
//!   O(1) `dominates`、`strictly_dominates`、`depth_of`、`contains`、
//!   `children_of`；
//! - **可达性语义**：v2 明确只含可达块，不可达块查询即 panic 或 `false`，不会
//!   把不可达块当入口支配；
//! - **入口表示**：v1 用自指哨兵 0（消费方必须记得跳过）；v2 公开为 `None`；
//! - **构造入口**：`new(&FunctionData)` 一步到位（声明 → `None`）；`from_cfg`
//!   可复用已建 CFG；
//! - **健壮性**：查询对树外块 panic 报错；debug 构建自动 `verify`；`number_tree`
//!   迭代实现免深递归（配 2 万块测试）；
//! - **存储**：`FxHashMap` + `SmallVec`（子表 `DomTreeChildren`）。
//!
//! ## 使用方清单
//!
//! 直接使用 v2：
//! - `opt/passes/gvn_pre.rs`：`from_cfg` 建树；`children_of` 逆序做支配树自顶向
//!   下遍历，`dominates` 判定 load 跨块上提与定义可用性（`docs/
//!   memory_alias_analysis.md` 也点名复用 `dom_tree::v2`）；
//! - `opt/passes/if_conversion.rs`：`DominanceTree::new(data)`（`Option`），判定
//!   菱形结构能否合并；
//! - `opt/passes/licm.rs`：`dominates` 判定候选块支配循环头（循环不变性）；
//! - `opt/passes/pointer_strength_reduction.rs`（含 `candidate.rs`）：`dominates`
//!   判定指针/索引定义点支配使用点；
//! - `opt/passes/reduction_unroll.rs` / `blocked_reduction.rs`：
//!   `dominates_loop_entry` 要求 bound 严格支配循环入口才允许版本化；
//! - `opt/passes/matmul_interchange.rs`：经 `LoopAnalysis` 拿树。
//!
//! 间接使用（经 `loop_analysis`：`LoopAnalysis::new`/`from_cfg` 内部用 v2 建树，
//! 并以 `dominates` 判回边/可归约性、验证循环体）：`licm`、
//! `pointer_strength_reduction`、`reduction_unroll`、`blocked_reduction`、
//! `invariant_reduction_hoisting`、`matmul_interchange`、`mod_fold`、
//! `loop_unroll`、`utils/preheader.rs`、`induction_variable.rs`（测试）等。
//!
//! 使用 v1：`opt/passes/ssa.rs`（`idom` + `build_dominance_tree` 建树后做支配
//! 边界分析，用于 phi 插入）、`opt/passes/gvn.rs`（同样两函数建树）；
//! `opt/passes/scalar_global_promotion.rs` 接收 `IDomMap`/`DomTree` 参数（由
//! ssa 的 `dominance_analysis` 产出）。
//!
//! ## 快照语义（重要）
//!
//! 支配树是**一次性快照**：它只对构建时刻的 CFG 成立。任何改动 CFG 的操作——
//! 增删块、改跳转/分支目标、改结构边/逻辑边、动块参数——都会使旧树失效，之后
//! 继续查询旧树是未定义行为（可能 panic，也可能静默给出错误答案）。参照
//! `docs/Convention.md` 与 `crate::opt` 模块文档：CFG/支配/循环/IV 分析都是快照，
//! **改写后必须重建**。固定点管线中每个 pass 各自负责在需要时重建分析（如
//! `licm` 跑在 `SimplifyCFG` 之后）。
//!
//! ## 验证
//!
//! v2 内联 `#[cfg(test)] mod tests`，共 5 个测试：
//! `declaration_has_no_dominance_tree`（声明返回 `None`）、
//! `computes_diamond_immediate_dominators`（菱形合并点 idom 为入口）、
//! `handles_backedges_and_ignores_unreachable_blocks`（回边 + 不可达块剔除）、
//! `numbers_a_deep_dominance_tree_without_recursion`（2 万块链免递归编号）、
//! `dominance_queries_match_the_graph_definition`（与图定义的暴力对拍：
//! `dominates_by_removal` 删除支配者后 BFS 验证）。v1 无独立测试，由 ssa/gvn 的
//! 测试间接覆盖。全量：`cargo test -p raana_ir`。
use crate::opt::prelude::*;

pub fn idom(prece: &CFGGraph, rpo: &[BId]) -> IDomMap {
    fn lca(n1: BId, n2: BId, map: &IDomMap, rpo_idx: &[BId]) -> BId {
        let mut p1 = n1;
        let mut p2 = n2;
        while p1 != p2 {
            while rpo_idx[p1] > rpo_idx[p2] {
                p1 = map[p1];
            }
            while rpo_idx[p1] < rpo_idx[p2] {
                p2 = map[p2];
            }
        }
        p1
    }

    let mut map = IDomMap::new();
    map.resize(rpo.len(), usize::MAX);
    debug!("rpo before panic: {:?}", rpo);
    let mut rpo_idx = vec![0; rpo.len()];
    for (i, &id) in rpo.iter().enumerate() {
        rpo_idx[id] = i;
    }

    map[0] = 0;

    let mut converged = false;
    while !converged {
        converged = true;
        for node in &rpo[1..] {
            let mut it = prece[node].iter();
            let mut new_idom = *it.find(|&&x| map[x] != usize::MAX).unwrap();
            for &other_node in it.filter(|&&x| map[x] != usize::MAX) {
                new_idom = lca(new_idom, other_node, &map, rpo);
            }
            if map[*node] != new_idom {
                map[*node] = new_idom;
                converged = false;
            }
        }
    }
    map
}

#[must_use]
pub fn build_dominance_tree(idom_map: &IDomMap, rpo_len: usize) -> DomTree {
    let mut ret = vec![vec![]; rpo_len];
    // INFO: remember that idom_map we make `idom_map[0] = 0`
    // that is not allowed in a tree (no loop or ring)
    for (vid, &pa) in idom_map.iter().enumerate().skip(1) {
        ret[pa].push(vid);
    }
    ret
}

pub struct DominanceTree {
    old_id_alloc: BIDAlloc,
    idom_edges: Vec<BasicBlock>,
}

pub mod v2 {
    use rustc_hash::FxHashMap;
    use smallvec::SmallVec;

    use crate::{
        ir::{BasicBlock, FunctionData},
        opt::utils::cfg::CFG,
    };

    pub type DomTreeChildren = SmallVec<[BasicBlock; 4]>;

    /// Dominance information for the reachable blocks of one function CFG.
    ///
    /// The entry has no immediate dominator in the public representation.
    /// Analysis results are tied to the CFG snapshot used to construct them and
    /// must be discarded after a control-flow mutation.
    #[derive(Debug, Clone)]
    pub struct DominanceTree {
        entry: BasicBlock,
        immediate_dominators: FxHashMap<BasicBlock, Option<BasicBlock>>,
        children: FxHashMap<BasicBlock, DomTreeChildren>,
        depths: FxHashMap<BasicBlock, usize>,
        dfs_in: FxHashMap<BasicBlock, usize>,
        dfs_out: FxHashMap<BasicBlock, usize>,
    }

    impl DominanceTree {
        /// Analyze a function definition. Function declarations have no CFG and
        /// therefore return `None`.
        pub fn new(data: &FunctionData) -> Option<Self> {
            CFG::new(data).map(|cfg| Self::from_cfg(&cfg))
        }

        /// Analyze an existing CFG using the Cooper-Harvey-Kennedy iterative
        /// immediate-dominator algorithm.
        pub fn from_cfg(cfg: &CFG) -> Self {
            let entry = cfg.entry();
            let reverse_postorder = cfg.reverse_postorder();
            assert_eq!(reverse_postorder.first(), Some(&entry));

            let rpo_indices = reverse_postorder
                .iter()
                .enumerate()
                .map(|(index, &block)| (block, index))
                .collect::<FxHashMap<_, _>>();

            // The entry self-dominates only as an internal algorithm sentinel.
            // It is converted to `None` in the public result below.
            let mut algorithm_idoms = FxHashMap::default();
            algorithm_idoms.insert(entry, entry);

            loop {
                let mut changed = false;
                for &block in reverse_postorder.iter().skip(1) {
                    let mut processed_predecessors = cfg
                        .predecessors_of(block)
                        .iter()
                        .copied()
                        .filter(|predecessor| algorithm_idoms.contains_key(predecessor));
                    let Some(first_predecessor) = processed_predecessors.next() else {
                        continue;
                    };

                    let new_idom = processed_predecessors.fold(first_predecessor, |idom, pred| {
                        intersect(pred, idom, &algorithm_idoms, &rpo_indices)
                    });
                    if algorithm_idoms.get(&block) != Some(&new_idom) {
                        algorithm_idoms.insert(block, new_idom);
                        changed = true;
                    }
                }
                if !changed {
                    break;
                }
            }

            assert_eq!(
                algorithm_idoms.len(),
                cfg.block_count(),
                "every reachable non-entry block must acquire an immediate dominator"
            );

            let mut immediate_dominators = FxHashMap::default();
            let mut children = FxHashMap::default();
            for &block in reverse_postorder {
                children.insert(block, DomTreeChildren::new());
                let idom = (block != entry).then(|| algorithm_idoms[&block]);
                immediate_dominators.insert(block, idom);
            }
            for &block in reverse_postorder.iter().skip(1) {
                let parent = algorithm_idoms[&block];
                children
                    .get_mut(&parent)
                    .expect("an immediate dominator must be reachable")
                    .push(block);
            }

            let mut depths = FxHashMap::default();
            let mut dfs_in = FxHashMap::default();
            let mut dfs_out = FxHashMap::default();
            number_tree(entry, &children, &mut depths, &mut dfs_in, &mut dfs_out);

            let tree = Self {
                entry,
                immediate_dominators,
                children,
                depths,
                dfs_in,
                dfs_out,
            };
            #[cfg(debug_assertions)]
            tree.verify(cfg);
            tree
        }

        pub fn entry(&self) -> BasicBlock {
            self.entry
        }

        pub fn contains(&self, block: BasicBlock) -> bool {
            self.immediate_dominators.contains_key(&block)
        }

        pub fn immediate_dominator(&self, block: BasicBlock) -> Option<BasicBlock> {
            *self
                .immediate_dominators
                .get(&block)
                .unwrap_or_else(|| panic!("basic block {block:?} is not in this dominance tree"))
        }

        pub fn children_of(&self, block: BasicBlock) -> &[BasicBlock] {
            self.children
                .get(&block)
                .map(DomTreeChildren::as_slice)
                .unwrap_or_else(|| panic!("basic block {block:?} is not in this dominance tree"))
        }

        pub fn depth_of(&self, block: BasicBlock) -> usize {
            *self
                .depths
                .get(&block)
                .unwrap_or_else(|| panic!("basic block {block:?} is not in this dominance tree"))
        }

        pub fn dominates(&self, dominator: BasicBlock, block: BasicBlock) -> bool {
            let dominator_in = self.dfs_in.get(&dominator).unwrap_or_else(|| {
                panic!("basic block {dominator:?} is not in this dominance tree")
            });
            let block_in = self
                .dfs_in
                .get(&block)
                .unwrap_or_else(|| panic!("basic block {block:?} is not in this dominance tree"));
            dominator_in <= block_in && self.dfs_out[&block] <= self.dfs_out[&dominator]
        }

        pub fn strictly_dominates(&self, dominator: BasicBlock, block: BasicBlock) -> bool {
            dominator != block && self.dominates(dominator, block)
        }

        #[cfg(debug_assertions)]
        fn verify(&self, cfg: &CFG) {
            debug_assert_eq!(self.entry, cfg.entry());
            debug_assert_eq!(self.immediate_dominators.len(), cfg.block_count());
            debug_assert_eq!(self.children.len(), cfg.block_count());
            debug_assert_eq!(self.depths.len(), cfg.block_count());
            debug_assert_eq!(self.dfs_in.len(), cfg.block_count());
            debug_assert_eq!(self.dfs_out.len(), cfg.block_count());
            debug_assert_eq!(self.immediate_dominator(self.entry), None);
            debug_assert_eq!(self.depth_of(self.entry), 0);

            for &block in cfg.blocks() {
                debug_assert!(self.dominates(block, block));
                if block == self.entry {
                    continue;
                }
                let idom = self
                    .immediate_dominator(block)
                    .expect("every reachable non-entry block must have an idom");
                debug_assert!(self.strictly_dominates(idom, block));
                debug_assert_eq!(self.depth_of(block), self.depth_of(idom) + 1);
                debug_assert!(self.children_of(idom).contains(&block));

                let mut ancestor = block;
                for _ in 0..cfg.block_count() {
                    if ancestor == self.entry {
                        break;
                    }
                    ancestor = self
                        .immediate_dominator(ancestor)
                        .expect("only the entry may lack an idom");
                }
                debug_assert_eq!(
                    ancestor, self.entry,
                    "immediate-dominator chain must reach the entry"
                );
            }
        }
    }

    fn intersect(
        mut lhs: BasicBlock,
        mut rhs: BasicBlock,
        idoms: &FxHashMap<BasicBlock, BasicBlock>,
        rpo_indices: &FxHashMap<BasicBlock, usize>,
    ) -> BasicBlock {
        while lhs != rhs {
            while rpo_indices[&lhs] > rpo_indices[&rhs] {
                lhs = idoms[&lhs];
            }
            while rpo_indices[&rhs] > rpo_indices[&lhs] {
                rhs = idoms[&rhs];
            }
        }
        lhs
    }

    fn number_tree(
        entry: BasicBlock,
        children: &FxHashMap<BasicBlock, DomTreeChildren>,
        depths: &mut FxHashMap<BasicBlock, usize>,
        dfs_in: &mut FxHashMap<BasicBlock, usize>,
        dfs_out: &mut FxHashMap<BasicBlock, usize>,
    ) {
        #[derive(Clone, Copy)]
        enum Visit {
            Enter(BasicBlock, usize),
            Exit(BasicBlock),
        }

        let mut timestamp = 0;
        let mut stack = vec![Visit::Enter(entry, 0)];
        while let Some(visit) = stack.pop() {
            match visit {
                Visit::Enter(block, depth) => {
                    depths.insert(block, depth);
                    dfs_in.insert(block, timestamp);
                    timestamp += 1;
                    stack.push(Visit::Exit(block));
                    for &child in children[&block].iter().rev() {
                        stack.push(Visit::Enter(child, depth + 1));
                    }
                }
                Visit::Exit(block) => {
                    dfs_out.insert(block, timestamp);
                    timestamp += 1;
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::collections::VecDeque;

        use rustc_hash::FxHashSet;

        use super::*;
        use crate::ir::{
            Program, Type,
            builder_trait::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
        };

        #[test]
        fn declaration_has_no_dominance_tree() {
            let mut program = Program::new();
            let function = program.new_function(Type::get_unit(), "decl".into(), vec![]);
            assert!(DominanceTree::new(program.func_data(function)).is_none());
        }

        #[test]
        fn computes_diamond_immediate_dominators() {
            let mut program = Program::new();
            let function = program.new_function(Type::get_i32(), "diamond".into(), vec![]);
            let data = program.func_data_mut(function);
            let entry = data.add_entry_block();
            let left = data.new_basic_block().basic_block("left".into(), vec![]);
            let right = data.new_basic_block().basic_block("right".into(), vec![]);
            let merge = data.new_basic_block().basic_block("merge".into(), vec![]);
            for block in [left, right, merge] {
                data.layout_mut().push_bb_back(block);
            }

            let condition = data.new_local_inst().integer(1);
            let branch = data
                .new_local_inst()
                .branch(condition, left, vec![], right, vec![]);
            data.layout_mut().insert_inst(entry, branch);
            let left_jump = data.new_local_inst().jump(merge, vec![]);
            data.layout_mut().insert_inst(left, left_jump);
            let right_jump = data.new_local_inst().jump(merge, vec![]);
            data.layout_mut().insert_inst(right, right_jump);
            let value = data.new_local_inst().integer(7);
            let ret = data.new_local_inst().ret(Some(value));
            data.layout_mut().insert_inst(merge, ret);

            let tree = DominanceTree::new(data).unwrap();
            assert_eq!(tree.immediate_dominator(entry), None);
            assert_eq!(tree.immediate_dominator(left), Some(entry));
            assert_eq!(tree.immediate_dominator(right), Some(entry));
            assert_eq!(tree.immediate_dominator(merge), Some(entry));
            assert!(tree.dominates(entry, merge));
            assert!(!tree.dominates(left, merge));
            assert!(!tree.dominates(right, merge));
            assert_eq!(tree.depth_of(entry), 0);
            assert_eq!(tree.depth_of(merge), 1);
            assert_eq!(
                tree.children_of(entry)
                    .iter()
                    .copied()
                    .collect::<FxHashSet<_>>(),
                FxHashSet::from_iter([left, right, merge])
            );
        }

        #[test]
        fn handles_backedges_and_ignores_unreachable_blocks() {
            let mut program = Program::new();
            let function = program.new_function(Type::get_unit(), "loop".into(), vec![]);
            let data = program.func_data_mut(function);
            let entry = data.add_entry_block();
            let header = data.new_basic_block().basic_block("header".into(), vec![]);
            let body = data.new_basic_block().basic_block("body".into(), vec![]);
            let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
            let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
            let dead = data.new_basic_block().basic_block("dead".into(), vec![]);
            for block in [header, body, latch, exit, dead] {
                data.layout_mut().push_bb_back(block);
            }

            let entry_jump = data.new_local_inst().jump(header, vec![]);
            data.layout_mut().insert_inst(entry, entry_jump);
            let condition = data.new_local_inst().integer(1);
            let header_branch = data
                .new_local_inst()
                .branch(condition, body, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, header_branch);
            let body_jump = data.new_local_inst().jump(latch, vec![]);
            data.layout_mut().insert_inst(body, body_jump);
            let latch_jump = data.new_local_inst().jump(header, vec![]);
            data.layout_mut().insert_inst(latch, latch_jump);
            let exit_ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(exit, exit_ret);
            let dead_ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(dead, dead_ret);

            let tree = DominanceTree::new(data).unwrap();
            assert_eq!(tree.immediate_dominator(header), Some(entry));
            assert_eq!(tree.immediate_dominator(body), Some(header));
            assert_eq!(tree.immediate_dominator(latch), Some(body));
            assert_eq!(tree.immediate_dominator(exit), Some(header));
            assert!(tree.dominates(header, latch));
            assert!(!tree.dominates(body, header));
            assert!(!tree.contains(dead));
        }

        #[test]
        fn numbers_a_deep_dominance_tree_without_recursion() {
            let mut program = Program::new();
            let function = program.new_function(Type::get_unit(), "deep_dom".into(), vec![]);
            let data = program.func_data_mut(function);
            let entry = data.add_entry_block();
            let mut blocks = vec![entry];
            for index in 0..20_000 {
                let block = data
                    .new_basic_block()
                    .basic_block(format!("block_{index}"), vec![]);
                data.layout_mut().push_bb_back(block);
                blocks.push(block);
            }
            for pair in blocks.windows(2) {
                let jump = data.new_local_inst().jump(pair[1], vec![]);
                data.layout_mut().insert_inst(pair[0], jump);
            }
            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(*blocks.last().unwrap(), ret);

            let cfg = CFG::new(data).unwrap();
            let tree = DominanceTree::from_cfg(&cfg);
            let last = *blocks.last().unwrap();
            assert_eq!(tree.depth_of(last), blocks.len() - 1);
            assert!(tree.dominates(entry, last));
            assert!(!tree.dominates(last, entry));
        }

        #[test]
        fn dominance_queries_match_the_graph_definition() {
            let mut program = Program::new();
            let function = program.new_function(Type::get_unit(), "nested".into(), vec![]);
            let data = program.func_data_mut(function);
            let entry = data.add_entry_block();
            let header = data.new_basic_block().basic_block("header".into(), vec![]);
            let left = data.new_basic_block().basic_block("left".into(), vec![]);
            let right = data.new_basic_block().basic_block("right".into(), vec![]);
            let merge = data.new_basic_block().basic_block("merge".into(), vec![]);
            let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
            let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
            for block in [header, left, right, merge, latch, exit] {
                data.layout_mut().push_bb_back(block);
            }

            let condition = data.new_local_inst().integer(1);
            let entry_jump = data.new_local_inst().jump(header, vec![]);
            data.layout_mut().insert_inst(entry, entry_jump);
            let header_branch = data
                .new_local_inst()
                .branch(condition, left, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, header_branch);
            let left_branch = data
                .new_local_inst()
                .branch(condition, right, vec![], merge, vec![]);
            data.layout_mut().insert_inst(left, left_branch);
            let right_jump = data.new_local_inst().jump(merge, vec![]);
            data.layout_mut().insert_inst(right, right_jump);
            let merge_jump = data.new_local_inst().jump(latch, vec![]);
            data.layout_mut().insert_inst(merge, merge_jump);
            let latch_jump = data.new_local_inst().jump(header, vec![]);
            data.layout_mut().insert_inst(latch, latch_jump);
            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(exit, ret);

            let cfg = CFG::new(data).unwrap();
            let tree = DominanceTree::from_cfg(&cfg);
            for &dominator in cfg.blocks() {
                for &block in cfg.blocks() {
                    assert_eq!(
                        tree.dominates(dominator, block),
                        dominates_by_removal(&cfg, dominator, block),
                        "mismatched dominance query for {dominator:?} -> {block:?}"
                    );
                }
            }
        }

        fn dominates_by_removal(cfg: &CFG, dominator: BasicBlock, block: BasicBlock) -> bool {
            if dominator == block || dominator == cfg.entry() {
                return true;
            }

            let mut visited = FxHashSet::default();
            let mut worklist = VecDeque::from([cfg.entry()]);
            while let Some(current) = worklist.pop_front() {
                if current == dominator || !visited.insert(current) {
                    continue;
                }
                if current == block {
                    return false;
                }
                worklist.extend(cfg.successors_of(current).iter().copied());
            }
            true
        }
    }
}
