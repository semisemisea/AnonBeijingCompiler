//! # IfConversion：分支转 select / 布尔折叠
//!
//! 把产生值的小分支结构转成无分支的 `select`，把 `&&`/`||` 分支三角折成
//! 单条 `and`/`or`。消除分支（分支预测失败惩罚 + 后端分支指令），暴露更大
//! 的基本块。四种规范形状见 `IfConversion` 的英文文档；中文要点如下。
//!
//! ## 四种形状（`candidate` 逐一识别）
//!
//! 1. **同目标分支**：`br c, M, args1, M, args2`——两臂同目标，条件只剩
//!   选择实参的作用 → 目标块的参数用 `select(c, args1, args2)` 合并；
//! 2. **空 diamond**：`br c, A, B`，A、B 都是空块且跳到同一 merge → merge
//!   的参数用 `select(c, a, b)` 合并，A、B 删除；
//! 3. **triangle 链**：一臂为空直达 merge、另一臂是"可投机执行的整数指令
//!   链"（无副作用、可安全提前执行）→ 链上指令移到分支前，merge 参数用
//!   select 合并；
//! 4. **land/lor triangle**：`br c1, rhs, merge(0)` 且 `rhs: c2 = ...;
//!   jump merge(c2)` → 折成 `c1 && c2`（`BinaryOp::And`，land）；对称形态
//!   折 `Or`。
//!
//! 合并值（`MergedValue`）三种：两臂相同（`Common`）、select、布尔二元
//! 运算（`BoolBinary`）。
//!
//! ## 触发 / 放弃条件
//!
//! - 分支条件必须 i32；目标块不得是 head 自身；
//! - 空 diamond/triangle 要求臂块无参数、`exact_users` 校验（目标块只被
//!   该分支使用，删除安全）、实参在 head 处可用（`available_at`，支配
//!   检查）；
//! - triangle 链只收可投机（无副作用）的整数指令；
//! - 放弃：不可投机指令、目标块有其它前驱、条件非 i32。
//!
//! ## 正确性
//!
//! - select 语义 = 条件选择两值，与分支+Phi 等价（两臂值分别对应 true/
//!   false）；
//! - 投机执行只允许无副作用指令（异常/内存副作用会改变可观察行为）；
//! - `apply` 只改写 head 及其后的块（贪婪按布局序扫描，从不删 head 之前的
//!   块），布局序扫描可安全继续。
//!
//! ## 管线位置
//!
//! - 注册：`opt/pass.rs` 的 `from_config`，fixpoint 段，`blocked_reduction`
//!   之后、`tco`（第二次）之前；
//! - 与 `boolean_simplify` 配合：后者先规范化布尔形态，本 pass 消费；
//! - 无目标门控、无 config 开关。
//!
//! ## 验证
//!
//! - 本文件 `mod tests` 覆盖四种形状与拒绝路径；
//! - 端到端：`make test` 差分比对。

use crate::opt::prelude::*;

/// Converts small, value-producing branches into `select`s, and folds
/// `&&`/`||` branch triangles into single `and`/`or` instructions.
///
/// Handles four canonical shapes: a branch whose two edges have the same
/// target, an empty diamond, a triangle containing a chain of speculatable
/// integer instructions, and a land/lor triangle (`br c1, rhs, merge(0)`
/// with `rhs: c2 = ...; jump merge(c2)` folding to `band(c1, c2)`).
pub struct IfConversion;

#[derive(Clone, Copy)]
enum MergedValue {
    Common(Inst),
    Select { if_true: Inst, if_false: Inst },
    BoolBinary { op: BinaryOp, lhs: Inst, rhs: Inst },
}

struct Candidate {
    head: BasicBlock,
    terminator: Inst,
    merge: BasicBlock,
    cond: Inst,
    values: Vec<MergedValue>,
    remove_blocks: Vec<BasicBlock>,
    move_insts: Vec<(BasicBlock, Inst)>,
}

impl Pass for IfConversion {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        // Greedily convert candidates in layout order. `apply` only rewrites
        // the applied head and blocks it removes/remerges downstream, never
        // blocks that appear before the head, so the scan can resume after the
        // applied head instead of restarting from entry. The initial block list
        // stays valid because the pass only removes blocks (never adds them).
        //
        // 主循环总览：先建支配树快照——`candidate` 里的 `available_at` /
        // `dominates` 判定都基于它；随后按布局序贪心：找候选 → `apply` →
        // 继续，直到扫完入口处拍下的块列表。列表是快照、`apply` 只删块
        // 不增块，所以这份列表从头到尾有效。
        let mut changed = false;
        let Some(tree) = dom_tree::v2::DominanceTree::new(data) else {
            return false;
        };
        let initial_blocks = data
            .layout()
            .basicblocks()
            .iter()
            .map(|layout| layout.bb())
            .collect::<Vec<_>>();
        let mut cursor = 0usize;
        loop {
            let mut candidate = None;
            while cursor < initial_blocks.len() {
                let head = initial_blocks[cursor];
                cursor += 1;
                // 被前面候选删除的块在快照列表里仍然存在，用 contains_bb 跳过。
                if !data.layout().contains_bb(head) {
                    continue;
                }
                if let Some(found) = self.candidate(data, head, &tree) {
                    candidate = Some(found);
                    break;
                }
            }
            let Some(candidate) = candidate else {
                return changed;
            };
            self.apply(data, candidate);
            changed = true;
        }
    }
}

impl IfConversion {
    fn candidate(
        &self,
        data: &ArenaContextMut<'_>,
        head: BasicBlock,
        tree: &dom_tree::v2::DominanceTree,
    ) -> Option<Candidate> {
        // 候选识别入口：只处理"终结符是条件分支"的块，逐一匹配四种规范
        // 形状（同目标分支 / 空 diamond / triangle 链 / land-lor 折叠）。
        let terminator = *data.layout().basicblock(head).insts().get_last()?;
        let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
            return None;
        };
        // 条件必须是 i32：select 的语义就是按 i32 条件二选一，非 i32
        // 条件（如 unit）无法用 select 表达。
        if !data.inst_data(branch.cond()).ty().is_i32() {
            return None;
        }

        // 形状一：同目标分支——两臂指向同一块，分支不再决定去向，只决定
        // 目标块的实参取哪一份，故可用 `select(cond, t_args, f_args)` 合并。
        // 拒绝条件：目标块是 head 自身；目标块前驱不只本分支（`exact_users`，
        // 否则删除分支会丢掉其它前驱喂的参数）；任一实参在 head 处不可用
        // （`available_at`——select 在 head 求值，操作数必须在此可见）。
        if branch.t_target() == branch.f_target() {
            if branch.t_target() == head
                || !self.exact_users(data, branch.t_target(), &[terminator])
                || branch
                    .t_args()
                    .iter()
                    .chain(branch.f_args())
                    .any(|&value| !self.available_at(data, value, head, tree))
            {
                return None;
            }
            return self.finish_candidate(
                data,
                head,
                terminator,
                branch.t_target(),
                branch.cond(),
                branch.t_args(),
                branch.f_args(),
                vec![],
                vec![],
                tree,
            );
        }

        let t = branch.t_target();
        let f = branch.f_target();
        // 两臂都不能指向 head 自身：自环分支没有"另一条边"可合并，无法转 select。
        if t == head || f == head {
            return None;
        }

        // Full diamond: both arms are empty and jump to the same merge.
        //
        // 形状二：空 diamond——两臂都是"只有一条 jump 的空块"且跳到同一
        // merge。分支 + 两个空块 + merge 参数等价于一条 select。
        if let (Some((tj, tm, ta)), Some((fj, fm, fa))) =
            (self.empty_arm(data, t), self.empty_arm(data, f))
        {
            // 全部条件：merge 非 head/两臂本身；两臂无参数、分支也不带实参
            // （臂块不传值，值只来自两臂跳向 merge 的实参）；臂块只被本分支
            // 使用、merge 只被两条臂使用（`exact_users`，删除臂块安全）；实参
            // 在 head 处可用。
            if tm == fm
                && tm != head
                && tm != t
                && tm != f
                && data.bb_data(t).params().is_empty()
                && data.bb_data(f).params().is_empty()
                && branch.t_args().is_empty()
                && branch.f_args().is_empty()
                && self.exact_users(data, t, &[terminator])
                && self.exact_users(data, f, &[terminator])
                && self.exact_users(data, tm, &[tj, fj])
                && ta
                    .iter()
                    .chain(fa.iter())
                    .all(|&value| self.available_at(data, value, head, tree))
            {
                return self.finish_candidate(
                    data,
                    head,
                    terminator,
                    tm,
                    branch.cond(),
                    &ta,
                    &fa,
                    vec![t, f],
                    vec![],
                    tree,
                );
            }
        }

        // Triangle: one edge reaches the merge directly, the other through a
        // unique arm with a chain of safe integer operations. Prefer the
        // true-edge-arm shape; when the merge itself is jump-terminated (a
        // loop latch), `arm(f)` would still match and must not shadow the arm.
        //
        // 形状三/四：triangle——一臂直达 merge，另一臂经过唯一臂块。此处解析
        // 出"臂块 / merge / 直达边实参 / 直达边是否为 true 臂"四元组。优先
        // 把 true 臂当臂块；merge 本身以 jump 终结（循环 latch）时，对 f 调
        // `arm` 也会命中，必须让 true 臂优先，避免把臂块认错。
        let (arm, merge, direct_args, direct_is_true) = if let Some((_, m, _)) = self.arm(data, t) {
            if m == f {
                (t, f, branch.f_args(), false)
            } else if let Some((_, m2, _)) = self.arm(data, f) {
                if m2 == t {
                    (f, t, branch.t_args(), true)
                } else {
                    return None;
                }
            } else {
                return None;
            }
        } else if let Some((_, m2, _)) = self.arm(data, f) {
            if m2 == t {
                (f, t, branch.t_args(), true)
            } else {
                return None;
            }
        } else {
            return None;
        };
        let arm_edge_args = if arm == t {
            branch.t_args()
        } else {
            branch.f_args()
        };
        // 臂块必须干净：merge 不与 head/臂块重合；臂块无参数、分支不向臂块
        // 传实参、臂块只被本分支使用——这样删除臂块并把指令外提到 head 才安全。
        if merge == head
            || merge == arm
            || !data.bb_data(arm).params().is_empty()
            || !arm_edge_args.is_empty()
            || !self.exact_users(data, arm, &[terminator])
        {
            return None;
        }
        let (arm_jump, _, arm_args) = self.arm(data, arm)?;
        // merge 的前驱必须恰好是 head 分支与臂块 jump 两条边：多一个前驱
        // 就意味着还有别的路径给 merge 传参，select 合并会漏掉那条路径的值。
        if !self.exact_users(data, merge, &[terminator, arm_jump]) {
            return None;
        }

        // The arm may hold a chain of pure integer instructions ending in the
        // merge value (e.g. `c2 = eq(and(x, m), 1)` for a land/lor fold). Every
        // instruction must be single-use, feed the next link or the jump, and
        // have operands available at `head` (either dominating values or
        // earlier chain results that are hoisted together).
        //
        // 链 = 臂块内除结尾 jump 外的全部指令，将整体投机外提到 head。
        let insts = data
            .layout()
            .basicblock(arm)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        // The arm may hold a chain of pure integer instructions ending in the
        // merge value (e.g. `c2 = eq(and(x, m), 1)` for a land/lor fold).
        // Every link must be single-use, feeding the next link (or the jump
        // for the last link), with operands available at `head` or from an
        // earlier link that is hoisted along with it.
        // 逐条验证链上指令：必须可投机（`safe_arm_binary`：纯整数、无
        // div/rem、操作数可用）、单 use、且使用者是下一条链指令（中间链）
        // 或臂块 jump（末条）。任何一条不满足就整体放弃——投机外提必须
        // 一次成功，不能留下半截链。
        let mut move_insts = Vec::new();
        if insts.len() > 1 {
            let chain_len = insts.len() - 1;
            for (idx, &inst) in insts[..chain_len].iter().enumerate() {
                if !self.safe_arm_binary(
                    data,
                    inst,
                    head,
                    &move_insts.iter().map(|&(_, i)| i).collect::<Vec<_>>(),
                    tree,
                ) {
                    return None;
                }
                let users = data.inst_data(inst).used_by();
                if users.len() != 1 {
                    return None;
                }
                let feeds_next = idx + 1 < chain_len && users.contains(&insts[idx + 1]);
                let feeds_jump = idx + 1 == chain_len && users.contains(&arm_jump);
                if !feeds_next && !feeds_jump {
                    return None;
                }
                move_insts.push((arm, inst));
            }
        }
        // 可用性收尾：臂块传给 merge 的实参，要么来自链内被外提的指令，
        // 要么在 head 处可用；直达边的实参同理。
        if arm_args.iter().any(|&value| {
            !move_insts.iter().any(|&(_, inst)| inst == value)
                && !self.available_at(data, value, head, tree)
        }) || direct_args
            .iter()
            .any(|&value| !self.available_at(data, value, head, tree))
        {
            return None;
        }

        let (true_args, false_args) = if direct_is_true {
            (direct_args, arm_args.as_slice())
        } else {
            (arm_args.as_slice(), direct_args)
        };
        self.finish_candidate(
            data,
            head,
            terminator,
            merge,
            branch.cond(),
            true_args,
            false_args,
            vec![arm],
            move_insts,
            tree,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_candidate(
        &self,
        data: &ArenaContextMut<'_>,
        head: BasicBlock,
        terminator: Inst,
        merge: BasicBlock,
        cond: Inst,
        true_args: &[Inst],
        false_args: &[Inst],
        remove_blocks: Vec<BasicBlock>,
        move_insts: Vec<(BasicBlock, Inst)>,
        tree: &dom_tree::v2::DominanceTree,
    ) -> Option<Candidate> {
        // The hoisted instructions are speculated into `head`, so `head` must
        // dominate `merge` (its operands are already checked to be available
        // at `head`, and `safe_arm_binary` excludes div/rem). The previous
        // `reaches(merge, head)` rejection blocked every loop-carried
        // accumulator (`if (bit_a==1 && bit_b==1) result += power`), which is
        // exactly the profitable case; dominance is the right precondition.
        //
        // 收尾校验：把识别结果固化成 `Candidate`。支配检查 head 必须支配
        // merge——这比旧的"merge 不能到达 head"更宽松也更正确：循环累积器
        // （merge 经回边可达 head）正是最该转换的形态，支配条件放行它。
        if merge == head || !self.dominates(tree, head, merge) {
            return None;
        }
        // 参数逐对校验：merge 参数个数必须与两臂实参数一致；两臂值的类型
        // 必须与参数类型相同，且不能是 unit（select 只对有值类型有意义）。
        let params = data.bb_data(merge).params();
        if params.len() != true_args.len() || params.len() != false_args.len() {
            return None;
        }
        let mut values = Vec::with_capacity(params.len());
        for ((&param, &if_true), &if_false) in params.iter().zip(true_args).zip(false_args) {
            let ty = data.inst_data(param).ty();
            if ty.is_unit()
                || data.inst_data(if_true).ty() != ty
                || data.inst_data(if_false).ty() != ty
            {
                return None;
            }
            // 每个参数归入三种合并形态：两臂同值 → `Common` 直接复用（不产生
            // 新指令）；`0/1` 布尔形态 → `BoolBinary` 折叠成 and/or；其余 → select。
            values.push(if if_true == if_false {
                MergedValue::Common(if_true)
            } else if let Some((op, lhs, rhs)) = self.fold_land_lor(data, cond, if_true, if_false) {
                MergedValue::BoolBinary { op, lhs, rhs }
            } else {
                MergedValue::Select { if_true, if_false }
            });
        }
        // 若所有参数都是 `Common`（两臂值完全相同），转换没有任何收益，
        // 不值得为它改写 CFG，放弃。
        if !values.iter().any(|value| {
            matches!(
                value,
                MergedValue::Select { .. } | MergedValue::BoolBinary { .. }
            )
        }) {
            return None;
        }
        Some(Candidate {
            head,
            terminator,
            merge,
            cond,
            values,
            remove_blocks,
            move_insts,
        })
    }

    /// Fold `select(c1, c2, 0)` to `band(c1, c2)` and `select(c1, c1, c2)` to
    /// `bor(c1, c2)` when both operands are 0/1 comparison results. This is
    /// LLVM's `&&`/`||` lowering; Cranelift has no such pass.
    fn fold_land_lor(
        &self,
        data: &ArenaContextMut<'_>,
        cond: Inst,
        if_true: Inst,
        if_false: Inst,
    ) -> Option<(BinaryOp, Inst, Inst)> {
        // 布尔折叠：`c1 ? c2 : 0` ⇔ `c1 && c2`，`c1 ? c1 : c2` ⇔ `c1 || c2`。
        // 前提是 c1/c2 都保证取 0/1（比较结果或 0/1 常量），否则按位运算的
        // 结果不一定是布尔值，不能与 select 等价。
        // `c1 ? c2 : 0` with c1, c2 in {0,1} == `c1 && c2`.
        if self.zero_one(data, cond)
            && self.zero_one(data, if_true)
            && self.is_zero_const(data, if_false)
        {
            return Some((BinaryOp::And, cond, if_true));
        }
        // `c1 ? c1 : c2` with c1, c2 in {0,1} == `c1 || c2`.
        if self.zero_one(data, cond) && if_true == cond && self.zero_one(data, if_false) {
            return Some((BinaryOp::Or, cond, if_false));
        }
        None
    }

    /// Whether a value is guaranteed to be 0 or 1: an integer comparison
    /// result, or the constants 0/1 themselves.
    fn zero_one(&self, data: &ArenaContextMut<'_>, value: Inst) -> bool {
        match data.inst_data(value).kind() {
            InstKind::Binary(binary) => binary.op().is_compare(),
            InstKind::Integer(integer) => matches!(integer.value(), 0 | 1),
            _ => false,
        }
    }

    fn is_zero_const(&self, data: &ArenaContextMut<'_>, value: Inst) -> bool {
        matches!(data.inst_data(value).kind(), InstKind::Integer(i) if i.value() == 0)
    }

    fn empty_arm(
        &self,
        data: &ArenaContextMut<'_>,
        bb: BasicBlock,
    ) -> Option<(Inst, BasicBlock, Vec<Inst>)> {
        let insts = data.layout().basicblock(bb).insts();
        if insts.len() != 1 {
            return None;
        }
        let jump_inst = *insts.get_last()?;
        let InstKind::Jump(jump) = data.inst_data(jump_inst).kind() else {
            return None;
        };
        Some((jump_inst, jump.target(), jump.args().to_vec()))
    }

    fn arm(
        &self,
        data: &ArenaContextMut<'_>,
        bb: BasicBlock,
    ) -> Option<(Inst, BasicBlock, Vec<Inst>)> {
        let jump_inst = *data.layout().basicblock(bb).insts().get_last()?;
        let InstKind::Jump(jump) = data.inst_data(jump_inst).kind() else {
            return None;
        };
        Some((jump_inst, jump.target(), jump.args().to_vec()))
    }

    /// Whether an arm instruction is speculatable: a pure i32 integer binary
    /// (no div/rem, no side effects) whose operands are either available at
    /// `head` or produced by an earlier chain link hoisted along with it.
    fn safe_arm_binary(
        &self,
        data: &ArenaContextMut<'_>,
        inst: Inst,
        head: BasicBlock,
        chain: &[Inst],
        tree: &dom_tree::v2::DominanceTree,
    ) -> bool {
        // 可投机性判定：只收纯 i32 整数二元运算——排除 div/rem（除零会
        // trap，投机执行会凭空引入原程序没有的异常，破坏可观察行为），
        // 且操作数在 head 处可用或由链内先前指令产生（随链一起外提）。
        let InstKind::Binary(binary) = data.inst_data(inst).kind() else {
            return false;
        };
        let operand_ok =
            |value: Inst| chain.contains(&value) || self.available_at(data, value, head, tree);
        data.inst_data(inst).ty().is_i32()
            && data.inst_data(binary.lhs()).ty().is_i32()
            && data.inst_data(binary.rhs()).ty().is_i32()
            && !matches!(binary.op(), BinaryOp::Div | BinaryOp::Rem)
            && operand_ok(binary.lhs())
            && operand_ok(binary.rhs())
    }

    fn available_at(
        &self,
        data: &ArenaContextMut<'_>,
        value: Inst,
        head: BasicBlock,
        tree: &dom_tree::v2::DominanceTree,
    ) -> bool {
        // 可用性判定：一个值能否在 head 处被安全引用。全局值、常量、head
        // 自己的参数天然可用；其余值要求定义块支配 head，或定义就在 head
        // 内且不是终结符（终结符在 select 之后才执行，不能被它引用）。
        if value.is_global()
            || data.inst_data(value).is_const()
            || data.bb_data(head).params().contains(&value)
        {
            return true;
        }
        // Block parameters are not part of the instruction layout, so the
        // `parent_bb` lookup below cannot place them. Resolve their defining
        // block explicitly and apply ordinary dominance. This lets values
        // derived from the entry-block parameters (which mirror the function
        // arguments and dominate the whole body) be hoisted anywhere.
        if matches!(data.inst_data(value).kind(), InstKind::BlockArgRef(..)) {
            return self.block_of_param(data, value).is_some_and(|def_block| {
                def_block == head || self.dominates(tree, def_block, head)
            });
        }
        let Some(def_bb) = data.layout().parent_bb(value) else {
            return false;
        };
        if def_bb == head {
            let insts = data.layout().basicblock(head).insts();
            return insts.iter().any(|&inst| inst == value)
                && insts.get_last().is_some_and(|&term| term != value);
        }
        self.dominates(tree, def_bb, head)
    }

    fn block_of_param(&self, data: &ArenaContextMut<'_>, value: Inst) -> Option<BasicBlock> {
        data.layout()
            .basicblocks()
            .iter()
            .find(|layout| data.bb_data(layout.bb()).params().contains(&value))
            .map(|layout| layout.bb())
    }

    /// Whether `dominator` dominates `block`. Uses a dominator tree computed
    /// from the CFG snapshot at the start of this pass invocation. The pass
    /// only ever removes blocks and edges, which cannot turn a true dominance
    /// into a false one, so the snapshot stays sound; an occasional
    /// still-valid candidate may be rejected and simply revisited by the
    /// pipeline's fixed point on its next iteration.
    fn dominates(
        &self,
        tree: &dom_tree::v2::DominanceTree,
        dominator: BasicBlock,
        block: BasicBlock,
    ) -> bool {
        if dominator == block {
            return true;
        }
        if tree.entry() == dominator {
            return true;
        }
        // Preserve the legacy semantics for blocks outside the snapshot (made
        // unreachable or removed): an unreachable target is dominated by
        // everything, and an unreachable dominator does not dominate a
        // reachable target.
        if !tree.contains(block) {
            return true;
        }
        if !tree.contains(dominator) {
            return false;
        }
        tree.dominates(dominator, block)
    }

    fn exact_users(&self, data: &ArenaContextMut<'_>, bb: BasicBlock, expected: &[Inst]) -> bool {
        // 前驱精确匹配：块的使用者（前驱边）集合必须恰好等于 expected。
        // 这是删除/改写安全性的基石——多一条前驱就多一条传参路径，
        // 遗漏它会导致合并后的值语义错误。
        let users = data
            .bb_data(bb)
            .used_by()
            .iter()
            .copied()
            .filter(|&inst| data.layout().parent_bb(inst).is_some())
            .collect::<HashSet<_>>();
        users.len() == expected.len() && expected.iter().all(|inst| users.contains(inst))
    }

    fn apply(&self, data: &mut ArenaContextMut<'_>, candidate: Candidate) {
        // 改写执行：把 head 的 `branch` 换成对 merge 的无参 `jump`，merge 的
        // 每个参数用 select / and-or 取代。步骤：删旧终结符 → 外提链上指令
        // → 逐个生成替换值 → 统一替换参数引用 → 清参数删孤儿 → 插新 jump
        // → 删空臂块。全程保持指令身份不变，只改布局与引用。
        let params = data.bb_data(candidate.merge).params().clone();

        // Remove the old terminator first so newly inserted values naturally
        // precede the replacement jump in layout order.
        //
        // 先删终结符：之后插入的 select 会排在 jump 之前（布局序），head 的
        // 指令顺序自然变成"…select…jump"。
        data.remove_layout_inst(candidate.head, candidate.terminator);
        // 外提链上指令：整条 move 而非克隆，指令身份与 def-use 链不变，
        // 其它指令对链上结果的引用自动跟随新位置。
        for (arm, inst) in &candidate.move_insts {
            // Moving preserves the instruction identity and all use-def links.
            data.layout_mut().remove_inst(*arm, *inst);
            data.layout_mut().insert_inst(candidate.head, *inst);
        }

        // 生成替换值：`Common` 直接复用原指令；`Select` 造 `select(cond, t, f)`
        // ——与原分支+Phi 语义等价（true 边取 t、false 边取 f），只是把控制流
        // 依赖换成数据依赖；`BoolBinary` 造 and/or 布尔指令。新指令都插在 head。
        let mut replacements = Vec::with_capacity(params.len());
        for value in candidate.values {
            let replacement = match value {
                MergedValue::Common(value) => value,
                MergedValue::Select { if_true, if_false } => {
                    let select = data
                        .new_local_inst()
                        .select(candidate.cond, if_true, if_false);
                    data.layout_mut().insert_inst(candidate.head, select);
                    select
                }
                MergedValue::BoolBinary { op, lhs, rhs } => {
                    let binary = data.new_local_inst().binary(op, lhs, rhs);
                    data.layout_mut().insert_inst(candidate.head, binary);
                    binary
                }
            };
            replacements.push(replacement);
        }

        // Validate every parameter before this point and rewrite all of them as
        // one transaction; never leave a partially converted merge signature.
        //
        // 原子替换：把 merge 参数的每个使用点一次性换成新值，随后清空参数
        // 列表并删除成为孤儿的参数指令——不留下"半转换"的 merge 签名。
        for (&param, &replacement) in params.iter().zip(&replacements) {
            utils::visit_and_replace(data, param, replacement);
            assert!(data.inst_data(param).used_by().is_empty());
        }
        data.bb_data_mut(candidate.merge).params_mut().clear();
        for param in params {
            data.remove_orphan_inst(param);
        }
        // 新 jump 不带实参（参数已被 select/and-or 取代），最后删掉空臂块。
        let jump = data.new_local_inst().jump(candidate.merge, vec![]);
        data.layout_mut().insert_inst(candidate.head, jump);
        for bb in candidate.remove_blocks {
            data.remove_layout_basicblock(bb);
        }
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

    fn blocks(data: &FunctionData) -> Vec<BasicBlock> {
        data.layout()
            .basicblocks()
            .iter()
            .map(|layout| layout.bb())
            .collect()
    }

    fn select_count(data: &FunctionData) -> usize {
        data.layout()
            .basicblocks()
            .iter()
            .flat_map(|layout| layout.insts())
            .filter(|&&inst| matches!(data.inst_data(inst).kind(), InstKind::Select(..)))
            .count()
    }

    fn run(program: &mut Program) {
        IfConversion.run(program);
    }

    #[test]
    fn converts_same_target_branch_atomically() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "same".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let __pty = data.params_ty().to_vec();
        let head = data.new_basic_block().basic_block("head".into(), __pty);
        let __params = data.bb_data(head).params().to_vec();

        data.set_params(__params);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32(), Type::get_i32()]);
        data.layout_mut().push_bb_back(head);
        data.layout_mut().push_bb_back(merge);
        let cond = data.params()[0];
        let one = data.new_local_inst().integer(1);
        let two = data.new_local_inst().integer(2);
        let branch =
            data.new_local_inst()
                .branch(cond, merge, vec![one, two], merge, vec![two, two]);
        data.layout_mut().insert_inst(head, branch);
        let params = data.bb_data(merge).params().clone();
        let sum = data
            .new_local_inst()
            .binary(BinaryOp::Add, params[0], params[1]);
        data.layout_mut().insert_inst(merge, sum);
        let ret = data.new_local_inst().ret(Some(sum));
        data.layout_mut().insert_inst(merge, ret);

        run(&mut program);
        let data = program.func_data(function);
        assert_eq!(blocks(data).len(), 2);
        assert_eq!(select_count(data), 1);
        assert!(data.bb_data(merge).params().is_empty());
        let terminator = utils::get_terminator_inst(data, head);
        assert!(
            matches!(data.inst_data(terminator).kind(), InstKind::Jump(j) if j.target() == merge && j.args().is_empty())
        );
    }

    #[test]
    fn converts_empty_diamond_with_dominating_values() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "diamond".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let __pty = data.params_ty().to_vec();
        let head = data.new_basic_block().basic_block("head".into(), __pty);
        let __params = data.bb_data(head).params().to_vec();

        data.set_params(__params);
        let yes = data.new_basic_block().basic_block("yes".into(), vec![]);
        let no = data.new_basic_block().basic_block("no".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        for bb in [head, yes, no, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let cond = data.params()[0];
        let one = data.new_local_inst().integer(1);
        let dominating = data.new_local_inst().binary(BinaryOp::Add, cond, one);
        data.layout_mut().insert_inst(head, dominating);
        let branch = data.new_local_inst().branch(cond, yes, vec![], no, vec![]);
        data.layout_mut().insert_inst(head, branch);
        let yes_jump = data.new_local_inst().jump(merge, vec![dominating]);
        data.layout_mut().insert_inst(yes, yes_jump);
        let no_jump = data.new_local_inst().jump(merge, vec![one]);
        data.layout_mut().insert_inst(no, no_jump);
        let param = data.bb_data(merge).params()[0];
        let ret = data.new_local_inst().ret(Some(param));
        data.layout_mut().insert_inst(merge, ret);

        run(&mut program);
        let data = program.func_data(function);
        assert_eq!(blocks(data), vec![head, merge]);
        assert_eq!(select_count(data), 1);
    }

    #[test]
    fn converts_abs_triangle_with_sub_zero_x() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "abs".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let __pty = data.params_ty().to_vec();
        let head = data.new_basic_block().basic_block("head".into(), __pty);
        let __params = data.bb_data(head).params().to_vec();

        data.set_params(__params);
        let neg = data.new_basic_block().basic_block("neg".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        for bb in [head, neg, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let x = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let branch = data.new_local_inst().branch(x, merge, vec![x], neg, vec![]);
        data.layout_mut().insert_inst(head, branch);
        let negate = data.new_local_inst().binary(BinaryOp::Sub, zero, x);
        data.layout_mut().insert_inst(neg, negate);
        let jump = data.new_local_inst().jump(merge, vec![negate]);
        data.layout_mut().insert_inst(neg, jump);
        let param = data.bb_data(merge).params()[0];
        let ret = data.new_local_inst().ret(Some(param));
        data.layout_mut().insert_inst(merge, ret);

        run(&mut program);
        let data = program.func_data(function);
        assert_eq!(blocks(data), vec![head, merge]);
        assert_eq!(data.layout().parent_bb(negate), Some(head));
        assert_eq!(select_count(data), 1);
    }

    #[test]
    fn rejects_arm_local_with_external_user() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "external".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let __pty = data.params_ty().to_vec();
        let head = data.new_basic_block().basic_block("head".into(), __pty);
        let __params = data.bb_data(head).params().to_vec();

        data.set_params(__params);
        let arm = data.new_basic_block().basic_block("arm".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        for bb in [head, arm, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let x = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let branch = data.new_local_inst().branch(x, merge, vec![x], arm, vec![]);
        data.layout_mut().insert_inst(head, branch);
        let local = data.new_local_inst().binary(BinaryOp::Sub, zero, x);
        data.layout_mut().insert_inst(arm, local);
        let jump = data.new_local_inst().jump(merge, vec![local]);
        data.layout_mut().insert_inst(arm, jump);
        let param = data.bb_data(merge).params()[0];
        let external = data.new_local_inst().binary(BinaryOp::Add, param, local);
        data.layout_mut().insert_inst(merge, external);
        let ret = data.new_local_inst().ret(Some(external));
        data.layout_mut().insert_inst(merge, ret);

        run(&mut program);
        let data = program.func_data(function);
        assert_eq!(blocks(data).len(), 3);
        assert_eq!(select_count(data), 0);
        assert!(matches!(
            data.inst_data(branch).kind(),
            InstKind::Branch(..)
        ));
    }

    #[test]
    fn rejects_merge_with_extra_predecessor_and_trapping_binary() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "negative".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let __pty = data.params_ty().to_vec();
        let head = data.new_basic_block().basic_block("head".into(), __pty);
        let __params = data.bb_data(head).params().to_vec();

        data.set_params(__params);
        let arm = data.new_basic_block().basic_block("arm".into(), vec![]);
        let extra = data.new_basic_block().basic_block("extra".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        for bb in [head, arm, extra, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let x = data.params()[0];
        let one = data.new_local_inst().integer(1);
        let branch = data.new_local_inst().branch(x, merge, vec![x], arm, vec![]);
        data.layout_mut().insert_inst(head, branch);
        let div = data.new_local_inst().binary(BinaryOp::Div, one, x);
        data.layout_mut().insert_inst(arm, div);
        let arm_jump = data.new_local_inst().jump(merge, vec![div]);
        data.layout_mut().insert_inst(arm, arm_jump);
        let extra_jump = data.new_local_inst().jump(merge, vec![one]);
        data.layout_mut().insert_inst(extra, extra_jump);
        let param = data.bb_data(merge).params()[0];
        let ret = data.new_local_inst().ret(Some(param));
        data.layout_mut().insert_inst(merge, ret);

        run(&mut program);
        let data = program.func_data(function);
        assert_eq!(blocks(data).len(), 4);
        assert_eq!(select_count(data), 0);
    }

    #[test]
    fn converts_same_target_loop_preserving_the_loop() {
        // A branch whose merge reaches the head used to be rejected wholesale;
        // M31 relaxes this to a dominance check, so the loop survives but the
        // branch becomes a select (correct: head dominates merge, and the
        // select's operands are all available at head).
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "loop".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let __pty = data.params_ty().to_vec();
        let head = data.new_basic_block().basic_block("head".into(), __pty);
        let __params = data.bb_data(head).params().to_vec();

        data.set_params(__params);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        for bb in [head, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let cond = data.params()[0];
        let one = data.new_local_inst().integer(1);
        let two = data.new_local_inst().integer(2);
        let branch = data
            .new_local_inst()
            .branch(cond, merge, vec![one], merge, vec![two]);
        data.layout_mut().insert_inst(head, branch);
        let backedge = data.new_local_inst().jump(head, vec![cond]);
        data.layout_mut().insert_inst(merge, backedge);

        run(&mut program);
        let data = program.func_data(function);
        assert_eq!(blocks(data).len(), 2, "loop must survive");
        assert_eq!(select_count(data), 1);
        assert!(matches!(
            data.inst_data(backedge).kind(),
            InstKind::Jump(j) if j.target() == head
        ));
    }

    #[test]
    fn folds_land_triangle_into_band() {
        // `if (bit_a == 1 && bit_b == 1) result += power` (land shape):
        //   head: c1 = eq ...; br c1, rhs, merge(0, ...)
        //   rhs:  c2 = eq ...; jump merge(c2, ...)
        // folds to band(c1, c2) in head, rhs deleted.
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "land".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let __pty = data.params_ty().to_vec();
        let head = data.new_basic_block().basic_block("head".into(), __pty);
        let __params = data.bb_data(head).params().to_vec();

        data.set_params(__params);
        let rhs = data.new_basic_block().basic_block("rhs".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32(), Type::get_i32()]);
        for bb in [head, rhs, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let a = data.params()[0];
        let one_a = data.new_local_inst().integer(1);
        let c1 = data.new_local_inst().binary(BinaryOp::Eq, a, one_a);
        data.layout_mut().insert_inst(head, c1);
        let seven = data.new_local_inst().integer(7);
        let pass = data.new_local_inst().binary(BinaryOp::Add, a, seven);
        data.layout_mut().insert_inst(head, pass);
        let zero = data.new_local_inst().integer(0);
        let two_a = data.new_local_inst().integer(2);
        let c2 = data.new_local_inst().binary(BinaryOp::Eq, a, two_a);
        data.layout_mut().insert_inst(rhs, c2);
        let branch = data
            .new_local_inst()
            .branch(c1, rhs, vec![], merge, vec![zero, pass]);
        data.layout_mut().insert_inst(head, branch);
        let rhs_jump = data.new_local_inst().jump(merge, vec![c2, pass]);
        data.layout_mut().insert_inst(rhs, rhs_jump);
        let param = data.bb_data(merge).params()[0];
        let ret = data.new_local_inst().ret(Some(param));
        data.layout_mut().insert_inst(merge, ret);

        run(&mut program);
        let data = program.func_data(function);
        assert_eq!(blocks(data).len(), 2, "rhs block must be removed");
        assert_eq!(select_count(data), 0);
        let band_count = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|layout| layout.insts())
            .filter(|&&inst| {
                matches!(data.inst_data(inst).kind(), InstKind::Binary(b) if b.op() == BinaryOp::And)
                    && data.inst_data(inst).used_by().len() == 1
            })
            .count();
        assert_eq!(band_count, 1, "one band must replace the land branch");
        assert!(data.layout().parent_bb(c2).is_some_and(|bb| bb == head));
    }

    #[test]
    fn folds_lor_triangle_into_bor() {
        // `if (bit_a == 1 || bit_b == 1) ...` (lor shape):
        //   head: c1 = eq ...; br c1, merge(c1, ...), rhs
        //   rhs:  c2 = eq ...; jump merge(c2, ...)
        // folds to bor(c1, c2) in head, rhs deleted.
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "lor".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let __pty = data.params_ty().to_vec();
        let head = data.new_basic_block().basic_block("head".into(), __pty);
        let __params = data.bb_data(head).params().to_vec();

        data.set_params(__params);
        let rhs = data.new_basic_block().basic_block("rhs".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32(), Type::get_i32()]);
        for bb in [head, rhs, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let a = data.params()[0];
        let one_a = data.new_local_inst().integer(1);
        let c1 = data.new_local_inst().binary(BinaryOp::Eq, a, one_a);
        data.layout_mut().insert_inst(head, c1);
        let seven = data.new_local_inst().integer(7);
        let pass = data.new_local_inst().binary(BinaryOp::Add, a, seven);
        data.layout_mut().insert_inst(head, pass);
        let branch = data
            .new_local_inst()
            .branch(c1, merge, vec![c1, pass], rhs, vec![]);
        data.layout_mut().insert_inst(head, branch);
        let two_a = data.new_local_inst().integer(2);
        let c2 = data.new_local_inst().binary(BinaryOp::Eq, a, two_a);
        data.layout_mut().insert_inst(rhs, c2);
        let rhs_jump = data.new_local_inst().jump(merge, vec![c2, pass]);
        data.layout_mut().insert_inst(rhs, rhs_jump);
        let param = data.bb_data(merge).params()[0];
        let ret = data.new_local_inst().ret(Some(param));
        data.layout_mut().insert_inst(merge, ret);

        run(&mut program);
        let data = program.func_data(function);
        assert_eq!(blocks(data).len(), 2, "rhs block must be removed");
        assert_eq!(select_count(data), 0);
        let bor_count = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|layout| layout.insts())
            .filter(|&&inst| {
                matches!(data.inst_data(inst).kind(), InstKind::Binary(b) if b.op() == BinaryOp::Or)
                    && data.inst_data(inst).used_by().len() == 1
            })
            .count();
        assert_eq!(bor_count, 1, "one bor must replace the lor branch");
    }

    #[test]
    fn converts_loop_accumulator_with_speculation() {
        // The M31 headline: `while (len) { if (bit_a == 1 && bit_b == 1)
        // result += power; ... }` — the inner triangle's merge is reachable
        // from its head through the loop back-edge, which the old
        // `reaches(merge, head)` guard rejected. head dominates merge, so the
        // accumulator now converts to a select.
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "acc".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let __pty = data.params_ty().to_vec();
        let head = data.new_basic_block().basic_block("head".into(), __pty);
        let __params = data.bb_data(head).params().to_vec();

        data.set_params(__params);
        let arm = data.new_basic_block().basic_block("arm".into(), vec![]);
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        for bb in [head, arm, merge] {
            data.layout_mut().push_bb_back(bb);
        }
        let a = data.params()[0];
        let one_a = data.new_local_inst().integer(1);
        let cond = data.new_local_inst().binary(BinaryOp::Eq, a, one_a);
        data.layout_mut().insert_inst(head, cond);
        let branch = data
            .new_local_inst()
            .branch(cond, arm, vec![], merge, vec![a]);
        data.layout_mut().insert_inst(head, branch);
        // arm: result' = result + power (operands dominate head).
        let one_b = data.new_local_inst().integer(1);
        let inc = data.new_local_inst().binary(BinaryOp::Add, a, one_b);
        data.layout_mut().insert_inst(arm, inc);
        let arm_jump = data.new_local_inst().jump(merge, vec![inc]);
        data.layout_mut().insert_inst(arm, arm_jump);
        let param = data.bb_data(merge).params()[0];
        let back = data.new_local_inst().jump(head, vec![param]);
        data.layout_mut().insert_inst(merge, back);

        run(&mut program);
        let data = program.func_data(function);
        assert_eq!(blocks(data).len(), 2, "arm must be removed, loop survives");
        assert_eq!(select_count(data), 1);
        assert!(data.layout().parent_bb(inc).is_some_and(|bb| bb == head));
        assert!(matches!(
            data.inst_data(back).kind(),
            InstKind::Jump(j) if j.target() == head
        ));
    }
}
