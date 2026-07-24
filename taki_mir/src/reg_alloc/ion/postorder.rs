/*
 * Adapted from regalloc2 0.15.1 src/postorder.rs.
 *
 * Released under the Apache License 2.0 with LLVM Exception. See the
 * repository LICENSE for the full license text. Local modifications replace
 * regalloc2 error and allocation utilities with taki_mir equivalents.
 */

//! Iterative postorder traversal for allocator CFG analysis.

use smallvec::SmallVec;

use crate::reg_alloc::index::Block;

pub(super) fn calculate<'a, SuccFn: Fn(Block) -> &'a [Block]>(
    num_blocks: usize,
    entry: Block,
    visited: &mut Vec<bool>,
    out: &mut Vec<Block>,
    succ_blocks: SuccFn,
) -> Result<(), String> {
    struct State<'a> {
        block: Block,
        succs: core::slice::Iter<'a, Block>,
    }

    if !entry.is_valid() || entry.index() >= num_blocks {
        return Err(format!("invalid CFG entry block {entry:?}"));
    }

    visited.clear();
    visited.resize(num_blocks, false);
    out.clear();

    let mut stack: SmallVec<[State<'_>; 64]> = SmallVec::new();
    visited[entry.index()] = true;
    stack.push(State {
        block: entry,
        succs: succ_blocks(entry).iter(),
    });

    while let Some(state) = stack.last_mut() {
        if let Some(&succ) = state.succs.next() {
            if !succ.is_valid() || succ.index() >= num_blocks {
                return Err(format!("invalid CFG successor block {succ:?}"));
            }
            if !visited[succ.index()] {
                visited[succ.index()] = true;
                stack.push(State {
                    block: succ,
                    succs: succ_blocks(succ).iter(),
                });
            }
        } else {
            out.push(state.block);
            stack.pop();
        }
    }

    Ok(())
}
