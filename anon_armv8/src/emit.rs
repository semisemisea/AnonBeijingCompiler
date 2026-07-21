use std::fmt::Write;

use taki_mir::{
    block_order::MirBlockIndex,
    reg_alloc::reg::{Allocation, Edit},
    register::Reg,
    types::Type,
};

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

/// Emits one register-allocation move using the finalized spill-slot base.
/// Spill slots are eight bytes apart, while the load/store width follows `ty`.
pub fn emit_post_ra_move(output: &mut String, edit: &Edit, spill_base: u32) -> Result<(), String> {
    let Edit::Move { from, to, ty } = edit;
    if from.is_none() || to.is_none() {
        return Err("register allocation produced a move with no location".into());
    }

    match (from.is_reg(), to.is_reg()) {
        (true, true) => {
            let src = allocation_reg(*from)?;
            let dst = allocation_reg(*to)?;
            let opcode = if ty.is_f32() { "fmov" } else { "mov" };
            writeln!(
                output,
                "    {opcode} {}, {}",
                regs::format_reg(dst, *ty),
                regs::format_reg(src, *ty)
            )
            .unwrap();
        }
        (true, false) => {
            let src = allocation_reg(*from)?;
            emit_spill_access(output, "str", src, *ty, spill_offset(*to, spill_base)?)?;
        }
        (false, true) => {
            let dst = allocation_reg(*to)?;
            emit_spill_access(output, "ldr", dst, *ty, spill_offset(*from, spill_base)?)?;
        }
        (false, false) => {
            let scratch = if ty.is_f32() {
                regs::float_reg(regs::FP_SCRATCH)
            } else {
                regs::int_reg(regs::INT_SCRATCH0)
            };
            emit_spill_access(
                output,
                "ldr",
                scratch,
                *ty,
                spill_offset(*from, spill_base)?,
            )?;
            emit_spill_access(output, "str", scratch, *ty, spill_offset(*to, spill_base)?)?;
        }
    }
    Ok(())
}

fn allocation_reg(allocation: Allocation) -> Result<Reg, String> {
    allocation
        .as_reg()
        .map(Reg::from_physical_reg)
        .ok_or_else(|| "expected register allocation".into())
}

fn spill_offset(allocation: Allocation, spill_base: u32) -> Result<u32, String> {
    let slot = allocation
        .as_stack()
        .ok_or_else(|| "expected spill-slot allocation".to_string())?;
    spill_base
        .checked_add(u32::from(slot))
        .ok_or_else(|| "spill-slot offset exceeds AArch64 frame range".into())
}

fn emit_spill_access(
    output: &mut String,
    opcode: &str,
    reg: Reg,
    ty: Type,
    offset: u32,
) -> Result<(), String> {
    let scale = if ty.is_i64() { 8 } else { 4 };
    if offset % scale == 0 && offset / scale <= 4095 {
        writeln!(
            output,
            "    {opcode} {}, [sp, #{offset}]",
            regs::format_reg(reg, ty)
        )
        .unwrap();
        return Ok(());
    }

    writeln!(output, "    movz x17, #{}", offset & 0xffff).unwrap();
    if offset >> 16 != 0 {
        writeln!(output, "    movk x17, #{}, lsl #16", offset >> 16).unwrap();
    }
    writeln!(output, "    add x17, sp, x17").unwrap();
    writeln!(output, "    {opcode} {}, [x17]", regs::format_reg(reg, ty)).unwrap();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regs;
    use std::{
        io::Write as _,
        process::{Command, Stdio},
    };
    use taki_mir::reg_alloc::reg::{Allocation, Edit, SpillSlot};

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

    #[test]
    fn emits_typed_register_and_spill_moves() {
        let mut output = String::new();
        emit_post_ra_move(
            &mut output,
            &Edit::Move {
                from: Allocation::reg(regs::int_preg(1)),
                to: Allocation::stack(SpillSlot::new(8)),
                ty: Type::new_i32(),
            },
            16,
        )
        .unwrap();
        emit_post_ra_move(
            &mut output,
            &Edit::Move {
                from: Allocation::stack(SpillSlot::new(8)),
                to: Allocation::reg(regs::float_preg(2)),
                ty: Type::new_f32(),
            },
            16,
        )
        .unwrap();
        emit_post_ra_move(
            &mut output,
            &Edit::Move {
                from: Allocation::reg(regs::float_preg(1)),
                to: Allocation::reg(regs::float_preg(2)),
                ty: Type::new_f32(),
            },
            0,
        )
        .unwrap();

        assert_eq!(
            output,
            "    str w1, [sp, #24]\n    ldr s2, [sp, #24]\n    fmov s2, s1\n"
        );
    }

    #[test]
    fn uses_reserved_scratch_register_for_spill_to_spill_move() {
        let mut output = String::new();
        emit_post_ra_move(
            &mut output,
            &Edit::Move {
                from: Allocation::stack(SpillSlot::new(0)),
                to: Allocation::stack(SpillSlot::new(40_000)),
                ty: Type::new_i64(),
            },
            0,
        )
        .unwrap();

        assert_eq!(
            output,
            "    ldr x16, [sp, #0]\n    movz x17, #40000\n    add x17, sp, x17\n    str x16, [x17]\n"
        );
    }

    #[test]
    fn clang_accepts_post_ra_move_sequences() {
        let mut moves = String::new();
        emit_post_ra_move(
            &mut moves,
            &Edit::Move {
                from: Allocation::stack(SpillSlot::new(0)),
                to: Allocation::stack(SpillSlot::new(40_000)),
                ty: Type::new_i64(),
            },
            0,
        )
        .unwrap();
        emit_post_ra_move(
            &mut moves,
            &Edit::Move {
                from: Allocation::reg(regs::float_preg(1)),
                to: Allocation::stack(SpillSlot::new(8)),
                ty: Type::new_f32(),
            },
            16,
        )
        .unwrap();
        let assembly = format!("    .text\nmain:\n{moves}    ret\n");
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
            .write_all(assembly.as_bytes())
            .unwrap();
        let output = clang.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "clang rejected post-RA moves: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
