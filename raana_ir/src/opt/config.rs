#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptimizationLevel {
    O0,
    O1,
    O2,
}

impl TryFrom<u8> for OptimizationLevel {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::O0),
            1 => Ok(Self::O1),
            2 => Ok(Self::O2),
            value => Err(value),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetIsa {
    Aarch64,
    Riscv64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetPolicy {
    pub isa: TargetIsa,
    pub enable_chain_to_switch: bool,
}

impl TargetPolicy {
    pub const fn aarch64() -> Self {
        Self {
            isa: TargetIsa::Aarch64,
            enable_chain_to_switch: true,
        }
    }

    pub const fn riscv64() -> Self {
        Self {
            isa: TargetIsa::Riscv64,
            enable_chain_to_switch: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopUnrollMode {
    Enabled,
    Disabled,
    DryRun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PassesConfig {
    pub opt_level: OptimizationLevel,
    pub target: TargetPolicy,
    pub loop_unroll: LoopUnrollMode,
    pub collect_stats: bool,
}

impl PassesConfig {
    pub const fn new(opt_level: OptimizationLevel, target: TargetPolicy) -> Self {
        Self {
            opt_level,
            target,
            loop_unroll: match opt_level {
                OptimizationLevel::O0 => LoopUnrollMode::Disabled,
                OptimizationLevel::O1 | OptimizationLevel::O2 => LoopUnrollMode::Enabled,
            },
            collect_stats: false,
        }
    }
}
