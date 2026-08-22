//! # BooleanSimplification：布尔值规范化与化简
//!
//! 把"真值测试"统一到规范形态，并消除布尔包装。核心约定（见
//! `BooleanSimplification` 的英文文档）：**比较指令产生规范的 0/1**；
//! `branch` / `select` 只把条件当作"非零即真"。本 pass 在**确认值确实是
//! 规范布尔**（`is_canonical_bool`：常量 0/1、比较结果、或两臂都是规范
//! 布尔的 select）之后才做替换，绝不把任意 i32 真值（如 2）当布尔用——
//! 这是正确性的根基。
//!
//! ## 变换形态（IR 示例）
//!
//! ```text
//! ① x == x              →   1        （Eq/Ge/Le → 1；NotEq/Gt/Lt → 0）
//! ② (a < b) != 0        →   a < b     （比较结果的 truthiness 包装剥掉）
//!    (a < b) == 1        →   a < b
//!    (a < b) == 0        →   a >= b    （补比较 complement_integer_compare）
//! ③ br (c == 0), A, B   →   br c, B, A（交换两臂）
//! ④ select(c, c, 0)     →   c
//!    select(c, 1, 0)    →   c         （规范布尔投影）
//!    select(c, 0, 1)    →   c == 0
//!    select(c == 0, a, b) → select(c, b, a)
//! ```
//!
//! ## 触发 / 放弃条件
//!
//! - ①只认 i32 比较且操作数相同；
//! - ②③④都要求涉及的布尔值是**规范布尔**（`is_canonical_bool`，带
//!   `visiting` 集合防环）；不是则放弃；
//! - ②的 `== 0` / `!= 1` 分支要求内层是比较指令且能取补（
//!   `complement_integer_compare`），否则放弃；
//! - ③要求条件形如 `value == 0` / `value != 0`（`zero_comparison`）。
//!
//! ## 正确性
//!
//! - 所有替换都保持"条件非零即真"的语义；补比较只在规范布尔上做，0/1
//!   域内取补恒等；
//! - `is_canonical_bool` 的递归检查确保 select 展开后仍是 0/1。
//!
//! ## 管线位置
//!
//! - 注册：`opt/pass.rs` 的 `from_config`，fixpoint 段，`tail_recursive_inline`
//!   之后、`gvn_pre` 之前（尽早把布尔形态规范化，供后续 pass 匹配）；
//! - 无目标门控、无 config 开关。
//!
//! ## 验证
//!
//! - 本文件 `mod tests`（232 行起）覆盖各规则与真值边界；
//! - 端到端：`make test` 差分比对。

use crate::ir::{Binary, Branch, Select};
use crate::opt::prelude::*;

/// Canonicalizes boolean values without conflating them with arbitrary i32
/// truth values. Comparisons produce canonical `0`/`1`; branches and selects
/// merely test their condition for zero versus nonzero.
pub struct BooleanSimplification;

impl Pass for BooleanSimplification {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        // 固定点循环：只要某一轮扫描化简了任何指令就再来一轮。规则②③④会"剥掉"
        // 一层比较包装，可能暴露出新的化简机会（如 select(c,1,0)→c 之后，c 若是
        // (a<b)!=0，下一轮再由规则②剥掉），所以必须跑到不再变化为止。
        let mut changed = false;
        loop {
            // 每轮先收集全部指令的快照再遍历：simplify_inst 会改写/删除指令，
            // 在快照上迭代既避免借用冲突，也避免处理已被删除的指令；
            // 本轮新产生的指令会在下一轮的快照里被看到。
            let insts = data
                .layout()
                .basicblocks()
                .iter()
                .flat_map(|layout| layout.insts().iter().copied())
                .collect::<Vec<_>>();
            if !insts.into_iter().any(|inst| self.simplify_inst(data, inst)) {
                return changed;
            }
            // 终止性：每个规则要么删除一条指令（replace_value），要么把条件上的
            // "与 0/1 比较"包装剥掉一层（比较嵌套深度严格下降），不会来回震荡。
            // changed 记录整个 pass 是否改过任何东西，供上层判断是否值得继续后续 pass。
            changed = true;
        }
    }
}

impl BooleanSimplification {
    fn simplify_inst(&self, data: &mut ArenaContextMut<'_>, inst: Inst) -> bool {
        // 按指令种类分发：只有 Binary / Branch / Select 可能携带"布尔测试"形态，
        // 其余指令（调用、load、store 等）一律不处理。kind() 返回借用，这里 clone
        // 出所有权，才能把 binary/branch/select 传进需要 &mut data 的改写函数。
        match data.inst_data(inst).kind().clone() {
            InstKind::Binary(binary) => self.simplify_binary(data, inst, binary),
            InstKind::Branch(branch) => self.simplify_branch(data, inst, branch),
            InstKind::Select(select) => self.simplify_select(data, inst, select),
            _ => false,
        }
    }

    fn simplify_binary(&self, data: &mut ArenaContextMut<'_>, inst: Inst, binary: Binary) -> bool {
        // 规则①：同一操作数自比较。只对 i32 做——i32 无 NaN，x==x / x>=x 恒真、
        // x!=x 恒假；浮点下 x==x 遇 NaN 为假、x>=x 亦为假，故必须保守放弃。
        if binary.op().is_compare()
            && data.inst_data(binary.lhs()).ty().is_i32()
            && binary.lhs() == binary.rhs()
        {
            // 相等类比较（Eq/Ge/Le）恒为 1，不等类（NotEq/Gt/Lt）恒为 0。
            // is_compare() 已把分支限定在这 6 个操作符上，unreachable!() 只是兜底。
            let value = match binary.op() {
                BinaryOp::Eq | BinaryOp::Ge | BinaryOp::Le => 1,
                BinaryOp::NotEq | BinaryOp::Gt | BinaryOp::Lt => 0,
                _ => unreachable!(),
            };
            let replacement = data.new_local_inst().integer(value);
            self.replace_value(data, inst, replacement);
            return true;
        }

        // 规则②：剥掉比较结果的 truthiness 包装。boolean_comparison 识别
        // `value == 0/1` / `value != 0/1`（常数在左或在右均可），返回 (value, 常数)。
        let Some((value, expected)) = self.boolean_comparison(data, &binary) else {
            return false;
        };
        // 安全闸门：value 必须是规范布尔（常量 0/1、比较结果、或两臂皆规范布尔的
        // select）。否则 value 可能是任意真值（如 2），直接替换会改变程序语义。
        if !self.is_canonical_bool(data, value, &mut HashSet::default()) {
            return false;
        }

        match (binary.op(), expected) {
            // value != 0 或 value == 1：等价于"value 为真"，直接剥掉包装用 value 本身。
            (BinaryOp::NotEq, 0) | (BinaryOp::Eq, 1) => {
                self.replace_value(data, inst, value);
            }
            // value == 0 或 value != 1：相当于对 value 取反。value 必须是 i32 比较
            // 指令才能用补比较就地改写；浮点比较取补在 NaN 上不等价（a==b 与 a!=b
            // 对 NaN 都为 0），所以先查操作数类型，再试 complement_integer_compare。
            (BinaryOp::Eq, 0) | (BinaryOp::NotEq, 1) => {
                let InstKind::Binary(inner) = data.inst_data(value).kind().clone() else {
                    return false;
                };
                // 比较指令两操作数类型相同（builder 强制），查 lhs 一个即可。
                if !inner.op().is_compare() || !data.inst_data(inner.lhs()).ty().is_i32() {
                    return false;
                }
                let Some(op) = inner.op().complement_integer_compare() else {
                    return false;
                };
                // 就地改写：(a<b)==0 → a>=b，消除布尔包装且不新增指令。
                data.replace_inst_with(inst)
                    .binary(op, inner.lhs(), inner.rhs());
            }
            _ => return false,
        }
        true
    }

    fn simplify_branch(&self, data: &mut ArenaContextMut<'_>, inst: Inst, branch: Branch) -> bool {
        // 规则③：剥掉条件上的零测试。注意这里只要求 value 是 i32、不要求规范布尔——
        // branch 的语义就是"非零即真"：value != 0 为真 ⇔ value 非零，对任意 i32 恒成立。
        let Some((value, expected)) = self.zero_comparison(data, branch.cond()) else {
            return false;
        };
        // 只处理标量 i32 条件；向量掩码分支不在此 pass 的职责内。
        if !data.inst_data(value).ty().is_i32() {
            return false;
        }
        if expected {
            // cond 是 value == 0：value==0 时走原真目标，等价于交换两臂后以 value
            // 本身做条件（branch(value, F, T)）。
            data.replace_inst_with(inst).branch(
                value,
                branch.f_target(),
                branch.f_args().to_vec(),
                branch.t_target(),
                branch.t_args().to_vec(),
            );
        } else {
            // cond 是 value != 0：两臂原样保留，仅把条件换成 value。
            data.replace_inst_with(inst).branch(
                value,
                branch.t_target(),
                branch.t_args().to_vec(),
                branch.f_target(),
                branch.f_args().to_vec(),
            );
        }
        true
    }

    fn simplify_select(&self, data: &mut ArenaContextMut<'_>, inst: Inst, select: Select) -> bool {
        // 规则④，按"代价从低到高"依次尝试：先做纯语法检查，最后才递归
        // is_canonical_bool；任何一个分支成功改写就立即返回。
        // 两臂相同：条件无关，select 退化为该臂。对任意类型、任意条件恒成立。
        if select.if_true() == select.if_false() {
            self.replace_value(data, inst, select.if_true());
            return true;
        }
        // select(c, c, 0) → c：条件为真得 c、为假得 0 也等于 c，两路都收敛到 c。
        // 该模式对任意 i32 c 成立（无需 c 是规范布尔），是最便宜的改写。
        if select.if_true() == select.cond() && self.is_zero(data, select.if_false()) {
            self.replace_value(data, inst, select.cond());
            return true;
        }

        // 条件本身是零测试 `value == 0` / `value != 0`：剥掉一层并相应交换两臂。
        // select(value==0, a, b) = value 非零 ? b : a = select(value, b, a)。
        // 与规则③同理，"非零即真"下对任意 i32 value 恒等价，无需规范布尔。
        if let Some((value, is_eq)) = self.zero_comparison(data, select.cond()) {
            // 只处理标量 i32；向量掩码条件不在此 pass 范围内。
            if data.inst_data(value).ty().is_i32() {
                if is_eq {
                    // value == 0 为真 ⇔ value 非零：交换两臂，value 非零时取原 if_false 臂。
                    data.replace_inst_with(inst)
                        .select(value, select.if_false(), select.if_true());
                } else {
                    // value != 0：两臂不变，仅把条件换成 value。
                    data.replace_inst_with(inst)
                        .select(value, select.if_true(), select.if_false());
                }
                return true;
            }
        }

        // 兜底路径——"规范布尔投影"：select(c, 1, 0) → c、select(c, 0, 1) → c == 0。
        // 结果要么替换成 c、要么替换成 c==0，都要求 c 是规范布尔：若 c 是任意真值
        // （如 2），select(c,1,0) 得 1 ≠ 2，直接替换会破坏语义。
        // 结果类型闸门：只有 i32 结果才做投影（c 与 c==0 都是 i32）。
        if !data.inst_data(inst).ty().is_i32()
            || !self.is_canonical_bool(data, select.cond(), &mut HashSet::default())
        {
            return false;
        }
        if self.is_one(data, select.if_true()) && self.is_zero(data, select.if_false()) {
            // select(c, 1, 0)：条件为真得 1、为假得 0，恰是 c 的规范值本身。
            self.replace_value(data, inst, select.cond());
            return true;
        }
        if self.is_zero(data, select.if_true()) && self.is_one(data, select.if_false()) {
            // select(c, 0, 1) = (c == 0)：把"取反的布尔值"编译成一次 i32 比较。
            let zero = data.new_local_inst().integer(0);
            data.replace_inst_with(inst)
                .binary(BinaryOp::Eq, select.cond(), zero);
            return true;
        }
        false
    }

    /// Returns `(value, is_eq)` for `value == 0` / `value != 0`.
    fn zero_comparison(&self, data: &ArenaContextMut<'_>, inst: Inst) -> Option<(Inst, bool)> {
        let InstKind::Binary(binary) = data.inst_data(inst).kind() else {
            return None;
        };
        // 只认 Eq / NotEq 两种"零测试"形态；Gt/Lt 等比较不是 truthiness 测试。
        let is_eq = match binary.op() {
            BinaryOp::Eq => true,
            BinaryOp::NotEq => false,
            _ => return None,
        };
        // 常数 0 可能在左也可能在右（0 == x 与 x == 0 等价），两种情况都识别。
        if self.is_zero(data, binary.lhs()) {
            Some((binary.rhs(), is_eq))
        } else if self.is_zero(data, binary.rhs()) {
            Some((binary.lhs(), is_eq))
        } else {
            None
        }
    }

    /// Returns `(canonical_boolean, expected_value)` for equality-like tests
    /// against 0 or 1.
    fn boolean_comparison(
        &self,
        data: &ArenaContextMut<'_>,
        binary: &Binary,
    ) -> Option<(Inst, i32)> {
        // 只识别"与 0/1 比较"的 Eq/NotEq；与 2、-1 等常数比较不是布尔测试形态。
        if !matches!(binary.op(), BinaryOp::Eq | BinaryOp::NotEq) {
            return None;
        }
        // 常数在左或在右都识别（1 == x 与 x == 1 等价），返回另一侧操作数。
        if let Some(value) = self.integer_constant(data, binary.lhs()) {
            if matches!(value, 0 | 1) {
                return Some((binary.rhs(), value));
            }
        }
        if let Some(value) = self.integer_constant(data, binary.rhs()) {
            if matches!(value, 0 | 1) {
                return Some((binary.lhs(), value));
            }
        }
        None
    }

    fn is_canonical_bool(
        &self,
        data: &ArenaContextMut<'_>,
        value: Inst,
        visiting: &mut HashSet<Inst>,
    ) -> bool {
        // 递归判定 value 是否保证取值 0/1（规范布尔）：常量 0/1、比较指令的结果
        // （比较产生规范 0/1）、或两臂都是规范布尔的 select。只有通过此检查，
        // 调用方才敢把 value 当作布尔值直接替换。
        // visiting 是"当前路径"集合而非全局已访问集合：同一值可能从不同路径被多次
        // 检查，所以查完必须 remove 归还；SSA 下 def-use 天然无环，防环只是防御性
        // 措施（防止未来 IR 改动引入环导致无限递归）。
        if !visiting.insert(value) {
            return false;
        }
        let result = matches!(self.integer_constant(data, value), Some(0 | 1))
            || matches!(data.inst_data(value).kind(), InstKind::Binary(binary) if binary.op().is_compare())
            || matches!(data.inst_data(value).kind(), InstKind::Select(select)
                if self.is_canonical_bool(data, select.if_true(), visiting)
                    && self.is_canonical_bool(data, select.if_false(), visiting));
        visiting.remove(&value);
        result
    }

    fn integer_constant(&self, data: &ArenaContextMut<'_>, inst: Inst) -> Option<i32> {
        let InstKind::Integer(integer) = data.inst_data(inst).kind() else {
            return None;
        };
        Some(integer.value())
    }

    fn is_zero(&self, data: &ArenaContextMut<'_>, inst: Inst) -> bool {
        self.integer_constant(data, inst) == Some(0)
    }

    fn is_one(&self, data: &ArenaContextMut<'_>, inst: Inst) -> bool {
        self.integer_constant(data, inst) == Some(1)
    }

    fn replace_value(&self, data: &mut ArenaContextMut<'_>, inst: Inst, replacement: Inst) {
        // 把 inst 的全部使用点改写为 replacement，之后 inst 不再被任何人使用。
        utils::visit_and_replace(data, inst, replacement);
        // 不变量：改写后 inst 必须已无使用者，才能安全地从布局中删除；
        // 断言把"漏改某处使用点"的 bug 提前暴露在开发期。
        assert!(data.inst_data(inst).used_by().is_empty());
        let bb = data.layout().parent_bb(inst).unwrap();
        data.remove_layout_inst(bb, inst);
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

    fn run(program: &mut Program) {
        BooleanSimplification.run(program);
    }

    fn returned_value(data: &FunctionData, bb: BasicBlock) -> Inst {
        let ret = utils::get_terminator_inst(data, bb);
        let InstKind::Return(ret) = data.inst_data(ret).kind() else {
            panic!("expected return");
        };
        ret.value().unwrap()
    }

    #[test]
    fn removes_compare_truthiness_wrapper() {
        // 规则②：(a<b)!=0 剥成 a<b，返回值指令直接变成内层比较。
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "truthy".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let x = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, x, zero);
        let wrapped = data.new_local_inst().binary(BinaryOp::NotEq, compare, zero);
        let ret = data.new_local_inst().ret(Some(wrapped));
        for inst in [compare, wrapped, ret] {
            data.layout_mut().insert_inst(entry, inst);
        }

        run(&mut program);
        assert_eq!(returned_value(program.func_data(function), entry), compare);
    }

    #[test]
    fn complements_integer_compare_against_zero() {
        // 规则② 的取补路径：(a<b)==0 就地改写为 a>=b。
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "inverse".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let x = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, x, zero);
        let inverse = data.new_local_inst().binary(BinaryOp::Eq, compare, zero);
        let ret = data.new_local_inst().ret(Some(inverse));
        for inst in [compare, inverse, ret] {
            data.layout_mut().insert_inst(entry, inst);
        }

        run(&mut program);
        let data = program.func_data(function);
        let InstKind::Binary(result) = data.inst_data(inverse).kind() else {
            panic!("expected rewritten comparison");
        };
        assert_eq!(result.op(), BinaryOp::Ge);
        assert_eq!((result.lhs(), result.rhs()), (x, zero));
        assert_eq!(returned_value(data, entry), inverse);
    }

    #[test]
    fn retains_float_relational_complement() {
        // 规则② 的保守路径：浮点比较不取补——NaN 下 a==b 与 a!=b 都为 0，取补不等价。
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "float".into(), vec![Type::get_f32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let x = data.params()[0];
        let zero_float = data.new_local_inst().float(0.0);
        let zero_int = data.new_local_inst().integer(0);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, x, zero_float);
        let inverse = data
            .new_local_inst()
            .binary(BinaryOp::Eq, compare, zero_int);
        let ret = data.new_local_inst().ret(Some(inverse));
        for inst in [compare, inverse, ret] {
            data.layout_mut().insert_inst(entry, inst);
        }

        run(&mut program);
        let data = program.func_data(function);
        assert!(matches!(
            data.inst_data(inverse).kind(),
            InstKind::Binary(binary)
                if binary.op() == BinaryOp::Eq && binary.lhs() == compare && binary.rhs() == zero_int
        ));
    }

    #[test]
    fn strips_branch_truthiness_and_swaps_zero_test_edges() {
        // 规则③：br(x==0) 剥掉零测试并交换两臂。
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_unit(), "branch".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let yes = data.new_basic_block().basic_block("yes".into(), vec![]);
        let no = data.new_basic_block().basic_block("no".into(), vec![]);
        for bb in [yes, no] {
            data.layout_mut().push_bb_back(bb);
        }
        let x = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let cond = data.new_local_inst().binary(BinaryOp::Eq, x, zero);
        let branch = data.new_local_inst().branch(cond, yes, vec![], no, vec![]);
        let yes_ret = data.new_local_inst().ret(None);
        let no_ret = data.new_local_inst().ret(None);
        for (bb, inst) in [(entry, cond), (entry, branch), (yes, yes_ret), (no, no_ret)] {
            data.layout_mut().insert_inst(bb, inst);
        }

        run(&mut program);
        let data = program.func_data(function);
        let InstKind::Branch(branch) = data.inst_data(branch).kind() else {
            panic!("expected branch");
        };
        assert_eq!(branch.cond(), x);
        assert_eq!((branch.t_target(), branch.f_target()), (no, yes));
    }

    #[test]
    fn simplifies_select_truthiness_without_substituting_arbitrary_truthy_value() {
        // 规则④ 的闸门：select(x!=0, 1, 0) 只把条件剥成 x，但不把整个 select
        // 替换成 x——x 不是规范布尔，任意真值（如 2）会破坏 0/1 结果。
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "select".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let x = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let one = data.new_local_inst().integer(1);
        let cond = data.new_local_inst().binary(BinaryOp::NotEq, x, zero);
        let select = data.new_local_inst().select(cond, one, zero);
        let ret = data.new_local_inst().ret(Some(select));
        for inst in [cond, select, ret] {
            data.layout_mut().insert_inst(entry, inst);
        }

        run(&mut program);
        let data = program.func_data(function);
        assert!(matches!(
            data.inst_data(returned_value(data, entry)).kind(),
            InstKind::Select(result)
                if result.cond() == x && result.if_true() == one && result.if_false() == zero
        ));
    }

    #[test]
    fn swaps_select_arms_for_zero_test() {
        // 规则④ 的零测试换臂：select(x==0, a, b) → select(x, b, a)。
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "select_zero".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let x = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let if_true = data.new_local_inst().integer(10);
        let if_false = data.new_local_inst().integer(20);
        let cond = data.new_local_inst().binary(BinaryOp::Eq, x, zero);
        let select = data.new_local_inst().select(cond, if_true, if_false);
        let ret = data.new_local_inst().ret(Some(select));
        for inst in [cond, select, ret] {
            data.layout_mut().insert_inst(entry, inst);
        }

        run(&mut program);
        let data = program.func_data(function);
        assert!(matches!(
            data.inst_data(returned_value(data, entry)).kind(),
            InstKind::Select(result)
                if result.cond() == x && result.if_true() == if_false && result.if_false() == if_true
        ));
    }
}
