use anon_armv8::AArch64Backend;
use clap::Parser;
use raana_ir::fmt::writer::Writer;
use std::path::Path;
use uika_riscv::lower::Riscv64Backend;

use crate::frontend::utils::AstGenContext;
use frontend::utils::ToRaanaIR;

mod cli;
mod frontend;

#[cfg(test)]
mod abi_matrix;

lalrpop_util::lalrpop_mod!(sysy);

/// compiler --emit asm -o testcase.s testcase.sy [-O1]
/// extra support:
///     -S is a compatibility alias for `--emit asm`
///     --emit ir,asm writes both outputs under the folder passed to `-o`
fn main() {
    let args = cli::Arg::parse();
    let mut logger = env_logger::Builder::new();
    logger.target(env_logger::Target::Stderr);
    logger.filter_level(log::LevelFilter::Warn);
    let env_filter = std::env::var("RUST_LOG").ok();
    let filter = args
        .log
        .as_deref()
        .or(env_filter.as_deref())
        .unwrap_or("warn");
    logger.parse_filters(filter);
    logger.init();

    if let Err(error) = run(args) {
        eprintln!("compiler: {error}");
        std::process::exit(1);
    }
}

fn run(args: cli::Arg) -> Result<(), String> {
    args.validate()?;
    let aarch64_config = args.aarch64_codegen_config();
    let source_code = std::fs::read_to_string(&args.input_path).unwrap();

    let ast = sysy::CompUnitsParser::new().parse(&source_code).unwrap();
    let mut ctx = AstGenContext::new();
    ast.convert(&mut ctx);

    let mut program = ctx.program;

    if args.opt_level > 0 {
        let mut pass_manager = match args.target {
            cli::Target::Aarch64 => raana_ir::opt::pass::PassesManager::aarch64(),
            cli::Target::Riscv64 => raana_ir::opt::pass::PassesManager::default(),
        };
        pass_manager.run_passes(&mut program);
    }

    let emit = if args.emit.is_empty() {
        vec![if args.assembly_only {
            cli::EmitOption::Asm
        } else {
            cli::EmitOption::Ir
        }]
    } else {
        args.emit
    };
    let needs_ir = emit.contains(&cli::EmitOption::Ir);
    let needs_llvm = emit.contains(&cli::EmitOption::Llvm);
    let needs_asm = emit.contains(&cli::EmitOption::Asm);
    let ir = if needs_ir {
        Some(dump_ir(&program))
    } else {
        None
    };
    let llvm = if needs_llvm {
        Some(dump_llvm(&program))
    } else {
        None
    };
    let asm = if needs_asm {
        Some(dump_asm(&program, args.target, &aarch64_config))
    } else {
        None
    };

    if emit.len() == 1 {
        match emit[0] {
            cli::EmitOption::Ir => write_file(&args.output_path, ir.unwrap()),
            cli::EmitOption::Llvm => write_file(&args.output_path, llvm.unwrap()),
            cli::EmitOption::Asm => write_file(&args.output_path, asm.unwrap()),
        }
    } else {
        let stem = args
            .input_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("out");
        if let Some(ir) = ir {
            write_file(&args.output_path.join(format!("{stem}.ir")), ir);
        }
        if let Some(llvm) = llvm {
            write_file(&args.output_path.join(format!("{stem}.ll")), llvm);
        }
        if let Some(asm) = asm {
            write_file(&args.output_path.join(format!("{stem}.s")), asm);
        }
    }
    Ok(())
}

fn dump_ir(program: &raana_ir::ir::Program) -> String {
    let mut writer = Writer::new(program);
    writer.write().unwrap();
    writer.finish()
}

fn dump_llvm(program: &raana_ir::ir::Program) -> String {
    raana_ir::llvm::write_llvm_ir(program)
}

fn dump_asm(
    program: &raana_ir::ir::Program,
    target: cli::Target,
    aarch64_config: &anon_armv8::AArch64CodegenConfig,
) -> String {
    match target {
        cli::Target::Riscv64 => taki_mir::compile::<Riscv64Backend>(program),
        cli::Target::Aarch64 => {
            taki_mir::compile_with_config::<AArch64Backend>(program, aarch64_config).assembly
        }
    }
}

fn write_file(path: &Path, buf: impl AsRef<[u8]>) {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).unwrap();
        }
    }
    std::fs::write(path, buf).unwrap();
}
