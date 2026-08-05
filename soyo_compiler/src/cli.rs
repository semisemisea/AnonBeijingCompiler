use std::path::PathBuf;

#[derive(Debug, clap::ValueEnum, Clone, Copy, PartialEq, Eq)]
#[clap(rename_all = "lowercase")]
pub enum Target {
    Riscv64,
    #[clap(help = "emit GNU AArch64 assembly through the generic VCode pipeline")]
    Aarch64,
}

#[derive(Debug, clap::ValueEnum, Clone, Copy, PartialEq, Eq)]
#[clap(rename_all = "kebab-case")]
pub enum SchedModel {
    CortexA53,
}

#[derive(Debug, clap::ValueEnum, Clone, Copy, PartialEq, Eq)]
#[clap(rename_all = "kebab-case")]
pub enum LoopUnrollMode {
    On,
    Off,
    DryRun,
}

#[derive(Debug, clap::Parser)]
pub(crate) struct Arg {
    #[arg(
        long,
        value_name = "FILTER",
        help = "logging filter (overrides RUST_LOG; default: warn)"
    )]
    pub(crate) log: Option<String>,
    #[arg(short = 'S', default_value_t = false, conflicts_with = "emit")]
    pub(crate) assembly_only: bool,
    #[arg(value_name = "INPUT")]
    pub(crate) input_path: PathBuf,
    #[arg(short = 'o', value_name = "OUTPUT")]
    pub(crate) output_path: PathBuf,
    #[arg(short = 'O', default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=2))]
    pub(crate) opt_level: u8,
    #[arg(
        long,
        value_enum,
        value_delimiter = ',',
        help = "emit ir, asm, or ir,asm"
    )]
    pub(crate) emit: Vec<EmitOption>,
    #[arg(
        long = "target",
        value_enum,
        default_value_t = Target::Riscv64
    )]
    pub(crate) target: Target,
    #[arg(
        long,
        value_enum,
        help = "override loop unrolling: on, off, or analysis-only dry-run"
    )]
    pub(crate) loop_unroll: Option<LoopUnrollMode>,
    #[arg(long, help = "print structured IR pass statistics to stderr")]
    pub(crate) pass_stats: bool,
    #[arg(long, conflicts_with = "disable_mir_dce")]
    pub(crate) enable_mir_dce: bool,
    #[arg(long, conflicts_with = "enable_mir_dce")]
    pub(crate) disable_mir_dce: bool,
    #[arg(long, conflicts_with = "disable_mir_peephole")]
    pub(crate) enable_mir_peephole: bool,
    #[arg(long, conflicts_with = "enable_mir_peephole")]
    pub(crate) disable_mir_peephole: bool,
    #[arg(long, conflicts_with = "disable_pair_combine")]
    pub(crate) enable_pair_combine: bool,
    #[arg(long, conflicts_with = "enable_pair_combine")]
    pub(crate) disable_pair_combine: bool,
    #[arg(long, conflicts_with = "disable_sched")]
    pub(crate) enable_sched: bool,
    #[arg(long, conflicts_with = "enable_sched")]
    pub(crate) disable_sched: bool,
    #[arg(long, conflicts_with = "disable_const_cse")]
    pub(crate) enable_const_cse: bool,
    #[arg(long, conflicts_with = "enable_const_cse")]
    pub(crate) disable_const_cse: bool,
    #[arg(long, conflicts_with = "disable_blocked_reduction")]
    pub(crate) enable_blocked_reduction: bool,
    #[arg(long, conflicts_with = "enable_blocked_reduction")]
    pub(crate) disable_blocked_reduction: bool,
    #[arg(long, conflicts_with = "disable_memoize")]
    pub(crate) enable_memoize: bool,
    #[arg(long, conflicts_with = "enable_memoize")]
    pub(crate) disable_memoize: bool,
    #[arg(long, value_enum, default_value_t = SchedModel::CortexA53)]
    pub(crate) sched_model: SchedModel,
}

impl Arg {
    pub(crate) fn ir_optimization_config(&self) -> raana_ir::opt::config::PassesConfig {
        use raana_ir::opt::config::{
            LoopUnrollMode as IrLoopUnrollMode, OptimizationLevel, PassesConfig, TargetPolicy,
        };

        let opt_level = OptimizationLevel::try_from(self.opt_level)
            .expect("clap restricts optimization levels to 0 through 2");
        let target = match self.target {
            Target::Aarch64 => TargetPolicy::aarch64(),
            Target::Riscv64 => TargetPolicy::riscv64(),
        };
        let mut config = PassesConfig::new(opt_level, target);
        if let Some(mode) = self.loop_unroll {
            config.loop_unroll = match mode {
                LoopUnrollMode::On => IrLoopUnrollMode::Enabled,
                LoopUnrollMode::Off => IrLoopUnrollMode::Disabled,
                LoopUnrollMode::DryRun => IrLoopUnrollMode::DryRun,
            };
        }
        config.collect_stats = self.pass_stats;
        if self.enable_blocked_reduction {
            config.blocked_reduction = true;
        }
        if self.disable_blocked_reduction {
            config.blocked_reduction = false;
        }
        if self.enable_memoize {
            config.memoize = true;
        }
        if self.disable_memoize {
            config.memoize = false;
        }
        config
    }

    pub(crate) fn aarch64_codegen_config(&self) -> anon_armv8::AArch64CodegenConfig {
        use anon_armv8::AArch64CodegenConfig;
        let mut config = match self.opt_level {
            0 => AArch64CodegenConfig {
                dce: false,
                peephole_combine: false,
                pair_combine: false,
                list_scheduler: false,
                sched_model: self.sched_model.into(),
                branch_opt: false,
                chain_fusion: false,
                const_cse: false,
            },
            1 => AArch64CodegenConfig {
                dce: true,
                peephole_combine: true,
                pair_combine: true,
                list_scheduler: false,
                sched_model: self.sched_model.into(),
                branch_opt: true,
                chain_fusion: true,
                const_cse: true,
            },
            _ => AArch64CodegenConfig {
                dce: true,
                peephole_combine: true,
                pair_combine: true,
                list_scheduler: true,
                sched_model: self.sched_model.into(),
                branch_opt: true,
                chain_fusion: true,
                const_cse: true,
            },
        };
        if self.enable_mir_dce {
            config.dce = true;
        }
        if self.disable_mir_dce {
            config.dce = false;
        }
        if self.enable_mir_peephole {
            config.peephole_combine = true;
        }
        if self.disable_mir_peephole {
            config.peephole_combine = false;
        }
        if self.enable_pair_combine {
            config.pair_combine = true;
        }
        if self.disable_pair_combine {
            config.pair_combine = false;
        }
        if self.enable_sched {
            config.list_scheduler = true;
        }
        if self.disable_sched {
            config.list_scheduler = false;
        }
        if self.enable_const_cse {
            config.const_cse = true;
        }
        if self.disable_const_cse {
            config.const_cse = false;
        }
        config
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.opt_level == 0
            && matches!(
                self.loop_unroll,
                Some(LoopUnrollMode::On | LoopUnrollMode::DryRun)
            )
        {
            return Err("--loop-unroll on/dry-run requires -O1 or -O2".to_string());
        }
        if self.target == Target::Riscv64 {
            let aarch64_only = self.enable_mir_dce
                || self.disable_mir_dce
                || self.enable_mir_peephole
                || self.disable_mir_peephole
                || self.enable_pair_combine
                || self.disable_pair_combine
                || self.enable_sched
                || self.disable_sched
                || self.sched_model != SchedModel::CortexA53;
            if aarch64_only {
                return Err(
                    "MIR pass and scheduler-model flags are only supported with --target aarch64"
                        .to_string(),
                );
            }
        }
        Ok(())
    }
}

impl From<SchedModel> for anon_armv8::AArch64SchedModel {
    fn from(model: SchedModel) -> Self {
        match model {
            SchedModel::CortexA53 => Self::CortexA53,
        }
    }
}

#[derive(Debug, clap::ValueEnum, Clone, Copy, PartialEq, Eq)]
#[clap(rename_all = "lower")]
pub enum EmitOption {
    Ir,
    Llvm,
    Asm,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn parse(args: &[&str]) -> Arg {
        Arg::try_parse_from(args).expect("valid arguments")
    }

    fn base() -> Vec<&'static str> {
        vec!["soyo", "-o", "out.s", "-S", "test.sy"]
    }

    #[test]
    fn o0_disables_all_mir_passes() {
        let mut argv = base();
        argv.extend(["-O", "0"]);
        let config = parse(&argv).aarch64_codegen_config();
        assert!(!config.dce);
        assert!(!config.peephole_combine);
        assert!(!config.pair_combine);
        assert!(!config.list_scheduler);
    }

    #[test]
    fn o1_enables_peephole_and_pair_only() {
        let mut argv = base();
        argv.extend(["-O", "1"]);
        let config = parse(&argv).aarch64_codegen_config();
        assert!(config.dce);
        assert!(config.peephole_combine);
        assert!(config.pair_combine);
        assert!(!config.list_scheduler);
    }

    #[test]
    fn o2_enables_all_mir_passes() {
        let mut argv = base();
        argv.extend(["-O", "2"]);
        let config = parse(&argv).aarch64_codegen_config();
        assert!(config.dce);
        assert!(config.peephole_combine);
        assert!(config.pair_combine);
        assert!(config.list_scheduler);
    }

    #[test]
    fn explicit_flags_override_opt_level() {
        let mut argv = base();
        argv.extend(["-O", "0", "--enable-sched"]);
        let config = parse(&argv).aarch64_codegen_config();
        assert!(!config.dce);
        assert!(!config.peephole_combine);
        assert!(!config.pair_combine);
        assert!(config.list_scheduler);

        let mut argv = base();
        argv.extend(["-O", "2", "--disable-sched"]);
        let config = parse(&argv).aarch64_codegen_config();
        assert!(config.dce);
        assert!(config.peephole_combine);
        assert!(config.pair_combine);
        assert!(!config.list_scheduler);

        let mut argv = base();
        argv.extend([
            "-O",
            "2",
            "--disable-mir-peephole",
            "--disable-pair-combine",
        ]);
        let config = parse(&argv).aarch64_codegen_config();
        assert!(!config.peephole_combine);
        assert!(!config.pair_combine);
        assert!(config.list_scheduler);

        let mut argv = base();
        argv.extend(["-O", "0", "--enable-mir-dce"]);
        let config = parse(&argv).aarch64_codegen_config();
        assert!(config.dce);
        assert!(!config.peephole_combine);
        assert!(!config.pair_combine);
        assert!(!config.list_scheduler);

        let mut argv = base();
        argv.extend(["-O", "2", "--disable-mir-dce"]);
        let config = parse(&argv).aarch64_codegen_config();
        assert!(!config.dce);
        assert!(config.peephole_combine);
        assert!(config.pair_combine);
        assert!(config.list_scheduler);
    }

    #[test]
    fn conflicting_flags_are_rejected() {
        let mut argv = base();
        argv.extend(["--enable-sched", "--disable-sched"]);
        assert!(Arg::try_parse_from(&argv).is_err());

        let mut argv = base();
        argv.extend(["--enable-mir-dce", "--disable-mir-dce"]);
        assert!(Arg::try_parse_from(&argv).is_err());
    }

    #[test]
    fn opt_level_above_two_is_rejected() {
        let mut argv = base();
        argv.extend(["-O", "3"]);
        assert!(Arg::try_parse_from(&argv).is_err());
    }

    #[test]
    fn riscv_rejects_aarch64_only_flags() {
        let mut argv = base();
        argv.extend(["--target", "riscv64", "--enable-sched"]);
        assert!(parse(&argv).validate().is_err());

        let mut argv = base();
        argv.extend(["--target", "riscv64"]);
        assert!(parse(&argv).validate().is_ok());

        let mut argv = base();
        argv.extend(["--target", "aarch64", "--enable-sched"]);
        assert!(parse(&argv).validate().is_ok());
    }

    #[test]
    fn ir_config_tracks_opt_target_and_loop_unroll_override() {
        use raana_ir::opt::config::{LoopUnrollMode, OptimizationLevel, TargetIsa};

        let mut argv = base();
        argv.extend([
            "-O",
            "2",
            "--target",
            "aarch64",
            "--loop-unroll",
            "dry-run",
            "--pass-stats",
        ]);
        let config = parse(&argv).ir_optimization_config();
        assert_eq!(config.opt_level, OptimizationLevel::O2);
        assert_eq!(config.target.isa, TargetIsa::Aarch64);
        assert_eq!(config.loop_unroll, LoopUnrollMode::DryRun);
        assert!(config.collect_stats);

        let mut argv = base();
        argv.extend(["-O", "0", "--target", "riscv64"]);
        let config = parse(&argv).ir_optimization_config();
        assert_eq!(config.opt_level, OptimizationLevel::O0);
        assert_eq!(config.target.isa, TargetIsa::Riscv64);
        assert_eq!(config.loop_unroll, LoopUnrollMode::Disabled);
    }

    #[test]
    fn o0_rejects_active_loop_unroll_modes() {
        let mut argv = base();
        argv.extend(["-O", "0", "--loop-unroll", "on"]);
        assert!(parse(&argv).validate().is_err());

        let mut argv = base();
        argv.extend(["-O", "0", "--loop-unroll", "dry-run"]);
        assert!(parse(&argv).validate().is_err());

        let mut argv = base();
        argv.extend(["-O", "0", "--loop-unroll", "off"]);
        assert!(parse(&argv).validate().is_ok());
    }
}
