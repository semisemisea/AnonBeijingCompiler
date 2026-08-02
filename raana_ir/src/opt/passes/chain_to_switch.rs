//! Convert linear `if (x == k) ...` chains into a balanced binary decision
//! tree.
//!
//! The classic shape produced by C code like
//!
//! ```text
//! if (x == 1) return rotl(v, 1);
//! if (x == 2) return rotl(v, 2);
//! ...
//! if (x == 8) return rotl(v, 8);
//! return v;
//! ```
//!
//! is a chain of N equality tests and branches, which costs N comparisons on
//! the worst path and ~N/2 on average. A balanced decision tree over the same
//! distinct case values costs at most ~log2(N) + 1 comparisons (4 for 8
//! cases, matching clang's `rotrN`) and ~2.6 on average for 8 cases.
//!
//! Each tree node is a pair of blocks:
//!
//! ```text
//! check_k:  %t = eq x, k;    br %t, handler_k, split_k
//! split_k:  %u = lt x, k;    br %u, left_subtree, right_subtree
//! ```
//!
//! The AArch64 backend fuses the two comparisons of a (check, split) pair
//! into a single `cmp; beq; blo; b` sequence (`chain_fusion` pass), which is
//! exactly clang's emitted shape.
//!
//! Soundness: the transformation only fires on blocks whose entire content is
//! the equality test plus the terminator, so every handler is reached through
//! the same edge arguments as before. The tested value `x` keeps its original
//! defining block (a chain block cannot define it: its only instructions are
//! the test and the terminator), which dominates every tree block because the
//! tree is entered only through the chain head's predecessors. A chain rooted
//! at the function entry keeps the entry block as the tree root so the
//! function parameters survive.

use crate::opt::prelude::*;

pub struct ChainToSwitch;

/// A chain block must contain exactly the equality test and the terminator.
const CHAIN_BLOCK_INSTS: usize = 2;

/// A chain shorter than this is already as cheap as a tree.
const MIN_CHAIN_LEN: usize = 4;

struct Link {
    block: BasicBlock,
    k: i32,
    handler: BasicBlock,
    handler_args: Vec<Inst>,
    /// The false target: the next chain block, or the default.
    next: BasicBlock,
    next_args: Vec<Inst>,
}

impl Pass for ChainToSwitch {
    fn run_on(&self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().entry_bb().is_none() {
            return false;
        }
        let blocks: Vec<BasicBlock> = data
            .layout()
            .basicblocks()
            .iter()
            .map(|layout| layout.bb())
            .collect();
        let mut changed = false;
        for head in blocks {
            // An earlier conversion may have removed this block.
            if !data
                .layout()
                .basicblocks()
                .iter()
                .any(|layout| layout.bb() == head)
            {
                continue;
            }
            if Self::convert_chain(data, head) {
                changed = true;
            }
        }
        changed
    }
}

impl ChainToSwitch {
    /// Try to turn the equality chain starting at `head` into a decision tree.
    fn convert_chain(data: &mut ArenaContextMut<'_>, head: BasicBlock) -> bool {
        let Some((x, links, default, default_args)) = Self::detect_chain(data, head) else {
            return false;
        };
        let mut sorted = links;
        sorted.sort_by_key(|link| link.k);

        // The chain head becomes the tree root in place, so it keeps its
        // block parameters (function parameters for an entry-rooted chain,
        // inlined arguments for an inlined chain) and its incoming edges
        // need no rewiring.
        let old_head_insts: Vec<Inst> = data
            .layout()
            .basicblock(head)
            .insts()
            .iter()
            .copied()
            .collect();

        let tree_root = Self::build_tree(
            data,
            x,
            &sorted,
            default,
            default_args,
            0,
            sorted.len(),
            head,
        );
        assert_eq!(tree_root, head, "the tree root must land in the chain head");

        // Drop the head's original test; the rest of the chain is replaced
        // by the tree.
        for inst in old_head_insts {
            data.remove_layout_inst(head, inst);
        }
        for link in &sorted {
            if link.block != head {
                data.remove_layout_basicblock(link.block);
            }
        }
        true
    }

    /// Scan the equality chain starting at `head`. Returns the tested value,
    /// the chain links, the default block, and the default edge arguments.
    fn detect_chain(
        data: &ArenaContextMut<'_>,
        head: BasicBlock,
    ) -> Option<(Inst, Vec<Link>, BasicBlock, Vec<Inst>)> {
        let mut x: Option<Inst> = None;
        let mut links: Vec<Link> = Vec::new();
        let mut cur = head;
        loop {
            let layout = data.layout().basicblock(cur);
            if layout.insts().len() != CHAIN_BLOCK_INSTS {
                break;
            }
            let insts: Vec<Inst> = layout.insts().iter().copied().collect();
            let InstKind::Binary(binary) = data.inst_data(insts[0]).kind() else {
                break;
            };
            let InstKind::Branch(branch) = data.inst_data(insts[1]).kind() else {
                break;
            };
            if binary.op() != BinaryOp::Eq || branch.cond() != insts[0] {
                break;
            }
            // The tested value and the case constant (either side may carry
            // the constant).
            let (lhs, rhs) = (binary.lhs(), binary.rhs());
            let (value, k) = match data.inst_data(rhs).kind() {
                InstKind::Integer(i) => (lhs, i.value()),
                _ => match data.inst_data(lhs).kind() {
                    InstKind::Integer(i) => (rhs, i.value()),
                    _ => break,
                },
            };
            if let Some(expected) = x {
                if expected != value {
                    break;
                }
            } else {
                x = Some(value);
            }
            if links.iter().any(|link| link.k == k) {
                break;
            }
            let next = branch.f_target();
            if next == cur {
                break;
            }
            links.push(Link {
                block: cur,
                k,
                handler: branch.t_target(),
                handler_args: branch.t_args().to_vec(),
                next,
                next_args: branch.f_args().to_vec(),
            });
            cur = next;
        }
        if links.len() < MIN_CHAIN_LEN {
            return None;
        }
        // Chain blocks must not carry block parameters (the tree reuses the
        // tested value directly and carries each handler's edge arguments).
        // The chain head is exempt when it is the function entry: it survives
        // as the tree root and keeps its parameters.
        // Chain blocks other than the head must not carry block parameters:
        // the tree's edges feed them no arguments. The head keeps its
        // parameters (it survives as the tree root), so its parameters stay
        // valid without any edge rewiring.
        if links
            .iter()
            .skip(1)
            .any(|link| !data.bb_data(link.block).params().is_empty())
        {
            return None;
        }
        let last = links.last().unwrap();
        let (default, default_args) = (last.next, last.next_args.clone());
        Some((x.unwrap(), links, default, default_args))
    }

    /// Build the decision tree over the sorted links in `[lo, hi)`.
    ///
    /// Returns the entry block of the subtree. When `lo == 0` and
    /// `hi == links.len()`, the root is built inside `into_block` (the chain
    /// head), preserving the entry block when the chain starts there.
    fn build_tree(
        data: &mut ArenaContextMut<'_>,
        x: Inst,
        links: &[Link],
        default: BasicBlock,
        default_args: Vec<Inst>,
        lo: usize,
        hi: usize,
        into_block: BasicBlock,
    ) -> BasicBlock {
        if hi - lo == 1 {
            let link = &links[lo];
            let check = if lo == 0 && hi == links.len() {
                into_block
            } else {
                let bb = data
                    .new_basic_block()
                    .basic_block(format!("chain_case_{}", link.k), vec![]);
                data.layout_mut().push_bb_back(bb);
                bb
            };
            let k_inst = data.new_local_inst().integer(link.k);
            let eq = data.new_local_inst().binary(BinaryOp::Eq, x, k_inst);
            data.layout_mut().insert_inst(check, eq);
            let br = data.new_local_inst().branch(
                eq,
                link.handler,
                link.handler_args.clone(),
                default,
                default_args,
            );
            data.layout_mut().insert_inst(check, br);
            return check;
        }

        let mid = lo + (hi - lo) / 2;
        let key = links[mid].k;
        let (left_target, left_args) = if lo < mid {
            (
                Self::build_tree(data, x, links, default, default_args.clone(), lo, mid, into_block),
                vec![],
            )
        } else {
            (default, default_args.clone())
        };
        let (right_target, right_args) = if mid + 1 < hi {
            (
                Self::build_tree(
                    data,
                    x,
                    links,
                    default,
                    default_args.clone(),
                    mid + 1,
                    hi,
                    into_block,
                ),
                vec![],
            )
        } else {
            (default, default_args.clone())
        };

        let check = if lo == 0 && hi == links.len() {
            into_block
        } else {
            let bb = data
                .new_basic_block()
                .basic_block(format!("chain_check_{}", key), vec![]);
            data.layout_mut().push_bb_back(bb);
            bb
        };
        let key_inst = data.new_local_inst().integer(key);
        let eq = data.new_local_inst().binary(BinaryOp::Eq, x, key_inst);
        data.layout_mut().insert_inst(check, eq);

        let split = {
            let bb = data
                .new_basic_block()
                .basic_block(format!("chain_split_{}", key), vec![]);
            data.layout_mut().push_bb_back(bb);
            bb
        };
        let lt = data.new_local_inst().binary(BinaryOp::Lt, x, key_inst);
        data.layout_mut().insert_inst(split, lt);
        let split_br = data
            .new_local_inst()
            .branch(lt, left_target, left_args, right_target, right_args);
        data.layout_mut().insert_inst(split, split_br);

        let check_br = data.new_local_inst().branch(
            eq,
            links[mid].handler,
            links[mid].handler_args.clone(),
            split,
            vec![],
        );
        data.layout_mut().insert_inst(check, check_br);
        check
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        Program,
        arena::Arena,
        builder::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
    };

    fn build_chain(
        program: &mut Program,
        len: usize,
    ) -> (Function, BasicBlock, Vec<BasicBlock>, BasicBlock) {
        let func = program.new_function(Type::get_i32(), "chain".to_owned(), vec![Type::get_i32()]);
        let (entry, exit) = {
            let data = program.func_data_mut(func);
            let entry = data
                .new_basic_block()
                .basic_block("entry".to_owned(), vec![Type::get_i32()]);
            let exit = data.new_basic_block().basic_block("exit".to_owned(), vec![]);
            data.layout_mut().push_bb_back(entry);
            data.layout_mut().push_bb_back(exit);
            (entry, exit)
        };
        let mut cases = Vec::new();
        {
            let data = program.func_data_mut(func);
            let x = data.bb_data(entry).params()[0];
            // Pre-create every chain block so the false target of a test is
            // exactly the next chain block.
            let chain_blocks: Vec<BasicBlock> = (1..=len)
                .map(|k| {
                    if k == 1 {
                        entry
                    } else {
                        let bb = data.new_basic_block().basic_block(format!("case_{k}"), vec![]);
                        data.layout_mut().push_bb_back(bb);
                        bb
                    }
                })
                .collect();
            for (k, &case) in chain_blocks.iter().enumerate() {
                let k = k + 1;
                let k_inst = data.new_local_inst().integer(k as i32);
                let test = data.new_local_inst().binary(BinaryOp::Eq, x, k_inst);
                data.layout_mut().insert_inst(case, test);
                let handler = data
                    .new_basic_block()
                    .basic_block(format!("ret_{k}"), vec![]);
                data.layout_mut().push_bb_back(handler);
                let value = data.new_local_inst().integer((k * 2) as i32);
                data.layout_mut().insert_inst(handler, value);
                let next = if k < len { chain_blocks[k] } else { exit };
                let br = data.new_local_inst().branch(test, handler, vec![], next, vec![]);
                data.layout_mut().insert_inst(case, br);
                cases.push(case);
            }
        }
        (func, entry, cases, exit)
    }

    #[test]
    fn converts_an_equality_chain_into_a_balanced_tree() {
        let mut program = Program::new();
        let (func, entry, cases, exit) = build_chain(&mut program, 6);
        let mut ctx = crate::opt::pass::ArenaContextMut {
            program: &mut program,
            curr_func: Some(func),
        };
        assert!(ChainToSwitch.run_on(&mut ctx));
        let data = ctx.curr_func_data();
        let block_list: Vec<BasicBlock> = data
            .layout()
            .basicblocks()
            .iter()
            .map(|layout| layout.bb())
            .collect();
        // The old chain blocks are gone (except the entry, which survives as
        // the tree root), replaced by the tree.
        for case in cases {
            if case != entry {
                assert!(!block_list.contains(&case));
            }
        }
        // The entry block survives as the tree root and still tests x.
        let entry_insts: Vec<Inst> = data
            .layout()
            .basicblock(entry)
            .insts()
            .iter()
            .copied()
            .collect();
        assert!(matches!(
            data.inst_data(entry_insts[0]).kind(),
            InstKind::Binary(b) if b.op() == BinaryOp::Eq
        ));
        // The default block is still reachable from the tree.
        assert!(data.bb_data(exit).used_by().len() >= 1);
    }

    #[test]
    fn rejects_chains_shorter_than_four_links() {
        let mut program = Program::new();
        let (func, _, cases, _) = build_chain(&mut program, 3);
        let mut ctx = crate::opt::pass::ArenaContextMut {
            program: &mut program,
            curr_func: Some(func),
        };
        assert!(!ChainToSwitch.run_on(&mut ctx));
        let data = ctx.curr_func_data();
        for case in cases {
            assert!(data.layout().basicblock(case).insts().len() == 2);
        }
    }
}
