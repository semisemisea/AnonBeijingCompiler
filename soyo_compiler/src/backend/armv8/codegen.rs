mod asm_gen_context;
mod epilogue;
mod register_alloc;
mod register_manager;
mod generate_asm;

use raana_ir::ir::{
    FunctionData, InstKind, Inst, Program, Type, TypeKind,
    Binary, BlockArgRef, Branch, Call, GetElemPtr, GetPtr, Integer, Jump, Load, Return, Store
};

use crate::{
    backend::armv8::register::Register,
};
use register_alloc::{
    RegisterAllocationResult
};
use generate_asm::GenerateAsm;
use asm_gen_context::AsmGenContext;

const AUTO_FUNC_ARG_ON_STACK: bool = true;

#[inline]
pub fn inst_size(func: &FunctionData, val: Inst) -> usize {
    match func.dfg().value(val).kind() {
        InstKind::Alloc => ptr_size(func.dfg().value(val).ty()),
        _ => func.dfg().value(val).ty().size(),
    }
}

impl GenerateAsm for FunctionData {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) -> anyhow::Result<()> {
        ctx.writeln(&format!("{}:", self.name().strip_prefix("@").unwrap()));
        ctx.incr_indent();

        let RegisterAllocationResult {
            allocation,
            offset,
            call_ra,
            extra_args,
            callee_usage,
        } = register_alloc::liveness_analysis(ctx.curr_func_data(program));

        *ctx.allocation_mut() = allocation;

        ctx.prologue(offset, call_ra, callee_usage);

        let curr_offset = (extra_args.max(8) - 8) * 4;

        let mut curr_offset = if AUTO_FUNC_ARG_ON_STACK {
            self.params()
                .iter()
                .take(8)
                .fold(curr_offset, |acc_offset, &param| {
                    use crate::riscv_utils::Register::a0;
                    ctx.insert_inst(param, acc_offset);
                    // ctx.load_to_register(program, param);
                    let InstKind::FuncArgRef(arg_ref) =
                        ctx.curr_func_data(program).dfg().value(param).kind()
                    else {
                        unreachable!()
                    };
                    let reg = (a0 as u8 + arg_ref.index() as u8).try_into().unwrap();
                    ctx.alloc_para_reg(reg);
                    ctx.save_word_at_inst(param);
                    acc_offset + self.dfg().value(param).ty().size()
                })
        } else {
            curr_offset
        };

        self.params()
            .iter()
            .skip(8)
            // TODO:
            .fold(offset, |acc_offset, &param| {
                ctx.insert_inst(param, acc_offset);
                acc_offset + self.dfg().value(param).ty().size()
            });

        for &bb_param in self
            .layout()
            .bbs()
            .iter()
            .flat_map(|(&bb, _)| self.dfg().bb(bb).params())
        {
            ctx.insert_inst(bb_param, curr_offset);
            curr_offset += inst_size(self, bb_param);
        }

        // then handle each instruction.
        for (&bb, node) in self.layout().bbs() {
            if self.dfg().bb(bb).name().as_ref().unwrap() != "%entry" {
                ctx.decr_indent();
                ctx.writeln(&format!("{}:", ctx.get_bb_name(bb, program)));
                ctx.incr_indent();
            }
            let insts_iter = node.insts().keys();
            for &inst in insts_iter {
                // update the current instruction.
                let single_inst_size = inst_size(self, inst);
                if single_inst_size != 0 {
                    ctx.insert_inst(inst, curr_offset);
                }
                ctx.curr_inst_mut().replace(inst);
                inst.generate(program, ctx)?;
                curr_offset += inst_size(self, inst);
            }
        }

        ctx.decr_indent();
        ctx.pop_epilogue();
        Ok(())
    }
}

impl GenerateAsm for Inst {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) -> anyhow::Result<()> {
        match ctx.curr_func_data(program).dfg().value(*self).kind() {
            InstKind::Integer(int) => int.generate(program, ctx),
            InstKind::Alloc => todo!(),
            InstKind::Store(store) => store.generate(program, ctx),
            InstKind::Load(load) => load.generate(program, ctx),
            InstKind::Branch(branch) => branch.generate(program, ctx),
            InstKind::Jump(jump) => jump.generate(program, ctx),
            InstKind::Return(ret) => ret.generate(program, ctx),
            InstKind::Binary(op) => op.generate(program, ctx),
            InstKind::BlockArgRef(block_arg_ref) => block_arg_ref.generate(program, ctx),
            InstKind::Call(call) => call.generate(program, ctx),
            InstKind::GetElemPtr(get_elem_ptr) => get_elem_ptr.generate(program, ctx),
            InstKind::GetPtr(get_ptr) => get_ptr.generate(program, ctx),
            unimpl => todo!("{:?}", unimpl),
        }
    }
}

/// ```
/// GetPtr {
///     src: Inst
///     index: Inst
/// }
/// ```
impl GenerateAsm for GetPtr {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) -> anyhow::Result<()> {
        let global_flag = self.src().is_global();
        // element type size
        let pointee_size = if global_flag {
            ptr_size(program.borrow_value(self.src()).ty())
        } else {
            ptr_size(ctx.curr_func_data(program).dfg().value(self.src()).ty())
        };
        // load index to register
        ctx.load_to_register(program, self.index());
        // load element type size to register
        ctx.load_imm(pointee_size as _);
        // do multipication
        ctx.multiply();

        // get the base address of array
        if global_flag {
            ctx.load_address(get_glob_var_name(self.src(), program));
            // ctx.load_from_address();
            ctx.add_op();
        } else {
            // let pre_drifted_address = ctx.get_inst_offset(self.src()).unwrap();
            if let Some(pre_drifted_address) = ctx.register_or_offset(self.src()) {
                ctx.load_word_sp(pre_drifted_address as _);
            }
            ctx.add_op()
        }

        ctx.save_word_at_curr_inst();
        Ok(())
    }
}

/// ```
/// GetElemPtr {
///     src: Inst, // alloc kind
///     index: Inst,
/// }
/// ```
impl GenerateAsm for GetElemPtr {
    // base_addr + elem_ty_size * index
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) -> anyhow::Result<()> {
        let global_flag = self.src().is_global();
        let ptr_flag = if global_flag {
            false
        } else {
            is_ptr(self.src(), ctx.curr_func_data(program))
        };
        // let global_flag = is_get_elem_ptr_from_global(self, ctx.curr_func_data(program));
        // element type size
        let elem_ty_size = if global_flag {
            ptr_elem_size(program.borrow_value(self.src()).ty())
        } else {
            ptr_elem_size(ctx.curr_func_data(program).dfg().value(self.src()).ty())
        };
        // load index to register
        ctx.load_to_register(program, self.index());
        // load element type size to register
        ctx.load_imm(elem_ty_size as _);
        // do multipication
        ctx.multiply();

        // get the base address of array
        if global_flag {
            ctx.load_address(get_glob_var_name(self.src(), program));
            // ctx.load_from_address();
            ctx.add_op();
        } else if ptr_flag {
            // let pre_drifted_address = ctx.get_inst_offset(self.src()).unwrap();
            if let Some(pre_drifted_address) = ctx.register_or_offset(self.src()) {
                ctx.load_word_sp(pre_drifted_address as _);
            }
            ctx.add_op()
        } else {
            // let rel_base_address = ctx.get_inst_offset(self.src()).unwrap();
            if let Some(rel_base_address) = ctx.register_or_offset(self.src()) {
                ctx.load_imm(rel_base_address as _);
            }
            // add to the base_address
            ctx.add_op();
            // We are calculating relative offset. Add sp to make it absolute
            ctx.add_sp();
        }

        ctx.save_word_at_curr_inst();
        Ok(())
    }
}

/// ```
/// Call {
///     callee: Function,
///     args: Vec<Inst>,
/// }
/// ```
impl GenerateAsm for Call {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) -> anyhow::Result<()> {
        // arity of the function
        let arity = self.args().len();

        // reference to function arguments
        let args = self.args();

        // function data
        let func_data = program.func(self.callee());

        // name of the function
        let name = func_data.name().strip_prefix('@').unwrap();

        // move the 1st-8th parameters to the register
        for (i, &arg) in args[..8.min(arity)].iter().enumerate() {
            use Register::*;
            let rd = (a0 as u8 + i as u8).try_into().unwrap();
            ctx.load_to_para_register(program, arg, rd);
        }

        // move the 8th and more parameters to the stack
        for (i, &arg) in args.iter().skip(8).enumerate() {
            ctx.load_to_register(program, arg);
            ctx.save_word_with_offset(4 * i as i32);
        }

        // write the call instruction
        use crate::riscv_utils::RiscvInst::call;
        ctx.write_inst(call {
            callee: name.to_string(),
        });

        let TypeKind::Function(_param_ty, ret_ty) = func_data.ty().kind() else {
            unreachable!()
        };
        if !ret_ty.is_unit() {
            ctx.alloc_ret_reg();
            ctx.save_word_at_curr_inst();
        }

        Ok(())
    }
}

/// ```
/// BlockArgRef {
///     index: usize, // the index of basic block arguments
/// }
/// ```
impl GenerateAsm for BlockArgRef {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) -> anyhow::Result<()> {
        let curr_inst = ctx.curr_inst_mut().unwrap();
        ctx.load_to_register(program, curr_inst);
        Ok(())
    }
}

///```
/// Branch {
///     cond: Inst,
///     true_bb: BasicBlock,
///     false_bb: BasicBlock,
///     true_args: Vec<Inst>,
///     false_args: Vec<Inst>,
/// }
///```
impl GenerateAsm for Branch {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) -> anyhow::Result<()> {
        ctx.load_to_register(program, self.cond());
        let true_args_and_params = self
            .true_args()
            .iter()
            .zip(ctx.bb_params(self.true_bb(), program));
        let false_args_and_params = self
            .false_args()
            .iter()
            .zip(ctx.bb_params(self.false_bb(), program));
        true_args_and_params
            .chain(false_args_and_params)
            .for_each(|(&arg, &param)| {
                eprint!("params:");
                ctx.load_to_register(program, arg);
                ctx.save_word_at_inst(param);
            });
        ctx.if_jump(self.true_bb(), self.false_bb(), program);
        Ok(())
    }
}

///```
/// Jump {
///     target: BasicBlock,
///     args: Vec<Inst>,
/// }
///```
impl GenerateAsm for Jump {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) -> anyhow::Result<()> {
        let args_and_params = self
            .args()
            .iter()
            .zip(ctx.bb_params(self.target(), program));
        args_and_params.for_each(|(&arg, &param)| {
            ctx.load_to_register(program, arg);
            ctx.save_word_at_inst(param);
        });
        ctx.jump(self.target(), program);
        Ok(())
    }
}

impl GenerateAsm for Return {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) -> anyhow::Result<()> {
        if let Some(ret_val) = self.value() {
            ctx.load_to_register(program, ret_val);
            ctx.ret();
        } else {
            ctx.void_ret();
        }
        Ok(())
    }
}

impl GenerateAsm for Binary {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) -> anyhow::Result<()> {
        ctx.load_to_register(program, self.lhs());
        ctx.load_to_register(program, self.rhs());
        ctx.binary_op(self.op());
        ctx.save_word_at_curr_inst();
        Ok(())
    }
}

/// ```
/// Load {
///    src: Inst // alloc
/// }
/// ```
/// Get 1 register
impl GenerateAsm for Load {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) -> anyhow::Result<()> {
        let global_flag = self.src().is_global();
        let ptr_flag = if global_flag {
            false
        } else {
            is_ptr(self.src(), ctx.curr_func_data(program))
        };
        if global_flag {
            ctx.load_address(get_glob_var_name(self.src(), program));
            ctx.load_from_address();
            ctx.save_word_at_curr_inst();
        } else if ptr_flag {
            // let offset = ctx.get_inst_offset(self.src()).unwrap() as i32;
            if let Some(offset) = ctx.register_or_offset(self.src()) {
                ctx.load_word_sp(offset as _);
            }
            ctx.load_from_address();
            ctx.save_word_at_curr_inst();
        } else {
            // let offset = ctx.get_inst_offset(self.src()).unwrap() as i32;
            if let Some(offset) = ctx.register_or_offset(self.src()) {
                ctx.load_word_sp(offset as _);
            }
            ctx.save_word_at_curr_inst();
        }
        Ok(())
    }
}

impl GenerateAsm for Alloc {
    /// alloc is marker instruction for IR representation, we have already allocate a stack(sp) to
    /// store the instruction, so it won't have counterpart in RISC-V instruction
    #[allow(unused)]
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) -> anyhow::Result<()> {
        Ok(())
    }
}

/// ```
/// Store {
///    value: Inst,
///    dest: Inst, // From alloc instruction
/// }
/// ```
impl GenerateAsm for Store {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) -> anyhow::Result<()> {
        if self.dest().is_global() {
            ctx.load_address(get_glob_var_name(self.dest(), program));
            ctx.load_to_register(program, self.value());
            ctx.save_word_at_address();
        } else {
            match ctx.curr_func_data(program).dfg().value(self.dest()).kind() {
                InstKind::GetElemPtr(..) | InstKind::GetPtr(..) => {
                    ctx.load_to_register(program, self.dest());
                    ctx.load_to_register(program, self.value());
                    ctx.save_word_at_address();
                }
                _ => {
                    // store the value where it's located.
                    ctx.load_to_register(program, self.value());
                    ctx.save_word_at_inst(self.dest());
                }
            };
        }
        Ok(())
    }
}

///```
/// Integer {
///     val: i32,
/// }
/// ```
/// This instruction produce a i32 as instruction return value.
impl GenerateAsm for Integer {
    /// Load a ingeter immediate to a register.
    fn generate(&self, _program: &Program, ctx: &mut AsmGenContext) -> anyhow::Result<()> {
        ctx.load_imm(self.value());
        Ok(())
    }
}

/// also we remove the prefix '%'
#[inline]
fn get_glob_var_name(var: Inst, program: &Program) -> String {
    assert!(var.is_global());
    program
        .borrow_value(var)
        .name()
        .clone()
        .unwrap()
        .strip_prefix('%')
        .unwrap()
        .to_string()
}

// fn is_get_elem_ptr_from_global(inst: &values::GetElemPtr, func_data: &FunctionData) -> bool {
//     if inst.src().is_global() {
//         true
//     } else if let InstKind::GetElemPtr(child_inst) = func_data.dfg().value(inst.src()).kind() {
//         is_get_elem_ptr_from_global(child_inst, func_data)
//     } else {
//         false
//     }
// }

fn dereference(ty: &Type) -> &Type {
    use TypeKind::*;
    if let Pointer(point_to) = ty.kind() {
        dereference(point_to)
    } else {
        ty
    }
}

fn ptr_elem_size(ty: &Type) -> usize {
    use TypeKind::*;
    let point_to = dereference(ty);
    match point_to.kind() {
        Array(elem_ty, _len) => elem_ty.size(),
        Int32 => 4,
        _fuck => unreachable!("{point_to:?}"),
    }
}

fn ptr_size(ty: &Type) -> usize {
    use TypeKind::*;
    let Pointer(point_to) = ty.kind() else {
        unreachable!();
    };
    point_to.size()
}

#[inline]
fn is_ptr(val: Inst, func_data: &FunctionData) -> bool {
    matches!(
        func_data.dfg().value(val).kind(),
        InstKind::GetElemPtr(..) | InstKind::GetPtr(..)
    )
}