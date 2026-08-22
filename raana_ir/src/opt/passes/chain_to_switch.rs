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
//!
//! ## 补充说明（中文）
//!
//! 术语：支配（dominate）/ block 参数（Phi）/ 边参数等见
//! `docs/offline-handbook/glossary.md`。本 pass 与 AArch64 后端
//! `anon_armv8/src/passes/chain_fusion.rs` 配套：IR 侧把相等链摆成树，
//! 后端把每个 (check, split) 节点对融合成一次比较。
//!
//! ### 一句话定位与动机
//!
//! 把 C 里常见的 `if (x == 1) ...; if (x == 2) ...` 线性相等测试链改写为
//! 平衡二叉判定树：最坏路径比较次数从 O(N)（N 条链）降到 ~log2(N)+1
//! （8 个 case 时 4 次，与 clang `rotrN` 一致），平均 ~2.6 次。
//!
//! ### 变换形态
//!
//! 英文部分已画过每个树节点的形状，这里只补结构要点：链块按 case 常数
//! `k` 升序排序（`convert_chain` 里 `sort_by_key`），`build_tree` 以中位数
//! 切分递归建树；**链头块就地成为树根**（其 block 参数与入边原样保留），
//! 其余链块删除；内部节点是 check + split 一对块，叶子只留 check、
//! `lt` 侧直连 default。
//!
//! ### 触发 / 放弃条件
//!
//! `detect_chain` 从链头逐块扫描，**全部**满足才触发：
//!
//! - 每块恰好 `CHAIN_BLOCK_INSTS`（2）条指令：`Binary(Eq)` + 以它为条件
//!   的 `Branch`（任一不满足即断链）；
//! - 常数在 Eq 任一侧均可（`Integer` 常量），且所有链块测试**同一个**
//!   值 `x`；
//! - case 常数不重复（重复即拒绝）；
//! - false 目标不能是当前块自身（拒绝自环），并沿 false 边接到下一链块；
//! - 链长 ≥ `MIN_CHAIN_LEN`（4）——更短的链直接线性测试已足够便宜；
//! - 除链头外的链块不得带 block 参数（树的各边不传参）；链头豁免：它
//!   就地变树根，函数入口起链时函数参数借此保留。
//!
//! `run_on` 层面：无函数入口块直接返回 false；同一轮里被先前转换删除
//! 的块跳过。
//!
//! ### 正确性要点
//!
//! - 只改写"整块内容 = 测试 + 终结符"的块，每个 handler 仍经同一条边、
//!   携带同样的边参数到达（与变换前一致）；
//! - 被测值 `x` 保持原定义块（链块只有 2 条指令，不可能定义它），且树
//!   只能经链头的前驱进入，故 `x` 支配所有树块（SSA 合法性）；
//! - 链头就地变树根：其 block 参数（函数入口即函数参数）与入边无需
//!   重接线；
//! - default（最后一条链的 false 目标及其边参数）原样复制到每个叶子 /
//!   空子树位置。
//!
//! ### 管线位置与门控
//!
//! - 注册：`opt/pass.rs` 的 `from_config`，fixpoint 段，`rotate_loops`、
//!   `zero_store_loop` 之后、`licm` 之前（先旋转 / 折叠循环，再做判定树）；
//! - 门控：`config.target.enable_chain_to_switch`，仅 AArch64 挂载
//!   （`config.rs`：aarch64 为 true、riscv 为 false）——RISC-V 分支把条件
//!   物化进寄存器，判定树无法像 AArch64 那样把 (eq, lt) 两次比较融合，
//!   因此不启用；无独立 CLI / config 开关；
//! - 后端配套：`chain_fusion` 把 check + split 对融合成 `cmp; beq; blo; b`，
//!   即 clang 的发射形态。
//!
//! ### 验证
//!
//! - 本文件 `mod tests`（400 行起）：`converts_an_equality_chain_into_a_balanced_tree`
//!   （6 链：旧链块被删、entry 保留为树根、default 仍可达）与
//!   `rejects_chains_shorter_than_four_links`（3 链拒绝、原块不动）；
//! - 端到端：`make test`（AArch64 差分比对）。

use crate::opt::prelude::*;

pub struct ChainToSwitch;

/// A chain block must contain exactly the equality test and the terminator.
const CHAIN_BLOCK_INSTS: usize = 2;

/// A chain shorter than this is already as cheap as a tree.
const MIN_CHAIN_LEN: usize = 4;

// 一条链节 = 一个 `if (x == k) goto handler; goto next`：记下块、case
// 常数、true/false 两个目标以及两侧边参数。树建成后这些目标与参数
// 要原样重建。
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
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        // 门控落点：本 pass 自身不查目标架构——是否注册由管线 `opt/pass.rs`
        // 按 `config.target.enable_chain_to_switch` 决定（仅 AArch64 挂载）。
        if data.layout().entry_bb().is_none() {
            // 没有函数入口块则无处安放树根，直接放弃。
            return false;
        }
        // 先快照块列表再遍历：转换会新建/删除块，不能在迭代 layout 的同时改写它。
        let blocks: Vec<BasicBlock> = data
            .layout()
            .basicblocks()
            .iter()
            .map(|layout| layout.bb())
            .collect();
        let mut changed = false;
        for head in blocks {
            // An earlier conversion may have removed this block.
            // 前一个链的转换可能已把该块当作链中块删掉：layout 里已不存在就跳过。
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
        // 改写四步：识别链 → 按 case 常数排序 → 递归建树（根落在链头）→
        // 删除被取代的旧指令与链块。识别失败则本函数什么都不做。
        let Some((x, links, default, default_args)) = Self::detect_chain(data, head) else {
            return false;
        };
        let mut sorted = links;
        // 按 case 常数 k 升序：树靠中位数切分分叉，必须保证左子树全 < k <
        // 右子树，才能用一次 `x < k` 比较把搜索范围减半。
        sorted.sort_by_key(|link| link.k);

        // The chain head becomes the tree root in place, so it keeps its
        // block parameters (function parameters for an entry-rooted chain,
        // inlined arguments for an inlined chain) and its incoming edges
        // need no rewiring.
        // 快照链头的旧指令：链头原地变树根，树建成后要删掉这两条旧指令，
        // 由 build_tree 在相同位置重建根检查。
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
        // 链头只删指令不删块（它现在是树根）；其余链块连同指令整体移除，
        // handler / default 是外部块，不受影响。
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
        // 沿 false 边逐块前进做形态匹配：每块必须恰好是「Eq 测试 + 以它为
        // 条件的分支」，任一条件不满足立即断链返回 None。全部通过后还要
        // 过链长与 block 参数两关。
        let mut x: Option<Inst> = None;
        let mut links: Vec<Link> = Vec::new();
        let mut cur = head;
        loop {
            let layout = data.layout().basicblock(cur);
            // 整块必须恰好 2 条指令：多了说明块里还有别的计算（x 可能被重新
            // 定义），树无法安全重建，断链。
            if layout.insts().len() != CHAIN_BLOCK_INSTS {
                break;
            }
            let insts: Vec<Inst> = layout.insts().iter().copied().collect();
            // 指令 0 必须是 Eq 比较、指令 1 必须是分支且条件正是指令 0：
            // 这正是 `if (x == k) goto handler; goto next` 的 IR 形态。
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
            // case 常数可写在 Eq 任一侧（`x == k` 或 `k == x`），另一侧即被测值 x。
            let (lhs, rhs) = (binary.lhs(), binary.rhs());
            let (value, k) = match data.inst_data(rhs).kind() {
                InstKind::Integer(i) => (lhs, i.value()),
                _ => match data.inst_data(lhs).kind() {
                    InstKind::Integer(i) => (rhs, i.value()),
                    _ => break,
                },
            };
            // 全部链节必须测试同一个 x：树是对 x 建索引的，若某节换测别的值，
            // 中位数切分就不再是按同一变量的数值分界。
            if let Some(expected) = x {
                if expected != value {
                    break;
                }
            } else {
                x = Some(value);
            }
            // case 常数重复即拒绝：两个链节命中同一个 k，树里无法区分。
            if links.iter().any(|link| link.k == k) {
                break;
            }
            let next = branch.f_target();
            // false 目标指向自身（自环）不可能是普通相等链的写法，断链。
            if next == cur {
                break;
            }
            // 记下链节并沿 false 边前进到下一链节；两侧边参数一并保存，
            // 建树时要原样重建这些边。
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
        // 收益判定：链长 ≥ 4 才改写。线性链最坏 N 次比较、平均 N/2；树最坏
        // ~log2(N)+1 次。太短的链省下的比较不抵新建块的开销。
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
        // 除链头外，链块不得带 block 参数：树新建的各边都不传参，带参数的
        // 块无法重接；链头豁免——它保留为树根，参数随原入边继续有效。
        if links
            .iter()
            .skip(1)
            .any(|link| !data.bb_data(link.block).params().is_empty())
        {
            return None;
        }
        // 最后一条链的 false 目标就是 default：它要连同边参数复制到树的每个
        // 叶子 / 空子树位置。
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
        // 递归建树：区间 [lo, hi) 内以中位数切分。内部节点 = check + split
        // 一对块，叶子只留 check、false 边直连 default。返回子树入口块。
        if hi - lo == 1 {
            // 叶子：只剩一个 case，无需 split——`x == k` 命中即进 handler，
            // 否则落 default。顶层叶子原地复用链头（into_block）。
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

        // 中位数切分：左 [lo, mid)、根 mid、右 (mid, hi)；哪一侧为空就直接
        // 用 default 兜底（该方向上没有 case 可命中）。
        let mid = lo + (hi - lo) / 2;
        let key = links[mid].k;
        let (left_target, left_args) = if lo < mid {
            (
                Self::build_tree(
                    data,
                    x,
                    links,
                    default,
                    default_args.clone(),
                    lo,
                    mid,
                    into_block,
                ),
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

        // into_block（链头）只在顶层生效：递归调用也原样传入它，但 lo/hi
        // 已不是全区间，条件不成立，子树照常新建块。
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

        // 内部节点两段式：check 先测 `x == key`（命中即进 handler），未命中
        // 落到 split 再测 `x < key` 分左右。这对 (eq, lt) 正是后端
        // `chain_fusion` 融合成一次 `cmp; beq; blo` 的目标形态。
        let split = {
            let bb = data
                .new_basic_block()
                .basic_block(format!("chain_split_{}", key), vec![]);
            data.layout_mut().push_bb_back(bb);
            bb
        };
        let lt = data.new_local_inst().binary(BinaryOp::Lt, x, key_inst);
        data.layout_mut().insert_inst(split, lt);
        let split_br =
            data.new_local_inst()
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
            let exit = data
                .new_basic_block()
                .basic_block("exit".to_owned(), vec![]);
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
                        let bb = data
                            .new_basic_block()
                            .basic_block(format!("case_{k}"), vec![]);
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
                let br = data
                    .new_local_inst()
                    .branch(test, handler, vec![], next, vec![]);
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
