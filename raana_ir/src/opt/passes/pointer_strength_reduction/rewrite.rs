//! Preheader cloning and loop rewrites for pointer strength reduction.

use super::*;

impl PointerStrengthReduction {
    pub(super) fn clone_affine_initial(
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

    pub(super) fn apply_candidate(
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
