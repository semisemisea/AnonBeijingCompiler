use raana_ir::ir::{
    inst_kind::{BinaryOp, InstKind},
    Function as HirFunction, Program,
};
use taki_mir::{
    block_order::MirBlockIndex,
    lower::{LowerBackend, LowerContext},
    lower_function,
    types::Type,
    vcode::VCodeContainer,
};

use crate::{
    abi::FrameLayout,
    emit::{emit_post_ra_function, PostRaBlock},
    Cond, Inst,
};

/// Lowers the deliberately small integer subset currently represented by
/// [`Inst`] through VCode and register allocation. The direct lowering path
/// remains the production backend until this selector covers full SysY HIR.
pub fn compile_function_vcode(program: &Program, func: HirFunction) -> Result<String, String> {
    let data = program.func_data(func);
    if !data.params().is_empty() {
        return Err(format!(
            "VCode AArch64 lowering for {} does not support function parameters yet",
            data.name()
        ));
    }
    if !data.ret_ty().is_i32() {
        return Err(format!(
            "VCode AArch64 lowering for {} only supports i32 returns",
            data.name()
        ));
    }

    let vcode = lower_function(program, func, &IntegerBackend);
    let allocations = vcode.run_regalloc()?;
    let layout = FrameLayout::from_regalloc(0, 0, &allocations);
    let blocks = post_ra_blocks(&vcode);
    let mut output = String::from("    .text\n");
    emit_post_ra_function(&mut output, data.name(), &blocks, &allocations, &layout)
        .map_err(|error| format!("{error}; register allocation output: {allocations:?}"))?;
    Ok(output)
}

fn post_ra_blocks(vcode: &VCodeContainer<Inst>) -> Vec<PostRaBlock<'_>> {
    (0..vcode.num_blocks())
        .map(|index| {
            let index = MirBlockIndex::new(index);
            PostRaBlock {
                index,
                inst_range: vcode.inst_range_for_block(index),
                insts: vcode.insts_for_block(index),
            }
        })
        .collect()
}

struct IntegerBackend;

impl LowerBackend for IntegerBackend {
    type MInst = Inst;

    fn lower(&self, ctx: &mut LowerContext<Inst>, inst: taki_mir::prelude::HirInst) {
        let kind = ctx.inst_data(inst).kind().clone();
        let ty = ctx.inst_data(inst).ty().clone();
        match kind {
            InstKind::Integer(integer) => ctx.emit_inst(Inst::MovImm {
                dst: ctx.result_reg(inst),
                value: integer.value(),
            }),
            InstKind::Binary(binary) if ty.is_i32() => {
                let lhs = ctx.value_reg(binary.lhs());
                let rhs = ctx.value_reg(binary.rhs());
                let dst = ctx.result_reg(inst);
                match binary.op() {
                    BinaryOp::Add => ctx.emit_inst(Inst::Add {
                        dst,
                        lhs,
                        rhs,
                        ty: Type::new_i32(),
                    }),
                    op if op.is_compare() => {
                        ctx.emit_inst(Inst::Cmp {
                            lhs,
                            rhs,
                            ty: Type::new_i32(),
                        });
                        ctx.emit_inst(Inst::CSet {
                            dst,
                            cond: comparison_cond(op),
                        });
                    }
                    op => panic!("unsupported VCode AArch64 integer binary operation: {op}"),
                }
            }
            InstKind::Return(ret) => match ret.value() {
                Some(value) if ctx.inst_data(value).ty().is_i32() => {
                    let src = ctx.value_reg(value);
                    ctx.emit_inst(Inst::RetI32 { src });
                }
                Some(_) => panic!("VCode AArch64 lowering only supports i32 return values"),
                None => ctx.emit_inst(Inst::Ret),
            },
            kind => panic!("unsupported VCode AArch64 instruction: {kind:?}"),
        }
    }

    fn lower_branch(
        &self,
        _ctx: &mut LowerContext<Inst>,
        _inst: taki_mir::prelude::HirInst,
        _target: &[MirBlockIndex],
    ) {
        panic!("VCode AArch64 branch lowering is not implemented yet")
    }
}

fn comparison_cond(op: BinaryOp) -> Cond {
    match op {
        BinaryOp::Eq => Cond::Eq,
        BinaryOp::NotEq => Cond::Ne,
        BinaryOp::Lt => Cond::Lt,
        BinaryOp::Le => Cond::Le,
        BinaryOp::Gt => Cond::Gt,
        BinaryOp::Ge => Cond::Ge,
        _ => unreachable!("comparison condition requested for non-comparison operation"),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write as _,
        process::{Command, Stdio},
    };

    use raana_ir::ir::{BinaryOp, Program, Type};
    use taki_mir::prelude::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder};

    use super::compile_function_vcode;

    #[test]
    fn lowers_and_assembles_integer_add_return() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let lhs = data.new_local_inst().integer(40_000);
        let rhs = data.new_local_inst().integer(-2);
        let sum = data.new_local_inst().binary(BinaryOp::Add, lhs, rhs);
        let ret = data.new_local_inst().ret(Some(sum));
        data.layout_mut().insert_inst(entry, sum);
        data.layout_mut().insert_inst(entry, ret);

        let assembly = compile_function_vcode(&program, function).unwrap();
        assert!(assembly.contains("movz"));
        assert!(assembly.contains("movk"));
        assert!(assembly.contains("add"));
        assert!(assembly.contains(".Lmain_epilogue:"));

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
            "clang rejected generated assembly: {}\n{assembly}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
