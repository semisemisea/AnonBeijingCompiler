//! Loop rotation for test-at-bottom countdown loops.
//!
//! `while (len) { body; len = len - 1; }` lowers to a header test at the top
//! of the loop:
//!
//! ```text
//! entry:      jump header(v0)
//! header(v):  br v, body, exit
//! body:       ...; v' = f(v); jump header(v')
//! exit:       ret
//! ```
//!
//! When every non-back edge into `header` passes a provably nonzero value
//! for the tested parameter, the first-iteration test can never fail, so the
//! test moves to the bottom of the loop, merged into the latch (do-while
//! form):
//!
//! ```text
//! entry:      jump header(v0)
//! header(v):  jump body
//! body:       ...; v' = f(v); br v', header(v'), exit
//! exit:       ret
//! ```
//!
//! The backend can then fuse the count-down arithmetic with the loop test
//! (`subs w8, w8, #1; b.ne header` instead of `sub; b header; cmp; b.ne`),
//! eliminating the standalone compare from the loop.
//!
//! Correctness: the head test only gates the *first* iteration; every other
//! iteration is gated by the same test on the same value at the bottom.
//! Removing the head test is sound exactly when all non-back edges pass a
//! nonzero value, which the constant check proves.
//!
//! SysY `while (i < n) { body; i += 1 }` loops are count-up and test a
//! comparison (`lt i, bound`) rather than a counter directly. They are
//! rotated to countdown form by introducing a trip counter `t = bound - i0`
//! carried in a new header parameter, guarded by a pre-header test `t > 0`
//! that preserves the trip-zero semantics of the original head test.
//!
//! ---
//!
//! ## 补充说明（中文）
//!
//! 术语：latch / preheader / backedge / test-at-top 等见
//! `docs/offline-handbook/glossary.md` 的"循环"分组。
//!
//! ### 两种旋转
//!
//! 1. **countdown 旋转**（`rotate_countdown`，上文示例）：测试值直接是
//!    header 参数、且循环的每条非回边前驱都传**非零常量**的 countdown 循环
//!    → 测试移到底部。除上文示例外还有三个拒绝条件：
//!    - body / exit 块带参数（旋转要求它们无参数）；
//!    - 非回边前驱超过一类：全部传非零常量（首测必过）或唯一回边，否则
//!      首测可能失败，不能安全移除头部测试；
//!    - `exit` 使用了被测计数器**以外**的 header 参数：旋转后 false 边从
//!      latch 直跳 exit、绕过 header，exit 里看到的会是上一轮迭代的值。
//! 2. **count-up 旋转**（`rotate_count_up`）：`while (i < bound) { body;
//!    i += 1 }` 形态 → 引入 trip 计数器 `t = bound - i0` 作为新 header
//!    参数；pre-header 里插入 guard `t0 > 0`（保持 trip = 0 时零次执行的
//!    语义，guard 失败直跳 exit）；latch 里计算 `t' = t - 1` 并在底部测试
//!    `t'`。要求：
//!    - 比较必须是 `i < bound`（`BinaryOp::Lt`），`i` 是 header 参数，
//!      `bound` 在循环前可用（全局 / 常量 / 外部块参数，且不是 header
//!      参数——否则 trip 计数无法在 pre-header 计算）；
//!    - 恰好一条 entry 边（带初值）+ 一条 back 边（带的 `i` 参数是
//!      `i + 1` 更新），均为 `jump`；
//!    - `exit` 及其后续可达区域（绕过循环的路径）不使用循环内产生的值：
//!      guard 的 false 边从 pre-header 直入 exit，若该区域用了循环值，
//!      其支配定义会丢失（SSA 合法性）。`exit` 本身读 header 参数是允许
//!      的，会为其加 block 参数并重映射（要求 header 终结符是 exit 的
//!      唯一前驱，保证所有入边同步传参）。
//!
//! ### 收益
//!
//! 旋转后 latch 形态是 `subs w8, w8, #1; b.ne header`——减法和测试融合成
//! 一条指令，省掉独立的 compare。count-up 转 countdown 正是为了让 AArch64
//! 后端能做这个融合（count-up 的 `i < bound` 比较无法与 `i += 1` 融合）。
//!
//! ### 管线位置
//!
//! - 注册：`opt/pass.rs` 的 `from_config`，fixpoint 段，**`loop_unroll`
//!   之后**、`zero_store_loop` 之前；
//! - 与 `loop_unroll` 先后配合：`loop_unroll` 只吃 test-at-top 的精确
//!   小循环，必须先跑；旋转把剩余循环转成 test-at-bottom 供后端融合；
//!   `zero_store_loop` 识别零初始化循环需要 countdown 形态（旋转后的）。
//! - 无目标门控、无 config 开关。
//!
//! ### 验证
//!
//! - 本文件 `mod tests`（500 行起）覆盖两种旋转的命中与各拒绝形态；
//! - 端到端：`make test` 差分比对 + 汇编检查（countdown 融合形态）。

use crate::opt::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

pub struct RotateLoops;

impl Pass for RotateLoops {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().entry_bb().is_none() {
            return false;
        }
        let mut changed = false;
        let headers: Vec<BasicBlock> = data
            .layout()
            .basicblocks()
            .iter()
            .map(|layout| layout.bb())
            .collect();
        let entry = data.layout().entry_bb().unwrap().bb();
        // 主循环：对每个非 entry 块依次尝试两种旋转——先试 countdown（被测值
        // 是 header 参数），失败再试 count-up（`i < bound` 引入 trip 计数器）。
        // 旋转会改写 header 与 latch 的终结符，故块列表在遍历前快照，不被
        // 本轮改写干扰。
        for header in headers {
            if header == entry {
                continue;
            }
            if Self::rotate_countdown(data, header) || Self::rotate_count_up(data, header) {
                changed = true;
            }
        }
        changed
    }
}

impl RotateLoops {
    /// Try to rotate the loop whose header is `header`. Returns true when the
    /// test moved to the bottom of the loop.
    ///
    /// 中文：countdown 旋转。输入 test-at-top 倒计数循环（header 终结符
    /// `br v, body, exit`，`v` 是 header 参数），输出 test-at-bottom 形态。
    /// 四步：① 确认头部测试与无参数形态；② 确认每条非回边传非零常量
    /// （首测必过）；③ 检查 exit 不越权引用 header 参数；④ 测试改写进 latch、
    /// header 直通 body。
    fn rotate_countdown(data: &mut ArenaContextMut<'_>, header: BasicBlock) -> bool {
        // 形态一：header 终结符必须是分支 `br v, body, exit`，且两路都不带
        // 参数、body/exit 无块参数。旋转后 header 直通 body、latch 的 false
        // 边直跳 exit，任何块参数都会错位，故这类形态一律拒绝。
        let (cond, body, exit) = {
            let terminator = data.layout().basicblock(header).terminator();
            let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
                return false;
            };
            let body = branch.t_target();
            let exit = branch.f_target();
            if body == exit
                || !branch.t_args().is_empty()
                || !branch.f_args().is_empty()
                || !data.bb_data(body).params().is_empty()
                || !data.bb_data(exit).params().is_empty()
            {
                return false;
            }
            (branch.cond(), body, exit)
        };

        // The tested value must be one of the header's parameters directly.
        // 被测值必须是 header 参数本身而非中间结果：IV 更新流经块参数——回边
        // 把新值作为参数传回 header，只有参数才能保证底部测试的值与头部测试
        // 严格对齐。
        let params = data.bb_data(header).params().to_vec();
        let Some(tested_index) = params.iter().position(|&p| p == cond) else {
            return false;
        };

        // Partition the header's predecessors into constant (nonzero) edges,
        // which cannot fail the first-iteration test, and the single back
        // edge, whose arrival must be retested at the bottom of the loop.
        // 把 header 的前驱按被测参数分类：传非零常量的非回边（首测必过，可
        // 安全移除头部测试）与唯一回边（到达时需在底部重测）。传零常量、
        // 函数参数或中间结果的前驱会使首测可能失败，旋转不安全。
        let pred_insts: Vec<Inst> = data.bb_data(header).used_by().iter().copied().collect();
        let mut const_preds = Vec::new();
        let mut back_edge: Option<Inst> = None;
        for &pred_inst in &pred_insts {
            let InstKind::Jump(jump) = data.inst_data(pred_inst).kind() else {
                return false;
            };
            let args = jump.args().to_vec();
            if args.len() != params.len() {
                return false;
            }
            let tested_arg = args[tested_index];
            let is_nonzero_const = matches!(data.inst_data(tested_arg).kind(),
                InstKind::Integer(i) if i.value() != 0);
            if is_nonzero_const {
                const_preds.push(pred_inst);
            } else if back_edge.is_none() {
                back_edge = Some(pred_inst);
            } else {
                return false;
            }
        }
        // A loop needs at least one guaranteed-executed entry and exactly one
        // back edge to retest.
        // 至少一条保证执行的非回边 entry + 恰好一条回边，缺一不可；回边多于
        // 一条时底部重测无法唯一对应，同样拒绝。
        if const_preds.is_empty() || back_edge.is_none() {
            return false;
        }
        let back_edge = back_edge.unwrap();

        // After rotation the body's `br v', header(args), exit` skips the
        // header on the false edge, so any reference to a header parameter
        // inside `exit` would see the *previous* iteration's value instead of
        // the freshly computed one. The tested counter is the exception: its
        // value on exit is zero either way. Refuse to rotate when `exit`
        // mentions any other header parameter.
        // 旋转后 latch 的 false 边绕过 header 直跳 exit，exit 里读到的会是
        // 上一轮迭代的 header 参数值；唯一例外是被测计数器——退出时它恒为
        // 零，读哪个版本都一样。故 exit 引用其他 header 参数即拒绝旋转。
        let tested_param = params[tested_index];
        for &inst in data.layout().basicblock(exit).insts().iter() {
            for used in data.inst_data(inst).inst_usage() {
                if params.contains(&used) && used != tested_param {
                    return false;
                }
            }
        }

        // Move the header's test to the latch: br v', header(args), exit.
        // 步骤四（改写 1/2）：回边 `jump header(args)` 换成条件分支
        // `br v', header(args), exit`——测试移到底部、紧邻 latch 的递减。
        // 这正是给后端的目标形态：`subs w8, w8, #1; b.ne header`，递减写
        // NZCV flag、分支直接消费，一条指令完成"减一 + 测试"，循环内不再有
        // 独立的 `cmp`。true 边带原回边参数（含更新后的被测值）。
        let back_args = match data.inst_data(back_edge).kind() {
            InstKind::Jump(jump) => jump.args().to_vec(),
            _ => unreachable!(),
        };
        data.replace_inst_with(back_edge).branch(
            back_args[tested_index],
            header,
            back_args,
            exit,
            vec![],
        );

        // The header no longer tests; it passes straight through to the body.
        // 步骤四（改写 2/2）：header 的测试分支替换为 `jump body`。头部测试
        // 只守第一次迭代，而所有非回边都传非零常量、首测必过，删除是安全
        // 的；后续迭代由 latch 底部的同一测试把关。
        data.replace_inst_with(data.layout().basicblock(header).terminator())
            .jump(body, vec![]);

        true
    }

    /// Rotate a count-up `while (i < bound) { body; i += 1 }` loop to countdown
    /// form so the backend can fuse the decrement with the loop test. A trip
    /// counter `t = bound - i0` is carried in a new header parameter; a guard
    /// in the pre-header preserves the trip-zero semantics.
    ///
    /// 中文：count-up 旋转。`i < bound` 的比较无法与 `i += 1` 融合成一条
    /// 指令，故引入 trip 计数器 `t = bound - i0` 把循环改写为倒计数，让后端
    /// 能产出 `subs; b.ne` 融合形态。步骤：① 识别 `lt` 头部测试；② 找唯一
    /// entry/back 边（back 边带 `i + 1` 更新）；③ 处理 exit 区域的支配性
    /// （必要时给 exit 加镜像参数）；④ pre-header 插 guard；⑤ latch 递减
    /// 并底部测试。
    fn rotate_count_up(data: &mut ArenaContextMut<'_>, header: BasicBlock) -> bool {
        // 形态二：header 终结符必须是 `br (i < bound), body, exit`——条件为
        // `BinaryOp::Lt`（严格小于），分支两路不带参数、exit 无块参数；
        // `i` 稍后须是 header 参数，`bound` 须在循环前可用。
        let (body, exit, lt) = {
            let terminator = data.layout().basicblock(header).terminator();
            let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
                return false;
            };
            let body = branch.t_target();
            let exit = branch.f_target();
            if body == exit
                || !branch.t_args().is_empty()
                || !branch.f_args().is_empty()
                || !data.bb_data(exit).params().is_empty()
            {
                return false;
            }
            let InstKind::Binary(lt) = data.inst_data(branch.cond()).kind() else {
                return false;
            };
            if lt.op() != BinaryOp::Lt {
                return false;
            }
            (body, exit, (lt.lhs(), lt.rhs()))
        };
        let (iv, bound) = lt;

        let params = data.bb_data(header).params().to_vec();
        // `i` 必须是 header 参数：回边的 `i + 1` 更新流经块参数，才能在传参
        // 列表的同一位置多带一个 trip 计数参数。
        let Some(iv_pos) = params.iter().position(|&p| p == iv) else {
            return false;
        };
        // The bound must be defined before the loop so the trip counter and the
        // guard can be computed in the pre-header.
        // `bound` 不能是 header 参数、且必须在循环前可用（全局/常量/外部块
        // 参数），否则 trip 初值 `t0 = bound - i0` 无法在 pre-header 计算。
        if params.contains(&bound) || !Self::available_before_loop(data, &params, bound) {
            return false;
        }

        // Exactly one entry edge and one back edge, both jumps. The back edge
        // is the one carrying the `iv + 1` update; the entry edge carries the
        // initial value.
        // 恰好一条 entry 边（带初值）与一条 back 边（带的 `i` 参数是 `i + 1`
        // 加一更新）。IV 更新必须是加一常量：只有单位步进才能保证 trip 计数
        // `t - 1` 与原测试 `i < bound` 的迭代次数严格一致。同类型前驱重复
        // 出现会破坏 trip 初值/更新的唯一性，拒绝。
        let preds: Vec<Inst> = data.bb_data(header).used_by().iter().copied().collect();
        let mut back_edge: Option<(Inst, Vec<Inst>)> = None;
        let mut entry_edge: Option<(Inst, Vec<Inst>)> = None;
        for &pred in &preds {
            let InstKind::Jump(jump) = data.inst_data(pred).kind() else {
                return false;
            };
            let args = jump.args().to_vec();
            if args.len() != params.len() {
                return false;
            }
            let carries_update = match data.inst_data(args[iv_pos]).kind() {
                InstKind::Binary(b)
                    if b.op() == BinaryOp::Add
                        && matches!(
                            data.inst_data(b.rhs()).kind(),
                            InstKind::Integer(one) if one.value() == 1
                        ) =>
                {
                    // Direct `iv + 1`, or `p + 1` where `p` provably flows
                    // from `iv` through a chain of block parameters (the
                    // exit block of an inner loop carries the outer IV
                    // through its own parameters; the back edge updates it
                    // after the inner loop exits).
                    if b.lhs() == iv {
                        true
                    } else {
                        Self::value_flows_from(data, b.lhs(), iv, &mut FxHashSet::default())
                    }
                }
                _ => false,
            };
            if carries_update {
                if back_edge.replace((pred, args)).is_some() {
                    return false;
                }
            } else if entry_edge.replace((pred, args)).is_some() {
                return false;
            }
        }
        let Some((back_edge_inst, back_args)) = back_edge else {
            return false;
        };
        let Some((entry_edge_inst, entry_args)) = entry_edge else {
            return false;
        };

        // Give `exit` block parameters mirroring the header parameters whenever
        // it reads any of them. The guard and the latch both flow into `exit`
        // (with the entry or the last-iteration values respectively), so the
        // reads are remapped from the header parameters to the new block
        // parameters to keep SSA dominance.
        // 旋转后 guard 的 false 边从 pre-header 直入 exit、绕过 header 与整个
        // 循环；exit 及其后续可达区域若使用循环内产生的值，其支配定义会丢失
        // （SSA 非法）。对策分两层：区域用循环值则拒绝旋转；仅 exit 读 header
        // 参数则给它加镜像参数并重映射。
        let exit_reads_header = data.layout().basicblock(exit).insts().iter().any(|&inst| {
            data.inst_data(inst)
                .inst_usage()
                .any(|operand| params.contains(&operand))
        });
        // The guard's false edge flows into `exit` from the pre-header without
        // passing through the header. Every block reachable from `exit` then
        // has a path that bypasses the loop, so values produced inside the
        // loop (header parameters or body-defined values) would lose their
        // dominating definition. Refuse rotation whenever that region uses
        // such a value, unless the only user is `exit` itself, which is
        // handled by substituting its own block parameters below.
        let loop_blocks = Self::loop_blocks(data, header, body);
        let exit_region = Self::exit_region(data, exit, &loop_blocks);
        let region_uses_loop_value = exit_region.iter().any(|&block| {
            if block == exit {
                return false;
            }
            data.layout().basicblock(block).insts().iter().any(|&inst| {
                data.inst_data(inst)
                    .inst_usage()
                    .any(|operand| Self::is_loop_value(data, operand, &params, &loop_blocks))
            })
        });
        if region_uses_loop_value {
            return false;
        }
        // `exit` itself may read loop values only when they are header
        // parameters, which are remapped to its own parameters below; a
        // loop-defined non-parameter value cannot be substituted.
        let exit_uses_loop_value = data.layout().basicblock(exit).insts().iter().any(|&inst| {
            data.inst_data(inst).inst_usage().any(|operand| {
                if params.contains(&operand) {
                    return false;
                }
                Self::is_loop_value(data, operand, &params, &loop_blocks)
            })
        });
        if exit_uses_loop_value {
            return false;
        }
        if exit_reads_header {
            // Param-izing the exit block requires updating every edge into it.
            // The guard and latch are rewritten below, but any other
            // predecessor (e.g. a `break`/`continue` path) would keep passing
            // the old empty argument list, desynchronizing args from params.
            // exit 加参数要求 header 终结符是它的唯一前驱：guard 与 latch 两条
            // 入边由本函数改写并传参，其他前驱（break/continue 路径）不会同步
            // 传新参数，导致 args 与 params 错位，故拒绝。
            let header_terminator = data.layout().basicblock(header).terminator();
            if data
                .bb_data(exit)
                .used_by()
                .iter()
                .any(|&pred| pred != header_terminator)
            {
                return false;
            }
        }
        // 为 exit 镜像 header 的每个参数（同名类型），并把 exit 内对 header
        // 参数的引用重映射到新参数；guard 的 false 边传 entry 初值、latch
        // 传最后一轮迭代的值，两条入边各自带参。
        let _exit_params: Vec<Inst> = if exit_reads_header {
            let exit_params = params
                .iter()
                .map(|&parameter| {
                    let ty = data.inst_data(parameter).ty().clone();
                    data.new_basic_block().add_param(exit, ty)
                })
                .collect::<Vec<_>>();
            let substitution = params
                .iter()
                .zip(&exit_params)
                .map(|(&parameter, &replacement)| (parameter, replacement))
                .collect::<FxHashMap<_, _>>();
            let mut mapper = SubstMapper {
                substitution: &substitution,
            };
            let insts: Vec<Inst> = data
                .layout()
                .basicblock(exit)
                .insts()
                .iter()
                .copied()
                .collect();
            for inst in insts {
                if data
                    .inst_data(inst)
                    .inst_usage()
                    .any(|operand| substitution.contains_key(&operand))
                {
                    let remapped = data
                        .inst_data(inst)
                        .clone()
                        .remap_refs(&mut mapper)
                        .expect("mapping header parameters cannot fail");
                    data.replace_inst_with(inst).raw(remapped);
                }
            }
            exit_params
        } else {
            Vec::new()
        };

        // Append the trip counter to the header parameters.
        // 在 header 参数表末尾追加 trip 计数器 `t`；entry 与 back 两条入边的
        // 传参列表也要在末尾补上新值（guard 传 `t0`、latch 传 `t_next`），
        // 与参数表保持对齐。
        let t = data.new_basic_block().add_param(header, Type::get_i32());

        // Guard in the pre-header: `t0 = bound - i0`, enter the loop only when
        // the trip count is positive.
        // 在 pre-header 末尾插入 `t0 = bound - i0` 与 guard `t0 > 0`。guard
        // 保留原头部测试的 trip=0 语义：原 `i0 >= bound` 时零次执行循环体，
        // 若直接进循环会多执行一次。guard 失败（false 边）直跳 exit。
        let preheader = data.layout().parent_bb(entry_edge_inst).unwrap();
        let init_iv = entry_args[iv_pos];
        let zero = data.new_local_value().integer(0);
        let one = data.new_local_value().integer(1);
        let t0 = data.new_local_value().binary(BinaryOp::Sub, bound, init_iv);
        let positive = data.new_local_value().binary(BinaryOp::Gt, t0, zero);
        data.layout_mut().insert_before_terminator(preheader, t0);
        data.layout_mut()
            .insert_before_terminator(preheader, positive);
        // 把 entry 边改写为 guard 分支：true 边带 `(i0, t0)` 进 header，
        // false 边（trip 为零）直跳 exit。
        let mut header_entry_args = entry_args.clone();
        header_entry_args.push(t0);
        let exit_entry_args = if exit_reads_header {
            entry_args.clone()
        } else {
            Vec::new()
        };
        data.replace_inst_with(entry_edge_inst).branch(
            positive,
            header,
            header_entry_args,
            exit,
            exit_entry_args,
        );

        // The header passes straight through to the body.
        // 与 countdown 相同：header 移除测试、直通 body，迭代把关交给 latch
        // 底部的 `t_next` 测试。
        data.replace_inst_with(data.layout().basicblock(header).terminator())
            .jump(body, vec![]);

        // Latch: `t_next = t - 1`; test it at the bottom.
        // latch 里计算 `t_next = t - 1` 并作为底部测试条件，回边带
        // `(i + 1, t_next)` 回 header——与 countdown 同样的
        // `subs w8, w8, #1; b.ne header` 融合形态：递减写 NZCV flag、分支
        // 直接消费，省掉独立 compare。true 边（trip 未尽）回 header，false
        // 边（trip 归零）跳 exit。
        let latch = data.layout().parent_bb(back_edge_inst).unwrap();
        let t_next = data.new_local_value().binary(BinaryOp::Sub, t, one);
        data.layout_mut().insert_before_terminator(latch, t_next);
        let mut header_back_args = back_args.clone();
        header_back_args.push(t_next);
        let exit_back_args = if exit_reads_header {
            back_args.clone()
        } else {
            Vec::new()
        };
        data.replace_inst_with(back_edge_inst).branch(
            t_next,
            header,
            header_back_args,
            exit,
            exit_back_args,
        );

        true
    }

    /// Whether `value` provably equals `target` at the point of use: either
    /// it *is* `target`, or it is a block parameter whose every incoming
    /// edge carries a value that provably equals `target`. This follows the
    /// value through single-value chains of block parameters (nested-loop
    /// exit blocks pass the outer IV through their own parameters). A cyclic
    /// self-carrying edge is neutral (the parameter keeps its value), so a
    /// visited parameter is accepted.
    fn value_flows_from(
        data: &FunctionData,
        value: Inst,
        target: Inst,
        visited: &mut FxHashSet<Inst>,
    ) -> bool {
        if value == target {
            return true;
        }
        if !visited.insert(value) {
            return true; // Self-carrying edge: value unchanged.
        }
        let InstKind::BlockArgRef(_) = data.inst_data(value).kind() else {
            return false;
        };
        // Locate the owning block by scanning for the parameter (BlockArgRef
        // stores no back-reference to its block).
        let Some(param_block) = data
            .layout()
            .basicblocks()
            .iter()
            .find(|l| data.bb_data(l.bb()).params().contains(&value))
            .map(|l| l.bb())
        else {
            return false;
        };
        let param_pos = data
            .bb_data(param_block)
            .params()
            .iter()
            .position(|&p| p == value)
            .unwrap();
        let preds: Vec<Inst> = data
            .bb_data(param_block)
            .used_by()
            .iter()
            .copied()
            .collect();
        if preds.is_empty() {
            return false;
        }
        for pred_inst in preds {
            let args: Vec<Inst> = match data.inst_data(pred_inst).kind() {
                InstKind::Jump(jump) => jump.args().to_vec(),
                InstKind::Branch(branch) if branch.t_target() == param_block => {
                    branch.t_args().to_vec()
                }
                InstKind::Branch(branch) if branch.f_target() == param_block => {
                    branch.f_args().to_vec()
                }
                _ => return false,
            };
            let Some(&arg) = args.get(param_pos) else {
                return false;
            };
            if !Self::value_flows_from(data, arg, target, visited) {
                return false;
            }
        }
        true
    }

    /// 中文：`bound` 在循环前是否可用——全局值、常量，或外部块的块参数
    /// （header 参数被排除，因为它属于循环内产生的值）。
    fn available_before_loop(data: &FunctionData, params: &[Inst], bound: Inst) -> bool {
        if bound.is_global() || data.inst_data(bound).kind().is_const() {
            return true;
        }
        matches!(data.inst_data(bound).kind(), InstKind::BlockArgRef(..))
            && !params.contains(&bound)
    }

    /// Whether `value` is produced inside the loop being rotated: it is a
    /// header parameter, defined by an instruction in a loop block, or a
    /// block parameter of a loop block.
    ///
    /// 中文：`value` 是否由循环产生——header 参数、循环块内指令定义的值，
    /// 或循环块的块参数（无父块的指令按各块参数表判定）。
    fn is_loop_value(
        data: &FunctionData,
        value: Inst,
        params: &[Inst],
        loop_blocks: &FxHashSet<BasicBlock>,
    ) -> bool {
        if params.contains(&value) {
            return true;
        }
        match data.layout().parent_bb(value) {
            Some(block) => loop_blocks.contains(&block),
            None => loop_blocks
                .iter()
                .any(|&block| data.bb_data(block).params().contains(&value)),
        }
    }

    /// Blocks belonging to the loop being rotated: the header plus every
    /// block reachable from the body without passing back through the header.
    ///
    /// 中文：循环体块集合——header 加上从 body 沿 CFG 正向可达、且不经过
    /// header 的所有块（栈式 DFS，遇到 header 即截断）。
    fn loop_blocks(
        data: &FunctionData,
        header: BasicBlock,
        body: BasicBlock,
    ) -> FxHashSet<BasicBlock> {
        let mut blocks = FxHashSet::default();
        blocks.insert(header);
        let mut stack = vec![body];
        while let Some(block) = stack.pop() {
            if blocks.insert(block) {
                for successor in Self::block_successors(data, block) {
                    if successor != header {
                        stack.push(successor);
                    }
                }
            }
        }
        blocks
    }

    /// The region reachable from the loop `exit`: every block that becomes
    /// reachable on the pre-header guard's false edge after rotation.
    ///
    /// 中文：从 exit 出发、不进入循环体块的可达区域——旋转后 guard 的
    /// false 边（trip=0 直跳 exit）能到达的全部块。
    fn exit_region(
        data: &FunctionData,
        exit: BasicBlock,
        loop_blocks: &FxHashSet<BasicBlock>,
    ) -> Vec<BasicBlock> {
        let mut region = Vec::new();
        let mut seen = FxHashSet::default();
        let mut stack = vec![exit];
        while let Some(block) = stack.pop() {
            if seen.insert(block) {
                region.push(block);
                for successor in Self::block_successors(data, block) {
                    if !loop_blocks.contains(&successor) {
                        stack.push(successor);
                    }
                }
            }
        }
        region
    }

    /// 中文：块的终结符后继列表（`jump` 一个、`branch` 两个），供上述两个
    /// DFS 使用；其他终结符（`ret` 等）无后继。
    fn block_successors(data: &FunctionData, block: BasicBlock) -> Vec<BasicBlock> {
        let terminator = data.layout().basicblock(block).terminator();
        match data.inst_data(terminator).kind() {
            InstKind::Jump(jump) => vec![jump.target()],
            InstKind::Branch(branch) => vec![branch.t_target(), branch.f_target()],
            _ => Vec::new(),
        }
    }
}

/// Remaps header-parameter operands inside the exit block to its own
/// parameters while the loop rotates.
///
/// 中文：实体重映射器——把 exit 块内对 header 参数的引用替换为 exit 自己的
/// 新块参数（`EntityMapper::map_inst` 查表替换；块保持不变，故 `map_block`
/// 原样返回）。
struct SubstMapper<'a> {
    substitution: &'a FxHashMap<Inst, Inst>,
}

impl crate::ir::remap::EntityMapper for SubstMapper<'_> {
    type Error = std::convert::Infallible;

    fn map_inst(&mut self, inst: Inst) -> Result<Inst, Self::Error> {
        Ok(self.substitution.get(&inst).copied().unwrap_or(inst))
    }

    fn map_block(&mut self, block: BasicBlock) -> Result<BasicBlock, Self::Error> {
        Ok(block)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_loop(
        program: &mut Program,
        entry_value: i32,
    ) -> (Function, BasicBlock, BasicBlock, BasicBlock, BasicBlock) {
        let func = program.new_function(Type::get_i32(), "rot".to_owned(), vec![]);
        let (entry, header, body, exit) = {
            let data = program.func_data_mut(func);
            let entry = data
                .new_basic_block()
                .basic_block("entry".to_owned(), vec![]);
            let header = data
                .new_basic_block()
                .basic_block("header".to_owned(), vec![Type::get_i32()]);
            let body = data
                .new_basic_block()
                .basic_block("body".to_owned(), vec![]);
            let exit = data
                .new_basic_block()
                .basic_block("exit".to_owned(), vec![]);
            data.layout_mut().push_bb_back(entry);
            data.layout_mut().push_bb_back(header);
            data.layout_mut().push_bb_back(body);
            data.layout_mut().push_bb_back(exit);

            let init = data.new_local_inst().integer(entry_value);
            let jump = data.new_local_inst().jump(header, vec![init]);
            data.layout_mut().insert_inst(entry, jump);

            let param = data.bb_data(header).params()[0];
            let branch = data
                .new_local_inst()
                .branch(param, body, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, branch);

            let one = data.new_local_inst().integer(1);
            let decrement = data.new_local_inst().binary(BinaryOp::Sub, param, one);
            let back = data.new_local_inst().jump(header, vec![decrement]);
            data.layout_mut().insert_inst(body, back);

            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(exit, ret);

            (entry, header, body, exit)
        };
        (func, entry, header, body, exit)
    }

    fn run(data: &mut ArenaContextMut<'_>) -> bool {
        RotateLoops.run_on(data)
    }

    #[test]
    fn rotates_nonzero_entry_loop() {
        let mut program = Program::new();
        let (func, _entry, header, body, exit) = build_loop(&mut program, 32);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(func),
        };

        assert!(run(&mut data));
        // The header no longer tests; it jumps straight into the body.
        let terminator = data.layout().basicblock(header).terminator();
        match data.inst_data(terminator).kind() {
            InstKind::Jump(jump) => assert_eq!(jump.target(), body),
            other => panic!("expected jump to body, got {other:?}"),
        }
        // The latch now carries the test: one block branches back to the
        // header (with args) and out to the exit.
        let mut tested = data
            .layout()
            .basicblocks()
            .iter()
            .filter(|layout| {
                let terminator = layout.terminator();
                matches!(data.inst_data(terminator).kind(), InstKind::Branch(b)
                    if b.t_target() == header && b.f_target() == exit)
            })
            .count();
        assert_eq!(tested, 1);
        tested = data
            .layout()
            .basicblocks()
            .iter()
            .filter(|layout| {
                let terminator = layout.terminator();
                matches!(data.inst_data(terminator).kind(), InstKind::Branch(b)
                    if b.t_target() == body)
            })
            .count();
        assert_eq!(tested, 0, "no block may still branch to the body directly");
    }

    #[test]
    fn skips_zero_or_missing_const_entry() {
        let mut program = Program::new();
        let (func, _entry, _header, _body, _exit) = build_loop(&mut program, 0);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(func),
        };
        assert!(!run(&mut data), "zero entry value must not rotate");
    }

    fn build_count_up(
        program: &mut Program,
        function_name: &str,
        init: i32,
    ) -> (
        Function,
        BasicBlock,
        BasicBlock,
        BasicBlock,
        BasicBlock,
        Inst,
    ) {
        let func = program.new_function(
            Type::get_i32(),
            function_name.to_owned(),
            vec![Type::get_i32()],
        );
        let (entry, header, body, exit, entry_jump) = {
            let data = program.func_data_mut(func);
            let entry = data.add_entry_block();
            let header = data
                .new_basic_block()
                .basic_block("header".to_owned(), vec![Type::get_i32()]);
            let body = data
                .new_basic_block()
                .basic_block("body".to_owned(), vec![]);
            let exit = data
                .new_basic_block()
                .basic_block("exit".to_owned(), vec![]);
            for block in [header, body, exit] {
                data.layout_mut().push_bb_back(block);
            }

            let init_inst = data.new_local_inst().integer(init);
            let entry_jump = data.new_local_inst().jump(header, vec![init_inst]);
            data.layout_mut().insert_inst(entry, entry_jump);

            let iv = data.bb_data(header).params()[0];
            let bound = data.params()[0];
            let lt = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
            let header_branch = data.new_local_inst().branch(lt, body, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, lt);
            data.layout_mut().insert_inst(header, header_branch);

            let one = data.new_local_inst().integer(1);
            let next_iv = data.new_local_inst().binary(BinaryOp::Add, iv, one);
            let backedge = data.new_local_inst().jump(header, vec![next_iv]);
            data.layout_mut().insert_inst(body, next_iv);
            data.layout_mut().insert_inst(body, backedge);

            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(exit, ret);

            (entry, header, body, exit, entry_jump)
        };
        (func, entry, header, body, exit, entry_jump)
    }

    #[test]
    fn rotates_count_up_loop_into_guarded_countdown() {
        let mut program = Program::new();
        let (func, entry, header, body, exit, entry_jump) =
            build_count_up(&mut program, "rot_up", 0);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(func),
        };

        assert!(run(&mut data));
        // The header no longer tests; it jumps straight into the body and now
        // carries the countdown trip counter as a second parameter.
        let header_terminator = data.layout().basicblock(header).terminator();
        assert!(
            matches!(data.inst_data(header_terminator).kind(), InstKind::Jump(j) if j.target() == body)
        );
        assert_eq!(data.bb_data(header).params().len(), 2);
        // The trip counter is a header parameter.
        let t = data.bb_data(header).params()[1];
        assert!(data.inst_data(t).ty().is_i32());

        // The pre-header now guards `trip > 0`: it branches to the header (with
        // the countdown initial value) or to the exit.
        let InstKind::Branch(guard) = data.inst_data(entry_jump).kind() else {
            panic!("pre-header terminator must become a guard branch");
        };
        assert_eq!(guard.t_target(), header);
        assert_eq!(guard.f_target(), exit);
        assert_eq!(guard.t_args().len(), 2);
        assert!(guard.f_args().is_empty());

        // The latch tests `t - 1` at the bottom and passes the updated counter.
        let backedge = data.layout().basicblock(body).terminator();
        let InstKind::Branch(latch) = data.inst_data(backedge).kind() else {
            panic!("latch must become the bottom test");
        };
        assert_eq!(latch.t_target(), header);
        assert_eq!(latch.f_target(), exit);
        assert_eq!(latch.t_args().len(), 2);
        let InstKind::Binary(step) = data.inst_data(latch.cond()).kind() else {
            panic!("latch condition must be the decremented counter");
        };
        assert_eq!(step.op(), BinaryOp::Sub);
        assert_eq!(step.lhs(), t);
    }

    #[test]
    fn rotates_count_up_when_the_update_flows_through_block_params() {
        // matmul1 shape: the outer (j) loop's back edge updates the IV via a
        // chain of inner-loop exit block parameters — `t = add(p2, 1)` where
        // `p2` flows from the header IV through `body -> mid -> latch`. The
        // rotation must recognize the phi-chain update as the back edge.
        let mut program = Program::new();
        let func = program.new_function(
            Type::get_i32(),
            "rot_up_phi_chain".to_owned(),
            vec![Type::get_i32()],
        );
        let (entry, header, body, exit, latch) = {
            let data = program.func_data_mut(func);
            let entry = data.add_entry_block();
            let header = data
                .new_basic_block()
                .basic_block("header".to_owned(), vec![Type::get_i32()]);
            let body = data
                .new_basic_block()
                .basic_block("body".to_owned(), vec![]);
            let mid = data
                .new_basic_block()
                .basic_block("mid".to_owned(), vec![Type::get_i32()]);
            let latch = data
                .new_basic_block()
                .basic_block("latch".to_owned(), vec![Type::get_i32()]);
            let exit = data
                .new_basic_block()
                .basic_block("exit".to_owned(), vec![]);
            for block in [header, body, mid, latch, exit] {
                data.layout_mut().push_bb_back(block);
            }

            let init_inst = data.new_local_inst().integer(0);
            let entry_jump = data.new_local_inst().jump(header, vec![init_inst]);
            data.layout_mut().insert_inst(entry, entry_jump);

            let iv = data.bb_data(header).params()[0];
            let bound = data.params()[0];
            let lt = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
            let header_branch =
                data.new_local_inst().branch(lt, body, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, lt);
            data.layout_mut().insert_inst(header, header_branch);

            // body -> mid(iv); mid(p) -> latch(p); latch(p2) -> header(add(p2, 1)).
            let body_jump = data.new_local_inst().jump(mid, vec![iv]);
            data.layout_mut().insert_inst(body, body_jump);
            let mid_param = data.bb_data(mid).params()[0];
            let mid_jump = data.new_local_inst().jump(latch, vec![mid_param]);
            data.layout_mut().insert_inst(mid, mid_jump);
            let latch_param = data.bb_data(latch).params()[0];
            let one = data.new_local_inst().integer(1);
            let next_iv = data.new_local_inst().binary(BinaryOp::Add, latch_param, one);
            let backedge = data.new_local_inst().jump(header, vec![next_iv]);
            data.layout_mut().insert_inst(latch, next_iv);
            data.layout_mut().insert_inst(latch, backedge);

            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(exit, ret);

            (entry, header, body, exit, latch)
        };
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(func),
        };

        assert!(run(&mut data));
        // The header passes through and carries the trip counter.
        let header_terminator = data.layout().basicblock(header).terminator();
        assert!(
            matches!(data.inst_data(header_terminator).kind(), InstKind::Jump(j) if j.target() == body)
        );
        assert_eq!(data.bb_data(header).params().len(), 2);
        // The pre-header guard branches to header/exit.
        let guard = data.layout().basicblock(entry).terminator();
        assert!(
            matches!(data.inst_data(guard).kind(), InstKind::Branch(b) if b.t_target() == header && b.f_target() == exit)
        );
        // The latch tests the decremented trip counter at the bottom.
        let latch_term = data.layout().basicblock(latch).terminator();
        assert!(
            matches!(data.inst_data(latch_term).kind(), InstKind::Branch(b) if b.t_target() == header && b.f_target() == exit)
        );
    }

    #[test]
    fn refuses_when_the_region_after_exit_uses_the_induction_variable() {
        // Mirrors the sort-test regression: the exit block itself is clean,
        // but a block reached after the loop reads the induction variable
        // directly. After rotation the pre-header guard reaches that block
        // without passing through the header, so the header parameter would
        // lose its dominating definition; rotation must be refused.
        let mut program = Program::new();
        let func = program.new_function(
            Type::get_i32(),
            "rot_up_exit_region".to_owned(),
            vec![Type::get_i32()],
        );
        let (entry, header, body, exit, downstream, _entry_jump) = {
            let data = program.func_data_mut(func);
            let entry = data.add_entry_block();
            let header = data
                .new_basic_block()
                .basic_block("header".to_owned(), vec![Type::get_i32()]);
            let body = data
                .new_basic_block()
                .basic_block("body".to_owned(), vec![]);
            let exit = data
                .new_basic_block()
                .basic_block("exit".to_owned(), vec![]);
            let downstream = data
                .new_basic_block()
                .basic_block("downstream".to_owned(), vec![]);
            for block in [header, body, exit, downstream] {
                data.layout_mut().push_bb_back(block);
            }

            let init_inst = data.new_local_inst().integer(0);
            let entry_jump = data.new_local_inst().jump(header, vec![init_inst]);
            data.layout_mut().insert_inst(entry, entry_jump);

            let iv = data.bb_data(header).params()[0];
            let bound = data.params()[0];
            let lt = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
            let header_branch = data.new_local_inst().branch(lt, body, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, lt);
            data.layout_mut().insert_inst(header, header_branch);

            let one = data.new_local_inst().integer(1);
            let next_iv = data.new_local_inst().binary(BinaryOp::Add, iv, one);
            let backedge = data.new_local_inst().jump(header, vec![next_iv]);
            data.layout_mut().insert_inst(body, next_iv);
            data.layout_mut().insert_inst(body, backedge);

            let continue_jump = data.new_local_inst().jump(downstream, vec![]);
            data.layout_mut().insert_inst(exit, continue_jump);

            // The downstream block reads the induction variable directly, as
            // the sort-test swap block does.
            let five = data.new_local_inst().integer(5);
            let observed = data.new_local_inst().binary(BinaryOp::Add, iv, five);
            let ret = data.new_local_inst().ret(Some(observed));
            data.layout_mut().insert_inst(downstream, five);
            data.layout_mut().insert_inst(downstream, observed);
            data.layout_mut().insert_inst(downstream, ret);

            (entry, header, body, exit, downstream, entry_jump)
        };
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(func),
        };
        assert!(
            !run(&mut data),
            "rotation must be refused when the post-exit region reads a header parameter"
        );
        // The header still tests.
        let terminator = data.layout().basicblock(header).terminator();
        assert!(
            matches!(data.inst_data(terminator).kind(), InstKind::Branch(_)),
            "header must keep its test"
        );
    }

    #[test]
    fn gives_the_exit_block_parameters_when_it_reads_the_induction_variable() {
        let mut program = Program::new();
        let func = program.new_function(
            Type::get_i32(),
            "rot_up_exit_iv".to_owned(),
            vec![Type::get_i32()],
        );
        let (entry, header, body, exit, entry_jump) = {
            let data = program.func_data_mut(func);
            let entry = data.add_entry_block();
            let header = data
                .new_basic_block()
                .basic_block("header".to_owned(), vec![Type::get_i32()]);
            let body = data
                .new_basic_block()
                .basic_block("body".to_owned(), vec![]);
            let exit = data
                .new_basic_block()
                .basic_block("exit".to_owned(), vec![]);
            for block in [header, body, exit] {
                data.layout_mut().push_bb_back(block);
            }

            let init_inst = data.new_local_inst().integer(0);
            let entry_jump = data.new_local_inst().jump(header, vec![init_inst]);
            data.layout_mut().insert_inst(entry, entry_jump);

            let iv = data.bb_data(header).params()[0];
            let bound = data.params()[0];
            let lt = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
            let header_branch = data.new_local_inst().branch(lt, body, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, lt);
            data.layout_mut().insert_inst(header, header_branch);

            let one = data.new_local_inst().integer(1);
            let next_iv = data.new_local_inst().binary(BinaryOp::Add, iv, one);
            let backedge = data.new_local_inst().jump(header, vec![next_iv]);
            data.layout_mut().insert_inst(body, next_iv);
            data.layout_mut().insert_inst(body, backedge);

            // The exit reads the induction variable, forcing the rotation to
            // give it its own parameter.
            let five = data.new_local_inst().integer(5);
            let observed = data.new_local_inst().binary(BinaryOp::Add, iv, five);
            let ret = data.new_local_inst().ret(Some(observed));
            data.layout_mut().insert_inst(exit, five);
            data.layout_mut().insert_inst(exit, observed);
            data.layout_mut().insert_inst(exit, ret);

            (entry, header, body, exit, entry_jump)
        };
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(func),
        };
        assert!(run(&mut data));

        // Exit now carries one parameter mirroring the induction variable, and
        // its computation reads that parameter rather than the header one.
        assert_eq!(data.bb_data(exit).params().len(), 1);
        let exit_param = data.bb_data(exit).params()[0];
        let observed_uses_param = data
            .layout()
            .basicblock(exit)
            .insts()
            .iter()
            .any(|&inst| data.inst_data(inst).inst_usage().any(|op| op == exit_param));
        assert!(observed_uses_param, "exit must use its own parameter");

        // Both the guard (trip zero) and the latch (last iteration) pass the
        // induction variable to the exit.
        let InstKind::Branch(guard) = data.inst_data(entry_jump).kind() else {
            panic!("pre-header must guard");
        };
        assert_eq!(guard.f_target(), exit);
        assert_eq!(guard.f_args().len(), 1);
        let backedge = data.layout().basicblock(body).terminator();
        let InstKind::Branch(latch) = data.inst_data(backedge).kind() else {
            panic!("latch must be a branch");
        };
        assert_eq!(latch.f_target(), exit);
        assert_eq!(latch.f_args().len(), 1);
    }
}
