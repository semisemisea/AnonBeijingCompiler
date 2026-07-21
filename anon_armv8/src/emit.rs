use std::fmt::Write;

use taki_mir::{block_order::MirBlockIndex, types::Type};

use crate::{inst::Inst, regs};

pub struct AsmBlock {
    pub index: MirBlockIndex,
    pub insts: Vec<Inst>,
}

pub struct AsmFunction {
    pub name: String,
    pub blocks: Vec<AsmBlock>,
}

pub struct AsmProgram {
    pub functions: Vec<AsmFunction>,
}

impl AsmProgram {
    pub fn emit(&self) -> String {
        let mut output = String::new();
        writeln!(output, "    .text").unwrap();
        for function in &self.functions {
            writeln!(output, "    .p2align 2").unwrap();
            writeln!(output, "    .globl {}", function.name).unwrap();
            writeln!(output, "    .type {}, %function", function.name).unwrap();
            writeln!(output, "{}:", function.name).unwrap();
            for block in &function.blocks {
                writeln!(output, "{}:", label(&function.name, block.index)).unwrap();
                for inst in &block.insts {
                    writeln!(output, "    {}", format_inst(&function.name, inst)).unwrap();
                }
            }
            writeln!(output, "    .size {}, .-{}", function.name, function.name).unwrap();
        }
        output
    }
}

fn label(function: &str, block: MirBlockIndex) -> String {
    format!(".L{}_bb{}", function, block.raw_u32())
}

fn format_inst(function: &str, inst: &Inst) -> String {
    match inst {
        Inst::Mov { dst, src, ty } => format!(
            "mov {}, {}",
            regs::format_reg(*dst, *ty),
            regs::format_reg(*src, *ty)
        ),
        Inst::Add { dst, lhs, rhs, ty } => format!(
            "add {}, {}, {}",
            regs::format_reg(*dst, *ty),
            regs::format_reg(*lhs, *ty),
            regs::format_reg(*rhs, *ty)
        ),
        Inst::Cmp { lhs, rhs, ty } => format!(
            "cmp {}, {}",
            regs::format_reg(*lhs, *ty),
            regs::format_reg(*rhs, *ty)
        ),
        Inst::CSet { dst, cond } => format!(
            "cset {}, {}",
            regs::format_reg(*dst, Type::new_i32()),
            cond.asm()
        ),
        Inst::Jump { target } => format!("b {}", label(function, *target)),
        Inst::Branch { cond, target } => format!("b.{} {}", cond.asm(), label(function, *target)),
        Inst::Call { symbol } => format!("bl {symbol}"),
        Inst::Ret => "ret".to_string(),
        Inst::Nop => "nop".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regs;
    use std::{
        io::Write as _,
        process::{Command, Stdio},
    };

    #[test]
    fn emits_assembleable_function_skeleton() {
        let program = AsmProgram {
            functions: vec![AsmFunction {
                name: "main".into(),
                blocks: vec![AsmBlock {
                    index: MirBlockIndex::new(0),
                    insts: vec![
                        Inst::Mov {
                            dst: regs::int_reg(0),
                            src: regs::int_reg(0),
                            ty: Type::new_i32(),
                        },
                        Inst::Ret,
                    ],
                }],
            }],
        };
        assert_eq!(
            program.emit(),
            "    .text\n    .p2align 2\n    .globl main\n    .type main, %function\nmain:\n.Lmain_bb0:\n    mov w0, w0\n    ret\n    .size main, .-main\n"
        );
    }

    #[test]
    fn clang_accepts_generated_aarch64_assembly() {
        let program = AsmProgram {
            functions: vec![AsmFunction {
                name: "main".into(),
                blocks: vec![AsmBlock {
                    index: MirBlockIndex::new(0),
                    insts: vec![Inst::Ret],
                }],
            }],
        };
        let mut clang = Command::new("clang")
            .args([
                "--target=aarch64-linux-gnu",
                "-x",
                "assembler",
                "-c",
                "-",
                "-o",
                "/dev/null",
            ])
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("clang must be available for AArch64 assembly validation");
        clang
            .stdin
            .take()
            .unwrap()
            .write_all(program.emit().as_bytes())
            .unwrap();
        let output = clang.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "clang rejected generated assembly: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
