//! MIR-level pass infrastructure.
//!
//! Mirrors `raana_ir::opt::pass` so that machine-code transformations have the
//! same shape as IR-level passes: each pass is a trait object that reports
//! whether it changed the code, and a pipeline runs them in order.
//!
//! The pipeline is split into two phases that run at different points in
//! `taki_mir::compile`:
//!
//! * **pre-RA** — right after `LowerContext::lower` produces the `VCodeContainer`
//!   and before `reg_alloc::ion::run`. Passes here operate on virtual
//!   registers and must preserve SSA form and operand-traversal order (see
//!   `VCodeContainer::verify_operand_order_stable`).
//! * **post-RA** — right after `VCodeContainer::write_back_allocs` (and after
//!   `MaterializeEdits`, once it lands) and before frame-layout computation /
//!   emission. Passes here operate on physical registers and spill slots.
//!
//! Backends assemble their pipeline via `LowerBackend::mir_pipeline`.

use crate::{prelude::ArenaContext, vcode::VCodeContainer};

/// A MIR-level transformation pass.
///
/// A pass receives the per-function `VCodeContainer` (mutable) plus the
/// immutable program/function context (`ArenaContext`). It must return `true`
/// iff it modified the VCode in any way that requires downstream re-verification
/// of operand tables, CFG side tables, or SSA invariants.
///
/// # Invariants passes must uphold
///
/// 1. **Operand order**: `MachInst::get_operands` must yield the same operand
///    sequence before and after the pass mutates any instruction. The
///    post-allocation `write_back_allocs` and `verify_alloc_output` walks both
///    re-invoke `get_operands` and rely on this order.
/// 2. **SSA preservation** (pre-RA only): every virtual register is defined
///    exactly once and dominates all uses.
/// 3. **Terminator placement**: a block's terminator remains the last
///    instruction of the block.
/// 4. **CFG consistency**: `block_range`, `block_succ`, `block_pred` and the
///    flattened operand tables remain consistent. Use
///    `VCodeContainer::recompute_cfg` if the pass reorders across blocks.
pub trait MIRPass<I: crate::vcode::VCodeInst>: Send + Sync {
    /// Stable identifier used in logs and verifier labels.
    fn name(&self) -> &'static str;

    /// Run on a single function's VCode. Returns `true` if the VCode changed.
    fn run(&self, vcode: &mut VCodeContainer<I>, arena: ArenaContext) -> bool;
}

/// An ordered collection of MIR passes split by the phase at which they run.
///
/// Constructed by `LowerBackend::mir_pipeline`; the default is empty.
pub struct MIRPassPipeline<I: crate::vcode::VCodeInst> {
    pre_ra: Vec<Box<dyn MIRPass<I>>>,
    post_ra: Vec<Box<dyn MIRPass<I>>>,
}

impl<I: crate::vcode::VCodeInst> MIRPassPipeline<I> {
    pub fn new() -> Self {
        MIRPassPipeline {
            pre_ra: Vec::new(),
            post_ra: Vec::new(),
        }
    }

    /// Append a pass that runs after lowering and before register allocation.
    pub fn add_pre_ra(&mut self, pass: Box<dyn MIRPass<I>>) {
        self.pre_ra.push(pass);
    }

    /// Append a pass that runs after `write_back_allocs` and before frame
    /// layout / emission.
    pub fn add_post_ra(&mut self, pass: Box<dyn MIRPass<I>>) {
        self.post_ra.push(pass);
    }

    /// `true` if neither phase has any registered passes.
    pub fn is_empty(&self) -> bool {
        self.pre_ra.is_empty() && self.post_ra.is_empty()
    }

    /// Run every pre-RA pass in order, returning `true` if any pass changed
    /// the VCode. Verifies the operand/CFG invariants before and after the
    /// phase when running in debug builds.
    pub fn run_pre_ra(&self, vcode: &mut VCodeContainer<I>, arena: ArenaContext) -> bool {
        self.run_phase("pre-RA", &self.pre_ra, vcode, arena)
    }

    /// Run every post-RA pass in order. See `run_pre_ra`.
    pub fn run_post_ra(&self, vcode: &mut VCodeContainer<I>, arena: ArenaContext) -> bool {
        self.run_phase("post-RA", &self.post_ra, vcode, arena)
    }

    fn run_phase(
        &self,
        phase: &'static str,
        passes: &[Box<dyn MIRPass<I>>],
        vcode: &mut VCodeContainer<I>,
        arena: ArenaContext,
    ) -> bool {
        if passes.is_empty() {
            return false;
        }
        let func_name = arena
            .curr_func
            .map(|f| arena.program.func_data(f).name().to_string())
            .unwrap_or_else(|| "<unknown>".to_string());

        // TODO(M5): replace this with a fine-grained operand-order stability
        // check once we have passes that mutate instruction operand
        // structures. The existing structural verify() catches metadata
        // desync but not operand-count drift within a single instruction.
        if let Err(error) = vcode.verify(&format!("pre-{phase}-{func_name}")) {
            panic!(
                "VCode verification failed before {phase} pipeline for function {func_name}: {error}"
            );
        }

        let mut any_changed = false;
        for pass in passes {
            let changed = pass.run(vcode, arena);
            log::trace!(
                target: "taki_mir::passes",
                "function={} phase={} pass={} changed={}",
                func_name,
                phase,
                pass.name(),
                changed,
            );
            if changed {
                if let Err(error) =
                    vcode.verify(&format!("post-{phase}-{}-{func_name}", pass.name()))
                {
                    panic!(
                        "VCode verification failed after {phase} pass {} for function {func_name}: {error}",
                        pass.name(),
                    );
                }
                any_changed = true;
            }
        }
        any_changed
    }
}

impl<I: crate::vcode::VCodeInst> Default for MIRPassPipeline<I> {
    fn default() -> Self {
        Self::new()
    }
}
