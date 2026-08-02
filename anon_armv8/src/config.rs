//! Explicit AArch64 code-generation configuration.

/// Instruction scheduling model used by the AArch64 scheduler.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AArch64SchedModel {
    #[default]
    CortexA53,
}

/// Configuration controlling target-specific MIR passes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AArch64CodegenConfig {
    pub dce: bool,
    pub peephole_combine: bool,
    pub pair_combine: bool,
    pub list_scheduler: bool,
    pub sched_model: AArch64SchedModel,
    /// Emission-time branch optimization (EmitBuffer rules).
    pub branch_opt: bool,
    /// Pre-RA chain fusion: fold the second compare of a `chain_to_switch`
    /// (check, split) node pair into the check block's compare.
    pub chain_fusion: bool,
}

impl Default for AArch64CodegenConfig {
    fn default() -> Self {
        Self {
            dce: true,
            peephole_combine: true,
            pair_combine: true,
            list_scheduler: true,
            sched_model: AArch64SchedModel::CortexA53,
            branch_opt: true,
            chain_fusion: true,
        }
    }
}
