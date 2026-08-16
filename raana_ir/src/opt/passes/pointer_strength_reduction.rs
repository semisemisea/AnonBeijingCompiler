use crate::opt::{
    analysis_passes::{
        dom_tree::v2::DominanceTree,
        induction_variable::{
            BasicInductionVariableAnalysis, ConstantInductionRange, InductionStep,
            constant_induction_range, normalize_strict_exit,
        },
        loop_analysis::{Loop, LoopAnalysis},
        range::{IntRange, RangeAnalysis, RangeContext},
    },
    prelude::*,
    utils::{
        cfg::CFG,
        gep::gep_index_stride,
        logical_edge::{
            LogicalEdge, LogicalEdgeRewriter, forwarded_block_params, incoming_edges,
            outgoing_edges, resolve_forwarded_params,
        },
        pointer_strength_reduction_cost::estimate_aarch64_pointer_strength_reduction,
        preheader::{EnsurePreheader, ensure_preheader},
    },
};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

pub struct PointerStrengthReduction;

struct Candidate {
    gep: Inst,
    iv: Inst,
    header_iv_position: usize,
    backedge_groups: SmallVec<[BackedgeGroup; 2]>,
    base: Inst,
    offsets: Vec<Inst>,
    pointer_ty: Type,
    address_evolution: FlattenedAddressEvolution,
    forwarded_params: FxHashMap<Inst, Inst>,
}

#[derive(Clone)]
enum IndexEvolution {
    Invariant,
    Direct,
    Affine(AffineI32Expr),
}

#[derive(Clone)]
struct AffineI32Expr {
    value: Inst,
    coefficient: i64,
    offset_range: I64Range,
    chain: SmallVec<[Inst; 4]>,
    invariants: SmallVec<[Inst; 4]>,
}

struct FlattenedAddressEvolution {
    indices: Vec<IndexEvolution>,
    coefficient: i64,
    pointer_step: i32,
}

#[derive(Clone, Copy)]
struct I64Range {
    min: i64,
    max: i64,
}

impl I64Range {
    fn from_i32(range: IntRange) -> Option<Self> {
        Some(Self {
            min: i64::from(range.min()?),
            max: i64::from(range.max()?),
        })
    }

    fn add(self, other: Self) -> Option<Self> {
        Some(Self {
            min: self.min.checked_add(other.min)?,
            max: self.max.checked_add(other.max)?,
        })
    }

    fn sub(self, other: Self) -> Option<Self> {
        Some(Self {
            min: self.min.checked_sub(other.max)?,
            max: self.max.checked_sub(other.min)?,
        })
    }

    fn mul(self, other: Self) -> Option<Self> {
        let values = [
            self.min.checked_mul(other.min)?,
            self.min.checked_mul(other.max)?,
            self.max.checked_mul(other.min)?,
            self.max.checked_mul(other.max)?,
        ];
        Some(Self {
            min: *values.iter().min().unwrap(),
            max: *values.iter().max().unwrap(),
        })
    }
}

#[derive(Clone)]
struct BackedgeGroup {
    source: BasicBlock,
    edges: SmallVec<[LogicalEdge; 2]>,
}

enum ApplyResult {
    Unchanged,
    Changed,
}

impl PointerStrengthReduction {
    fn find_candidate(
        data: &ArenaContextMut<'_>,
        cfg: &CFG,
        dom_tree: &DominanceTree,
        loops: &LoopAnalysis,
        ivs: &BasicInductionVariableAnalysis,
        ranges: &RangeAnalysis,
        looop: &Loop,
        parameter_blocks: &FxHashMap<Inst, BasicBlock>,
    ) -> Option<Candidate> {
        let backedges = incoming_edges(data, cfg, looop.header())
            .into_iter()
            .filter(|edge| looop.contains(edge.source()))
            .collect::<Vec<_>>();
        if backedges.is_empty() {
            return None;
        }
        let mut backedge_groups = SmallVec::<[BackedgeGroup; 2]>::new();
        for &edge in &backedges {
            if let Some(group) = backedge_groups
                .iter_mut()
                .find(|group| group.source == edge.source())
            {
                group.edges.push(edge);
            } else {
                backedge_groups.push(BackedgeGroup {
                    source: edge.source(),
                    edges: SmallVec::from_slice(&[edge]),
                });
            }
        }

        let mut best = None;
        let header_params = data.bb_data(looop.header()).params().to_vec();
        let forwarded_params = resolve_forwarded_params(&forwarded_block_params(data, cfg));
        // A header block parameter that every backedge passes through unchanged
        // is loop-invariant: its value on the first entry equals its value in
        // every iteration, so the preheader edge argument can substitute for it
        // when the initial pointer is computed.
        let invariant_header_params = header_params
            .iter()
            .enumerate()
            .filter_map(|(position, &parameter)| {
                backedges
                    .iter()
                    .all(|edge| edge.args(data).get(position) == Some(&parameter))
                    .then_some(parameter)
            })
            .collect::<FxHashSet<_>>();
        for iv in ivs.for_loop(looop) {
            // The pointer step only needs the IV's constant update value. The
            // strict-exit normalization additionally provides a constant
            // induction range used to prove that affine (non-direct) index
            // expressions cannot wrap `i32`. Rotated countdown loops whose
            // header no longer compares the IV (the test moved to the latch
            // comparing the countdown counter) still qualify for direct and
            // invariant indices.
            let signed_step = match iv.step() {
                InductionStep::Add(step) => Self::integer_constant(data, step)?,
                InductionStep::Sub(step) => Self::integer_constant(data, step)?.checked_neg()?,
            };
            // Non-unit steps must keep the no-wrap proof from a normalized
            // strict exit with a constant bound: the pointer follows `iv`
            // directly and could otherwise skip past the bound. Unit steps
            // only need a strict exit; the constant range is optional there
            // because a direct or invariant index is carried exactly by the
            // pointer (the affine wrap check still requires it). A loop whose
            // header branch `normalize_strict_exit` rejected (non-strict
            // compare such as `Le`) is refused: the IV can wrap past the
            // bound. Rotated countdown loops move the test to the latch and
            // leave the header as a jump-through block, which is the shape
            // h-5's inner loop has.
            let iv_range = if signed_step.unsigned_abs() != 1 {
                Some(constant_induction_range(
                    data,
                    iv,
                    normalize_strict_exit(data, looop, iv)?,
                )?)
            } else {
                match normalize_strict_exit(data, looop, iv) {
                    Some(exit) => constant_induction_range(data, iv, exit),
                    None => {
                        let header_terminator =
                            data.layout().basicblock(looop.header()).terminator();
                        if matches!(data.inst_data(header_terminator).kind(), InstKind::Jump(..)) {
                            None
                        } else {
                            continue;
                        }
                    }
                }
            };
            let Some(header_iv_position) = data
                .bb_data(looop.header())
                .params()
                .iter()
                .position(|&parameter| parameter == iv.parameter())
            else {
                continue;
            };
            if backedges.iter().any(|edge| {
                edge.args(data)
                    .get(header_iv_position)
                    .is_none_or(|value| !iv.update_values().contains(value))
            }) {
                continue;
            }

            for block_layout in data.layout().basicblocks() {
                let block = block_layout.bb();
                if !looop.contains(block)
                    || loops.min_loop_contain(block).map(Loop::header) != Some(looop.header())
                {
                    continue;
                }
                for &inst in block_layout.insts() {
                    let InstKind::GetElemPtr(gep) = data.inst_data(inst).kind() else {
                        continue;
                    };
                    if !Self::available_at_header(
                        data,
                        dom_tree,
                        looop,
                        parameter_blocks,
                        &invariant_header_params,
                        gep.base(),
                    ) || !Self::has_only_loop_memory_users(data, looop, inst)
                        || !backedge_groups
                            .iter()
                            .all(|group| dom_tree.dominates(block, group.source))
                    {
                        continue;
                    }
                    let mut index_evolutions = Vec::with_capacity(gep.offsets().len());
                    let mut flat_coefficient = 0_i64;
                    let mut removable_chain = FxHashSet::default();
                    let mut valid = true;
                    for (position, &offset) in gep.offsets().iter().enumerate() {
                        let evolution = Self::classify_index_evolution(
                            data,
                            ranges,
                            looop,
                            iv.parameter(),
                            iv_range,
                            &forwarded_params,
                            inst,
                            offset,
                        );
                        let Some(evolution) = evolution else {
                            valid = false;
                            break;
                        };
                        let (coefficient, chain, invariants) = match &evolution {
                            IndexEvolution::Invariant => {
                                if !Self::available_at_header(
                                    data,
                                    dom_tree,
                                    looop,
                                    parameter_blocks,
                                    &invariant_header_params,
                                    offset,
                                ) {
                                    valid = false;
                                    break;
                                }
                                index_evolutions.push(evolution);
                                continue;
                            }
                            IndexEvolution::Direct => (1, None, None),
                            IndexEvolution::Affine(affine) => (
                                affine.coefficient,
                                Some(&affine.chain),
                                Some(&affine.invariants),
                            ),
                        };
                        let Some(stride) = gep_index_stride(data, inst, position) else {
                            valid = false;
                            break;
                        };
                        if !invariants.into_iter().flatten().all(|&invariant| {
                            Self::available_at_header(
                                data,
                                dom_tree,
                                looop,
                                parameter_blocks,
                                &invariant_header_params,
                                invariant,
                            )
                        }) {
                            valid = false;
                            break;
                        }
                        let Some(contribution) =
                            coefficient.checked_mul(i64::from(stride.result_element_stride))
                        else {
                            valid = false;
                            break;
                        };
                        let Some(coefficient) = flat_coefficient.checked_add(contribution) else {
                            valid = false;
                            break;
                        };
                        flat_coefficient = coefficient;
                        if let Some(chain) = chain {
                            removable_chain.extend(chain.iter().copied());
                        }
                        index_evolutions.push(evolution);
                    }
                    if !valid || flat_coefficient == 0 {
                        continue;
                    }
                    let Some(index_delta) = i64::from(signed_step).checked_mul(flat_coefficient)
                    else {
                        continue;
                    };
                    let Some(signed_pointer_step) = i32::try_from(index_delta).ok() else {
                        continue;
                    };
                    let result_element_size = match data.inst_data(inst).ty().kind() {
                        crate::ir::TypeKind::Pointer(element) => i64::try_from(element.size()).ok(),
                        _ => None,
                    };
                    let Some(signed_byte_delta) =
                        result_element_size.and_then(|size| index_delta.checked_mul(size))
                    else {
                        continue;
                    };
                    // Runtime-bound loops (unknown iv_range) skip the i32
                    // intermediate-range proof: the incremental scheme only
                    // performs 64-bit pointer adds, so the protected 32-bit
                    // computation no longer exists. Guard the per-iteration
                    // byte step instead so a pathological stride cannot
                    // blow up the address span or the immediate encoding.
                    if iv_range.is_none() && signed_byte_delta.unsigned_abs() > (1 << 20) {
                        continue;
                    }
                    let removable_derived_insts = removable_chain
                        .iter()
                        .filter(|&&derived| {
                            Self::only_reaches_candidate(
                                data,
                                derived,
                                inst,
                                &removable_chain,
                                &forwarded_params,
                                &mut FxHashSet::default(),
                            )
                        })
                        .count();
                    let derived_setup_insts = index_evolutions
                        .iter()
                        .map(|evolution| match evolution {
                            IndexEvolution::Affine(affine) => affine.chain.len(),
                            IndexEvolution::Invariant | IndexEvolution::Direct => 0,
                        })
                        .sum();
                    let Some(cost) = estimate_aarch64_pointer_strength_reduction(
                        data,
                        cfg,
                        looop,
                        gep,
                        signed_byte_delta,
                        removable_derived_insts,
                        derived_setup_insts,
                    ) else {
                        continue;
                    };
                    if !cost.is_profitable() {
                        continue;
                    }
                    let candidate = Candidate {
                        gep: inst,
                        iv: iv.parameter(),
                        header_iv_position,
                        backedge_groups: backedge_groups.clone(),
                        base: gep.base(),
                        offsets: gep.offsets().to_vec(),
                        pointer_ty: data.inst_data(inst).ty().clone(),
                        address_evolution: FlattenedAddressEvolution {
                            indices: index_evolutions,
                            coefficient: flat_coefficient,
                            pointer_step: signed_pointer_step,
                        },
                        forwarded_params: forwarded_params.clone(),
                    };
                    if best
                        .as_ref()
                        .is_none_or(|(best_cost, _)| cost.is_better_than(*best_cost))
                    {
                        best = Some((cost, candidate));
                    }
                }
            }
        }
        best.map(|(_, candidate)| candidate)
    }

    fn available_at_header(
        data: &ArenaContextMut<'_>,
        dom_tree: &DominanceTree,
        looop: &Loop,
        parameter_blocks: &FxHashMap<Inst, BasicBlock>,
        invariant_header_params: &FxHashSet<Inst>,
        value: Inst,
    ) -> bool {
        if value.is_global() || data.inst_data(value).kind().is_const() {
            return true;
        }
        if invariant_header_params.contains(&value) {
            return true;
        }
        match data.layout().parent_bb(value) {
            Some(block) => !looop.contains(block) && dom_tree.dominates(block, looop.header()),
            None if matches!(data.inst_data(value).kind(), InstKind::BlockArgRef(..)) => {
                parameter_blocks.get(&value).is_some_and(|&block| {
                    !looop.contains(block)
                        && dom_tree.contains(block)
                        && dom_tree.dominates(block, looop.header())
                })
            }
            None => false,
        }
    }

    fn classify_index_evolution(
        data: &ArenaContextMut<'_>,
        ranges: &RangeAnalysis,
        looop: &Loop,
        iv: Inst,
        iv_range: Option<ConstantInductionRange>,
        forwarded_params: &FxHashMap<Inst, Inst>,
        gep: Inst,
        value: Inst,
    ) -> Option<IndexEvolution> {
        let value = forwarded_params.get(&value).copied().unwrap_or(value);
        if value == iv {
            return Some(IndexEvolution::Direct);
        }
        if value.is_global()
            || data.inst_data(value).kind().is_const()
            || data
                .layout()
                .parent_bb(value)
                .is_none_or(|block| !looop.contains(block))
        {
            return Some(IndexEvolution::Invariant);
        }

        fn classify(
            data: &ArenaContextMut<'_>,
            ranges: &RangeAnalysis,
            looop: &Loop,
            iv: Inst,
            iv_range: Option<ConstantInductionRange>,
            forwarded_params: &FxHashMap<Inst, Inst>,
            gep: Inst,
            value: Inst,
        ) -> Option<AffineI32Expr> {
            let value = forwarded_params.get(&value).copied().unwrap_or(value);
            if value == iv {
                return Some(AffineI32Expr {
                    value,
                    coefficient: 1,
                    offset_range: I64Range { min: 0, max: 0 },
                    chain: SmallVec::new(),
                    invariants: SmallVec::new(),
                });
            }
            if value.is_global()
                || data.inst_data(value).kind().is_const()
                || data
                    .layout()
                    .parent_bb(value)
                    .is_none_or(|block| !looop.contains(block))
            {
                return Some(AffineI32Expr {
                    value,
                    coefficient: 0,
                    offset_range: I64Range::from_i32(ranges.range_before(gep, value))?,
                    chain: SmallVec::new(),
                    invariants: if data.inst_data(value).kind().is_const() {
                        SmallVec::new()
                    } else {
                        SmallVec::from_slice(&[value])
                    },
                });
            }
            if !data.inst_data(value).ty().is_i32() {
                return None;
            }
            let InstKind::Binary(binary) = data.inst_data(value).kind() else {
                return None;
            };
            let lhs = classify(
                data,
                ranges,
                looop,
                iv,
                iv_range,
                forwarded_params,
                gep,
                binary.lhs(),
            )?;
            let rhs = classify(
                data,
                ranges,
                looop,
                iv,
                iv_range,
                forwarded_params,
                gep,
                binary.rhs(),
            )?;
            let (coefficient, offset_range) = match binary.op() {
                BinaryOp::Add => (
                    lhs.coefficient.checked_add(rhs.coefficient)?,
                    lhs.offset_range.add(rhs.offset_range)?,
                ),
                BinaryOp::Sub => (
                    lhs.coefficient.checked_sub(rhs.coefficient)?,
                    lhs.offset_range.sub(rhs.offset_range)?,
                ),
                // A product of two loop-invariant values is itself an
                // invariant offset. Keep the product in the affine chain so
                // `clone_affine_initial` moves it to the preheader. This is
                // the common flattened-row form `row * runtime_width + iv`.
                BinaryOp::Mul if lhs.coefficient == 0 && rhs.coefficient == 0 => {
                    (0, lhs.offset_range.mul(rhs.offset_range)?)
                }
                BinaryOp::Mul if lhs.coefficient == 0 => {
                    let factor = PointerStrengthReduction::integer_constant(data, binary.lhs())?;
                    (
                        rhs.coefficient.checked_mul(i64::from(factor))?,
                        rhs.offset_range.mul(I64Range {
                            min: i64::from(factor),
                            max: i64::from(factor),
                        })?,
                    )
                }
                BinaryOp::Mul if rhs.coefficient == 0 => {
                    let factor = PointerStrengthReduction::integer_constant(data, binary.rhs())?;
                    (
                        lhs.coefficient.checked_mul(i64::from(factor))?,
                        lhs.offset_range.mul(I64Range {
                            min: i64::from(factor),
                            max: i64::from(factor),
                        })?,
                    )
                }
                BinaryOp::Shl if rhs.coefficient == 0 => {
                    let shift = PointerStrengthReduction::integer_constant(data, binary.rhs())?;
                    let factor = 1_i64.checked_shl(u32::try_from(shift).ok()?)?;
                    (
                        lhs.coefficient.checked_mul(factor)?,
                        lhs.offset_range.mul(I64Range {
                            min: factor,
                            max: factor,
                        })?,
                    )
                }
                _ => return None,
            };
            let depends_on_iv = lhs.coefficient != 0 || rhs.coefficient != 0;
            // With a constant induction range, both the intermediate wrap
            // proof and the i32 range fit keep the 32-bit offset arithmetic
            // sound. With a runtime bound (iv_range unknown) both checks are
            // skipped: the incremental scheme replaces that arithmetic with
            // 64-bit pointer adds, and the valid-input domain excludes i32
            // wraparound, so the incremental address sequence matches the
            // original linear one.
            if depends_on_iv
                && iv_range.is_some_and(|range| {
                    !ranges.proves_binary_no_signed_wrap(
                        binary.op(),
                        binary.lhs(),
                        binary.rhs(),
                        RangeContext::Before(gep),
                    ) || !PointerStrengthReduction::affine_range_fits_i32(
                        coefficient,
                        offset_range,
                        range,
                    )
                })
            {
                return None;
            }
            let mut chain = lhs.chain;
            for inst in rhs.chain {
                if !chain.contains(&inst) {
                    chain.push(inst);
                }
            }
            if !chain.contains(&value) {
                chain.push(value);
            }
            let mut invariants = lhs.invariants;
            for invariant in rhs.invariants {
                if !invariants.contains(&invariant) {
                    invariants.push(invariant);
                }
            }
            Some(AffineI32Expr {
                value,
                coefficient,
                offset_range,
                chain,
                invariants,
            })
        }

        let affine = classify(
            data,
            ranges,
            looop,
            iv,
            iv_range,
            forwarded_params,
            gep,
            value,
        )?;
        (affine.coefficient != 0).then_some(IndexEvolution::Affine(affine))
    }

    fn affine_range_fits_i32(
        coefficient: i64,
        offset: I64Range,
        iv: ConstantInductionRange,
    ) -> bool {
        let iv_min = i64::from(iv.min());
        let iv_max = i64::from(iv.max());
        let (scaled_min, scaled_max) = if coefficient >= 0 {
            (
                coefficient.checked_mul(iv_min),
                coefficient.checked_mul(iv_max),
            )
        } else {
            (
                coefficient.checked_mul(iv_max),
                coefficient.checked_mul(iv_min),
            )
        };
        let (Some(scaled_min), Some(scaled_max)) = (scaled_min, scaled_max) else {
            return false;
        };
        let Some(min) = scaled_min.checked_add(offset.min) else {
            return false;
        };
        let Some(max) = scaled_max.checked_add(offset.max) else {
            return false;
        };
        min >= i64::from(i32::MIN) && max <= i64::from(i32::MAX)
    }

    fn integer_constant(data: &ArenaContextMut<'_>, value: Inst) -> Option<i32> {
        match data.inst_data(value).kind() {
            InstKind::Integer(integer) => Some(integer.value()),
            _ => None,
        }
    }

    fn evaluate_affine_initial(
        data: &ArenaContextMut<'_>,
        affine: &AffineI32Expr,
        iv: Inst,
        initial_iv: i32,
        forwarded_params: &FxHashMap<Inst, Inst>,
    ) -> Option<i32> {
        fn evaluate(
            data: &ArenaContextMut<'_>,
            chain: &[Inst],
            iv: Inst,
            initial_iv: i32,
            forwarded_params: &FxHashMap<Inst, Inst>,
            value: Inst,
        ) -> Option<i32> {
            let value = forwarded_params.get(&value).copied().unwrap_or(value);
            if value == iv {
                return Some(initial_iv);
            }
            if !chain.contains(&value) {
                return PointerStrengthReduction::integer_constant(data, value);
            }
            let InstKind::Binary(binary) = data.inst_data(value).kind() else {
                return None;
            };
            let lhs = evaluate(data, chain, iv, initial_iv, forwarded_params, binary.lhs())?;
            let rhs = evaluate(data, chain, iv, initial_iv, forwarded_params, binary.rhs())?;
            Some(match binary.op() {
                BinaryOp::Add => lhs.wrapping_add(rhs),
                BinaryOp::Sub => lhs.wrapping_sub(rhs),
                BinaryOp::Mul => lhs.wrapping_mul(rhs),
                BinaryOp::Shl => lhs.wrapping_shl(rhs as u32),
                _ => return None,
            })
        }

        evaluate(
            data,
            &affine.chain,
            iv,
            initial_iv,
            forwarded_params,
            affine.value,
        )
    }

    fn clone_affine_initial(
        data: &mut ArenaContextMut<'_>,
        preheader: BasicBlock,
        affine: &AffineI32Expr,
        iv: Inst,
        initial_iv: Inst,
        header_param_positions: &FxHashMap<Inst, usize>,
        edge_args: &[Inst],
        forwarded_params: &FxHashMap<Inst, Inst>,
    ) -> Inst {
        fn clone_value(
            data: &mut ArenaContextMut<'_>,
            preheader: BasicBlock,
            chain: &[Inst],
            iv: Inst,
            initial_iv: Inst,
            header_param_positions: &FxHashMap<Inst, usize>,
            edge_args: &[Inst],
            forwarded_params: &FxHashMap<Inst, Inst>,
            value: Inst,
        ) -> Inst {
            let value = forwarded_params.get(&value).copied().unwrap_or(value);
            if value == iv {
                return initial_iv;
            }
            if !chain.contains(&value) {
                if let Some(&parameter_position) = header_param_positions.get(&value) {
                    return edge_args[parameter_position];
                }
                return value;
            }
            let InstKind::Binary(binary) = data.inst_data(value).kind() else {
                unreachable!("affine chains contain only binary instructions")
            };
            let (op, lhs, rhs) = (binary.op(), binary.lhs(), binary.rhs());
            let lhs = clone_value(
                data,
                preheader,
                chain,
                iv,
                initial_iv,
                header_param_positions,
                edge_args,
                forwarded_params,
                lhs,
            );
            let rhs = clone_value(
                data,
                preheader,
                chain,
                iv,
                initial_iv,
                header_param_positions,
                edge_args,
                forwarded_params,
                rhs,
            );
            let cloned = data.new_local_value().binary(op, lhs, rhs);
            data.layout_mut()
                .insert_before_terminator(preheader, cloned);
            cloned
        }

        clone_value(
            data,
            preheader,
            &affine.chain,
            iv,
            initial_iv,
            header_param_positions,
            edge_args,
            forwarded_params,
            affine.value,
        )
    }

    /// Upper bound on how many nested `getelemptr` levels (an outer GEP used
    /// as the base of an inner GEP) `has_only_loop_memory_users` will follow
    /// before giving up conservatively. Realistic 2D/3D array access chains
    /// are 2-3 levels deep; anything deeper is rejected to bound compile time.
    const MAX_TRANSITIVE_GEP_DEPTH: usize = 8;

    /// Returns whether every use of `gep` inside `looop` is a memory
    /// operation (`Load`/`Store`/`MemZero`) that uses `gep` as its address,
    /// possibly through nested `getelemptr` levels: a GEP that uses `gep` as
    /// its base is accepted iff its own users satisfy the same property. Any
    /// use in a non-address role (stored as data, passed to a call, returned,
    /// ...) anywhere along the chain rejects. This admits the 2D array shape
    /// `b[k][i]` where the outer GEP carries the induction-variable index and
    /// its only user is the inner GEP.
    fn has_only_loop_memory_users(data: &ArenaContextMut<'_>, looop: &Loop, gep: Inst) -> bool {
        fn chain_has_only_loop_memory_users(
            data: &ArenaContextMut<'_>,
            looop: &Loop,
            gep: Inst,
            visited: &mut FxHashSet<Inst>,
            depth: usize,
        ) -> bool {
            // Cycle protection: each GEP is visited at most once. In a
            // well-formed SSA use graph a GEP has a single base, so a revisit
            // can only happen through a base-edge cycle that never reaches a
            // memory operation; reject it conservatively (this also bounds
            // the walk, alongside the depth cap).
            if depth >= PointerStrengthReduction::MAX_TRANSITIVE_GEP_DEPTH || !visited.insert(gep) {
                return false;
            }
            let users = data.inst_data(gep).used_by();
            !users.is_empty()
                && users.iter().all(|&user| {
                    data.layout()
                        .parent_bb(user)
                        .is_some_and(|block| looop.contains(block))
                        && match data.inst_data(user).kind() {
                            InstKind::Load(load) => load.src() == gep,
                            InstKind::Store(store) => store.dest() == gep,
                            InstKind::MemZero(mem_zero) => mem_zero.dest() == gep,
                            // Follow the chain only through the pointer
                            // (base) operand; a GEP used anywhere else is not
                            // an address-only use.
                            InstKind::GetElemPtr(inner) => {
                                inner.base() == gep
                                    && chain_has_only_loop_memory_users(
                                        data,
                                        looop,
                                        user,
                                        visited,
                                        depth + 1,
                                    )
                            }
                            _ => false,
                        }
                })
        }
        chain_has_only_loop_memory_users(data, looop, gep, &mut FxHashSet::default(), 0)
    }

    fn only_reaches_candidate(
        data: &ArenaContextMut<'_>,
        value: Inst,
        gep: Inst,
        chain: &FxHashSet<Inst>,
        forwarded_params: &FxHashMap<Inst, Inst>,
        visiting: &mut FxHashSet<Inst>,
    ) -> bool {
        if !visiting.insert(value) {
            return false;
        }
        let result = data.inst_data(value).used_by().iter().all(|&user| {
            if user == gep {
                return true;
            }
            if chain.contains(&user) {
                return Self::only_reaches_candidate(
                    data,
                    user,
                    gep,
                    chain,
                    forwarded_params,
                    visiting,
                );
            }

            let Some(block) = data.layout().parent_bb(user) else {
                return false;
            };
            if matches!(data.inst_data(user).kind(), InstKind::Branch(branch) if branch.cond() == value)
            {
                return false;
            }
            if !matches!(data.inst_data(user).kind(), InstKind::Jump(..) | InstKind::Branch(..)) {
                return false;
            }

            let mut saw_forward = false;
            for edge in outgoing_edges(data, block) {
                for (position, &argument) in edge.args(data).iter().enumerate() {
                    if argument != value {
                        continue;
                    }
                    let Some(&parameter) = data.bb_data(edge.target(data)).params().get(position)
                    else {
                        return false;
                    };
                    let parameter_end = forwarded_params
                        .get(&parameter)
                        .copied()
                        .unwrap_or(parameter);
                    let value_end = forwarded_params.get(&value).copied().unwrap_or(value);
                    if parameter_end != value_end
                        || !Self::only_reaches_candidate(
                            data,
                            parameter,
                            gep,
                            chain,
                            forwarded_params,
                            visiting,
                        )
                    {
                        return false;
                    }
                    saw_forward = true;
                }
            }
            saw_forward
        });
        visiting.remove(&value);
        result
    }

    fn apply_candidate(
        data: &mut ArenaContextMut<'_>,
        cfg: &CFG,
        looop: &Loop,
        candidate: Candidate,
    ) -> ApplyResult {
        debug_assert_ne!(candidate.address_evolution.coefficient, 0);
        let preheader = match ensure_preheader(data, cfg, looop) {
            Some(EnsurePreheader::Existing(preheader) | EnsurePreheader::Created(preheader)) => {
                preheader
            }
            None => return ApplyResult::Unchanged,
        };

        let entry_edges = outgoing_edges(data, preheader)
            .into_iter()
            .filter(|edge| edge.target(data) == looop.header())
            .collect::<Vec<_>>();
        if entry_edges.is_empty()
            || entry_edges
                .iter()
                .any(|edge| edge.args(data).len() != data.bb_data(looop.header()).params().len())
        {
            return ApplyResult::Unchanged;
        }

        let mut initial_indices_by_edge = Vec::with_capacity(entry_edges.len());
        for edge in entry_edges {
            let initial_iv = edge.args(data)[candidate.header_iv_position];
            let constant_offsets = match data.inst_data(initial_iv).kind() {
                InstKind::Integer(initial) => {
                    let mut offsets = Vec::with_capacity(candidate.offsets.len());
                    for (offset, evolution) in candidate
                        .offsets
                        .iter()
                        .zip(&candidate.address_evolution.indices)
                    {
                        offsets.push(match evolution {
                            IndexEvolution::Invariant => match data.inst_data(*offset).kind() {
                                InstKind::Integer(integer) => Some(integer.value()),
                                _ => None,
                            },
                            IndexEvolution::Direct => Some(initial.value()),
                            IndexEvolution::Affine(affine) => Self::evaluate_affine_initial(
                                data,
                                affine,
                                candidate.iv,
                                initial.value(),
                                &candidate.forwarded_params,
                            ),
                        });
                    }
                    Some(offsets)
                }
                _ => None,
            };
            if let Some(offsets) = constant_offsets
                .as_ref()
                .and_then(|offsets| offsets.iter().copied().collect::<Option<Vec<_>>>())
            {
                if !crate::opt::utils::gep::gep_constant_offsets_fit(data, candidate.base, &offsets)
                {
                    return ApplyResult::Unchanged;
                }
            }
            initial_indices_by_edge.push((edge, constant_offsets, initial_iv));
        }

        let mut rewrites = LogicalEdgeRewriter::new();
        let header_param_positions = data
            .bb_data(looop.header())
            .params()
            .iter()
            .enumerate()
            .map(|(position, &parameter)| (parameter, position))
            .collect::<FxHashMap<_, _>>();
        for (edge, constant_offsets, initial_iv) in initial_indices_by_edge {
            // Non-IV offsets may be loop-invariant header parameters; their
            // preheader edge argument substitutes for them in the initial
            // pointer (the parameter value is unchanged on every backedge).
            let edge_args = edge.args(data).to_vec();
            let mut initial_offsets = candidate.offsets.clone();
            for (position, evolution) in candidate.address_evolution.indices.iter().enumerate() {
                initial_offsets[position] = match evolution {
                    IndexEvolution::Invariant => {
                        match header_param_positions.get(&candidate.offsets[position]) {
                            Some(&parameter_position) => edge_args[parameter_position],
                            None => initial_offsets[position],
                        }
                    }
                    IndexEvolution::Direct => initial_iv,
                    IndexEvolution::Affine(affine) => match constant_offsets
                        .as_ref()
                        .and_then(|offsets| offsets[position])
                    {
                        Some(offset) => data.new_local_value().integer(offset),
                        None => Self::clone_affine_initial(
                            data,
                            preheader,
                            affine,
                            candidate.iv,
                            initial_iv,
                            &header_param_positions,
                            &edge_args,
                            &candidate.forwarded_params,
                        ),
                    },
                };
            }
            let initial_pointer = data
                .new_local_value()
                .get_elem_ptr(candidate.base, initial_offsets);
            debug_assert_eq!(data.inst_data(initial_pointer).ty(), &candidate.pointer_ty);
            data.layout_mut()
                .insert_before_terminator(preheader, initial_pointer);
            rewrites.append_arg(data, edge, initial_pointer);
        }

        let pointer = data
            .new_basic_block()
            .add_param(looop.header(), candidate.pointer_ty.clone());
        let pointer_step = data
            .new_local_value()
            .integer(candidate.address_evolution.pointer_step);
        for group in candidate.backedge_groups {
            let next_pointer = data
                .new_local_value()
                .get_elem_ptr(pointer, vec![pointer_step]);
            debug_assert_eq!(data.inst_data(next_pointer).ty(), &candidate.pointer_ty);
            data.layout_mut()
                .insert_before_terminator(group.source, next_pointer);
            for edge in group.edges {
                rewrites.append_arg(data, edge, next_pointer);
            }
        }
        assert!(rewrites.apply(data));

        let zero = data.new_local_value().integer(0);
        data.replace_inst_with(candidate.gep)
            .get_elem_ptr(pointer, vec![zero]);
        debug_assert_eq!(data.inst_data(candidate.gep).ty(), &candidate.pointer_ty);
        ApplyResult::Changed
    }
}

impl Pass for PointerStrengthReduction {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().is_decl() {
            return false;
        }
        let mut changed = false;
        loop {
            let Some(cfg) = CFG::new(data) else {
                return changed;
            };
            if cfg.is_acyclic() {
                return changed;
            }
            let parameter_blocks = cfg
                .blocks()
                .iter()
                .flat_map(|&block| {
                    data.bb_data(block)
                        .params()
                        .iter()
                        .copied()
                        .map(move |parameter| (parameter, block))
                })
                .collect::<FxHashMap<_, _>>();
            let (cfg, dom_tree, loops) = LoopAnalysis::from_cfg(cfg);
            let ivs = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
            let range_arena = ArenaContext {
                program: &*data.program,
                curr_func: data.curr_func,
            };
            let nonneg = crate::opt::analysis_passes::return_summary::nonneg_preserving_functions(
                data.program,
            );
            let no_params = FxHashSet::default();
            let ranges = RangeAnalysis::new(&range_arena, &cfg, &loops, &ivs, &nonneg, &no_params);
            let mut transformed = false;
            for looop in loops.loops() {
                let Some(candidate) = Self::find_candidate(
                    data,
                    &cfg,
                    &dom_tree,
                    &loops,
                    &ivs,
                    &ranges,
                    looop,
                    &parameter_blocks,
                ) else {
                    continue;
                };
                match Self::apply_candidate(data, &cfg, looop, candidate) {
                    ApplyResult::Unchanged => continue,
                    ApplyResult::Changed => {
                        changed = true;
                        transformed = true;
                        break;
                    }
                }
            }
            if !transformed {
                return changed;
            }
        }
    }
}

#[cfg(test)]
mod tests;
