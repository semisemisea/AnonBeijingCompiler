//! Loop-invariant instruction solver for LICM.

use super::*;

impl LICM {
    pub(super) fn solve(
        looop: &Loop,
        analysis: &Option<EffectAnalysis>,
        data: &mut ArenaContextMut<'_>,
        cfg: &CFG,
        dom_tree: &DominanceTree,
        parameter_blocks: &FxHashMap<Inst, BasicBlock>,
        limit_computed_loads: bool,
    ) -> LoopResult {
        fn insts(looop: &Loop, data: &ArenaContextMut<'_>) -> impl Iterator<Item = Inst> {
            looop
                .body()
                .iter()
                .flat_map(|&bb| data.layout().basicblock(bb).insts())
                .copied()
        }

        fn can_be_invariant(
            kind: &InstKind,
            inst: Inst,
            hoistable_calls: &FxHashSet<Inst>,
        ) -> bool {
            matches!(
                kind,
                InstKind::Integer(..)
                    | InstKind::Float(..)
                    | InstKind::Binary(..)
                    | InstKind::Cast(..)
                    | InstKind::GetElemPtr(..)
                    | InstKind::Select(..)
                    | InstKind::Load(..)
            ) || (matches!(kind, InstKind::Call(..)) && hoistable_calls.contains(&inst))
        }

        fn is_integer_zero(data: &ArenaContextMut<'_>, inst: Inst) -> bool {
            matches!(data.inst_data(inst).kind(), InstKind::Integer(value) if value.value() == 0)
        }

        let loop_params = looop
            .body()
            .iter()
            .flat_map(|&block| data.bb_data(block).params().iter().copied())
            .collect::<FxHashSet<_>>();

        // A header block parameter passed through unchanged on every backedge is
        // loop-invariant. When the loop has exactly one entry edge, hoisting can
        // substitute such a parameter with the entry edge's argument (which is
        // available in the preheader). With multiple entry edges the arguments
        // differ per edge, so hoisting operands that reference header params is
        // unsafe and they stay variant.
        let entry_edges = incoming_edges(data, cfg, looop.header())
            .into_iter()
            .filter(|edge| !looop.contains(edge.source()))
            .collect::<Vec<_>>();
        let backedges = incoming_edges(data, cfg, looop.header())
            .into_iter()
            .filter(|edge| looop.contains(edge.source()))
            .collect::<Vec<_>>();
        let header_params = data.bb_data(looop.header()).params().to_vec();
        let invariant_header_params = header_params
            .iter()
            .enumerate()
            .filter_map(|(position, &parameter)| {
                (entry_edges.len() == 1
                    && backedges
                        .iter()
                        .all(|edge| edge.args(data).get(position) == Some(&parameter)))
                .then_some(parameter)
            })
            .collect::<FxHashSet<_>>();
        let substitution = if let [edge] = entry_edges.as_slice() {
            let edge_args = edge.args(data);
            Some(
                header_params
                    .iter()
                    .zip(edge_args)
                    .filter(|(parameter, argument)| parameter != argument)
                    .map(|(parameter, argument)| (*parameter, *argument))
                    .collect::<FxHashMap<_, _>>(),
            )
        } else {
            None
        };

        // A block parameter whose block has a single logical incoming edge is
        // that edge's argument. Resolving such parameters (transitively, and
        // through the passthrough header-parameter substitution below)
        // exposes invariant computations that reference values forwarded
        // through single-predecessor chains — e.g. conv2d's inlined `idx`
        // address arithmetic, where the row index reaches the inner loop
        // through a chain of forwarding blocks.
        //
        // The seed contains only *passthrough* header parameters (whose
        // backedge passes the parameter itself): their entry-edge argument is
        // the parameter's value in every iteration and is available in the
        // preheader. Loop-carried header parameters (entry X, backedge Y)
        // must NOT resolve to their entry argument — that is how the c/r/kr/kc
        // induction variables of conv2d were once folded to their entry value
        // 0. They stay terminal values and are judged by `resolved_invariant`
        // (their defining block is inside the loop, so they remain variant).
        let mut param_resolutions: FxHashMap<Inst, Inst> = FxHashMap::default();
        if let Some(substitution) = &substitution {
            for &parameter in &invariant_header_params {
                if let Some(&arg) = substitution.get(&parameter) {
                    param_resolutions.insert(parameter, arg);
                }
            }
        }
        let mut deferred: Vec<(Inst, Inst)> = Vec::new();
        for &block in cfg.blocks() {
            let params = data.bb_data(block).params().to_vec();
            if params.is_empty() {
                continue;
            }
            let edges = incoming_edges(data, cfg, block);
            if edges.len() != 1 {
                continue; // real phis and function parameters stay terminal
            }
            let args = edges[0].args(data);
            for (position, &parameter) in params.iter().enumerate() {
                if param_resolutions.contains_key(&parameter) {
                    continue;
                }
                let Some(&arg) = args.get(position) else {
                    continue;
                };
                if arg == parameter {
                    continue; // self-passthrough: no information
                }
                if parameter_blocks.contains_key(&arg) {
                    // The argument is another block's parameter: chain
                    // through its resolution when available. Parameters of
                    // single-edge blocks resolve in a later fixpoint pass;
                    // terminal parameters (multi-edge blocks — e.g. the
                    // loop-carried IVs of enclosing loops) are kept as the
                    // final value and judged by `resolved_invariant`.
                    match param_resolutions.get(&arg) {
                        Some(&value) => {
                            param_resolutions.insert(parameter, value);
                        }
                        None => {
                            let arg_block = parameter_blocks[&arg];
                            if incoming_edges(data, cfg, arg_block).len() == 1 {
                                deferred.push((parameter, arg));
                            } else {
                                param_resolutions.insert(parameter, arg);
                            }
                        }
                    }
                } else {
                    param_resolutions.insert(parameter, arg);
                }
            }
        }
        while !deferred.is_empty() {
            let mut progress = false;
            let still_deferred = Vec::with_capacity(deferred.len());
            for (parameter, arg) in deferred.drain(..) {
                if let Some(&value) = param_resolutions.get(&arg) {
                    param_resolutions.insert(parameter, value);
                    progress = true;
                } else {
                    // The argument never resolved (cyclic chain): keep it as
                    // a terminal value; `resolved_invariant` judges it.
                    param_resolutions.insert(parameter, arg);
                    progress = true;
                }
            }
            deferred = still_deferred;
            if !progress {
                break; // defensive: should not happen with terminal handling
            }
        }

        // A resolved parameter value is invariant when it is a passthrough
        // header parameter or its defining block dominates the loop header.
        // Any value dominating the header lies outside the loop and is
        // available at the preheader insertion point, so hoisting with the
        // substituted operand is dominance-safe (the 8-04 operand-substitution
        // crash cannot recur).
        let resolved_invariant = |value: Inst| -> bool {
            if value.is_global() || data.inst_data(value).kind().is_const() {
                return true;
            }
            if invariant_header_params.contains(&value) {
                return true;
            }
            match data.layout().parent_bb(value) {
                Some(block) => !looop.contains(block) && dom_tree.dominates(block, looop.header()),
                None => parameter_blocks.get(&value).is_some_and(|&block| {
                    !looop.contains(block) && dom_tree.dominates(block, looop.header())
                }),
            }
        };

        let loop_insts = insts(looop, data).collect::<Vec<_>>();
        let mut map = loop_insts
            .iter()
            .copied()
            .map(|inst| (inst, Lattice::Variant))
            .collect::<FxHashMap<_, _>>();

        // Calls that may be hoisted: the callee must be read-only (no I/O,
        // no timer, no external write), and nothing in the loop may write
        // anything the callee reads (stores, memzeroes, or other calls).
        let hoistable_calls =
            loop_insts
                .iter()
                .copied()
                .filter(|&inst| {
                    let InstKind::Call(call) = data.inst_data(inst).kind() else {
                        return false;
                    };
                    let Some(analysis) = analysis else {
                        return false;
                    };
                    if !analysis.is_removable(call.callee()) {
                        return false;
                    }
                    let func = data.curr_func.unwrap();
                    let conflicts = loop_insts.iter().copied().any(|other| {
                        match data.inst_data(other).kind() {
                            InstKind::Store(store) => {
                                let targets = analysis.targets_of(data, func, store.dest());
                                analysis.call_may_read(call.callee(), targets.as_ref())
                            }
                            InstKind::MemZero(mem_zero) => {
                                let targets = analysis.targets_of(data, func, mem_zero.dest());
                                analysis.call_may_read(call.callee(), targets.as_ref())
                            }
                            InstKind::Call(other_call) => {
                                let sibling = other_call.callee();
                                match analysis.call_read_roots(call.callee(), func) {
                                    Some(reads) => {
                                        let reads = reads
                                            .iter()
                                            .map(|r| match r {
                                                WriteRoot::Global(g) => AbstractObject::Global(*g),
                                                WriteRoot::Local(f, a) => {
                                                    AbstractObject::Alloc(*f, *a)
                                                }
                                            })
                                            .collect::<FxHashSet<_>>();
                                        analysis.call_may_write(sibling, Some(&reads))
                                    }
                                    None => analysis.effects_of(sibling).may_write_memory(),
                                }
                            }
                            _ => false,
                        }
                    });
                    !conflicts
                })
                .collect::<FxHashSet<_>>();

        let operand_is_invariant = |operand: Inst, states: &FxHashMap<Inst, Lattice>| {
            if operand.is_global() || data.inst_data(operand).kind().is_const() {
                return true;
            }
            if loop_params.contains(&operand) {
                // Header parameters are invariant only through the
                // backedge-passthrough rule above: the substitution map
                // seeds header-param rewrites, but a loop-carried header
                // parameter (entry value X, backedge value Y) is variant
                // even though the map rewrites it. Body parameters resolve
                // through the single-predecessor chain.
                return invariant_header_params.contains(&operand)
                    || (!header_params.contains(&operand)
                        && param_resolutions
                            .get(&operand)
                            .is_some_and(|&value| resolved_invariant(value)));
            }
            match data.layout().parent_bb(operand) {
                Some(block) if looop.contains(block) => {
                    states.get(&operand) == Some(&Lattice::Invariant)
                }
                Some(block) => dom_tree.dominates(block, looop.header()),
                None if matches!(data.inst_data(operand).kind(), InstKind::BlockArgRef(..)) => {
                    parameter_blocks
                        .get(&operand)
                        .is_some_and(|&block| dom_tree.dominates(block, looop.header()))
                }
                None => false,
            }
        };

        // All memory-writing instructions in the loop body: any of them
        // that may alias a load's address blocks hoisting that load.
        let loop_writes = loop_insts
            .iter()
            .copied()
            .filter(|inst| {
                matches!(
                    data.inst_data(*inst).kind(),
                    InstKind::Store(..)
                        | InstKind::MemZero(..)
                        | InstKind::Call(..)
                        | InstKind::TailCall(..)
                )
            })
            .collect::<Vec<_>>();

        let mut invariant_order = vec![];
        let mut worklist = VecDeque::from_iter(loop_insts.iter().copied());
        while let Some(inst) = worklist.pop_front() {
            let inst_data = data.inst_data(inst);
            let status = if can_be_invariant(inst_data.kind(), inst, &hoistable_calls)
                && inst_data
                    .inst_usage()
                    .all(|operand| operand_is_invariant(operand, &map))
                && load_hoist_safe(analysis, inst, data.curr_func.unwrap(), data, &loop_writes)
            {
                Lattice::Invariant
            } else {
                Lattice::Variant
            };
            let orig = map
                .insert(inst, status)
                .expect("loop instruction was initialized");
            if orig != status {
                invariant_order.push(inst);
                worklist.extend(data.inst_data(inst).used_by().iter().filter_map(|&user| {
                    data.layout()
                        .parent_bb(user)
                        .filter(|&block| looop.contains(block))
                        .map(|_| user)
                }));
            }
        }

        let computed_loads = invariant_order
            .iter()
            .copied()
            .filter(|&inst| {
                let InstKind::Load(load) = data.inst_data(inst).kind() else {
                    return false;
                };
                data.layout()
                    .parent_bb(load.src())
                    .is_some_and(|block| looop.contains(block))
            })
            .collect::<Vec<_>>();
        if limit_computed_loads && computed_loads.len() > MAX_HOISTED_COMPUTED_LOADS {
            let mut retained = VecDeque::from(computed_loads);
            while let Some(inst) = retained.pop_front() {
                if map.insert(inst, Lattice::Variant) != Some(Lattice::Invariant) {
                    continue;
                }
                if let InstKind::Load(load) = data.inst_data(inst).kind() {
                    retained.push_back(load.src());
                } else {
                    retained.extend(data.inst_data(inst).inst_usage().filter(|operand| {
                        data.layout()
                            .parent_bb(*operand)
                            .is_some_and(|block| looop.contains(block))
                    }));
                }
            }
            // Any invariant expression depending on a retained load must stay
            // with it. Iterate to a fixed point because dependency chains may
            // contain address casts or arithmetic before their final use.
            loop {
                let mut downgraded = false;
                for &inst in &invariant_order {
                    if map[&inst] != Lattice::Invariant {
                        continue;
                    }
                    if data.inst_data(inst).inst_usage().any(|operand| {
                        data.layout().parent_bb(operand).is_some_and(|block| {
                            looop.contains(block) && map.get(&operand) == Some(&Lattice::Variant)
                        })
                    }) {
                        map.insert(inst, Lattice::Variant);
                        downgraded = true;
                    }
                }
                if !downgraded {
                    break;
                }
            }
            invariant_order.retain(|inst| map[inst] == Lattice::Invariant);
        }

        let partial_geps = loop_insts
            .into_iter()
            .filter_map(|inst| {
                if map.get(&inst) == Some(&Lattice::Invariant) {
                    return None;
                }
                let InstKind::GetElemPtr(gep) = data.inst_data(inst).kind() else {
                    return None;
                };
                if !operand_is_invariant(gep.base(), &map) {
                    return None;
                }
                let prefix_len = gep
                    .offsets()
                    .iter()
                    .take_while(|&&offset| operand_is_invariant(offset, &map))
                    .count();
                if prefix_len == 0 || prefix_len == gep.offsets().len() {
                    return None;
                }
                if prefix_len == 1 && is_integer_zero(data, gep.offsets()[0]) {
                    return None;
                }
                Some((
                    inst,
                    gep.base(),
                    gep.offsets()[..prefix_len].to_vec(),
                    gep.offsets()[prefix_len..].to_vec(),
                    data.inst_data(inst).ty().clone(),
                ))
            })
            .collect::<Vec<_>>();

        let has_invariant_insts = invariant_order
            .iter()
            .any(|&inst| !data.inst_data(inst).kind().is_const());
        if !has_invariant_insts && partial_geps.is_empty() {
            return LoopResult::Unchanged;
        }

        let Some(preheader) = ensure_preheader(data, cfg, looop) else {
            return LoopResult::Unchanged;
        };
        let preheader = match preheader {
            EnsurePreheader::Existing(preheader) => preheader,
            EnsurePreheader::Created(..) => return LoopResult::CfgChanged,
        };

        let mut changed = false;
        for inst in invariant_order {
            let inst_data = data.inst_data(inst);
            if inst_data.kind().is_const() {
                continue;
            }
            changed = true;
            if !param_resolutions.is_empty() {
                substitute_header_params(data, inst, &param_resolutions);
            }
            let bb = data
                .layout()
                .parent_bb(inst)
                .expect("invariant inst must be in the layout, constants is excluded");
            data.layout_mut().remove_inst(bb, inst);
            data.layout_mut().insert_before_terminator(preheader, inst);
        }

        for (inst, base, prefix_offsets, remaining_offsets, original_ty) in partial_geps {
            let substitute =
                |value: Inst| -> Inst { param_resolutions.get(&value).copied().unwrap_or(value) };
            let prefix = data.new_local_value().get_elem_ptr(
                substitute(base),
                prefix_offsets
                    .iter()
                    .map(|&offset| substitute(offset))
                    .collect(),
            );
            data.layout_mut()
                .insert_before_terminator(preheader, prefix);

            let zero = data.new_local_value().integer(0);
            let mut suffix_offsets = Vec::with_capacity(remaining_offsets.len() + 1);
            suffix_offsets.push(zero);
            suffix_offsets.extend(remaining_offsets);
            data.replace_inst_with(inst)
                .get_elem_ptr(prefix, suffix_offsets);
            assert_eq!(
                data.inst_data(inst).ty(),
                &original_ty,
                "splitting GEP must preserve its result type"
            );
            changed = true;
        }

        if changed {
            LoopResult::Changed
        } else {
            LoopResult::Unchanged
        }
    }
}
