use std::path::PathBuf;

#[derive(Debug, clap::Parser)]
pub(crate) struct Arg {
    #[arg(short = 'S', default_value_t = true)]
    pub(crate) assembly_only: bool,
    #[arg(value_name = "INPUT")]
    pub(crate) input_path: PathBuf,
    #[arg(short = 'o', value_name = "OUTPUT")]
    pub(crate) output_path: PathBuf,
    #[arg(short = 'O', default_value_t = 0)]
    pub(crate) opt_level: u8,
    #[arg(long, value_enum, default_value_t = EmitOption::Asm)]
    pub(crate) emit: EmitOption,
}

#[derive(Debug, clap::ValueEnum, Clone, Copy)]
#[clap(rename_all = "lower")]
pub enum EmitOption {
    Ir,
    Asm,
}
