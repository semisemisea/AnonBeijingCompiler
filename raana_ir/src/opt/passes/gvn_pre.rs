//! # GVNPRE：全局值编号 + 部分冗余消除
//!
//! 经典 GVN-PRE 的一个保守子集：对 **join 块**（≥ 2 条入边）里的 i32 二元
//! 表达式，消除跨边的完全冗余（full redundancy）与部分冗余（partial
//! redundancy）。文件下方保留了一份英文实现状态/路线图注释（Implemented /
//! Current safety boundary / Next steps），中文版要点如下。
//!
//! ## 核心思想
//!
//! 两条路径汇入 join 块时，同一条表达式可能在其中几条路径上已经算过
//! （leader 可用），在另外的路径上没有。用 **block 参数**（Phi）把各边
//! 已有的值收进 join 块，替代重复计算：
//!
//! ```text
//!      A: a = x + y            A: a = x + y          A: a = x + y
//!     /                        /                       \
//!    J: b = x + y     →      J(join):                  J(join):
//!   / \                    参 a  参 a'              参 a  参 a'
//!   X   Y                  b 用参 a（A 边）          b 用参 a'（B 边插入 x+y）
//! ```
//!
//! 左：A、B 两条路径都算出过 `x + y`（A 有 leader，B 没有）→ join 块加参数，
//! A 边传 `a`，B 边**插入** `x + y` 再传。join 块里的 `b = x + y` 被参数替换。
//!
//! - **完全冗余**（所有入边都有 leader）：各边直接把 leader 作为实参追加到
//!   新加的 block 参数上（`apply`）；
//! - **部分冗余**（恰好一条边缺 leader）：在缺失边上插入重算
//!   （`apply_insertion`）——若缺失边是 `branch` 的一臂（critical edge，
//!   边上插指令会同时影响两条路径），先拆出 `gvn_pre_split` 块再插入。
//!
//! ## 操作数翻译（Phi 翻译）
//!
//! 表达式出现在 join 块里时，其操作数可能是本块的 block 参数（由不同入边
//! 传不同值）。逐条入边把操作数翻译成该边实参（`translated_operand`）再
//! 编号，才能找到"每条路径上实际的值"对应的 leader。
//!
//! ## 触发 / 安全边界（保守子集）
//!
//! - 只处理 **i32** 的 `Add/Sub/Mul/And/Or/Xor` 与整数比较；交换律操作
//!   （Add/Mul/And/Or/Xor/Eq/NotEq）操作数规范化（按值编号排序），比较
//!   用 `swap_compare_args` 翻向；
//! - **排除**：移位、Div/Rem、浮点、load/store、call、cast、GEP、Undef、
//!   内存操作；
//! - join 块的入边不得有回边（`dominates(bb, edge.from)` 即拒绝——循环头
//!   插入会破坏分析快照）；
//! - 缺失边多于一条 → 放弃（多边缺失留给路线图里的收益模型）；
//! - 每次函数调用**至多一次结构改写**（`apply` / `apply_insertion` 后立即
//!   return）——改写会使 CFG/支配/值编号快照过期，下一轮 fixpoint 迭代再
//!   处理下一个；
//! - 完全冗余时各边 leader 完全相同则跳过（无新信息）。
//!
//! ## 正确性
//!
//! - 值可用性（`value_available_before`）按支配 + 指令位置检查：leader 必须
//!   在缺失边/各边终结符之前可见，保证插入与参数引用都满足 SSA 支配；
//! - 插入的重算与原表达式操作数/类型一致（typed value numbering 保证同号
//!   同义）；
//! - 纯计算无副作用，插入只增加执行次数不改变结果。
//!
//! ## 管线位置
//!
//! - 注册：`opt/pass.rs` 的 `from_config`，fixpoint 段，`boolean_simplify`
//!   之后、`dce` 之前；与 `gvn`（同函数内冗余合并）互补：PRE 跨边消除，
//!   GVN 域内消除；
//! - 无目标门控、无 config 开关。
//!
//! ## 验证
//!
//! - 本文件 `mod tests`（619 行起）覆盖 diamond / critical edge / parallel
//!   edge / Phi 翻译 / 循环拒绝 / 幂等；
//! - 端到端：`make test` 差分比对。

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::opt::analysis_passes::dom_tree::v2::DominanceTree;
use crate::opt::prelude::*;
use crate::opt::utils::cfg::CFG;
use crate::opt::utils::logical_edge::{
    LogicalEdge as Edge, LogicalEdgeArm as EdgeArm, LogicalEdgeRewriter, incoming_edges,
};

// GVN-PRE implementation status and roadmap:
//
// Implemented:
// - Typed value numbering for safe i32 binary expressions.
// - Full-redundancy elimination at joins using block parameters.
// - Partial-redundancy elimination when exactly one incoming edge is missing.
// - Phi translation through existing block parameters.
// - Logical edge identity for jump/true/false edges, including parallel edges.
// - Critical-edge splitting and insertion on the split edge.
// - Conservative dominance checks and rejection of loop headers with backedges.
// - At most one structural rewrite per function invocation to avoid stale analyses.
// - Trace-level diagnostics for each committed transformation.
//
// Current safety boundary:
// - Candidates: i32 Add/Sub/Mul/And/Or/Xor and integer comparisons.
// - Excluded: shifts, Div/Rem, float operations, loads, calls, casts, GEPs,
//   Undef, memory operations, multiple missing edges, and loop-header insertion.
//
// Next implementation steps, in recommended order:
// 1. Replace the local CFG/dominator implementation with reusable edge-aware
//    CFG and dominance analyses, plus an IR verifier for block argument arity,
//    types, use-def consistency, and dominance after structural edits.
// 2. Share typed value numbering and expression canonicalization with the
//    ordinary GVN pass. Keep congruence classes separate from available leaders.
// 3. Add a profitability model using loop depth, dynamic edge estimates, code
//    growth, new block parameters, and expected register pressure.
// 4. Permit multiple missing edges only when the profitability model approves;
//    retain exact logical-edge splitting and transactional terminator rewrites.
// 5. Add carefully tested shift operations after confirming RaanaIR and every
//    backend agree on out-of-range shift semantics.
// 6. Handle loops only after natural-loop/preheader analysis exists. Avoid
//    backedge insertion initially; implement LICM as a separate pass.
// 7. Add memory value numbering separately: exact-address load CSE and local
//    store forwarding first, invalidating on uncertain stores, MemZero, or calls.
//    Do not add load PRE until aliasing and speculation safety are modeled.
// 8. Add function effect summaries (ReadNone/ReadOnly/WriteOrUnknown) before
//    considering call CSE, call PRE, or movement of calls.
//
// Required validation for each extension:
// - Focused unit tests for diamonds, critical edges, parallel edges, phi
//   translation, loops, unsafe operations, and pass idempotence.
// - Full workspace tests, release build, all functional tests under AArch64/QEMU,
//   perf output checks, and before/after instruction, spill, and runtime metrics.
pub struct GVNPRE;

type ValueNumber = u32;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Expr {
    operand_ty: Type,
    result_ty: Type,
    op: BinaryOp,
    lhs: ValueNumber,
    rhs: ValueNumber,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum ValueKey {
    Integer(Type, i32),
    Expr(Expr),
    Identity(Type, Inst),
}

struct ValueNumbers {
    next: ValueNumber,
    values: HashMap<Inst, ValueNumber>,
    keys: HashMap<ValueKey, ValueNumber>,
}

#[derive(Default)]
struct ScopedLeaders {
    leaders: HashMap<Expr, Inst>,
    scopes: Vec<Vec<(Expr, Option<Inst>)>>,
}

impl ScopedLeaders {
    fn enter_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    fn insert(&mut self, expr: Expr, leader: Inst) {
        let previous = self.leaders.insert(expr.clone(), leader);
        self.scopes.last_mut().unwrap().push((expr, previous));
    }

    fn get(&self, expr: &Expr) -> Option<Inst> {
        self.leaders.get(expr).copied()
    }

    fn exit_scope(&mut self) {
        for (expr, previous) in self.scopes.pop().unwrap().into_iter().rev() {
            if let Some(previous) = previous {
                self.leaders.insert(expr, previous);
            } else {
                self.leaders.remove(&expr);
            }
        }
    }
}

impl ValueNumbers {
    fn new() -> Self {
        Self {
            next: 0,
            values: HashMap::default(),
            keys: HashMap::default(),
        }
    }

    /// 值编号（VN）：把每个值映射到同余类的编号。整数按 (类型, 值) 编号，
    /// Binary 表达式按操作数编号（递归求）编号，其余值按身份编号。
    /// 同余的两个表达式得到同一编号——这是 PRE 判断"边上前导可复用"的
    /// 基础。
    fn number(&mut self, data: &ArenaContextMut<'_>, value: Inst) -> ValueNumber {
        if let Some(&number) = self.values.get(&value) {
            return number;
        }
        let ty = data.inst_data(value).ty().clone();
        let key = match data.inst_data(value).kind() {
            InstKind::Integer(integer) => ValueKey::Integer(ty, integer.value()),
            InstKind::Binary(binary) => self
                .expr(data, binary.op(), binary.lhs(), binary.rhs())
                .map(ValueKey::Expr)
                .unwrap_or(ValueKey::Identity(ty, value)),
            _ => ValueKey::Identity(ty, value),
        };
        let number = *self.keys.entry(key).or_insert_with(|| {
            let number = self.next;
            self.next += 1;
            number
        });
        self.values.insert(value, number);
        number
    }

    /// 规范化表达式键：把操作数编号化并做交换律排序（加/乘/与/或/异或/
    /// 相等比较交换操作数，顺序比较换成对称形式），使 `a+b` 与 `b+a`
    /// 同键。非 i32 或不支持的操作返回 None（保持身份编号，宁漏勿错）。
    fn expr(
        &mut self,
        data: &ArenaContextMut<'_>,
        op: BinaryOp,
        lhs: Inst,
        rhs: Inst,
    ) -> Option<Expr> {
        let operand_ty = data.inst_data(lhs).ty().clone();
        if !operand_ty.is_i32() || data.inst_data(rhs).ty() != &operand_ty {
            return None;
        }
        if !matches!(
            op,
            BinaryOp::Add
                | BinaryOp::Sub
                | BinaryOp::Mul
                | BinaryOp::And
                | BinaryOp::Or
                | BinaryOp::Xor
                | BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Gt
                | BinaryOp::Lt
                | BinaryOp::Ge
                | BinaryOp::Le
        ) {
            return None;
        }

        let mut op = op;
        let mut lhs = self.number(data, lhs);
        let mut rhs = self.number(data, rhs);
        if lhs > rhs {
            if matches!(
                op,
                BinaryOp::Add
                    | BinaryOp::Mul
                    | BinaryOp::And
                    | BinaryOp::Or
                    | BinaryOp::Xor
                    | BinaryOp::Eq
                    | BinaryOp::NotEq
            ) {
                std::mem::swap(&mut lhs, &mut rhs);
            } else if let Some(swapped) = op.swap_compare_args() {
                op = swapped;
                std::mem::swap(&mut lhs, &mut rhs);
            }
        }
        Some(Expr {
            operand_ty,
            result_ty: Type::get_i32(),
            op,
            lhs,
            rhs,
        })
    }
}

impl GVNPRE {
    fn incoming_edges(
        &self,
        data: &ArenaContextMut<'_>,
        cfg: &CFG,
    ) -> HashMap<BasicBlock, Vec<Edge>> {
        let mut incoming: HashMap<BasicBlock, Vec<Edge>> = HashMap::default();
        for &bb in cfg.blocks() {
            let edges = incoming_edges(data.curr_func_data(), cfg, bb);
            if !edges.is_empty() {
                incoming.insert(bb, edges.into_iter().collect());
            }
        }
        incoming
    }

    fn parameter_indices(
        &self,
        data: &ArenaContextMut<'_>,
        cfg: &CFG,
    ) -> HashMap<BasicBlock, HashMap<Inst, usize>> {
        cfg.blocks()
            .iter()
            .copied()
            .map(|bb| {
                let indices = data
                    .bb_data(bb)
                    .params()
                    .iter()
                    .enumerate()
                    .map(|(index, &param)| (param, index))
                    .collect();
                (bb, indices)
            })
            .collect()
    }

    fn translated_operand(
        &self,
        data: &ArenaContextMut<'_>,
        parameter_indices: &HashMap<Inst, usize>,
        edge: Edge,
        operand: Inst,
    ) -> Option<Inst> {
        let Some(&index) = parameter_indices.get(&operand) else {
            return Some(operand);
        };
        edge.args(data).get(index).copied()
    }

    fn requested_leaders(
        &self,
        data: &ArenaContextMut<'_>,
        cfg: &CFG,
        dom_tree: &DominanceTree,
        incoming: &HashMap<BasicBlock, Vec<Edge>>,
        parameter_indices: &HashMap<BasicBlock, HashMap<Inst, usize>>,
        numbers: &mut ValueNumbers,
    ) -> HashMap<(BasicBlock, Expr), Inst> {
        let mut requested: HashMap<BasicBlock, HashSet<Expr>> = HashMap::default();
        for &bb in cfg.blocks() {
            let Some(edges) = incoming.get(&bb).filter(|edges| edges.len() >= 2) else {
                continue;
            };
            let indices = &parameter_indices[&bb];
            for &inst in data.layout().basicblock(bb).insts() {
                let InstKind::Binary(binary) = data.inst_data(inst).kind() else {
                    continue;
                };
                for &edge in edges {
                    let Some(lhs) = self.translated_operand(data, indices, edge, binary.lhs())
                    else {
                        continue;
                    };
                    let Some(rhs) = self.translated_operand(data, indices, edge, binary.rhs())
                    else {
                        continue;
                    };
                    if let Some(expr) = numbers.expr(data, binary.op(), lhs, rhs) {
                        requested.entry(edge.from).or_default().insert(expr);
                    }
                }
            }
        }

        #[derive(Clone, Copy)]
        enum Visit {
            Enter(BasicBlock),
            Exit,
        }

        let mut output = HashMap::default();
        let mut leaders = ScopedLeaders::default();
        let mut stack = vec![Visit::Enter(cfg.entry())];
        while let Some(visit) = stack.pop() {
            match visit {
                Visit::Enter(bb) => {
                    leaders.enter_scope();
                    for &inst in data.layout().basicblock(bb).insts() {
                        let InstKind::Binary(binary) = data.inst_data(inst).kind() else {
                            continue;
                        };
                        if let Some(expr) =
                            numbers.expr(data, binary.op(), binary.lhs(), binary.rhs())
                        {
                            leaders.insert(expr, inst);
                        }
                    }
                    if let Some(expressions) = requested.get(&bb) {
                        for expr in expressions {
                            if let Some(leader) = leaders.get(expr) {
                                output.insert((bb, expr.clone()), leader);
                            }
                        }
                    }
                    stack.push(Visit::Exit);
                    for &child in dom_tree.children_of(bb).iter().rev() {
                        stack.push(Visit::Enter(child));
                    }
                }
                Visit::Exit => leaders.exit_scope(),
            }
        }
        output
    }

    fn value_locations(
        &self,
        data: &ArenaContextMut<'_>,
        cfg: &CFG,
    ) -> (
        HashMap<Inst, BasicBlock>,
        HashMap<Inst, (BasicBlock, usize)>,
    ) {
        let mut parameter_blocks = HashMap::default();
        let mut instruction_positions = HashMap::default();
        for &bb in cfg.blocks() {
            for &param in data.bb_data(bb).params() {
                parameter_blocks.insert(param, bb);
            }
            for (index, &inst) in data.layout().basicblock(bb).insts().iter().enumerate() {
                instruction_positions.insert(inst, (bb, index));
            }
        }
        (parameter_blocks, instruction_positions)
    }

    fn value_available_before(
        &self,
        data: &ArenaContextMut<'_>,
        dom_tree: &DominanceTree,
        parameter_blocks: &HashMap<Inst, BasicBlock>,
        instruction_positions: &HashMap<Inst, (BasicBlock, usize)>,
        bb: BasicBlock,
        terminator: Inst,
        value: Inst,
    ) -> bool {
        if value.is_global()
            || data.params().contains(&value)
            || matches!(data.inst_data(value).kind(), InstKind::Integer(..))
        {
            return true;
        }

        if let Some(&(def_bb, position)) = instruction_positions.get(&value) {
            if def_bb != bb {
                return dom_tree.dominates(def_bb, bb);
            }
            return instruction_positions
                .get(&terminator)
                .is_some_and(|&(_, terminator_position)| position < terminator_position);
        }

        if let Some(&def_bb) = parameter_blocks.get(&value) {
            return dom_tree.dominates(def_bb, bb);
        }

        false
    }

    fn edge_is_valid(&self, data: &ArenaContextMut<'_>, bb: BasicBlock, edge: Edge) -> bool {
        let args = edge.args(data);
        let params = data.bb_data(bb).params();
        args.len() == params.len()
            && args
                .iter()
                .zip(params)
                .all(|(&arg, &param)| data.inst_data(arg).ty() == data.inst_data(param).ty())
    }

    fn apply(
        &self,
        data: &mut ArenaContextMut<'_>,
        bb: BasicBlock,
        recomputation: Inst,
        edges: &[Edge],
        leaders: &[Inst],
    ) {
        let mut rewrites = LogicalEdgeRewriter::new();
        for (&edge, &leader) in edges.iter().zip(leaders) {
            rewrites.append_arg(data, edge, leader);
        }

        let ty = data.inst_data(recomputation).ty().clone();
        let param = data.new_basic_block().add_param(bb, ty);
        rewrites.apply(data);
        utils::visit_and_replace(data, recomputation, param);
        data.remove_layout_inst(bb, recomputation);
    }

    fn apply_insertion(
        &self,
        data: &mut ArenaContextMut<'_>,
        bb: BasicBlock,
        recomputation: Inst,
        edges: &[Edge],
        leaders: &[Option<Inst>],
        missing_index: usize,
        op: BinaryOp,
        lhs: Inst,
        rhs: Inst,
    ) {
        let missing = edges[missing_index];
        let mut rewrites = LogicalEdgeRewriter::new();
        for &edge in edges {
            rewrites.seed(data, edge.terminator);
        }

        let inserted = data.new_local_inst().binary(op, lhs, rhs);
        let effective_missing = if matches!(missing.arm, EdgeArm::Jump) {
            data.layout_mut()
                .insert_inst_before(missing.terminator, inserted);
            missing
        } else {
            let old_args = missing.args(data).to_vec();
            let split = data
                .new_basic_block()
                .basic_block("gvn_pre_split".into(), vec![]);
            data.layout_mut().push_bb_back(split);
            data.layout_mut().insert_inst(split, inserted);
            rewrites.retarget(data, missing, split, vec![]);
            let split_jump = data.new_local_inst().jump(bb, old_args);
            data.layout_mut().insert_inst(split, split_jump);
            Edge::new(split, split_jump, EdgeArm::Jump)
        };

        let ty = data.inst_data(recomputation).ty().clone();
        let param = data.new_basic_block().add_param(bb, ty);
        for (index, &edge) in edges.iter().enumerate() {
            if index == missing_index && !matches!(missing.arm, EdgeArm::Jump) {
                continue;
            }
            let value = leaders[index].unwrap_or(inserted);
            rewrites.append_arg(data, edge, value);
        }
        if effective_missing.terminator != missing.terminator {
            rewrites.append_arg(data, effective_missing, inserted);
        }

        rewrites.apply(data);
        utils::visit_and_replace(data, recomputation, param);
        data.remove_layout_inst(bb, recomputation);
    }
}

impl Pass for GVNPRE {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        // 主流程：一次 run_on 只消除/插入一个表达式（返回 true 让 fixpoint
        // 重跑），保证任何分析结果都不会在 CFG 改写后被复用——CFG/支配树
        // 是快照，改写后立即失效。
        // 三步：① 对每个多入边块（≥2 条 incoming），把块内 Binary 表达式的
        // 操作数沿各条入边做 phi 翻译（translated_operand），收集"请求"的
        // 表达式集合；② 沿支配树前序 DFS 维护作用域化 leader 表，算出每个
        // 请求表达式在各前驱块里可用的 leader（requested_leaders）；③ 逐
        // 候选判定：全可用 → 直接替换为块参数（完全 PRE）；恰一条边缺 →
        // 缺的那条边插入计算（部分 PRE）；缺多条 → 放弃（保持单缺失边
        // 的保守设计）。
        let Some(cfg) = CFG::new(data.curr_func_data()) else {
            return false;
        };
        let dom_tree = DominanceTree::from_cfg(&cfg);
        let incoming = self.incoming_edges(data, &cfg);
        let parameter_indices = self.parameter_indices(data, &cfg);
        let mut numbers = ValueNumbers::new();
        let output = self.requested_leaders(
            data,
            &cfg,
            &dom_tree,
            &incoming,
            &parameter_indices,
            &mut numbers,
        );
        let (parameter_blocks, instruction_positions) = self.value_locations(data, &cfg);

        for &bb in cfg.blocks() {
            let Some(edges) = incoming.get(&bb).filter(|edges| edges.len() >= 2) else {
                continue;
            };
            if edges.iter().any(|edge| dom_tree.dominates(bb, edge.from))
                || edges
                    .iter()
                    .any(|&edge| !self.edge_is_valid(data, bb, edge))
            {
                continue;
            }
            let indices = &parameter_indices[&bb];
            let insts = data
                .layout()
                .basicblock(bb)
                .insts()
                .iter()
                .copied()
                .collect::<Vec<_>>();
            for recomputation in insts {
                let InstKind::Binary(binary) = data.inst_data(recomputation).kind() else {
                    continue;
                };
                let (op, lhs, rhs) = (binary.op(), binary.lhs(), binary.rhs());
                let Some(candidate_expr) = numbers.expr(data, op, lhs, rhs) else {
                    continue;
                };
                if data.inst_data(recomputation).ty() != &candidate_expr.result_ty {
                    continue;
                }
                let mut leaders = Vec::with_capacity(edges.len());
                let mut translated = Vec::with_capacity(edges.len());
                for &edge in edges {
                    let Some(lhs) = self.translated_operand(data, indices, edge, lhs) else {
                        leaders.clear();
                        break;
                    };
                    let Some(rhs) = self.translated_operand(data, indices, edge, rhs) else {
                        leaders.clear();
                        break;
                    };
                    let Some(expr) = numbers.expr(data, op, lhs, rhs) else {
                        leaders.clear();
                        break;
                    };
                    if expr.result_ty != candidate_expr.result_ty
                        || !self.value_available_before(
                            data,
                            &dom_tree,
                            &parameter_blocks,
                            &instruction_positions,
                            edge.from,
                            edge.terminator,
                            lhs,
                        )
                        || !self.value_available_before(
                            data,
                            &dom_tree,
                            &parameter_blocks,
                            &instruction_positions,
                            edge.from,
                            edge.terminator,
                            rhs,
                        )
                    {
                        leaders.clear();
                        break;
                    }
                    let leader = output.get(&(edge.from, expr)).copied().filter(|&leader| {
                        data.inst_data(leader).ty() == data.inst_data(recomputation).ty()
                            && self.value_available_before(
                                data,
                                &dom_tree,
                                &parameter_blocks,
                                &instruction_positions,
                                edge.from,
                                edge.terminator,
                                leader,
                            )
                    });
                    translated.push((lhs, rhs));
                    leaders.push(leader);
                }
                if leaders.len() != edges.len() {
                    continue;
                }

                let missing = leaders
                    .iter()
                    .enumerate()
                    .filter_map(|(index, leader)| leader.is_none().then_some(index))
                    .collect::<Vec<_>>();
                if missing.is_empty() {
                    let leaders = leaders.into_iter().map(Option::unwrap).collect::<Vec<_>>();
                    if leaders.windows(2).all(|pair| pair[0] == pair[1]) {
                        continue;
                    }

                    // All edge arguments and leaders have been validated. Mutate once,
                    // then return so no analysis result can be reused after CFG changes.
                    self.apply(data, bb, recomputation, edges, &leaders);
                    trace!(
                        "gvn-pre fully_available function={} block={bb:?} expression={candidate_expr:?} block_parameters_added=1",
                        data.name()
                    );
                    return true;
                }
                if missing.len() == 1 {
                    let missing_index = missing[0];
                    let (lhs, rhs) = translated[missing_index];
                    self.apply_insertion(
                        data,
                        bb,
                        recomputation,
                        edges,
                        &leaders,
                        missing_index,
                        op,
                        lhs,
                        rhs,
                    );
                    trace!(
                        "gvn-pre partial_insertion function={} block={bb:?} expression={candidate_expr:?} edge_split={} block_parameters_added=1 expressions_inserted=1",
                        data.name(),
                        !matches!(edges[missing_index].arm, EdgeArm::Jump)
                    );
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Program, arena::Arena, builder_trait::*};

    fn returned_value(data: &FunctionData, bb: BasicBlock) -> Inst {
        let ret = data.layout().basicblock(bb).terminator();
        let InstKind::Return(ret) = data.inst_data(ret).kind() else {
            panic!("expected return")
        };
        ret.value().unwrap()
    }

    #[test]
    fn eliminates_diamond_recomputation_with_commuted_arm() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "diamond".into(),
            vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let head = data.add_entry_block();
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let merge = data.new_basic_block().basic_block("merge".into(), vec![]);
        for bb in [left, right, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let (cond, x, y) = (data.params()[0], data.params()[1], data.params()[2]);
        let branch = data
            .new_local_inst()
            .branch(cond, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(head, branch);
        let left_add = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let left_jump = data.new_local_inst().jump(merge, vec![]);
        data.layout_mut().insert_inst(left, left_add);
        data.layout_mut().insert_inst(left, left_jump);
        let right_add = data.new_local_inst().binary(BinaryOp::Add, y, x);
        let right_jump = data.new_local_inst().jump(merge, vec![]);
        data.layout_mut().insert_inst(right, right_add);
        data.layout_mut().insert_inst(right, right_jump);
        let recomputation = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let ret = data.new_local_inst().ret(Some(recomputation));
        data.layout_mut().insert_inst(merge, recomputation);
        data.layout_mut().insert_inst(merge, ret);

        assert!(GVNPRE.run(&mut program));
        let data = program.func_data(function);
        let param = data.bb_data(merge).params()[0];
        assert_eq!(returned_value(data, merge), param);
        assert_eq!(data.layout().parent_bb(recomputation), None);
        assert!(matches!(
            data.inst_data(left_jump).kind(),
            InstKind::Jump(jump) if jump.args() == [left_add]
        ));
        assert!(matches!(
            data.inst_data(right_jump).kind(),
            InstKind::Jump(jump) if jump.args() == [right_add]
        ));
    }

    #[test]
    fn inserts_on_non_critical_missing_edge() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "negative".into(),
            vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let head = data.add_entry_block();
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let merge = data.new_basic_block().basic_block("merge".into(), vec![]);
        for bb in [left, right, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let (cond, x, y) = (data.params()[0], data.params()[1], data.params()[2]);
        let branch = data
            .new_local_inst()
            .branch(cond, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(head, branch);
        let left_add = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let left_jump = data.new_local_inst().jump(merge, vec![]);
        data.layout_mut().insert_inst(left, left_add);
        data.layout_mut().insert_inst(left, left_jump);
        let right_jump = data.new_local_inst().jump(merge, vec![]);
        data.layout_mut().insert_inst(right, right_jump);
        let recomputation = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let ret = data.new_local_inst().ret(Some(recomputation));
        data.layout_mut().insert_inst(merge, recomputation);
        data.layout_mut().insert_inst(merge, ret);

        let mut pass = GVNPRE;
        assert!(pass.run(&mut program));
        assert!(!pass.run(&mut program));
        let data = program.func_data(function);
        let param = data.bb_data(merge).params()[0];
        assert_eq!(returned_value(data, merge), param);
        assert_eq!(data.layout().parent_bb(recomputation), None);
        assert!(matches!(
            data.inst_data(left_jump).kind(),
            InstKind::Jump(jump) if jump.args() == [left_add]
        ));
        let right_insts = data
            .layout()
            .basicblock(right)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(right_insts.len(), 2);
        let inserted = right_insts[0];
        assert!(matches!(
            data.inst_data(inserted).kind(),
            InstKind::Binary(binary)
                if binary.op() == BinaryOp::Add && binary.lhs() == x && binary.rhs() == y
        ));
        assert!(matches!(
            data.inst_data(right_jump).kind(),
            InstKind::Jump(jump) if jump.args() == [inserted]
        ));
    }

    #[test]
    fn splits_critical_missing_edge() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "critical".into(),
            vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let head = data.add_entry_block();
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let merge = data.new_basic_block().basic_block("merge".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [left, right, merge, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let (cond, x, y) = (data.params()[0], data.params()[1], data.params()[2]);
        let head_branch = data
            .new_local_inst()
            .branch(cond, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(head, head_branch);
        let left_add = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let left_jump = data.new_local_inst().jump(merge, vec![]);
        data.layout_mut().insert_inst(left, left_add);
        data.layout_mut().insert_inst(left, left_jump);
        let right_branch = data
            .new_local_inst()
            .branch(cond, merge, vec![], exit, vec![]);
        data.layout_mut().insert_inst(right, right_branch);
        let recomputation = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let merge_ret = data.new_local_inst().ret(Some(recomputation));
        data.layout_mut().insert_inst(merge, recomputation);
        data.layout_mut().insert_inst(merge, merge_ret);
        let zero = data.new_local_inst().integer(0);
        let exit_ret = data.new_local_inst().ret(Some(zero));
        data.layout_mut().insert_inst(exit, exit_ret);

        let mut pass = GVNPRE;
        assert!(pass.run(&mut program));
        assert!(!pass.run(&mut program));
        let data = program.func_data(function);
        assert_eq!(data.layout().basicblocks().len(), 6);
        let InstKind::Branch(branch) = data.inst_data(right_branch).kind() else {
            panic!("expected branch")
        };
        let split = branch.t_target();
        assert_ne!(split, merge);
        assert_eq!(branch.f_target(), exit);
        assert!(branch.t_args().is_empty());
        let split_insts = data
            .layout()
            .basicblock(split)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(split_insts.len(), 2);
        let inserted = split_insts[0];
        assert!(matches!(
            data.inst_data(inserted).kind(),
            InstKind::Binary(binary) if binary.op() == BinaryOp::Add
        ));
        assert!(matches!(
            data.inst_data(split_insts[1]).kind(),
            InstKind::Jump(jump) if jump.target() == merge && jump.args() == [inserted]
        ));
        assert!(matches!(
            data.inst_data(left_jump).kind(),
            InstKind::Jump(jump) if jump.args() == [left_add]
        ));
        assert_eq!(returned_value(data, merge), data.bb_data(merge).params()[0]);
    }

    #[test]
    fn splits_only_missing_same_target_parallel_arm() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "parallel_missing".into(),
            vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let head = data.add_entry_block();
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32(), Type::get_i32()]);
        data.layout_mut().push_bb_back(merge);
        let (cond, x, y) = (data.params()[0], data.params()[1], data.params()[2]);
        let xy = data.new_local_inst().binary(BinaryOp::Sub, x, y);
        let branch = data
            .new_local_inst()
            .branch(cond, merge, vec![x, y], merge, vec![y, x]);
        data.layout_mut().insert_inst(head, xy);
        data.layout_mut().insert_inst(head, branch);
        let params = data.bb_data(merge).params().clone();
        let recomputation = data
            .new_local_inst()
            .binary(BinaryOp::Sub, params[0], params[1]);
        let ret = data.new_local_inst().ret(Some(recomputation));
        data.layout_mut().insert_inst(merge, recomputation);
        data.layout_mut().insert_inst(merge, ret);

        let mut pass = GVNPRE;
        assert!(pass.run(&mut program));
        assert!(!pass.run(&mut program));
        let data = program.func_data(function);
        let InstKind::Branch(branch_data) = data.inst_data(branch).kind() else {
            panic!("expected branch")
        };
        assert_eq!(branch_data.t_target(), merge);
        assert_eq!(branch_data.t_args(), [x, y, xy]);
        let split = branch_data.f_target();
        assert_ne!(split, merge);
        assert!(branch_data.f_args().is_empty());
        let split_insts = data
            .layout()
            .basicblock(split)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let inserted = split_insts[0];
        assert!(matches!(
            data.inst_data(inserted).kind(),
            InstKind::Binary(binary)
                if binary.op() == BinaryOp::Sub && binary.lhs() == y && binary.rhs() == x
        ));
        assert!(matches!(
            data.inst_data(split_insts[1]).kind(),
            InstKind::Jump(jump) if jump.target() == merge && jump.args() == [y, x, inserted]
        ));
    }

    #[test]
    fn translates_phi_operands_for_inserted_expression() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "phi_insert".into(),
            vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let head = data.add_entry_block();
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32(), Type::get_i32()]);
        for bb in [left, right, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let (cond, x, y) = (data.params()[0], data.params()[1], data.params()[2]);
        let head_branch = data
            .new_local_inst()
            .branch(cond, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(head, head_branch);
        let left_add = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let left_jump = data.new_local_inst().jump(merge, vec![x, y]);
        data.layout_mut().insert_inst(left, left_add);
        data.layout_mut().insert_inst(left, left_jump);
        let right_jump = data.new_local_inst().jump(merge, vec![y, x]);
        data.layout_mut().insert_inst(right, right_jump);
        let params = data.bb_data(merge).params().clone();
        let recomputation = data
            .new_local_inst()
            .binary(BinaryOp::Add, params[0], params[1]);
        let ret = data.new_local_inst().ret(Some(recomputation));
        data.layout_mut().insert_inst(merge, recomputation);
        data.layout_mut().insert_inst(merge, ret);

        assert!(GVNPRE.run(&mut program));
        let data = program.func_data(function);
        let right_insts = data
            .layout()
            .basicblock(right)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let inserted = right_insts[0];
        assert!(matches!(
            data.inst_data(inserted).kind(),
            InstKind::Binary(binary)
                if binary.op() == BinaryOp::Add && binary.lhs() == y && binary.rhs() == x
        ));
        assert!(matches!(
            data.inst_data(right_jump).kind(),
            InstKind::Jump(jump) if jump.args() == [y, x, inserted]
        ));
        assert!(matches!(
            data.inst_data(left_jump).kind(),
            InstKind::Jump(jump) if jump.args() == [x, y, left_add]
        ));
    }

    #[test]
    fn rejects_loop_header_with_backedge() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "loop".into(),
            vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data.new_basic_block().basic_block("header".into(), vec![]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for bb in [header, latch, exit] {
            data.layout_mut().push_bb_back(bb);
        }
        let (cond, x, y) = (data.params()[0], data.params()[1], data.params()[2]);
        let entry_add = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let entry_jump = data.new_local_inst().jump(header, vec![]);
        data.layout_mut().insert_inst(entry, entry_add);
        data.layout_mut().insert_inst(entry, entry_jump);
        let recomputation = data.new_local_inst().binary(BinaryOp::Add, x, y);
        let header_branch = data
            .new_local_inst()
            .branch(cond, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, recomputation);
        data.layout_mut().insert_inst(header, header_branch);
        let latch_jump = data.new_local_inst().jump(header, vec![]);
        data.layout_mut().insert_inst(latch, latch_jump);
        let ret = data.new_local_inst().ret(Some(recomputation));
        data.layout_mut().insert_inst(exit, ret);

        assert!(!GVNPRE.run(&mut program));
        let data = program.func_data(function);
        assert!(data.bb_data(header).params().is_empty());
        assert_eq!(data.layout().parent_bb(recomputation), Some(header));
    }

    #[test]
    fn rejects_unsafe_binary_operation() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "unsafe".into(),
            vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let head = data.add_entry_block();
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let merge = data.new_basic_block().basic_block("merge".into(), vec![]);
        for bb in [left, right, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let (cond, x, y) = (data.params()[0], data.params()[1], data.params()[2]);
        let branch = data
            .new_local_inst()
            .branch(cond, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(head, branch);
        let left_div = data.new_local_inst().binary(BinaryOp::Div, x, y);
        let left_jump = data.new_local_inst().jump(merge, vec![]);
        data.layout_mut().insert_inst(left, left_div);
        data.layout_mut().insert_inst(left, left_jump);
        let right_jump = data.new_local_inst().jump(merge, vec![]);
        data.layout_mut().insert_inst(right, right_jump);
        let recomputation = data.new_local_inst().binary(BinaryOp::Div, x, y);
        let ret = data.new_local_inst().ret(Some(recomputation));
        data.layout_mut().insert_inst(merge, recomputation);
        data.layout_mut().insert_inst(merge, ret);

        assert!(!GVNPRE.run(&mut program));
        let data = program.func_data(function);
        assert!(data.bb_data(merge).params().is_empty());
        assert_eq!(data.layout().parent_bb(recomputation), Some(merge));
    }

    #[test]
    fn preserves_same_target_true_and_false_edges() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "parallel".into(),
            vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let head = data.add_entry_block();
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32(), Type::get_i32()]);
        data.layout_mut().push_bb_back(merge);
        let (cond, x, y) = (data.params()[0], data.params()[1], data.params()[2]);
        let xy = data.new_local_inst().binary(BinaryOp::Sub, x, y);
        let yx = data.new_local_inst().binary(BinaryOp::Sub, y, x);
        let branch = data
            .new_local_inst()
            .branch(cond, merge, vec![x, y], merge, vec![y, x]);
        for inst in [xy, yx, branch] {
            data.layout_mut().insert_inst(head, inst);
        }
        let params = data.bb_data(merge).params().clone();
        let recomputation = data
            .new_local_inst()
            .binary(BinaryOp::Sub, params[0], params[1]);
        let ret = data.new_local_inst().ret(Some(recomputation));
        data.layout_mut().insert_inst(merge, recomputation);
        data.layout_mut().insert_inst(merge, ret);

        assert!(GVNPRE.run(&mut program));
        let data = program.func_data(function);
        let result = data.bb_data(merge).params()[2];
        assert_eq!(returned_value(data, merge), result);
        let InstKind::Branch(branch) = data.inst_data(branch).kind() else {
            panic!("expected branch")
        };
        assert_eq!(branch.t_args(), [x, y, xy]);
        assert_eq!(branch.f_args(), [y, x, yx]);
    }
}
