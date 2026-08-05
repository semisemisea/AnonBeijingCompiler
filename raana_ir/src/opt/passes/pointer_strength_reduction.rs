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
            resolve_forwarded_params,
            outgoing_edges,
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
        let forwarded_params =
            resolve_forwarded_params(&forwarded_block_params(data, cfg));
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

    fn has_only_loop_memory_users(data: &ArenaContextMut<'_>, looop: &Loop, gep: Inst) -> bool {
        let users = data.inst_data(gep).used_by();
        !users.is_empty()
            && users.iter().all(|&user| {
                data.layout()
                    .parent_bb(user)
                    .is_some_and(|block| looop.contains(block))
                    && match data.inst_data(user).kind() {
                        InstKind::Load(load) => load.src() == gep,
                        InstKind::Store(store) => store.dest() == gep,
                        _ => false,
                    }
            })
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
                &data.program,
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
mod tests {
    use super::*;
    use crate::ir::{Program, builder_trait::*};
    use crate::opt::analysis_passes::induction_variable::constant_induction_range;

    struct LoopFixture {
        function: Function,
        entry: BasicBlock,
        header: BasicBlock,
        latch: BasicBlock,
        gep: Inst,
        load: Inst,
        iv: Inst,
        next_iv: Inst,
        entry_jump: Inst,
        backedge: Inst,
    }

    fn build_loop_with_update(
        update_op: BinaryOp,
        step_value: i32,
        compare_op: BinaryOp,
        iv_last: bool,
    ) -> (Program, LoopFixture) {
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr".into(),
            vec![
                Type::get_pointer(array_ty),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let bound = data.params()[1];
        let outer_index = data.params()[2];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, latch, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let initial = data.new_local_inst().integer(3);
        let entry_jump = data.new_local_inst().jump(header, vec![initial]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let iv = data.bb_data(header).params()[0];
        let offsets = if iv_last {
            vec![outer_index, iv]
        } else {
            vec![iv, outer_index]
        };
        let gep = data.new_local_inst().get_elem_ptr(base, offsets);
        let load = data.new_local_inst().load(gep);
        let step = data.new_local_inst().integer(step_value);
        let next_iv = data.new_local_inst().binary(update_op, iv, step);
        for inst in [gep, load, next_iv] {
            data.layout_mut().insert_inst(header, inst);
        }
        let condition = data.new_local_inst().binary(compare_op, iv, bound);
        data.layout_mut().insert_inst(header, condition);
        let branch = data
            .new_local_inst()
            .branch(condition, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);

        let backedge = data.new_local_inst().jump(header, vec![next_iv]);
        data.layout_mut().insert_inst(latch, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        (
            program,
            LoopFixture {
                function,
                entry,
                header,
                latch,
                gep,
                load,
                iv,
                next_iv,
                entry_jump,
                backedge,
            },
        )
    }

    fn build_loop(compare_op: BinaryOp, iv_last: bool) -> (Program, LoopFixture) {
        build_loop_with_update(BinaryOp::Add, 1, compare_op, iv_last)
    }

    fn replace_bound_with_constant(program: &mut Program, fixture: &LoopFixture, bound_value: i32) {
        let data = program.func_data_mut(fixture.function);
        let terminator = data.layout().basicblock(fixture.header).terminator();
        let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
            panic!("header terminator must be a branch");
        };
        let condition = branch.cond();
        let InstKind::Binary(compare) = data.inst_data(condition).kind() else {
            panic!("header condition must be a comparison");
        };
        let (op, lhs, rhs) = (compare.op(), compare.lhs(), compare.rhs());
        let bound = data.new_local_value().integer(bound_value);
        let (lhs, rhs) = if lhs == fixture.iv {
            (lhs, bound)
        } else if rhs == fixture.iv {
            (bound, rhs)
        } else {
            panic!("header comparison must use the induction variable");
        };
        data.replace_inst_with(condition).binary(op, lhs, rhs);
    }

    fn replace_gep_index_with_affine(
        program: &mut Program,
        fixture: &LoopFixture,
        coefficient: i32,
        offset: i32,
        subtract_product: bool,
    ) -> Inst {
        let data = program.func_data_mut(fixture.function);
        let coefficient = data.new_local_value().integer(coefficient);
        let product = data
            .new_local_value()
            .binary(BinaryOp::Mul, fixture.iv, coefficient);
        let offset = data.new_local_value().integer(offset);
        let derived = if subtract_product {
            data.new_local_value()
                .binary(BinaryOp::Sub, offset, product)
        } else {
            data.new_local_value()
                .binary(BinaryOp::Add, product, offset)
        };
        data.layout_mut().insert_inst_before(fixture.gep, product);
        data.layout_mut().insert_inst_before(fixture.gep, derived);

        let InstKind::GetElemPtr(gep) = data.inst_data(fixture.gep).kind() else {
            panic!("fixture address must be a GEP");
        };
        let mut offsets = gep.offsets().to_vec();
        let position = offsets
            .iter()
            .position(|&value| value == fixture.iv)
            .expect("fixture GEP must use the induction variable");
        let base = gep.base();
        offsets[position] = derived;
        data.replace_inst_with(fixture.gep)
            .get_elem_ptr(base, offsets);
        derived
    }

    fn replace_gep_index_with_shift(
        program: &mut Program,
        fixture: &LoopFixture,
        shift: i32,
        offset: i32,
    ) {
        let data = program.func_data_mut(fixture.function);
        let shift = data.new_local_value().integer(shift);
        let shifted = data
            .new_local_value()
            .binary(BinaryOp::Shl, fixture.iv, shift);
        let offset = data.new_local_value().integer(offset);
        let derived = data
            .new_local_value()
            .binary(BinaryOp::Add, shifted, offset);
        data.layout_mut().insert_inst_before(fixture.gep, shifted);
        data.layout_mut().insert_inst_before(fixture.gep, derived);

        let InstKind::GetElemPtr(gep) = data.inst_data(fixture.gep).kind() else {
            panic!("fixture address must be a GEP");
        };
        let mut offsets = gep.offsets().to_vec();
        let position = offsets
            .iter()
            .position(|&value| value == fixture.iv)
            .expect("fixture GEP must use the induction variable");
        let base = gep.base();
        offsets[position] = derived;
        data.replace_inst_with(fixture.gep)
            .get_elem_ptr(base, offsets);
    }

    fn run(program: &mut Program, function: Function) -> bool {
        let mut context = ArenaContextMut {
            program,
            curr_func: Some(function),
        };
        PointerStrengthReduction.run_on(&mut context)
    }

    #[test]
    fn skips_an_acyclic_function() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "acyclic".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        data.layout_mut().push_bb_back(exit);
        let jump = data.new_local_inst().jump(exit, vec![]);
        data.layout_mut().insert_inst(entry, jump);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(!run(&mut program, function));
    }

    fn integer_constant(data: &FunctionData, inst: Inst) -> Option<i32> {
        match data.inst_data(inst).kind() {
            InstKind::Integer(integer) => Some(integer.value()),
            _ => None,
        }
    }

    #[test]
    fn carries_a_pointer_for_a_forward_unit_step_loop() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        replace_bound_with_constant(&mut program, &fixture, 10);
        assert!(run(&mut program, fixture.function));

        let data = program.func_data(fixture.function);
        let params = data.bb_data(fixture.header).params();
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], fixture.iv);
        let pointer = params[1];
        assert_eq!(
            data.inst_data(pointer).ty(),
            data.inst_data(fixture.gep).ty()
        );

        let InstKind::GetElemPtr(rewritten) = data.inst_data(fixture.gep).kind() else {
            panic!("the original address must remain a GEP");
        };
        assert_eq!(rewritten.base(), pointer);
        assert_eq!(rewritten.offsets().len(), 1);
        assert_eq!(integer_constant(data, rewritten.offsets()[0]), Some(0));
        assert_eq!(
            data.inst_data(fixture.load)
                .inst_usage()
                .collect::<Vec<_>>(),
            vec![fixture.gep]
        );

        let InstKind::Jump(entry_jump) = data.inst_data(fixture.entry_jump).kind() else {
            panic!("entry terminator must remain a jump");
        };
        assert_eq!(entry_jump.args().len(), 2);
        let initial_pointer = entry_jump.args()[1];
        assert_eq!(
            data.layout().parent_bb(initial_pointer),
            Some(fixture.entry)
        );
        let InstKind::GetElemPtr(initial) = data.inst_data(initial_pointer).kind() else {
            panic!("entry must compute the initial pointer");
        };
        assert_eq!(
            initial.offsets().last().copied(),
            Some(entry_jump.args()[0])
        );

        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        assert_eq!(backedge.args().len(), 2);
        assert_eq!(backedge.args()[0], fixture.next_iv);
        let next_pointer = backedge.args()[1];
        assert_eq!(data.layout().parent_bb(next_pointer), Some(fixture.latch));
        let InstKind::GetElemPtr(next) = data.inst_data(next_pointer).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(next.base(), pointer);
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(1));

        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let ivs = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        let looop = loops
            .loops()
            .iter()
            .find(|looop| looop.header() == fixture.header)
            .unwrap();
        assert!(ivs.find(looop, fixture.iv).is_some());
        assert!(!run(&mut program, fixture.function));
    }

    #[test]
    fn carries_a_pointer_for_a_backward_unit_step_loop() {
        let (mut program, fixture) = build_loop_with_update(BinaryOp::Sub, 1, BinaryOp::Gt, true);
        replace_bound_with_constant(&mut program, &fixture, -10);
        assert!(run(&mut program, fixture.function));

        let data = program.func_data(fixture.function);
        let pointer = data.bb_data(fixture.header).params()[1];
        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let next_pointer = backedge.args()[1];
        let InstKind::GetElemPtr(next) = data.inst_data(next_pointer).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(next.base(), pointer);
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(-1));
        assert!(!run(&mut program, fixture.function));
    }

    #[test]
    fn carries_a_pointer_for_a_forward_non_unit_step_loop() {
        let (mut program, fixture) = build_loop_with_update(BinaryOp::Add, 2, BinaryOp::Lt, true);
        replace_bound_with_constant(&mut program, &fixture, 100);
        assert!(run(&mut program, fixture.function));

        let data = program.func_data(fixture.function);
        let pointer = data.bb_data(fixture.header).params()[1];
        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let InstKind::GetElemPtr(next) = data.inst_data(backedge.args()[1]).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(next.base(), pointer);
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(2));
        assert!(!run(&mut program, fixture.function));
    }

    #[test]
    fn carries_a_pointer_for_a_backward_non_unit_step_loop() {
        let (mut program, fixture) = build_loop_with_update(BinaryOp::Sub, 2, BinaryOp::Gt, true);
        replace_bound_with_constant(&mut program, &fixture, -100);
        assert!(run(&mut program, fixture.function));

        let data = program.func_data(fixture.function);
        let pointer = data.bb_data(fixture.header).params()[1];
        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let InstKind::GetElemPtr(next) = data.inst_data(backedge.args()[1]).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(next.base(), pointer);
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(-2));
        assert!(!run(&mut program, fixture.function));
    }

    #[test]
    fn scales_a_non_unit_step_by_an_intermediate_gep_stride() {
        let (mut program, fixture) = build_loop_with_update(BinaryOp::Add, 2, BinaryOp::Lt, false);
        replace_bound_with_constant(&mut program, &fixture, 100);
        assert!(run(&mut program, fixture.function));

        let data = program.func_data(fixture.function);
        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let InstKind::GetElemPtr(next) = data.inst_data(backedge.args()[1]).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(64));
    }

    #[test]
    fn carries_a_pointer_for_a_constant_affine_index() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        replace_bound_with_constant(&mut program, &fixture, 10);
        let derived = replace_gep_index_with_affine(&mut program, &fixture, 2, 1, false);
        assert!(run(&mut program, fixture.function));

        let data = program.func_data(fixture.function);
        let pointer = data.bb_data(fixture.header).params()[1];
        let InstKind::Jump(entry) = data.inst_data(fixture.entry_jump).kind() else {
            panic!("entry terminator must remain a jump");
        };
        let InstKind::GetElemPtr(initial) = data.inst_data(entry.args()[1]).kind() else {
            panic!("entry must compute the initial pointer");
        };
        assert_eq!(
            integer_constant(data, *initial.offsets().last().unwrap()),
            Some(7)
        );

        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let InstKind::GetElemPtr(next) = data.inst_data(backedge.args()[1]).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(next.base(), pointer);
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(2));
        assert!(data.inst_data(derived).used_by().is_empty());
        assert!(!run(&mut program, fixture.function));
    }

    #[test]
    fn carries_a_pointer_for_a_negative_affine_coefficient() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        replace_bound_with_constant(&mut program, &fixture, 10);
        replace_gep_index_with_affine(&mut program, &fixture, 2, 20, true);
        assert!(run(&mut program, fixture.function));

        let data = program.func_data(fixture.function);
        let InstKind::Jump(entry) = data.inst_data(fixture.entry_jump).kind() else {
            panic!("entry terminator must remain a jump");
        };
        let InstKind::GetElemPtr(initial) = data.inst_data(entry.args()[1]).kind() else {
            panic!("entry must compute the initial pointer");
        };
        assert_eq!(
            integer_constant(data, *initial.offsets().last().unwrap()),
            Some(14)
        );
        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let InstKind::GetElemPtr(next) = data.inst_data(backedge.args()[1]).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(-2));
    }

    #[test]
    fn carries_a_pointer_for_a_shift_derived_index() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        replace_bound_with_constant(&mut program, &fixture, 10);
        replace_gep_index_with_shift(&mut program, &fixture, 2, 1);
        assert!(run(&mut program, fixture.function));

        let data = program.func_data(fixture.function);
        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let InstKind::GetElemPtr(next) = data.inst_data(backedge.args()[1]).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(4));
    }

    #[test]
    fn carries_an_affine_index_with_a_runtime_bound() {
        // An affine index with a runtime (non-constant) bound: the i32
        // intermediate-range proof is skipped because the incremental
        // scheme replaces the 32-bit offset arithmetic with 64-bit pointer
        // adds.
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        replace_gep_index_with_affine(&mut program, &fixture, 2, 1, false);
        assert!(run(&mut program, fixture.function));

        let data = program.func_data(fixture.function);
        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let InstKind::GetElemPtr(next) = data.inst_data(backedge.args()[1]).kind() else {
            panic!("latch must compute the next pointer");
        };
        let pointer = data.bb_data(fixture.header).params()[1];
        assert_eq!(next.base(), pointer);
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(2));
    }

    #[test]
    fn rejects_an_affine_index_whose_32_bit_intermediate_can_wrap() {
        // A constant bound keeps the range proof active: a coefficient that
        // overflows i32 would corrupt the intermediate offset arithmetic.
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        replace_bound_with_constant(&mut program, &fixture, 10);
        replace_gep_index_with_affine(&mut program, &fixture, 1 << 30, 0, false);
        assert!(!run(&mut program, fixture.function));
    }

    #[test]
    fn carries_a_direct_induction_index_without_a_constant_range_proof() {
        // A strict-exit unit-step loop with a runtime bound: the pointer is
        // carried exactly by the IV (base + iv*stride), so no constant
        // induction range is required for a direct index. This is the shape of
        // h-5's inner k loop after count-up rotation.
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        assert!(run(&mut program, fixture.function));
        let data = program.func_data(fixture.function);
        let pointer = data.bb_data(fixture.header).params()[1];
        let InstKind::GetElemPtr(rewritten) = data.inst_data(fixture.gep).kind() else {
            unreachable!()
        };
        assert_eq!(rewritten.base(), pointer);
        assert_eq!(integer_constant(data, rewritten.offsets()[0]), Some(0));
    }

    #[test]
    fn carries_a_pointer_for_a_direct_induction_index_with_a_runtime_bound() {
        // The primary runtime-bound case: a direct induction index with a
        // runtime bound becomes an incremental pointer. The header gains a
        // pointer parameter, the latch steps it, and the original GEP reads
        // through it with a zero offset.
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        assert!(run(&mut program, fixture.function));

        let data = program.func_data(fixture.function);
        assert_eq!(data.bb_data(fixture.header).params().len(), 2);
        let pointer = data.bb_data(fixture.header).params()[1];
        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let InstKind::GetElemPtr(next) = data.inst_data(backedge.args()[1]).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(next.base(), pointer);
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(1));
        let InstKind::Jump(entry_jump) = data.inst_data(fixture.entry_jump).kind() else {
            panic!("entry terminator must remain a jump");
        };
        let InstKind::GetElemPtr(initial) = data.inst_data(entry_jump.args()[1]).kind() else {
            panic!("entry must compute the initial pointer");
        };
        // Direct index at position 1 (iv_last): the initial offset is the
        // constant 3 from the entry jump; the invariant position 0 keeps
        // the original offset.
        assert_eq!(integer_constant(data, initial.offsets()[1]), Some(3));
        let InstKind::GetElemPtr(gep) = data.inst_data(fixture.gep).kind() else {
            unreachable!()
        };
        assert_eq!(gep.base(), pointer);
        assert_eq!(integer_constant(data, gep.offsets()[0]), Some(0));
    }

    #[test]
    fn carries_a_pointer_with_a_non_constant_initial_value() {
        // The initial value only needs to be available in the preheader: a
        // non-constant (function parameter) initial offset is carried into
        // the initial pointer as-is, with no constant-fold requirement.
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        {
            let data = program.func_data_mut(fixture.function);
            let bound = data.params()[1];
            data.replace_inst_with(fixture.entry_jump)
                .jump(fixture.header, vec![bound]);
        }
        assert!(run(&mut program, fixture.function));

        let data = program.func_data(fixture.function);
        let InstKind::Jump(entry_jump) = data.inst_data(fixture.entry_jump).kind() else {
            panic!("entry terminator must remain a jump");
        };
        let InstKind::GetElemPtr(initial) = data.inst_data(entry_jump.args()[1]).kind() else {
            panic!("entry must compute the initial pointer");
        };
        assert_eq!(initial.offsets()[1], data.params()[1]);
    }

    #[test]
    fn rejects_a_non_unit_step_with_a_runtime_bound() {
        // normalize_strict_exit requires a constant bound for non-unit
        // steps: a runtime bound with step 2 stays untransformed.
        let (mut program, fixture) = build_loop_with_update(BinaryOp::Add, 2, BinaryOp::Lt, true);
        assert!(!run(&mut program, fixture.function));
        assert_eq!(
            program
                .func_data(fixture.function)
                .bb_data(fixture.header)
                .params()
                .len(),
            1
        );
    }

    #[test]
    fn carries_an_affine_index_with_a_bounded_invariant_offset() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        replace_bound_with_constant(&mut program, &fixture, 10);
        let data = program.func_data_mut(fixture.function);
        let invariant = data.params()[2];
        let mask = data.new_local_value().integer(7);
        let bounded = data
            .new_local_value()
            .binary(BinaryOp::And, invariant, mask);
        data.layout_mut()
            .insert_inst_before(fixture.entry_jump, bounded);
        let derived = data
            .new_local_value()
            .binary(BinaryOp::Add, fixture.iv, bounded);
        data.layout_mut().insert_inst_before(fixture.gep, derived);
        let InstKind::GetElemPtr(gep) = data.inst_data(fixture.gep).kind() else {
            unreachable!()
        };
        let mut offsets = gep.offsets().to_vec();
        let position = offsets
            .iter()
            .position(|&offset| offset == fixture.iv)
            .unwrap();
        let base = gep.base();
        offsets[position] = derived;
        data.replace_inst_with(fixture.gep)
            .get_elem_ptr(base, offsets);

        assert!(run(&mut program, fixture.function));
    }

    #[test]
    fn rejects_an_affine_index_with_an_unknown_invariant_offset_range() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        replace_bound_with_constant(&mut program, &fixture, 10);
        let data = program.func_data_mut(fixture.function);
        let invariant = data.params()[2];
        let derived = data
            .new_local_value()
            .binary(BinaryOp::Add, fixture.iv, invariant);
        data.layout_mut().insert_inst_before(fixture.gep, derived);
        let InstKind::GetElemPtr(gep) = data.inst_data(fixture.gep).kind() else {
            unreachable!()
        };
        let mut offsets = gep.offsets().to_vec();
        let position = offsets
            .iter()
            .position(|&offset| offset == fixture.iv)
            .unwrap();
        let base = gep.base();
        offsets[position] = derived;
        data.replace_inst_with(fixture.gep)
            .get_elem_ptr(base, offsets);

        assert!(!run(&mut program, fixture.function));
    }

    #[test]
    fn rejects_affine_wrap_on_the_final_header_visit() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        replace_bound_with_constant(&mut program, &fixture, 2);
        {
            let data = program.func_data_mut(fixture.function);
            let zero = data.new_local_value().integer(0);
            data.replace_inst_with(fixture.entry_jump)
                .jump(fixture.header, vec![zero]);
        }
        let derived = replace_gep_index_with_affine(&mut program, &fixture, i32::MAX, 0, false);

        let data = program.func_data(fixture.function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let looop = loops
            .loops()
            .iter()
            .find(|looop| looop.header() == fixture.header)
            .unwrap();
        let ivs = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
        let iv = ivs.find(looop, fixture.iv).unwrap();
        let context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(fixture.function),
        };
        let exit = normalize_strict_exit(&context, looop, iv).unwrap();
        let range = constant_induction_range(&context, iv, exit).unwrap();
        assert_eq!(range.min(), 0);
        assert_eq!(range.max(), 2);
        drop(context);
        assert!(!run(&mut program, fixture.function));
        assert!(
            !program
                .func_data(fixture.function)
                .inst_data(derived)
                .used_by()
                .is_empty()
        );
    }

    #[test]
    fn rejects_affine_cancellation_when_an_intermediate_operation_can_wrap() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        replace_bound_with_constant(&mut program, &fixture, 3);
        let data = program.func_data_mut(fixture.function);
        let coefficient = data.new_local_value().integer(i32::MAX);
        let product = data
            .new_local_value()
            .binary(BinaryOp::Mul, fixture.iv, coefficient);
        let cancelled = data
            .new_local_value()
            .binary(BinaryOp::Sub, product, product);
        let derived = data
            .new_local_value()
            .binary(BinaryOp::Add, cancelled, fixture.iv);
        for inst in [product, cancelled, derived] {
            data.layout_mut().insert_inst_before(fixture.gep, inst);
        }
        let InstKind::GetElemPtr(gep) = data.inst_data(fixture.gep).kind() else {
            unreachable!()
        };
        let mut offsets = gep.offsets().to_vec();
        let position = offsets
            .iter()
            .position(|&offset| offset == fixture.iv)
            .unwrap();
        let base = gep.base();
        offsets[position] = derived;
        data.replace_inst_with(fixture.gep)
            .get_elem_ptr(base, offsets);

        assert!(!run(&mut program, fixture.function));
        assert_eq!(
            program
                .func_data(fixture.function)
                .bb_data(fixture.header)
                .params()
                .len(),
            1
        );
    }

    #[test]
    fn carries_an_affine_address_through_a_forwarding_block() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_forwarded_affine".into(),
            vec![
                Type::get_pointer(Type::get_i32()),
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let params = data.params().to_vec();
        let [base, bound, row, width] = params.as_slice() else {
            unreachable!()
        };
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let dispatch = data
            .new_basic_block()
            .basic_block("dispatch".into(), vec![]);
        let access = data
            .new_basic_block()
            .basic_block("access".into(), vec![Type::get_i32()]);
        let forwarding = data
            .new_basic_block()
            .basic_block("forwarding".into(), vec![Type::get_i32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, dispatch, forwarding, access, latch, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, *bound);
        data.layout_mut().insert_inst(header, compare);
        let header_branch = data
            .new_local_inst()
            .branch(compare, dispatch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, header_branch);

        let product = data.new_local_inst().binary(BinaryOp::Mul, *row, *width);
        let index = data.new_local_inst().binary(BinaryOp::Add, product, iv);
        for inst in [product, index] {
            data.layout_mut().insert_inst(dispatch, inst);
        }
        let dispatch_jump = data.new_local_inst().jump(forwarding, vec![index]);
        data.layout_mut().insert_inst(dispatch, dispatch_jump);

        let first_forwarded_index = data.bb_data(forwarding).params()[0];
        let forwarding_jump = data
            .new_local_inst()
            .jump(access, vec![first_forwarded_index]);
        data.layout_mut().insert_inst(forwarding, forwarding_jump);

        let forwarded_index = data.bb_data(access).params()[0];
        let gep = data
            .new_local_inst()
            .get_elem_ptr(*base, vec![forwarded_index]);
        let load = data.new_local_inst().load(gep);
        let access_jump = data.new_local_inst().jump(latch, vec![]);
        for inst in [gep, load, access_jump] {
            data.layout_mut().insert_inst(access, inst);
        }
        let one = data.new_local_inst().integer(1);
        let update = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let backedge = data.new_local_inst().jump(header, vec![update]);
        data.layout_mut().insert_inst(latch, update);
        data.layout_mut().insert_inst(latch, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        let pointer = data.bb_data(header).params()[1];
        let InstKind::GetElemPtr(rewritten) = data.inst_data(gep).kind() else {
            unreachable!()
        };
        assert_eq!(rewritten.base(), pointer);
        assert_eq!(integer_constant(data, rewritten.offsets()[0]), Some(0));
    }

    #[test]
    fn carries_a_pointer_across_two_latches() {
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_two_latches".into(),
            vec![
                Type::get_pointer(array_ty),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let bound = data.new_local_inst().integer(10);
        let outer_index = data.params()[2];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let dispatch = data
            .new_basic_block()
            .basic_block("dispatch".into(), vec![]);
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, dispatch, left, right, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let gep = data
            .new_local_inst()
            .get_elem_ptr(base, vec![outer_index, iv]);
        let load = data.new_local_inst().load(gep);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        for inst in [gep, load, compare] {
            data.layout_mut().insert_inst(header, inst);
        }
        let header_branch = data
            .new_local_inst()
            .branch(compare, dispatch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, header_branch);

        let choose = data.new_local_inst().integer(1);
        let dispatch_branch = data
            .new_local_inst()
            .branch(choose, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(dispatch, dispatch_branch);

        let one_left = data.new_local_inst().integer(1);
        let left_update = data.new_local_inst().binary(BinaryOp::Add, iv, one_left);
        data.layout_mut().insert_inst(left, left_update);
        let left_backedge = data.new_local_inst().jump(header, vec![left_update]);
        data.layout_mut().insert_inst(left, left_backedge);

        let one_right = data.new_local_inst().integer(1);
        let right_update = data.new_local_inst().binary(BinaryOp::Add, one_right, iv);
        data.layout_mut().insert_inst(right, right_update);
        let right_backedge = data.new_local_inst().jump(header, vec![right_update]);
        data.layout_mut().insert_inst(right, right_backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        let pointer = data.bb_data(header).params()[1];
        for (latch, terminator, update) in [
            (left, left_backedge, left_update),
            (right, right_backedge, right_update),
        ] {
            let InstKind::Jump(backedge) = data.inst_data(terminator).kind() else {
                panic!("latch must remain a jump");
            };
            assert_eq!(backedge.args()[0], update);
            let next_pointer = backedge.args()[1];
            assert_eq!(data.layout().parent_bb(next_pointer), Some(latch));
            let InstKind::GetElemPtr(next) = data.inst_data(next_pointer).kind() else {
                panic!("latch must compute the next pointer");
            };
            assert_eq!(next.base(), pointer);
            assert_eq!(integer_constant(data, next.offsets()[0]), Some(1));
        }
        assert!(!run(&mut program, function));
    }

    #[test]
    fn shares_one_pointer_update_across_parallel_backedge_arms() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        replace_bound_with_constant(&mut program, &fixture, 10);
        let data = program.func_data_mut(fixture.function);
        let condition = data.new_local_value().integer(1);
        let one = data.new_local_value().integer(1);
        let alternate_update = data
            .new_local_value()
            .binary(BinaryOp::Add, one, fixture.iv);
        data.layout_mut()
            .insert_inst_before(fixture.backedge, alternate_update);
        data.replace_inst_with(fixture.backedge).branch(
            condition,
            fixture.header,
            vec![fixture.next_iv],
            fixture.header,
            vec![alternate_update],
        );

        assert!(run(&mut program, fixture.function));
        let data = program.func_data(fixture.function);
        let pointer = data.bb_data(fixture.header).params()[1];
        let InstKind::Branch(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a branch");
        };
        assert_eq!(backedge.t_args().len(), 2);
        assert_eq!(backedge.f_args().len(), 2);
        let next_pointer = backedge.t_args()[1];
        assert_eq!(backedge.f_args()[1], next_pointer);
        assert_eq!(data.layout().parent_bb(next_pointer), Some(fixture.latch));
        let InstKind::GetElemPtr(next) = data.inst_data(next_pointer).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(next.base(), pointer);
        assert!(!run(&mut program, fixture.function));
    }

    #[test]
    fn rejects_inconsistent_parallel_backedge_updates() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        let data = program.func_data_mut(fixture.function);
        let condition = data.new_local_value().integer(1);
        let two = data.new_local_value().integer(2);
        let alternate_update = data
            .new_local_value()
            .binary(BinaryOp::Add, fixture.iv, two);
        data.layout_mut()
            .insert_inst_before(fixture.backedge, alternate_update);
        data.replace_inst_with(fixture.backedge).branch(
            condition,
            fixture.header,
            vec![fixture.next_iv],
            fixture.header,
            vec![alternate_update],
        );

        assert!(!run(&mut program, fixture.function));
        assert_eq!(
            program
                .func_data(fixture.function)
                .bb_data(fixture.header)
                .params()
                .len(),
            1
        );
    }

    #[test]
    fn rejects_a_path_conditional_gep_with_multiple_latches() {
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_conditional_gep".into(),
            vec![
                Type::get_pointer(array_ty),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let bound = data.new_local_inst().integer(10);
        let outer_index = data.params()[2];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let dispatch = data
            .new_basic_block()
            .basic_block("dispatch".into(), vec![]);
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, dispatch, left, right, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        data.layout_mut().insert_inst(header, compare);
        let header_branch = data
            .new_local_inst()
            .branch(compare, dispatch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, header_branch);

        let choose = data.new_local_inst().integer(1);
        let dispatch_branch = data
            .new_local_inst()
            .branch(choose, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(dispatch, dispatch_branch);

        let gep = data
            .new_local_inst()
            .get_elem_ptr(base, vec![outer_index, iv]);
        let load = data.new_local_inst().load(gep);
        let one_left = data.new_local_inst().integer(1);
        let left_update = data.new_local_inst().binary(BinaryOp::Add, iv, one_left);
        for inst in [gep, load, left_update] {
            data.layout_mut().insert_inst(left, inst);
        }
        let left_backedge = data.new_local_inst().jump(header, vec![left_update]);
        data.layout_mut().insert_inst(left, left_backedge);

        let one_right = data.new_local_inst().integer(1);
        let right_update = data.new_local_inst().binary(BinaryOp::Add, iv, one_right);
        data.layout_mut().insert_inst(right, right_update);
        let right_backedge = data.new_local_inst().jump(header, vec![right_update]);
        data.layout_mut().insert_inst(right, right_backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(!run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.bb_data(header).params().len(), 1);
        let InstKind::GetElemPtr(gep) = data.inst_data(gep).kind() else {
            unreachable!()
        };
        assert_eq!(gep.offsets().last().copied(), Some(iv));
    }

    #[test]
    fn carries_a_pointer_through_a_header_self_loop() {
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_self_loop".into(),
            vec![
                Type::get_pointer(array_ty),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let bound = data.new_local_inst().integer(10);
        let outer_index = data.params()[2];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let gep = data
            .new_local_inst()
            .get_elem_ptr(base, vec![outer_index, iv]);
        let load = data.new_local_inst().load(gep);
        let one = data.new_local_inst().integer(1);
        let update = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        for inst in [gep, load, update, compare] {
            data.layout_mut().insert_inst(header, inst);
        }
        let backedge = data
            .new_local_inst()
            .branch(compare, header, vec![update], exit, vec![]);
        data.layout_mut().insert_inst(header, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        let pointer = data.bb_data(header).params()[1];
        let InstKind::Branch(backedge) = data.inst_data(backedge).kind() else {
            panic!("header terminator must remain a branch");
        };
        assert_eq!(backedge.t_args().len(), 2);
        assert!(backedge.f_args().is_empty());
        let next_pointer = backedge.t_args()[1];
        assert_eq!(data.layout().parent_bb(next_pointer), Some(header));
        let InstKind::GetElemPtr(next) = data.inst_data(next_pointer).kind() else {
            panic!("header must compute the next pointer");
        };
        assert_eq!(next.base(), pointer);
        assert!(!run(&mut program, function));
    }

    #[test]
    fn rejects_more_than_two_backedge_sources() {
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_three_latches".into(),
            vec![
                Type::get_pointer(array_ty),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let bound = data.params()[1];
        let outer_index = data.params()[2];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let dispatch_a = data
            .new_basic_block()
            .basic_block("dispatch_a".into(), vec![]);
        let dispatch_b = data
            .new_basic_block()
            .basic_block("dispatch_b".into(), vec![]);
        let latches = [
            data.new_basic_block().basic_block("left".into(), vec![]),
            data.new_basic_block().basic_block("middle".into(), vec![]),
            data.new_basic_block().basic_block("right".into(), vec![]),
        ];
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, dispatch_a, dispatch_b]
            .into_iter()
            .chain(latches)
            .chain([exit])
        {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let gep = data
            .new_local_inst()
            .get_elem_ptr(base, vec![outer_index, iv]);
        let load = data.new_local_inst().load(gep);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        for inst in [gep, load, compare] {
            data.layout_mut().insert_inst(header, inst);
        }
        let header_branch = data
            .new_local_inst()
            .branch(compare, dispatch_a, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, header_branch);

        let choose = data.new_local_inst().integer(1);
        let dispatch_a_branch =
            data.new_local_inst()
                .branch(choose, latches[0], vec![], dispatch_b, vec![]);
        data.layout_mut().insert_inst(dispatch_a, dispatch_a_branch);
        let dispatch_b_branch =
            data.new_local_inst()
                .branch(choose, latches[1], vec![], latches[2], vec![]);
        data.layout_mut().insert_inst(dispatch_b, dispatch_b_branch);

        for latch in latches {
            let one = data.new_local_inst().integer(1);
            let update = data.new_local_inst().binary(BinaryOp::Add, iv, one);
            data.layout_mut().insert_inst(latch, update);
            let backedge = data.new_local_inst().jump(header, vec![update]);
            data.layout_mut().insert_inst(latch, backedge);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(!run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.bb_data(header).params().as_slice(), [iv]);
        let InstKind::GetElemPtr(gep) = data.inst_data(gep).kind() else {
            unreachable!()
        };
        assert_eq!(gep.offsets().last().copied(), Some(iv));
    }

    #[test]
    fn rejects_a_non_unit_step_without_a_no_wrap_proof() {
        let (mut program, fixture) = build_loop_with_update(BinaryOp::Add, 2, BinaryOp::Lt, true);
        assert!(!run(&mut program, fixture.function));
        let data = program.func_data(fixture.function);
        assert_eq!(data.bb_data(fixture.header).params().len(), 1);
        let InstKind::GetElemPtr(gep) = data.inst_data(fixture.gep).kind() else {
            unreachable!()
        };
        assert_eq!(gep.offsets().last().copied(), Some(fixture.iv));
    }

    #[test]
    fn rejects_a_non_unit_step_with_a_wrapping_bound() {
        for (update_op, step, compare_op, bound) in [
            (BinaryOp::Add, 2, BinaryOp::Lt, i32::MAX),
            (BinaryOp::Sub, 2, BinaryOp::Gt, i32::MIN),
        ] {
            let (mut program, fixture) = build_loop_with_update(update_op, step, compare_op, true);
            replace_bound_with_constant(&mut program, &fixture, bound);
            assert!(!run(&mut program, fixture.function));
            let data = program.func_data(fixture.function);
            assert_eq!(data.bb_data(fixture.header).params().len(), 1);
            let InstKind::GetElemPtr(gep) = data.inst_data(fixture.gep).kind() else {
                unreachable!()
            };
            assert_eq!(gep.offsets().last().copied(), Some(fixture.iv));
        }
    }

    #[test]
    fn rejects_a_non_strict_bound_that_can_wrap() {
        let (mut program, fixture) = build_loop(BinaryOp::Le, true);
        let param_count = program
            .func_data(fixture.function)
            .bb_data(fixture.header)
            .params()
            .len();
        assert!(!run(&mut program, fixture.function));
        let data = program.func_data(fixture.function);
        assert_eq!(data.bb_data(fixture.header).params().len(), param_count);
        let InstKind::GetElemPtr(gep) = data.inst_data(fixture.gep).kind() else {
            unreachable!()
        };
        assert_eq!(gep.offsets().last().copied(), Some(fixture.iv));
    }

    #[test]
    fn rejects_a_single_cheap_aarch64_address_term() {
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_cheap_gep".into(),
            vec![Type::get_pointer(array_ty), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let bound = data.params()[1];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, latch, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let gep = data.new_local_inst().get_elem_ptr(base, vec![zero, iv]);
        let load = data.new_local_inst().load(gep);
        let one = data.new_local_inst().integer(1);
        let next_iv = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        for inst in [gep, load, next_iv, compare] {
            data.layout_mut().insert_inst(header, inst);
        }
        let branch = data
            .new_local_inst()
            .branch(compare, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);
        let backedge = data.new_local_inst().jump(header, vec![next_iv]);
        data.layout_mut().insert_inst(latch, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(!run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.bb_data(header).params().len(), 1);
        let InstKind::GetElemPtr(gep) = data.inst_data(gep).kind() else {
            unreachable!()
        };
        assert_eq!(gep.offsets().last().copied(), Some(iv));
    }

    #[test]
    fn carries_a_pointer_for_an_induction_index_before_the_final_gep_index() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, false);
        replace_bound_with_constant(&mut program, &fixture, 10);
        assert!(run(&mut program, fixture.function));
        let data = program.func_data(fixture.function);
        let pointer = data.bb_data(fixture.header).params()[1];
        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let InstKind::GetElemPtr(next) = data.inst_data(backedge.args()[1]).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(next.base(), pointer);
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(32));
        let InstKind::Jump(entry_jump) = data.inst_data(fixture.entry_jump).kind() else {
            panic!("entry terminator must remain a jump");
        };
        let InstKind::GetElemPtr(initial) = data.inst_data(entry_jump.args()[1]).kind() else {
            panic!("entry must compute the initial pointer");
        };
        assert_eq!(initial.offsets()[0], entry_jump.args()[0]);
        assert!(!run(&mut program, fixture.function));
    }

    #[test]
    fn queries_the_stride_of_a_global_array_base() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, true);
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let initializer = program.new_value().zero_init(array_ty);
        let base = program.new_value().global_alloc(initializer);
        {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(fixture.function),
            };
            let zero = data.new_local_value().integer(0);
            data.replace_inst_with(fixture.gep)
                .get_elem_ptr(base, vec![zero, fixture.iv]);
        }

        let data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(fixture.function),
        };
        assert_eq!(
            gep_index_stride(&data, fixture.gep, 1),
            Some(crate::opt::utils::gep::GepIndexStride {
                byte_stride: 4,
                result_element_stride: 1,
            })
        );
    }

    #[test]
    fn carries_a_backward_pointer_through_an_invariant_suffix() {
        let row_ty = Type::get_array(Type::get_i32(), 32);
        let matrix_ty = Type::get_array(row_ty, 4);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_middle".into(),
            vec![
                Type::get_pointer(matrix_ty),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let bound = data.new_local_inst().integer(-10);
        let suffix = data.params()[2];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, latch, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let initial = data.new_local_inst().integer(3);
        let entry_jump = data.new_local_inst().jump(header, vec![initial]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let zero = data.new_local_inst().integer(0);
        let gep = data
            .new_local_inst()
            .get_elem_ptr(base, vec![zero, iv, suffix]);
        let load = data.new_local_inst().load(gep);
        let one = data.new_local_inst().integer(1);
        let next_iv = data.new_local_inst().binary(BinaryOp::Sub, iv, one);
        let compare = data.new_local_inst().binary(BinaryOp::Gt, iv, bound);
        for inst in [gep, load, next_iv, compare] {
            data.layout_mut().insert_inst(header, inst);
        }
        let branch = data
            .new_local_inst()
            .branch(compare, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);
        let backedge = data.new_local_inst().jump(header, vec![next_iv]);
        data.layout_mut().insert_inst(latch, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        let pointer = data.bb_data(header).params()[1];
        let InstKind::Jump(backedge) = data.inst_data(backedge).kind() else {
            panic!("backedge must remain a jump");
        };
        let InstKind::GetElemPtr(next) = data.inst_data(backedge.args()[1]).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(next.base(), pointer);
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(-32));
        let InstKind::Jump(entry_jump) = data.inst_data(entry_jump).kind() else {
            panic!("entry terminator must remain a jump");
        };
        let InstKind::GetElemPtr(initial_pointer) = data.inst_data(entry_jump.args()[1]).kind()
        else {
            panic!("entry must compute the initial pointer");
        };
        assert_eq!(initial_pointer.offsets(), [zero, initial, suffix]);
        assert!(!run(&mut program, function));
    }

    #[test]
    fn combines_the_same_induction_variable_across_gep_dimensions() {
        let (mut program, fixture) = build_loop(BinaryOp::Lt, false);
        replace_bound_with_constant(&mut program, &fixture, 10);
        let data = program.func_data_mut(fixture.function);
        let base = data.params()[0];
        data.replace_inst_with(fixture.gep)
            .get_elem_ptr(base, vec![fixture.iv, fixture.iv]);

        assert!(run(&mut program, fixture.function));
        let data = program.func_data(fixture.function);
        let InstKind::Jump(backedge) = data.inst_data(fixture.backedge).kind() else {
            panic!("latch must remain a jump");
        };
        let InstKind::GetElemPtr(next) = data.inst_data(backedge.args()[1]).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(33));
    }

    #[test]
    fn rejects_a_loop_variant_suffix_after_the_induction_index() {
        // A suffix that is variant but cannot be classified as affine (here:
        // a load inside the loop) must reject the whole GEP. (An affine
        // suffix such as iv + 1 is legal and folds into the pointer step;
        // this test pins the unclassifiable case.)
        let (mut program, fixture) = build_loop(BinaryOp::Lt, false);
        let data = program.func_data_mut(fixture.function);
        let base = data.params()[0];
        let slot = data.new_local_value().alloc(Type::get_i32());
        let suffix = data.new_local_value().load(slot);
        data.layout_mut().insert_inst_before(fixture.gep, slot);
        data.layout_mut().insert_inst_before(fixture.gep, suffix);
        data.replace_inst_with(fixture.gep)
            .get_elem_ptr(base, vec![fixture.iv, suffix]);

        assert!(!run(&mut program, fixture.function));
        assert_eq!(
            program
                .func_data(fixture.function)
                .bb_data(fixture.header)
                .params()
                .len(),
            1
        );
    }

    #[test]
    fn prefers_the_larger_iteration_saving_when_pressure_allows_one_pointer() {
        let low_array_ty = Type::get_array(Type::get_i32(), 32);
        let high_array_ty = Type::get_array(low_array_ty.clone(), 4);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_best_candidate".into(),
            vec![
                Type::get_pointer(low_array_ty),
                Type::get_pointer(high_array_ty),
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let low_base = data.params()[0];
        let high_base = data.params()[1];
        let bound = data.new_local_inst().integer(10);
        let low_outer = data.params()[3];
        let high_middle = data.params()[4];
        let high_inner = data.params()[5];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32(); 5]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, latch, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let one = data.new_local_inst().integer(1);
        let two = data.new_local_inst().integer(2);
        let three = data.new_local_inst().integer(3);
        let four = data.new_local_inst().integer(4);
        let entry_jump = data
            .new_local_inst()
            .jump(header, vec![zero, one, two, three, four]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let params = data.bb_data(header).params().to_vec();
        let iv = params[0];
        let low_gep = data
            .new_local_inst()
            .get_elem_ptr(low_base, vec![low_outer, iv]);
        let low_load = data.new_local_inst().load(low_gep);
        let high_gep = data
            .new_local_inst()
            .get_elem_ptr(high_base, vec![iv, high_middle, high_inner]);
        let high_load = data.new_local_inst().load(high_gep);
        let next_iv = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        for inst in [low_gep, low_load, high_gep, high_load, next_iv, compare] {
            data.layout_mut().insert_inst(header, inst);
        }
        let branch = data
            .new_local_inst()
            .branch(compare, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);
        let backedge = data.new_local_inst().jump(
            header,
            vec![next_iv, params[1], params[2], params[3], params[4]],
        );
        data.layout_mut().insert_inst(latch, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.bb_data(header).params().len(), 6);
        let pointer = data.bb_data(header).params()[5];

        let InstKind::GetElemPtr(low) = data.inst_data(low_gep).kind() else {
            unreachable!()
        };
        assert_eq!(low.base(), low_base);
        assert_eq!(low.offsets(), [low_outer, iv]);

        let InstKind::GetElemPtr(high) = data.inst_data(high_gep).kind() else {
            unreachable!()
        };
        assert_eq!(high.base(), pointer);
        assert_eq!(integer_constant(data, high.offsets()[0]), Some(0));
        assert!(!run(&mut program, function));
    }

    #[test]
    fn carries_at_most_three_pointers_through_one_loop() {
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_three_pointer_budget".into(),
            vec![
                Type::get_pointer(array_ty.clone()),
                Type::get_pointer(array_ty.clone()),
                Type::get_pointer(array_ty.clone()),
                Type::get_pointer(array_ty),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let bases = [
            data.params()[0],
            data.params()[1],
            data.params()[2],
            data.params()[3],
        ];
        let bound = data.new_local_inst().integer(10);
        let outer = data.params()[5];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, latch, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let iv = data.bb_data(header).params()[0];
        let mut geps = Vec::new();
        for base in bases {
            let gep = data.new_local_inst().get_elem_ptr(base, vec![outer, iv]);
            let load = data.new_local_inst().load(gep);
            data.layout_mut().insert_inst(header, gep);
            data.layout_mut().insert_inst(header, load);
            geps.push(gep);
        }
        let one = data.new_local_inst().integer(1);
        let next_iv = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        data.layout_mut().insert_inst(header, next_iv);
        data.layout_mut().insert_inst(header, compare);
        let branch = data
            .new_local_inst()
            .branch(compare, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);
        let backedge = data.new_local_inst().jump(header, vec![next_iv]);
        data.layout_mut().insert_inst(latch, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.bb_data(header).params().len(), 4);
        for (&gep, &base) in geps.iter().zip(&bases).take(3) {
            let InstKind::GetElemPtr(gep) = data.inst_data(gep).kind() else {
                unreachable!()
            };
            assert_ne!(gep.base(), base);
            assert_eq!(integer_constant(data, gep.offsets()[0]), Some(0));
        }
        let InstKind::GetElemPtr(untransformed) = data.inst_data(geps[3]).kind() else {
            unreachable!()
        };
        assert_eq!(untransformed.base(), bases[3]);
        assert_eq!(untransformed.offsets(), [outer, iv]);

        let InstKind::Jump(backedge) = data.inst_data(backedge).kind() else {
            panic!("latch must remain a jump");
        };
        assert_eq!(backedge.args().len(), 4);
        assert!(!run(&mut program, function));
    }

    #[test]
    fn carries_an_affine_pointer_through_a_created_preheader() {
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_created_preheader".into(),
            vec![Type::get_pointer(array_ty), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let outer_index = data.params()[1];
        let left = data.new_basic_block().basic_block("left".into(), vec![]);
        let right = data.new_basic_block().basic_block("right".into(), vec![]);
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [left, right, header, latch, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let choose = data.new_local_inst().integer(1);
        let entry_branch = data
            .new_local_inst()
            .branch(choose, left, vec![], right, vec![]);
        data.layout_mut().insert_inst(entry, entry_branch);

        let zero = data.new_local_inst().integer(0);
        let left_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(left, left_jump);
        let three = data.new_local_inst().integer(3);
        let right_jump = data.new_local_inst().jump(header, vec![three]);
        data.layout_mut().insert_inst(right, right_jump);

        let iv = data.bb_data(header).params()[0];
        let two = data.new_local_inst().integer(2);
        let product = data.new_local_inst().binary(BinaryOp::Mul, iv, two);
        let one = data.new_local_inst().integer(1);
        let derived = data.new_local_inst().binary(BinaryOp::Add, product, one);
        let gep = data
            .new_local_inst()
            .get_elem_ptr(base, vec![outer_index, derived]);
        let load = data.new_local_inst().load(gep);
        let next_iv = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let bound = data.new_local_inst().integer(10);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        for inst in [product, derived, gep, load, next_iv, compare] {
            data.layout_mut().insert_inst(header, inst);
        }
        let header_branch = data
            .new_local_inst()
            .branch(compare, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, header_branch);

        let backedge = data.new_local_inst().jump(header, vec![next_iv]);
        data.layout_mut().insert_inst(latch, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let looop = loops
            .loops()
            .iter()
            .find(|looop| looop.header() == header)
            .unwrap();
        let preheader = looop.get_preheader(&cfg).unwrap();
        assert!(![entry, left, right].contains(&preheader));

        for (jump_inst, initial) in [(left_jump, zero), (right_jump, three)] {
            let InstKind::Jump(jump) = data.inst_data(jump_inst).kind() else {
                panic!("outside edge must remain a jump");
            };
            assert_eq!(jump.target(), preheader);
            assert_eq!(jump.args(), [initial]);
        }

        let preheader_parameter = data.bb_data(preheader).params()[0];
        let preheader_terminator = data.layout().basicblock(preheader).terminator();
        let InstKind::Jump(preheader_jump) = data.inst_data(preheader_terminator).kind() else {
            panic!("created preheader must end in a jump");
        };
        assert_eq!(preheader_jump.target(), header);
        assert_eq!(preheader_jump.args().len(), 2);
        assert_eq!(preheader_jump.args()[0], preheader_parameter);

        let initial_pointer = preheader_jump.args()[1];
        assert_eq!(data.layout().parent_bb(initial_pointer), Some(preheader));
        let InstKind::GetElemPtr(initial_gep) = data.inst_data(initial_pointer).kind() else {
            panic!("preheader must compute the initial pointer");
        };
        let initial_index = *initial_gep.offsets().last().unwrap();
        let InstKind::Binary(initial_add) = data.inst_data(initial_index).kind() else {
            panic!("affine initial index must end in an add");
        };
        assert_eq!(initial_add.op(), BinaryOp::Add);
        assert_eq!(integer_constant(data, initial_add.rhs()), Some(1));
        let InstKind::Binary(initial_product) = data.inst_data(initial_add.lhs()).kind() else {
            panic!("affine initial index must contain a product");
        };
        assert_eq!(initial_product.op(), BinaryOp::Mul);
        assert_eq!(initial_product.lhs(), preheader_parameter);
        assert_eq!(integer_constant(data, initial_product.rhs()), Some(2));

        let pointer = data.bb_data(header).params()[1];
        let InstKind::Jump(backedge) = data.inst_data(backedge).kind() else {
            panic!("latch must remain a jump");
        };
        assert_eq!(backedge.args()[0], next_iv);
        let next_pointer = backedge.args()[1];
        let InstKind::GetElemPtr(next_gep) = data.inst_data(next_pointer).kind() else {
            panic!("latch must compute the next pointer");
        };
        assert_eq!(next_gep.base(), pointer);
        assert_eq!(integer_constant(data, next_gep.offsets()[0]), Some(2));

        let InstKind::GetElemPtr(rewritten_gep) = data.inst_data(gep).kind() else {
            unreachable!()
        };
        assert_eq!(rewritten_gep.base(), pointer);
        assert_eq!(integer_constant(data, rewritten_gep.offsets()[0]), Some(0));
        assert!(!run(&mut program, function));
    }

    #[test]
    fn preserves_distinct_same_target_initial_values() {
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_preheader".into(),
            vec![
                Type::get_pointer(array_ty),
                Type::get_i32(),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let bound = data.new_local_inst().integer(10);
        let outer_index = data.params()[2];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, latch, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let condition = data.new_local_inst().integer(1);
        let zero = data.new_local_inst().integer(0);
        let one = data.new_local_inst().integer(1);
        let entry_branch =
            data.new_local_inst()
                .branch(condition, header, vec![zero], header, vec![one]);
        data.layout_mut().insert_inst(entry, entry_branch);

        let iv = data.bb_data(header).params()[0];
        let gep = data
            .new_local_inst()
            .get_elem_ptr(base, vec![outer_index, iv]);
        let load = data.new_local_inst().load(gep);
        let next_iv = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        for inst in [gep, load, next_iv, compare] {
            data.layout_mut().insert_inst(header, inst);
        }
        let header_branch = data
            .new_local_inst()
            .branch(compare, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, header_branch);
        let backedge = data.new_local_inst().jump(header, vec![next_iv]);
        data.layout_mut().insert_inst(latch, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = LoopAnalysis::new(data);
        let looop = loops
            .loops()
            .iter()
            .find(|looop| looop.header() == header)
            .unwrap();
        let preheader = looop.get_preheader(&cfg).unwrap();
        assert_eq!(preheader, entry);
        assert_eq!(data.bb_data(header).params().len(), 2);

        let InstKind::Branch(entry_branch) = data.inst_data(entry_branch).kind() else {
            unreachable!()
        };
        assert_eq!(entry_branch.t_target(), header);
        assert_eq!(entry_branch.t_args()[0], zero);
        assert_eq!(entry_branch.f_target(), header);
        assert_eq!(entry_branch.f_args()[0], one);
        assert_eq!(entry_branch.t_args().len(), 2);
        assert_eq!(entry_branch.f_args().len(), 2);
        let true_pointer = entry_branch.t_args()[1];
        let false_pointer = entry_branch.f_args()[1];
        assert_ne!(true_pointer, false_pointer);
        let InstKind::GetElemPtr(true_initial) = data.inst_data(true_pointer).kind() else {
            unreachable!()
        };
        let InstKind::GetElemPtr(false_initial) = data.inst_data(false_pointer).kind() else {
            unreachable!()
        };
        assert_eq!(true_initial.offsets().last().copied(), Some(zero));
        assert_eq!(false_initial.offsets().last().copied(), Some(one));
        assert!(!run(&mut program, function));
    }

    #[test]
    fn substitutes_a_passthrough_invariant_header_parameter_in_the_initial_pointer() {
        let array_ty = Type::get_array(Type::get_i32(), 32);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "pointer_sr_header_param".into(),
            vec![Type::get_pointer(array_ty), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let outer_index = data.params()[1];
        // The loop header carries [outer_index, iv]; the backedge passes the
        // first parameter through unchanged, so it is loop-invariant.
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32(), Type::get_i32()]);
        let latch = data.new_basic_block().basic_block("latch".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, latch, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![outer_index, zero]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let header_outer = data.bb_data(header).params()[0];
        let iv = data.bb_data(header).params()[1];
        let gep = data
            .new_local_inst()
            .get_elem_ptr(base, vec![header_outer, iv]);
        let load = data.new_local_inst().load(gep);
        let one = data.new_local_inst().integer(1);
        let next_iv = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        for inst in [gep, load, next_iv] {
            data.layout_mut().insert_inst(header, inst);
        }
        let bound = data.new_local_inst().integer(32);
        let compare = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        data.layout_mut().insert_inst(header, compare);
        let header_branch = data
            .new_local_inst()
            .branch(compare, latch, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, header_branch);

        let backedge = data
            .new_local_inst()
            .jump(header, vec![header_outer, next_iv]);
        data.layout_mut().insert_inst(latch, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.bb_data(header).params().len(), 3);

        let InstKind::Jump(entry_jump) = data.inst_data(entry_jump).kind() else {
            unreachable!()
        };
        let initial_pointer = entry_jump.args()[2];
        let InstKind::GetElemPtr(initial) = data.inst_data(initial_pointer).kind() else {
            unreachable!()
        };
        assert_eq!(initial.base(), base);
        // The invariant header parameter is replaced by the preheader edge
        // argument (the `outer_index` function parameter).
        assert_eq!(initial.offsets().len(), 2);
        assert_eq!(initial.offsets()[0], outer_index);
        assert_eq!(integer_constant(data, initial.offsets()[1]), Some(0));

        let InstKind::Jump(backedge) = data.inst_data(backedge).kind() else {
            unreachable!()
        };
        let next_pointer = backedge.args()[2];
        let InstKind::GetElemPtr(next) = data.inst_data(next_pointer).kind() else {
            unreachable!()
        };
        let pointer = data.bb_data(header).params()[2];
        assert_eq!(next.base(), pointer);
        assert_eq!(integer_constant(data, next.offsets()[0]), Some(1));
        assert!(!run(&mut program, function));
    }
}
