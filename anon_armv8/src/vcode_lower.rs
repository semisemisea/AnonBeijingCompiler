use raana_ir::ir::{
    inst_kind::{BinaryOp, InstKind},
    Function as HirFunction, Program,
};
use taki_mir::prelude::Arena;
use taki_mir::{
    abi::ArgPair,
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
    if data.params().len() > 8
        || data
            .params()
            .iter()
            .any(|param| !data.inst_data(*param).ty().is_i32())
    {
        return Err(format!(
            "VCode AArch64 lowering for {} supports at most eight i32 register parameters",
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
                    BinaryOp::Sub => ctx.emit_inst(Inst::Sub {
                        dst,
                        lhs,
                        rhs,
                        ty: Type::new_i32(),
                    }),
                    BinaryOp::Mul => ctx.emit_inst(Inst::Mul {
                        dst,
                        lhs,
                        rhs,
                        ty: Type::new_i32(),
                    }),
                    BinaryOp::Div => ctx.emit_inst(Inst::SDiv {
                        dst,
                        lhs,
                        rhs,
                        ty: Type::new_i32(),
                    }),
                    BinaryOp::Rem => {
                        let quotient = ctx.alloc_temp(Type::new_i32());
                        ctx.emit_inst(Inst::SDiv {
                            dst: quotient,
                            lhs,
                            rhs,
                            ty: Type::new_i32(),
                        });
                        ctx.emit_inst(Inst::MSub {
                            dst,
                            mul_lhs: quotient,
                            mul_rhs: rhs,
                            sub: lhs,
                            ty: Type::new_i32(),
                        });
                    }
                    BinaryOp::And => ctx.emit_inst(Inst::And {
                        dst,
                        lhs,
                        rhs,
                        ty: Type::new_i32(),
                    }),
                    BinaryOp::Or => ctx.emit_inst(Inst::Orr {
                        dst,
                        lhs,
                        rhs,
                        ty: Type::new_i32(),
                    }),
                    BinaryOp::Xor => ctx.emit_inst(Inst::Eor {
                        dst,
                        lhs,
                        rhs,
                        ty: Type::new_i32(),
                    }),
                    BinaryOp::Shl => ctx.emit_inst(Inst::Lsl {
                        dst,
                        lhs,
                        rhs,
                        ty: Type::new_i32(),
                    }),
                    BinaryOp::Shr => ctx.emit_inst(Inst::Lsr {
                        dst,
                        lhs,
                        rhs,
                        ty: Type::new_i32(),
                    }),
                    BinaryOp::Sar => ctx.emit_inst(Inst::Asr {
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
            InstKind::Call(call) => {
                let callee = ctx.program().func_data(call.callee());
                if !callee.ret_ty().is_i32()
                    || call.args().len() > 8
                    || call
                        .args()
                        .iter()
                        .any(|arg| !ctx.inst_data(*arg).ty().is_i32())
                {
                    panic!(
                        "VCode AArch64 lowering only supports direct calls with at most eight i32 arguments and an i32 result"
                    );
                }
                let args = call
                    .args()
                    .iter()
                    .enumerate()
                    .map(|(index, arg)| ArgPair {
                        vreg: ctx.value_reg(*arg),
                        preg: crate::regs::int_reg(index as u8),
                        ty: Type::new_i32(),
                    })
                    .collect();
                ctx.emit_inst(Inst::Call {
                    symbol: callee.name().to_owned(),
                    args,
                    result: Some((ctx.result_reg(inst), Type::new_i32())),
                });
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
        ctx: &mut LowerContext<Inst>,
        inst: taki_mir::prelude::HirInst,
        target: &[MirBlockIndex],
    ) {
        match ctx.inst_data(inst).kind() {
            InstKind::Jump(_) => {
                assert_eq!(target.len(), 1, "jump must have exactly one target");
                ctx.emit_inst(Inst::Jump { target: target[0] });
            }
            InstKind::Branch(branch) => {
                assert_eq!(target.len(), 2, "branch must have exactly two targets");
                let cond = ctx.value_reg(branch.cond());
                ctx.emit_inst(Inst::CmpZero {
                    src: cond,
                    ty: Type::new_i32(),
                });
                ctx.emit_inst(Inst::Branch {
                    cond: Cond::Ne,
                    target: target[0],
                });
                ctx.emit_inst(Inst::Jump { target: target[1] });
            }
            kind => {
                panic!("VCode AArch64 branch lowering received non-branch instruction: {kind:?}")
            }
        }
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
    use taki_mir::prelude::{Arena, BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder};

    use super::compile_function_vcode;

    #[test]
    fn lowers_and_assembles_integer_arithmetic_return() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let lhs = data.new_local_inst().integer(40_000);
        let rhs = data.new_local_inst().integer(-2);
        let sub = data.new_local_inst().binary(BinaryOp::Sub, lhs, rhs);
        let mul = data.new_local_inst().binary(BinaryOp::Mul, sub, rhs);
        let div = data.new_local_inst().binary(BinaryOp::Div, mul, rhs);
        let ret = data.new_local_inst().ret(Some(div));
        data.layout_mut().insert_inst(entry, sub);
        data.layout_mut().insert_inst(entry, mul);
        data.layout_mut().insert_inst(entry, div);
        data.layout_mut().insert_inst(entry, ret);

        let assembly = compile_function_vcode(&program, function).unwrap();
        assert!(assembly.contains("movz"));
        assert!(assembly.contains("movk"));
        assert!(assembly.contains("sub"), "{assembly}");
        assert!(assembly.contains("mul"), "{assembly}");
        assert!(assembly.contains("sdiv"), "{assembly}");
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

    #[test]
    fn lowers_all_signed_integer_comparisons() {
        for (op, condition) in [
            (BinaryOp::Eq, "eq"),
            (BinaryOp::NotEq, "ne"),
            (BinaryOp::Lt, "lt"),
            (BinaryOp::Le, "le"),
            (BinaryOp::Gt, "gt"),
            (BinaryOp::Ge, "ge"),
        ] {
            let mut program = Program::new();
            let function = program.new_function(Type::get_i32(), "main".into(), vec![]);
            let data = program.func_data_mut(function);
            let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
            data.layout_mut().push_bb_back(entry);
            let lhs = data.new_local_inst().integer(-1);
            let rhs = data.new_local_inst().integer(1);
            let comparison = data.new_local_inst().binary(op, lhs, rhs);
            let ret = data.new_local_inst().ret(Some(comparison));
            data.layout_mut().insert_inst(entry, comparison);
            data.layout_mut().insert_inst(entry, ret);

            let assembly = compile_function_vcode(&program, function).unwrap();
            assert!(assembly.contains("cset w"), "{assembly}");
            assert!(assembly.contains(&format!(", {condition}")), "{assembly}");
        }
    }

    #[test]
    fn lowers_and_assembles_remaining_integer_operations() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let lhs = data.new_local_inst().integer(-29);
        let rhs = data.new_local_inst().integer(6);
        let rem = data.new_local_inst().binary(BinaryOp::Rem, lhs, rhs);
        let and = data.new_local_inst().binary(BinaryOp::And, rem, rhs);
        let or = data.new_local_inst().binary(BinaryOp::Or, and, lhs);
        let xor = data.new_local_inst().binary(BinaryOp::Xor, or, rhs);
        let shl = data.new_local_inst().binary(BinaryOp::Shl, xor, rhs);
        let shr = data.new_local_inst().binary(BinaryOp::Shr, shl, rhs);
        let sar = data.new_local_inst().binary(BinaryOp::Sar, shr, rhs);
        let ret = data.new_local_inst().ret(Some(sar));
        for inst in [rem, and, or, xor, shl, shr, sar, ret] {
            data.layout_mut().insert_inst(entry, inst);
        }

        let assembly = compile_function_vcode(&program, function).unwrap();
        for opcode in [
            "sdiv w", "msub w", "and w", "orr w", "eor w", "lsl w", "lsr w", "asr w",
        ] {
            assert!(
                assembly.contains(opcode),
                "missing {opcode} in:\n{assembly}"
            );
        }

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

    #[test]
    fn lowers_and_assembles_i32_register_parameters() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "sum".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let params = data.params().to_vec();
        let sum = data
            .new_local_inst()
            .binary(BinaryOp::Add, params[0], params[1]);
        let ret = data.new_local_inst().ret(Some(sum));
        data.layout_mut().insert_inst(entry, sum);
        data.layout_mut().insert_inst(entry, ret);

        let assembly = compile_function_vcode(&program, function).unwrap();
        assert!(assembly.contains("mov w0, w0"), "{assembly}");
        assert!(assembly.contains("mov w1, w1"), "{assembly}");
        assert!(assembly.contains("add w"), "{assembly}");

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

    #[test]
    fn lowers_and_assembles_i32_direct_call() {
        let mut program = Program::new();
        let callee = program.new_function(
            Type::get_i32(),
            "add".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        let caller = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let data = program.func_data_mut(caller);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let lhs = data.new_local_inst().integer(40);
        let rhs = data.new_local_inst().integer(2);
        let call = data
            .new_local_inst()
            .call_with_type(callee, vec![lhs, rhs], Type::get_i32());
        let ret = data.new_local_inst().ret(Some(call));
        data.layout_mut().insert_inst(entry, call);
        data.layout_mut().insert_inst(entry, ret);

        let assembly = compile_function_vcode(&program, caller).unwrap();
        assert!(assembly.contains("bl add"), "{assembly}");
        assert!(assembly.contains("movz w0, #40"), "{assembly}");
        assert!(assembly.contains("movz w1, #2"), "{assembly}");

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

    #[test]
    fn preserves_i32_value_live_across_direct_call() {
        let mut program = Program::new();
        let callee = program.new_function(
            Type::get_i32(),
            "add".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        let caller = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let data = program.func_data_mut(caller);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let preserved = data.new_local_inst().integer(7);
        let lhs = data.new_local_inst().integer(40);
        let rhs = data.new_local_inst().integer(2);
        let call = data
            .new_local_inst()
            .call_with_type(callee, vec![lhs, rhs], Type::get_i32());
        let sum = data.new_local_inst().binary(BinaryOp::Add, preserved, call);
        let ret = data.new_local_inst().ret(Some(sum));
        for inst in [call, sum, ret] {
            data.layout_mut().insert_inst(entry, inst);
        }

        let assembly = compile_function_vcode(&program, caller).unwrap();
        let call = assembly.find("    bl add").expect("missing direct call");
        let add = assembly[call..]
            .find("    add w")
            .map(|offset| call + offset)
            .expect("missing post-call use");
        assert!(call < add, "{assembly}");
        assert!(
            assembly.contains("str w")
                || (19..=28).any(|reg| assembly.contains(&format!("str x{reg}"))),
            "live value was neither spilled nor saved in a callee-save register:\n{assembly}"
        );

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

    #[test]
    fn spills_i32_values_under_cross_call_register_pressure() {
        let mut program = Program::new();
        let callee = program.new_function(
            Type::get_i32(),
            "add".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        let caller = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let data = program.func_data_mut(caller);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let values = (1..=32)
            .map(|value| data.new_local_inst().integer(value))
            .collect::<Vec<_>>();
        let call = data.new_local_inst().call_with_type(
            callee,
            vec![values[0], values[1]],
            Type::get_i32(),
        );
        let mut sum = call;
        let mut insts = vec![call];
        for value in values.iter().skip(2) {
            sum = data.new_local_inst().binary(BinaryOp::Add, sum, *value);
            insts.push(sum);
        }
        let ret = data.new_local_inst().ret(Some(sum));
        insts.push(ret);
        for inst in insts {
            data.layout_mut().insert_inst(entry, inst);
        }

        let assembly = compile_function_vcode(&program, caller).unwrap();
        let call = assembly.find("    bl add").expect("missing direct call");
        let stores_before_call = assembly[..call].matches("    str w").count();
        let reloads_after_call = assembly[call..].matches("    ldr w").count();
        assert!(
            stores_before_call > 0,
            "expected spills before call:\n{assembly}"
        );
        assert!(
            reloads_after_call > 0,
            "expected reloads after call:\n{assembly}"
        );
        assert!(
            assembly[call..].matches("    add w").count() >= 30,
            "{assembly}"
        );

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

    #[test]
    fn lowers_and_assembles_integer_control_flow() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        let decide = data.new_basic_block().basic_block("decide".into(), vec![]);
        let on_true = data.new_basic_block().basic_block("true".into(), vec![]);
        let on_false = data.new_basic_block().basic_block("false".into(), vec![]);
        for block in [entry, decide, on_true, on_false] {
            data.layout_mut().push_bb_back(block);
        }

        let jump = data.new_local_inst().jump(decide, vec![]);
        data.layout_mut().insert_inst(entry, jump);
        let cond = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(cond, on_true, vec![], on_false, vec![]);
        data.layout_mut().insert_inst(decide, branch);
        let true_value = data.new_local_inst().integer(7);
        let true_return = data.new_local_inst().ret(Some(true_value));
        data.layout_mut().insert_inst(on_true, true_return);
        let false_value = data.new_local_inst().integer(9);
        let false_return = data.new_local_inst().ret(Some(false_value));
        data.layout_mut().insert_inst(on_false, false_return);

        let assembly = compile_function_vcode(&program, function).unwrap();
        let compare = assembly.find("    cmp w").expect("missing compare");
        let branch = assembly
            .find("    b.ne .Lmain_bb")
            .expect("missing conditional branch");
        let jump = assembly[branch + 1..]
            .find("    b .Lmain_bb")
            .map(|offset| branch + 1 + offset)
            .expect("missing false-edge jump");
        assert!(compare < branch && branch < jump, "{assembly}");
        assert!(assembly.contains(", #0"), "{assembly}");
        assert!(
            assembly.matches("    b .Lmain_bb").count() >= 2,
            "{assembly}"
        );

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

    #[test]
    fn lowers_and_assembles_jump_block_parameter() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        let join = data
            .new_basic_block()
            .basic_block("join".into(), vec![Type::get_i32()]);
        let param = data.bb_data(join).params()[0];
        data.layout_mut().push_bb_back(entry);
        data.layout_mut().push_bb_back(join);

        let value = data.new_local_inst().integer(42);
        let jump = data.new_local_inst().jump(join, vec![value]);
        data.layout_mut().insert_inst(entry, jump);
        let ret = data.new_local_inst().ret(Some(param));
        data.layout_mut().insert_inst(join, ret);

        let assembly = compile_function_vcode(&program, function).unwrap();
        let transfer = assembly
            .find("    str w")
            .expect("missing edge spill store");
        let jump = assembly.find("    b .Lmain_bb1").expect("missing jump");
        let reload = assembly[jump..]
            .find("    ldr w")
            .map(|offset| jump + offset)
            .expect("missing block-parameter reload");
        assert!(transfer < jump && jump < reload, "{assembly}");

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
