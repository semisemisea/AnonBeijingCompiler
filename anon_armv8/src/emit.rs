use std::{fmt::Write, ops::Range};

use taki_mir::{
    block_order::MirBlockIndex,
    reg_alloc::reg::{Allocation, Edit, Output as RegAllocOutput},
    register::Reg,
    types::Type,
};

use crate::{abi::FrameLayout, inst::Inst, regs};

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

/// A finalized VCode block paired with the global instruction range used by
/// register-allocation outputs.
pub struct PostRaBlock<'a> {
    pub index: MirBlockIndex,
    pub inst_range: Range<usize>,
    pub insts: &'a [Inst],
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

/// Emits one finalized function from VCode blocks and register-allocation
/// output. Returns branch to a single epilogue so callee saves are restored.
pub fn emit_post_ra_function(
    output: &mut String,
    name: &str,
    blocks: &[PostRaBlock<'_>],
    allocations: &RegAllocOutput,
    layout: &FrameLayout,
) -> Result<(), String> {
    writeln!(output, "    .p2align 2").unwrap();
    writeln!(output, "    .globl {name}").unwrap();
    writeln!(output, "    .type {name}, %function").unwrap();
    writeln!(output, "{name}:").unwrap();
    emit_post_ra_prologue(output, layout)?;

    for block in blocks {
        if block.inst_range.len() != block.insts.len() {
            return Err(format!(
                "block {} has {} instructions but range {:?}",
                block.index.raw_u32(),
                block.insts.len(),
                block.inst_range
            ));
        }
        writeln!(output, "{}:", label(name, block.index)).unwrap();
        for (offset, inst) in block.insts.iter().enumerate() {
            let index = (block.inst_range.start + offset) as u32;
            emit_edits_at(
                output,
                allocations,
                index,
                taki_mir::reg_alloc::reg::InstPosition::Before,
                layout.outgoing_args + layout.locals,
            )?;
            if matches!(inst, Inst::Ret | Inst::RetI32 { .. }) {
                writeln!(output, "    b .L{name}_epilogue").unwrap();
            } else {
                emit_post_ra_inst(
                    output,
                    name,
                    inst,
                    allocations.inst_allocs(index),
                    layout.outgoing_args + layout.locals,
                )?;
            }
            emit_edits_at(
                output,
                allocations,
                index,
                taki_mir::reg_alloc::reg::InstPosition::After,
                layout.outgoing_args + layout.locals,
            )?;
        }
    }
    writeln!(output, ".L{name}_epilogue:").unwrap();
    emit_post_ra_epilogue(output, layout)?;
    writeln!(output, "    .size {name}, .-{name}").unwrap();
    Ok(())
}

fn label(function: &str, block: MirBlockIndex) -> String {
    format!(".L{}_bb{}", function, block.raw_u32())
}

fn format_inst(function: &str, inst: &Inst) -> String {
    match inst {
        Inst::MovImm { dst, value } => {
            let value = *value as u32;
            let dst = regs::format_reg(*dst, Type::new_i32());
            let low = value & 0xffff;
            let high = value >> 16;
            if high == 0 {
                format!("movz {dst}, #{low}")
            } else {
                format!("movz {dst}, #{low}\n    movk {dst}, #{high}, lsl #16")
            }
        }
        Inst::Mov { dst, src, ty } => format!(
            "{} {}, {}",
            if ty.is_f32() { "fmov" } else { "mov" },
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
        Inst::RetI32 { src } => format!("mov w0, {}", regs::format_reg(*src, Type::new_i32())),
        Inst::Ret => "ret".to_string(),
        Inst::Nop => "nop".to_string(),
    }
}

/// Emits one register-allocation move using the finalized spill-slot base.
/// Spill slots are eight bytes apart, while the load/store width follows `ty`.
pub fn emit_post_ra_move(output: &mut String, edit: &Edit, spill_base: u32) -> Result<(), String> {
    let Edit::Move { from, to, ty } = edit;
    if from.is_none() || to.is_none() {
        return Err(format!(
            "register allocation produced a move with no location: {edit:?}"
        ));
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

/// Rewrites one VCode instruction using its register-allocation results and
/// emits any reloads or spill stores required around it.
pub fn emit_post_ra_inst(
    output: &mut String,
    function: &str,
    inst: &Inst,
    allocs: &[Allocation],
    spill_base: u32,
) -> Result<(), String> {
    let mut rewritten = inst.clone();
    let spill_def = match &mut rewritten {
        Inst::MovImm { dst, .. } => {
            expect_alloc_count(inst, allocs, 1)?;
            let (reg, spill) = resolve_def(allocs[0], Type::new_i32(), 0)?;
            *dst = reg;
            spill
        }
        Inst::Mov { dst, src, ty } => {
            expect_alloc_count(inst, allocs, 2)?;
            *src = resolve_use(output, allocs[0], *ty, 0, spill_base)?;
            let (reg, spill) = resolve_def(allocs[1], *ty, 0)?;
            *dst = reg;
            spill
        }
        Inst::Add { dst, lhs, rhs, ty } => {
            expect_alloc_count(inst, allocs, 3)?;
            *lhs = resolve_use(output, allocs[0], *ty, 0, spill_base)?;
            *rhs = resolve_use(output, allocs[1], *ty, 1, spill_base)?;
            let (reg, spill) = resolve_def(allocs[2], *ty, 0)?;
            *dst = reg;
            spill
        }
        Inst::Cmp { lhs, rhs, ty } => {
            expect_alloc_count(inst, allocs, 2)?;
            if ty.is_f32() {
                return Err("f32 comparison requires an FCmp instruction".into());
            }
            *lhs = resolve_use(output, allocs[0], *ty, 0, spill_base)?;
            *rhs = resolve_use(output, allocs[1], *ty, 1, spill_base)?;
            None
        }
        Inst::CSet { dst, .. } => {
            expect_alloc_count(inst, allocs, 1)?;
            let (reg, spill) = resolve_def(allocs[0], Type::new_i32(), 0)?;
            *dst = reg;
            spill
        }
        Inst::RetI32 { src } => {
            expect_alloc_count(inst, allocs, 1)?;
            *src = resolve_use(output, allocs[0], Type::new_i32(), 0, spill_base)?;
            None
        }
        Inst::Jump { .. } | Inst::Branch { .. } | Inst::Call { .. } | Inst::Ret | Inst::Nop => {
            expect_alloc_count(inst, allocs, 0)?;
            None
        }
    };

    writeln!(output, "    {}", format_inst(function, &rewritten)).unwrap();
    if let Some((reg, allocation, ty)) = spill_def {
        emit_spill_access(
            output,
            "str",
            reg,
            ty,
            spill_offset(allocation, spill_base)?,
        )?;
    }
    Ok(())
}

/// Emits a finalized VCode instruction stream without changing allocator edit
/// order. Each program point is emitted around its corresponding instruction.
pub fn emit_post_ra_stream(
    output: &mut String,
    function: &str,
    insts: &[Inst],
    allocations: &RegAllocOutput,
    spill_base: u32,
) -> Result<(), String> {
    if allocations.inst_alloc_offsets.len() != insts.len() {
        return Err(format!(
            "register allocation has {} instruction allocation ranges for {} instructions",
            allocations.inst_alloc_offsets.len(),
            insts.len()
        ));
    }

    for (index, inst) in insts.iter().enumerate() {
        let index = index as u32;
        emit_edits_at(
            output,
            allocations,
            index,
            taki_mir::reg_alloc::reg::InstPosition::Before,
            spill_base,
        )?;
        emit_post_ra_inst(
            output,
            function,
            inst,
            allocations.inst_allocs(index),
            spill_base,
        )?;
        emit_edits_at(
            output,
            allocations,
            index,
            taki_mir::reg_alloc::reg::InstPosition::After,
            spill_base,
        )?;
    }
    Ok(())
}

fn emit_edits_at(
    output: &mut String,
    allocations: &RegAllocOutput,
    index: u32,
    position: taki_mir::reg_alloc::reg::InstPosition,
    spill_base: u32,
) -> Result<(), String> {
    for (point, edit) in &allocations.edits {
        if point.inst() == index && point.pos() == position {
            emit_post_ra_move(output, edit, spill_base)?;
        }
    }
    Ok(())
}

/// Emits the frame state required by a finalized post-RA function body.
pub fn emit_post_ra_prologue(output: &mut String, layout: &FrameLayout) -> Result<(), String> {
    writeln!(output, "    stp x29, x30, [sp, #-16]!").unwrap();
    writeln!(output, "    mov x29, sp").unwrap();
    emit_sp_adjust(output, "sub", layout.frame_size)?;

    let mut offset = layout.outgoing_args + layout.locals + layout.spills;
    for preg in &layout.used_callee_saves {
        if preg.class() != taki_mir::reg_alloc::reg::RegClass::Int {
            return Err("AArch64 post-RA frame only supports integer callee saves".into());
        }
        emit_spill_access(
            output,
            "str",
            Reg::from_physical_reg(*preg),
            Type::new_i64(),
            offset,
        )?;
        offset += 8;
    }
    Ok(())
}

/// Restores the frame state established by [`emit_post_ra_prologue`].
pub fn emit_post_ra_epilogue(output: &mut String, layout: &FrameLayout) -> Result<(), String> {
    let save_base = layout.outgoing_args + layout.locals + layout.spills;
    for (index, preg) in layout.used_callee_saves.iter().enumerate().rev() {
        emit_spill_access(
            output,
            "ldr",
            Reg::from_physical_reg(*preg),
            Type::new_i64(),
            save_base + (index as u32) * 8,
        )?;
    }
    emit_sp_adjust(output, "add", layout.frame_size)?;
    writeln!(output, "    ldp x29, x30, [sp], #16").unwrap();
    writeln!(output, "    ret").unwrap();
    Ok(())
}

fn emit_sp_adjust(output: &mut String, opcode: &str, amount: u32) -> Result<(), String> {
    if amount == 0 {
        return Ok(());
    }
    if amount <= 4095 {
        writeln!(output, "    {opcode} sp, sp, #{amount}").unwrap();
        return Ok(());
    }
    writeln!(output, "    movz x17, #{}", amount & 0xffff).unwrap();
    if amount >> 16 != 0 {
        writeln!(output, "    movk x17, #{}, lsl #16", amount >> 16).unwrap();
    }
    writeln!(output, "    {opcode} sp, sp, x17").unwrap();
    Ok(())
}

fn expect_alloc_count(inst: &Inst, allocs: &[Allocation], expected: usize) -> Result<(), String> {
    if allocs.len() == expected {
        Ok(())
    } else {
        Err(format!(
            "register allocation returned {} locations for {inst:?}; expected {expected}",
            allocs.len()
        ))
    }
}

fn resolve_use(
    output: &mut String,
    allocation: Allocation,
    ty: Type,
    scratch_index: usize,
    spill_base: u32,
) -> Result<Reg, String> {
    if let Some(reg) = allocation.as_reg().map(Reg::from_physical_reg) {
        return Ok(reg);
    }
    let scratch = scratch_reg(ty, scratch_index)?;
    emit_spill_access(
        output,
        "ldr",
        scratch,
        ty,
        spill_offset(allocation, spill_base)?,
    )?;
    Ok(scratch)
}

fn resolve_def(
    allocation: Allocation,
    ty: Type,
    scratch_index: usize,
) -> Result<(Reg, Option<(Reg, Allocation, Type)>), String> {
    if let Some(reg) = allocation.as_reg().map(Reg::from_physical_reg) {
        return Ok((reg, None));
    }
    let scratch = scratch_reg(ty, scratch_index)?;
    Ok((scratch, Some((scratch, allocation, ty))))
}

fn scratch_reg(ty: Type, scratch_index: usize) -> Result<Reg, String> {
    match (ty.is_f32(), scratch_index) {
        (false, 0) => Ok(regs::int_reg(regs::INT_SCRATCH0)),
        (false, 1) => Ok(regs::int_reg(regs::INT_SCRATCH1)),
        (true, 0) => Ok(regs::float_reg(regs::FP_SCRATCH)),
        (true, 1) => Ok(regs::float_reg(regs::FP_SCRATCH1)),
        _ => Err("instruction needs more post-RA scratch registers than AArch64 reserves".into()),
    }
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
    use taki_mir::reg_alloc::reg::{
        Allocation, Edit, Output as RegAllocOutput, ProgPoint, SpillSlot,
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

    #[test]
    fn rewrites_spilled_instruction_operands() {
        let mut output = String::new();
        emit_post_ra_inst(
            &mut output,
            "main",
            &Inst::Add {
                dst: regs::int_reg(0),
                lhs: regs::int_reg(1),
                rhs: regs::int_reg(2),
                ty: Type::new_i32(),
            },
            &[
                Allocation::stack(SpillSlot::new(0)),
                Allocation::stack(SpillSlot::new(8)),
                Allocation::stack(SpillSlot::new(16)),
            ],
            32,
        )
        .unwrap();
        emit_post_ra_inst(
            &mut output,
            "main",
            &Inst::Cmp {
                lhs: regs::int_reg(0),
                rhs: regs::int_reg(1),
                ty: Type::new_i32(),
            },
            &[
                Allocation::stack(SpillSlot::new(0)),
                Allocation::stack(SpillSlot::new(8)),
            ],
            0,
        )
        .unwrap();

        assert_eq!(
            output,
            "    ldr w16, [sp, #32]\n    ldr w17, [sp, #40]\n    add w16, w16, w17\n    str w16, [sp, #48]\n    ldr w16, [sp, #0]\n    ldr w17, [sp, #8]\n    cmp w16, w17\n"
        );
    }

    #[test]
    fn formats_float_moves_with_fmov() {
        let program = AsmProgram {
            functions: vec![AsmFunction {
                name: "main".into(),
                blocks: vec![AsmBlock {
                    index: MirBlockIndex::new(0),
                    insts: vec![Inst::Mov {
                        dst: regs::float_reg(0),
                        src: regs::float_reg(1),
                        ty: Type::new_f32(),
                    }],
                }],
            }],
        };
        assert!(program.emit().contains("    fmov s0, s1\n"));
    }

    #[test]
    fn preserves_before_instruction_after_edit_order() {
        let mut output = String::new();
        let allocations = RegAllocOutput {
            num_spillslots: 0,
            edits: vec![
                (
                    ProgPoint::before(0),
                    Edit::Move {
                        from: Allocation::reg(regs::int_preg(0)),
                        to: Allocation::reg(regs::int_preg(1)),
                        ty: Type::new_i32(),
                    },
                ),
                (
                    ProgPoint::after(0),
                    Edit::Move {
                        from: Allocation::reg(regs::int_preg(1)),
                        to: Allocation::reg(regs::int_preg(2)),
                        ty: Type::new_i32(),
                    },
                ),
            ],
            allocs: vec![Allocation::reg(regs::int_preg(3))],
            inst_alloc_offsets: vec![0],
        };
        emit_post_ra_stream(
            &mut output,
            "main",
            &[Inst::CSet {
                dst: regs::int_reg(0),
                cond: crate::inst::Cond::Eq,
            }],
            &allocations,
            0,
        )
        .unwrap();

        assert_eq!(output, "    mov w1, w0\n    cset w3, eq\n    mov w2, w1\n");
    }

    #[test]
    fn emits_finalized_callee_save_frame() {
        let layout = FrameLayout {
            outgoing_args: 8,
            locals: 8,
            spills: 16,
            callee_saves: 16,
            frame_size: 48,
            used_callee_saves: vec![regs::int_preg(19), regs::int_preg(21)],
        };
        let mut output = String::new();
        emit_post_ra_prologue(&mut output, &layout).unwrap();
        emit_post_ra_epilogue(&mut output, &layout).unwrap();

        assert_eq!(
            output,
            "    stp x29, x30, [sp, #-16]!\n    mov x29, sp\n    sub sp, sp, #48\n    str x19, [sp, #32]\n    str x21, [sp, #40]\n    ldr x21, [sp, #40]\n    ldr x19, [sp, #32]\n    add sp, sp, #48\n    ldp x29, x30, [sp], #16\n    ret\n"
        );
    }

    #[test]
    fn clang_accepts_finalized_frame() {
        let layout = FrameLayout {
            outgoing_args: 0,
            locals: 0,
            spills: 0,
            callee_saves: 8,
            frame_size: 65_536,
            used_callee_saves: vec![regs::int_preg(19)],
        };
        let mut body = String::new();
        emit_post_ra_prologue(&mut body, &layout).unwrap();
        emit_post_ra_epilogue(&mut body, &layout).unwrap();
        let assembly = format!("    .text\nmain:\n{body}");
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
            "clang rejected finalized frame: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn emits_block_labelled_post_ra_function_with_shared_epilogue() {
        let layout = FrameLayout::new(0, 0, 0, 0);
        let allocations = RegAllocOutput {
            num_spillslots: 0,
            edits: vec![(
                ProgPoint::before(1),
                Edit::Move {
                    from: Allocation::reg(regs::int_preg(0)),
                    to: Allocation::reg(regs::int_preg(1)),
                    ty: Type::new_i32(),
                },
            )],
            allocs: vec![
                Allocation::reg(regs::int_preg(0)),
                Allocation::reg(regs::int_preg(2)),
            ],
            inst_alloc_offsets: vec![0, 1],
        };
        let blocks = [
            PostRaBlock {
                index: MirBlockIndex::new(0),
                inst_range: 0..1,
                insts: &[Inst::CSet {
                    dst: regs::int_reg(0),
                    cond: crate::inst::Cond::Eq,
                }],
            },
            PostRaBlock {
                index: MirBlockIndex::new(1),
                inst_range: 1..2,
                insts: &[Inst::Ret],
            },
        ];
        let mut output = String::new();
        emit_post_ra_function(&mut output, "main", &blocks, &allocations, &layout).unwrap();

        assert_eq!(
            output,
            "    .p2align 2\n    .globl main\n    .type main, %function\nmain:\n    stp x29, x30, [sp, #-16]!\n    mov x29, sp\n.Lmain_bb0:\n    cset w0, eq\n.Lmain_bb1:\n    mov w1, w0\n    b .Lmain_epilogue\n.Lmain_epilogue:\n    ldp x29, x30, [sp], #16\n    ret\n    .size main, .-main\n"
        );
    }

    #[test]
    fn clang_accepts_block_labelled_post_ra_function() {
        let layout = FrameLayout::new(0, 0, 0, 0);
        let allocations = RegAllocOutput {
            num_spillslots: 0,
            edits: vec![],
            allocs: vec![Allocation::reg(regs::int_preg(0))],
            inst_alloc_offsets: vec![0],
        };
        let blocks = [PostRaBlock {
            index: MirBlockIndex::new(0),
            inst_range: 0..1,
            insts: &[Inst::Ret],
        }];
        let mut body = String::from("    .text\n");
        emit_post_ra_function(&mut body, "main", &blocks, &allocations, &layout).unwrap();
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
            .write_all(body.as_bytes())
            .unwrap();
        let output = clang.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "clang rejected post-RA function: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
