use raana_ir::{fmt::writer::Writer, ir::Program};

use crate::frontend::utils::AstGenContext;
use frontend::utils::ToRaanaIR;

// mod backend;
mod context;
mod frontend;

lalrpop_util::lalrpop_mod!(sysy);

/// compiler -S -o testcase.s testcase.sy [-O1]
fn main() {
    let mut args = std::env::args();
    // compiler
    args.next();
    // -S
    args.next();
    // -o
    args.next();
    let output_path = args.next().unwrap();
    let input_path = args.next().unwrap();
    let opt_flag = args.next().is_some();

    let source_code = std::fs::read_to_string(input_path).unwrap();

    let ast = sysy::CompUnitsParser::new().parse(&source_code).unwrap();
    let mut ctx = AstGenContext::new();
    ast.convert(&mut ctx);

    let mut program = ctx.program;

    let mut pass_manager = raana_ir::opt::pass::PassesManager::new();
    pass_manager.run_passes(&mut program);

    let mut writer = Writer::new(&program);
    writer.write().unwrap();
    let buf = writer.finish();
    println!("{}", buf);
}

#[cfg(test)]
mod test {
    use raana_ir::fmt::writer::Writer;

    use crate::{
        frontend::utils::{AstGenContext, ToRaanaIR},
        sysy,
    };

    fn test(input_path: String) {
        eprintln!("{input_path}");
        let source_code = std::fs::read_to_string(input_path).unwrap();

        let ast = sysy::CompUnitsParser::new().parse(&source_code).unwrap();
        let mut ctx = AstGenContext::new();
        ast.convert(&mut ctx);

        let mut program = ctx.program;

        let mut pass_manager = raana_ir::opt::pass::PassesManager::new();
        pass_manager.run_passes(&mut program);

        let mut writer = Writer::new(&program);
        writer.write().unwrap();
        let buf = writer.finish();
        println!("{}", buf);
    }

    #[test]
    fn functional() {
        let path = "/Users/azureskye/Documents/Programs/rust/AnonBeijingCompiler/tests/functional";
        let r = std::fs::read_dir(path).unwrap();
        for f in r
            .filter(|f| {
                f.as_ref()
                    .unwrap()
                    .file_name()
                    .to_str()
                    .unwrap()
                    .strip_suffix(".sy")
                    .is_some()
            })
            .map(|f| f.unwrap())
        {
            test(format!("{}/{}", path, f.file_name().to_str().unwrap()));
        }
    }
}
