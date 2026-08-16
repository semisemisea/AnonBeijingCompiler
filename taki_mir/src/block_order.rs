//! The order of traversing basic blocks uses RPO of the dominance tree.
//!
//! ## 中文文档
//!
//! `BlockLoweringOrder` 决定一个函数的**块发射/遍历顺序**：把 RaanaIR
//! （HLIR）函数的 CFG 折叠成一列 `LoweredBlock`（原块 + 边块），后续的
//! lower、汇编发射都按这个顺序处理基本块；vcode 容器的块编号
//! （`MirBlockIndex`，即 `crate::reg_alloc::index::Block`）就是该序列的
//! 下标。
//!
//! ### 算法：支配树的 RPO 前序
//!
//! `BlockLoweringOrder::new` 的步骤（CFG/支配分析全部复用 `raana_ir`）：
//!
//! 1. `cfg::build_cfg_both` 从函数数据构建 CFG，返回两个图：`graph`
//!    （后继边）与 `prece`（前驱/反向边）；块用 `IDAllocator` 编号为
//!    `BId`。`cfg::rpo_path(&graph)` 求出普通 CFG 的 RPO（`plain_rpo`）；
//! 2. `dom_tree::idom(&prece, &plain_rpo)` 计算立即支配者，
//!    `dom_tree::build_dominance_tree` 把 idom 关系展开成支配树的孩子列表；
//! 3. 对支配树做**前序 DFS**（`domtree_dfs`，从入口 `BId` 0 开始），每个
//!    节点的孩子按各自在 CFG RPO 中的下标排序（`rpo_index`），得到
//!    `domtree_rpo`——这正是英文首行说的 "RPO of the dominance tree"；
//! 4. 把 `BId` 经 `bb_id.search_id` 映回 `HirBasicBlock`，得到块的发射
//!    顺序 `rpo`；
//! 5. 按 `rpo` 顺序把每个原块作为 `LoweredBlock::Orig` 压入
//!    `lowered_order`，并建立 `hlir_block_map`（`HirBasicBlock` → 原块
//!    下标）。对出度 > 1 的块（多路终结符），逐条后继检查
//!    `in_degree > 1 || 边带实参 || 目标块带参数`，命中则为该边追加一个
//!    `LoweredBlock::Edge { pred, succ, succ_idx }`，紧跟在原块之后；
//! 6. 最后构建 `lowered_succ_indices` / `lowered_succ_ranges`：每个
//!    lowered 块的后继下标表。`Orig` 块的条目额外附带其终结符
//!    `Option<HirInst>`（只有 `Branch` 才是 `Some`，`Jump`/`Return` 为
//!    `None`）；`Edge` 块固定单后继（指向目标原块）且不带终结符。
//!
//! `outgoing_block_args` 负责从 `Branch`（按 `succ_idx` 取 true/false 边）
//! 或 `Jump` 终结符中取出目标块参数对应的实参，并断言实参数量、类型与
//! 目标块 `params()` 一致。
//!
//! ### 为什么是这个顺序（正确性）
//!
//! - **RPO 性质**：CFG 上除回边外每条边都从 RPO 中较早的块指向较晚的
//!   块；支配树前序进一步保证每个块的立即支配者先于它出现。于是发射/
//!   lower 一个块时，它的所有支配者及其定义的值必然已经处理完毕；
//! - **循环友好**：循环头支配循环体，故 header 先于 body/回边块发射，
//!   回边跳转指向已经布局好的标签；兄弟分支按 CFG RPO 排序，布局确定、
//!   可预测；
//! - **边分裂**：多路终结符不能承担"边专属"的传参工作。临界边（目标
//!   有多个前驱）与带值边（`args`/`params` 非空）被拆成独立 `Edge` 块，
//!   参数搬运落到边块；`succ_idx` 保留原始后继下标，两条指向同一目标的
//!   不同边仍然可区分（见测试
//!   `keeps_distinct_branch_edges_to_the_same_parameterized_target`）。
//!
//! ### 谁在用
//!
//! - `VCodeBuilder::new(abi, block_order)`（`vcode/builder.rs`）：lower
//!   以本顺序构建 vcode 容器；
//! - `LowerBackend`（`taki_mir/src/lower.rs`）：按 `lowered_order()` 逐块
//!   lower，经 `succ_indices(block)` 解析后继，
//!   `collect_outgoing_block_args` / `lower_branch_blockparam_args_move`
//!   搬运边参数并用 `add_succ` 登记，对 `Edge` 块调用 `emit_long_jump`
//!   发射长跳转；`lowered_index_for_block` 用于定位入口块；
//! - `AsmWriter`（`taki_mir/src/emit.rs`）：按 `lowered_order()` 顺序
//!   `bind_label` 并逐条输出指令（即最终汇编的块布局）；
//! - 寄存器分配不直接读本模块：它消费的 vcode 容器在构建时已按本顺序
//!   编号（`MirBlockIndex` 就是 lowered order 的下标）。
//!
//! ### 验证
//!
//! 本文件 `mod tests` 用 `ArenaContext` 手工构造程序，检查
//! `lowered_order()` / `succ_indices()` 的输出：
//! - `keeps_distinct_branch_edges_to_the_same_parameterized_target`：
//!   diamond 的两条分支边指向同一带参 merge，两条边保持独立 `Edge` 块；
//! - `keeps_return_out_of_branch_metadata`：return 不是 CFG 分支，终结符
//!   位为 `None`、无后继；
//! - `preserves_loop_continue_break_and_critical_edges`：循环 continue 的
//!   参数经边块传递，break 临界边与 header→exit 临界边各有专属 `Edge` 块。
use std::ops::Range;

use raana_ir::opt::prelude::{IDAllocator, cfg, dom_tree};
use rustc_hash::FxHashMap;

pub type MirBlockIndex = crate::reg_alloc::index::Block;

use crate::prelude::*;

pub struct BlockLoweringOrder {
    lowered_order: Vec<LoweredBlock>,
    lowered_succ_indices: Vec<MirBlockIndex>,
    lowered_succ_ranges: Vec<(Option<HirInst>, Range<usize>)>,
    hlir_block_map: FxHashMap<HirBasicBlock, MirBlockIndex>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LoweredBlock {
    Orig {
        block: HirBasicBlock,
    },
    Edge {
        pred: HirBasicBlock,
        succ: HirBasicBlock,
        succ_idx: u32,
    },
}

impl LoweredBlock {
    pub fn orig_block(&self) -> Option<HirBasicBlock> {
        match self {
            LoweredBlock::Orig { block } => Some(*block),
            LoweredBlock::Edge { .. } => None,
        }
    }

    pub fn pred_block(&self) -> Option<HirBasicBlock> {
        match self {
            LoweredBlock::Edge { pred, .. } => Some(*pred),
            LoweredBlock::Orig { .. } => None,
        }
    }

    pub fn succ_block(&self) -> Option<HirBasicBlock> {
        match self {
            LoweredBlock::Edge { succ, .. } => Some(*succ),
            LoweredBlock::Orig { .. } => None,
        }
    }
}

impl BlockLoweringOrder {
    pub fn new(arena: ArenaContext<'_>) -> BlockLoweringOrder {
        assert!(arena.curr_func.is_some());

        let func_data = arena.f();
        let mut bb_id = IDAllocator::default();
        let (graph, prece) = cfg::build_cfg_both(func_data, &mut bb_id);
        let plain_rpo = cfg::rpo_path(&graph);

        let idom_map = dom_tree::idom(&prece, &plain_rpo);
        let dom_children = dom_tree::build_dominance_tree(&idom_map, plain_rpo.len());

        let mut rpo_index = vec![0usize; plain_rpo.len()];
        for (i, &id) in plain_rpo.iter().enumerate() {
            rpo_index[id] = i;
        }

        // Visit the dominator tree in pre-order, sorting children by the
        // original CFG RPO so that sibling branches are processed in a
        // predictable order.
        let mut domtree_rpo = Vec::with_capacity(plain_rpo.len());
        fn domtree_dfs(
            node: usize,
            children: &[Vec<usize>],
            rpo_idx: &[usize],
            result: &mut Vec<usize>,
        ) {
            result.push(node);
            let mut child_list = children[node].clone();
            child_list.sort_by_key(|&c| rpo_idx[c]);
            for child in child_list {
                domtree_dfs(child, children, rpo_idx, result);
            }
        }
        domtree_dfs(0, &dom_children, &rpo_index, &mut domtree_rpo);

        let rpo: Vec<HirBasicBlock> = domtree_rpo.iter().map(|&id| bb_id.search_id(id)).collect();

        let mut out_degree: FxHashMap<HirBasicBlock, u32> = FxHashMap::default();
        let mut in_degree: FxHashMap<HirBasicBlock, u32> = FxHashMap::default();
        let mut lowered_order = Vec::new();
        let mut block_succ = Vec::new();
        let mut block_succ_range: FxHashMap<HirBasicBlock, Range<usize>> = FxHashMap::default();

        // Record each original CFG successor in terminator successor order.
        for bb_layout in arena.f().layout().basicblocks() {
            let succ_start_index = block_succ.len();
            let bb = bb_layout.bb();
            let terminator = *bb_layout.insts().get_last().unwrap();
            let term_data = arena.inst_data(terminator);

            out_degree.entry(bb).or_insert(0);
            for succ in term_data.bb_usage() {
                *out_degree.get_mut(&bb).unwrap() += 1;
                *in_degree.entry(succ).or_insert(0) += 1;
                block_succ.push(LoweredBlock::Orig { block: succ });
            }

            let succ_end_index = block_succ.len();
            block_succ_range.insert(bb, succ_start_index..succ_end_index);
        }

        let mut hlir_block_map = FxHashMap::default();
        for &bb in &rpo {
            let idx = MirBlockIndex::new(lowered_order.len());
            lowered_order.push(LoweredBlock::Orig { block: bb });
            hlir_block_map.insert(bb, idx);

            if out_degree[&bb] > 1 {
                let range = block_succ_range[&bb].clone();
                let succs = block_succ[range].iter_mut().enumerate();
                for (succ_idx, lowered_block) in succs {
                    let orig = lowered_block.orig_block().unwrap();
                    let terminator = *arena
                        .f()
                        .layout()
                        .basicblock(bb)
                        .insts()
                        .get_last()
                        .unwrap();
                    let args = outgoing_block_args(arena, terminator, succ_idx, orig);
                    let params = arena.f().bb_data(orig).params();
                    assert_eq!(
                        args.len(),
                        params.len(),
                        "edge arguments must match successor block parameters"
                    );
                    for (&arg, &param) in args.iter().zip(params) {
                        assert_eq!(
                            arena.inst_data(arg).ty(),
                            arena.inst_data(param).ty(),
                            "edge arguments must have the type of their successor block parameter"
                        );
                    }

                    // A multi-way terminator cannot own edge-specific work. Split
                    // critical edges and every value-carrying edge, retaining the
                    // source successor index even when two edges share a target.
                    if in_degree[&orig] > 1 || !args.is_empty() || !params.is_empty() {
                        let edge = LoweredBlock::Edge {
                            pred: bb,
                            succ: orig,
                            succ_idx: succ_idx as u32,
                        };
                        debug_assert!(
                            !lowered_order.contains(&edge),
                            "each source successor index must produce a distinct edge block"
                        );
                        *lowered_block = edge;
                        lowered_order.push(edge);
                    }
                }
            }
        }

        let lb_index_map = FxHashMap::from_iter(
            lowered_order
                .iter()
                .enumerate()
                .map(|(idx, lb)| (lb, MirBlockIndex::new(idx))),
        );

        let mut lowered_succ_indices = Vec::new();
        let lowered_succ_ranges = Vec::from_iter(lowered_order.iter().map(|lb| {
            let start = lowered_succ_indices.len();
            let opt_inst = match *lb {
                LoweredBlock::Orig { block } => {
                    let range = block_succ_range[&block].clone();
                    lowered_succ_indices
                        .extend(block_succ[range].iter().map(|lb| lb_index_map[lb]));
                    let last = *arena
                        .f()
                        .layout()
                        .basicblock(block)
                        .insts()
                        .get_last()
                        .unwrap();

                    arena.is_branch(last).then_some(last)
                }
                LoweredBlock::Edge { succ, .. } => {
                    let succ_index = lb_index_map[&LoweredBlock::Orig { block: succ }];
                    lowered_succ_indices.push(succ_index);
                    None
                }
            };
            let end = lowered_succ_indices.len();
            (opt_inst, start..end)
        }));

        BlockLoweringOrder {
            lowered_order,
            lowered_succ_indices,
            lowered_succ_ranges,
            hlir_block_map,
        }
    }

    pub fn lowered_order(&self) -> &[LoweredBlock] {
        &self.lowered_order[..]
    }

    pub fn lowered_index_for_block(&self, bb: HirBasicBlock) -> Option<MirBlockIndex> {
        self.hlir_block_map
            .get(&bb)
            .filter(|block| block.is_valid())
            .copied()
    }

    pub fn succ_indices(&self, block: MirBlockIndex) -> (Option<HirInst>, &[MirBlockIndex]) {
        let (opt_inst, range) = &self.lowered_succ_ranges[block.index()];
        (*opt_inst, &self.lowered_succ_indices[range.clone()])
    }
}

fn outgoing_block_args(
    arena: ArenaContext<'_>,
    terminator: HirInst,
    succ_idx: usize,
    expected_succ: HirBasicBlock,
) -> Vec<HirInst> {
    match arena.inst_data(terminator).kind() {
        InstKind::Branch(branch) => match succ_idx {
            0 => {
                assert_eq!(branch.t_target(), expected_succ);
                branch.t_args().to_vec()
            }
            1 => {
                assert_eq!(branch.f_target(), expected_succ);
                branch.f_args().to_vec()
            }
            _ => unreachable!("branch has exactly two successors"),
        },
        InstKind::Jump(jump) => {
            assert_eq!(succ_idx, 0, "jump has exactly one successor");
            assert_eq!(jump.target(), expected_succ);
            jump.args().to_vec()
        }
        _ => unreachable!("CFG successor must come from a branch or jump terminator"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use raana_ir::ir::{Program, arena::Arena};
    use raana_ir::opt::prelude::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder};

    fn add_block(data: &mut HirFunctionData, name: &str, params: Vec<HirType>) -> HirBasicBlock {
        let block = data.new_basic_block().basic_block(name.to_owned(), params);
        data.layout_mut().push_bb_back(block);
        block
    }

    fn order_for(program: &HirProgram, func: HirFunction) -> BlockLoweringOrder {
        BlockLoweringOrder::new(ArenaContext {
            program,
            curr_func: Some(func),
        })
    }

    #[test]
    fn keeps_distinct_branch_edges_to_the_same_parameterized_target() {
        let mut program = Program::new();
        let func = program.new_function(HirType::get_i32(), "diamond".to_owned(), vec![]);
        let (entry, merge, branch) = {
            let data = program.func_data_mut(func);
            let entry = add_block(data, "entry", vec![]);
            let merge = add_block(data, "merge", vec![HirType::get_i32()]);
            let (cond, true_value, false_value, branch) = {
                let mut builder = data.new_local_inst();
                let cond = builder.integer(1);
                let true_value = builder.integer(10);
                let false_value = builder.integer(20);
                let branch =
                    builder.branch(cond, merge, vec![true_value], merge, vec![false_value]);
                (cond, true_value, false_value, branch)
            };
            let _ = (cond, true_value, false_value);
            data.layout_mut().insert_inst(entry, branch);
            let param = data.bb_data(merge).params()[0];
            let ret = data.new_local_inst().ret(Some(param));
            data.layout_mut().insert_inst(merge, ret);
            (entry, merge, branch)
        };

        let order = order_for(&program, func);
        let entry_index = order.lowered_index_for_block(entry).unwrap();
        let (terminator, successors) = order.succ_indices(entry_index);

        assert_eq!(terminator, Some(branch));
        assert_eq!(successors.len(), 2);
        assert_ne!(successors[0], successors[1]);
        assert_eq!(
            order.lowered_order()[successors[0].index()],
            LoweredBlock::Edge {
                pred: entry,
                succ: merge,
                succ_idx: 0,
            }
        );
        assert_eq!(
            order.lowered_order()[successors[1].index()],
            LoweredBlock::Edge {
                pred: entry,
                succ: merge,
                succ_idx: 1,
            }
        );
    }

    #[test]
    fn keeps_return_out_of_branch_metadata() {
        let mut program = Program::new();
        let func = program.new_function(HirType::get_i32(), "returning".to_owned(), vec![]);
        let entry = {
            let data = program.func_data_mut(func);
            let entry = add_block(data, "entry", vec![]);
            let value = data.new_local_inst().integer(42);
            let ret = data.new_local_inst().ret(Some(value));
            data.layout_mut().insert_inst(entry, ret);
            entry
        };

        let order = order_for(&program, func);
        let entry_index = order.lowered_index_for_block(entry).unwrap();
        let (branch, successors) = order.succ_indices(entry_index);

        assert_eq!(branch, None, "return is not a CFG branch");
        assert!(successors.is_empty(), "return has no CFG successors");
    }

    #[test]
    fn preserves_loop_continue_break_and_critical_edges() {
        let mut program = Program::new();
        let func = program.new_function(HirType::get_i32(), "loop".to_owned(), vec![]);
        let (entry, header, body, exit) = {
            let data = program.func_data_mut(func);
            let entry = add_block(data, "entry", vec![]);
            let header = add_block(data, "header", vec![HirType::get_i32()]);
            let body = add_block(data, "body", vec![]);
            let exit = add_block(data, "exit", vec![]);

            let initial = data.new_local_inst().integer(0);
            let entry_jump = data.new_local_inst().jump(header, vec![initial]);
            data.layout_mut().insert_inst(entry, entry_jump);

            let cond = data.new_local_inst().integer(1);
            let header_branch = data
                .new_local_inst()
                .branch(cond, body, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, header_branch);

            let next = data.new_local_inst().integer(1);
            let body_cond = data.new_local_inst().integer(1);
            let body_branch =
                data.new_local_inst()
                    .branch(body_cond, header, vec![next], exit, vec![]);
            data.layout_mut().insert_inst(body, body_branch);

            let result = data.new_local_inst().integer(0);
            let ret = data.new_local_inst().ret(Some(result));
            data.layout_mut().insert_inst(exit, ret);
            (entry, header, body, exit)
        };

        let order = order_for(&program, func);
        let body_index = order.lowered_index_for_block(body).unwrap();
        let header_index = order.lowered_index_for_block(header).unwrap();
        let (_, body_successors) = order.succ_indices(body_index);
        let (_, header_successors) = order.succ_indices(header_index);

        assert_eq!(body_successors.len(), 2);
        assert_eq!(
            order.lowered_order()[body_successors[0].index()],
            LoweredBlock::Edge {
                pred: body,
                succ: header,
                succ_idx: 0,
            },
            "continue carries the loop parameter through an edge-owned transfer"
        );
        assert_eq!(
            order.lowered_order()[body_successors[1].index()],
            LoweredBlock::Edge {
                pred: body,
                succ: exit,
                succ_idx: 1,
            },
            "break critical edge has a dedicated edge block"
        );
        assert_eq!(header_successors.len(), 2);
        assert!(
            header_successors
                .iter()
                .any(|successor| order.lowered_order()[successor.index()]
                    == LoweredBlock::Edge {
                        pred: header,
                        succ: exit,
                        succ_idx: 1,
                    }),
            "the header-to-exit critical edge has its own edge block"
        );
        assert!(order.lowered_index_for_block(entry).is_some());
    }
}
