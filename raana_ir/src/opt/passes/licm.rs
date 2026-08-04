use rustc_hash::{FxHashMap, FxHashSet};

use crate::opt::{
    analysis_passes::{
        dom_tree::v2::DominanceTree,
        effects::{AbstractObject, EffectAnalysis, WriteRoot},
        loop_analysis::Loop,
    },
    prelude::*,
    utils::{
        cfg::CFG,
        logical_edge::incoming_edges,
        preheader::{EnsurePreheader, ensure_preheader},
    },
};

/// Hoisting a large bank of address-computed loads extends all of their live
/// ranges across the loop and commonly costs more spill traffic than it saves.
/// Direct scalar loads remain eligible because they do not create the same
/// address/value register bank.
const MAX_HOISTED_COMPUTED_LOADS: usize = 8;

/// Loop invariant code motion
pub struct LICM {
    /// Whole-program purity / alias analysis, rebuilt on every `Pass::run`.
    /// Loads are only hoisted when no loop instruction may write the loaded
    /// address (see `load_hoist_safe`); without the analysis (direct
    /// `run_on` use) loads stay put.
    analysis: Option<EffectAnalysis>,
    limit_computed_loads: bool,
}

impl LICM {
    pub fn new() -> LICM {
        Self::with_computed_load_limit(true)
    }

    pub fn with_computed_load_limit(limit_computed_loads: bool) -> LICM {
        LICM {
            analysis: None,
            limit_computed_loads,
        }
    }
}

impl Default for LICM {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lattice {
    Variant,
    Invariant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopResult {
    Unchanged,
    Changed,
    CfgChanged,
}

/// Rewrites `inst` in place, replacing loop-invariant header parameter operands
/// with their preheader edge arguments. Only the instruction kinds `LICM` hoists
/// can carry such operands.
fn substitute_header_params(
    data: &mut ArenaContextMut<'_>,
    inst: Inst,
    substitution: &FxHashMap<Inst, Inst>,
) {
    let substitute = |value: Inst| -> Inst { substitution.get(&value).copied().unwrap_or(value) };
    match data.inst_data(inst).kind().clone() {
        InstKind::GetElemPtr(gep) => {
            let base = substitute(gep.base());
            let offsets = gep
                .offsets()
                .iter()
                .map(|&offset| substitute(offset))
                .collect();
            data.replace_inst_with(inst).get_elem_ptr(base, offsets);
        }
        InstKind::Binary(binary) => {
            let lhs = substitute(binary.lhs());
            let rhs = substitute(binary.rhs());
            data.replace_inst_with(inst).binary(binary.op(), lhs, rhs);
        }
        InstKind::Cast(cast) => {
            let src = substitute(cast.src());
            let ty = data.inst_data(inst).ty().clone();
            data.replace_inst_with(inst).cast(src, ty);
        }
        InstKind::Select(select) => {
            let cond = substitute(select.cond());
            let if_true = substitute(select.if_true());
            let if_false = substitute(select.if_false());
            data.replace_inst_with(inst).select(cond, if_true, if_false);
        }
        _ => {}
    }
}

impl LICM {
    fn solve(
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
            let mut still_deferred = Vec::with_capacity(deferred.len());
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
                Some(block) => {
                    !looop.contains(block) && dom_tree.dominates(block, looop.header())
                }
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

/// Whether hoisting `inst` out of the loop is safe with respect to memory
/// ordering. Non-loads are always safe; a load is safe only when no loop
/// write (store, MemZero, or call) may write its address. Without the
/// whole-program analysis, loads are conservatively not hoisted.
fn load_hoist_safe(
    analysis: &Option<EffectAnalysis>,
    inst: Inst,
    func: Function,
    data: &ArenaContextMut<'_>,
    loop_writes: &[Inst],
) -> bool {
    let InstKind::Load(load) = data.inst_data(inst).kind() else {
        return true;
    };
    let Some(analysis) = analysis else {
        return false;
    };
    let addr = load.src();
    loop_writes
        .iter()
        .all(|&write| match data.inst_data(write).kind() {
            InstKind::Store(store) => !analysis.alias(data, func, addr, store.dest()).may_alias(),
            InstKind::MemZero(mem_zero) => !analysis
                .alias(data, func, addr, mem_zero.dest())
                .may_alias(),
            InstKind::Call(call) => {
                let targets = analysis.targets_of(data, func, addr);
                !analysis.call_may_write(call.callee(), targets.as_ref())
            }
            InstKind::TailCall(tail_call) => {
                let targets = analysis.targets_of(data, func, addr);
                !analysis.call_may_write(tail_call.callee(), targets.as_ref())
            }
            _ => true,
        })
}

impl Pass for LICM {
    fn run(&mut self, program: &mut Program) -> bool {
        // Rebuild the whole-program purity / alias analysis: the fixed-point
        // manager re-invokes run() after every IR change, so a stale
        // snapshot must never be reused.
        self.analysis = Some(EffectAnalysis::new(program));
        let func_layout = program.function_layout().to_vec();
        let mut changed = false;
        for func in func_layout {
            let mut arena_context = ArenaContextMut {
                program,
                curr_func: Some(func),
            };
            changed |= self.run_on(&mut arena_context);
        }
        changed
    }

    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().is_decl() {
            return false;
        }
        // Direct invocations (unit tests, pre-run_on callers) have no
        // analysis yet; build one locally. Function-level side effects do
        // not change while this pass runs (LICM only relocates
        // instructions), so analyzing once per invocation is enough.
        if self.analysis.is_none() {
            self.analysis = Some(EffectAnalysis::new(data.program));
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
            let (cfg, dom_tree, loop_analysis) = loop_analysis::LoopAnalysis::from_cfg(cfg);
            let mut rebuild = false;

            // Loops are ordered from small to big. This lets an instruction
            // hoisted from an inner loop be considered by its outer loop.
            for looop in loop_analysis.loops() {
                match Self::solve(
                    looop,
                    &self.analysis,
                    data,
                    &cfg,
                    &dom_tree,
                    &parameter_blocks,
                    self.limit_computed_loads,
                ) {
                    LoopResult::Unchanged => {}
                    LoopResult::Changed => changed = true,
                    LoopResult::CfgChanged => {
                        changed = true;
                        rebuild = true;
                        break;
                    }
                }
            }

            if !rebuild {
                return changed;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        Program, Type,
        builder_trait::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
    };

    fn run(program: &mut Program, function: Function) -> bool {
        let mut context = ArenaContextMut {
            program,
            curr_func: Some(function),
        };
        LICM::new().run_on(&mut context)
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

    #[test]
    fn hoists_transitive_pure_invariants_in_dependency_order() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "licm".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let external = data.params()[0];
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

        let induction = data.bb_data(header).params()[0];
        let one = data.new_local_inst().integer(1);
        let invariant_one = data.new_local_inst().binary(BinaryOp::Add, external, one);
        let invariant_two = data
            .new_local_inst()
            .binary(BinaryOp::Mul, invariant_one, one);
        let variant = data.new_local_inst().binary(BinaryOp::Add, induction, one);
        let slot = data.new_local_inst().alloc(Type::get_i32());
        let store = data.new_local_inst().store(invariant_two, slot);
        for inst in [invariant_one, invariant_two, variant, slot, store] {
            data.layout_mut().insert_inst(header, inst);
        }
        let condition = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);

        let backedge = data.new_local_inst().jump(header, vec![variant]);
        data.layout_mut().insert_inst(body, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.layout().parent_bb(invariant_one), Some(entry));
        assert_eq!(data.layout().parent_bb(invariant_two), Some(entry));
        assert_eq!(data.layout().parent_bb(variant), Some(header));
        assert_eq!(data.layout().parent_bb(slot), Some(header));
        assert_eq!(data.layout().parent_bb(store), Some(header));
        assert_eq!(data.layout().parent_bb(branch), Some(header));
        assert_eq!(data.layout().parent_bb(backedge), Some(body));

        let preheader_insts = data
            .layout()
            .basicblock(entry)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let first = preheader_insts
            .iter()
            .position(|&inst| inst == invariant_one)
            .unwrap();
        let second = preheader_insts
            .iter()
            .position(|&inst| inst == invariant_two)
            .unwrap();
        let terminator = preheader_insts
            .iter()
            .position(|&inst| inst == entry_jump)
            .unwrap();
        assert!(first < second && second < terminator);
    }

    #[test]
    fn hoists_pure_binary_ops_but_not_memory_side_effects() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "licm_effects".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data.new_basic_block().basic_block("header".into(), vec![]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let slot = data.new_local_inst().alloc(Type::get_i32());
        data.layout_mut().insert_inst(entry, slot);
        let entry_jump = data.new_local_inst().jump(header, vec![]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let one = data.new_local_inst().integer(1);
        let zero = data.new_local_inst().integer(0);
        let div = data.new_local_inst().binary(BinaryOp::Div, one, zero);
        let rem = data.new_local_inst().binary(BinaryOp::Rem, one, zero);
        let load = data.new_local_inst().load(slot);
        let store = data.new_local_inst().store(one, slot);
        for inst in [div, rem, load, store] {
            data.layout_mut().insert_inst(header, inst);
        }
        let condition = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);
        let backedge = data.new_local_inst().jump(header, vec![]);
        data.layout_mut().insert_inst(body, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.layout().parent_bb(div), Some(entry));
        assert_eq!(data.layout().parent_bb(rem), Some(entry));
        for inst in [load, store] {
            assert_eq!(data.layout().parent_bb(inst), Some(header));
        }
    }

    #[test]
    fn creates_a_preheader_before_hoisting() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "licm_no_preheader".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let side = data.new_basic_block().basic_block("side".into(), vec![]);
        let header = data.new_basic_block().basic_block("header".into(), vec![]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [side, header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let condition = data.new_local_inst().integer(1);
        let entry_branch = data
            .new_local_inst()
            .branch(condition, header, vec![], side, vec![]);
        data.layout_mut().insert_inst(entry, entry_branch);
        let side_ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(side, side_ret);

        let one = data.new_local_inst().integer(1);
        let invariant = data.new_local_inst().binary(BinaryOp::Add, one, one);
        data.layout_mut().insert_inst(header, invariant);
        let header_branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, header_branch);
        let backedge = data.new_local_inst().jump(header, vec![]);
        data.layout_mut().insert_inst(body, backedge);
        let exit_ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, exit_ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = loop_analysis::LoopAnalysis::new(data);
        let looop = loops
            .loops()
            .iter()
            .find(|looop| looop.header() == header)
            .unwrap();
        let preheader = looop.get_preheader(&cfg).unwrap();
        assert_ne!(preheader, entry);
        assert_eq!(data.layout().parent_bb(invariant), Some(preheader));
        let insts = data.layout().basicblock(preheader).insts();
        let invariant_position = insts.iter().position(|&inst| inst == invariant).unwrap();
        let terminator_position = insts.len() - 1;
        assert!(invariant_position < terminator_position);
        assert!(!run(&mut program, function));
    }

    #[test]
    fn does_not_create_a_preheader_without_hoist_candidates() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "licm_no_candidate".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let side = data.new_basic_block().basic_block("side".into(), vec![]);
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [side, header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let condition = data.new_local_inst().integer(1);
        let zero = data.new_local_inst().integer(0);
        let entry_branch =
            data.new_local_inst()
                .branch(condition, header, vec![zero], side, vec![]);
        data.layout_mut().insert_inst(entry, entry_branch);
        let side_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(side, side_jump);

        let induction = data.bb_data(header).params()[0];
        let one = data.new_local_inst().integer(1);
        let next = data.new_local_inst().binary(BinaryOp::Add, induction, one);
        data.layout_mut().insert_inst(header, next);
        let header_branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, header_branch);
        let backedge = data.new_local_inst().jump(header, vec![next]);
        data.layout_mut().insert_inst(body, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        let block_count = data.layout().basicblocks().len();
        assert!(!run(&mut program, function));
        assert_eq!(
            program.func_data(function).layout().basicblocks().len(),
            block_count
        );
    }

    #[test]
    fn does_not_rewrite_an_entry_header_loop() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "licm_entry_loop".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let one = data.new_local_inst().integer(1);
        let invariant = data.new_local_inst().binary(BinaryOp::Add, one, one);
        data.layout_mut().insert_inst(entry, invariant);
        let condition = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(entry, branch);
        let backedge = data.new_local_inst().jump(entry, vec![]);
        data.layout_mut().insert_inst(body, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(!run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.layout().entry_bb().unwrap().bb(), entry);
        assert_eq!(data.layout().parent_bb(invariant), Some(entry));
    }

    #[test]
    fn rebuilds_analyses_after_each_created_preheader() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "licm_rebuild".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let side_a = data.new_basic_block().basic_block("side_a".into(), vec![]);
        let header_a = data
            .new_basic_block()
            .basic_block("header_a".into(), vec![]);
        let latch_a = data.new_basic_block().basic_block("latch_a".into(), vec![]);
        let after_a = data.new_basic_block().basic_block("after_a".into(), vec![]);
        let side_b = data.new_basic_block().basic_block("side_b".into(), vec![]);
        let header_b = data
            .new_basic_block()
            .basic_block("header_b".into(), vec![]);
        let latch_b = data.new_basic_block().basic_block("latch_b".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [
            side_a, header_a, latch_a, after_a, side_b, header_b, latch_b, exit,
        ] {
            data.layout_mut().push_bb_back(block);
        }

        let condition = data.new_local_inst().integer(1);
        let entry_branch =
            data.new_local_inst()
                .branch(condition, header_a, vec![], side_a, vec![]);
        data.layout_mut().insert_inst(entry, entry_branch);
        let side_a_jump = data.new_local_inst().jump(header_a, vec![]);
        data.layout_mut().insert_inst(side_a, side_a_jump);

        let one = data.new_local_inst().integer(1);
        let invariant_a = data.new_local_inst().binary(BinaryOp::Add, one, one);
        data.layout_mut().insert_inst(header_a, invariant_a);
        let branch_a = data
            .new_local_inst()
            .branch(condition, latch_a, vec![], after_a, vec![]);
        data.layout_mut().insert_inst(header_a, branch_a);
        let backedge_a = data.new_local_inst().jump(header_a, vec![]);
        data.layout_mut().insert_inst(latch_a, backedge_a);

        let after_a_branch =
            data.new_local_inst()
                .branch(condition, header_b, vec![], side_b, vec![]);
        data.layout_mut().insert_inst(after_a, after_a_branch);
        let side_b_jump = data.new_local_inst().jump(header_b, vec![]);
        data.layout_mut().insert_inst(side_b, side_b_jump);

        let invariant_b = data.new_local_inst().binary(BinaryOp::Mul, one, one);
        data.layout_mut().insert_inst(header_b, invariant_b);
        let branch_b = data
            .new_local_inst()
            .branch(condition, latch_b, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header_b, branch_b);
        let backedge_b = data.new_local_inst().jump(header_b, vec![]);
        data.layout_mut().insert_inst(latch_b, backedge_b);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = loop_analysis::LoopAnalysis::new(data);
        for (header, invariant) in [(header_a, invariant_a), (header_b, invariant_b)] {
            let looop = loops
                .loops()
                .iter()
                .find(|looop| looop.header() == header)
                .unwrap();
            let preheader = looop.get_preheader(&cfg).unwrap();
            assert_eq!(data.layout().parent_bb(invariant), Some(preheader));
        }
        assert!(!run(&mut program, function));
    }

    #[test]
    fn hoists_fully_invariant_gep() {
        let array_ty = Type::get_array(Type::get_array(Type::get_i32(), 8), 8);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "licm_gep".into(),
            vec![Type::get_pointer(array_ty), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let row = data.params()[1];
        let header = data.new_basic_block().basic_block("header".into(), vec![]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let entry_jump = data.new_local_inst().jump(header, vec![]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let zero = data.new_local_inst().integer(0);
        let gep = data.new_local_inst().get_elem_ptr(base, vec![zero, row]);
        data.layout_mut().insert_inst(header, gep);
        let condition = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);
        let backedge = data.new_local_inst().jump(header, vec![]);
        data.layout_mut().insert_inst(body, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.layout().parent_bb(gep), Some(entry));
        let entry_insts = data.layout().basicblock(entry).insts();
        let gep_position = entry_insts.iter().position(|&inst| inst == gep).unwrap();
        let terminator_position = entry_insts
            .iter()
            .position(|&inst| inst == entry_jump)
            .unwrap();
        assert!(gep_position < terminator_position);
    }

    #[test]
    fn hoists_invariant_gep_prefix_and_preserves_original_value() {
        let array_ty = Type::get_array(Type::get_array(Type::get_i32(), 8), 8);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "licm_partial_gep".into(),
            vec![Type::get_pointer(array_ty), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let row = data.params()[1];
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
        let column = data.bb_data(header).params()[0];
        let gep = data
            .new_local_inst()
            .get_elem_ptr(base, vec![zero, row, column]);
        let original_ty = data.inst_data(gep).ty().clone();
        let load = data.new_local_inst().load(gep);
        for inst in [gep, load] {
            data.layout_mut().insert_inst(header, inst);
        }
        let one = data.new_local_inst().integer(1);
        let next = data.new_local_inst().binary(BinaryOp::Add, column, one);
        data.layout_mut().insert_inst(header, next);
        let condition = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);
        let backedge = data.new_local_inst().jump(header, vec![next]);
        data.layout_mut().insert_inst(body, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.layout().parent_bb(gep), Some(header));
        assert_eq!(data.inst_data(gep).ty(), &original_ty);
        assert_eq!(
            data.inst_data(load).inst_usage().collect::<Vec<_>>(),
            vec![gep]
        );

        let InstKind::GetElemPtr(suffix) = data.inst_data(gep).kind() else {
            panic!("original value must remain a GEP");
        };
        assert_eq!(suffix.offsets().len(), 2);
        assert!(matches!(
            data.inst_data(suffix.offsets()[0]).kind(),
            InstKind::Integer(value) if value.value() == 0
        ));
        assert_eq!(suffix.offsets()[1], column);

        let prefix = suffix.base();
        assert_eq!(data.layout().parent_bb(prefix), Some(entry));
        let InstKind::GetElemPtr(prefix_gep) = data.inst_data(prefix).kind() else {
            panic!("partial motion must create a prefix GEP");
        };
        assert_eq!(prefix_gep.base(), base);
        assert_eq!(prefix_gep.offsets(), &[zero, row]);
        assert!(data.inst_data(prefix).used_by().contains(&gep));
        assert!(!data.inst_data(base).used_by().contains(&gep));

        assert!(!run(&mut program, function));
    }

    // --- load hoisting (requires the whole-program purity / alias analysis) ---

    fn run_with_analysis(program: &mut Program) -> bool {
        LICM::new().run(program)
    }

    fn new_global(program: &mut Program) -> Inst {
        let init = program.new_value().zero_init(Type::get_i32());
        program.new_value().global_alloc(init)
    }

    /// A loop `entry -> header -> (body -> header | exit)` where `entry` is
    /// the dedicated preheader; `fill_header` adds the header instructions
    /// (the terminator is appended by the helper).
    fn build_loop(
        program: &mut Program,
        name: &str,
        fill_header: impl FnOnce(&mut ArenaContextMut<'_>, BasicBlock, BasicBlock, BasicBlock),
    ) -> Function {
        let function = program.new_function(Type::get_unit(), name.into(), vec![]);
        let mut data = ArenaContextMut {
            program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let header = data.new_basic_block().basic_block("header".into(), vec![]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }
        let entry_jump = data.new_local_value().jump(header, vec![]);
        data.layout_mut().insert_inst(entry, entry_jump);

        fill_header(&mut data, header, body, exit);

        let condition = data.new_local_value().integer(1);
        let branch = data
            .new_local_value()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);
        let backedge = data.new_local_value().jump(header, vec![]);
        data.layout_mut().insert_inst(body, backedge);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(exit, ret);
        function
    }

    #[test]
    fn hoists_load_with_no_may_alias_write() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        let mut load = None;
        let function = build_loop(&mut program, "licm_load", |data, header, _body, _exit| {
            let l = data.new_local_value().load(global);
            data.layout_mut().insert_inst(header, l);
            load = Some(l);
        });

        assert!(run_with_analysis(&mut program));
        let load = load.unwrap();
        let data = program.func_data(function);
        assert_eq!(
            data.layout().parent_bb(load),
            Some(data.layout().entry_bb().unwrap().bb())
        );
    }

    #[test]
    fn retains_large_banks_of_computed_loads_in_the_loop() {
        let mut program = Program::new();
        let globals = (0..=MAX_HOISTED_COMPUTED_LOADS)
            .map(|_| new_global(&mut program))
            .collect::<Vec<_>>();
        let mut loads = Vec::new();
        let mut addresses = Vec::new();
        let mut loop_header = None;
        let function = build_loop(
            &mut program,
            "licm_computed_load_bank",
            |data, header, _body, _exit| {
                loop_header = Some(header);
                let zero = data.new_local_value().integer(0);
                for global in globals {
                    let address = data.new_local_value().get_elem_ptr(global, vec![zero]);
                    let load = data.new_local_value().load(address);
                    data.layout_mut().insert_inst(header, address);
                    data.layout_mut().insert_inst(header, load);
                    addresses.push(address);
                    loads.push(load);
                }
            },
        );

        assert!(!run_with_analysis(&mut program));
        let data = program.func_data(function);
        for load in loads {
            assert_eq!(data.layout().parent_bb(load), loop_header);
        }
        for address in addresses {
            assert_eq!(data.layout().parent_bb(address), loop_header);
        }
    }

    #[test]
    fn target_without_load_bank_limit_hoists_computed_loads() {
        let mut program = Program::new();
        let globals = (0..=MAX_HOISTED_COMPUTED_LOADS)
            .map(|_| new_global(&mut program))
            .collect::<Vec<_>>();
        let mut loads = Vec::new();
        let function = build_loop(
            &mut program,
            "licm_unlimited_computed_load_bank",
            |data, header, _body, _exit| {
                let zero = data.new_local_value().integer(0);
                for global in globals {
                    let address = data.new_local_value().get_elem_ptr(global, vec![zero]);
                    let load = data.new_local_value().load(address);
                    data.layout_mut().insert_inst(header, address);
                    data.layout_mut().insert_inst(header, load);
                    loads.push(load);
                }
            },
        );

        assert!(LICM::with_computed_load_limit(false).run(&mut program));
        let data = program.func_data(function);
        let entry = data.layout().entry_bb().unwrap().bb();
        for load in loads {
            assert_eq!(data.layout().parent_bb(load), Some(entry));
        }
    }

    #[test]
    fn does_not_hoist_load_when_loop_writes_same_global() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        let mut load = None;
        let function = build_loop(&mut program, "licm_load_write", |data, header, _b, _e| {
            let l = data.new_local_value().load(global);
            data.layout_mut().insert_inst(header, l);
            load = Some(l);
            let one = data.new_local_value().integer(1);
            let store = data.new_local_value().store(one, global);
            data.layout_mut().insert_inst(header, store);
        });

        // Nothing is hoistable: the store cannot move and the load is
        // blocked by the same-global write, so the pass reports no change.
        assert!(!run_with_analysis(&mut program));
        let data = program.func_data(function);
        let entry = data.layout().entry_bb().unwrap().bb();
        assert_ne!(data.layout().parent_bb(load.unwrap()), Some(entry));
    }

    #[test]
    fn hoists_load_across_write_to_different_global() {
        let mut program = Program::new();
        let global_a = new_global(&mut program);
        let global_b = new_global(&mut program);
        let mut load = None;
        let function = build_loop(&mut program, "licm_load_noalias", |data, header, _b, _e| {
            let l = data.new_local_value().load(global_a);
            data.layout_mut().insert_inst(header, l);
            load = Some(l);
            let one = data.new_local_value().integer(1);
            let store = data.new_local_value().store(one, global_b);
            data.layout_mut().insert_inst(header, store);
        });

        assert!(run_with_analysis(&mut program));
        let data = program.func_data(function);
        assert_eq!(
            data.layout().parent_bb(load.unwrap()),
            Some(data.layout().entry_bb().unwrap().bb())
        );
    }

    #[test]
    fn does_not_hoist_load_across_call_that_writes() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        // `writer` stores into the global.
        let writer = program.new_function(Type::get_unit(), "writer".into(), vec![]);
        {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(writer),
            };
            let entry = data.add_entry_block();
            let one = data.new_local_value().integer(1);
            let store = data.new_local_value().store(one, global);
            data.layout_mut().insert_inst(entry, store);
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(entry, ret);
        }
        let mut load = None;
        let function = build_loop(&mut program, "licm_load_call", |data, header, _b, _e| {
            let l = data.new_local_value().load(global);
            data.layout_mut().insert_inst(header, l);
            load = Some(l);
            let call = data.new_local_value().call(writer, vec![]);
            data.layout_mut().insert_inst(header, call);
        });

        // The call may write the loaded global, so the load is not
        // hoistable and the pass reports no change.
        assert!(!run_with_analysis(&mut program));
        let data = program.func_data(function);
        let entry = data.layout().entry_bb().unwrap().bb();
        assert_ne!(data.layout().parent_bb(load.unwrap()), Some(entry));
    }

    #[test]
    fn hoists_load_across_pure_call() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        // `helper` has no memory or I/O effects.
        let helper = program.new_function(Type::get_unit(), "helper".into(), vec![]);
        {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(helper),
            };
            let entry = data.add_entry_block();
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(entry, ret);
        }
        let mut load = None;
        let function = build_loop(
            &mut program,
            "licm_load_purecall",
            |data, header, _b, _e| {
                let l = data.new_local_value().load(global);
                data.layout_mut().insert_inst(header, l);
                load = Some(l);
                let call = data.new_local_value().call(helper, vec![]);
                data.layout_mut().insert_inst(header, call);
            },
        );

        assert!(run_with_analysis(&mut program));
        let data = program.func_data(function);
        assert_eq!(
            data.layout().parent_bb(load.unwrap()),
            Some(data.layout().entry_bb().unwrap().bb())
        );
    }

    /// A leaf callee whose body is a bare `ret` of its first parameter (or
    /// `ret void` when `ret_ty` is unit): strictly pure.
    fn make_pure_callee(
        program: &mut Program,
        name: &str,
        params_ty: Vec<Type>,
        ret_ty: Type,
    ) -> Function {
        let func = program.new_function(ret_ty.clone(), name.into(), params_ty);
        let data = program.func_data_mut(func);
        let entry = data.add_entry_block();
        let value = if ret_ty.is_unit() {
            None
        } else {
            Some(data.params()[0])
        };
        let r = data.new_local_inst().ret(value);
        data.layout_mut().insert_inst(entry, r);
        func
    }

    /// A leaf callee that reads global element `gv[0]` and returns it.
    fn make_global_reader(program: &mut Program, name: &str, gv: Inst) -> Function {
        let func = program.new_function(Type::get_i32(), name.into(), vec![]);
        let data = program.func_data_mut(func);
        data.add_entry_block();
        drop(data);
        let mut ctx = ArenaContextMut {
            program,
            curr_func: Some(func),
        };
        let entry = ctx.layout().entry_bb().unwrap().bb();
        let zero = ctx.new_local_value().integer(0);
        let gep = ctx.new_local_value().get_elem_ptr(gv, vec![zero]);
        let load = ctx.new_local_value().load(gep);
        let r = ctx.new_local_value().ret(Some(load));
        ctx.layout_mut().insert_inst(entry, zero);
        ctx.layout_mut().insert_inst(entry, gep);
        ctx.layout_mut().insert_inst(entry, load);
        ctx.layout_mut().insert_inst(entry, r);
        func
    }

    #[test]
    fn hoists_invariant_pure_call() {
        let mut program = Program::new();
        let callee = make_pure_callee(
            &mut program,
            "pure_fn",
            vec![Type::get_i32()],
            Type::get_i32(),
        );
        let function =
            program.new_function(Type::get_unit(), "licm_call".into(), vec![Type::get_i32()]);
        program.func_data_mut(function).add_entry_block();
        let call;
        {
            let mut ctx = ArenaContextMut {
                program: &mut program,
                curr_func: Some(function),
            };
            let external = ctx.curr_func_data().params()[0];
            let entry = ctx.layout().entry_bb().unwrap().bb();
            let header = ctx.new_basic_block().basic_block("header".into(), vec![]);
            let body = ctx.new_basic_block().basic_block("body".into(), vec![]);
            let exit = ctx.new_basic_block().basic_block("exit".into(), vec![]);
            for block in [header, body, exit] {
                ctx.layout_mut().push_bb_back(block);
            }
            let entry_jump = ctx.new_local_value().jump(header, vec![]);
            ctx.layout_mut().insert_inst(entry, entry_jump);
            call = ctx
                .new_local_value()
                .call_with_type(callee, vec![external], Type::get_i32());
            ctx.layout_mut().insert_inst(header, call);
            let condition = ctx.new_local_value().integer(1);
            let branch = ctx
                .new_local_value()
                .branch(condition, body, vec![], exit, vec![]);
            ctx.layout_mut().insert_inst(header, branch);
            let backedge = ctx.new_local_value().jump(header, vec![]);
            ctx.layout_mut().insert_inst(body, backedge);
            let ret = ctx.new_local_value().ret(None);
            ctx.layout_mut().insert_inst(exit, ret);
        }

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(
            data.layout().parent_bb(call),
            Some(data.layout().entry_bb().unwrap().bb())
        );
    }

    #[test]
    fn does_not_hoist_io_call() {
        let mut program = Program::new();
        let getint = program.new_function(Type::get_i32(), "getint".into(), vec![]);
        let function = program.new_function(Type::get_unit(), "licm_io".into(), vec![]);
        program.func_data_mut(function).add_entry_block();
        let call;
        let header;
        {
            let mut ctx = ArenaContextMut {
                program: &mut program,
                curr_func: Some(function),
            };
            let entry = ctx.layout().entry_bb().unwrap().bb();
            header = ctx.new_basic_block().basic_block("header".into(), vec![]);
            let body = ctx.new_basic_block().basic_block("body".into(), vec![]);
            let exit = ctx.new_basic_block().basic_block("exit".into(), vec![]);
            for block in [header, body, exit] {
                ctx.layout_mut().push_bb_back(block);
            }
            let entry_jump = ctx.new_local_value().jump(header, vec![]);
            ctx.layout_mut().insert_inst(entry, entry_jump);
            call = ctx
                .new_local_value()
                .call_with_type(getint, vec![], Type::get_i32());
            ctx.layout_mut().insert_inst(header, call);
            let condition = ctx.new_local_value().integer(1);
            let branch = ctx
                .new_local_value()
                .branch(condition, body, vec![], exit, vec![]);
            ctx.layout_mut().insert_inst(header, branch);
            let backedge = ctx.new_local_value().jump(header, vec![]);
            ctx.layout_mut().insert_inst(body, backedge);
            let ret = ctx.new_local_value().ret(None);
            ctx.layout_mut().insert_inst(exit, ret);
        }

        assert!(!run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(
            data.layout().parent_bb(call),
            Some(data.layout().basicblock(header).bb())
        );
    }

    #[test]
    fn does_not_hoist_call_writing_an_array_parameter() {
        let mut program = Program::new();
        let writer = program.new_function(
            Type::get_unit(),
            "writes_param".into(),
            vec![Type::get_pointer(Type::get_i32())],
        );
        {
            let data = program.func_data_mut(writer);
            let entry = data.add_entry_block();
            let arr = data.params()[0];
            let zero = data.new_local_inst().integer(0);
            let gep = data.new_local_inst().get_elem_ptr(arr, vec![zero]);
            let one = data.new_local_inst().integer(1);
            let store = data.new_local_inst().store(one, gep);
            for inst in [zero, gep, store] {
                data.layout_mut().insert_inst(entry, inst);
            }
            let r = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(entry, r);
        }
        let function = program.new_function(
            Type::get_unit(),
            "licm_writer".into(),
            vec![Type::get_pointer(Type::get_i32())],
        );
        program.func_data_mut(function).add_entry_block();
        let call;
        let header;
        {
            let mut ctx = ArenaContextMut {
                program: &mut program,
                curr_func: Some(function),
            };
            let arr = ctx.curr_func_data().params()[0];
            let entry = ctx.layout().entry_bb().unwrap().bb();
            header = ctx.new_basic_block().basic_block("header".into(), vec![]);
            let body = ctx.new_basic_block().basic_block("body".into(), vec![]);
            let exit = ctx.new_basic_block().basic_block("exit".into(), vec![]);
            for block in [header, body, exit] {
                ctx.layout_mut().push_bb_back(block);
            }
            let entry_jump = ctx.new_local_value().jump(header, vec![]);
            ctx.layout_mut().insert_inst(entry, entry_jump);
            call = ctx
                .new_local_value()
                .call_with_type(writer, vec![arr], Type::get_unit());
            ctx.layout_mut().insert_inst(header, call);
            let condition = ctx.new_local_value().integer(1);
            let branch = ctx
                .new_local_value()
                .branch(condition, body, vec![], exit, vec![]);
            ctx.layout_mut().insert_inst(header, branch);
            let backedge = ctx.new_local_value().jump(header, vec![]);
            ctx.layout_mut().insert_inst(body, backedge);
            let ret = ctx.new_local_value().ret(None);
            ctx.layout_mut().insert_inst(exit, ret);
        }

        assert!(!run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.layout().parent_bb(call), Some(header));
    }

    #[test]
    fn store_to_global_read_by_callee_blocks_hoist() {
        let mut program = Program::new();
        let zero_init = program.new_value().integer(0);
        let gv = program.new_value().global_alloc(zero_init);
        let reader = make_global_reader(&mut program, "reads_global", gv);
        let function = program.new_function(Type::get_unit(), "licm_conflict".into(), vec![]);
        program.func_data_mut(function).add_entry_block();
        let call;
        let header;
        {
            let mut ctx = ArenaContextMut {
                program: &mut program,
                curr_func: Some(function),
            };
            let entry = ctx.layout().entry_bb().unwrap().bb();
            header = ctx.new_basic_block().basic_block("header".into(), vec![]);
            let body = ctx.new_basic_block().basic_block("body".into(), vec![]);
            let exit = ctx.new_basic_block().basic_block("exit".into(), vec![]);
            for block in [header, body, exit] {
                ctx.layout_mut().push_bb_back(block);
            }
            let entry_jump = ctx.new_local_value().jump(header, vec![]);
            ctx.layout_mut().insert_inst(entry, entry_jump);
            call = ctx
                .new_local_value()
                .call_with_type(reader, vec![], Type::get_i32());
            ctx.layout_mut().insert_inst(header, call);
            let zero = ctx.new_local_value().integer(0);
            let gep = ctx.new_local_value().get_elem_ptr(gv, vec![zero]);
            let one = ctx.new_local_value().integer(1);
            let store = ctx.new_local_value().store(one, gep);
            for inst in [gep, store] {
                ctx.layout_mut().insert_inst(body, inst);
            }
            let condition = ctx.new_local_value().integer(1);
            let branch = ctx
                .new_local_value()
                .branch(condition, body, vec![], exit, vec![]);
            ctx.layout_mut().insert_inst(header, branch);
            let backedge = ctx.new_local_value().jump(header, vec![]);
            ctx.layout_mut().insert_inst(body, backedge);
            let ret = ctx.new_local_value().ret(None);
            ctx.layout_mut().insert_inst(exit, ret);
        }

        // The address GEP itself is loop-invariant and may be hoisted; the
        // key guarantee is that the call stays in the loop because a store to
        // a global the callee reads makes the hoist unsafe.
        run(&mut program, function);
        let data = program.func_data(function);
        assert_eq!(data.layout().parent_bb(call), Some(header));
    }

    #[test]
    fn hoists_call_when_loop_writes_are_local() {
        let mut program = Program::new();
        let zero_init = program.new_value().integer(0);
        let gv = program.new_value().global_alloc(zero_init);
        let reader = make_global_reader(&mut program, "reads_global", gv);
        let function = program.new_function(Type::get_unit(), "licm_local_write".into(), vec![]);
        program.func_data_mut(function).add_entry_block();
        let call;
        {
            let mut ctx = ArenaContextMut {
                program: &mut program,
                curr_func: Some(function),
            };
            let entry = ctx.layout().entry_bb().unwrap().bb();
            let header = ctx.new_basic_block().basic_block("header".into(), vec![]);
            let body = ctx.new_basic_block().basic_block("body".into(), vec![]);
            let exit = ctx.new_basic_block().basic_block("exit".into(), vec![]);
            for block in [header, body, exit] {
                ctx.layout_mut().push_bb_back(block);
            }
            let entry_jump = ctx.new_local_value().jump(header, vec![]);
            ctx.layout_mut().insert_inst(entry, entry_jump);
            call = ctx
                .new_local_value()
                .call_with_type(reader, vec![], Type::get_i32());
            ctx.layout_mut().insert_inst(header, call);
            let slot = ctx.new_local_value().alloc(Type::get_i32());
            let one = ctx.new_local_value().integer(1);
            let store = ctx.new_local_value().store(one, slot);
            for inst in [slot, store] {
                ctx.layout_mut().insert_inst(body, inst);
            }
            let condition = ctx.new_local_value().integer(1);
            let branch = ctx
                .new_local_value()
                .branch(condition, body, vec![], exit, vec![]);
            ctx.layout_mut().insert_inst(header, branch);
            let backedge = ctx.new_local_value().jump(header, vec![]);
            ctx.layout_mut().insert_inst(body, backedge);
            let ret = ctx.new_local_value().ret(None);
            ctx.layout_mut().insert_inst(exit, ret);
        }

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(
            data.layout().parent_bb(call),
            Some(data.layout().entry_bb().unwrap().bb())
        );
    }

    #[test]
    fn hoists_an_invariant_referencing_a_passthrough_header_parameter() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "licm_header_param".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let outer = data.params()[0];
        let bound = data.params()[1];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32(), Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![outer, zero]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let header_outer = data.bb_data(header).params()[0];
        let iv = data.bb_data(header).params()[1];
        let two = data.new_local_inst().integer(2);
        let doubled = data
            .new_local_inst()
            .binary(BinaryOp::Mul, header_outer, two);
        let condition = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        let header_branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        for inst in [two, doubled, condition, header_branch] {
            data.layout_mut().insert_inst(header, inst);
        }

        let one = data.new_local_inst().integer(1);
        let next_iv = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let backedge = data
            .new_local_inst()
            .jump(header, vec![header_outer, next_iv]);
        for inst in [one, next_iv, backedge] {
            data.layout_mut().insert_inst(body, inst);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        // The invariant `mul header_outer, 2` is hoisted to the preheader with
        // the passthrough header parameter substituted by its entry argument.
        assert_eq!(data.layout().parent_bb(doubled), Some(entry));
        let InstKind::Binary(binary) = data.inst_data(doubled).kind() else {
            unreachable!()
        };
        assert_eq!(binary.lhs(), outer);
        assert_eq!(binary.rhs(), two);
        assert!(!run(&mut program, function));
    }

    /// conv2d shape: a single-predecessor forwarding block (the inlined `idx`
    /// chain) passes the passthrough header parameter on; an invariant
    /// computation on the forwarded value is hoisted, its operand replaced by
    /// the header's entry argument.
    #[test]
    fn hoists_through_single_predecessor_body_forwarding() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "licm_forward".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let forward = data
            .new_basic_block()
            .basic_block("forward".into(), vec![Type::get_i32()]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, forward, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let five = data.new_local_inst().integer(5);
        let entry_jump = data.new_local_inst().jump(header, vec![five]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let header_param = data.bb_data(header).params()[0];
        let condition = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);

        let forward_jump = data.new_local_inst().jump(forward, vec![header_param]);
        data.layout_mut().insert_inst(body, forward_jump);

        let forwarded = data.bb_data(forward).params()[0];
        let two = data.new_local_inst().integer(2);
        let doubled = data
            .new_local_inst()
            .binary(BinaryOp::Mul, forwarded, two);
        let backedge = data.new_local_inst().jump(header, vec![header_param]);
        for inst in [two, doubled, backedge] {
            data.layout_mut().insert_inst(forward, inst);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.layout().parent_bb(doubled), Some(entry));
        let InstKind::Binary(binary) = data.inst_data(doubled).kind() else {
            unreachable!()
        };
        assert_eq!(binary.lhs(), five);
        assert_eq!(binary.rhs(), two);
        assert!(!run(&mut program, function));
    }

    /// The resolution chain may pass through the preheader's own parameter:
    /// the hoisted instruction lands in the preheader with that parameter as
    /// its operand, which is available at the insertion point by
    /// construction.
    #[test]
    fn resolves_chain_through_preheader_parameter() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "licm_chain".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let side = data.new_basic_block().basic_block("side".into(), vec![]);
        let preheader = data
            .new_basic_block()
            .basic_block("preheader".into(), vec![Type::get_i32()]);
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let forward = data
            .new_basic_block()
            .basic_block("forward".into(), vec![Type::get_i32()]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [side, preheader, header, body, forward, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let seven = data.new_local_inst().integer(7);
        let condition = data.new_local_inst().integer(1);
        let entry_branch = data
            .new_local_inst()
            .branch(condition, preheader, vec![seven], side, vec![]);
        data.layout_mut().insert_inst(entry, entry_branch);
        let side_jump = data.new_local_inst().jump(exit, vec![]);
        data.layout_mut().insert_inst(side, side_jump);

        let pre_param = data.bb_data(preheader).params()[0];
        let pre_jump = data.new_local_inst().jump(header, vec![pre_param]);
        data.layout_mut().insert_inst(preheader, pre_jump);

        let header_param = data.bb_data(header).params()[0];
        let header_branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, header_branch);

        let forward_jump = data.new_local_inst().jump(forward, vec![header_param]);
        data.layout_mut().insert_inst(body, forward_jump);

        let forwarded = data.bb_data(forward).params()[0];
        let two = data.new_local_inst().integer(2);
        let doubled = data
            .new_local_inst()
            .binary(BinaryOp::Mul, forwarded, two);
        let backedge = data.new_local_inst().jump(header, vec![header_param]);
        for inst in [two, doubled, backedge] {
            data.layout_mut().insert_inst(forward, inst);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.layout().parent_bb(doubled), Some(preheader));
        let InstKind::Binary(binary) = data.inst_data(doubled).kind() else {
            unreachable!()
        };
        // forwarded -> header_param -> pre_param: the preheader's own
        // parameter substitutes for the forwarded value.
        assert_eq!(binary.lhs(), pre_param);
        assert_eq!(binary.rhs(), two);
    }

    /// A body parameter whose block has two logical incoming edges (here a
    /// same-target branch) is a real phi and stays variant.
    #[test]
    fn keeps_body_param_with_two_logical_edges_variant() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "licm_two_edges".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let forward = data
            .new_basic_block()
            .basic_block("forward".into(), vec![Type::get_i32()]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, forward, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let header_param = data.bb_data(header).params()[0];
        let condition = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);

        // Same-target branch: two logical edges into `forward`.
        let forward_jump = data.new_local_inst().branch(
            condition,
            forward,
            vec![header_param],
            forward,
            vec![header_param],
        );
        data.layout_mut().insert_inst(body, forward_jump);

        let forwarded = data.bb_data(forward).params()[0];
        let two = data.new_local_inst().integer(2);
        let doubled = data
            .new_local_inst()
            .binary(BinaryOp::Mul, forwarded, two);
        let backedge = data.new_local_inst().jump(header, vec![header_param]);
        for inst in [two, doubled, backedge] {
            data.layout_mut().insert_inst(forward, inst);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(!run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.layout().parent_bb(doubled), Some(forward));
    }

    /// A resolution chain ending at a loop-internal variant value keeps the
    /// parameter variant.
    #[test]
    fn keeps_body_param_resolving_to_loop_variant_variant() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "licm_variant_chain".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let forward = data
            .new_basic_block()
            .basic_block("forward".into(), vec![Type::get_i32()]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, forward, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let header_param = data.bb_data(header).params()[0];
        let condition = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);

        // The forwarded value is `header_param + 1`, computed in the loop:
        // variant, so the chain cannot make `doubled` invariant.
        let one = data.new_local_inst().integer(1);
        let step = data
            .new_local_inst()
            .binary(BinaryOp::Add, header_param, one);
        let forward_jump = data.new_local_inst().jump(forward, vec![step]);
        for inst in [one, step, forward_jump] {
            data.layout_mut().insert_inst(body, inst);
        }

        let forwarded = data.bb_data(forward).params()[0];
        let two = data.new_local_inst().integer(2);
        let doubled = data
            .new_local_inst()
            .binary(BinaryOp::Mul, forwarded, two);
        let backedge = data.new_local_inst().jump(header, vec![step]);
        for inst in [two, doubled, backedge] {
            data.layout_mut().insert_inst(forward, inst);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(!run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.layout().parent_bb(doubled), Some(forward));
    }
}
