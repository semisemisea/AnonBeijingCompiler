/*
 * Adapted from regalloc2 0.15.1 src/postorder.rs.
 *
 * Released under the Apache License 2.0 with LLVM Exception. See the
 * repository LICENSE for the full license text. Local modifications replace
 * regalloc2 error and allocation utilities with taki_mir equivalents.
 */

//! Iterative postorder traversal for allocator CFG analysis.
//!
//! 从入口块出发对 CFG 做**迭代式后序遍历**（显式栈代替递归，避免深 CFG
//! 栈溢出），产出块的后序列表：每个块都排在它所有后继之后。分配器后续
//! 阶段按这个顺序消费它：
//!
//! - 取其反序即得 **RPO（反后序）**：`domtree.rs` 用 RPO 给块编号并迭代
//!   计算支配树（Cooper–Harvey–Kennedy 算法）；
//! - `liveranges.rs` 用它初始化活跃性分析的 worklist（后序 = 子块在前），
//!   让活跃区间分析和移动解析等阶段拥有与 CFG 结构一致的块处理次序。
//!
//! 术语（后继、回边、支配等）见 `docs/offline-handbook/glossary.md`。

use smallvec::SmallVec;

use crate::reg_alloc::index::Block;

/// 计算从 `entry` 出发**可达子图**的后序列表（迭代 DFS）。
///
/// - `num_blocks`：函数总块数，用于边界校验和 `visited` 定长；
/// - `succ_blocks`：CFG 出边查询——给定块返回其后继块切片；
/// - `visited` / `out`：调用方提供的可复用缓冲，遍历开始前会被清空；
///   返回时 `out` 是后序排列的可达块，每个块恰好出现一次。
///
/// 显式栈模拟递归 DFS：栈帧是"块 + 后继迭代器"。迭代器耗尽意味着该块的
/// 所有后继都已处理完毕，此刻把它追加到 `out`——这正是后序位置。已访问
/// 的后继直接跳过，环因此被截断，所以结果对**去掉回边后的 DAG 是逆拓扑
/// 序**：任意非回边 `u -> v`，`v` 都先于 `u` 出现在 `out` 中（反过来说，
/// `out` 的反序 RPO 中 `u` 在 `v` 之前）。入口或后继块下标越界（CFG 不
/// 完整）时返回 `Err`。
pub(super) fn calculate<'a, SuccFn: Fn(Block) -> &'a [Block]>(
    num_blocks: usize,
    entry: Block,
    visited: &mut Vec<bool>,
    out: &mut Vec<Block>,
    succ_blocks: SuccFn,
) -> Result<(), String> {
    // 显式栈帧：块 + 指向后继切片当前位置的迭代器。迭代器等价于递归版
    // DFS 里 `for succ in succs` 的循环现场，恢复帧即可继续处理后继。
    struct State<'a> {
        block: Block,
        succs: core::slice::Iter<'a, Block>,
    }

    // 入口块必须合法且落在 `num_blocks` 范围内，否则遍历无从开始。
    if !entry.is_valid() || entry.index() >= num_blocks {
        return Err(format!("invalid CFG entry block {entry:?}"));
    }

    // 复用调用方缓冲：清空并按块数定长，避免每次遍历重新分配；
    // `out` 从空列表开始，由遍历过程累积。
    visited.clear();
    visited.resize(num_blocks, false);
    out.clear();

    // 从入口开始 DFS：先标记已访问并压栈，主循环随后深入它的后继。
    // 栈用 `SmallVec`（容量 64）：常见 CFG 深度内零堆分配。
    let mut stack: SmallVec<[State<'_>; 64]> = SmallVec::new();
    visited[entry.index()] = true;
    stack.push(State {
        block: entry,
        succs: succ_blocks(entry).iter(),
    });

    // 主循环：只改动栈顶帧（`last_mut`），弹出/压入都发生在栈顶——
    // 这正是深度优先：新发现的后继总是最先被继续探索。
    while let Some(state) = stack.last_mut() {
        if let Some(&succ) = state.succs.next() {
            if !succ.is_valid() || succ.index() >= num_blocks {
                return Err(format!("invalid CFG successor block {succ:?}"));
            }
            // 后继尚未访问：标记并压栈继续深入；已访问过的直接跳过——
            // 回边/交叉边不会导致重复入栈，环在此被截断。
            if !visited[succ.index()] {
                visited[succ.index()] = true;
                stack.push(State {
                    block: succ,
                    succs: succ_blocks(succ).iter(),
                });
            }
        } else {
            // 后继迭代器耗尽：该块的所有可达后继都已处理完，此刻记录它。
            // 子块总是先于父块"完成"，因此 `out` 中子块必然排在父块之前，
            // 满足后序定义——这也是活跃性分析 worklist 想要的初值次序。
            out.push(state.block);
            stack.pop();
        }
    }

    Ok(())
}
