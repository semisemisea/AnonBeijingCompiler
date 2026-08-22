//! # 自然循环分析：从支配回边发现循环结构（`LoopAnalysis` / `Loop`）
//!
//! 一句话定位：本模块从 CFG 的**支配回边**出发，发现函数里所有的**自然循环**
//! （natural loop），并把它们组织成一棵**循环嵌套森林**（按包含关系分层），
//! 供所有循环优化 pass 查询"某个块属于哪些循环、最内层循环是什么、循环的
//! header/latch/前驱结构长什么样"。`LoopAnalysis` 是循环结构的只读快照，
//! `Loop` 是单个循环的描述（header + body + latches）。
//!
//! ## 为什么需要循环分析
//!
//! 一半以上的优化 pass 都在问"哪里是循环、循环怎么嵌套"：展开（`loop_unroll`）、
//! 旋转（`rotate_loops`）、LICM、强度削减（PSR）、规约优化（`reduction_unroll` /
//! `blocked_reduction` / `invariant_reduction_hoisting`）、矩阵转置（`column_major` /
//! `matmul_interchange`）……如果每个 pass 都自己遍历 CFG 找循环，代码重复不说，
//! "循环"的判定标准还会各写各的。本模块把**统一的自然循环模型**集中实现一次，
//! 其他 pass 拿到 `LoopAnalysis` 之后只做查询。
//!
//! 定位链：`FunctionData` → `CFG`（数据驱动的 CFG，只含从入口可达的块）→
//! `DominanceTree`（支配树）→ **`LoopAnalysis`**（本模块）→ 各循环优化 pass。
//!
//! ## 核心数据结构
//!
//! - `Loop`：**单个自然循环**。三个字段：`header`（循环入口块，唯一）、`body`
//!   （循环体块集合，含 header 与所有 latch）、`latches`（回边源块，可能有多个，
//!   也可能就是 header 本身——自循环）。**注意**：`body` 是 `FxHashSet`，迭代
//!   顺序不稳定，需要稳定顺序时请自己排序。
//! - `LoopAnalysis`：**整份循环分析结果**。内部维护四个字段：
//!   - `loops`：所有循环，**按 body 大小从小到大排序**（小循环在前）；
//!   - `direct_parent`：`direct_parent[i] == j` 表示 `loops[i]` 的直接父循环是
//!     `loops[j]`；`direct_parent[i] == i` 表示它是循环森林的根（没有被任何循环
//!     包含）；
//!   - `loop_index`：header → 循环下标（每个自然循环 header 唯一，故可作键）；
//!   - `block_to_inner_loop`：块 → 包含它的**最内层**循环下标。
//!
//! ## API 详解
//!
//! ### `Loop` 的方法
//!
//! - `header(&self) -> BasicBlock`：循环头块。所有进入循环的路径都经过它，它也
//!   支配 body 里每个块；
//! - `body(&self) -> &FxHashSet<BasicBlock>`：循环体块集合（含 header 和所有
//!   latch）；
//! - `latches(&self) -> &[BasicBlock]`：回边源块列表。每个 latch 都有一条指向
//!   header 的边；多 latch 是合法的（如 `while` 里带 `break` 改写出的结构）；
//! - `contains(&self, block: BasicBlock) -> bool`：判断块是否属于本循环体；
//! - `get_preheader(&self, cfg: &CFG) -> Option<BasicBlock>`：**识别**（不是创建）
//!   循环的前驱块。按 `docs/Convention.md` 的定义，前驱块必须同时满足：在循环
//!   外、是 header **唯一**的外部结构前驱（`predecessors_of(header)` 里过滤掉
//!   body 内块后恰好一个）、且它唯一的后继就是 header。三者任一不满足返回
//!   `None`。识别与创建是两件事，创建走 `opt/utils/preheader.rs`。
//!
//! ### `LoopAnalysis` 的构造与查询
//!
//! - `new(data: &FunctionData) -> (CFG, DominanceTree, LoopAnalysis)`：从函数数据
//!   一步到位——内部先建 `CFG` 再调 `from_cfg`。三者打包返回是因为循环分析依赖
//!   CFG 和支配树，调用方通常三者都要；
//! - `from_cfg(cfg: CFG) -> (CFG, DominanceTree, LoopAnalysis)`：从现成的 `CFG`
//!   出发重建全套分析（`DominanceTree::from_cfg` → 找回边 → 展开 body → 分层）。
//!   返回的 `CFG` 就是传入的那个（`CFG` 是值类型，这里借出又还回）；
//! - `loops(&self) -> &[Loop]`：全部循环，**按 body 大小从小到大**排序——遍历时
//!   先遇到的总是更内层的循环；
//! - `min_loop_contain(&self, block) -> Option<&Loop>`：包含 `block` 的**最内层**
//!   循环；`None` 表示块不在任何循环里；
//! - `min_loop_contain_index(&self, block) -> Option<usize>`：同上，但返回下标
//!   （省一次解引用，也方便传给 `loops()` 索引）；
//! - `loop_index(&self, header: BasicBlock) -> Option<usize>`：按 header 反查循环
//!   下标。用于"已知循环头、想拿到 `Loop` 或父循环"的场景；
//! - `parent_loop_index(&self, index: usize) -> Option<usize>`：`loops[index]` 的
//!   直接父循环下标；根循环返回 `None`；
//! - `parent_loop(&self, index: usize) -> Option<&Loop>`：同上，返回父循环引用；
//! - `containing_loop_indices(&self, block) -> impl Iterator<Item = usize> + '_`：
//!   迭代包含 `block` 的全部循环下标，**从内到外**（靠 `parent_loop_index` 沿
//!   父链走）；
//! - `containing_loops(&self, block) -> impl Iterator<Item = &Loop> + '_`：同上，
//!   直接给循环引用。
//!
//! 使用模式速记：想知道"某块在哪些循环里、最内层是谁"用 `min_loop_contain` /
//! `containing_loops`；想逐循环做变换用 `loops()` 遍历 + `Loop` 各方法；想沿
//! 嵌套往上层走用 `parent_loop_index` / `parent_loop`。
//!
//! ## 算法：自然循环发现（5 步）
//!
//! 1. **建 CFG 与支配树**：`CFG::new(data)` 得到只含可达块的数据驱动 CFG，
//!   再用 `DominanceTree::from_cfg(&cfg)` 求出支配关系（`dominates(a, b)` 表示
//!   a 支配 b，即所有到达 b 的路径都经过 a）；
//! 2. **找支配回边**：遍历 `cfg.edges()`，边 `src → dst` 若满足
//!   `dominates(dst, src)`（目标支配源，即从 dst 出发绕一圈又能回到 dst），
//!   它就是一条**回边**：`dst` 是循环 header，`src` 是 latch。同一 header 的
//!   多个 latch 合并进同一个循环（`FxHashMap<header, SmallVec<latch>>`）；
//! 3. **展开循环体**：对每个 (header, latches)，从所有 latch 出发沿前驱反向
//!   BFS（`VecDeque` 工作列表），把沿途块加入 body，**遇到 header 就停**——
//!   这保证 body 恰好是"从 header 出发能到达、又能回到 header"的闭包。自循环
//!   （latch == header）不需要前驱遍历，直接收进 body。debug 构建下断言 body
//!   中每个块都被 header 支配（自然循环的定义性质）；
//! 4. **排序与分层**：`sort_unstable_by_key(|l| l.body.len())` 把循环按大小升序
//!   排好；然后对每个循环 i，找第一个包含其 header 的下标更大的循环 j，令
//!   `direct_parent[i] = j`——由于循环要么嵌套要么不相交（可归约性保证，见下），
//!   这个 j 就是**最小的**包含循环，即直接父循环；
//! 5. **建索引表**：`block_to_inner_loop` 用 `entry().or_insert(i)` 逐循环填充，
//!   因为循环已按小→大排序，先写入的必然是最内层；`loop_index` 由
//!   header → 下标构成。最后（debug 构建）跑 `verify` 全量校验。
//!
//! 时间代价：支配树构建 + 每循环一次反向 BFS，实践中远小于一次全函数数据流
//! 分析；每个循环优化 pass 各自重建一份（见快照语义），不跨 pass 缓存。
//!
//! ## 可归约性前提（`docs/Convention.md` 第四节）
//!
//! 自然循环模型建立在**可归约 CFG** 之上，本模块把这条前提写进了 debug 断言：
//!
//! - `assert_reducible`：把所有非回边（即不被支配的边）当成有向图，从入度为 0
//!   的块开始拓扑遍历，若访问不到全部块，说明存在不经过支配回边的环——即
//!   **不可归约**（irreducible，典型如多个头互相跳转），自然循环分析不支持，
//!   直接 panic（仅 debug 构建）；
//! - `assert_laminar`：任意两个自然循环**要么嵌套、要么不相交**（laminar）。
//!   相交却互不包含、或两块集合完全相同，都是 bug；
//! - 每个自然循环允许 **1 个或多个 latch**，`body` 必须含 header 与所有 latch；
//!   header 必须支配 body 中每个块；每个自然循环 header 唯一（`loop_index` 长度
//!   断言）。
//!
//! ## 快照语义（重要！）
//!
//! `LoopAnalysis` 与 `CFG`、`DominanceTree` 一样是**快照**：`from_cfg` 完成后，
//! 分析结果与当时的 CFG 结构绑定。`docs/Convention.md` 明确规定：**任何对块、
//! 终结符、跳转目标、逻辑边参数或可达性的修改，都会使全部相关快照失效**——
//! 包括本模块。也就是说：
//!
//! - pass 必须在自己 `run_on` 的开头（改完 CFG 之后）调用 `LoopAnalysis::from_cfg`
//!   拿一份**新鲜**的分析，用完即弃；
//! - 绝不能在改写 CFG 之后继续用旧分析做查询或变换决策（会基于过期结构做错
//!   事）；一个 pass 里多次改写 CFG 的，每次改写后都要重建；
//! - 只读查询（`contains` / `min_loop_contain` / `get_preheader` 等）不修改任何
//!   状态，可随意调用。
//!
//! 另外按 Convention.md，**前驱块的识别与创建是两件事**：本模块只负责识别
//! （`get_preheader`）；创建前驱块、专用 latch、专用 exit 都是破坏性改写，必须
//! 有具体消费者才做（`opt/utils/preheader.rs`），且创建后必须重建分析。循环
//! 规范化只做消费者要求的最小形态，不全局铺开。
//!
//! ## 使用方清单
//!
//! 全仓库 grep `LoopAnalysis` 的结果（`raana_ir/src/opt` 下）：
//!
//! - `passes/loop_unroll.rs`：`from_cfg` 后逐循环展开；用 `get_preheader` 确认
//!   入口边形态再决定展开方式；
//! - `passes/licm.rs`（含 `licm/tests.rs`）：逐循环提不变式，`get_preheader`
//!   定位插入点；
//! - `passes/pointer_strength_reduction.rs`（PSR，含 `candidate.rs`/`rewrite.rs`/
//!   `tests.rs`）：逐循环做指针强度削减；代价模型
//!   `utils/pointer_strength_reduction_cost.rs` 也用 `get_preheader` 估入口代价；
//! - `passes/column_major.rs`：用 `min_loop_contain` / `containing_loops` 判断
//!   访存块所在的（最内层）循环，决定矩阵转置策略；
//! - `passes/blocked_reduction.rs`、`passes/reduction_unroll.rs`、
//!   `passes/invariant_reduction_hoisting.rs`：都是 `from_cfg` 后逐循环做规约
//!   相关的改写；
//! - `passes/matmul_interchange.rs`：用 `loop_index` + `loops()` 定位 i/j/k 三层
//!   循环及其父子关系，做循环交换；
//! - `passes/mod_fold.rs`：`from_cfg` 判定循环结构后折叠取模运算；
//! - `passes/recursive_memoize.rs`：`min_loop_contain_index(call_bb)` 判断调用点
//!   在循环内，`get_preheader` 找记忆化初始化的位置；
//! - `passes/inline.rs`：按调用者逐个建 `LoopAnalysis`，用 `min_loop_contain`
//!   判断调用点是否在循环内（影响内联收益估计）；
//! - `analysis_passes/induction_variable.rs`：`loops()` 逐循环做基本归纳变量
//!   分析（IV 分析本身依赖本模块）；
//! - `analysis_passes/range.rs`：收集 `loops().iter().map(Loop::header)` 做范围
//!   分析；
//! - `utils/preheader.rs`：前驱块识别（`get_preheader`）与创建，是各 pass 的
//!   公共工具。
//!
//! **注意**：`passes/rotate_loops.rs` **不依赖**本模块——它直接对 header 的
//! 终结符做模式匹配（countdown / count-up 形态）来旋转循环，请勿误以为它消费
//! `LoopAnalysis`。
//!
//! ## 验证
//!
//! - 单元测试在本文件底部 `#[cfg(test)] mod tests`：`exposes_direct_parent_
//!   indices_and_loops`（父子链与 `loop_index`）和 `lists_containing_loops_from_
//!   inner_to_outer`（嵌套循环下 `min_loop_contain_index` / `containing_loops`
//!   的内→外顺序），用 `LoopAnalysis::new` 构造嵌套循环夹具；
//! - debug 构建下的运行时不变量：`from_cfg` 末尾的 `verify`、`assert_laminar`、
//!   `assert_reducible`，任何违反自然循环模型的 CFG 都会在测试与 debug 运行中
//!   立即触发断言（这也是"改完 CFG 必须重建分析"的兜底）；
//! - 各消费 pass 的 inline 测试（如 `loop_unroll.rs`、`licm/tests.rs`、
//!   `pointer_strength_reduction/tests.rs`）会间接覆盖本模块的查询语义。
//!
use itertools::Itertools;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use crate::opt::{analysis_passes::dom_tree::v2::DominanceTree, prelude::*, utils::cfg::CFG};

pub struct Loop {
    header: BasicBlock,
    body: FxHashSet<BasicBlock>,
    latches: SmallVec<[BasicBlock; 2]>,
}

impl Loop {
    pub fn header(&self) -> BasicBlock {
        self.header
    }

    pub fn body(&self) -> &FxHashSet<BasicBlock> {
        &self.body
    }

    pub fn latches(&self) -> &[BasicBlock] {
        &self.latches
    }

    pub fn contains(&self, block: BasicBlock) -> bool {
        self.body.contains(&block)
    }

    pub fn get_preheader(&self, cfg: &utils::cfg::CFG) -> Option<BasicBlock> {
        let candidate = *cfg
            .predecessors_of(self.header)
            .iter()
            .filter(|&&b| !self.contains(b))
            .exactly_one()
            .ok()?;
        let _preds_succ = *cfg.successors_of(candidate).iter().exactly_one().ok()?;
        Some(candidate)
    }
}

pub struct LoopAnalysis {
    /// Storage place.
    /// The order of loops is sorted by its size, from small to big.
    loops: Vec<Loop>,

    /// `direct_parent[i] == j`, means `loops[i]`'s direct parent is `loops[j]`
    /// if `i == j`, means `loops[i]` is one of the root in the loop forest.
    direct_parent: Vec<usize>,

    /// Each loop owns a unique header, so we use it to find the index of `loops`.
    loop_index: FxHashMap<BasicBlock, usize>,

    /// `block_to_inner_loop[&bb] == i`, means the smallest loop contains the `bb` is `loops[i]`
    block_to_inner_loop: FxHashMap<BasicBlock, usize>,
}

impl LoopAnalysis {
    pub fn loops(&self) -> &[Loop] {
        &self.loops
    }

    pub fn min_loop_contain(&self, block: BasicBlock) -> Option<&Loop> {
        self.min_loop_contain_index(block)
            .map(|index| &self.loops[index])
    }

    /// Returns the index of the innermost loop containing `block`.
    pub fn min_loop_contain_index(&self, block: BasicBlock) -> Option<usize> {
        self.block_to_inner_loop.get(&block).copied()
    }

    /// Returns the analysis-local index of the loop with `header`.
    pub fn loop_index(&self, header: BasicBlock) -> Option<usize> {
        self.loop_index.get(&header).copied()
    }

    /// Returns the direct parent index, or `None` when `index` is a root loop.
    pub fn parent_loop_index(&self, index: usize) -> Option<usize> {
        let parent = self.direct_parent[index];
        (parent != index).then_some(parent)
    }

    /// Returns the direct parent, or `None` when `index` is a root loop.
    pub fn parent_loop(&self, index: usize) -> Option<&Loop> {
        self.parent_loop_index(index)
            .map(|parent| &self.loops[parent])
    }

    /// Iterates loop indices containing `block`, from innermost to outermost.
    pub fn containing_loop_indices(&self, block: BasicBlock) -> impl Iterator<Item = usize> + '_ {
        std::iter::successors(self.min_loop_contain_index(block), |&index| {
            self.parent_loop_index(index)
        })
    }

    /// Iterates loops containing `block`, from innermost to outermost.
    pub fn containing_loops(&self, block: BasicBlock) -> impl Iterator<Item = &Loop> + '_ {
        self.containing_loop_indices(block)
            .map(|index| &self.loops[index])
    }

    pub fn new(data: &FunctionData) -> (CFG, DominanceTree, LoopAnalysis) {
        let cfg = utils::cfg::CFG::new(data).unwrap();
        Self::from_cfg(cfg)
    }

    pub fn from_cfg(cfg: CFG) -> (CFG, DominanceTree, LoopAnalysis) {
        let dom_tree = dom_tree::v2::DominanceTree::from_cfg(&cfg);
        let mut back_edges: FxHashMap<BasicBlock, SmallVec<[BasicBlock; 2]>> = FxHashMap::default();
        for edge in cfg.edges() {
            if dom_tree.dominates(edge.dst, edge.src) {
                back_edges.entry(edge.dst).or_default().push(edge.src);
            }
        }
        #[cfg(debug_assertions)]
        assert_reducible(&cfg, &dom_tree);

        let mut loops = vec![];
        for (header, latches) in back_edges {
            for &latch in &latches {
                debug_assert!(
                    cfg.successors_of(latch).contains(&header),
                    "loop latch must have an edge to its header"
                );
                debug_assert!(
                    dom_tree.dominates(header, latch),
                    "loop header must dominate every latch"
                );
            }

            // A self-loop has the header itself as its latch and needs no
            // predecessor walk beyond the header.
            let mut worklist =
                VecDeque::from_iter(latches.iter().filter(|&&block| block != header).copied());
            let mut body = FxHashSet::from_iter(latches.iter().copied());
            body.insert(header);
            while let Some(block) = worklist.pop_front() {
                for &pred in cfg.predecessors_of(block) {
                    // prevent re-explore and over-explore (beyond header block)
                    if body.insert(pred) {
                        debug_assert!(
                            dom_tree.dominates(header, pred),
                            "natural loop contains a block not dominated by its header"
                        );
                        worklist.push_back(pred);
                    }
                }
            }
            debug_assert!(body.contains(&header));
            debug_assert!(latches.iter().all(|latch| body.contains(latch)));
            debug_assert!(
                body.iter().all(|&block| dom_tree.dominates(header, block)),
                "loop header must dominate every block in its natural loop"
            );
            loops.push(Loop {
                header,
                body,
                latches,
            });
        }

        // sort the loops from small to big.
        loops.sort_unstable_by_key(|l| l.body.len());
        #[cfg(debug_assertions)]
        assert_laminar(&loops);

        let mut block_to_inner_loop = FxHashMap::default();
        block_to_inner_loop.reserve(cfg.block_count());
        let mut direct_parent = Vec::with_capacity(loops.len());
        for i in 0..loops.len() {
            direct_parent.push(i);
        }
        for (i, l1) in loops.iter().enumerate() {
            l1.body.iter().for_each(|&bb| {
                block_to_inner_loop.entry(bb).or_insert(i);
            });
            if let Some((j, _)) = loops
                .iter()
                .enumerate()
                .skip(i + 1)
                .find(|&(_j, l2)| l2.contains(l1.header))
            {
                direct_parent[i] = j;
            }
        }
        let loop_index = FxHashMap::from_iter(loops.iter().enumerate().map(|(i, l)| (l.header, i)));
        #[cfg(debug_assertions)]
        debug_assert_eq!(
            loop_index.len(),
            loops.len(),
            "each natural loop must have a unique header"
        );

        let analysis = LoopAnalysis {
            loops,
            direct_parent,
            loop_index,
            block_to_inner_loop,
        };
        #[cfg(debug_assertions)]
        analysis.verify(&cfg, &dom_tree);
        (cfg, dom_tree, analysis)
    }

    #[cfg(debug_assertions)]
    fn verify(&self, cfg: &utils::cfg::CFG, dom_tree: &dom_tree::v2::DominanceTree) {
        debug_assert_eq!(self.direct_parent.len(), self.loops.len());
        debug_assert_eq!(self.loop_index.len(), self.loops.len());

        for (index, looop) in self.loops.iter().enumerate() {
            debug_assert_eq!(self.loop_index.get(&looop.header), Some(&index));
            debug_assert!(looop.body.contains(&looop.header));
            debug_assert!(!looop.latches.is_empty());
            for &latch in &looop.latches {
                debug_assert!(looop.body.contains(&latch));
                debug_assert!(cfg.successors_of(latch).contains(&looop.header));
            }
            debug_assert!(
                looop
                    .body
                    .iter()
                    .all(|&block| dom_tree.dominates(looop.header, block))
            );

            let parent = self.direct_parent[index];
            if parent == index {
                debug_assert!(
                    self.loops
                        .iter()
                        .enumerate()
                        .all(|(other, candidate)| other == index
                            || !looop
                                .body
                                .iter()
                                .all(|block| candidate.body.contains(block))),
                    "a root loop must not be contained in another loop"
                );
            } else {
                debug_assert!(parent < self.loops.len());
                let parent_loop = &self.loops[parent];
                debug_assert!(looop.body.len() < parent_loop.body.len());
                debug_assert!(
                    looop
                        .body
                        .iter()
                        .all(|block| parent_loop.body.contains(block))
                );
                debug_assert!(
                    self.loops
                        .iter()
                        .enumerate()
                        .all(|(candidate_index, candidate)| {
                            candidate_index == index
                                || candidate_index == parent
                                || !looop
                                    .body
                                    .iter()
                                    .all(|block| candidate.body.contains(block))
                                || parent_loop.body.len() <= candidate.body.len()
                        }),
                    "direct parent must be the smallest loop containing its child"
                );
            }
        }

        for (&block, &index) in &self.block_to_inner_loop {
            debug_assert!(index < self.loops.len());
            debug_assert!(self.loops[index].contains(block));
            debug_assert!(
                self.loops.iter().enumerate().all(|(other, candidate)| {
                    !candidate.contains(block)
                        || self.loops[index].body.len() <= candidate.body.len()
                        || other == index
                }),
                "block must map to its innermost loop"
            );
        }
    }
}

#[cfg(debug_assertions)]
fn assert_laminar(loops: &[Loop]) {
    for (index, lhs) in loops.iter().enumerate() {
        for rhs in loops.iter().skip(index + 1) {
            let overlaps = lhs.body.iter().any(|block| rhs.body.contains(block));
            if !overlaps {
                continue;
            }
            let lhs_in_rhs = lhs.body.iter().all(|block| rhs.body.contains(block));
            let rhs_in_lhs = rhs.body.iter().all(|block| lhs.body.contains(block));
            debug_assert!(
                lhs_in_rhs || rhs_in_lhs,
                "natural loops in a reducible CFG must be nested or disjoint"
            );
            debug_assert_ne!(
                lhs.body, rhs.body,
                "distinct natural loops must not have identical block sets"
            );
        }
    }
}

#[cfg(debug_assertions)]
fn assert_reducible(cfg: &utils::cfg::CFG, dom_tree: &dom_tree::v2::DominanceTree) {
    debug_assert_eq!(cfg.entry(), dom_tree.entry());
    debug_assert!(cfg.blocks().iter().all(|&block| dom_tree.contains(block)));

    let mut indegrees = FxHashMap::default();
    for &block in cfg.blocks() {
        indegrees.insert(block, 0usize);
    }
    for edge in cfg.edges() {
        if !dom_tree.dominates(edge.dst, edge.src) {
            indegrees.entry(edge.dst).and_modify(|degree| *degree += 1);
        }
    }

    let mut worklist = VecDeque::from_iter(
        indegrees
            .iter()
            .filter_map(|(&block, &degree)| (degree == 0).then_some(block)),
    );
    let mut visited = 0;
    while let Some(block) = worklist.pop_front() {
        visited += 1;
        for &successor in cfg.successors_of(block) {
            if dom_tree.dominates(successor, block) {
                continue;
            }
            let degree = indegrees
                .get_mut(&successor)
                .expect("every successor must be reachable");
            *degree -= 1;
            if *degree == 0 {
                worklist.push_back(successor);
            }
        }
    }
    debug_assert_eq!(
        visited,
        cfg.block_count(),
        "irreducible CFG is not supported by natural loop analysis"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NestedLoops {
        outer_header: BasicBlock,
        inner_header: BasicBlock,
        inner_body: BasicBlock,
        outer_latch: BasicBlock,
        exit: BasicBlock,
    }

    fn build_nested_loops() -> (Program, Function, NestedLoops) {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "nested_loops".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let outer_header = data
            .new_basic_block()
            .basic_block("outer_header".into(), vec![]);
        let inner_header = data
            .new_basic_block()
            .basic_block("inner_header".into(), vec![]);
        let inner_body = data
            .new_basic_block()
            .basic_block("inner_body".into(), vec![]);
        let after_inner = data
            .new_basic_block()
            .basic_block("after_inner".into(), vec![]);
        let outer_latch = data
            .new_basic_block()
            .basic_block("outer_latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [
            outer_header,
            inner_header,
            inner_body,
            after_inner,
            outer_latch,
            exit,
        ] {
            data.layout_mut().push_bb_back(block);
        }

        let condition = data.new_local_inst().integer(1);
        let entry_jump = data.new_local_inst().jump(outer_header, vec![]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let outer_branch =
            data.new_local_inst()
                .branch(condition, inner_header, vec![], exit, vec![]);
        data.layout_mut().insert_inst(outer_header, outer_branch);
        let inner_branch =
            data.new_local_inst()
                .branch(condition, inner_body, vec![], after_inner, vec![]);
        data.layout_mut().insert_inst(inner_header, inner_branch);
        let inner_backedge = data.new_local_inst().jump(inner_header, vec![]);
        data.layout_mut().insert_inst(inner_body, inner_backedge);
        let after_inner_jump = data.new_local_inst().jump(outer_latch, vec![]);
        data.layout_mut().insert_inst(after_inner, after_inner_jump);
        let outer_backedge = data.new_local_inst().jump(outer_header, vec![]);
        data.layout_mut().insert_inst(outer_latch, outer_backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        (
            program,
            function,
            NestedLoops {
                outer_header,
                inner_header,
                inner_body,
                outer_latch,
                exit,
            },
        )
    }

    #[test]
    fn exposes_direct_parent_indices_and_loops() {
        let (program, function, blocks) = build_nested_loops();
        let (_cfg, _dom_tree, loops) = LoopAnalysis::new(program.func_data(function));
        let inner = loops.loop_index(blocks.inner_header).unwrap();
        let outer = loops.loop_index(blocks.outer_header).unwrap();

        assert_eq!(loops.parent_loop_index(inner), Some(outer));
        assert_eq!(
            loops.parent_loop(inner).map(Loop::header),
            Some(blocks.outer_header)
        );
        assert_eq!(loops.parent_loop_index(outer), None);
        assert!(loops.parent_loop(outer).is_none());
    }

    #[test]
    fn lists_containing_loops_from_inner_to_outer() {
        let (program, function, blocks) = build_nested_loops();
        let (_cfg, _dom_tree, loops) = LoopAnalysis::new(program.func_data(function));
        let inner = loops.loop_index(blocks.inner_header).unwrap();
        let outer = loops.loop_index(blocks.outer_header).unwrap();

        assert_eq!(loops.min_loop_contain_index(blocks.inner_body), Some(inner));
        assert_eq!(
            loops
                .containing_loop_indices(blocks.inner_body)
                .collect::<Vec<_>>(),
            vec![inner, outer]
        );
        assert_eq!(
            loops
                .containing_loops(blocks.inner_body)
                .map(Loop::header)
                .collect::<Vec<_>>(),
            vec![blocks.inner_header, blocks.outer_header]
        );
        assert_eq!(
            loops
                .containing_loop_indices(blocks.outer_latch)
                .collect::<Vec<_>>(),
            vec![outer]
        );
        assert_eq!(loops.min_loop_contain_index(blocks.exit), None);
        assert_eq!(loops.containing_loops(blocks.exit).count(), 0);
    }
}
