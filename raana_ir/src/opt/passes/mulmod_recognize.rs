//! # MulmodRecognize：模乘递归识别（M60，AArch64-only）
//!
//! 识别"乘加翻倍"（multiply by doubling）线性递归——NTT 内核 fft0/fft1 里
//! `multiply` 这类用 halving/doubling 计算 `a * b % P` 的自递归函数——并改写为
//! `b < 0` guard 加一次 `soyo_mulmod(a, b, P)` builtin 调用。AArch64 后端把该
//! 调用原地展开成 `smull; sxtw; sdiv; msub` 四条指令（不产生真实调用），递归的
//! 调用 / 分支 / 取模开销全部消失。无独立 CLI 开关，由目标门控
//! `enable_chain_to_switch`（仅 AArch64）决定是否挂载。术语（SSA / block 参数
//! (Phi) / guard / 内联 / builtin）见 `docs/offline-handbook/glossary.md`。
//!
//! ## 变换形态（IR 示例）
//!
//! 改写前的递归函数（SSA 伪码，`P` 是匹配到的常数模数）：
//!
//! ```text
//! f(a, b):
//!   br b, cont, ret0            ; b == 0 → ret 0（前端也可能发出 eq(b, 0)）
//!   ret0:  ret 0
//!   cont:  br eq(b, 1), base, rec
//!   base:  ret a % P
//!   rec:   half = sar(add(b, shr(b, 31)), 1)   ; b/2（向 -∞ 取整）
//!          r    = call f(a, half)
//!          dbl  = (r + r) % P
//!          br eq(and(b, 0x80000001), 1), odd, even
//!          odd:  ret (dbl + a) % P
//!          even: ret dbl
//! ```
//!
//! 改写后（入口块参数 `a`、`b` 保留，新建 `mulmod_zero` / `mulmod_fast` 两个块）：
//!
//! ```text
//! f(a, b):
//!   cond = b < 0
//!   br cond, mulmod_zero, mulmod_fast
//!   mulmod_zero:  ret 0
//!   mulmod_fast:  ret call soyo_mulmod(a, b, P)   // (i64)a*b % P
//! ```
//!
//! 对任意 `b >= 0`，递归与 fast path 同值（模运算对翻倍 `2x` 与 `+a` 步骤可分配）；
//! 对 `b < 0`，halving 链（前端形态 `sar(add(b, shr(b, 31)), 1)`，向 -∞ 取整）在
//! `-1`/`0` 处终止、递归返回 0；且负数的符号位恒为 1，使 `b & 0x80000001` 永不
//! 等于 1，奇偶分支对负输入不进入——guard 路径同样返回 0，对所有输入语义保持。
//!
//! ## 触发条件
//!
//! `recognize` 对每个函数做形状匹配，全部满足才返回模数 `P`（`Modulus = i32`）：
//!
//! - **函数形状**：有入口块、返回 `i32`、恰好 2 个 `i32` 参数 `(a, b)`；
//! - **指令集合**：恰好 1 个自递归 `Call`，不允许其它 Call / TailCall / 携带块
//!   参数的 Branch / Jump / 其它指令种类；全部 Return / Branch / Binary 被收集；
//! - **halving**：自调用第二实参 `half` 必须是 `b / 2` 的三种前端形态之一
//!   （`half_binaries`）：`sar(b, 1)`、`sar(add(b, shr(b, 31)), 1)`（前端实际发出
//!   的形态）或 `div(b, 2)`；
//! - **翻倍与加**：存在 `double_add = rec + rec`、`double = double_add % P`
//!   （`P > 0` 常数，即模数）、`odd_add = double + a`（或 `a + double`）；
//! - **四个 return 恰好各一**：`0` / `a % P` / `double` / `odd_add % P`；
//! - **恰好三个分支**：入口 guard（`b` 自身 / `neq(b, 0)` / `eq(b, 0)`，前端在
//!   强度削减前发出 `eq` 形态；`entry_b_use`）、基例 `eq(b, 1)`（`eq1_inst`）、
//!   奇偶测试（`eq(and(b, mask), 1)`，mask 为 `0x80000001`（-2147483647）或 `1`；
//!   或 `eq(rem(b, 2), 1)`；`parity_binaries`）；
//! - **use 完整性**：`rec` / `double` / `a` / `b` 的每个 use 必须被上述形状解释
//!   （`uses_are_exactly`），所有 Binary 必须属于形状（`explained` 集合）。
//!
//! ## 放弃（拒绝）条件
//!
//! 匹配器刻意严格（宁漏勿错）：上述任一条件不满足——多一条指令、多一个 use、
//! 多一个分支、return 重复或形状不符——整个函数保持原样。此外 `run` 在改写前
//! 检查程序里是否已有**用户定义**的 `soyo_mulmod`（同名且有函数体），有则整体
//! 放弃本 pass，绝不把用户函数的名字让给 builtin。
//!
//! ## 正确性
//!
//! - 代数恒等：`a * b % P` 对 halving/doubling 递归成立（模运算分配律），
//!   fast path 的 `(i64)a*b % P` 与之同值；
//! - 负参数：`b < 0` 时递归返回 0，guard 路径也返回 0（测试
//!   `guard_path_matches_negative_b_semantics` 对 `-1`、`-2`、`i32::MIN` 等验证）；
//! - 后端展开：`smull` 对两个 32 位操作数给出**精确**的 64 位乘积（不会溢出），
//!   64 位 `sdiv`/`msub` 算出截断余数，落在 `(-P, P)` 内，低 32 位即正确的 `i32`
//!   返回值（`anon_armv8/src/lower/call.rs` 的 `lower_mulmod_builtin`）；
//! - 改写要点：入口块指令先全部摘除、其余基本块全部删除，入口块参数（`a`、`b`）
//!   保留；guard 的 `cond`（Binary）必须 `insert_inst` 进入口块 layout——否则
//!   IPSCCP 从块 layout 建 worklist，孤儿值"从未被访问"，分支两个后继都不被跟随，
//!   会静默丢掉调用方的循环与回边（无限循环误编译；回归测试
//!   `guard_condition_lives_in_the_entry_layout`）。`zero` 是 Integer 内联操作数，
//!   无需 layout；
//! - 下游约定：`soyo_mulmod` 是无体声明，`pure_function.rs` 视其为纯函数，range
//!   分析在 `a`、`b` 均非负时给出 `[0, P)`（见
//!   `analysis_passes/return_summary.rs`）。
//!
//! ## 管线位置与门控
//!
//! - 注册：`opt/pass.rs` 的 `PassesManager::from_config`，**initial 段**
//!   （`register_initial`，跑在优化 fixpoint 之前且不参与迭代），`specialize` 之后、
//!   `inline` 之前：`specialize` 跳过自递归候选（如 `multiply`），必须先跑；改写后
//!   的函数只剩 guard + 一次调用，`inline` 随后把它内联进调用方。顺序：SSA →
//!   Specialize → `mulmod_recognize` → Inline → …；
//! - 门控：`config.target.enable_chain_to_switch`（AArch64-only）——RISC-V 保留原
//!   递归；`soyo_mulmod` 声明只在 AArch64 运行里出现。
//!
//! ## 验证
//!
//! - 本文件 `mod tests`（614 行起）覆盖：形状识别
//!   `recognizes_the_doubling_recursion`、改写 + helper 声明 + 幂等
//!   `rewrites_to_guard_plus_builtin_and_declares_helper`、M60/Inline 交互回归
//!   `guard_condition_lives_in_the_entry_layout`、多余指令拒绝
//!   `refuses_a_function_with_a_stray_operation`，以及语义等价
//!   `guard_path_matches_negative_b_semantics` / `fast_path_matches_recursion_for_positive_b`
//!   （对照参考实现 `recursion_value`）；
//! - 端到端：`make test ARGS="-O 2"` 差分比对（M60 落地时 fft0 QEMU
//!   14.93s → ~0.5s，见 TODO.md）。
//!
use crate::{
    ir::{
        BinaryOp, Function, Inst, InstKind, Program, Type, arena::Arena, basic_block::BasicBlock,
    },
    opt::{pass::Pass, prelude::*},
};

/// Compiler-provided modular-multiplication builtin. The AArch64 backend
/// recognizes calls to this declared function and expands them inline to
/// `smull; sxtw; sdiv; msub` (one exact 64-bit product plus one division), so
/// the symbol is never defined in the emitted assembly. The declaration only
/// appears on AArch64 runs where this pass rewrites a matched function.
pub const MULMOD_HELPER: &str = "soyo_mulmod";

/// Recognizes the "multiply by doubling" linear recursion
///
/// ```c
/// int f(int a, int b) {
///     if (b == 0) return 0;
///     if (b == 1) return a % P;
///     int cur = f(a, b / 2);
///     cur = (cur + cur) % P;
///     if (b % 2 == 1) return (cur + a) % P;
///     return cur;
/// }
/// ```
///
/// which computes `a * b % P` for every `b >= 0` (modular arithmetic
/// distributes over the doubling `2x` and the `+ a` step). For `b < 0` the
/// halving chain terminates at `-1`/`0` and the "odd" test is never true, so
/// the function returns 0. The rewrite keeps the exact semantics for every
/// input:
///
/// ```c
/// int f(int a, int b) {
///     if (b < 0) return 0;
///     return soyo_mulmod(a, b, P);   // (i64)a*b % P
/// }
/// ```
///
/// `smull xd, wn, wm` computes the exact 64-bit product of two 32-bit
/// operands (never overflowing), and a 64-bit `sdiv`/`msub` pair computes the
/// truncating remainder — the same value the recursion produces for `b >= 0`.
///
/// The matcher is deliberately strict (宁漏勿错): every instruction, return
/// value and branch condition must fit the shape above, every use of the key
/// SSA values must be explained, and every binary instruction must be one of
/// the shape's operators, otherwise the function is left untouched.
pub struct MulmodRecognize;

impl Pass for MulmodRecognize {
    fn run(&mut self, program: &mut Program) -> bool {
        let candidates: Vec<Function> = program
            .function_layout()
            .iter()
            .copied()
            .filter(|&f| recognize(program, f).is_some())
            .collect();
        if candidates.is_empty() {
            return false;
        }
        // Never call a user-defined `soyo_mulmod`: skip the optimization if a
        // source function already owns the builtin name.
        if program.function_layout().iter().any(|&f| {
            program.func_data(f).name() == MULMOD_HELPER
                && program.func_data(f).layout().entry_bb().is_some()
        }) {
            return false;
        }
        let helper = find_or_declare_helper(program);
        let mut changed = false;
        for f in candidates {
            if let Some(modulus) = recognize(program, f) {
                rewrite(program, f, helper, modulus);
                changed = true;
            }
        }
        changed
    }
}

fn find_or_declare_helper(program: &mut Program) -> Function {
    for &f in program.function_layout() {
        if program.func_data(f).name() == MULMOD_HELPER {
            return f;
        }
    }
    program.new_function(
        Type::get_i32(),
        MULMOD_HELPER.into(),
        vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
    )
}

fn const_i32(data: &FunctionData, inst: Inst) -> Option<i32> {
    match data.inst_data(inst).kind() {
        InstKind::Integer(int) => Some(int.value()),
        _ => None,
    }
}

/// The `(op, lhs, rhs)` of a binary instruction.
fn binary_of(data: &FunctionData, inst: Inst) -> Option<(BinaryOp, Inst, Inst)> {
    match data.inst_data(inst).kind() {
        InstKind::Binary(bin) => Some((bin.op(), bin.lhs(), bin.rhs())),
        _ => None,
    }
}

/// Collect the binary instructions that compute `b / 2` in one of the forms
/// the frontend emits: an arithmetic shift `sar(b, 1)`, the chain
/// `sar(add(b, shr(b, 31)), 1)`, or a plain division `div(b, 2)`. Returns the
/// instruction that reads `b` and every involved binary (so the matcher can
/// account for all of them).
fn half_binaries(data: &FunctionData, b: Inst, half: Inst) -> Option<(Vec<Inst>, Vec<Inst>)> {
    if let Some((BinaryOp::Sar, x, k)) = binary_of(data, half) {
        if is_same_inst(x, b) && const_i32(data, k) == Some(1) {
            return Some((vec![half], vec![half]));
        }
        // half == sar(add(b, shr(b, 31)), 1): `b` is read by both the `shr`
        // and the `add`, so both must be accounted for as users of `b`.
        if let Some((BinaryOp::Add, l, r)) = binary_of(data, x) {
            if is_same_inst(l, b) {
                if let Some((BinaryOp::Shr, s, k31)) = binary_of(data, r) {
                    if is_same_inst(s, b) && const_i32(data, k31) == Some(31) {
                        return Some((vec![half, x, r], vec![x, r]));
                    }
                }
            }
        }
        return None;
    }
    if let Some((BinaryOp::Div, x, k)) = binary_of(data, half) {
        if is_same_inst(x, b) && const_i32(data, k) == Some(2) {
            return Some((vec![half], vec![half]));
        }
    }
    None
}

/// True when `cond` tests the low bit of `b` (the `b % 2 == 1` of the shape).
/// Accepts both `eq(and(b, 0x80000001), 1)` (the frontend's lowering) and
/// `eq(rem(b, 2), 1)`. Returns the binary instruction that reads `b` (the
/// `and`/`rem`) together with its `eq` so the matcher can account for both.
fn parity_binaries(data: &FunctionData, b: Inst, cond: Inst) -> Option<(Inst, Inst)> {
    let Some((BinaryOp::Eq, l, r)) = binary_of(data, cond) else {
        return None;
    };
    if const_i32(data, r) != Some(1) {
        return None;
    }
    match binary_of(data, l) {
        Some((BinaryOp::And, x, m))
            if is_same_inst(x, b)
                && matches!(const_i32(data, m), Some(mask) if mask == -2147483647 || mask == 1) =>
        {
            Some((l, cond))
        }
        Some((BinaryOp::Rem, x, k)) if is_same_inst(x, b) && const_i32(data, k) == Some(2) => {
            Some((l, cond))
        }
        _ => None,
    }
}

/// True when `cond` is the `b == 0` guard of the shape (`b` itself,
/// `neq(b, 0)` or `eq(b, 0)` — the frontend emits the `eq` form before
/// strength reduction). Returns the instruction that reads `b` (`None` means
/// the branch condition is `b` itself).
fn entry_b_use(data: &FunctionData, b: Inst, cond: Inst) -> Option<Option<Inst>> {
    if is_same_inst(cond, b) {
        return Some(None);
    }
    if let Some((BinaryOp::NotEq, l, r)) = binary_of(data, cond) {
        if is_same_inst(l, b) && const_i32(data, r) == Some(0) {
            return Some(Some(cond));
        }
    }
    if let Some((BinaryOp::Eq, l, r)) = binary_of(data, cond) {
        if is_same_inst(l, b) && const_i32(data, r) == Some(0) {
            return Some(Some(cond));
        }
    }
    None
}

/// True when `cond` is the `b == 1` base-case guard. Returns the `eq`
/// instruction (which reads `b`).
fn eq1_inst(data: &FunctionData, b: Inst, cond: Inst) -> Option<Inst> {
    if let Some((BinaryOp::Eq, l, r)) = binary_of(data, cond) {
        if is_same_inst(l, b) && const_i32(data, r) == Some(1) {
            return Some(cond);
        }
    }
    None
}

fn is_same_inst(a: Inst, b: Inst) -> bool {
    a == b
}

/// Recognized mulmod pattern: the constant modulus `P`.
type Modulus = i32;

fn recognize(program: &Program, f: Function) -> Option<Modulus> {
    let data = program.func_data(f);
    if data.layout().entry_bb().is_none() || !data.ret_ty().is_i32() {
        return None;
    }
    let params = data.params().to_vec();
    if params.len() != 2 {
        return None;
    }
    let (a, b) = (params[0], params[1]);
    if !data.inst_data(a).ty().is_i32() || !data.inst_data(b).ty().is_i32() {
        return None;
    }

    let mut self_call = None;
    let mut returns = Vec::new();
    let mut branches = Vec::new();
    let mut binaries: HashSet<Inst> = HashSet::default();

    for layout in data.layout().basicblocks() {
        for &inst in layout.insts() {
            match data.inst_data(inst).kind() {
                InstKind::Call(call) => {
                    if call.callee() == f {
                        self_call = Some(inst);
                    } else {
                        return None;
                    }
                }
                InstKind::TailCall(..) => return None,
                InstKind::Return(ret) => returns.push((inst, ret.value())),
                InstKind::Branch(br) => {
                    if !br.t_args().is_empty() || !br.f_args().is_empty() {
                        return None;
                    }
                    branches.push((inst, br.cond()));
                }
                InstKind::Jump(jump) => {
                    if !jump.args().is_empty() {
                        return None;
                    }
                }
                InstKind::Binary(bin) => {
                    binaries.insert(inst);
                    let _ = bin;
                }
                InstKind::Integer(..) | InstKind::BlockArgRef(..) => {}
                _ => return None,
            }
        }
    }

    let self_call = self_call?;
    let call_args = match data.inst_data(self_call).kind() {
        InstKind::Call(call) => call.args().to_vec(),
        _ => unreachable!(),
    };
    if call_args.len() != 2 || !is_same_inst(call_args[0], a) {
        return None;
    }
    let half = call_args[1];
    let (half_bins, half_b_users) = half_binaries(data, b, half)?;
    let rec = self_call;

    // double_add = add(rec, rec);  double = rem(double_add, P).
    let mut double_add = None;
    for &inst in &binaries {
        if let Some((BinaryOp::Add, l, r)) = binary_of(data, inst) {
            if is_same_inst(l, rec) && is_same_inst(r, rec) {
                double_add = Some(inst);
            }
        }
    }
    let double_add = double_add?;
    let mut double = None;
    for &inst in &binaries {
        if let Some((BinaryOp::Rem, l, r)) = binary_of(data, inst) {
            if is_same_inst(l, double_add) {
                if let Some(p) = const_i32(data, r) {
                    if p > 0 {
                        double = Some((inst, p));
                    }
                }
            }
        }
    }
    let (double, modulus) = double?;

    // odd_add = add(double, a).
    let mut odd_add = None;
    for &inst in &binaries {
        if let Some((BinaryOp::Add, l, r)) = binary_of(data, inst) {
            if (is_same_inst(l, double) && is_same_inst(r, a))
                || (is_same_inst(l, a) && is_same_inst(r, double))
            {
                odd_add = Some(inst);
            }
        }
    }
    let odd_add = odd_add?;

    // Classify the four returns: ret 0, ret a%P, ret double, ret (double+a)%P.
    let mut ret0_inst = None;
    let mut ret1_rem = None;
    let mut ret1_inst = None;
    let mut ret_even_inst = None;
    let mut ret_odd_rem = None;
    let mut ret_odd_inst = None;
    for (ret_inst, value_opt) in &returns {
        let value = (*value_opt)?;
        if const_i32(data, value) == Some(0) {
            if ret0_inst.is_some() {
                return None;
            }
            ret0_inst = Some(*ret_inst);
        } else if is_same_inst(value, double) {
            if ret_even_inst.is_some() {
                return None;
            }
            ret_even_inst = Some(*ret_inst);
        } else if let Some((BinaryOp::Rem, l, r)) = binary_of(data, value) {
            if is_same_inst(l, a) && const_i32(data, r) == Some(modulus) {
                if ret1_inst.is_some() {
                    return None;
                }
                ret1_rem = Some(value);
                ret1_inst = Some(*ret_inst);
            } else if is_same_inst(l, odd_add) && const_i32(data, r) == Some(modulus) {
                if ret_odd_inst.is_some() {
                    return None;
                }
                ret_odd_rem = Some(value);
                ret_odd_inst = Some(*ret_inst);
            } else {
                return None;
            }
        } else {
            return None;
        }
    }
    let (_ret0, _ret1, ret_even_inst, _ret_odd) =
        (ret0_inst?, ret1_inst?, ret_even_inst?, ret_odd_inst?);
    let (ret1_rem, ret_odd_rem) = (ret1_rem?, ret_odd_rem?);

    // The three branch conditions must be exactly: b/neq(b,0), eq(b,1), parity(b).
    if branches.len() != 3 {
        return None;
    }
    let mut entry_branch = None;
    let mut entry_b_use_inst = None;
    let mut eq1 = None;
    let mut parity_branch = None;
    for (branch_inst, cond) in &branches {
        if let Some(entry_use) = entry_b_use(data, b, *cond) {
            if entry_branch.is_some() {
                return None;
            }
            entry_branch = Some(*branch_inst);
            entry_b_use_inst = entry_use;
        } else if let Some(eq) = eq1_inst(data, b, *cond) {
            if eq1.is_some() {
                return None;
            }
            eq1 = Some(eq);
        } else if let Some(parity_bins) = parity_binaries(data, b, *cond) {
            if parity_branch.is_some() {
                return None;
            }
            parity_branch = Some(parity_bins);
        } else {
            return None;
        }
    }
    let (entry_branch, eq1, parity_branch) = (entry_branch?, eq1?, parity_branch?);
    let (parity_use, parity_eq) = parity_branch;

    // Every use of the key SSA values must be explained by the shape above.
    if !uses_are_exactly(data, rec, &[double_add]) {
        return None;
    }
    let double_users = [ret_even_inst, odd_add];
    if !uses_are_exactly(data, double, &double_users) {
        return None;
    }
    let a_users = [self_call, ret1_rem, odd_add];
    if !uses_are_exactly(data, a, &a_users) {
        return None;
    }
    let mut b_users = half_b_users;
    if let Some(neq0) = entry_b_use_inst {
        b_users.push(neq0);
    } else {
        b_users.push(entry_branch);
    }
    b_users.push(eq1);
    b_users.push(parity_use);
    if !uses_are_exactly(data, b, &b_users) {
        return None;
    }

    // Every binary instruction must belong to the shape.
    let mut explained = HashSet::default();
    for inst in half_bins {
        explained.insert(inst);
    }
    explained.insert(double_add);
    explained.insert(double);
    explained.insert(odd_add);
    explained.insert(ret1_rem);
    explained.insert(ret_odd_rem);
    explained.insert(parity_use);
    explained.insert(parity_eq);
    explained.insert(eq1);
    if let Some(neq0) = entry_b_use_inst {
        explained.insert(neq0);
    }
    if !binaries.iter().all(|inst| explained.contains(inst)) {
        return None;
    }

    Some(modulus)
}

fn uses_are_exactly(data: &FunctionData, value: Inst, expected: &[Inst]) -> bool {
    let uses = data.inst_data(value).used_by();
    if uses.len() != expected.len() {
        return false;
    }
    expected.iter().all(|&u| uses.contains(&u))
}

/// Replace the whole body of `f` with the guard plus the `soyo_mulmod` call.
fn rewrite(program: &mut Program, f: Function, helper: Function, modulus: i32) {
    let data = program.func_data_mut(f);
    let entry = data.layout().entry_bb().unwrap().bb();

    // Detach the entry's instructions first (their branches point at the
    // blocks about to be removed), then drop every other block. The entry's
    // block parameters (the function's `a`, `b`) are kept.
    let entry_insts: Vec<Inst> = data
        .layout()
        .basicblock(entry)
        .insts()
        .iter()
        .copied()
        .collect();
    for inst in entry_insts {
        data.remove_layout_inst(entry, inst);
    }
    let others: Vec<BasicBlock> = data
        .layout()
        .basicblocks()
        .iter()
        .map(|l| l.bb())
        .filter(|&bb| bb != entry)
        .collect();
    for bb in others {
        data.remove_layout_basicblock(bb);
    }

    let a = data.params()[0];
    let b = data.params()[1];

    let zero = data.new_local_inst().integer(0);
    let cond = data.new_local_inst().binary(BinaryOp::Lt, b, zero);
    let then_zero = data
        .new_basic_block()
        .basic_block("mulmod_zero".into(), vec![]);
    let then_fast = data
        .new_basic_block()
        .basic_block("mulmod_fast".into(), vec![]);
    data.layout_mut().push_bb_back(then_zero);
    data.layout_mut().push_bb_back(then_fast);

    let branch = data
        .new_local_inst()
        .branch(cond, then_zero, vec![], then_fast, vec![]);
    // The guard's `cond` (a Binary) must live in the entry's layout, not as an
    // orphan value: passes that build a worklist from the block layout (IPSCCP)
    // only visit values that sit inside a block, and an orphaned branch
    // condition stays "unvisited" so the pass follows neither successor —
    // silently dropping the rest of the caller's loop. The `zero` Integer
    // stays an inline operand (Integer insts are never laid out).
    data.layout_mut().insert_inst(entry, cond);
    data.layout_mut().insert_inst(entry, branch);

    let ret_zero = data.new_local_inst().ret(Some(zero));
    data.layout_mut().insert_inst(then_zero, ret_zero);

    let p = data.new_local_inst().integer(modulus);
    let call = data
        .new_local_inst()
        .call_with_type(helper, vec![a, b, p], Type::get_i32());
    let ret_call = data.new_local_inst().ret(Some(call));
    data.layout_mut().insert_inst(then_fast, call);
    data.layout_mut().insert_inst(then_fast, ret_call);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opt::pass::Pass;

    const MOD: i32 = 998244353;

    /// Build the exact post-optimization IR shape of the doubling recursion:
    /// `if b==0 ret 0; if b==1 ret a%P; cur = f(a, b/2); cur = (cur+cur)%P;
    /// if (b & 0x80000001) == 1 ret (cur+a)%P else ret cur`.
    fn build_multiply(program: &mut Program, name: &str) -> Function {
        let f = program.new_function(
            Type::get_i32(),
            name.into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        {
            let data = program.func_data_mut(f);
            let entry = data.add_entry_block();
            let then_1 = data.new_basic_block().basic_block("then_1".into(), vec![]);
            let end_2 = data.new_basic_block().basic_block("end_2".into(), vec![]);
            let then_3 = data.new_basic_block().basic_block("then_3".into(), vec![]);
            let end_4 = data.new_basic_block().basic_block("end_4".into(), vec![]);
            let then_5 = data.new_basic_block().basic_block("then_5".into(), vec![]);
            let else_6 = data.new_basic_block().basic_block("else_6".into(), vec![]);
            for bb in [then_1, end_2, then_3, end_4, then_5, else_6] {
                data.layout_mut().push_bb_back(bb);
            }
            let a = data.params()[0];
            let b = data.params()[1];

            let zero = data.new_local_inst().integer(0);
            let one = data.new_local_inst().integer(1);
            let mod_const = data.new_local_inst().integer(MOD);

            let entry_br = data
                .new_local_inst()
                .branch(b, end_2, vec![], then_1, vec![]);
            data.layout_mut().insert_inst(entry, entry_br);
            let ret_0 = data.new_local_inst().ret(Some(zero));
            data.layout_mut().insert_inst(then_1, ret_0);

            let eq1 = data.new_local_inst().binary(BinaryOp::Eq, b, one);
            let br2 = data
                .new_local_inst()
                .branch(eq1, then_3, vec![], end_4, vec![]);
            data.layout_mut().insert_inst(end_2, br2);

            let rem_a = data.new_local_inst().binary(BinaryOp::Rem, a, mod_const);
            let ret_1 = data.new_local_inst().ret(Some(rem_a));
            data.layout_mut().insert_inst(then_3, ret_1);

            let thirty_one = data.new_local_inst().integer(31);
            let shr = data.new_local_inst().binary(BinaryOp::Shr, b, thirty_one);
            let add_b = data.new_local_inst().binary(BinaryOp::Add, b, shr);
            let sar = data.new_local_inst().binary(BinaryOp::Sar, add_b, one);
            let call = data
                .new_local_inst()
                .call_with_type(f, vec![a, sar], Type::get_i32());
            let rec_add = data.new_local_inst().binary(BinaryOp::Add, call, call);
            let double = data
                .new_local_inst()
                .binary(BinaryOp::Rem, rec_add, mod_const);
            let mask = data.new_local_inst().integer(-2147483647);
            let and = data.new_local_inst().binary(BinaryOp::And, b, mask);
            let eq_par = data.new_local_inst().binary(BinaryOp::Eq, and, one);
            let br3 = data
                .new_local_inst()
                .branch(eq_par, then_5, vec![], else_6, vec![]);
            for inst in [shr, add_b, sar, call, rec_add, double, and, eq_par, br3] {
                data.layout_mut().insert_inst(end_4, inst);
            }

            let odd_add = data.new_local_inst().binary(BinaryOp::Add, double, a);
            let odd_rem = data
                .new_local_inst()
                .binary(BinaryOp::Rem, odd_add, mod_const);
            let ret_odd = data.new_local_inst().ret(Some(odd_rem));
            data.layout_mut().insert_inst(then_5, odd_add);
            data.layout_mut().insert_inst(then_5, odd_rem);
            data.layout_mut().insert_inst(then_5, ret_odd);

            let ret_even = data.new_local_inst().ret(Some(double));
            data.layout_mut().insert_inst(else_6, ret_even);
        }
        f
    }

    #[test]
    fn recognizes_the_doubling_recursion() {
        let mut program = Program::new();
        build_multiply(&mut program, "multiply");
        let f = program.function_layout()[0];
        assert_eq!(recognize(&program, f), Some(MOD));
    }

    #[test]
    fn rewrites_to_guard_plus_builtin_and_declares_helper() {
        let mut program = Program::new();
        let f = build_multiply(&mut program, "multiply");
        assert!(MulmodRecognize.run(&mut program));
        assert!(!MulmodRecognize.run(&mut program));

        let data = program.func_data(f);
        let insts: Vec<Inst> = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|l| l.insts().iter().copied())
            .collect();
        // The self-recursion is gone.
        assert!(insts.iter().all(|&inst| {
            !matches!(data.inst_data(inst).kind(), InstKind::Call(c) if c.callee() == f)
        }));
        // A call to the builtin helper remains.
        assert!(insts.iter().any(|&inst| {
            matches!(data.inst_data(inst).kind(), InstKind::Call(c)
                if program.func_data(c.callee()).name() == MULMOD_HELPER)
        }));
        // The helper is declared in the program.
        assert!(
            program
                .function_layout()
                .iter()
                .any(|&g| { program.func_data(g).name() == MULMOD_HELPER })
        );
    }

    #[test]
    fn guard_condition_lives_in_the_entry_layout() {
        // Regression for the M60/Inline interaction bug: the rewritten guard's
        // `cond` must be inserted into the entry block's layout, not left as an
        // orphan value. IPSCCP only visits values that sit in a block layout;
        // an orphaned branch condition stays "unvisited", so IPSCCP follows
        // neither successor of the branch and silently drops the caller's loop
        // body and back-edge (an infinite-loop miscompile).
        let mut program = Program::new();
        let f = build_multiply(&mut program, "multiply");
        assert!(MulmodRecognize.run(&mut program));

        let data = program.func_data(f);
        let entry = data.layout().entry_bb().unwrap().bb();
        let entry_insts: Vec<Inst> = data
            .layout()
            .basicblock(entry)
            .insts()
            .iter()
            .copied()
            .collect();
        let terminator = data.layout().basicblock(entry).terminator();
        let cond = match data.inst_data(terminator).kind() {
            InstKind::Branch(branch) => branch.cond(),
            other => panic!("entry must end in a branch, got {other:?}"),
        };
        assert!(
            entry_insts.contains(&cond),
            "the guard condition must be laid out in the entry block, not orphaned"
        );
    }

    #[test]
    fn refuses_a_function_with_a_stray_operation() {
        let mut program = Program::new();
        let f = build_multiply(&mut program, "multiply");
        {
            let data = program.func_data_mut(f);
            let end_4 = data
                .layout()
                .basicblocks()
                .iter()
                .map(|l| l.bb())
                .nth(4)
                .expect("expected an end_4 block");
            let one = data.new_local_inst().integer(1);
            let b = data.params()[1];
            let stray = data.new_local_inst().binary(BinaryOp::Add, b, one);
            data.layout_mut().insert_inst(end_4, stray);
        }
        // An unexplained binary forces a conservative refusal.
        assert_eq!(recognize(&program, f), None);
    }

    #[test]
    fn guard_path_matches_negative_b_semantics() {
        // The recursion returns 0 for every negative b; the guard must too.
        for b in [-1, -2, -3, i32::MIN, -2147483647, -100] {
            let rec = recursion_value(3, b);
            assert_eq!(rec, 0, "recursion({b}) should be 0, got {rec}");
        }
    }

    /// Reference implementation of the doubling recursion (matches the IR
    /// shape: `shr;add;sar` halving, `and(b, 0x80000001)==1` odd test).
    fn recursion_value(a: i32, b: i32) -> i32 {
        if b == 0 {
            return 0;
        }
        if b == 1 {
            return a % MOD;
        }
        let half = (b.wrapping_add((b as u32 >> 31) as i32)) >> 1;
        let cur = recursion_value(a, half);
        let cur = (cur.wrapping_add(cur)) % MOD;
        if b & -2147483647 == 1 {
            return (cur.wrapping_add(a)) % MOD;
        }
        cur
    }

    #[test]
    fn fast_path_matches_recursion_for_positive_b() {
        for a in [0, 1, 3, 998244353, 1000000007, -5, -998244353] {
            for b in [0, 1, 2, 3, 17, 1024, 998244353, 2147483647] {
                let rec = recursion_value(a, b);
                let fast = (i64::from(a) * i64::from(b)) % i64::from(MOD);
                let fast = fast as i32;
                assert_eq!(rec, fast, "a={a} b={b}: recursion {rec} vs fast {fast}");
            }
        }
    }
}
