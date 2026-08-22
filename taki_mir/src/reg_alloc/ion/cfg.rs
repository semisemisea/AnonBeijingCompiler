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

    // 一次扫描建立分配器需要的全部控制流信息：
    //   postorder —— 块的后序（活跃区间分析按此顺序合并）；
    //   domtree  —— 立即支配者表（支配查询）；
    //   insn_block / block_entry / block_exit —— 指令↔块归属与块的
    //   程序点边界（活跃区间跨块合并用）；
    //   approx_loop_depth —— 每块近似循环深度（spill 权重的热度加成）；
    // 同时校验**关键边已分裂**（分裂后的边复制在源块尾执行，仅当后继
    // 单前驱或源终结符无普通操作数时安全，否则报错——Ion 要求输入
    // 无未分裂关键边）。
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

            // Edge copies execute at the source tail. They are safe only when
            // the successor has one predecessor, or when the source terminator
            // has no ordinary operands that a copy could overwrite.
            let preds =
                function.block_preds(block).len() + usize::from(block == function.entry_block());
            if preds > 1 {
                for &pred in function.block_preds(block) {
                    if function.block_succs(pred).len() > 1 {
                        return Err(format!(
                            "unsplit critical edge from block {} to block {}",
                            pred.index(),
                            block.index()
                        ));
                    }
                }
            }
            let requires_operand_free_terminator =
                function.block_succs(block).iter().any(|&succ| {
                    function.block_preds(succ).len() + usize::from(succ == function.entry_block())
                        > 1
                });
            if requires_operand_free_terminator && !function.inst_operands(insns.last()).is_empty()
            {
                return Err(format!(
                    "block {} terminator instruction {} has register operands on an edge that may require copies",
                    block.index(),
                    insns.last().index()
                ));
            }
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
