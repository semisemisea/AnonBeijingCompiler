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
        match rpo1.cmp(&rpo2) {
            core::cmp::Ordering::Greater => node1 = idom[node1.index()],
            core::cmp::Ordering::Less => node2 = idom[node2.index()],
            core::cmp::Ordering::Equal => {}
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
