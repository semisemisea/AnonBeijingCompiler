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
        // Hoisted pure calls must have their arguments rewritten too: a call
        // argument that is a block parameter (header or single-predecessor
        // body parameter) is only available inside the loop, while the hoisted
        // call lands in the preheader. Leaving the parameter in place made the
        // hoisted call use a value whose definition does not dominate it
        // (fft0/fft1/fft2 VCode SSA verification panic).
        InstKind::Call(call) => {
            let callee = call.callee();
            let args = call.args().iter().map(|&arg| substitute(arg)).collect();
            data.replace_inst_with(inst).call(callee, args);
        }
        InstKind::TailCall(tail) => {
            let callee = tail.callee();
            let args = tail.args().iter().map(|&arg| substitute(arg)).collect();
            data.replace_inst_with(inst).tail_call(callee, args);
        }
        _ => {}
    }
}

mod solver;

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
mod tests;
