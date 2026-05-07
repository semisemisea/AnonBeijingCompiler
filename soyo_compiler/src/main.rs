use clap::Parser;
use raana_ir::fmt::writer::Writer;
use std::path::Path;

use crate::backend::armv8::codegen::asm_gen_context::AsmGenContext;
use crate::frontend::utils::AstGenContext;
use frontend::utils::ToRaanaIR;

mod backend;
mod cli;
mod context;
mod frontend;

lalrpop_util::lalrpop_mod!(sysy);

/// compiler --emit asm -o testcase.s testcase.sy [-O1]
/// extra support:
///     -S is a compatibility alias for `--emit asm`
///     --emit ir,asm writes both outputs under the folder passed to `-o`
fn main() {
    env_logger::init();

    let args = cli::Arg::parse();

    let source_code = std::fs::read_to_string(&args.input_path).unwrap();

    let ast = sysy::CompUnitsParser::new().parse(&source_code).unwrap();
    let mut ctx = AstGenContext::new();
    ast.convert(&mut ctx);

    let mut program = ctx.program;

    if args.opt_level > 0 {
        let pass_manager = raana_ir::opt::pass::PassesManager::default_ref();
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
    let needs_asm = emit.contains(&cli::EmitOption::Asm);
    let ir = if needs_ir {
        Some(dump_ir(&program))
    } else {
        None
    };
    let asm = if needs_asm {
        Some(dump_asm(&program))
    } else {
        None
    };

    if emit.len() == 1 {
        match emit[0] {
            cli::EmitOption::Ir => write_file(&args.output_path, ir.unwrap()),
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
        if let Some(asm) = asm {
            write_file(&args.output_path.join(format!("{stem}.s")), asm);
        }
    }
}

fn dump_ir(program: &raana_ir::ir::Program) -> String {
    let mut writer = Writer::new(program);
    writer.write().unwrap();
    writer.finish()
}

fn dump_asm(program: &raana_ir::ir::Program) -> String {
    let codegen_ctx = AsmGenContext::new();
    let insts = codegen_ctx.generate(program);
    insts
        .iter()
        .map(|inst| inst.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn write_file(path: &Path, buf: impl AsRef<[u8]>) {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).unwrap();
        }
    }
    std::fs::write(path, buf).unwrap();
}

#[cfg(test)]
mod test {
    use log::{info, trace};
    use raana_ir::fmt::writer::Writer;
    use std::path::{Path, PathBuf};

    use crate::{
        frontend::utils::{AstGenContext, ToRaanaIR},
        sysy,
    };

    fn logger_init() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    fn sy_files(path: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                sy_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "sy") {
                out.push(path);
            }
        }
    }

    fn print_progress(message: &str) {
        #[cfg(unix)]
        {
            if let Ok(mut stderr) = std::fs::OpenOptions::new().write(true).open("/dev/stderr") {
                use std::io::Write;

                let _ = writeln!(stderr, "{message}");
                let _ = stderr.flush();
                return;
            }
        }

        eprintln!("{message}");
    }

    fn test(input_path: &Path) {
        let source_code = std::fs::read_to_string(input_path).unwrap();

        let ast = sysy::CompUnitsParser::new().parse(&source_code).unwrap();
        let mut ctx = AstGenContext::new();
        ast.convert(&mut ctx);

        let mut program = ctx.program;

        let pass_manager = raana_ir::opt::pass::PassesManager::default_ref();
        pass_manager.run_passes(&mut program);

        let mut writer = Writer::new(&program);
        writer.write().unwrap();
        let buf = writer.finish();
        trace!("{}", buf);
    }

    #[test]
    fn functional() {
        logger_init();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests");
        let mut files = Vec::new();
        sy_files(&root, &mut files);
        files.sort();
        let total = files.len();
        for (index, file) in files.iter().enumerate() {
            test(file);
            let name = file.strip_prefix(&root).unwrap_or(file);
            print_progress(&format!(
                "[{}/{} passed] {}",
                index + 1,
                total,
                name.display()
            ));
        }
    }
}
