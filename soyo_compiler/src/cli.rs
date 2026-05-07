use std::path::PathBuf;

#[derive(Debug, clap::Parser)]
pub(crate) struct Arg {
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
}

#[derive(Debug, clap::ValueEnum, Clone, Copy, PartialEq, Eq)]
#[clap(rename_all = "lower")]
pub enum EmitOption {
    Ir,
    Asm,
}
