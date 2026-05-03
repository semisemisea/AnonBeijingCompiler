use clap::Parser;
use raana_ir::fmt::writer::Writer;

use crate::frontend::utils::AstGenContext;
use frontend::utils::ToRaanaIR;

// mod backend;
mod cli;
mod context;
mod frontend;

lalrpop_util::lalrpop_mod!(sysy);

/// compiler -S -o testcase.s testcase.sy [-O1]
/// extra support:
///     --emit,  `ir` or `asm`
fn main() {
    let args = cli::Arg::parse();
    assert!(args.assembly_only);

    let source_code = std::fs::read_to_string(&args.input_path).unwrap();

    let ast = sysy::CompUnitsParser::new().parse(&source_code).unwrap();
    let mut ctx = AstGenContext::new();
    ast.convert(&mut ctx);

    let mut program = ctx.program;

    if args.opt_level > 0 {
        let pass_manager = raana_ir::opt::pass::PassesManager::default_ref();
        pass_manager.run_passes(&mut program);
    }

    let mut writer = Writer::new(&program);
    writer.write().unwrap();
    let buf = writer.finish();
    std::fs::write(&args.output_path, buf).unwrap();
}

#[cfg(test)]
mod test {
    use raana_ir::fmt::writer::Writer;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use crate::{
        frontend::utils::{AstGenContext, ToRaanaIR},
        sysy,
    };

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
        println!("{}", buf);
    }

    #[test]
    fn functional() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests");
        let mut files = Vec::new();
        sy_files(&root, &mut files);
        files.sort();
        let total = files.len();
        for (index, file) in files.iter().enumerate() {
            test(&file);
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
