use std::path::PathBuf;

#[derive(Debug, clap::ValueEnum, Clone, Copy, PartialEq, Eq)]
#[clap(rename_all = "lowercase")]
pub enum Target {
    Riscv64,
    #[clap(help = "emit GNU AArch64 assembly through the generic VCode pipeline")]
    Aarch64,
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
    #[arg(short = 'O', default_value_t = 0)]
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
}

#[derive(Debug, clap::ValueEnum, Clone, Copy, PartialEq, Eq)]
#[clap(rename_all = "lower")]
pub enum EmitOption {
    Ir,
    Llvm,
    Asm,
}
