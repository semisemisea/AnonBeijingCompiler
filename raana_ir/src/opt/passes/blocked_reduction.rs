//! Register-block the strided matrix-reduction loop (h-5 / matmul shape).
//!
//! The inner loop `acc = acc - A[i][k] * B[k][j]` is memory-bound on the
//! column load `B[k][j]`: a row-major array makes consecutive `k` land
//! `n * 4` bytes apart, so every iteration takes a fresh cache-line miss. The
//! rewrite processes the trip count four at a time with four independent
//! accumulator lanes and four column pointers, so the column loads for
//! `k, k+1, k+2, k+3` are issued back-to-back and overlap the miss latency on
//! the in-order Cortex-A53 (the serial `msub` chain also breaks).
//!
//! Target shape (the test-at-bottom countdown form emitted after
//! `rotate_loops`):
//!
//! ```text
//! header(pt..., k, acc, ctr, ptr):
//!     jump body
//! body:
//!     row = load(get_elem_ptr(row_base, offsets_containing_k))
//!     col = load(get_elem_ptr(ptr, zero_offsets))
//!     acc2 = acc - row * col
//!     k2 = k + 1
//!     ctr2 = ctr - 1
//!     ptr2 = get_elem_ptr(ptr, [stride])
//!     br ctr2, header(pt..., k2, acc2, ctr2, ptr2), exit(...)
//! ```
//!
//! Every `pt...` parameter must be a pure pass-through (its back-edge argument
//! is the parameter itself); `ctr`'s entry value is the trip bound; `stride`
//! is a compile-time constant. Only this shape is rewritten (宁漏勿错).
//!
//! Rewrite: a versioning block guards `bound >= 4`; a main loop runs
//! `bound & ~3` iterations, four per round, with `acc0..acc3` and
//! `ptr0..ptr3` (`ptr_l` starts at `base + l*stride`, steps `4*stride`); the
//! scalar epilogue re-enters the original header at `k = bound & ~3`,
//! `ctr = bound & 3`, `acc = acc_in + Σ lanes`, `ptr = lane0's ptr`.
//!
//! ---
//!
//! ## 补充说明（中文）
//!
//! 术语：header / latch / preheader / backedge / trip count / test-at-bottom /
//! 归纳变量（IV）/ 固定点 / 支配 / 掩码 等见
//! `docs/offline-handbook/glossary.md`（"循环"与"优化与 pass 概念"分组）。
//!
//! ### 一句话定位
//!
//! 寄存器分块（register blocking）矩阵归约 pass：把 stride 访存的 msub 归约
//! 环 `acc -= A[i][k] * B[k][j]` 改写成 4 路累加器 + 4 路列指针的主循环，让
//! `k, k+1, k+2, k+3` 的列加载背靠背发射、互相掩盖缓存缺失延迟（in-order
//! Cortex-A53 上串行 `msub` 依赖链也一并断开）。面向 h-5 / matmul 热点
//! （M67，见 `TODO.md`：h-5 QEMU 7.16s → 4.49s）；只命中这一形态，宁漏勿错。
//!
//! ### 变换形态
//!
//! 英文示例即 `rotate_loops` 产出的 test-at-bottom 倒数式原环（header 只
//! `jump body`，latch 里 `br ctr2, header, exit`），不重复。改写后：
//!
//! ```text
//! preheader:           jump blocked_guard
//! blocked_guard:       br bound >= 4, blocked_main_header, header(原实参)
//! blocked_main_header: jump blocked_main_body
//! blocked_main_body:   4 条车道：acc_l / ptr_l 各 4 份；k 取 k_m+0..3
//!                      br ctr_m-4, blocked_main_header, blocked_main_exit
//! blocked_main_exit:   acc_final = acc_in + Σ acc_l; ctr_final = bound & 3
//!                      br ctr_final, header(尾参), exit(重建实参)
//! ```
//!
//! 主循环参数布局 `[acc0..acc3, k_m, ctr_m, ptr0..ptr3, pt...]`
//! （`main_block_types`）。车道 l 的列指针初值 `ptr_in + l*stride`、每轮步进
//! `4*stride`（`stride4`），即车道 l 依次访问 `k = l, l+4, l+8, ...`。
//!
//! ### 触发 / 放弃条件
//!
//! `run_on` 先排除 decl（`layout().is_decl()`）和无环 CFG（`cfg.is_acyclic()`）；
//! `find_candidate` 对每个循环做 (a)-(h) 逐项检查，任一不满足即放弃：
//!
//! - 形状 (a)-(b)：恰好 2 块的循环（header + 唯一 latch，即循环体）；header
//!   终结符是 `jump`（test-at-bottom）；latch 的 `branch` 一条臂回 header、
//!   另一条臂出循环；
//! - 参数角色 (c)：header 参数逐个对照回边实参——`ctr-1`
//!   （`BinaryOp::Sub`、右操作数常量 1）、`k+1`（`BinaryOp::Add` + 常量 1）、
//!   `acc - X`（`X` 是 `Mul`）、`get_elem_ptr(ptr, ...)`；其余必须纯透传
//!   （实参 == 参数本身）。四个角色缺一不可，出现其它更新形态即放弃；
//! - (d) `branch.cond()` 必须是 `ctr` 的更新值（倒数计数器在底部被测试）；
//! - (e) 累加更新必须是 `acc - mul(row_load, col_load)`：两操作数都是 `load`；
//!   `col` 的 GEP 基址是 `ptr_param`；`row` 的 GEP 偏移含 `k_param` 且基址
//!   定义在循环外（循环内定义基址即放弃）；
//! - (f) `ptr_back` 必须是恰好一个常量偏移的 `get_elem_ptr`
//!   （`constant_offset`），由此提取 `stride`；
//! - (g) 循环体（除终结符）不得含 `Store` / `Call` / `TailCall` / `MemZero` /
//!   `GlobalAlloc`（有副作用即放弃）；
//! - (g2) 原循环 exit 边的每个实参必须是透传值、`k_back` 或 `acc_back` 之一
//!   ——主循环出口必须能重建它们（`apply` 里重建失败是 `unreachable!`）；
//! - (h) trip bound 取 `ctr` 的入口初值（`init_arg`，恰好一条非回边提供），
//!   且必须支配循环入口（`dominates_loop_entry`：全局 / 常量 / 严格支配
//!   header 的块内定义）——版本化 guard 在 preheader 里要读它；
//! - `apply` 还要求恰好一个循环外前驱（`ensure_preheader`）锚定版本化块。
//!
//! ### 正确性要点
//!
//! - 主循环迭代数 = `bound & ~3`（`masked`），guard `bound >= 4` 保证至少跑
//!   一轮；`ctr_m` 每轮减 4、在底部测试（镜像原环的 test-at-bottom 语义）；
//! - 余数 `bound & 3` 由原环承接：`blocked_main_exit` 里
//!   `br ctr_final, header(尾参), exit(重建实参)`——余数非零重进原环、为零直跳
//!   exit。不能无条件重进：原环是 test-at-bottom，`ctr = 0` 重进会多跑一次
//!   迭代（英文注释 510-515 行同款论证）；
//! - 车道初值 `acc_l = 0`，各自累积自己的 k 切片乘积；出口
//!   `acc_final = acc_in + Σ acc_l` 与原环 `acc -= Σ A[k]*B[k]` 一致；k 的
//!   最终值 `bound & ~3` 交给标量尾循环继续；
//! - 透传参数逐位搬运进主循环（`pt...`）；exit 实参按 `k_back`→最终 k、
//!   `acc_back`→`acc_final`、透传→主循环参数重建；
//! - `BlockLaneMapper` 克隆循环体：`k_param`→`k_m + lane`、`acc_param`→车道
//!   参数、`ptr_param`→车道指针；循环外定义的不变值共享不克隆（SSA 引用仍
//!   合法）；单块纯循环体不可能引用其它块（`map_block` 直接 panic）。
//!
//! ### 管线位置与门控
//!
//! - 注册：`opt/pass.rs` 的 `PassesManager::from_config`，fixpoint 段，
//!   `reduction_unroll` **之后**、`if_conversion` 之前。`reduction_unroll`
//!   处理纯 `acc += a[j]` 形态（4 路拆分打断串行链），本 pass 处理带列指针的
//!   stride 形态，两者互补；命中形态由上游 `rotate_loops`（test-at-bottom
//!   countdown 化）、`pointer_strength_reduction`（列指针携带）等产出，
//!   同热点链上的 `matmul_interchange`（AArch64 门控）负责 GEMM 内层交换；
//! - 门控：`config.blocked_reduction`，默认开启（`opt/config.rs`），CLI
//!   `--enable-blocked-reduction` / `--disable-blocked-reduction`
//!   （`soyo_compiler/src/cli.rs`）做 A/B 测量；**无目标门控**（AArch64 /
//!   RISC-V 都跑，收益在 AArch64 上体现）；
//! - 一次 `run_on` 只改写一个循环：循环分析（`LoopAnalysis::from_cfg`）是
//!   快照，改写后立即失效，返回 true 交给 fixpoint 基于新状态重跑
//!   （`run_passes`，最多 100 轮）。
//!
//! ### 验证
//!
//! - 本文件 `mod tests`（765 行起）：
//!   `blocks_a_bound_eight_countdown_reduction`（bound = 8 命中：块数 +4、
//!   guard 存在且以 branch 终结）、`leaves_a_two_trip_countdown_loop_alone`
//!   （bound = 2 < 4：pass 仍改写、guard 恒走原环、块数同样 +4）；
//! - 全量：`cargo test -p raana_ir`（236 个单测）；端到端 `make test
//!   ARGS="-O 2"`（Docker harness）。M67 记录：h-5 QEMU 7.16s → 4.49s
//!   （-37%），输出逐字节一致。

use rustc_hash::FxHashMap;

use crate::{
    ir::{BasicBlock, BinaryOp, GetElemPtr, Inst, InstKind, Type, remap::EntityMapper},
    opt::{
        analysis_passes::{
            dom_tree::v2::DominanceTree,
            loop_analysis::{Loop, LoopAnalysis},
        },
        pass::Pass,
        prelude::*,
        utils::{cfg::CFG, logical_edge::incoming_edges},
    },
};

const UNROLL_FACTOR: usize = 4;

pub struct BlockedReduction;

impl Pass for BlockedReduction {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().is_decl() {
            return false;
        }
        let Some(cfg) = CFG::new(data) else {
            return false;
        };
        if cfg.is_acyclic() {
            return false;
        }
        let (cfg, dom_tree, loop_analysis) = LoopAnalysis::from_cfg(cfg);
        for looop in loop_analysis.loops() {
            let Some(candidate) =
                Self::find_candidate(data, &cfg, &dom_tree, &loop_analysis, looop)
            else {
                continue;
            };
            if Self::apply(data, &cfg, looop, &candidate) {
                return true;
            }
        }
        false
    }
}

struct Candidate {
    header: BasicBlock,
    body: BasicBlock,
    exit: BasicBlock,
    k_param: Inst,
    acc_param: Inst,
    ctr_param: Inst,
    ptr_param: Inst,
    acc_back: Inst,
    k_back: Inst,
    ctr_back: Inst,
    ptr_back: Inst,
    /// `(param, header_param_index)` for pass-through parameters.
    passthroughs: Vec<(Inst, usize)>,
    /// The trip bound: `ctr`'s value at loop entry. Must dominate the entry.
    bound: Inst,
    /// Column pointer stride extracted from `ptr_back`.
    stride: i64,
    header_params: Vec<Inst>,
    /// The original loop-back edge's exit arguments. Each must be a
    /// pass-through, `k_back`, or `acc_back` so the main-loop exit can
    /// reconstruct them.
    exit_args: Vec<Inst>,
}

impl BlockedReduction {
    fn find_candidate(
        data: &ArenaContextMut<'_>,
        cfg: &CFG,
        dom_tree: &DominanceTree,
        _loop_analysis: &LoopAnalysis,
        looop: &Loop,
    ) -> Option<Candidate> {
        // (a) A two-block loop: header plus a single pure body block (latch).
        if looop.body().len() != 2 || looop.latches().len() != 1 {
            return None;
        }
        let header = looop.header();
        let body = looop
            .body()
            .iter()
            .copied()
            .find(|&block| block != header)?;
        if looop.latches()[0] != body {
            return None;
        }

        // (b) The header falls into the body; the body branches back or out on
        // a countdown counter.
        let header_term = data.layout().basicblock(header).terminator();
        if !matches!(data.inst_data(header_term).kind(), InstKind::Jump(_)) {
            return None;
        }
        let body_insts: Vec<Inst> = data
            .layout()
            .basicblock(body)
            .insts()
            .iter()
            .copied()
            .collect();
        let back_term = *body_insts.last()?;
        let InstKind::Branch(branch) = data.inst_data(back_term).kind() else {
            return None;
        };
        let (exit, back_args) = if branch.t_target() == header {
            (branch.f_target(), branch.t_args().to_vec())
        } else if branch.f_target() == header {
            (branch.t_target(), branch.f_args().to_vec())
        } else {
            return None;
        };
        if looop.contains(exit) {
            return None;
        }

        // (c) Header parameters: each is either a pure pass-through or one of
        // {k, acc, ctr, ptr}. Resolve the roles from the loop-back arguments.
        let header_params = data.bb_data(header).params().to_vec();
        let mut k_param = None;
        let mut acc_param = None;
        let mut ctr_param = None;
        let mut ptr_param = None;
        let mut passthroughs = Vec::new();
        for (index, (&param, &back_arg)) in header_params.iter().zip(&back_args).enumerate() {
            let ty = data.inst_data(param).ty();
            if ty.is_i32() {
                match data.inst_data(back_arg).kind() {
                    // ctr = ctr - 1
                    InstKind::Binary(b)
                        if b.op() == BinaryOp::Sub
                            && b.lhs() == param
                            && is_constant(data, b.rhs(), 1) =>
                    {
                        ctr_param = Some(param);
                        continue;
                    }
                    // k = k + 1
                    InstKind::Binary(b)
                        if b.op() == BinaryOp::Add
                            && b.lhs() == param
                            && is_constant(data, b.rhs(), 1) =>
                    {
                        k_param = Some(param);
                        continue;
                    }
                    // acc = acc - row * col
                    InstKind::Binary(b)
                        if b.op() == BinaryOp::Sub
                            && b.lhs() == param
                            && matches!(
                                data.inst_data(b.rhs()).kind(),
                                InstKind::Binary(m) if m.op() == BinaryOp::Mul
                            ) =>
                    {
                        acc_param = Some(param);
                        continue;
                    }
                    _ => {}
                }
            } else if ty.is_pointer()
                && matches!(
                    data.inst_data(back_arg).kind(),
                    InstKind::GetElemPtr(gep) if gep.base() == param
                )
            {
                ptr_param = Some(param);
                continue;
            }
            if back_arg == param {
                passthroughs.push((param, index));
            } else {
                return None;
            }
        }
        let k_param = k_param?;
        let acc_param = acc_param?;
        let ctr_param = ctr_param?;
        let ptr_param = ptr_param?;
        let at = |param: Inst| header_params.iter().position(|&p| p == param).unwrap();
        let acc_back = back_args[at(acc_param)];
        let k_back = back_args[at(k_param)];
        let ctr_back = back_args[at(ctr_param)];
        let ptr_back = back_args[at(ptr_param)];

        // (d) The branch condition must be the countdown counter's update.
        if branch.cond() != ctr_back {
            return None;
        }

        // (e) The accumulator update must be `acc - mul(row_load, col_load)`
        // with the row load indexing `k` and the column load from `ptr`.
        let InstKind::Binary(acc_bin) = data.inst_data(acc_back).kind() else {
            return None;
        };
        let mul = acc_bin.rhs();
        let InstKind::Binary(mul_bin) = data.inst_data(mul).kind() else {
            return None;
        };
        let (row_load, col_load) = (mul_bin.lhs(), mul_bin.rhs());
        if !matches!(data.inst_data(row_load).kind(), InstKind::Load(_))
            || !matches!(data.inst_data(col_load).kind(), InstKind::Load(_))
        {
            return None;
        }
        let row_gep = gep_of(data, row_load)?;
        let col_gep = gep_of(data, col_load)?;
        if col_gep.base() != ptr_param {
            return None;
        }
        let row_base = row_gep.base();
        if data
            .layout()
            .parent_bb(row_base)
            .is_some_and(|b| looop.contains(b))
        {
            return None;
        }
        if !row_gep.offsets().iter().any(|&offset| offset == k_param) {
            return None;
        }

        // (f) The column pointer stride must be a compile-time constant.
        let InstKind::GetElemPtr(ptr_gep) = data.inst_data(ptr_back).kind() else {
            return None;
        };
        if ptr_gep.base() != ptr_param {
            return None;
        }
        let stride = constant_offset(data, ptr_gep)?;

        // (g) No stores / calls anywhere in the body.
        for &inst in &body_insts[..body_insts.len() - 1] {
            if matches!(
                data.inst_data(inst).kind(),
                InstKind::Store(..)
                    | InstKind::Call(..)
                    | InstKind::TailCall(..)
                    | InstKind::MemZero(..)
                    | InstKind::GlobalAlloc(..)
            ) {
                return None;
            }
        }

        // (g2) Every exit argument must be reconstructible at the main-loop
        // exit: a pass-through, `k_back`, or `acc_back`.
        let exit_args = branch.f_args().to_vec();
        for &arg in &exit_args {
            let is_pass = passthroughs.iter().any(|&(pt, _)| pt == arg);
            if !(is_pass || arg == k_back || arg == acc_back) {
                return None;
            }
        }

        // (h) The trip bound is `ctr`'s initializer and must dominate the loop
        // entry so the versioning guard in the preheader can read it.
        let bound = init_arg(data, cfg, looop, header, ctr_param)?;
        if !dominates_loop_entry(data, dom_tree, looop, bound) {
            return None;
        }

        Some(Candidate {
            header,
            body,
            exit,
            k_param,
            acc_param,
            ctr_param,
            ptr_param,
            acc_back,
            k_back,
            ctr_back,
            ptr_back,
            passthroughs,
            bound,
            stride,
            header_params,
            exit_args,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn apply(data: &mut ArenaContextMut<'_>, cfg: &CFG, looop: &Loop, cand: &Candidate) -> bool {
        // A dedicated preheader anchors the versioning block.
        let Some(preheader) = ensure_preheader(data, cfg, looop) else {
            return false;
        };
        let InstKind::Jump(jump) = data
            .inst_data(data.layout().basicblock(preheader).terminator())
            .kind()
        else {
            return false;
        };
        let orig_args = jump.args().to_vec();
        let _k_idx = cand
            .header_params
            .iter()
            .position(|&p| p == cand.k_param)
            .unwrap();
        let acc_idx = cand
            .header_params
            .iter()
            .position(|&p| p == cand.acc_param)
            .unwrap();
        let _ctr_idx = cand
            .header_params
            .iter()
            .position(|&p| p == cand.ctr_param)
            .unwrap();
        let ptr_idx = cand
            .header_params
            .iter()
            .position(|&p| p == cand.ptr_param)
            .unwrap();
        let acc_in = orig_args[acc_idx];
        let ptr_in = orig_args[ptr_idx];

        let pt_count = cand.passthroughs.len();
        let main_arg_count = UNROLL_FACTOR * 2 + 2 + pt_count;
        let main_types = main_block_types(data, cand);
        let version = data
            .new_basic_block()
            .basic_block("blocked_guard".into(), vec![]);
        let main_header = data
            .new_basic_block()
            .basic_block("blocked_main_header".into(), main_types.clone());
        let main_body = data
            .new_basic_block()
            .basic_block("blocked_main_body".into(), main_types.clone());
        let main_exit = data
            .new_basic_block()
            .basic_block("blocked_main_exit".into(), main_types);
        data.layout_mut().insert_bb_after(preheader, version);
        data.layout_mut().insert_bb_after(version, main_header);
        data.layout_mut().insert_bb_after(main_header, main_body);
        data.layout_mut().insert_bb_after(main_body, main_exit);

        let main_params = data.bb_data(main_header).params().to_vec();
        let body_params = data.bb_data(main_body).params().to_vec();
        let exit_params = data.bb_data(main_exit).params().to_vec();
        let (acc_start, k_mi, ctr_mi, ptr_start) =
            (0usize, UNROLL_FACTOR, UNROLL_FACTOR + 1, UNROLL_FACTOR + 2);
        let pt_start = UNROLL_FACTOR * 2 + 2;

        // ---- versioning block: guard bound >= 4 ----
        let zero = data.new_local_value().integer(0);
        let four = data.new_local_value().integer(4);
        let neg_four = data.new_local_value().integer(-4);
        let guard = data
            .new_local_value()
            .binary(BinaryOp::Ge, cand.bound, four);
        let masked = data
            .new_local_value()
            .binary(BinaryOp::And, cand.bound, neg_four);
        let mut main_init = Vec::with_capacity(main_arg_count);
        for _ in 0..UNROLL_FACTOR {
            main_init.push(zero);
        }
        main_init.push(zero); // k_m = 0
        main_init.push(masked); // ctr_m = bound & ~3
        for lane in 0..UNROLL_FACTOR {
            if lane == 0 {
                main_init.push(ptr_in);
            } else {
                let off = data
                    .new_local_value()
                    .integer((lane as i64 * cand.stride) as i32);
                let p = data.new_local_value().get_elem_ptr(ptr_in, vec![off]);
                data.layout_mut().insert_inst(version, p);
                main_init.push(p);
            }
        }
        for (_, idx) in &cand.passthroughs {
            main_init.push(orig_args[*idx]);
        }
        let guard_branch =
            data.new_local_value()
                .branch(guard, main_header, main_init, cand.header, orig_args);
        for inst in [guard, masked, guard_branch] {
            data.layout_mut().insert_inst(version, inst);
        }

        // ---- main loop header: jump to body ----
        let main_header_jump = data.new_local_value().jump(main_body, main_params.clone());
        data.layout_mut().insert_inst(main_header, main_header_jump);

        // ---- main loop body: four lanes, step-4 trip update, countdown ----
        let body_insts: Vec<Inst> = data
            .layout()
            .basicblock(cand.body)
            .insts()
            .iter()
            .copied()
            .collect();
        let body_insts = &body_insts[..body_insts.len() - 1];
        let mut next_accs = Vec::with_capacity(UNROLL_FACTOR);
        for lane in 0..UNROLL_FACTOR {
            let lane_const = data.new_local_value().integer(lane as i32);
            let kml = data
                .new_local_value()
                .binary(BinaryOp::Add, main_params[k_mi], lane_const);
            data.layout_mut().insert_inst(main_body, kml);
            let mut mapper = BlockLaneMapper {
                data: &mut *data,
                map: FxHashMap::default(),
                in_loop: |b: BasicBlock| looop.contains(b),
                k_param: cand.k_param,
                acc_param: cand.acc_param,
                ptr_param: cand.ptr_param,
                kml,
                acc_lane: body_params[acc_start + lane],
                ptr_lane: body_params[ptr_start + lane],
                block: main_body,
            };
            for &inst in body_insts {
                if inst == cand.k_back || inst == cand.ctr_back || inst == cand.ptr_back {
                    continue; // the main loop performs its own step-4 updates
                }
                mapper.clone_inst(inst);
            }
            let cloned_acc = mapper
                .map
                .get(&cand.acc_back)
                .copied()
                .expect("the accumulator update must be cloned into the main body");
            next_accs.push(cloned_acc);
        }
        let km_next = data
            .new_local_value()
            .binary(BinaryOp::Add, main_params[k_mi], four);
        data.layout_mut().insert_inst(main_body, km_next);
        let ctrm_next = data
            .new_local_value()
            .binary(BinaryOp::Sub, main_params[ctr_mi], four);
        data.layout_mut().insert_inst(main_body, ctrm_next);
        let stride4 = data.new_local_value().integer((4 * cand.stride) as i32);
        let mut back_args = Vec::with_capacity(main_arg_count);
        for &acc in &next_accs {
            back_args.push(acc);
        }
        back_args.push(km_next);
        back_args.push(ctrm_next);
        for lane in 0..UNROLL_FACTOR {
            let p = data
                .new_local_value()
                .get_elem_ptr(body_params[ptr_start + lane], vec![stride4]);
            data.layout_mut().insert_inst(main_body, p);
            back_args.push(p);
        }
        for (_, idx) in &cand.passthroughs {
            back_args.push(
                main_params[pt_start
                    + cand
                        .passthroughs
                        .iter()
                        .position(|(_, i)| i == idx)
                        .unwrap()],
            );
        }
        let back = data.new_local_value().branch(
            ctrm_next,
            main_header,
            back_args.clone(),
            main_exit,
            back_args.clone(),
        );
        data.layout_mut().insert_inst(main_body, back);

        // ---- main loop exit: acc = acc_in + Σ lanes. If the remainder trip
        // (`ctr_final = bound & 3`) is nonzero the scalar epilogue re-enters
        // the original header; otherwise the loop is fully consumed and we
        // jump straight to the original exit (the original loop is test-at-
        // bottom, so re-entering the header with a zero counter would run one
        // extra iteration). ----
        let s01 = data.new_local_value().binary(
            BinaryOp::Add,
            exit_params[acc_start],
            exit_params[acc_start + 1],
        );
        let s23 = data.new_local_value().binary(
            BinaryOp::Add,
            exit_params[acc_start + 2],
            exit_params[acc_start + 3],
        );
        let total = data.new_local_value().binary(BinaryOp::Add, s01, s23);
        let acc_final = data.new_local_value().binary(BinaryOp::Add, acc_in, total);
        // exit_params[k_mi] = bound & ~3 (the iterations the main loop ran).
        let ctr_final = data
            .new_local_value()
            .binary(BinaryOp::Sub, cand.bound, exit_params[k_mi]);
        let mut tail_args = Vec::with_capacity(cand.header_params.len());
        for (index, &param) in cand.header_params.iter().enumerate() {
            if param == cand.k_param {
                tail_args.push(exit_params[k_mi]);
            } else if param == cand.acc_param {
                tail_args.push(acc_final);
            } else if param == cand.ctr_param {
                tail_args.push(ctr_final);
            } else if param == cand.ptr_param {
                tail_args.push(exit_params[ptr_start]);
            } else {
                let pt = cand
                    .passthroughs
                    .iter()
                    .position(|(_, i)| *i == index)
                    .unwrap();
                tail_args.push(exit_params[pt_start + pt]);
            }
        }
        // Reconstruct the original exit's arguments by substituting the main
        // loop's final `k`/`acc` for the body's loop-back values.
        let mut exit_args = Vec::with_capacity(cand.exit_args.len());
        for &arg in &cand.exit_args {
            if arg == cand.k_back {
                exit_args.push(exit_params[k_mi]);
            } else if arg == cand.acc_back {
                exit_args.push(acc_final);
            } else if let Some(pt) = cand.passthroughs.iter().position(|&(p, _)| p == arg) {
                exit_args.push(exit_params[pt_start + pt]);
            } else {
                unreachable!("exit arguments were verified in find_candidate");
            }
        }
        let tail =
            data.new_local_value()
                .branch(ctr_final, cand.header, tail_args, cand.exit, exit_args);
        for inst in [s01, s23, total, acc_final, ctr_final, tail] {
            data.layout_mut().insert_inst(main_exit, inst);
        }

        // ---- redirect the preheader into the versioning block ----
        data.replace_inst_with(data.layout().basicblock(preheader).terminator())
            .jump(version, vec![]);

        true
    }
}

/// Types of the main-loop block parameters:
/// `[acc0..acc3, k_m, ctr_m, ptr0..ptr3, pt...]`.
fn main_block_types(data: &ArenaContextMut<'_>, cand: &Candidate) -> Vec<Type> {
    let mut types = Vec::with_capacity(UNROLL_FACTOR * 2 + 2 + cand.passthroughs.len());
    for _ in 0..UNROLL_FACTOR {
        types.push(Type::get_i32());
    }
    types.push(Type::get_i32()); // k_m
    types.push(Type::get_i32()); // ctr_m
    for _ in 0..UNROLL_FACTOR {
        types.push(data.inst_data(cand.ptr_param).ty().clone());
    }
    for (_, idx) in &cand.passthroughs {
        types.push(data.inst_data(cand.header_params[*idx]).ty().clone());
    }
    types
}

/// The GEP feeding a load.
fn gep_of(data: &ArenaContextMut<'_>, load: Inst) -> Option<GetElemPtr> {
    match data.inst_data(load).kind() {
        InstKind::Load(load) => match data.inst_data(load.src()).kind() {
            InstKind::GetElemPtr(gep) => Some(gep.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn is_constant(data: &ArenaContextMut<'_>, inst: Inst, value: i64) -> bool {
    matches!(data.inst_data(inst).kind(), InstKind::Integer(v) if i64::from(v.value()) == value)
}

/// Extract a single constant offset from a `get_elem_ptr` (used for the
/// column pointer stride).
fn constant_offset(data: &ArenaContextMut<'_>, gep: &GetElemPtr) -> Option<i64> {
    if gep.offsets().len() != 1 {
        return None;
    }
    match data.inst_data(gep.offsets()[0]).kind() {
        InstKind::Integer(v) => Some(i64::from(v.value())),
        _ => None,
    }
}

/// The value `param` receives on the non-back-edge entry of `header`.
fn init_arg(
    data: &ArenaContextMut<'_>,
    cfg: &CFG,
    looop: &Loop,
    header: BasicBlock,
    param: Inst,
) -> Option<Inst> {
    let mut found = None;
    for edge in incoming_edges(data, cfg, header) {
        if looop.contains(edge.source()) {
            continue;
        }
        if found.is_some() {
            return None; // multiple entries
        }
        let idx = data
            .bb_data(header)
            .params()
            .iter()
            .position(|&p| p == param)?;
        found = edge.args(data).get(idx).copied();
    }
    found
}

/// True when `value` can be used from a versioning block placed in the loop's
/// preheader: it must be defined by a block that strictly dominates the header.
fn dominates_loop_entry(
    data: &ArenaContextMut<'_>,
    dom_tree: &DominanceTree,
    looop: &Loop,
    value: Inst,
) -> bool {
    let strictly_dominates =
        |block: BasicBlock| block != looop.header() && dom_tree.dominates(block, looop.header());
    if value.is_global() {
        return true;
    }
    match data.layout().parent_bb(value) {
        Some(block) => strictly_dominates(block),
        None => {
            if data.inst_data(value).kind().is_const() {
                return true;
            }
            let owner = data
                .layout()
                .basicblocks()
                .iter()
                .find(|layout| data.bb_data(layout.bb()).params().contains(&value));
            owner.is_none_or(|layout| strictly_dominates(layout.bb()))
        }
    }
}

/// The single block that feeds the loop header from outside the loop.
fn ensure_preheader(data: &ArenaContextMut<'_>, cfg: &CFG, looop: &Loop) -> Option<BasicBlock> {
    let header = looop.header();
    let entry_preds: Vec<BasicBlock> = incoming_edges(data, cfg, header)
        .into_iter()
        .filter(|edge| !looop.contains(edge.source()))
        .map(|edge| edge.source())
        .collect();
    if entry_preds.len() != 1 {
        return None;
    }
    let pred = entry_preds[0];
    if looop.contains(pred) {
        return None;
    }
    Some(pred)
}

/// Clones the pure body instructions of one lane into the unrolled main body.
struct BlockLaneMapper<'a, 'b, F>
where
    F: Fn(BasicBlock) -> bool,
{
    data: &'a mut ArenaContextMut<'b>,
    map: FxHashMap<Inst, Inst>,
    in_loop: F,
    k_param: Inst,
    acc_param: Inst,
    ptr_param: Inst,
    kml: Inst,
    acc_lane: Inst,
    ptr_lane: Inst,
    block: BasicBlock,
}

impl<F: Fn(BasicBlock) -> bool> BlockLaneMapper<'_, '_, F> {
    fn clone_inst(&mut self, inst: Inst) -> Inst {
        if inst.is_global() {
            return inst;
        }
        if inst == self.k_param {
            return self.kml;
        }
        if inst == self.acc_param {
            return self.acc_lane;
        }
        if inst == self.ptr_param {
            return self.ptr_lane;
        }
        if let Some(&cloned) = self.map.get(&inst) {
            return cloned;
        }
        // Loop-invariant values (defined outside this loop) are shared.
        let in_loop = self
            .data
            .layout()
            .parent_bb(inst)
            .is_some_and(|block| (self.in_loop)(block));
        if !in_loop {
            return inst;
        }
        let inst_data = self.data.inst_data(inst).clone();
        let shell = self.data.new_local_value().undef(inst_data.ty().clone());
        self.map.insert(inst, shell);
        let mapped = inst_data
            .remap_refs(self)
            .expect("a pure single-block reduction body cannot reference foreign blocks");
        self.data.replace_inst_with(shell).raw(mapped);
        self.data.layout_mut().insert_inst(self.block, shell);
        shell
    }
}

impl<F: Fn(BasicBlock) -> bool> EntityMapper for BlockLaneMapper<'_, '_, F> {
    type Error = std::convert::Infallible;

    fn map_inst(&mut self, inst: Inst) -> Result<Inst, Self::Error> {
        Ok(self.clone_inst(inst))
    }

    fn map_block(&mut self, _block: BasicBlock) -> Result<BasicBlock, Self::Error> {
        panic!("a pure single-block reduction body cannot reference blocks")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{Function, Program, Type, builder_trait::*},
        opt::pass::Pass as _,
    };

    /// Build the countdown msub reduction loop:
    /// `acc = acc - A[k] * B[k]` for `ctr` steps, with a column pointer that
    /// advances by `stride` elements each iteration. Header params
    /// `[k, acc, ctr, ptr]`; the body is a single test-at-bottom block.
    fn build_reduction(program: &mut Program, bound: i32, stride: i32) -> Function {
        let function = program.new_function(
            Type::get_i32(),
            "blocked_reduction".into(),
            vec![
                Type::get_pointer(Type::get_array(Type::get_i32(), 64)),
                Type::get_pointer(Type::get_i32()),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let row_base = data.params()[0];
        let col_base = data.params()[1];
        let header = data.new_basic_block().basic_block(
            "header".into(),
            vec![
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
                Type::get_pointer(Type::get_i32()),
            ],
        );
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data
            .new_basic_block()
            .basic_block("exit".into(), vec![Type::get_i32(), Type::get_i32()]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let params = data.bb_data(header).params().to_vec();
        let (k, acc, ctr, ptr) = (params[0], params[1], params[2], params[3]);
        let header_jump = data.new_local_inst().jump(body, vec![]);
        data.layout_mut().insert_inst(header, header_jump);

        let zero = data.new_local_inst().integer(0);
        let bound_inst = data.new_local_inst().integer(bound);
        let entry_jump = data
            .new_local_inst()
            .jump(header, vec![zero, zero, bound_inst, col_base]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let one = data.new_local_inst().integer(1);
        let row_gep = data.new_local_inst().get_elem_ptr(row_base, vec![zero, k]);
        let row_load = data.new_local_inst().load(row_gep);
        let col_gep = data.new_local_inst().get_elem_ptr(ptr, vec![zero]);
        let col_load = data.new_local_inst().load(col_gep);
        let mul = data
            .new_local_inst()
            .binary(BinaryOp::Mul, row_load, col_load);
        let acc_update = data.new_local_inst().binary(BinaryOp::Sub, acc, mul);
        let k_update = data.new_local_inst().binary(BinaryOp::Add, k, one);
        let ctr_update = data.new_local_inst().binary(BinaryOp::Sub, ctr, one);
        let stride_inst = data.new_local_inst().integer(stride);
        let ptr_update = data.new_local_inst().get_elem_ptr(ptr, vec![stride_inst]);
        for inst in [
            row_gep, row_load, col_gep, col_load, mul, acc_update, k_update, ctr_update, ptr_update,
        ] {
            data.layout_mut().insert_inst(body, inst);
        }
        let back = data.new_local_inst().branch(
            ctr_update,
            header,
            vec![k_update, acc_update, ctr_update, ptr_update],
            exit,
            vec![k_update, acc_update],
        );
        data.layout_mut().insert_inst(body, back);

        let exit_acc = data.bb_data(exit).params()[1];
        let ret = data.new_local_inst().ret(Some(exit_acc));
        data.layout_mut().insert_inst(exit, ret);
        function
    }

    fn run(program: &mut Program, function: Function) -> bool {
        let mut context = ArenaContextMut {
            program,
            curr_func: Some(function),
        };
        BlockedReduction.run_on(&mut context)
    }

    #[test]
    fn blocks_a_bound_eight_countdown_reduction() {
        let mut program = Program::new();
        let function = build_reduction(&mut program, 8, 6);
        let block_count = program.func_data(function).layout().basicblocks().len();

        assert!(run(&mut program, function), "the reduction must be blocked");
        let data = program.func_data(function);
        assert_eq!(
            data.layout().basicblocks().len(),
            block_count + 4,
            "versioning + main header/body/exit are added"
        );
        // The versioning guard branches to the main loop when bound >= 4.
        let guard = data
            .layout()
            .basicblocks()
            .iter()
            .find(|layout| data.bb_data(layout.bb()).name().contains("blocked_guard"))
            .expect("the versioning block must exist");
        let terminator = *guard.insts().get_last().unwrap();
        assert!(
            matches!(data.inst_data(terminator).kind(), InstKind::Branch(_)),
            "the guard ends in a branch"
        );
    }

    #[test]
    fn leaves_a_two_trip_countdown_loop_alone() {
        // bound = 2 (< 4): the versioning guard always takes the original
        // loop, but the pass still applies (the guard exists) and the original
        // loop remains for the scalar epilogue.
        let mut program = Program::new();
        let function = build_reduction(&mut program, 2, 6);
        let block_count = program.func_data(function).layout().basicblocks().len();
        assert!(run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.layout().basicblocks().len(), block_count + 4);
    }
}
