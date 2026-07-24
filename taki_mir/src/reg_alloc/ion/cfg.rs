/*
 * Adapted from regalloc2 0.15.1 src/cfg.rs.
 *
 * Released under the Apache License 2.0 with LLVM Exception. See the
 * repository LICENSE for the full license text. Local modifications remove
 * fastalloc-specific validation and use taki_mir program indices.
 */

//! CFG and dominator information required by Ion allocation.

use smallvec::SmallVec;

use crate::reg_alloc::{function::Function, index::Block, reg::ProgPoint};

use super::{domtree, postorder};

#[derive(Debug, Default)]
pub struct CFGInfoCtx {
    visited: Vec<bool>,
    block_to_rpo: Vec<Option<u32>>,
    backedge: Vec<u32>,
}

#[derive(Debug, Default)]
pub struct CFGInfo {
    pub postorder: Vec<Block>,
    pub domtree: Vec<Block>,
    pub insn_block: Vec<Block>,
    pub block_entry: Vec<ProgPoint>,
    pub block_exit: Vec<ProgPoint>,
    pub approx_loop_depth: Vec<u32>,
}

impl CFGInfo {
    pub fn new<F: Function>(function: &F) -> Result<Self, String> {
        let mut ctx = CFGInfoCtx::default();
        let mut info = Self::default();
        info.init(function, &mut ctx)?;
        Ok(info)
    }

    pub fn init<F: Function>(&mut self, function: &F, ctx: &mut CFGInfoCtx) -> Result<(), String> {
        let num_blocks = function.num_blocks();
        if num_blocks == 0 {
            return Err("CFG has no blocks".to_owned());
        }
        postorder::calculate(
            num_blocks,
            function.entry_block(),
            &mut ctx.visited,
            &mut self.postorder,
            |block| function.block_succs(block),
        )?;
        domtree::calculate(
            num_blocks,
            |block| function.block_preds(block),
            &self.postorder,
            &mut ctx.block_to_rpo,
            &mut self.domtree,
            function.entry_block(),
        );

        self.insn_block.clear();
        self.insn_block
            .resize(function.num_insts(), Block::invalid());
        self.block_entry.clear();
        self.block_entry
            .resize(num_blocks, ProgPoint::before(u32::MAX));
        self.block_exit.clear();
        self.block_exit
            .resize(num_blocks, ProgPoint::before(u32::MAX));
        ctx.backedge.clear();
        ctx.backedge.resize(num_blocks * 2, 0);
        let (backedge_in, backedge_out) = ctx.backedge.split_at_mut(num_blocks);

        for index in 0..num_blocks {
            let block = Block::new(index);
            let insns = function.block_insns(block);
            if insns.len() == 0 {
                return Err(format!("CFG block {index} has no instructions"));
            }
            for inst in insns.iter() {
                self.insn_block[inst.index()] = block;
            }
            self.block_entry[index] = ProgPoint::before(insns.first().raw_u32());
            self.block_exit[index] = ProgPoint::after(insns.last().raw_u32());
            for &succ in function.block_succs(block) {
                if succ.index() <= index {
                    backedge_in[succ.index()] += 1;
                    backedge_out[index] += 1;
                }
            }
        }

        self.approx_loop_depth.clear();
        let mut backedge_stack: SmallVec<[u32; 4]> = SmallVec::new();
        let mut depth = 0;
        for index in 0..num_blocks {
            if backedge_in[index] > 0 {
                depth += 1;
                backedge_stack.push(backedge_in[index]);
            }
            self.approx_loop_depth.push(depth);
            while !backedge_stack.is_empty() && backedge_out[index] > 0 {
                backedge_out[index] -= 1;
                *backedge_stack.last_mut().unwrap() -= 1;
                if *backedge_stack.last().unwrap() == 0 {
                    depth -= 1;
                    backedge_stack.pop();
                }
            }
        }
        Ok(())
    }

    pub fn dominates(&self, a: Block, b: Block) -> bool {
        domtree::dominates(&self.domtree, a, b)
    }
}
