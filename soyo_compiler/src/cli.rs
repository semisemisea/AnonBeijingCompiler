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
    #[arg(long, value_enum, default_value_t = SchedModel::CortexA53)]
    pub(crate) sched_model: SchedModel,
}

impl Arg {
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
            },
            1 => AArch64CodegenConfig {
                dce: true,
                peephole_combine: true,
                pair_combine: true,
                list_scheduler: false,
                sched_model: self.sched_model.into(),
                branch_opt: true,
            },
            _ => AArch64CodegenConfig {
                dce: true,
                peephole_combine: true,
                pair_combine: true,
                list_scheduler: true,
                sched_model: self.sched_model.into(),
                branch_opt: true,
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
        config
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
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
}
