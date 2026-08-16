/*
 * Adapted from regalloc2 0.15.1 src/domtree.rs, which derives from
 * regalloc.rs. Released under the Apache License 2.0 with LLVM Exception.
 * See the repository LICENSE for the full license text. Local modifications
 * use taki_mir index types and standard vectors.
 */

//! Dominator tree calculation using Cooper, Harvey, and Kennedy's algorithm.

use crate::reg_alloc::index::Block;

fn merge_sets(
    idom: &[Block],
    block_to_rpo: &[Option<u32>],
    mut node1: Block,
    mut node2: Block,
) -> Block {
    while node1 != node2 {
        if node1.is_invalid() || node2.is_invalid() {
            return Block::invalid();
        }
        let rpo1 = block_to_rpo[node1.index()].unwrap();
        let rpo2 = block_to_rpo[node2.index()].unwrap();
        if rpo1 > rpo2 {
            node1 = idom[node1.index()];
        } else if rpo2 > rpo1 {
            node2 = idom[node2.index()];
        }
    }
    node1
}

pub(super) fn calculate<'a, PredFn: Fn(Block) -> &'a [Block]>(
    num_blocks: usize,
    preds: PredFn,
    postorder: &[Block],
    block_to_rpo: &mut Vec<Option<u32>>,
    out: &mut Vec<Block>,
    start: Block,
) {
    // 立即支配者（idom）迭代算法：RPO 序下，每个节点的 idom 是其
    // 前驱中 RPO 最靠前者的 idom 链合并（merge_sets 沿 idom 链爬升取
    // 两个前驱的共同祖先）。反复迭代直到不动点——标准数据流算法，
    // 图规模小（后端基本块数有限）时收敛快。
    // 结果存 out（idom 表，entry 的 idom 置 invalid 表示无前驱）。
    block_to_rpo.clear();
    block_to_rpo.resize(num_blocks, None);
    for (index, &block) in postorder.iter().rev().enumerate() {
        block_to_rpo[block.index()] = Some(index as u32);
    }

    out.clear();
    out.resize(num_blocks, Block::invalid());
    out[start.index()] = start;

    let mut changed = true;
    while changed {
        changed = false;
        for &node in postorder.iter().rev() {
            let rpo = block_to_rpo[node.index()].unwrap();
            let mut parent = Block::invalid();
            for &pred in preds(node) {
                if block_to_rpo[pred.index()].is_some_and(|pred_rpo| pred_rpo < rpo) {
                    parent = pred;
                    break;
                }
            }
            if parent.is_valid() {
                for &pred in preds(node) {
                    if pred != parent && out[pred.index()].is_valid() {
                        parent = merge_sets(out, block_to_rpo, parent, pred);
                    }
                }
            }
            if parent.is_valid() && parent != out[node.index()] {
                out[node.index()] = parent;
                changed = true;
            }
        }
    }

    out[start.index()] = Block::invalid();
}

pub(super) fn dominates(idom: &[Block], a: Block, mut b: Block) -> bool {
    loop {
        if a == b {
            return true;
        }
        if b.is_invalid() {
            return false;
        }
        b = idom[b.index()];
    }
}
