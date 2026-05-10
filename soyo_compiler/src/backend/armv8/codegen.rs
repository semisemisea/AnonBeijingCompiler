pub(crate) mod asm_gen_context;
mod epilogue;
mod generate_asm;
mod register_alloc;

use raana_ir::ir::{
    Binary, BlockArgRef, Branch, Call, FunctionData, GetElemPtr, GetPtr, Inst, InstKind, Integer,
    Jump, Load, Program, Return, Store, Type, TypeKind, arena::Arena,
};

use crate::backend::armv8::inst::Inst::bl;
use crate::backend::armv8::register::{Bit, IReg, IntRegister, Register};
use asm_gen_context::AsmGenContext;
use generate_asm::GenerateAsm;
use register_alloc::RegisterAllocationResult;

const AUTO_FUNC_ARG_ON_STACK: bool = true;

#[inline]
pub fn inst_size(func: &FunctionData, inst: Inst) -> usize {
    match func.inst_data(inst).kind() {
        InstKind::Alloc => ptr_base_size(func.inst_data(inst).ty()),
        _ => func.inst_data(inst).ty().size(),
    }
}

impl GenerateAsm for FunctionData {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) {
        ctx.writeln(&format!("{}:", self.name()));
        ctx.incr_indent();

        let RegisterAllocationResult {
            allocation,
            offset,
            call_ra,
            extra_args,
            callee_usage,
        } = register_alloc::liveness_analysis(ctx.curr_func_data(program));

        *ctx.allocation_mut() = allocation;

        ctx.prologue(offset, call_ra, extra_args, callee_usage);

        let curr_offset = (extra_args.max(8) - 8) * 4;

        let mut curr_offset = if AUTO_FUNC_ARG_ON_STACK {
            self.params()
                .iter()
                .take(8)
                .fold(curr_offset, |acc_offset, &param| {
                    use crate::backend::armv8::register::*;
                    ctx.insert_inst(param, acc_offset);
                    // ctx.load_to_register(program, param);
                    let InstKind::FuncArgRef(arg_ref) =
                        ctx.curr_func_data(program).inst_data(param).kind()
                    else {
                        unreachable!()
                    };
                    let reg: IntRegister = (IntRegister::x0 as u8 + arg_ref.index() as u8)
                        .try_into()
                        .unwrap();
                    ctx.alloc_para_reg(Register::I(IReg(ctx.value_bit(program, param), reg)));
                    ctx.save_word_at_inst(program, param);
                    acc_offset + self.inst_data(param).ty().size()
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
                acc_offset + self.inst_data(param).ty().size()
            });

        for &bb_param in self
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|bb| self.bb_data(bb.bb()).params())
        {
            ctx.insert_inst(bb_param, curr_offset);
            curr_offset += inst_size(self, bb_param);
        }

        // then handle each instruction.
        for layout in self.layout().basicblocks() {
            let bb = layout.bb();
            let insts = layout.insts();
            if self.bb_data(bb).name() != "%entry" {
                ctx.decr_indent();
                ctx.writeln(&format!("{}:", ctx.get_bb_name(bb, program)));
                ctx.incr_indent();
            }
            for &inst in insts {
                // update the current instruction.
                let single_inst_size = inst_size(self, inst);
                if single_inst_size != 0 {
                    ctx.insert_inst(inst, curr_offset);
                }
                ctx.curr_inst_mut().replace(inst);
                inst.generate(program, ctx);
                curr_offset += inst_size(self, inst);
            }
        }

        ctx.decr_indent();
        ctx.pop_epilogue();
    }
}

impl GenerateAsm for Inst {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) {
        // FIXME: maybe incorrect use of curr_func_data
        match ctx.curr_func_data(program).inst_data(*self).kind() {
            InstKind::Integer(int) => int.generate(program, ctx),
            InstKind::Alloc => {}
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
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) {
        let global_flag = self.base().is_global();
        // element type size
        let pointee_size = if global_flag {
            ptr_base_size(program.inst_data(self.base()).ty())
        } else {
            // FIXME: maybe incorrect use of curr_func_data
            ptr_base_size(ctx.curr_func_data(program).inst_data(self.base()).ty())
        };
        // load index to register
        ctx.load_to_register(program, self.offset());
        // load element type size to register
        ctx.load_imm(pointee_size as _, ctx.value_bit(program, self.offset()));
        // do multipication
        ctx.multiply();

        // get the base address of array
        if global_flag {
            ctx.load_address(get_glob_var_name(self.base(), program));
            // ctx.load_from_address();
            ctx.add_op();
        } else {
            // let pre_drifted_address = ctx.get_inst_offset(self.src()).unwrap();
            if let Some(pre_drifted_address) = ctx.register_or_offset(program, self.base()) {
                ctx.load_word_sp(
                    pre_drifted_address as _,
                    ctx.value_bit(program, self.base()),
                );
            }
            ctx.add_op()
        }

        ctx.save_word_at_curr_inst(program);
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
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) {
        let global_flag = self.base().is_global();
        let ptr_flag = if global_flag {
            false
        } else {
            !matches!(
                ctx.curr_func_data(program).inst_data(self.base()).kind(),
                InstKind::Alloc
            )
        };
        // let global_flag = is_get_elem_ptr_from_global(self, ctx.curr_func_data(program));
        // element type size
        let elem_ty_size = if global_flag {
            ptr_base_elem_size(program.inst_data(self.base()).ty())
        } else {
            // FIXME: maybe incorrect use of curr_func_data
            ptr_base_elem_size(ctx.curr_func_data(program).inst_data(self.base()).ty())
        };
        // load index to register
        ctx.load_to_register(program, self.offset());
        // load element type size to register
        ctx.load_imm(elem_ty_size as _, ctx.value_bit(program, self.offset()));
        // do multipication
        ctx.multiply();

        // get the base address of array
        if global_flag {
            ctx.load_address(get_glob_var_name(self.base(), program));
            // ctx.load_from_address();
            ctx.add_op();
        } else if ptr_flag {
            // let pre_drifted_address = ctx.get_inst_offset(self.src()).unwrap();
            if let Some(pre_drifted_address) = ctx.register_or_offset(program, self.base()) {
                ctx.load_word_sp(
                    pre_drifted_address as _,
                    ctx.value_bit(program, self.base()),
                );
            }
            ctx.add_op()
        } else {
            // let rel_base_address = ctx.get_inst_offset(self.src()).unwrap();
            if let Some(rel_base_address) = ctx.register_or_offset(program, self.base()) {
                ctx.load_imm(rel_base_address as _, Bit::b64);
            }
            // add to the base_address
            ctx.add_op();
            // We are calculating relative offset. Add sp to make it absolute
            ctx.add_sp();
        }

        ctx.save_word_at_curr_inst(program);
    }
}

/// ```
/// Call {
///     callee: Function,
///     args: Vec<Inst>,
/// }
/// ```
impl GenerateAsm for Call {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) {
        // arity of the function
        let arity = self.args().len();

        // reference to function arguments
        let args = self.args();

        // function data
        let func_data = program.func_data(self.callee());

        // name of the function
        let name = func_data.name();

        // move the 1st-8th parameters to the register
        for (i, &arg) in args[..8.min(arity)].iter().enumerate() {
            use IntRegister::*;
            let rd = Register::I(IReg(
                ctx.value_bit(program, arg),
                IntRegister::try_from(x0 as u8 + i as u8).unwrap(),
            ));
            ctx.load_to_para_register(program, arg, rd);
        }

        // move the 8th and more parameters to the stack
        for (i, &arg) in args.iter().skip(8).enumerate() {
            ctx.load_to_register(program, arg);
            // TODO: validate the implementation follows ABI design.
            // the extra args should be stored at caller's [sp, 8 * i],
            // so the current code should be correct.
            ctx.save_word_with_offset(8 * i as i32);
        }

        // write the call instruction
        ctx.write_inst(bl {
            label: name.to_string(),
        });

        let ret_ty = func_data.ret_ty();
        if !ret_ty.is_unit() {
            ctx.alloc_ret_reg(Bit::try_from(ret_ty.size()).unwrap());
            ctx.save_word_at_curr_inst(program);
        }
    }
}

/// ```
/// BlockArgRef {
///     index: usize, // the index of basic block arguments
/// }
/// ```
impl GenerateAsm for BlockArgRef {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) {
        let curr_inst = ctx.curr_inst_mut().unwrap();
        ctx.load_to_register(program, curr_inst);
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
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) {
        ctx.load_to_register(program, self.cond());
        let true_args_and_params = self
            .t_args()
            .iter()
            .zip(ctx.bb_params(self.t_target(), program));
        let false_args_and_params = self
            .f_args()
            .iter()
            .zip(ctx.bb_params(self.f_target(), program));
        true_args_and_params
            .chain(false_args_and_params)
            .for_each(|(&arg, &param)| {
                ctx.load_to_register(program, arg);
                ctx.save_word_at_inst(program, param);
            });
        ctx.if_jump(self.t_target(), self.f_target(), program);
    }
}

///```
/// Jump {
///     target: BasicBlock,
///     args: Vec<Inst>,
/// }
///```
impl GenerateAsm for Jump {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) {
        let args_and_params = self
            .args()
            .iter()
            .zip(ctx.bb_params(self.target(), program));
        args_and_params.for_each(|(&arg, &param)| {
            ctx.load_to_register(program, arg);
            ctx.save_word_at_inst(program, param);
        });
        ctx.jump(self.target(), program);
    }
}

impl GenerateAsm for Return {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) {
        if let Some(ret_val) = self.value() {
            ctx.load_to_register(program, ret_val);
            let ret_sz = Bit::try_from(ctx.curr_func_data(program).ret_ty().size()).unwrap();
            ctx.ret(ret_sz);
        } else {
            ctx.void_ret();
        }
    }
}

impl GenerateAsm for Binary {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) {
        ctx.load_to_register(program, self.lhs());
        ctx.load_to_register(program, self.rhs());
        ctx.binary_op(self.op());
        ctx.save_word_at_curr_inst(program);
    }
}

/// ```
/// Load {
///    src: Inst // alloc
/// }
/// ```
/// Get 1 register
impl GenerateAsm for Load {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) {
        let global_flag = self.src().is_global();
        let ptr_flag = if global_flag {
            false
        } else {
            is_ptr(self.src(), ctx.curr_func_data(program))
        };

        let size = ctx.value_bit(program, ctx.curr_inst().unwrap());

        if global_flag {
            ctx.load_address(get_glob_var_name(self.src(), program));
            ctx.load_from_address(size);
            ctx.save_word_at_curr_inst(program);
        } else if ptr_flag {
            // let offset = ctx.get_inst_offset(self.src()).unwrap() as i32;
            if let Some(offset) = ctx.register_or_offset(program, self.src()) {
                ctx.load_word_sp(offset as _, ctx.value_bit(program, self.src()));
            }
            ctx.load_from_address(size);
            ctx.save_word_at_curr_inst(program);
        } else {
            // let offset = ctx.get_inst_offset(self.src()).unwrap() as i32;
            if let Some(offset) = ctx.register_or_offset(program, self.src()) {
                ctx.load_word_sp(offset as _, size);
            }
            ctx.save_word_at_curr_inst(program);
        }
    }
}

// impl GenerateAsm for Alloc {
//     /// alloc is marker instruction for IR representation, we have already allocate a stack(sp) to
//     /// store the instruction, so it won't have counterpart in RISC-V instruction
//     #[allow(unused)]
//     fn generate(&self, program: &Program, ctx: &mut AsmGenContext) {}
// }

/// ```
/// Store {
///    value: Inst,
///    dest: Inst, // From alloc instruction
/// }
/// ```
impl GenerateAsm for Store {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) {
        if self.dest().is_global() {
            ctx.load_address(get_glob_var_name(self.dest(), program));
            ctx.load_to_register(program, self.src());
            ctx.save_word_at_address();
        } else {
            // FIXME: maybe incorrect use of curr_func_data
            match ctx.curr_func_data(program).inst_data(self.dest()).kind() {
                InstKind::GetElemPtr(..) | InstKind::GetPtr(..) => {
                    ctx.load_to_register(program, self.dest());
                    ctx.load_to_register(program, self.src());
                    ctx.save_word_at_address();
                }
                _ => {
                    // store the value where it's located.
                    ctx.load_to_register(program, self.src());
                    ctx.save_word_at_inst(program, self.dest());
                }
            };
        }
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
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext) {
        let curr_inst = ctx.curr_inst().unwrap();
        ctx.load_imm(self.value(), ctx.value_bit(program, curr_inst));
    }
}

#[inline]
fn get_glob_var_name(var: Inst, program: &Program) -> String {
    assert!(var.is_global());
    program.inst_data(var).name().unwrap().to_string()
}

fn ptr_base_elem_size(ty: &Type) -> usize {
    use TypeKind::*;
    let point_to = ty.derefernce();
    match point_to.kind() {
        Array(elem_ty, _len) => elem_ty.size(),
        Int32 => 4,
        _fuck => unreachable!("{point_to:?}"),
    }
}

fn ptr_base_size(ty: &Type) -> usize {
    use TypeKind::*;
    let Pointer(point_to) = ty.kind() else {
        unreachable!();
    };
    point_to.size()
}

#[inline]
fn is_ptr(val: Inst, func_data: &FunctionData) -> bool {
    matches!(
        func_data.inst_data(val).kind(),
        InstKind::GetElemPtr(..) | InstKind::GetPtr(..)
    )
}
