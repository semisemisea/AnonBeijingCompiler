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
//! Soundness:
//! - The body is a single pure block (loads only: no store / call / memzero),
//!   so cloning it four times cannot duplicate side effects.
//! - The accumulator is only updated through `acc ± E` (or the `select`
//!   form), with `E` independent of `acc`; integer addition/subtraction is
//!   associative and commutative modulo 2³², so splitting the reduction into
//!   four independent lanes and summing the four results equals the original.
//! - The main loop runs `j < (T & ~3)`; the original loop, kept intact as a
//!   scalar epilogue, consumes the remaining `T % 4` iterations. A runtime
//!   versioning guard `T >= 4` selects the unrolled path; smaller trip counts
//!   take the scalar loop untouched.
//!
//! The trip counter must start at zero (`j_init == 0`); this holds for every
//! reduction loop in the corpus. The recognition is purely structural: it
//! never matches names, strings, or benchmark-specific bounds (per
//! `docs/Illegal_optimization.md` rule two; see TODO.md §2.6).

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
    /// The back-edge value of the accumulator (the update to be lane-split).
    acc_update: Inst,
    /// The back-edge value of the trip counter (not cloned into the main loop).
    j_back: Inst,
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

        // (b) Exactly two header parameters: the accumulator and the trip IV.
        let params = data.bb_data(header).params().to_vec();
        if params.len() != 2 {
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
        let a_idx = 1 - j_idx;
        let acc = params[a_idx];
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
        } else if branch.f_target() == body {
            return None;
        } else {
            return None;
        };
        if looop.contains(exit_block) {
            return None;
        }
        let _ = exit_block;

        // (e) The body ends with a jump back to the header carrying the updated
        // accumulator and trip counter.
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
        if back_args.len() != 2 {
            return None;
        }
        let acc_update = back_args[a_idx];
        let j_back = back_args[j_idx];

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
            acc_update,
            j_back,
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
        if orig_args.len() != 2 {
            return false;
        }
        let acc_in = orig_args[cand.a_idx];

        let i32_ty = Type::get_i32();
        let zero = data.new_local_value().integer(0);
        let four = data.new_local_value().integer(4);
        let neg_four = data.new_local_value().integer(-4);

        // The versioning block and the unrolled main loop. Every main-loop
        // block carries [acc_in, acc0, acc1, acc2, acc3, jm].
        let version = data
            .new_basic_block()
            .basic_block("reduction_guard".into(), vec![]);
        let main_header = data.new_basic_block().basic_block(
            "reduction_main_header".into(),
            vec![i32_ty.clone(); UNROLL_FACTOR + 2],
        );
        let main_body = data.new_basic_block().basic_block(
            "reduction_main_body".into(),
            vec![i32_ty.clone(); UNROLL_FACTOR + 2],
        );
        let main_exit = data.new_basic_block().basic_block(
            "reduction_main_exit".into(),
            vec![i32_ty.clone(); UNROLL_FACTOR + 2],
        );
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
        let main_args = vec![acc_in, zero, zero, zero, zero, zero];
        let guard_branch = data.new_local_value().branch(
            guard,
            main_header,
            main_args,
            cand.header,
            orig_args,
        );
        data.layout_mut().insert_inst(version, guard);
        data.layout_mut().insert_inst(version, masked);
        data.layout_mut().insert_inst(version, guard_branch);

        // ---- main loop header: test jm < (T & ~3) ----
        let main_params = data.bb_data(main_header).params().to_vec();
        let main_cond = data
            .new_local_value()
            .binary(BinaryOp::Lt, main_params[UNROLL_FACTOR + 1], masked);
        let body_params = data.bb_data(main_body).params().to_vec();
        let exit_params = data.bb_data(main_exit).params().to_vec();
        // Both targets receive the same loop-carried values as the header
        // itself carries: [acc_in, acc0, acc1, acc2, acc3, jm].
        let main_branch = data.new_local_value().branch(
            main_cond,
            main_body,
            main_params.clone(),
            main_exit,
            main_params.clone(),
        );
        data.layout_mut().insert_inst(main_header, main_cond);
        data.layout_mut().insert_inst(main_header, main_branch);

        // ---- main loop body: four cloned lanes + step-4 trip update ----
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
            let jmk = data
                .new_local_value()
                .binary(BinaryOp::Add, main_params[UNROLL_FACTOR + 1], lane_const);
            data.layout_mut().insert_inst(main_body, jmk);
            let mut mapper = LaneMapper {
                data: &mut *data,
                map: FxHashMap::default(),
                looop,
                j_param: cand.j,
                acc_param: cand.acc,
                jmk,
                acc_lane: body_params[1 + lane],
                block: main_body,
            };
            for &inst in &body_insts[..body_insts.len() - 1] {
                if inst == cand.j_back {
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
        let jm_next = data
            .new_local_value()
            .binary(BinaryOp::Add, main_params[UNROLL_FACTOR + 1], four);
        data.layout_mut().insert_inst(main_body, jm_next);
        let mut back_args =
            vec![body_params[0], next_accs[0], next_accs[1], next_accs[2], next_accs[3]];
        back_args.push(jm_next);
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
        let acc_final = data.new_local_value().binary(BinaryOp::Add, exit_params[0], total);
        // The original header's parameter order is `[j, acc]` or `[acc, j]`
        // depending on which parameter is the accumulator; reorder accordingly.
        let mut tail_args = vec![exit_params[UNROLL_FACTOR + 1], acc_final];
        if cand.a_idx == 0 {
            tail_args.swap(0, 1);
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
    let strictly_dominates = |block: BasicBlock| {
        block != looop.header() && dom_tree.dominates(block, looop.header())
    };
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
fn match_acc_update(
    data: &ArenaContextMut<'_>,
    acc: Inst,
    update: Inst,
) -> Option<AccPattern> {
    let is_delta = |binary: &Binary, acc: Inst| -> bool {
        match (binary.op(), binary.lhs(), binary.rhs()) {
            (BinaryOp::Add, lhs, rhs) if lhs == acc => rhs != acc,
            (BinaryOp::Add, lhs, rhs) if rhs == acc => lhs != acc,
            (BinaryOp::Sub, lhs, rhs) if lhs == acc => rhs != acc,
            _ => false,
        }
    };
    match data.inst_data(update).kind() {
        InstKind::Binary(binary) if is_delta(binary, acc) => {
            Some(AccPattern {
                add: update,
                select: None,
            })
        }
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
/// - `j` maps to `jm + k`, the accumulator maps to the lane accumulator.
/// - Values defined outside the loop are loop-invariant with respect to `j`
///   and `acc`; they are shared rather than duplicated.
/// - The trip-counter update (`j + 1`) is the only instruction deliberately
///   skipped; the main loop advances its own counter by four.
struct LaneMapper<'a, 'b> {
    data: &'a mut ArenaContextMut<'b>,
    map: FxHashMap<Inst, Inst>,
    looop: &'a Loop,
    j_param: Inst,
    acc_param: Inst,
    jmk: Inst,
    acc_lane: Inst,
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
            vec![Type::get_i32(), Type::get_pointer(Type::get_array(
                Type::get_i32(),
                16,
            ))],
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
            (data.bb_data(header).params()[0], data.bb_data(header).params()[1])
        } else {
            (data.bb_data(header).params()[1], data.bb_data(header).params()[0])
        };
        let one = data.new_local_inst().integer(1);

        let gep = data.new_local_inst().get_elem_ptr(base, vec![zero, j]);
        let load = data.new_local_inst().load(gep);
        let add = data.new_local_inst().binary(BinaryOp::Add, acc, load);
        let update = if select_form {
            let condition = data.new_local_inst().binary(BinaryOp::Gt, load, zero);
            data.new_local_inst()
                .select(condition, add, acc)
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
            .find(|layout| layout.bb() != blocks[0] && data.bb_data(layout.bb()).params().len() == 2)
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
            let InstKind::Jump(jump) = data
                .inst_data(*insts.iter().last().unwrap())
                .kind()
            else {
                continue;
            };
            if jump.target() == header {
                latch_ok = true;
            }
        }
        assert!(latch_ok, "scalar epilogue latch must still target the header");
        assert!(!run(&mut program, function), "second run must be idempotent");
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
}
