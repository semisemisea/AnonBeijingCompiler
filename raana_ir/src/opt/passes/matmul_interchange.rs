//! Rewrite an in-place matrix multiply `A[i][j] = Σ_k C[i][k] * A[k][j]`
//! (many_mat_cal's hotspot) from the i-j-k loop order to i-k-j with a stack
//! row buffer.
//!
//! ```text
//! for i:  for j:  A[i][j] = Σ_k C[i][k] * A[k][j]
//! ──rewritten to──
//! for i:
//!   buf[0..T) = 0                    // output row buffer (stack T×4B)
//!   for k:
//!     cik = C[i][k]                  // hoisted out of the j loop
//!     for j:  buf[j] += cik * A[k][j]  // A row-contiguous
//!   for j:  A[i][j] = buf[j]         // whole row, contiguous write
//! ```
//!
//! Why: the pristine inner k loop reads `A[k][j]` through a column pointer
//! that steps `row_stride` elements (4096 bytes) per trip, so every load
//! misses the cache. After the interchange the innermost loop walks `A[k][j]`
//! for a fixed `k` — one contiguous row.
//!
//! In-place correctness (no alias-check guard is needed):
//! - `k < i`: rows `k` were already written by earlier `i` iterations (new
//!   values); the rewrite reads them from `A` in the same state.
//! - `k == i`: the accumulator row `i` has not been written yet (the buffer is
//!   only spilled after the whole k loop), so `A[i][j]` still holds the old
//!   value — exactly as the original `k == i` term requires.
//! - `k > i`: untouched old rows.
//! - Each `(i, j)` accumulation order (over `k`) is unchanged, so i32
//!   overflow semantics are bit-for-bit preserved.
//!
//! Recognition is purely structural (never matches names/benchmarks):
//! an outer loop whose body computes `Crow = gep Croot (0,i)` and
//! `Arow = gep Aroot (0,i)`, a middle j loop whose body builds the column
//! pointer `gep Aroot (0,0) → (0,j)`, and an inner pointer-carrying reduction
//! `acc += load(Crow[k]) * load(ptr); ptr += row_stride` whose exit block
//! stores `acc` to `A[i][j]`.

use itertools::Itertools;

use crate::opt::{
    analysis_passes::{
        dom_tree::v2::DominanceTree,
        induction_variable::{
            BasicInductionVariableAnalysis, InductionDirection, normalize_strict_exit,
        },
        loop_analysis::{Loop, LoopAnalysis},
    },
    prelude::*,
    utils::cfg::CFG,
};

pub struct MatmulInterchange;

/// Everything needed to rewrite one matmul nest.
struct Candidate {
    // i loop
    i_header: BasicBlock,
    i_latch: BasicBlock,
    i_iv: Inst,
    // j loop
    j_header: BasicBlock,
    j_body: BasicBlock,
    j_latch: BasicBlock,
    // k loop
    k_header: BasicBlock,
    k_body: BasicBlock,
    // shared bound
    bound: Inst,
    // C row (`gep Croot (0,i)`) and A row (`gep Aroot (0,i)`)
    crow: Inst,
    arow: Inst,
    // the root A matrix
    aroot: Inst,
    /// Row length of A (elements), used to size the row buffer.
    row_len: usize,
}

impl MatmulInterchange {
    fn find_candidate(
        data: &ArenaContextMut<'_>,
        cfg: &CFG,
        _dom_tree: &DominanceTree,
        loop_analysis: &LoopAnalysis,
        k_loop: &Loop,
    ) -> Option<Candidate> {
        // ---- inner k loop: a pointer-carrying reduction ----
        let k_header = k_loop.header();
        if k_loop.body().len() != 2 || k_loop.latches().len() != 1 {
            return None;
        }
        let k_body = k_loop
            .body()
            .iter()
            .copied()
            .find(|&block| block != k_header)?;
        if k_loop.latches()[0] != k_body || !data.bb_data(k_body).params().is_empty() {
            return None;
        }
        let k_params = data.bb_data(k_header).params().to_vec();
        if k_params.len() != 3 {
            return None;
        }

        let biv_analysis = BasicInductionVariableAnalysis::new(data, cfg, loop_analysis);
        let k_bivs = biv_analysis.for_loop(k_loop);
        if k_bivs.len() != 1 {
            return None;
        }
        let k_biv = &k_bivs[0];
        let k_exit = normalize_strict_exit(data, k_loop, k_biv)?;
        if k_exit.direction() != InductionDirection::Forward || k_exit.signed_step() != 1 {
            return None;
        }
        let k_iv = k_biv.parameter();
        let k_idx = k_params.iter().position(|&param| param == k_iv)?;
        let mut acc_param = None;
        let mut ptr_param = None;
        for (idx, &param) in k_params.iter().enumerate() {
            if idx == k_idx {
                continue;
            }
            if data.inst_data(param).ty().is_pointer() {
                ptr_param = Some(param);
            } else if data.inst_data(param).ty().is_i32() {
                acc_param = Some(param);
            } else {
                return None;
            }
        }
        let acc_param = acc_param?;
        let ptr_param = ptr_param?;
        let acc_idx = k_params.iter().position(|&p| p == acc_param)?;
        let ptr_idx = k_params.iter().position(|&p| p == ptr_param)?;

        // The body must be the pure GEMM reduction.
        let k_body_insts = data
            .layout()
            .basicblock(k_body)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let back_term = *k_body_insts.last()?;
        let InstKind::Jump(back) = data.inst_data(back_term).kind() else {
            return None;
        };
        if back.target() != k_header {
            return None;
        }
        let back_args = back.args().to_vec();
        if back_args.len() != 3 {
            return None;
        }
        let acc_update = back_args[acc_idx];
        let ptr_back = back_args[ptr_idx];

        // ptr back-edge: `getelemptr ptr, const_stride`.
        let InstKind::GetElemPtr(ptr_gep) = data.inst_data(ptr_back).kind() else {
            return None;
        };
        if ptr_gep.base() != ptr_param {
            return None;
        }
        let [stride_off] = ptr_gep.offsets() else {
            return None;
        };
        let InstKind::Integer(stride) = data.inst_data(*stride_off).kind() else {
            return None;
        };
        if stride.value() <= 0 || data.layout().parent_bb(ptr_back) != Some(k_body) {
            return None;
        }

        // acc update: `add acc, mul(load(gep Crow (0,k)), load(gep ptr 0))`.
        let InstKind::Binary(add) = data.inst_data(acc_update).kind() else {
            return None;
        };
        if add.op() != BinaryOp::Add {
            return None;
        }
        let (mul_inst, acc_side) = if add.lhs() == acc_param {
            (add.rhs(), add.lhs())
        } else if add.rhs() == acc_param {
            (add.lhs(), add.rhs())
        } else {
            return None;
        };
        if acc_side != acc_param {
            return None;
        }
        let InstKind::Binary(mul) = data.inst_data(mul_inst).kind() else {
            return None;
        };
        if mul.op() != BinaryOp::Mul {
            return None;
        }
        let (c_load, a_load) = match (
            data.inst_data(mul.lhs()).kind(),
            data.inst_data(mul.rhs()).kind(),
        ) {
            (InstKind::Load(lhs), InstKind::Load(rhs)) => (lhs.src(), rhs.src()),
            _ => return None,
        };
        // The A side loads through the ptr parameter (`gep ptr, 0`); the C
        // side loads `Crow[k]` (`gep Crow, (0,k)`).
        let (crow, a_gep_base) = match (
            data.inst_data(c_load).kind(),
            data.inst_data(a_load).kind(),
        ) {
            (InstKind::GetElemPtr(c_gep), InstKind::GetElemPtr(a_gep))
                if a_gep.base() == ptr_param && a_gep.offsets().len() == 1 =>
            {
                if c_gep.offsets().len() != 2 || c_gep.offsets()[1] != k_iv {
                    return None;
                }
                (c_gep.base(), a_gep.base())
            }
            _ => return None,
        };
        let _ = a_gep_base;

        // body purity + acc used nowhere else inside the body.
        for &inst in &k_body_insts[..k_body_insts.len() - 1] {
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
            if inst != acc_update && inst != mul_inst {
                for used in data.inst_data(inst).inst_usage() {
                    if used == acc_param {
                        return None;
                    }
                }
            }
        }

        // ---- middle j loop ----
        let k_index = loop_analysis.loop_index(k_header)?;
        let j_index = loop_analysis.parent_loop_index(k_index)?;
        let j_loop = &loop_analysis.loops()[j_index];
        let j_header = j_loop.header();
        let j_bivs = biv_analysis.for_loop(j_loop);
        if j_bivs.len() != 1 {
            return None;
        }
        let j_exit = normalize_strict_exit(data, j_loop, &j_bivs[0])?;
        if j_exit.direction() != InductionDirection::Forward || j_exit.signed_step() != 1 {
            return None;
        }
        let j_iv = j_bivs[0].parameter();
        let j_latch = j_loop.latches().iter().copied().exactly_one().ok()?;
        if !j_loop.contains(j_latch) {
            return None;
        }
        let j_latch_term = data.layout().basicblock(j_latch).terminator();
        let InstKind::Jump(j_back) = data.inst_data(j_latch_term).kind() else {
            return None;
        };
        if j_back.target() != j_header {
            return None;
        }
        // The j-loop body builds the column pointer: it is the loop block
        // (not header/latch, not part of the inner k loop) whose terminator
        // jumps into the k loop.
        let j_body = j_loop
            .body()
            .iter()
            .copied()
            .find(|&block| {
                block != j_header
                    && block != j_latch
                    && !k_loop.contains(block)
                    && matches!(
                        data.inst_data(data.layout().basicblock(block).terminator()).kind(),
                        InstKind::Jump(jump) if jump.target() == k_header
                    )
            })?;

        // ---- outer i loop ----
        let jj_index = loop_analysis.loop_index(j_header)?;
        let i_index = loop_analysis.parent_loop_index(jj_index)?;
        let i_loop = &loop_analysis.loops()[i_index];
        let i_header = i_loop.header();

        // The i IV is the second offset of `crow = gep Croot (0,i)`. This is
        // structural (unlike BIV, which is defeated by the BlockArgRef form of
        // the i-loop's back edge). `crow` is already in hand from the k body.
        let InstKind::GetElemPtr(crow_gep) = data.inst_data(crow).kind() else {
            return None;
        };
        if crow_gep.offsets().len() != 2 {
            return None;
        }
        let i_iv = crow_gep.offsets()[1];
        if !data.bb_data(i_header).params().contains(&i_iv) {
            return None;
        }
        let croot = crow_gep.base();

        // The i-loop exit: a forward `i < bound` branch out of the loop.
        let i_bound = forward_strict_bound(data, i_loop, i_iv)?;
        if !(k_exit.bound() == j_exit.bound() && j_exit.bound() == i_bound) {
            return None;
        }
        let bound = k_exit.bound();

        // The store `store acc → gep Arow (0,j)` lives in the j latch.
        let mut arow = None;
        for &inst in data.layout().basicblock(j_latch).insts() {
            let InstKind::Store(store) = data.inst_data(inst).kind() else {
                continue;
            };
            if store.src() != acc_param {
                continue;
            }
            let InstKind::GetElemPtr(gep) = data.inst_data(store.dest()).kind() else {
                return None;
            };
            if gep.offsets().len() != 2 || gep.offsets()[1] != j_iv {
                return None;
            }
            arow = Some(gep.base());
        }
        let arow = arow?;

        // Arow must be `gep Aroot (0,i)`.
        let InstKind::GetElemPtr(arow_gep) = data.inst_data(arow).kind() else {
            return None;
        };
        let aroot = arow_gep.base();
        if arow_gep.offsets().len() != 2 || arow_gep.offsets()[1] != i_iv {
            return None;
        }
        // Distinct C and A roots.
        if croot == aroot {
            return None;
        }

        // Row length of A from Arow's pointee; pointer stride must equal it.
        let row_len = row_length(data, arow)?;
        if i64::from(stride.value()) != i64::try_from(row_len).ok()? {
            return None;
        }

        // The i-loop latch (i++ → i header).
        let i_latch = i_loop.latches().iter().copied().exactly_one().ok()?;
        if !i_loop.contains(i_latch) {
            return None;
        }

        Some(Candidate {
            i_header,
            i_latch,
            i_iv,
            j_header,
            j_body,
            j_latch,
            k_header,
            k_body,
            bound,
            crow,
            arow,
            aroot,
            row_len,
        })
    }

    fn apply(data: &mut ArenaContextMut<'_>, cand: &Candidate) -> bool {
        let i32_ty = Type::get_i32();
        let zero = data.new_local_value().integer(0);
        let one = data.new_local_value().integer(1);
        let buf_ty = Type::get_array(i32_ty.clone(), cand.row_len);

        // ---- new blocks ----
        let k_header = data
            .new_basic_block()
            .basic_block("mm_k_header".into(), vec![i32_ty.clone()]);
        let k_body = data.new_basic_block().basic_block("mm_k_body".into(), vec![]);
        let k_exit = data.new_basic_block().basic_block("mm_k_exit".into(), vec![]);
        let j_header = data.new_basic_block().basic_block(
            "mm_j_header".into(),
            vec![
                i32_ty.clone(),
                i32_ty.clone(),
                Type::get_pointer(buf_ty.clone()),
            ],
        );
        let j_body = data.new_basic_block().basic_block("mm_j_body".into(), vec![]);
        let j_latch = data.new_basic_block().basic_block("mm_j_latch".into(), vec![]);
        let wb_header = data
            .new_basic_block()
            .basic_block("mm_wb_header".into(), vec![i32_ty.clone()]);
        let wb_body = data.new_basic_block().basic_block("mm_wb_body".into(), vec![]);

        // Insert them after the i-loop body (the block that computes Crow).
        let i_body = data.layout().parent_bb(cand.crow).expect("crow in a block");
        let mut anchor = i_body;
        for block in [
            k_header, k_body, k_exit, j_header, j_body, j_latch, wb_header, wb_body,
        ] {
            data.layout_mut().insert_bb_after(anchor, block);
            anchor = block;
        }

        // ---- the row buffer (zeroed once per i) ----
        let buf = data.new_local_value().alloc(buf_ty);
        let mz = data.new_local_value().mem_zero(buf, cand.row_len * 4);
        data.layout_mut().insert_before_terminator(i_body, buf);
        data.layout_mut().insert_before_terminator(i_body, mz);

        // ---- k loop ----
        // k_header(k): br k < T, k_body, k_exit
        let k = data.bb_data(k_header).params()[0];
        let k_cond = data.new_local_value().binary(BinaryOp::Lt, k, cand.bound);
        let k_branch = data
            .new_local_value()
            .branch(k_cond, k_body, vec![], k_exit, vec![]);
        data.layout_mut().insert_inst(k_header, k_cond);
        data.layout_mut().insert_inst(k_header, k_branch);

        // k_body: arow_k = gep Aroot (0,k); cik = load Crow[k]; jump j(0,cik,arow_k)
        let arow_k = data.new_local_value().get_elem_ptr(cand.aroot, vec![zero, k]);
        let cik_gep = data.new_local_value().get_elem_ptr(cand.crow, vec![zero, k]);
        let cik = data.new_local_value().load(cik_gep);
        let j_init = data.new_local_value().jump(j_header, vec![zero, cik, arow_k]);
        for inst in [arow_k, cik_gep, cik, j_init] {
            data.layout_mut().insert_inst(k_body, inst);
        }

        // k_exit: jump wb_header(0)
        let wb_enter = data.new_local_value().jump(wb_header, vec![zero]);
        data.layout_mut().insert_inst(k_exit, wb_enter);

        // ---- j loop ----
        // j_header(j, cik, arow_k): br j < T, j_body, j_latch
        let j_params = data.bb_data(j_header).params().to_vec();
        let j_cond = data
            .new_local_value()
            .binary(BinaryOp::Lt, j_params[0], cand.bound);
        let j_branch = data
            .new_local_value()
            .branch(j_cond, j_body, vec![], j_latch, vec![]);
        data.layout_mut().insert_inst(j_header, j_cond);
        data.layout_mut().insert_inst(j_header, j_branch);

        // j_body: buf[j] += cik * A[k][j]
        let a_gep = data
            .new_local_value()
            .get_elem_ptr(j_params[2], vec![zero, j_params[0]]);
        let a_val = data.new_local_value().load(a_gep);
        let b_gep = data
            .new_local_value()
            .get_elem_ptr(buf, vec![zero, j_params[0]]);
        let b_val = data.new_local_value().load(b_gep);
        let prod = data.new_local_value().binary(BinaryOp::Mul, j_params[1], a_val);
        let sum = data.new_local_value().binary(BinaryOp::Add, b_val, prod);
        let store = data.new_local_value().store(sum, b_gep);
        let j_next = data.new_local_value().binary(BinaryOp::Add, j_params[0], one);
        let j_back = data
            .new_local_value()
            .jump(j_header, vec![j_next, j_params[1], j_params[2]]);
        for inst in [a_gep, a_val, b_gep, b_val, prod, sum, store, j_next, j_back] {
            data.layout_mut().insert_inst(j_body, inst);
        }

        // j_latch: k+1 → k_header
        let k_next = data.new_local_value().binary(BinaryOp::Add, k, one);
        let k_back = data.new_local_value().jump(k_header, vec![k_next]);
        data.layout_mut().insert_inst(j_latch, k_next);
        data.layout_mut().insert_inst(j_latch, k_back);

        // ---- writeback loop ----
        // wb_header(j): br j < T, wb_body, i_latch
        let wb_j = data.bb_data(wb_header).params()[0];
        let wb_cond = data.new_local_value().binary(BinaryOp::Lt, wb_j, cand.bound);
        let wb_branch = data
            .new_local_value()
            .branch(wb_cond, wb_body, vec![], cand.i_latch, vec![]);
        data.layout_mut().insert_inst(wb_header, wb_cond);
        data.layout_mut().insert_inst(wb_header, wb_branch);

        // wb_body: A[i][j] = buf[j]
        let src_gep = data
            .new_local_value()
            .get_elem_ptr(buf, vec![zero, wb_j]);
        let src_val = data.new_local_value().load(src_gep);
        let dst_gep = data
            .new_local_value()
            .get_elem_ptr(cand.arow, vec![zero, wb_j]);
        let wb_store = data.new_local_value().store(src_val, dst_gep);
        let wb_next = data.new_local_value().binary(BinaryOp::Add, wb_j, one);
        let wb_back = data.new_local_value().jump(wb_header, vec![wb_next]);
        for inst in [src_gep, src_val, dst_gep, wb_store, wb_next, wb_back] {
            data.layout_mut().insert_inst(wb_body, inst);
        }

        // ---- redirect the i-loop body to the new k loop ----
        let i_body_term = data.layout().basicblock(i_body).terminator();
        data.replace_inst_with(i_body_term).jump(k_header, vec![zero]);

        // ---- remove the old j/k-loop blocks (now unreachable) ----
        for block in [cand.j_header, cand.j_body, cand.j_latch, cand.k_header, cand.k_body] {
            data.remove_layout_basicblock(block);
        }

        // The i-loop latch's back edge used to reference values threaded
        // through the removed j-loop (e.g. the old `j`/`k`/`acc` carries).
        // Rewire any now-dangling argument to the i-header's own parameter at
        // that position, which dominates the latch.
        let i_latch_term = data.layout().basicblock(cand.i_latch).terminator();
        let i_params = data.bb_data(cand.i_header).params().to_vec();
        let latch_target = match data.inst_data(i_latch_term).kind() {
            InstKind::Jump(jump) => Some((jump.target(), jump.args().to_vec())),
            _ => None,
        };
        if let Some((target, args)) = latch_target {
            let mut rewired = false;
            let mut args = args;
            for (idx, arg) in args.iter_mut().enumerate() {
                if data.layout().parent_bb(*arg).is_none() {
                    if let Some(&param) = i_params.get(idx) {
                        *arg = param;
                        rewired = true;
                    }
                }
            }
            if rewired {
                data.replace_inst_with(i_latch_term).jump(target, args);
            }
        }

        // The i-latch body may also reference values that lived in the
        // removed j-loop (e.g. its `i = add <pass-through>, 1` uses the
        // j-header's own parameter as the loop-carried i value). Rewire any
        // now-dangling BlockArgRef operand inside the latch block to the
        // i-loop's own induction value, which dominates the latch.
        let latch_insts = data
            .layout()
            .basicblock(cand.i_latch)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for inst in latch_insts {
            let (op, old_lhs, old_rhs) = match data.inst_data(inst).kind() {
                InstKind::Binary(binary) => (binary.op(), binary.lhs(), binary.rhs()),
                _ => continue,
            };
            let is_dangling = |v: Inst| {
                matches!(data.inst_data(v).kind(), InstKind::BlockArgRef(..))
                    && data.layout().parent_bb(v).is_none()
            };
            let lhs = if is_dangling(old_lhs) {
                cand.i_iv
            } else {
                old_lhs
            };
            let rhs = if is_dangling(old_rhs) {
                cand.i_iv
            } else {
                old_rhs
            };
            if lhs != old_lhs || rhs != old_rhs {
                data.replace_inst_with(inst).binary(op, lhs, rhs);
            }
        }

        true
    }
}

/// The row length of the pointee array of a row pointer (e.g. `*[i32;1024]`).
fn row_length(data: &ArenaContextMut<'_>, row_ptr: Inst) -> Option<usize> {
    let ty = data.inst_data(row_ptr).ty();
    if !ty.is_pointer() {
        return None;
    }
    let (_, len) = ty.derefernce().get_array_info();
    Some(len)
}

/// The bound of a forward `iv < bound` header exit. Unlike BIV-based
/// normalization this matches on the header branch's compare directly, so it
/// also works when the back edge threads the IV through a `BlockArgRef`.
fn forward_strict_bound(
    data: &ArenaContextMut<'_>,
    looop: &Loop,
    iv: Inst,
) -> Option<Inst> {
    let terminator = data.layout().basicblock(looop.header()).terminator();
    let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
        return None;
    };
    let true_inside = looop.contains(branch.t_target());
    let false_inside = looop.contains(branch.f_target());
    if true_inside == false_inside {
        return None;
    }
    let InstKind::Binary(compare) = data.inst_data(branch.cond()).kind() else {
        return None;
    };
    if !data.inst_data(compare.lhs()).ty().is_i32() || !data.inst_data(compare.rhs()).ty().is_i32()
    {
        return None;
    }
    let mut op = compare.op();
    if !true_inside {
        op = op.complement_integer_compare()?;
    }
    let bound = if compare.lhs() == iv {
        compare.rhs()
    } else if compare.rhs() == iv {
        op = op.swap_compare_args()?;
        compare.lhs()
    } else {
        return None;
    };
    (op == BinaryOp::Lt).then_some(bound)
}

impl Pass for MatmulInterchange {
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
            if Self::apply(data, &candidate) {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        Program, Type,
        builder_trait::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
    };

    /// Build the many_mat_cal matmul nest: `for i: for j: A[i][j] =
    /// Σ_k C[i][k] * A[k][j]` with globals A, C and a runtime bound n.
    fn build_matmul(program: &mut Program) -> Function {
        let a_ty = Type::get_array(Type::get_array(Type::get_i32(), 64), 64);
        let c_ty = Type::get_array(Type::get_array(Type::get_i32(), 64), 64);
        let a_init = program.new_value().zero_init(a_ty);
        let a_root = program.new_value().global_alloc(a_init);
        let c_init = program.new_value().zero_init(c_ty);
        let c_root = program.new_value().global_alloc(c_init);
        let function = program.new_function(Type::get_unit(), "matmul".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let n = data.params()[0];

        let i_header = data
            .new_basic_block()
            .basic_block("i_header".into(), vec![Type::get_i32()]);
        let i_body = data.new_basic_block().basic_block("i_body".into(), vec![]);
        let i_latch = data.new_basic_block().basic_block("i_latch".into(), vec![]);
        let j_header = data
            .new_basic_block()
            .basic_block("j_header".into(), vec![Type::get_i32(), Type::get_i32()]);
        let j_body = data.new_basic_block().basic_block("j_body".into(), vec![]);
        let j_latch = data.new_basic_block().basic_block("j_latch".into(), vec![]);
        let k_header = data.new_basic_block().basic_block(
            "k_header".into(),
            vec![
                Type::get_i32(),
                Type::get_i32(),
                Type::get_pointer(Type::get_i32()),
            ],
        );
        let k_body = data.new_basic_block().basic_block("k_body".into(), vec![]);
        let i_exit = data.new_basic_block().basic_block("i_exit".into(), vec![]);
        for block in [
            i_header, i_body, i_latch, j_header, j_body, j_latch, k_header, k_body, i_exit,
        ] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let one = data.new_local_inst().integer(1);
        let entry_jump = data.new_local_inst().jump(i_header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let i_iv = data.bb_data(i_header).params()[0];
        let i_cond = data.new_local_inst().binary(BinaryOp::Lt, i_iv, n);
        let i_branch = data
            .new_local_inst()
            .branch(i_cond, i_body, vec![], i_exit, vec![]);
        for inst in [i_cond, i_branch] {
            data.layout_mut().insert_inst(i_header, inst);
        }

        // Global-touching GEPs go through the program-aware context.
        let mut context = ArenaContextMut {
            program,
            curr_func: Some(function),
        };
        // i body: a_base = gep A (0,0); crow = gep C (0,i); arow = gep A (0,i).
        // The i value is threaded through the j header (like the real
        // benchmark): `jump j_header(0, i)`.
        let a_base = context.new_local_value().get_elem_ptr(a_root, vec![zero, zero]);
        let crow = context.new_local_value().get_elem_ptr(c_root, vec![zero, i_iv]);
        let arow = context.new_local_value().get_elem_ptr(a_root, vec![zero, i_iv]);
        let data = context.curr_func_data_mut();
        let i_body_jump = data.new_local_inst().jump(j_header, vec![zero, i_iv]);
        for inst in [a_base, crow, arow, i_body_jump] {
            data.layout_mut().insert_inst(i_body, inst);
        }

        let j_iv = data.bb_data(j_header).params()[0];
        let j_i = data.bb_data(j_header).params()[1];
        let j_cond = data.new_local_inst().binary(BinaryOp::Lt, j_iv, n);
        let j_branch = data
            .new_local_inst()
            .branch(j_cond, j_body, vec![], i_latch, vec![]);
        for inst in [j_cond, j_branch] {
            data.layout_mut().insert_inst(j_header, inst);
        }

        // j body: ptr_init = gep a_base (0,j); jump k_header(0,0,ptr_init)
        let ptr_init = data.new_local_inst().get_elem_ptr(a_base, vec![zero, j_iv]);
        let j_body_jump = data
            .new_local_inst()
            .jump(k_header, vec![zero, zero, ptr_init]);
        for inst in [ptr_init, j_body_jump] {
            data.layout_mut().insert_inst(j_body, inst);
        }

        let (k_iv, acc, ptr) = {
            let p = data.bb_data(k_header).params();
            (p[0], p[1], p[2])
        };
        let k_cond = data.new_local_inst().binary(BinaryOp::Lt, k_iv, n);
        let k_branch = data
            .new_local_inst()
            .branch(k_cond, k_body, vec![], j_latch, vec![]);
        for inst in [k_cond, k_branch] {
            data.layout_mut().insert_inst(k_header, inst);
        }

        // k body: cik = load Crow[k]; a = load ptr; acc2 = acc + cik*a;
        // ptr2 = gep ptr 64; k2 = k+1
        let cik_gep = data.new_local_inst().get_elem_ptr(crow, vec![zero, k_iv]);
        let cik = data.new_local_inst().load(cik_gep);
        let ptr0 = data.new_local_inst().get_elem_ptr(ptr, vec![zero]);
        let a_val = data.new_local_inst().load(ptr0);
        let prod = data.new_local_inst().binary(BinaryOp::Mul, cik, a_val);
        let acc2 = data.new_local_inst().binary(BinaryOp::Add, acc, prod);
        let stride = data.new_local_inst().integer(64);
        let ptr2 = data.new_local_inst().get_elem_ptr(ptr, vec![stride]);
        let k2 = data.new_local_inst().binary(BinaryOp::Add, k_iv, one);
        let k_back = data.new_local_inst().jump(k_header, vec![k2, acc2, ptr2]);
        for inst in [cik_gep, cik, ptr0, a_val, prod, acc2, stride, ptr2, k2, k_back] {
            data.layout_mut().insert_inst(k_body, inst);
        }

        // j latch: store acc → gep arow (0,j); j2 = j+1; jump j_header(j2, j_i)
        let dst = data.new_local_inst().get_elem_ptr(arow, vec![zero, j_iv]);
        let store = data.new_local_inst().store(acc, dst);
        let j2 = data.new_local_inst().binary(BinaryOp::Add, j_iv, one);
        let j_back = data.new_local_inst().jump(j_header, vec![j2, j_i]);
        for inst in [dst, store, j2, j_back] {
            data.layout_mut().insert_inst(j_latch, inst);
        }

        // i latch: i2 = i + 1 (the i value lives in j_header's threaded
        // parameter, which the interchange must rewire after removing the
        // j-loop); jump i_header
        let i2 = data.new_local_inst().binary(BinaryOp::Add, j_i, one);
        let i_back = data.new_local_inst().jump(i_header, vec![i2]);
        for inst in [i2, i_back] {
            data.layout_mut().insert_inst(i_latch, inst);
        }

        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(i_exit, ret);

        function
    }

    fn run(program: &mut Program, function: Function) -> bool {
        let mut context = ArenaContextMut {
            program,
            curr_func: Some(function),
        };
        MatmulInterchange.run_on(&mut context)
    }

    #[test]
    fn interchanges_the_matmul_nest() {
        let mut program = Program::new();
        let function = build_matmul(&mut program);
        let block_count = program.func_data(function).layout().basicblocks().len();

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(
            data.layout().basicblocks().len(),
            block_count + 8 - 5,
            "matmul interchange adds eight blocks and removes the five old j/k blocks"
        );

        let names = data
            .layout()
            .basicblocks()
            .iter()
            .map(|layout| data.bb_data(layout.bb()).name().to_string())
            .collect::<Vec<_>>();
        for expected in ["mm_k_header", "mm_j_header", "mm_wb_header"] {
            assert!(
                names.iter().any(|name| name.contains(expected)),
                "missing {expected}: {names:?}"
            );
        }
        for gone in ["j_body", "j_latch", "k_header", "k_body"] {
            assert!(
                !names.iter().any(|name| name == gone),
                "old {gone} should have been removed: {names:?}"
            );
        }
    }

    #[test]
    fn is_idempotent_on_a_second_run() {
        let mut program = Program::new();
        let function = build_matmul(&mut program);
        assert!(run(&mut program, function));
        // The i-k-j shape no longer matches the i-j-k recogniser.
        assert!(!run(&mut program, function));
    }
}
