//! Target-specific MIR passes for the AArch64 backend.

pub mod dce;
pub mod list_scheduler;
pub mod pair_combine;
pub mod peephole_combine;

use taki_mir::passes::MIRPassPipeline;

use crate::config::AArch64CodegenConfig;
use crate::instructions::MInst;

/// Build the AArch64 MIR pass pipeline.
///
/// Pre-RA passes run after lowering and before register allocation.
/// Post-RA passes run after `finalize_for_emission` and before emission.
pub fn build_pipeline(config: &AArch64CodegenConfig) -> MIRPassPipeline<MInst> {
    let mut pipeline = MIRPassPipeline::new();
    if config.dce {
        pipeline.add_pre_ra(Box::new(dce::DeadCodeElim));
    }
    if config.peephole_combine {
        pipeline.add_pre_ra(Box::new(peephole_combine::PeepholeCombine));
    }
    if config.pair_combine {
        pipeline.add_post_ra(Box::new(pair_combine::PairCombine));
    }
    if config.list_scheduler {
        pipeline.add_post_ra(Box::new(list_scheduler::ListScheduler));
    }
    pipeline
}
