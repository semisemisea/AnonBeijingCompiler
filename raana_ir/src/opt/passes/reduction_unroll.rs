//! Scalar reduction unrolling with independent accumulator splitting.
//!
//! Recognizes unit-step, single-accumulator reduction loops of the form
//!
//! ```text
//! header(acc, j):  br j < bound, body, exit
//! body:            ...acc2 = acc ± E  (or select(c, acc ± E, acc))...
//!                  ...j2 = j + 1...
//!                  jump header(acc2, j2)
//! exit:
//! ```
//!
//! and rewrites them so the trip count is processed in groups of four by four
//! independent accumulators. This breaks the self-dependency chain of the
//! scalar accumulation (the `madd w11, w11, ...` chain that limits AArch64
//! throughput to the multiply latency, ~0.25 elem/cycle on Cortex-A53 for
//! `tests/perf/many_mat_cal-1.sy`).
//!
//! A third loop-carried parameter is supported: a pointer advanced by a
//! constant element offset each trip (the matmul inner loop `(k, acc, ptr)`
//! shape, `many_mat_cal`'s hotspot). The four lanes then read through
//! `ptr + {0, stride, 2*stride, 3*stride}` and the main loop advances the base
//! pointer by `4*stride`, so lane `k` covers trip `jm + k`.
//!
//! Soundness:
//! - The body is a single pure block (loads only: no store / call / memzero),
//!   so cloning it four times cannot duplicate side effects.
//! - The accumulator is only updated through `acc ± E` (or the `select`
//!   form), with `E` independent of `acc`; integer addition/subtraction is
//!   associative and commutative modulo 2³², so splitting the reduction into
//!   four independent lanes and summing the four results equals the original.
//! - The pointer is advanced by address arithmetic only (`getelemptr`); lane
//!   splitting is pure address algebra and never touches memory semantics.
//! - The main loop runs `j < (T & ~3)`; the original loop, kept intact as a
//!   scalar epilogue, consumes the remaining `T % 4` iterations. A runtime
//!   versioning guard `T >= 4` selects the unrolled path; smaller trip counts
//!   take the scalar loop untouched.
//!
//! The trip counter must start at zero (`j_init == 0`); this holds for every
//! reduction loop in the corpus. The recognition is purely structural: it
//! never matches names, strings, or benchmark-specific bounds (per
//! `docs/Illegal_optimization.md` rule two; see TODO.md §2.2).
//!
//! ---
//!
//! ## 补充说明（中文）
//!
//! 术语：归约（reduction，把一整个循环的运算收敛到一个累加值）、累加器
//! （accumulator，循环携带的中间和）、依赖链、latch / preheader / backedge、
//! 基本归纳变量（BIV）、严格 exit、运行时版本化（versioning）等见
//! `docs/offline-handbook/glossary.md`。
//!
//! ### 一句话定位与动机
//!
//! 识别**单位步长、单累加器**的标量归约循环（`for j: acc += a[j]` 形态），
//! 把一趟循环按 `UNROLL_FACTOR = 4` 拆成 4 条互相独立的累加通道，打破
//! 串行的累加依赖链。标量归约每轮 `acc' = acc ± E` 都读上一轮的 `acc`，
//! 在 AArch64 上退化成一条串行 `madd w11, w11, ...` 链，吞吐被乘法延迟
//! 卡死（Cortex-A53 上约 0.25 元素/周期，见
//! `tests/perf/many_mat_cal-1.sy`）；拆成 4 条独立链后 4 个累加器可以
//! 并行发射，循环退出时再把 4 个部分和相加。
//!
//! ### 变换形态
//!
//! 英文文档上方的 IR 就是原循环形态。改写后（`apply`）新增 4 个块：
//!
//! ```text
//! preheader:   jump version
//! version:     guard = T >= 4; masked = T & ~3; br guard, main_header, header
//! main_header: br jm < masked, main_body, main_exit   // 参数 [acc_in, a0..a3, jm]
//! main_body:   4 份克隆的纯 body（lane k 用 jm+k 与自己的累加器 a_k），
//!              jm' = jm + 4; jump main_header
//! main_exit:   acc = acc_in + a0 + a1 + a2 + a3; jump header
//! ```
//!
//! 原循环整体保留，作为标量 epilogue 吃掉剩余 `T % 4` 轮；版本化守卫
//! `T >= 4` 在运行时选择展开路径。带指针的循环（matmul 内层 `(k, acc,
//! ptr)` 形态）把指针作为第 3 个循环携带参数：4 条通道分别经
//! `ptr + {0, stride, 2*stride, 3*stride}` 读取，主循环基指针每轮前进
//! `4*stride`，通道 k 覆盖第 `jm + k` 轮。
//!
//! ### 触发条件
//!
//! `run_on` 先跳过声明函数（`is_decl`）与无环函数（`cfg.is_acyclic`）；
//! 之后 `find_candidate` 对每个循环逐条检查，任一不满足即拒绝：
//!
//! - 循环恰好 2 个基本块（header + 唯一的 latch/body），latch 无参数；
//! - header 恰好 2~3 个参数：i32 累加器 + trip 计数器 +（可选）指针；
//! - 恰好 1 个基本归纳变量（`BasicInductionVariableAnalysis`），步长
//!   +1、严格前向 exit（`normalize_strict_exit` +
//!   `InductionDirection::Forward`）；
//! - bound 支配循环入口（`dominates_loop_entry`），否则版本化守卫无法
//!   放进 preheader（conv2d 的 checksum bound 是循环内值，被拒）；
//! - body 以 `jump` 回 header，回边参数个数与 header 参数一致；
//! - trip 计数器初值必须是整数 0（`is_integer_zero`），展开通道才能与
//!   `j mod 4` 对齐（语料里所有归约循环都满足）；
//! - 指针回边必须是单个**非零常数**偏移的 `getelemptr` 且定义在 body 内
//!   （动态偏移 / 任意值一律拒绝：地址只能做纯代数运算）；
//! - 累加器更新匹配 `match_acc_update`：`acc ± E`（E ≠ acc）或
//!   `select(c, acc ± E, acc)`（`AccPattern { add, select }`）；
//! - body 内无 `store` / `call` / `tailcall` / `memzero` / `global_alloc`，
//!   且 `acc` 除上述更新模式外不被 body 内任何指令使用。
//!
//! 识别是纯结构性的：从不匹配名字、字符串或特定基准的边界
//! （`docs/Illegal_optimization.md` 规则二，TODO.md §2.2）。
//!
//! ### 放弃条件
//!
//! - 上述任一检查失败（`find_candidate` 各 `return None` 点）；
//! - `apply` 需要新建 preheader（`ensure_preheader` 返回
//!   `EnsurePreheader::Created`）时返回 true 让 fixpoint 从零重跑，避免
//!   半改写状态落地；preheader 拿不到则本次不改写；
//! - 守卫 `T >= 4` 运行时失败：不进主循环，直接走原标量循环（常量
//!   bound < 4 也照常发射版本化结构，只是运行时总走标量路径）；
//! - 每次 `run_on` 只改写一个循环（成功即返回 true），其余候选由
//!   fixpoint 多轮迭代处理。
//!
//! ### 正确性要点
//!
//! - 副作用：body 是单块纯代码（只 load），克隆 4 次不会复制副作用；
//! - 代数：累加器只经 `acc ± E`（或 select 形式）更新且 E 与 acc 独立；
//!   i32 加减在模 2³² 下满足结合律与交换律，4 条通道之和 ≡ 原序列结果；
//! - 指针：只做 `getelemptr` 地址算术，通道拆分是纯地址代数，不改变
//!   任何 load 的内存语义；
//! - 边界：主循环只跑 `j < (T & ~3)`，剩余 `T % 4` 轮由原样保留的标量
//!   epilogue 消费；守卫失败时整趟退回标量路径；
//! - SSA：主循环退出按原 header 的参数顺序回填（`a_idx` / `j_idx` /
//!   指针槽位），`[acc, j]` 与 `[j, acc]` 两种参数顺序都正确
//!   （`preserves_the_accumulator_position_in_the_epilogue_join`）；
//! - 幂等：改写后原循环不再满足识别条件，fixpoint 内不会二次改写
//!   （`unrolls_a_symbolic_bound_reduction_loop` 断言 second run 返回
//!   false）。
//!
//! ### 管线位置
//!
//! - 注册：`opt/pass.rs` 的 `PassesManager::from_config`，fixpoint 段，
//!   **`invariant_reduction_hoisting` 之后、`blocked_reduction` 之前**
//!   （后者有 `config.blocked_reduction` 门控；本 pass 无门控、无 config
//!   开关，无条件挂载）；
//! - 前后配合：`invariant_reduction_hoisting` 先把外层 trip 循环的不变
//!   归约退化掉，让本 pass 看到未动过的嵌套循环；本 pass 处理
//!   `acc += a[j]` 形态；`blocked_reduction` 处理跨步矩阵归约
//!   `acc -= A[i][k] * B[k][j]`（4 累加器 + 4 列指针重叠列装载的缓存
//!   未命中），与本 pass 互补。
//!
//! ### 验证
//!
//! - 本文件 `mod tests`（808 行起）：符号 bound 展开（新增 4 块、主循环
//!   bound 为 `n & ~3`、标量 epilogue 保留、幂等）、header 参数顺序
//!   `[acc, j]` / `[j, acc]`、select 形式条件累加、常量 bound < 4、
//!   指针携带循环（lane 偏移 {0, 1024, 2048, 3072} + 回边 4096）；
//! - 本地：`cargo test -p raana_ir`；端到端：`make test`（Docker
//!   harness，stdout + 退出码差分比对）。
//!

use rustc_hash::FxHashMap;

use crate::ir::inst_kind::Binary;
use crate::ir::remap::EntityMapper;
use crate::opt::{
    analysis_passes::{
        dom_tree::v2::DominanceTree,
        induction_variable::{
            BasicInductionVariableAnalysis, InductionDirection, normalize_strict_exit,
        },
        loop_analysis::{Loop, LoopAnalysis},
    },
    prelude::*,
    utils::{
        cfg::CFG,
        logical_edge::incoming_edges,
        preheader::{EnsurePreheader, ensure_preheader},
    },
};

/// Number of independent accumulator lanes (unroll factor).
const UNROLL_FACTOR: usize = 4;

pub struct ReductionUnroll;

/// Everything needed to rewrite one reduction loop.
struct Candidate {
    header: BasicBlock,
    body: BasicBlock,
    bound: Inst,
    acc: Inst,
    j: Inst,
    a_idx: usize,
    j_idx: usize,
    /// The back-edge value of the accumulator (the update to be lane-split).
    acc_update: Inst,
    /// The back-edge value of the trip counter (not cloned into the main loop).
    j_back: Inst,
    /// A loop-carried pointer stepped by a constant element offset (optional).
    ptr: Option<PtrIv>,
}

/// A loop-carried pointer with its element stride.
#[derive(Clone, Copy)]
struct PtrIv {
    /// Index of the pointer parameter in the header's parameter list.
    idx: usize,
    /// The header parameter carrying the pointer.
    param: Inst,
    /// The constant GEP offset (in elements) added each iteration.
    stride: i64,
    /// The back-edge value of the pointer (the `getelemptr ptr, stride`).
    ptr_back: Inst,
}

/// The instructions that make up the accumulator update pattern.
#[derive(Debug, Clone, Copy)]
struct AccPattern {
    add: Inst,
    select: Option<Inst>,
}

impl AccPattern {
    fn contains(self, inst: Inst) -> bool {
        self.add == inst || self.select == Some(inst)
    }
}

impl ReductionUnroll {
    fn find_candidate(
        data: &ArenaContextMut<'_>,
        cfg: &CFG,
        dom_tree: &DominanceTree,
        loop_analysis: &LoopAnalysis,
        looop: &Loop,
    ) -> Option<Candidate> {
        // (a) A two-block loop: header plus a single pure body block (the latch).
        if looop.body().len() != 2 || looop.latches().len() != 1 {
            return None;
        }
        let header = looop.header();
        let body = looop
            .body()
            .iter()
            .copied()
            .find(|&block| block != header)?;
        if looop.latches()[0] != body || !data.bb_data(body).params().is_empty() {
            return None;
        }

        // (b) Two or three header parameters: the accumulator and trip IV,
        // optionally plus a loop-carried pointer stepped by a constant offset.
        let params = data.bb_data(header).params().to_vec();
        if !(2..=3).contains(&params.len()) {
            return None;
        }

        // (c) Exactly one basic induction variable, step +1, strict `j < bound`.
        let biv_analysis = BasicInductionVariableAnalysis::new(data, cfg, loop_analysis);
        let bivs = biv_analysis.for_loop(looop);
        if bivs.len() != 1 {
            return None;
        }
        let biv = &bivs[0];
        let normalized_exit = normalize_strict_exit(data, looop, biv)?;
        if normalized_exit.direction() != InductionDirection::Forward
            || normalized_exit.signed_step() != 1
        {
            return None;
        }
        let j = biv.parameter();
        let j_idx = params.iter().position(|&param| param == j)?;
        // The accumulator is the i32 non-trip parameter; the pointer (optional)
        // is the pointer-typed parameter. Both must step back into the header.
        let mut acc = None;
        let mut ptr = None;
        for (idx, &param) in params.iter().enumerate() {
            if idx == j_idx {
                continue;
            }
            if data.inst_data(param).ty().is_pointer() {
                if ptr.is_some() {
                    return None;
                }
                ptr = Some((idx, param));
            } else {
                if acc.is_some() {
                    return None;
                }
                acc = Some((idx, param));
            }
        }
        let (a_idx, acc) = acc?;
        if !data.inst_data(acc).ty().is_i32() {
            return None;
        }
        // The runtime bound is tested in the new versioning block, which sits
        // in the loop's preheader. It must therefore dominate the loop header;
        // a bound computed inside the loop (e.g. conv2d's checksum bound) would
        // otherwise be used without a dominating definition.
        if !dominates_loop_entry(data, dom_tree, looop, normalized_exit.bound()) {
            return None;
        }

        // (d) The header branches into the body (loop) and out to the exit.
        let terminator = data.layout().basicblock(header).terminator();
        let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
            return None;
        };
        let exit_block = if branch.t_target() == body {
            branch.f_target()
        } else {
            return None;
        };
        if looop.contains(exit_block) {
            return None;
        }
        let _ = exit_block;

        // (e) The body ends with a jump back to the header carrying the updated
        // accumulator, trip counter, and (optionally) the stepped pointer.
        let body_insts = data
            .layout()
            .basicblock(body)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let back_term = *body_insts.last()?;
        let InstKind::Jump(back) = data.inst_data(back_term).kind() else {
            return None;
        };
        if back.target() != header {
            return None;
        }
        let back_args = back.args().to_vec();
        if back_args.len() != params.len() {
            return None;
        }
        let acc_update = back_args[a_idx];
        let j_back = back_args[j_idx];
        let ptr = match ptr {
            Some((ptr_idx, ptr_param)) => {
                // The pointer's back-edge must be a single-offset `getelemptr`
                // by a non-zero constant (a row/column stride), defined in the
                // body. Anything else (dynamic offset, arbitrary value) is
                // rejected; address algebra only, per the soundness notes.
                let ptr_back = back_args[ptr_idx];
                let InstKind::GetElemPtr(gep) = data.inst_data(ptr_back).kind() else {
                    return None;
                };
                let offsets = gep.offsets();
                if offsets.len() != 1 {
                    return None;
                }
                let InstKind::Integer(stride) = data.inst_data(offsets[0]).kind() else {
                    return None;
                };
                if stride.value() == 0 {
                    return None;
                }
                if data.layout().parent_bb(ptr_back) != Some(body) {
                    return None;
                }
                Some(PtrIv {
                    idx: ptr_idx,
                    param: ptr_param,
                    stride: i64::from(stride.value()),
                    ptr_back,
                })
            }
            None => None,
        };

        // (f) The non-back-edge initializer must start the trip counter at zero
        // so the unrolled lanes line up with `j mod 4`.
        let mut init_args = None;
        for edge in incoming_edges(data, cfg, header) {
            if !looop.contains(edge.source()) {
                if init_args.is_some() {
                    return None;
                }
                init_args = Some(edge.args(data).to_vec());
            }
        }
        let init_args = init_args?;
        if !is_integer_zero(data, init_args[j_idx]) {
            return None;
        }

        // (g) Match the accumulator update pattern and confirm the accumulator
        // is used nowhere else inside the body.
        let pattern = match_acc_update(data, acc, acc_update)?;
        for &inst in &body_insts[..body_insts.len() - 1] {
            let kind = data.inst_data(inst).kind();
            if matches!(
                kind,
                InstKind::Store(..)
                    | InstKind::Call(..)
                    | InstKind::TailCall(..)
                    | InstKind::MemZero(..)
                    | InstKind::GlobalAlloc(..)
            ) {
                return None;
            }
            if !pattern.contains(inst) {
                for used in data.inst_data(inst).inst_usage() {
                    if used == acc {
                        return None;
                    }
                }
            }
        }
        if data.layout().parent_bb(acc_update) != Some(body)
            || data.layout().parent_bb(j_back) != Some(body)
        {
            return None;
        }

        Some(Candidate {
            header,
            body,
            bound: normalized_exit.bound(),
            acc,
            j,
            a_idx,
            j_idx,
            acc_update,
            j_back,
            ptr,
        })
    }

    /// Rewrite one reduction loop. Returns true when the function changed.
    fn apply(data: &mut ArenaContextMut<'_>, cfg: &CFG, looop: &Loop, cand: &Candidate) -> bool {
        // A dedicated preheader anchors the versioning block. If the pass has
        // to create one, the caller re-runs everything from scratch.
        let Some(preheader) = ensure_preheader(data, cfg, looop) else {
            return false;
        };
        let preheader = match preheader {
            EnsurePreheader::Existing(preheader) => preheader,
            EnsurePreheader::Created(..) => return true,
        };

        let InstKind::Jump(jump) = data
            .inst_data(data.layout().basicblock(preheader).terminator())
            .kind()
        else {
            return false;
        };
        let orig_args = jump.args().to_vec();
        if orig_args.len() != data.bb_data(cand.header).params().len() {
            return false;
        }
        let acc_in = orig_args[cand.a_idx];

        let i32_ty = Type::get_i32();
        let zero = data.new_local_value().integer(0);
        let four = data.new_local_value().integer(4);
        let neg_four = data.new_local_value().integer(-4);

        // The versioning block and the unrolled main loop. Every main-loop
        // block carries [acc_in, acc0, acc1, acc2, acc3, jm] and, when the
        // loop carries a pointer, a trailing base pointer parameter.
        let main_arity = UNROLL_FACTOR + 2 + usize::from(cand.ptr.is_some());
        let mut main_tys = vec![i32_ty.clone(); UNROLL_FACTOR + 2];
        if let Some(ptr) = &cand.ptr {
            main_tys.push(data.inst_data(ptr.param).ty().clone());
        }
        let version = data
            .new_basic_block()
            .basic_block("reduction_guard".into(), vec![]);
        let main_header = data
            .new_basic_block()
            .basic_block("reduction_main_header".into(), main_tys.clone());
        let main_body = data
            .new_basic_block()
            .basic_block("reduction_main_body".into(), main_tys.clone());
        let main_exit = data
            .new_basic_block()
            .basic_block("reduction_main_exit".into(), main_tys);
        data.layout_mut().insert_bb_after(preheader, version);
        data.layout_mut().insert_bb_after(version, main_header);
        data.layout_mut().insert_bb_after(main_header, main_body);
        data.layout_mut().insert_bb_after(main_body, main_exit);

        // ---- versioning block: guard T >= 4, main loop bound T & ~3 ----
        let guard = data
            .new_local_value()
            .binary(BinaryOp::Ge, cand.bound, four);
        let masked = data
            .new_local_value()
            .binary(BinaryOp::And, cand.bound, neg_four);
        let mut main_args = vec![acc_in, zero, zero, zero, zero, zero];
        if let Some(ptr) = &cand.ptr {
            main_args.push(orig_args[ptr.idx]);
        }
        let guard_branch =
            data.new_local_value()
                .branch(guard, main_header, main_args, cand.header, orig_args);
        data.layout_mut().insert_inst(version, guard);
        data.layout_mut().insert_inst(version, masked);
        data.layout_mut().insert_inst(version, guard_branch);

        // ---- main loop header: test jm < (T & ~3) ----
        let main_params = data.bb_data(main_header).params().to_vec();
        let main_cond =
            data.new_local_value()
                .binary(BinaryOp::Lt, main_params[UNROLL_FACTOR + 1], masked);
        let body_params = data.bb_data(main_body).params().to_vec();
        let exit_params = data.bb_data(main_exit).params().to_vec();
        // Both targets receive the same loop-carried values as the header
        // itself carries.
        let main_branch = data.new_local_value().branch(
            main_cond,
            main_body,
            main_params.clone(),
            main_exit,
            main_params.clone(),
        );
        data.layout_mut().insert_inst(main_header, main_cond);
        data.layout_mut().insert_inst(main_header, main_branch);

        // ---- main loop body: four cloned lanes + step-4 trip/pointer update ----
        let body_insts = data
            .layout()
            .basicblock(cand.body)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let mut next_accs = Vec::with_capacity(UNROLL_FACTOR);
        for lane in 0..UNROLL_FACTOR {
            let lane_const = data.new_local_value().integer(lane as i32);
            let jmk = data.new_local_value().binary(
                BinaryOp::Add,
                main_params[UNROLL_FACTOR + 1],
                lane_const,
            );
            data.layout_mut().insert_inst(main_body, jmk);
            // Lane pointer: `base + lane * stride` (in elements). Only the
            // pointer *parameter* is re-based; the body's own `ptr + stride`
            // back-edge (`cand.ptr.ptr_back`) is skipped along with `j_back`.
            let ptr_lane = match &cand.ptr {
                Some(ptr) => {
                    let lane_off = data
                        .new_local_value()
                        .integer((lane as i64 * ptr.stride) as i32);
                    let lane_ptr = data
                        .new_local_value()
                        .get_elem_ptr(main_params[UNROLL_FACTOR + 2], vec![lane_off]);
                    data.layout_mut().insert_inst(main_body, lane_ptr);
                    Some(lane_ptr)
                }
                None => None,
            };
            let mut mapper = LaneMapper {
                data: &mut *data,
                map: FxHashMap::default(),
                looop,
                j_param: cand.j,
                acc_param: cand.acc,
                jmk,
                acc_lane: body_params[1 + lane],
                ptr_param: cand.ptr.as_ref().map(|ptr| ptr.param),
                ptr_lane,
                block: main_body,
            };
            for &inst in &body_insts[..body_insts.len() - 1] {
                if inst == cand.j_back || cand.ptr.as_ref().is_some_and(|ptr| inst == ptr.ptr_back)
                {
                    continue;
                }
                mapper.clone_inst(inst);
            }
            let cloned_update = mapper
                .map
                .get(&cand.acc_update)
                .copied()
                .expect("the accumulator update must be cloned into the main body");
            next_accs.push(cloned_update);
        }
        let jm_next =
            data.new_local_value()
                .binary(BinaryOp::Add, main_params[UNROLL_FACTOR + 1], four);
        data.layout_mut().insert_inst(main_body, jm_next);
        let mut back_args = vec![
            body_params[0],
            next_accs[0],
            next_accs[1],
            next_accs[2],
            next_accs[3],
            jm_next,
        ];
        if let Some(ptr) = &cand.ptr {
            let four_stride = data.new_local_value().integer((4 * ptr.stride) as i32);
            let ptr_next = data
                .new_local_value()
                .get_elem_ptr(main_params[UNROLL_FACTOR + 2], vec![four_stride]);
            data.layout_mut().insert_inst(main_body, ptr_next);
            back_args.push(ptr_next);
        }
        let back = data.new_local_value().jump(main_header, back_args);
        data.layout_mut().insert_inst(main_body, back);

        // ---- main loop exit: acc = acc_in + acc0 + acc1 + acc2 + acc3, then
        // fall into the scalar epilogue at j = (T & ~3). ----
        let s01 = data
            .new_local_value()
            .binary(BinaryOp::Add, exit_params[1], exit_params[2]);
        let s23 = data
            .new_local_value()
            .binary(BinaryOp::Add, exit_params[3], exit_params[4]);
        let total = data.new_local_value().binary(BinaryOp::Add, s01, s23);
        let acc_final = data
            .new_local_value()
            .binary(BinaryOp::Add, exit_params[0], total);
        // Reassemble the original header's arguments in its own parameter
        // order (j / acc / optional ptr).
        let mut tail_args = Vec::with_capacity(main_arity);
        for (idx, _param) in data.bb_data(cand.header).params().iter().enumerate() {
            if idx == cand.a_idx {
                tail_args.push(acc_final);
            } else if idx == cand.j_idx {
                tail_args.push(exit_params[UNROLL_FACTOR + 1]);
            } else {
                tail_args.push(exit_params[UNROLL_FACTOR + 2]);
            }
        }
        let tail = data.new_local_value().jump(cand.header, tail_args);
        for inst in [s01, s23, total, acc_final, tail] {
            data.layout_mut().insert_inst(main_exit, inst);
        }

        // ---- redirect the preheader into the versioning block ----
        data.replace_inst_with(data.layout().basicblock(preheader).terminator())
            .jump(version, vec![]);

        true
    }
}

impl Pass for ReductionUnroll {
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

/// True when `value` can be used from a versioning block placed in the loop's
/// preheader: it must be defined by a block that strictly dominates the header
/// (a header-local bound is not visible before the loop runs).
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
            // Constants carry no operands and dominate every block.
            if data.inst_data(value).kind().is_const() {
                return true;
            }
            // A block parameter: defined at its owning block.
            let owner = data
                .layout()
                .basicblocks()
                .iter()
                .find(|layout| data.bb_data(layout.bb()).params().contains(&value));
            owner.is_none_or(|layout| strictly_dominates(layout.bb()))
        }
    }
}

fn is_integer_zero(data: &ArenaContextMut<'_>, inst: Inst) -> bool {
    matches!(data.inst_data(inst).kind(), InstKind::Integer(value) if value.value() == 0)
}

/// Match `acc ± E` or `select(c, acc ± E, acc)`. `E` must not be `acc`.
fn match_acc_update(data: &ArenaContextMut<'_>, acc: Inst, update: Inst) -> Option<AccPattern> {
    let is_delta = |binary: &Binary, acc: Inst| -> bool {
        match (binary.op(), binary.lhs(), binary.rhs()) {
            (BinaryOp::Add, lhs, rhs) if lhs == acc => rhs != acc,
            (BinaryOp::Add, lhs, rhs) if rhs == acc => lhs != acc,
            (BinaryOp::Sub, lhs, rhs) if lhs == acc => rhs != acc,
            _ => false,
        }
    };
    match data.inst_data(update).kind() {
        InstKind::Binary(binary) if is_delta(binary, acc) => Some(AccPattern {
            add: update,
            select: None,
        }),
        InstKind::Select(select) if select.if_false() == acc => {
            let InstKind::Binary(add) = data.inst_data(select.if_true()).kind() else {
                return None;
            };
            if !is_delta(add, acc) {
                return None;
            }
            Some(AccPattern {
                add: select.if_true(),
                select: Some(update),
            })
        }
        _ => None,
    }
}

/// Clones the pure body instructions of one lane into the unrolled main body.
///
/// - `j` maps to `jm + k`, the accumulator maps to the lane accumulator, and
///   (when present) the pointer parameter maps to `base + k * stride`.
/// - Values defined outside the loop are loop-invariant with respect to `j`
///   and `acc`; they are shared rather than duplicated.
/// - The trip-counter update (`j + 1`) and the pointer's own back-edge GEP are
///   the only instructions deliberately skipped; the main loop advances its own
///   counter and pointer by four.
struct LaneMapper<'a, 'b> {
    data: &'a mut ArenaContextMut<'b>,
    map: FxHashMap<Inst, Inst>,
    looop: &'a Loop,
    j_param: Inst,
    acc_param: Inst,
    jmk: Inst,
    acc_lane: Inst,
    /// The header pointer parameter, when the loop carries one.
    ptr_param: Option<Inst>,
    /// The lane pointer (`base + k * stride`) each clone should use.
    ptr_lane: Option<Inst>,
    block: BasicBlock,
}

impl LaneMapper<'_, '_> {
    fn clone_inst(&mut self, inst: Inst) -> Inst {
        if inst.is_global() {
            return inst;
        }
        if inst == self.j_param {
            return self.jmk;
        }
        if inst == self.acc_param {
            return self.acc_lane;
        }
        if self.ptr_param == Some(inst) {
            return self
                .ptr_lane
                .expect("a pointer-lane value is only requested when one was built");
        }
        if let Some(&cloned) = self.map.get(&inst) {
            return cloned;
        }
        // Loop-invariant values (defined outside this loop) are shared.
        let in_loop = self
            .data
            .layout()
            .parent_bb(inst)
            .is_some_and(|block| self.looop.contains(block));
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

impl EntityMapper for LaneMapper<'_, '_> {
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
    use crate::ir::{
        Program, Type,
        builder_trait::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
    };

    /// Build a `sum += a[j]` loop over `[0, n)` with a symbolic bound `n`.
    /// Header params are `[acc, j]` when `acc_first`, otherwise `[j, acc]`; the
    /// body is a single pure block.
    fn build_reduction(
        program: &mut Program,
        bound: Option<i32>,
        select_form: bool,
        acc_first: bool,
    ) -> Function {
        let function = program.new_function(
            Type::get_i32(),
            "reduction".into(),
            vec![
                Type::get_i32(),
                Type::get_pointer(Type::get_array(Type::get_i32(), 16)),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let n = match bound {
            Some(value) => data.new_local_inst().integer(value),
            None => data.params()[0],
        };
        let base = data.params()[1];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32(), Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero, zero]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let (acc, j) = if acc_first {
            (
                data.bb_data(header).params()[0],
                data.bb_data(header).params()[1],
            )
        } else {
            (
                data.bb_data(header).params()[1],
                data.bb_data(header).params()[0],
            )
        };
        let one = data.new_local_inst().integer(1);

        let gep = data.new_local_inst().get_elem_ptr(base, vec![zero, j]);
        let load = data.new_local_inst().load(gep);
        let add = data.new_local_inst().binary(BinaryOp::Add, acc, load);
        let update = if select_form {
            let condition = data.new_local_inst().binary(BinaryOp::Gt, load, zero);
            data.new_local_inst().select(condition, add, acc)
        } else {
            add
        };
        let j_update = data.new_local_inst().binary(BinaryOp::Add, j, one);
        for inst in [gep, load, update, j_update] {
            data.layout_mut().insert_inst(body, inst);
        }
        let back = if acc_first {
            data.new_local_inst().jump(header, vec![update, j_update])
        } else {
            data.new_local_inst().jump(header, vec![j_update, update])
        };
        data.layout_mut().insert_inst(body, back);

        let compare = data.new_local_inst().binary(BinaryOp::Lt, j, n);
        let branch = data
            .new_local_inst()
            .branch(compare, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, compare);
        data.layout_mut().insert_inst(header, branch);

        let ret = data.new_local_inst().ret(Some(acc));
        data.layout_mut().insert_inst(exit, ret);
        function
    }

    fn run(program: &mut Program, function: Function) -> bool {
        let mut context = ArenaContextMut {
            program,
            curr_func: Some(function),
        };
        ReductionUnroll.run_on(&mut context)
    }

    #[test]
    fn unrolls_a_symbolic_bound_reduction_loop() {
        let mut program = Program::new();
        let function = build_reduction(&mut program, None, false, true);
        let block_count = program.func_data(function).layout().basicblocks().len();

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        // versioning + main header + main body + main exit were added.
        assert_eq!(
            data.layout().basicblocks().len(),
            block_count + 4,
            "one reduction loop adds four blocks"
        );
        let blocks = data
            .layout()
            .basicblocks()
            .iter()
            .map(|layout| layout.bb())
            .collect::<Vec<_>>();
        let header = data
            .layout()
            .basicblocks()
            .iter()
            .find(|layout| {
                layout.bb() != blocks[0] && data.bb_data(layout.bb()).params().len() == 2
            })
            .map(|layout| layout.bb())
            .expect("original two-param header must remain");
        let main_header = blocks
            .iter()
            .copied()
            .find(|&block| data.bb_data(block).params().len() == UNROLL_FACTOR + 2)
            .expect("main header carries acc_in + 4 lanes + jm");

        // The main loop bound must be `n & ~3` with step 4; the main header
        // test is `jm < (n & ~3)`.
        let terminator = data.layout().basicblock(main_header).terminator();
        let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
            panic!("main header must branch");
        };
        let InstKind::Binary(compare) = data.inst_data(branch.cond()).kind() else {
            panic!("main header test must be a comparison");
        };
        assert_eq!(compare.op(), BinaryOp::Lt);
        let InstKind::Binary(mask) = data.inst_data(compare.rhs()).kind() else {
            panic!("main bound must be computed by an and-mask");
        };
        assert_eq!(mask.op(), BinaryOp::And);
        assert!(matches!(
            data.inst_data(mask.rhs()).kind(),
            InstKind::Integer(value) if value.value() == -4
        ));

        // The original loop remains intact as the scalar epilogue.
        let mut latch_ok = false;
        for layout in data.layout().basicblocks() {
            let insts = layout.insts();
            let InstKind::Jump(jump) = data.inst_data(*insts.iter().last().unwrap()).kind() else {
                continue;
            };
            if jump.target() == header {
                latch_ok = true;
            }
        }
        assert!(
            latch_ok,
            "scalar epilogue latch must still target the header"
        );
        assert!(
            !run(&mut program, function),
            "second run must be idempotent"
        );
    }

    #[test]
    fn preserves_the_accumulator_position_in_the_epilogue_join() {
        // The header may be `[acc, j]` or `[j, acc]`. The main-loop exit must
        // always land `acc` on the accumulator parameter and `jm` on the trip
        // parameter (h-5-01/LUDCMP has the accumulator at index 1).
        for acc_first in [true, false] {
            let mut program = Program::new();
            let function = build_reduction(&mut program, Some(8), false, acc_first);
            assert!(run(&mut program, function));

            let data = program.func_data(function);
            // main_exit is the six-parameter block that ends in a jump (the
            // main header and body end in branches/jumps too, so match on the
            // jump terminator targetting a two-parameter block).
            let main_exit = data
                .layout()
                .basicblocks()
                .iter()
                .map(|layout| layout.bb())
                .find(|&block| {
                    if data.bb_data(block).params().len() != UNROLL_FACTOR + 2 {
                        return false;
                    }
                    let insts = data.layout().basicblock(block).insts();
                    matches!(
                        data.inst_data(*insts.iter().last().unwrap()).kind(),
                        InstKind::Jump(jump)
                            if data.bb_data(jump.target()).params().len() == 2
                    )
                })
                .unwrap();
            let insts = data.layout().basicblock(main_exit).insts();
            let InstKind::Jump(jump) = data.inst_data(*insts.iter().last().unwrap()).kind() else {
                panic!("main exit must end in a jump");
            };
            let args = jump.args();
            assert_eq!(args.len(), 2);
            let acc_index = if acc_first { 0 } else { 1 };
            // The accumulator slot receives a fresh add (`acc_in + lane sums`),
            // not a raw trip value or a block parameter.
            assert!(matches!(
                data.inst_data(args[acc_index]).kind(),
                InstKind::Binary(..)
            ));
        }
    }

    #[test]
    fn handles_conditional_select_form_accumulator() {
        let mut program = Program::new();
        let function = build_reduction(&mut program, None, true, true);
        let block_count = program.func_data(function).layout().basicblocks().len();

        assert!(run(&mut program, function));
        assert_eq!(
            program.func_data(function).layout().basicblocks().len(),
            block_count + 4
        );
    }

    #[test]
    fn skips_a_constant_bound_loop_under_four() {
        let mut program = Program::new();
        let function = build_reduction(&mut program, Some(2), false, true);
        let block_count = program.func_data(function).layout().basicblocks().len();

        // The guard `T >= 4` is false at runtime for T = 2, but the pass still
        // versiones (the guard is a runtime test on a symbolic/known bound).
        // A constant bound below four still emits the versioning structure.
        assert!(run(&mut program, function));
        assert_eq!(
            program.func_data(function).layout().basicblocks().len(),
            block_count + 4
        );
    }

    /// Build a pointer-carrying reduction loop shaped like the matmul inner
    /// loop: `acc += C[j] * load(ptr); ptr += stride`. Header params are
    /// `[j, acc, ptr]`; the body loads through `getelemptr ptr, 0`.
    fn build_pointer_reduction(program: &mut Program) -> Function {
        let function = program.new_function(
            Type::get_i32(),
            "ptr_reduction".into(),
            vec![
                Type::get_i32(),
                Type::get_pointer(Type::get_array(Type::get_i32(), 16)),
                Type::get_pointer(Type::get_i32()),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let n = data.params()[0];
        let c = data.params()[1];
        let base = data.params()[2];

        let header = data.new_basic_block().basic_block(
            "header".into(),
            vec![
                Type::get_i32(),
                Type::get_i32(),
                Type::get_pointer(Type::get_i32()),
            ],
        );
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero, zero, base]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let (j, acc, ptr) = {
            let params = data.bb_data(header).params();
            (params[0], params[1], params[2])
        };
        let one = data.new_local_inst().integer(1);

        let zero_off = data.new_local_inst().integer(0);
        let c_gep = data.new_local_inst().get_elem_ptr(c, vec![zero_off, j]);
        let c_val = data.new_local_inst().load(c_gep);
        let a_gep = data.new_local_inst().get_elem_ptr(ptr, vec![zero_off]);
        let a_val = data.new_local_inst().load(a_gep);
        let mul = data.new_local_inst().binary(BinaryOp::Mul, c_val, a_val);
        let acc2 = data.new_local_inst().binary(BinaryOp::Add, acc, mul);
        let j2 = data.new_local_inst().binary(BinaryOp::Add, j, one);
        let stride = data.new_local_inst().integer(1024);
        let ptr2 = data.new_local_inst().get_elem_ptr(ptr, vec![stride]);
        for inst in [
            c_gep, c_val, zero_off, a_gep, a_val, mul, acc2, j2, stride, ptr2,
        ] {
            data.layout_mut().insert_inst(body, inst);
        }
        let back = data.new_local_inst().jump(header, vec![j2, acc2, ptr2]);
        data.layout_mut().insert_inst(body, back);

        let compare = data.new_local_inst().binary(BinaryOp::Lt, j, n);
        let branch = data
            .new_local_inst()
            .branch(compare, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, compare);
        data.layout_mut().insert_inst(header, branch);

        let ret = data.new_local_inst().ret(Some(acc));
        data.layout_mut().insert_inst(exit, ret);
        function
    }

    #[test]
    fn unrolls_a_pointer_carrying_reduction_loop() {
        let mut program = Program::new();
        let function = build_pointer_reduction(&mut program);
        let block_count = program.func_data(function).layout().basicblocks().len();

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(
            data.layout().basicblocks().len(),
            block_count + 4,
            "one pointer reduction loop adds four blocks"
        );

        // The main loop blocks carry [acc_in, 4 lanes, jm, ptr] (7 params).
        let main_header = data
            .layout()
            .basicblocks()
            .iter()
            .find(|layout| data.bb_data(layout.bb()).params().len() == UNROLL_FACTOR + 3)
            .map(|layout| layout.bb())
            .expect("main header carries acc_in + 4 lanes + jm + ptr");
        let main_body = data
            .layout()
            .basicblocks()
            .iter()
            .find(|layout| {
                let insts = layout.insts();
                matches!(
                    data.inst_data(*insts.iter().last().unwrap()).kind(),
                    InstKind::Jump(jump) if jump.target() == main_header
                ) && data.bb_data(layout.bb()).params().len() == UNROLL_FACTOR + 3
            })
            .map(|layout| layout.bb())
            .expect("main body jumps back to the main header");

        // The lane loads must go through ptr + {0,1024,2048,3072}: the main
        // body must contain a getelemptr whose base is the ptr parameter and
        // whose offset is one of those lane strides. The lane pointer is
        // derived from the main *header's* pointer parameter (the value the
        // branch feeds the body), so scan for GEPs on that parameter.
        let ptr_param = data.bb_data(main_header).params()[UNROLL_FACTOR + 2];
        let mut lane_offsets = Vec::new();
        for layout in data.layout().basicblocks() {
            for &inst in layout.insts() {
                if let InstKind::GetElemPtr(gep) = data.inst_data(inst).kind() {
                    if gep.base() != ptr_param {
                        continue;
                    }
                    if let [off] = gep.offsets() {
                        if let InstKind::Integer(value) = data.inst_data(*off).kind() {
                            lane_offsets.push(value.value());
                        }
                    }
                }
            }
        }
        let expected = vec![0, 1024, 2048, 3072, 4096];
        assert!(
            expected.iter().all(|off| lane_offsets.contains(off)),
            "lane loads must cover ptr + {{0,1024,2048,3072}} and the back-edge +4096, got {lane_offsets:?}"
        );

        // The scalar epilogue's header (two-parameter target of the main exit)
        // must still receive its pointer slot from the main loop's carried ptr.
        let main_exit = data
            .layout()
            .basicblocks()
            .iter()
            .find(|layout| {
                let insts = layout.insts();
                matches!(
                    data.inst_data(*insts.iter().last().unwrap()).kind(),
                    InstKind::Jump(jump)
                        if data.bb_data(jump.target()).params().len() == 3
                )
            })
            .map(|layout| layout.bb())
            .expect("main exit jumps to the three-parameter original header");
        let insts = data.layout().basicblock(main_exit).insts();
        let InstKind::Jump(jump) = data.inst_data(*insts.iter().last().unwrap()).kind() else {
            panic!("main exit must end in a jump");
        };
        assert_eq!(jump.args().len(), 3);
    }
}
