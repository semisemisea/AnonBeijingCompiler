//! Candidate discovery and reachability checks for pointer strength reduction.

use super::*;

impl PointerStrengthReduction {
    pub(super) fn find_candidate(
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

    pub(super) fn available_at_header(
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

    /// Upper bound on how many nested `getelemptr` levels (an outer GEP used
    /// as the base of an inner GEP) `has_only_loop_memory_users` will follow
    /// before giving up conservatively. Realistic 2D/3D array access chains
    /// are 2-3 levels deep; anything deeper is rejected to bound compile time.
    pub(super) const MAX_TRANSITIVE_GEP_DEPTH: usize = 8;

    /// Returns whether every use of `gep` inside `looop` is a memory
    /// operation (`Load`/`Store`/`MemZero`) that uses `gep` as its address,
    /// possibly through nested `getelemptr` levels: a GEP that uses `gep` as
    /// its base is accepted iff its own users satisfy the same property. Any
    /// use in a non-address role (stored as data, passed to a call, returned,
    /// ...) anywhere along the chain rejects. This admits the 2D array shape
    /// `b[k][i]` where the outer GEP carries the induction-variable index and
    /// its only user is the inner GEP.
    pub(super) fn has_only_loop_memory_users(
        data: &ArenaContextMut<'_>,
        looop: &Loop,
        gep: Inst,
    ) -> bool {
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

    pub(super) fn only_reaches_candidate(
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
}
