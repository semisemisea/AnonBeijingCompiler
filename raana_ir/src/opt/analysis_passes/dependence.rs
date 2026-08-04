//! M42: Loop-carried dependence analysis for SIMD vectorization.
//!
//! Decides, per natural loop, whether its memory accesses are safe to
//! vectorize (`Vectorizable`), safe only as a recognized reduction
//! (`Reducible`), or must be left scalar (`Forbidden` with a reason).
//!
//! Structure (three layers):
//!   1. M49/M50 alias fast-path: pairs of accesses whose bases are provably
//!      disjoint (`NoAlias`) never reach the access-function layer.
//!   2. Access-function extraction: each GEP index is classified as an affine
//!      `coefficient * iv + invariant` expression (semantics aligned with
//!      `pointer_strength_reduction::classify_index_evolution`, read-only).
//!   3. Dependence tests over the affine functions: exact same-iteration
//!      overlap plus a conservative cross-iteration extrapolation.
//!
//! Conservative-by-construction: anything that cannot be *proven* disjoint is
//! treated as a conflict (宁漏勿错, zero false negatives for NoAlias).

use smallvec::SmallVec;
use rustc_hash::FxHashMap;

use crate::ir::{
    arena::Arena, BasicBlock, Binary, BinaryOp, Function, Inst, InstKind, Program,
};
use crate::ir::function::FunctionData;
use crate::opt::{
    analysis_passes::{
        effects::EffectAnalysis,
        induction_variable::{
            BasicInductionVariable, BasicInductionVariableAnalysis, InductionDirection,
            InductionStep,
        },
        loop_analysis::{Loop, LoopAnalysis},
        memory::{BaseEnv, MemObject, access_size, integer_constant},
        range::{IntRange, RangeAnalysis},
    },
    pass::ArenaContext,
    utils::{
        cfg::CFG,
        gep::gep_index_stride,
        logical_edge::incoming_edges,
    },
};

/// Access direction of a memory operation inside the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessKind {
    Read,
    Write,
    /// A `MemZero` with a compile-time length: an interval write.
    ZeroFill,
}

/// One memory access expressed as an affine function of the loop's basic
/// induction variable: `byte_coefficient * i + constant_part`, touching
/// `[addr(i), addr(i) + size)` bytes per iteration.
#[derive(Debug, Clone)]
pub struct AccessFunction {
    pub inst: Inst,
    pub kind: AccessKind,
    pub base: MemObject,
    /// Byte size of the access (MemZero: constant length).
    pub size: i64,
    /// Byte stride per loop iteration (sum of `index_coeff * byte_stride`).
    pub byte_coefficient: i64,
    /// Byte range of the iteration-invariant part `[min, max]`. `None` means
    /// unknown (the caller must reject or treat as conflict).
    pub constant_part: Option<(i64, i64)>,
}

/// Recognized integer reduction operators. Only integer reductions are
/// classified; float reassociation is deliberately unsupported (M42 spec §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReductionOp {
    IntAdd,
    IntSub,
    IntMul,
    IntMin,
    IntMax,
}

/// Per-loop verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// No loop-carried dependencies, no unmodelable accesses, no reduction.
    Vectorizable,
    /// The only carried dependency is the recognized accumulator; all other
    /// accesses passed the dependence tests. Vectorize as a lane-accumulator
    /// plus a horizontal `VectorReduce` at exit.
    Reducible { accumulator: Inst, op: ReductionOp },
    /// Not safe to vectorize; `reason` says why.
    Forbidden { reason: ForbidReason },
}

/// Why a loop cannot be vectorized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForbidReason {
    /// The access address resolves to `MemObject::Unknown`.
    UnknownBase(Inst),
    /// A GEP index is not an affine `c*iv + invariant` form (Div/Rem/Select/
    /// Cast/Load result, non-i32, unsupported op).
    NonAffineIndex { access: Inst, index: Inst },
    /// The IV's coefficient is scaled by a runtime value (e.g. `r*N_eff`).
    RuntimeCoefficient { access: Inst },
    /// The invariant part's range is unknown and the test cannot conclude.
    UnprovableConstantPart { access: Inst },
    /// A write-vs-write / read-vs-write conflict inside one iteration.
    IntraIterationConflict { a: Inst, b: Inst },
    /// A write-vs-write / read-vs-write conflict across iterations.
    LoopCarriedConflict { a: Inst, b: Inst },
    /// A `Call`/`TailCall` inside the loop body.
    CallInBody(Inst),
    /// A `MemZero` with a runtime length inside the loop body.
    DynamicMemZero(Inst),
    /// The loop directly contains an inner loop (not an innermost loop).
    NestedLoopBody,
    /// The loop has no basic induction variable (cannot model trip/accesses).
    NoInductionVariable,
}

/// Result of the analysis for one loop.
#[derive(Debug, Clone)]
pub struct LoopDependence {
    pub verdict: Verdict,
    /// Extracted accesses; M44 consumes these for contiguity/stride checks.
    pub accesses: Vec<AccessFunction>,
}

/// Whole-function dependence analysis, keyed by loop header.
pub struct DependenceAnalysis {
    by_header: FxHashMap<BasicBlock, LoopDependence>,
}

impl DependenceAnalysis {
    /// Builds a fresh analysis. The fixed-point manager re-invokes the
    /// consumer pass after every IR change, so a stale snapshot is never
    /// reused (same pattern as LICM rebuilding `EffectAnalysis`).
    pub fn new(
        program: &Program,
        func: Function,
        effects: &EffectAnalysis,
        _vf: u32,
    ) -> Self {
        let arena = ArenaContext {
            program,
            curr_func: Some(func),
        };
        let data = program.func_data(func);
        let (cfg, _dom, loops) = LoopAnalysis::new(data);
        let induction = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        let ranges = RangeAnalysis::new(&arena, &cfg, &loops, &induction);
        let env = effects.env_of(func);

        let mut by_header = FxHashMap::default();
        for looop in loops.loops() {
            let dep = analyze_loop(&arena, &cfg, &loops, looop, &induction, &ranges, env);
            by_header.insert(looop.header(), dep);
        }
        Self { by_header }
    }

    pub fn for_loop(&self, looop: &Loop) -> Option<&LoopDependence> {
        self.by_header.get(&looop.header())
    }
}

// ---------------------------------------------------------------------------
// Access-function extraction
// ---------------------------------------------------------------------------

/// Element-level affine classification of one GEP index.
pub(crate) struct IndexInfo {
    /// Coefficient of the loop's BIV (0 = invariant).
    pub(crate) coefficient: i64,
    /// Invariant part element range `[min, max]`.
    pub(crate) offset_range: (i64, i64),
}

pub(crate) fn classify_index(
    arena: &ArenaContext<'_>,
    looop: &Loop,
    iv: Inst,
    ranges: &RangeAnalysis,
    gep: Inst,
    value: Inst,
) -> Result<IndexInfo, ForbidReason> {
    if value == iv {
        return Ok(IndexInfo {
            coefficient: 1,
            offset_range: (0, 0),
        });
    }
    let data = arena.curr_func_data();
    let is_loop_outside = value.is_global()
        || data
            .layout()
            .parent_bb(value)
            .is_none_or(|block| !looop.contains(block));
    if is_loop_outside {
        // Constants carry an exact value; other loop-outside values take
        // their range analysis result (unknown -> UnprovableConstantPart).
        if let Some(constant) = integer_constant(arena, value) {
            return Ok(IndexInfo {
                coefficient: 0,
                offset_range: (i64::from(constant), i64::from(constant)),
            });
        }
        let range = ranges.range_before(gep, value);
        let (min, max) = match range {
            IntRange::Empty => {
                return Err(ForbidReason::UnprovableConstantPart { access: value });
            }
            IntRange::Bounded { min, max, .. } => (i64::from(min), i64::from(max)),
        };
        return Ok(IndexInfo {
            coefficient: 0,
            offset_range: (min, max),
        });
    }
    if !data.inst_data(value).ty().is_i32() {
        return Err(ForbidReason::NonAffineIndex {
            access: gep,
            index: value,
        });
    }
    let InstKind::Binary(binary) = data.inst_data(value).kind() else {
        return Err(ForbidReason::NonAffineIndex {
            access: gep,
            index: value,
        });
    };

    let classify = |v: Inst| classify_index(arena, looop, iv, ranges, gep, v);
    let classify_bin = |binary: &Binary| -> Result<IndexInfo, ForbidReason> {
        match binary.op() {
            BinaryOp::Add => {
                let lhs = classify(binary.lhs())?;
                let rhs = classify(binary.rhs())?;
                Ok(IndexInfo {
                    coefficient: lhs.coefficient + rhs.coefficient,
                    offset_range: (
                        lhs.offset_range.0 + rhs.offset_range.0,
                        lhs.offset_range.1 + rhs.offset_range.1,
                    ),
                })
            }
            BinaryOp::Sub => {
                let lhs = classify(binary.lhs())?;
                let rhs = classify(binary.rhs())?;
                Ok(IndexInfo {
                    coefficient: lhs.coefficient - rhs.coefficient,
                    offset_range: (
                        lhs.offset_range.0 - rhs.offset_range.1,
                        lhs.offset_range.1 - rhs.offset_range.0,
                    ),
                })
            }
            BinaryOp::Mul | BinaryOp::Shl => {
                // One side must be a compile-time constant factor; the other
                // side is classified. A runtime factor scaling the IV cannot
                // be modeled as a constant coefficient -> RuntimeCoefficient.
                let const_of = |v: Inst| integer_constant(arena, v);
                let (factor, other) = match (const_of(binary.lhs()), const_of(binary.rhs())) {
                    (Some(c), _) => (i64::from(c), binary.rhs()),
                    (None, Some(c)) => (i64::from(c), binary.lhs()),
                    (None, None) => {
                        // Both sides dynamic: if one side depends on the IV and
                        // the other is loop-outside, this is a runtime-scaled
                        // coefficient; otherwise it is not affine at all.
                        let lhs = classify(binary.lhs());
                        let rhs = classify(binary.rhs());
                        match (lhs, rhs) {
                            (Ok(l), Ok(r)) if l.coefficient != 0 && r.coefficient == 0 => {
                                return Err(ForbidReason::RuntimeCoefficient { access: gep });
                            }
                            (Ok(l), Ok(r)) if r.coefficient != 0 && l.coefficient == 0 => {
                                return Err(ForbidReason::RuntimeCoefficient { access: gep });
                            }
                            _ => {
                                return Err(ForbidReason::NonAffineIndex {
                                    access: gep,
                                    index: value,
                                });
                            }
                        }
                    }
                };
                let mut inner = classify(other)?;
                if binary.op() == BinaryOp::Shl {
                    // `v << c` == `v * 2^c`.
                    if !(0..=31).contains(&factor) {
                        return Err(ForbidReason::NonAffineIndex {
                            access: gep,
                            index: value,
                        });
                    }
                    let scale = 1i64 << factor;
                    inner.coefficient *= scale;
                    inner.offset_range = (
                        inner.offset_range.0 * scale,
                        inner.offset_range.1 * scale,
                    );
                    Ok(inner)
                } else {
                    inner.coefficient *= factor;
                    inner.offset_range = scale_range(inner.offset_range, factor);
                    Ok(inner)
                }
            }
            _ => Err(ForbidReason::NonAffineIndex {
                access: gep,
                index: value,
            }),
        }
    };

    classify_bin(binary)
}

fn scale_range((min, max): (i64, i64), factor: i64) -> (i64, i64) {
    if factor >= 0 {
        (min * factor, max * factor)
    } else {
        (max * factor, min * factor)
    }
}

/// Extract the access function of one memory address by walking its GEP chain.
/// Every index is classified relative to the loop BIV (or, when `iv` is
/// `None`, only loop-outside/constant indices are accepted).
fn extract_access(
    arena: &ArenaContext<'_>,
    looop: &Loop,
    iv: Option<Inst>,
    ranges: &RangeAnalysis,
    inst: Inst,
    addr: Inst,
    kind: AccessKind,
    base: MemObject,
    size: i64,
) -> Result<AccessFunction, ForbidReason> {
    let mut coefficient = 0i64;
    let mut off_min = 0i64;
    let mut off_max = 0i64;
    let mut cur = addr;
    loop {
        let InstKind::GetElemPtr(gep) = arena.inst_data(cur).kind() else {
            break;
        };
        for (pos, &index) in gep.offsets().iter().enumerate() {
            let stride = gep_index_stride(arena, cur, pos)
                .ok_or(ForbidReason::NonAffineIndex {
                    access: addr,
                    index,
                })?
                .byte_stride;
            let info = match iv {
                Some(iv) => classify_index(arena, looop, iv, ranges, cur, index)?,
                None => {
                    // No BIV: only loop-outside/constant indices are modelable.
                    let data = arena.curr_func_data();
                    let outside = index.is_global()
                        || data
                            .layout()
                            .parent_bb(index)
                            .is_none_or(|block| !looop.contains(block));
                    if !outside {
                        return Err(ForbidReason::NonAffineIndex {
                            access: addr,
                            index,
                        });
                    }
                    let constant = integer_constant(arena, index);
                    let (min, max) = match constant {
                        Some(c) => (i64::from(c), i64::from(c)),
                        None => match ranges.range_before(cur, index) {
                            IntRange::Empty => {
                                return Err(ForbidReason::UnprovableConstantPart {
                                    access: index,
                                });
                            }
                            IntRange::Bounded { min, max, .. } => {
                                (i64::from(min), i64::from(max))
                            }
                        },
                    };
                    IndexInfo {
                        coefficient: 0,
                        offset_range: (min, max),
                    }
                }
            };
            coefficient += info.coefficient * stride;
            off_min += info.offset_range.0 * stride;
            off_max += info.offset_range.1 * stride;
        }
        cur = gep.base();
    }
    Ok(AccessFunction {
        inst,
        kind,
        base,
        size,
        byte_coefficient: coefficient,
        constant_part: Some((off_min, off_max)),
    })
}

// ---------------------------------------------------------------------------
// Read-only trip-count estimation
// ---------------------------------------------------------------------------

/// Mirror of `induction_variable::normalize_strict_exit`'s acceptance
/// criteria, but with only `&FunctionData` (layout) and `&impl Arena`
/// (instructions) so the dependence analysis can stay in the read-only phase.
/// `induction_variable`'s own version requires `&ArenaContextMut`; duplicating
/// the ~30 lines avoids forcing the analysis to take a mutable context.
struct ExitInfo {
    direction: InductionDirection,
    signed_step: i32,
    bound: Inst,
}

pub(crate) fn normalize_exit_ro(
    data: &FunctionData,
    arena: &impl Arena,
    looop: &Loop,
    iv: &BasicInductionVariable,
) -> Option<ExitInfo> {
    let signed_step = match iv.step() {
        InductionStep::Add(step) => integer_constant(arena, step)?,
        InductionStep::Sub(step) => integer_constant(arena, step)?.checked_neg()?,
    };
    let direction = match signed_step.cmp(&0) {
        std::cmp::Ordering::Greater => InductionDirection::Forward,
        std::cmp::Ordering::Less => InductionDirection::Backward,
        std::cmp::Ordering::Equal => return None,
    };

    let terminator = data.layout().basicblock(looop.header()).terminator();
    let InstKind::Branch(branch) = arena.inst_data(terminator).kind() else {
        return None;
    };
    let true_inside = looop.contains(branch.t_target());
    let false_inside = looop.contains(branch.f_target());
    if true_inside == false_inside {
        return None;
    }

    let InstKind::Binary(compare) = arena.inst_data(branch.cond()).kind() else {
        return None;
    };
    if !arena.inst_data(compare.lhs()).ty().is_i32()
        || !arena.inst_data(compare.rhs()).ty().is_i32()
    {
        return None;
    }

    let mut op = compare.op();
    if !true_inside {
        op = op.complement_integer_compare()?;
    }
    let bound = if compare.lhs() == iv.parameter() {
        compare.rhs()
    } else if compare.rhs() == iv.parameter() {
        op = op.swap_compare_args()?;
        compare.lhs()
    } else {
        return None;
    };

    match (direction, op) {
        (InductionDirection::Forward, BinaryOp::Lt)
        | (InductionDirection::Backward, BinaryOp::Gt) => {}
        _ => return None,
    };

    if signed_step.unsigned_abs() != 1 {
        let bound = i64::from(integer_constant(arena, bound)?);
        let signed_step = i64::from(signed_step);
        let no_wrap = match direction {
            InductionDirection::Forward => bound + signed_step - 1 <= i64::from(i32::MAX),
            InductionDirection::Backward => bound + signed_step + 1 >= i64::from(i32::MIN),
        };
        if !no_wrap {
            return None;
        }
    }

    Some(ExitInfo {
        direction,
        signed_step,
        bound,
    })
}

/// Conservative trip-count upper bound from a normalized exit. `None` means
/// "unknown": the caller then uses the full i32 domain.
pub(crate) fn trip_count_upper_bound(
    arena: &impl Arena,
    data: &FunctionData,
    looop: &Loop,
    iv: &BasicInductionVariable,
) -> Option<i64> {
    let exit = normalize_exit_ro(data, arena, looop, iv)?;
    let bound = i64::from(integer_constant(arena, exit.bound)?);
    let step = i64::from(exit.signed_step);
    let initials = iv
        .initial_values()
        .iter()
        .filter_map(|&value| integer_constant(arena, value))
        .map(i64::from)
        .collect::<Vec<_>>();
    let (init_min, init_max) = (initials.iter().copied().min()?, initials.iter().copied().max()?);
    // Forward: farthest trip from the smallest initial. Backward: from the
    // largest initial.
    let (base, step) = if step > 0 {
        (bound - init_min, step)
    } else {
        (init_max - bound, -step)
    };
    if step <= 0 || base < 0 {
        return None;
    }
    Some((base + step - 1) / step)
}

// ---------------------------------------------------------------------------
// Dependence tests
// ---------------------------------------------------------------------------

/// `∃ i ∈ [0, t): d1*i < c1 && d2*i < c2`, integer arithmetic.
fn solve_linear_system(d1: i64, c1: i64, d2: i64, c2: i64, t: i64) -> bool {
    if t <= 0 {
        return false;
    }
    let mut hi = t - 1;
    for (d, c) in [(d1, c1), (d2, c2)] {
        if d > 0 {
            // i < c/d  ⇔  i <= ceil(c/d) - 1.  For c <= 0 truncating division
            // equals ceil (negative numerator), so both branches are safe.
            let ceil = if c > 0 { (c + d - 1) / d } else { c / d };
            hi = hi.min(ceil - 1);
        } else if d < 0 {
            // d*i < c: d*i is maximized at the largest i in the remaining
            // domain, so the constraint is satisfiable iff d*hi < c.
            if d * hi >= c {
                return false;
            }
        } else if c <= 0 {
            // 0 < c must hold for every i.
            return false;
        }
    }
    hi >= 0
}

/// Do the two access functions overlap within the *same* iteration `i`?
fn same_iteration_may_overlap(
    a: &AccessFunction,
    b: &AccessFunction,
    trip: i64,
) -> bool {
    let (ka_min, ka_max) = match a.constant_part {
        Some(range) => range,
        None => return true,
    };
    let (kb_min, kb_max) = match b.constant_part {
        Some(range) => range,
        None => return true,
    };
    let ca = a.byte_coefficient;
    let cb = b.byte_coefficient;
    // a: [ca*i + ka_min, ca*i + ka_max + a.size)
    // b: [cb*i + kb_min, cb*i + kb_max + b.size)
    // Overlap ⇔ ca*i + ka_min < cb*i + kb_max + b.size
    //        && cb*i + kb_min < ca*i + ka_max + a.size
    let d1 = ca - cb;
    let c1 = kb_max + b.size - ka_min;
    let d2 = cb - ca;
    let c2 = ka_max + a.size - kb_min;
    solve_linear_system(d1, c1, d2, c2, trip)
}

/// Conservative cross-iteration overlap: extrapolate each access over the
/// whole iteration domain and check the two intervals. This may reject loops
/// that are actually safe (no false negatives), which is the accepted
/// trade-off for the analysis (宁漏勿错).
fn cross_iteration_may_overlap(
    a: &AccessFunction,
    b: &AccessFunction,
    trip: i64,
) -> bool {
    let (ka_min, ka_max) = match a.constant_part {
        Some(range) => range,
        None => return true,
    };
    let (kb_min, kb_max) = match b.constant_part {
        Some(range) => range,
        None => return true,
    };
    let last = trip.saturating_sub(1) as i128;
    let ext = |coeff: i64, (min, max): (i64, i64), size: i64| -> (i128, i128) {
        let coeff = coeff as i128;
        let lo = min as i128 + if coeff >= 0 { 0 } else { coeff * last };
        let hi = max as i128 + size as i128 + if coeff > 0 { coeff * last } else { 0 };
        (lo, hi)
    };
    let (a_lo, a_hi) = ext(a.byte_coefficient, (ka_min, ka_max), a.size);
    let (b_lo, b_hi) = ext(b.byte_coefficient, (kb_min, kb_max), b.size);
    a_lo < b_hi && b_lo < a_hi
}

/// Test one pair of accesses. Read-read pairs are always harmless (loads may
/// alias without observable effect). Anything involving a write must be
/// proven disjoint or the loop is forbidden.
fn test_access_pair(
    a: &AccessFunction,
    b: &AccessFunction,
    trip: i64,
) -> Result<(), ForbidReason> {
    if a.kind == AccessKind::Read && b.kind == AccessKind::Read {
        return Ok(());
    }
    if same_iteration_may_overlap(a, b, trip) {
        return Err(ForbidReason::IntraIterationConflict {
            a: a.inst,
            b: b.inst,
        });
    }
    if cross_iteration_may_overlap(a, b, trip) {
        return Err(ForbidReason::LoopCarriedConflict {
            a: a.inst,
            b: b.inst,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Reduction recognition
// ---------------------------------------------------------------------------

/// Match `acc ± E` / `acc * E` / `min(acc, E)` etc., or
/// `select(c, acc op E, acc)`. `E` must not be `acc`.
/// The reduction operation when `update` combines `acc` with an expression:
/// `acc op E`, `E op acc` (binary), `select(cond, acc op E, acc)`, or a
/// phi-join block parameter whose incoming arms are `acc` (passthrough) or
/// `acc op E`. Returns `None` when the shape is not a reduction.
fn match_acc_update_op(
    arena: &ArenaContext<'_>,
    acc: Inst,
    update: Inst,
) -> Option<ReductionOp> {
    let data = arena.curr_func_data();
    let is_delta = |binary: &Binary| -> Option<ReductionOp> {
        let (op, other) = match (binary.op(), binary.lhs(), binary.rhs()) {
            (BinaryOp::Add, lhs, rhs) if lhs == acc && rhs != acc => (BinaryOp::Add, rhs),
            (BinaryOp::Add, lhs, rhs) if rhs == acc && lhs != acc => (BinaryOp::Add, lhs),
            (BinaryOp::Sub, lhs, rhs) if lhs == acc && rhs != acc => (BinaryOp::Sub, rhs),
            (BinaryOp::Mul, lhs, rhs) if lhs == acc && rhs != acc => (BinaryOp::Mul, rhs),
            (BinaryOp::Mul, lhs, rhs) if rhs == acc && lhs != acc => (BinaryOp::Mul, lhs),
            (BinaryOp::Min, lhs, rhs) if lhs == acc && rhs != acc => (BinaryOp::Min, rhs),
            (BinaryOp::Min, lhs, rhs) if rhs == acc && lhs != acc => (BinaryOp::Min, lhs),
            (BinaryOp::Max, lhs, rhs) if lhs == acc && rhs != acc => (BinaryOp::Max, rhs),
            (BinaryOp::Max, lhs, rhs) if rhs == acc && lhs != acc => (BinaryOp::Max, lhs),
            _ => return None,
        };
        let _ = other; // E is only required to differ from acc.
        Some(match op {
            BinaryOp::Add => ReductionOp::IntAdd,
            BinaryOp::Sub => ReductionOp::IntSub,
            BinaryOp::Mul => ReductionOp::IntMul,
            BinaryOp::Min => ReductionOp::IntMin,
            BinaryOp::Max => ReductionOp::IntMax,
            _ => unreachable!(),
        })
    };
    match data.inst_data(update).kind() {
        InstKind::Binary(binary) => is_delta(binary),
        InstKind::Select(select) if select.if_false() == acc => {
            let InstKind::Binary(binary) = data.inst_data(select.if_true()).kind() else {
                return None;
            };
            is_delta(binary)
        }
        InstKind::BlockArgRef(_) => {
            // Phi-join reduction (if-branch + join block): the back-edge
            // value is a block parameter whose incoming arms are `acc`
            // (passthrough) or `acc op E`. At least one arm must update.
            let Some(param_block) = data
                .layout()
                .basicblocks()
                .iter()
                .find(|l| data.bb_data(l.bb()).params().contains(&update))
                .map(|l| l.bb())
            else {
                return None;
            };
            let param_pos = data
                .bb_data(param_block)
                .params()
                .iter()
                .position(|&p| p == update)
                .unwrap();
            let preds: Vec<Inst> = data
                .bb_data(param_block)
                .used_by()
                .iter()
                .copied()
                .collect();
            if preds.is_empty() {
                return None;
            }
            let mut op: Option<ReductionOp> = None;
            let mut saw_update = false;
            for pred_inst in preds {
                let args: Vec<Inst> = match data.inst_data(pred_inst).kind() {
                    InstKind::Jump(jump) => jump.args().to_vec(),
                    InstKind::Branch(branch) if branch.t_target() == param_block => {
                        branch.t_args().to_vec()
                    }
                    InstKind::Branch(branch) if branch.f_target() == param_block => {
                        branch.f_args().to_vec()
                    }
                    _ => return None,
                };
                let &arg = args.get(param_pos)?;
                if arg == acc {
                    continue; // Passthrough arm.
                }
                let InstKind::Binary(binary) = data.inst_data(arg).kind() else {
                    return None;
                };
                let arm_op = is_delta(binary)?;
                if let Some(prev) = op {
                    if prev != arm_op {
                        return None;
                    }
                }
                op = Some(arm_op);
                saw_update = true;
            }
            if saw_update {
                op
            } else {
                None // All arms passthrough: invariant parameter, not a reduction.
            }
        }
        _ => None,
    }
}

/// Find a reduction accumulator among the header parameters. Requires exactly
/// one latch (multi-latch loops are left scalar) and that the accumulator is
/// not used anywhere in the body outside its own update chain.
fn identify_reduction(
    arena: &ArenaContext<'_>,
    cfg: &CFG,
    looop: &Loop,
) -> Option<(Inst, ReductionOp)> {
    let data = arena.curr_func_data();
    let header = looop.header();
    let params = data.bb_data(header).params();
    if params.len() < 2 {
        return None;
    }
    let mut backedge_args: Option<Vec<Inst>> = None;
    for edge in incoming_edges(data, cfg, header) {
        if looop.contains(edge.source()) {
            if backedge_args.is_some() {
                return None; // Multiple latches: conservative.
            }
            backedge_args = Some(edge.args(data).to_vec());
        }
    }
    let backedge_args = backedge_args?;

    for (index, &acc) in params.iter().enumerate() {
        if !data.inst_data(acc).ty().is_i32() {
            continue;
        }
        let update = *backedge_args.get(index)?;
        let Some(op) = match_acc_update_op(arena, acc, update) else {
            continue;
        };
        // The accumulator must not be used by any body instruction outside
        // the update chain (the update + its select wrapper, or the phi-join
        // arms for a block-parameter update).
        let mut chain: SmallVec<[Inst; 4]> = SmallVec::new();
        chain.push(update);
        if let InstKind::Select(select) = data.inst_data(update).kind() {
            chain.push(select.if_true());
        }
        if let InstKind::BlockArgRef(_) = data.inst_data(update).kind() {
            // Phi-join: the update block's incoming arms that consume `acc`
            // (the `acc op E` computations) are part of the chain.
            if let Some(param_block) = data
                .layout()
                .basicblocks()
                .iter()
                .find(|l| data.bb_data(l.bb()).params().contains(&update))
                .map(|l| l.bb())
            {
                for pred_inst in data.bb_data(param_block).used_by().iter().copied().collect::<Vec<_>>() {
                    let args: Vec<Inst> = match data.inst_data(pred_inst).kind() {
                        InstKind::Jump(jump) => jump.args().to_vec(),
                        InstKind::Branch(branch) if branch.t_target() == param_block => {
                            branch.t_args().to_vec()
                        }
                        InstKind::Branch(branch) if branch.f_target() == param_block => {
                            branch.f_args().to_vec()
                        }
                        _ => continue,
                    };
                    for arg in args {
                        if arg != update && data.inst_data(arg).inst_usage().any(|u| u == acc) {
                            chain.push(arg);
                        }
                    }
                }
            }
        }
        let mut used_elsewhere = false;
        // For a phi-join update, the edges passing `acc` into the join block
        // are part of the reduction (the passthrough arm); only the phi
        // block itself needs to be known to skip them.
        let phi_block: Option<BasicBlock> = if let InstKind::BlockArgRef(_) =
            data.inst_data(update).kind()
        {
            data.layout()
                .basicblocks()
                .iter()
                .find(|l| data.bb_data(l.bb()).params().contains(&update))
                .map(|l| l.bb())
        } else {
            None
        };
        for &bb in looop.body() {
            for &inst in data.layout().basicblock(bb).insts() {
                if chain.contains(&inst) {
                    continue;
                }
                let is_phi_edge = matches!(
                    (data.inst_data(inst).kind(), phi_block),
                    (InstKind::Jump(jump), Some(block)) if jump.target() == block,
                ) || matches!(
                    (data.inst_data(inst).kind(), phi_block),
                    (InstKind::Branch(branch), Some(block))
                        if branch.t_target() == block || branch.f_target() == block,
                );
                for used in data.inst_data(inst).inst_usage() {
                    if used == acc && !is_phi_edge {
                        used_elsewhere = true;
                    }
                }
            }
        }
        if !used_elsewhere {
            return Some((acc, op));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Per-loop analysis
// ---------------------------------------------------------------------------

fn forbidden(reason: ForbidReason) -> LoopDependence {
    LoopDependence {
        verdict: Verdict::Forbidden { reason },
        accesses: Vec::new(),
    }
}

fn analyze_loop(
    arena: &ArenaContext<'_>,
    cfg: &CFG,
    loops: &LoopAnalysis,
    looop: &Loop,
    induction: &BasicInductionVariableAnalysis,
    ranges: &RangeAnalysis,
    env: &BaseEnv,
) -> LoopDependence {
    let data = arena.curr_func_data();
    let header = looop.header();

    // Phase 0: structural filters.
    for other in loops.loops() {
        if other.header() != header && looop.contains(other.header()) {
            return forbidden(ForbidReason::NestedLoopBody);
        }
    }
    let bivs = induction.for_loop(looop);
    let Some(biv) = bivs.first() else {
        return forbidden(ForbidReason::NoInductionVariable);
    };
    let iv = biv.parameter();
    let trip: i64 = trip_count_upper_bound(arena, data, looop, biv)
        .unwrap_or(i32::MAX as i64); // Unknown trip: conservative domain.

    // Phase 0/1: collect accesses, extract access functions.
    let mut accesses: Vec<AccessFunction> = Vec::new();
    for &bb in looop.body() {
        for &inst in data.layout().basicblock(bb).insts() {
            let (kind, addr) = match data.inst_data(inst).kind() {
                InstKind::Load(load) => (AccessKind::Read, load.src()),
                InstKind::Store(store) => (AccessKind::Write, store.dest()),
                InstKind::MemZero(mem_zero) => {
                    use crate::ir::inst_kind::mem_zero::MemZeroLen;
                    let len = match mem_zero.byte_len_len() {
                        MemZeroLen::Const(n) => *n,
                        MemZeroLen::Value(_) => {
                            return forbidden(ForbidReason::DynamicMemZero(inst));
                        }
                    };
                    let size = len as i64;
                    let base = env.base_of(arena, mem_zero.dest());
                    if base.is_unknown() {
                        return forbidden(ForbidReason::UnknownBase(inst));
                    }
                    accesses.push(AccessFunction {
                        inst,
                        kind: AccessKind::ZeroFill,
                        base,
                        size,
                        byte_coefficient: 0,
                        constant_part: Some((0, 0)),
                    });
                    continue;
                }
                InstKind::Call(_) | InstKind::TailCall(_) => {
                    return forbidden(ForbidReason::CallInBody(inst));
                }
                _ => continue,
            };
            let base = env.base_of(arena, addr);
            if base.is_unknown() {
                return forbidden(ForbidReason::UnknownBase(inst));
            }
            let size = access_size(arena, addr);
            match extract_access(arena, looop, Some(iv), ranges, inst, addr, kind, base, size) {
                Ok(access) => accesses.push(access),
                Err(reason) => return forbidden(reason),
            }
        }
    }

    // Phase 2: pairwise dependence tests over same-base accesses. The same
    // instruction is skipped: its own iterations access either distinct
    // addresses (coefficient != 0) or the same address without ordering
    // hazards (coefficient == 0, a store writing its own address repeatedly).
    for i in 0..accesses.len() {
        for j in (i + 1)..accesses.len() {
            let a = &accesses[i];
            let b = &accesses[j];
            if a.base != b.base || a.inst == b.inst {
                continue;
            }
            if let Err(reason) = test_access_pair(a, b, trip) {
                return forbidden(reason);
            }
        }
    }

    // Phase 3: reduction recognition. A reduction is the only accepted
    // loop-carried dependency.
    if let Some((accumulator, op)) = identify_reduction(arena, cfg, looop) {
        return LoopDependence {
            verdict: Verdict::Reducible { accumulator, op },
            accesses,
        };
    }
    LoopDependence {
        verdict: Verdict::Vectorizable,
        accesses,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        Type,
        builder::LocalBuilder,
        builder_trait::*,
    };
    use crate::opt::pass::ArenaContextMut;

    /// A `sum += a[j]` loop over `[0, n)` (header params `[acc, j]`), with a
    /// symbolic or constant bound. Mirrors the reduction_unroll test helper.
    fn build_reduction(
        program: &mut Program,
        bound: Option<i32>,
        select_form: bool,
        extra_use_of_acc: bool,
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

        let acc = data.bb_data(header).params()[0];
        let j = data.bb_data(header).params()[1];
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
        let mut body_insts = vec![gep, load, update, j_update];
        if extra_use_of_acc {
            // A second consumer of the accumulator outside the update chain.
            let store = data
                .new_local_inst()
                .store(acc, base);
            body_insts.insert(2, store);
        }
        for inst in body_insts {
            data.layout_mut().insert_inst(body, inst);
        }
        let back = data.new_local_inst().jump(header, vec![update, j_update]);
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

    fn analyze(program: &Program, function: Function) -> LoopDependence {
        let effects = EffectAnalysis::new(program);
        let analysis = DependenceAnalysis::new(program, function, &effects, 4);
        let data = program.func_data(function);
        let (_cfg, _dom, loops) = LoopAnalysis::new(data);
        let looop = loops.loops().first().expect("one loop");
        analysis.for_loop(looop).expect("analyzed").clone()
    }

    /// matmul1 shape: the reduction flows through an if-branch + join block —
    /// the back-edge value is the join block's parameter, whose arms are
    /// `acc` (passthrough) and `acc + load` (update).
    fn build_phi_reduction(program: &mut Program, passthrough_only: bool) -> Function {
        let function = program.new_function(
            Type::get_i32(),
            "phi_reduction".into(),
            vec![Type::get_pointer(Type::get_array(Type::get_i32(), 16))],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32(), Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let then = data.new_basic_block().basic_block("then".into(), vec![]);
        let join = data.new_basic_block().basic_block("join".into(), vec![Type::get_i32()]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, then, join, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero, zero]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let acc = data.bb_data(header).params()[0];
        let j = data.bb_data(header).params()[1];
        let one = data.new_local_inst().integer(1);
        let n16 = data.new_local_inst().integer(16);

        let gep = data.new_local_inst().get_elem_ptr(base, vec![zero, j]);
        let load = data.new_local_inst().load(gep);
        let cond = data.new_local_inst().binary(BinaryOp::Gt, load, zero);
        let branch = data
            .new_local_inst()
            .branch(cond, then, vec![], join, vec![acc]);
        for inst in [gep, load, cond, branch] {
            data.layout_mut().insert_inst(body, inst);
        }
        let add = data.new_local_inst().binary(BinaryOp::Add, acc, load);
        let then_jump = if passthrough_only {
            // Degenerate arm: join receives `load` (not acc-derived).
            data.new_local_inst().jump(join, vec![load])
        } else {
            data.new_local_inst().jump(join, vec![add])
        };
        for inst in [add, then_jump] {
            data.layout_mut().insert_inst(then, inst);
        }
        let join_param = data.bb_data(join).params()[0];
        let j_update = data.new_local_inst().binary(BinaryOp::Add, j, one);
        let back = data.new_local_inst().jump(header, vec![join_param, j_update]);
        data.layout_mut().insert_inst(join, j_update);
        data.layout_mut().insert_inst(join, back);

        let compare = data.new_local_inst().binary(BinaryOp::Lt, j, n16);
        let header_branch = data
            .new_local_inst()
            .branch(compare, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, compare);
        data.layout_mut().insert_inst(header, header_branch);

        let ret = data.new_local_inst().ret(Some(acc));
        data.layout_mut().insert_inst(exit, ret);
        function
    }

    #[test]
    fn phi_join_reduction_is_reducible() {
        let mut program = Program::new();
        let function = build_phi_reduction(&mut program, false);
        let dep = analyze(&program, function);
        let Verdict::Reducible { accumulator, op } = dep.verdict else {
            panic!("expected Reducible, got {:?}", dep.verdict);
        };
        assert_eq!(op, ReductionOp::IntAdd);
        assert!(program.func_data(function).inst_data(accumulator).ty().is_i32());
    }

    #[test]
    fn phi_join_with_non_acc_arm_is_not_a_reduction() {
        let mut program = Program::new();
        let function = build_phi_reduction(&mut program, true);
        let dep = analyze(&program, function);
        assert!(
            !matches!(dep.verdict, Verdict::Reducible { .. }),
            "a phi arm carrying a non-acc value must not be a reduction"
        );
    }

    #[test]
    fn simple_reduction_is_reducible() {
        let mut program = Program::new();
        let function = build_reduction(&mut program, Some(16), false, false);
        let dep = analyze(&program, function);
        assert!(
            matches!(dep.verdict, Verdict::Reducible { .. }),
            "expected Reducible, got {:?}",
            dep.verdict
        );
        let Verdict::Reducible { op, .. } = dep.verdict else {
            unreachable!()
        };
        assert_eq!(op, ReductionOp::IntAdd);
    }

    #[test]
    fn select_form_reduction_is_reducible() {
        let mut program = Program::new();
        let function = build_reduction(&mut program, Some(16), true, false);
        let dep = analyze(&program, function);
        assert!(
            matches!(dep.verdict, Verdict::Reducible { .. }),
            "expected Reducible, got {:?}",
            dep.verdict
        );
    }

    #[test]
    fn accumulator_used_elsewhere_is_forbidden() {
        let mut program = Program::new();
        let function = build_reduction(&mut program, Some(16), false, true);
        let dep = analyze(&program, function);
        assert!(
            matches!(dep.verdict, Verdict::Forbidden { .. }),
            "expected Forbidden, got {:?}",
            dep.verdict
        );
    }

    #[test]
    fn vectorizable_loop_without_reduction() {
        // Pure elementwise loop over two distinct array params: dst[j] = src[j].
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "copy".into(),
            vec![
                Type::get_pointer(Type::get_array(Type::get_i32(), 16)),
                Type::get_pointer(Type::get_array(Type::get_i32(), 16)),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base_src = data.params()[0];
        let base_dst = data.params()[1];
        let n = data.new_local_inst().integer(16);
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }
        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let j = data.bb_data(header).params()[0];
        let one = data.new_local_inst().integer(1);
        let gep_src = data.new_local_inst().get_elem_ptr(base_src, vec![j]);
        let load = data.new_local_inst().load(gep_src);
        let gep_dst = data.new_local_inst().get_elem_ptr(base_dst, vec![j]);
        let store = data.new_local_inst().store(load, gep_dst);
        let j_update = data.new_local_inst().binary(BinaryOp::Add, j, one);
        for inst in [gep_src, load, gep_dst, store, j_update] {
            data.layout_mut().insert_inst(body, inst);
        }
        let back = data.new_local_inst().jump(header, vec![j_update]);
        data.layout_mut().insert_inst(body, back);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, j, n);
        let branch = data
            .new_local_inst()
            .branch(compare, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, compare);
        data.layout_mut().insert_inst(header, branch);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        let dep = analyze(&program, function);
        assert!(
            matches!(dep.verdict, Verdict::Vectorizable),
            "expected Vectorizable, got {:?}",
            dep.verdict
        );
    }

    #[test]
    fn call_in_body_is_forbidden() {
        let mut program = Program::new();
        let init = program.new_value().zero_init(Type::get_i32());
        let global = program.new_value().global_alloc(init);
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        // Create the callee before mutably borrowing the program.
        let callee = program.new_function(Type::get_unit(), "callee".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let n = data.new_local_inst().integer(16);
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }
        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let j = data.bb_data(header).params()[0];
        let one = data.new_local_inst().integer(1);
        // Build the call through an explicit `LocalBuilder` over the mutable
        // arena context: `FunctionData`-rooted builders cannot resolve the
        // callee type (its `Arena::global` is unimplemented).
        let call = {
            let arena: &mut dyn crate::ir::arena::Arena = &mut data;
            LocalBuilder { arena }.call(callee, vec![])
        };
        let j_update = data.new_local_inst().binary(BinaryOp::Add, j, one);
        for inst in [call, j_update] {
            data.layout_mut().insert_inst(body, inst);
        }
        let back = data.new_local_inst().jump(header, vec![j_update]);
        data.layout_mut().insert_inst(body, back);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, j, n);
        let branch = data
            .new_local_inst()
            .branch(compare, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, compare);
        data.layout_mut().insert_inst(header, branch);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        let dep = analyze(&program, function);
        assert!(
            matches!(
                dep.verdict,
                Verdict::Forbidden {
                    reason: ForbidReason::CallInBody(_)
                }
            ),
            "expected Forbidden(CallInBody), got {:?}",
            dep.verdict
        );
    }

    #[test]
    fn same_base_read_write_is_forbidden() {
        // `g[j] = g[j] + 1` over an array param: load and store share the same
        // base and offset within one iteration -> IntraIterationConflict.
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_pointer(Type::get_array(Type::get_i32(), 16))],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let n = data.new_local_inst().integer(16);
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }
        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let j = data.bb_data(header).params()[0];
        let one = data.new_local_inst().integer(1);
        let gep = data.new_local_inst().get_elem_ptr(base, vec![zero, j]);
        let load = data.new_local_inst().load(gep);
        let add = data.new_local_inst().binary(BinaryOp::Add, load, one);
        let store = data.new_local_inst().store(add, gep);
        let j_update = data.new_local_inst().binary(BinaryOp::Add, j, one);
        for inst in [gep, load, add, store, j_update] {
            data.layout_mut().insert_inst(body, inst);
        }
        let back = data.new_local_inst().jump(header, vec![j_update]);
        data.layout_mut().insert_inst(body, back);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, j, n);
        let branch = data
            .new_local_inst()
            .branch(compare, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, compare);
        data.layout_mut().insert_inst(header, branch);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        let dep = analyze(&program, function);
        assert!(
            matches!(
                dep.verdict,
                Verdict::Forbidden {
                    reason: ForbidReason::IntraIterationConflict { .. }
                }
            ),
            "expected Forbidden(IntraIterationConflict), got {:?}",
            dep.verdict
        );
    }
}
