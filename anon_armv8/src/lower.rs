use std::{collections::HashMap, fmt::Write};

use raana_ir::ir::{
    arena::Arena,
    basic_block::BasicBlock,
    inst_kind::{BinaryOp, InstKind},
    Function as HirFunction, Inst as HirInst, InstData, Program, Type as HirType, TypeKind,
};

use crate::abi::Signature;

pub fn compile_program_to_asm(program: &Program) -> Result<String, String> {
    let globals = program
        .global_inst_layout()
        .iter()
        .enumerate()
        .map(|(index, &inst)| (inst, format!(".LG{index}")))
        .collect::<HashMap<_, _>>();
    let mut output = String::new();
    emit_globals(program, &globals, &mut output)?;
    writeln!(output, "    .text").unwrap();
    for &func in program.function_layout() {
        let data = program.func_data(func);
        if !data.layout().is_decl() {
            FunctionLowerer::new(program, func, &globals)?.emit(&mut output)?;
        }
    }
    Ok(output)
}

struct FunctionLowerer<'a> {
    program: &'a Program,
    func: HirFunction,
    name: &'a str,
    slots: HashMap<HirInst, u32>,
    allocs: HashMap<HirInst, u32>,
    globals: &'a HashMap<HirInst, String>,
    block_labels: HashMap<BasicBlock, usize>,
    outgoing_size: u32,
    local_size: u32,
}

impl<'a> FunctionLowerer<'a> {
    fn new(
        program: &'a Program,
        func: HirFunction,
        globals: &'a HashMap<HirInst, String>,
    ) -> Result<Self, String> {
        let data = program.func_data(func);
        let mut slots = HashMap::new();
        let mut allocs = HashMap::new();
        let mut block_labels = HashMap::new();
        let mut local_size = 0;
        let mut outgoing_size = 0;

        for (index, layout) in data.layout().basicblocks().iter().enumerate() {
            block_labels.insert(layout.bb(), index);
        }
        for &param in data.params() {
            allocate_value_slot(&mut slots, param, &mut local_size);
        }
        for layout in data.layout().basicblocks() {
            for &param in data.bb_data(layout.bb()).params() {
                allocate_value_slot(&mut slots, param, &mut local_size);
            }
            for &inst in layout.insts() {
                let inst_data = data.inst_data(inst);
                match inst_data.kind() {
                    InstKind::Alloc => {
                        let size = target_size(&inst_data.ty().derefernce())?;
                        local_size =
                            align_to(local_size, target_align(&inst_data.ty().derefernce())?);
                        allocs.insert(inst, local_size);
                        local_size += size;
                    }
                    _ if !inst_data.ty().is_unit() && !inst_data.is_const() => {
                        allocate_value_slot(&mut slots, inst, &mut local_size);
                    }
                    _ => {}
                }
                if let InstKind::Call(call) = inst_data.kind() {
                    outgoing_size = outgoing_size.max(
                        Signature::new(
                            &call
                                .args()
                                .iter()
                                .map(|arg| {
                                    if arg.is_global() {
                                        program
                                            .global_arena()
                                            .inst_arena()
                                            .data_of(*arg)
                                            .ty()
                                            .clone()
                                    } else {
                                        data.inst_data(*arg).ty().clone()
                                    }
                                })
                                .collect::<Vec<_>>(),
                        )
                        .stack_size,
                    );
                }
                match inst_data.kind() {
                    InstKind::Jump(jump) => {
                        outgoing_size = outgoing_size.max((jump.args().len() as u32) * 8);
                    }
                    InstKind::Branch(branch) => {
                        outgoing_size = outgoing_size
                            .max((branch.t_args().len() as u32) * 8)
                            .max((branch.f_args().len() as u32) * 8);
                    }
                    _ => {}
                }
            }
        }
        local_size = align_to(local_size, 8);
        Ok(Self {
            program,
            func,
            name: data.name(),
            slots,
            allocs,
            globals,
            block_labels,
            outgoing_size,
            local_size,
        })
    }

    fn emit(&self, output: &mut String) -> Result<(), String> {
        let frame_size = align_to(self.outgoing_size + self.local_size, 16);
        writeln!(output, "    .p2align 2").unwrap();
        writeln!(output, "    .globl {}", self.name).unwrap();
        writeln!(output, "    .type {}, %function", self.name).unwrap();
        writeln!(output, "{}:", self.name).unwrap();
        writeln!(output, "    stp x29, x30, [sp, #-16]!").unwrap();
        writeln!(output, "    mov x29, sp").unwrap();
        if frame_size != 0 {
            emit_sp_adjust(output, "sub", frame_size)?;
        }

        let data = self.program.func_data(self.func);
        let signature = Signature::new(
            &data
                .params()
                .iter()
                .map(|param| data.inst_data(*param).ty().clone())
                .collect::<Vec<_>>(),
        );
        for (&param, location) in data.params().iter().zip(signature.args) {
            let offset = self.slot(param)?;
            match location {
                crate::abi::ValueLocation::Reg(reg) => {
                    let number = reg.to_physical_reg().unwrap().hw_enc();
                    if data.inst_data(param).ty().is_f32() {
                        emit_memory_store(
                            output,
                            &format!("s{number}"),
                            "sp",
                            self.local_base() + offset,
                        )?;
                    } else if data.inst_data(param).ty().is_pointer() {
                        emit_memory_store(
                            output,
                            &format!("x{number}"),
                            "sp",
                            self.local_base() + offset,
                        )?;
                    } else {
                        emit_memory_store(
                            output,
                            &format!("w{number}"),
                            "sp",
                            self.local_base() + offset,
                        )?;
                    }
                }
                crate::abi::ValueLocation::Stack { offset: incoming } => {
                    if data.inst_data(param).ty().is_f32() {
                        writeln!(output, "    ldr s9, [x29, #{}]", 16 + incoming).unwrap();
                        emit_memory_store(output, "s9", "sp", self.local_base() + offset)?;
                    } else if data.inst_data(param).ty().is_pointer() {
                        writeln!(output, "    ldr x9, [x29, #{}]", 16 + incoming).unwrap();
                        emit_memory_store(output, "x9", "sp", self.local_base() + offset)?;
                    } else {
                        writeln!(output, "    ldr w9, [x29, #{}]", 16 + incoming).unwrap();
                        emit_memory_store(output, "w9", "sp", self.local_base() + offset)?;
                    }
                }
            }
        }

        for layout in data.layout().basicblocks() {
            writeln!(
                output,
                ".L{}_bb{}:",
                self.name,
                self.block_label(layout.bb())?
            )
            .unwrap();
            for &inst in layout.insts() {
                self.emit_inst(output, inst)?;
            }
        }
        writeln!(output, ".L{}_epilogue:", self.name).unwrap();
        writeln!(output, "    mov sp, x29").unwrap();
        writeln!(output, "    ldp x29, x30, [sp], #16").unwrap();
        writeln!(output, "    ret").unwrap();
        writeln!(output, "    .size {}, .-{}", self.name, self.name).unwrap();
        Ok(())
    }

    fn emit_inst(&self, output: &mut String, inst: HirInst) -> Result<(), String> {
        let data = self.program.func_data(self.func);
        let inst_data = data.inst_data(inst);
        match inst_data.kind() {
            InstKind::Integer(_) | InstKind::FuncArgRef(_) | InstKind::BlockArgRef(_) => Ok(()),
            InstKind::ZeroInit if inst_data.ty().is_i32() => Ok(()),
            InstKind::Binary(binary)
                if self.inst_data(binary.lhs()).ty().is_f32() && binary.op().is_compare() =>
            {
                self.float_value_into(output, binary.lhs(), "s9")?;
                self.float_value_into(output, binary.rhs(), "s10")?;
                writeln!(output, "    fcmp s9, s10").unwrap();
                writeln!(output, "    cset w9, {}", cond_for(binary.op())?).unwrap();
                // AArch64 condition codes alone treat some unordered comparisons
                // as true. SysY's LLVM lowering uses ordered floating predicates.
                writeln!(output, "    cset w10, vc").unwrap();
                writeln!(output, "    and w9, w9, w10").unwrap();
                self.store_value(output, inst, "w9")
            }
            InstKind::Binary(binary) if self.inst_data(binary.lhs()).ty().is_f32() => {
                self.float_value_into(output, binary.lhs(), "s9")?;
                self.float_value_into(output, binary.rhs(), "s10")?;
                let opcode = match binary.op() {
                    BinaryOp::Add => "fadd",
                    BinaryOp::Sub => "fsub",
                    BinaryOp::Mul => "fmul",
                    BinaryOp::Div => "fdiv",
                    _ => {
                        return Err(format!(
                            "{}: unsupported float binary operation {:?}",
                            self.name,
                            binary.op()
                        ))
                    }
                };
                writeln!(output, "    {opcode} s9, s9, s10").unwrap();
                self.store_float(output, inst, "s9")
            }
            InstKind::Binary(binary) if inst_data.ty().is_i32() => {
                self.value_into(output, binary.lhs(), "w9")?;
                self.value_into(output, binary.rhs(), "w10")?;
                match binary.op() {
                    BinaryOp::Add => writeln!(output, "    add w9, w9, w10").unwrap(),
                    BinaryOp::Sub => writeln!(output, "    sub w9, w9, w10").unwrap(),
                    BinaryOp::Mul => writeln!(output, "    mul w9, w9, w10").unwrap(),
                    BinaryOp::Div => writeln!(output, "    sdiv w9, w9, w10").unwrap(),
                    BinaryOp::Rem => {
                        writeln!(output, "    sdiv w11, w9, w10").unwrap();
                        writeln!(output, "    msub w9, w11, w10, w9").unwrap();
                    }
                    BinaryOp::And => writeln!(output, "    and w9, w9, w10").unwrap(),
                    BinaryOp::Or => writeln!(output, "    orr w9, w9, w10").unwrap(),
                    BinaryOp::Xor => writeln!(output, "    eor w9, w9, w10").unwrap(),
                    BinaryOp::Shl => writeln!(output, "    lsl w9, w9, w10").unwrap(),
                    BinaryOp::Shr => writeln!(output, "    lsr w9, w9, w10").unwrap(),
                    BinaryOp::Sar => writeln!(output, "    asr w9, w9, w10").unwrap(),
                    op if op.is_compare() => {
                        writeln!(output, "    cmp w9, w10").unwrap();
                        writeln!(output, "    cset w9, {}", cond_for(op)?).unwrap();
                    }
                    _ => {
                        return Err(format!(
                            "unsupported integer binary operation: {:?}",
                            binary.op()
                        ))
                    }
                }
                self.store_value(output, inst, "w9")
            }
            InstKind::Jump(jump) => {
                self.copy_block_args(output, jump.target(), jump.args())?;
                writeln!(
                    output,
                    "    b .L{}_bb{}",
                    self.name,
                    self.block_label(jump.target())?
                )
                .unwrap();
                Ok(())
            }
            InstKind::Branch(branch) => {
                let cond_ty = self.inst_data(branch.cond()).ty();
                let true_copy = format!(
                    ".L{}_branch_true_{}",
                    self.name,
                    self.block_label(self.parent_block(inst)?)?
                );
                if cond_ty.is_f32() {
                    self.float_value_into(output, branch.cond(), "s9")?;
                    writeln!(output, "    fcmp s9, #0.0").unwrap();
                    writeln!(output, "    b.ne {true_copy}").unwrap();
                } else {
                    self.value_into(output, branch.cond(), "w9")?;
                    writeln!(output, "    cbnz w9, {true_copy}").unwrap();
                }
                self.copy_block_args(output, branch.f_target(), branch.f_args())?;
                writeln!(
                    output,
                    "    b .L{}_bb{}",
                    self.name,
                    self.block_label(branch.f_target())?
                )
                .unwrap();
                writeln!(output, "{true_copy}:").unwrap();
                self.copy_block_args(output, branch.t_target(), branch.t_args())?;
                writeln!(
                    output,
                    "    b .L{}_bb{}",
                    self.name,
                    self.block_label(branch.t_target())?
                )
                .unwrap();
                Ok(())
            }
            InstKind::Return(ret) => {
                if let Some(value) = ret.value() {
                    if self.inst_data(value).ty().is_f32() {
                        self.float_value_into(output, value, "s0")?;
                    } else if self.inst_data(value).ty().is_pointer() {
                        self.address_into(output, value, "x0")?;
                    } else {
                        self.value_into(output, value, "w0")?;
                    }
                }
                writeln!(output, "    b .L{}_epilogue", self.name).unwrap();
                Ok(())
            }
            InstKind::Call(call)
                if inst_data.ty().is_i32()
                    || inst_data.ty().is_f32()
                    || inst_data.ty().is_pointer()
                    || inst_data.ty().is_unit() =>
            {
                let signature = Signature::new(
                    &call
                        .args()
                        .iter()
                        .map(|arg| self.inst_data(*arg).ty().clone())
                        .collect::<Vec<_>>(),
                );
                for (&arg, location) in call.args().iter().zip(signature.args) {
                    match location {
                        crate::abi::ValueLocation::Reg(reg) => {
                            let number = reg.to_physical_reg().unwrap().hw_enc();
                            if self.inst_data(arg).ty().is_f32() {
                                self.float_value_into(output, arg, &format!("s{number}"))?;
                            } else if self.inst_data(arg).ty().is_pointer() {
                                self.address_into(output, arg, &format!("x{number}"))?;
                            } else {
                                self.value_into(output, arg, &format!("w{number}"))?;
                            }
                        }
                        crate::abi::ValueLocation::Stack { offset } => {
                            if self.inst_data(arg).ty().is_f32() {
                                self.float_value_into(output, arg, "s9")?;
                                writeln!(output, "    str s9, [sp, #{offset}]").unwrap();
                            } else if self.inst_data(arg).ty().is_pointer() {
                                self.address_into(output, arg, "x9")?;
                                writeln!(output, "    str x9, [sp, #{offset}]").unwrap();
                            } else {
                                self.value_into(output, arg, "w9")?;
                                writeln!(output, "    str w9, [sp, #{offset}]").unwrap();
                            }
                        }
                    }
                }
                let callee = self.program.func_data(call.callee()).name();
                writeln!(output, "    bl {callee}").unwrap();
                if inst_data.ty().is_f32() {
                    self.store_float(output, inst, "s0")?;
                } else if inst_data.ty().is_pointer() {
                    self.store_pointer(output, inst, "x0")?;
                } else if !inst_data.ty().is_unit() {
                    self.store_value(output, inst, "w0")?;
                }
                Ok(())
            }
            InstKind::Alloc => Ok(()),
            InstKind::Cast(cast) if inst_data.ty().is_f32() => {
                self.value_into(output, cast.src(), "w9")?;
                writeln!(output, "    scvtf s9, w9").unwrap();
                self.store_float(output, inst, "s9")
            }
            InstKind::Cast(cast) if inst_data.ty().is_i32() => {
                self.float_value_into(output, cast.src(), "s9")?;
                writeln!(output, "    fcvtzs w9, s9").unwrap();
                self.store_value(output, inst, "w9")
            }
            InstKind::GetElemPtr(gep) => {
                self.address_into(output, gep.base(), "x10")?;
                let mut current = self.inst_data(gep.base()).ty().clone();
                for &offset in gep.offsets() {
                    let (next, stride) = gep_step(&current)?;
                    self.value_into(output, offset, "w9")?;
                    if stride.is_power_of_two() && stride.trailing_zeros() <= 4 {
                        writeln!(
                            output,
                            "    add x10, x10, w9, sxtw #{}",
                            stride.trailing_zeros()
                        )
                        .unwrap();
                    } else {
                        emit_i32(output, "w11", stride as i32)?;
                        writeln!(output, "    smaddl x10, w9, w11, x10").unwrap();
                    }
                    current = next;
                }
                self.store_pointer(output, inst, "x10")
            }
            InstKind::Store(store) => {
                self.address_into(output, store.dest(), "x10")?;
                self.store_initializer(output, store.src(), self.inst_data(store.src()).ty(), 0)
            }
            InstKind::Load(load) if inst_data.ty().is_i32() => {
                self.address_into(output, load.src(), "x10")?;
                writeln!(output, "    ldr w9, [x10]").unwrap();
                self.store_value(output, inst, "w9")
            }
            InstKind::Load(load) if inst_data.ty().is_f32() => {
                self.address_into(output, load.src(), "x10")?;
                writeln!(output, "    ldr s9, [x10]").unwrap();
                self.store_float(output, inst, "s9")
            }
            InstKind::Load(load) if inst_data.ty().is_pointer() => {
                self.address_into(output, load.src(), "x10")?;
                writeln!(output, "    ldr x9, [x10]").unwrap();
                self.store_pointer(output, inst, "x9")
            }
            _ => Err(format!(
                "{}: unsupported instruction {:?}",
                self.name,
                inst_data.kind()
            )),
        }
    }

    fn value_into(&self, output: &mut String, inst: HirInst, reg: &str) -> Result<(), String> {
        let data = self.inst_data(inst);
        match data.kind() {
            InstKind::Integer(value) => emit_i32(output, reg, value.value()),
            InstKind::ZeroInit if data.ty().is_i32() => {
                writeln!(output, "    mov {reg}, wzr").unwrap();
                Ok(())
            }
            _ if data.ty().is_i32() => {
                emit_memory_load(output, reg, "sp", self.local_base() + self.slot(inst)?)?;
                Ok(())
            }
            _ => Err(format!("{}: expected i32 value for {}", self.name, inst)),
        }
    }

    fn float_value_into(
        &self,
        output: &mut String,
        inst: HirInst,
        reg: &str,
    ) -> Result<(), String> {
        let data = self.inst_data(inst);
        match data.kind() {
            InstKind::Float(value) => {
                emit_i32(output, "w11", value.value().to_bits() as i32)?;
                writeln!(output, "    fmov {reg}, w11").unwrap();
                Ok(())
            }
            InstKind::ZeroInit if data.ty().is_f32() => {
                writeln!(output, "    fmov {reg}, wzr").unwrap();
                Ok(())
            }
            _ if data.ty().is_f32() => {
                emit_memory_load(output, reg, "sp", self.local_base() + self.slot(inst)?)?;
                Ok(())
            }
            _ => Err(format!("{}: expected f32 value for {}", self.name, inst)),
        }
    }

    fn address_into(&self, output: &mut String, inst: HirInst, reg: &str) -> Result<(), String> {
        if let Some(offset) = self.allocs.get(&inst) {
            emit_sp_address(output, reg, self.local_base() + offset)?;
            Ok(())
        } else if let Some(symbol) = self.globals.get(&inst) {
            writeln!(output, "    adrp {reg}, {symbol}").unwrap();
            writeln!(output, "    add {reg}, {reg}, :lo12:{symbol}").unwrap();
            Ok(())
        } else if self.inst_data(inst).ty().is_pointer() {
            emit_memory_load(output, reg, "sp", self.local_base() + self.slot(inst)?)?;
            Ok(())
        } else {
            Err(format!("{}: unsupported pointer value {}", self.name, inst))
        }
    }

    fn copy_block_args(
        &self,
        output: &mut String,
        target: BasicBlock,
        args: &[HirInst],
    ) -> Result<(), String> {
        let params = self.program.func_data(self.func).bb_data(target).params();
        // Stage every edge argument before overwriting destination block
        // parameters so a parallel-copy cycle cannot corrupt a source value.
        for (index, &arg) in args.iter().enumerate() {
            let ty = self.inst_data(arg).ty();
            if ty.is_f32() {
                self.float_value_into(output, arg, "s9")?;
                writeln!(output, "    str s9, [sp, #{}]", index * 8).unwrap();
            } else if ty.is_pointer() {
                self.address_into(output, arg, "x9")?;
                writeln!(output, "    str x9, [sp, #{}]", index * 8).unwrap();
            } else {
                self.value_into(output, arg, "w9")?;
                writeln!(output, "    str w9, [sp, #{}]", index * 8).unwrap();
            }
        }
        for (index, &param) in params.iter().enumerate() {
            let ty = self.inst_data(param).ty();
            if ty.is_f32() {
                writeln!(output, "    ldr s9, [sp, #{}]", index * 8).unwrap();
                self.store_float(output, param, "s9")?;
            } else if ty.is_pointer() {
                writeln!(output, "    ldr x9, [sp, #{}]", index * 8).unwrap();
                self.store_pointer(output, param, "x9")?;
            } else {
                writeln!(output, "    ldr w9, [sp, #{}]", index * 8).unwrap();
                self.store_value(output, param, "w9")?;
            }
        }
        Ok(())
    }

    fn store_value(&self, output: &mut String, inst: HirInst, reg: &str) -> Result<(), String> {
        emit_memory_store(output, reg, "sp", self.local_base() + self.slot(inst)?)
    }

    fn store_pointer(&self, output: &mut String, inst: HirInst, reg: &str) -> Result<(), String> {
        emit_memory_store(output, reg, "sp", self.local_base() + self.slot(inst)?)
    }

    fn store_float(&self, output: &mut String, inst: HirInst, reg: &str) -> Result<(), String> {
        emit_memory_store(output, reg, "sp", self.local_base() + self.slot(inst)?)
    }

    fn store_initializer(
        &self,
        output: &mut String,
        value: HirInst,
        ty: &HirType,
        offset: u32,
    ) -> Result<(), String> {
        let data = self.inst_data(value);
        match data.kind() {
            InstKind::Aggregate(aggregate) => {
                let TypeKind::Array(element, _) = ty.kind() else {
                    return Err(format!(
                        "{}: aggregate value has non-array type {ty}",
                        self.name
                    ));
                };
                let stride = target_size(element)?;
                for (index, &element_value) in aggregate.value().iter().enumerate() {
                    self.store_initializer(
                        output,
                        element_value,
                        element,
                        offset + (index as u32) * stride,
                    )?;
                }
                Ok(())
            }
            InstKind::ZeroInit => self.zero_memory(output, ty, offset),
            _ if ty.is_i32() => {
                self.value_into(output, value, "w9")?;
                if offset == 0 {
                    writeln!(output, "    str w9, [x10]").unwrap();
                } else {
                    writeln!(output, "    str w9, [x10, #{offset}]").unwrap();
                }
                Ok(())
            }
            _ if ty.is_f32() => {
                self.float_value_into(output, value, "s9")?;
                if offset == 0 {
                    writeln!(output, "    str s9, [x10]").unwrap();
                } else {
                    writeln!(output, "    str s9, [x10, #{offset}]").unwrap();
                }
                Ok(())
            }
            _ if ty.is_pointer() => {
                self.address_into(output, value, "x9")?;
                if offset == 0 {
                    writeln!(output, "    str x9, [x10]").unwrap();
                } else {
                    writeln!(output, "    str x9, [x10, #{offset}]").unwrap();
                }
                Ok(())
            }
            _ => Err(format!(
                "{}: unsupported aggregate element type {ty}",
                self.name
            )),
        }
    }

    fn zero_memory(&self, output: &mut String, ty: &HirType, offset: u32) -> Result<(), String> {
        let size = target_size(ty)?;
        writeln!(output, "    mov w9, wzr").unwrap();
        for byte_offset in (0..size).step_by(4) {
            let at = offset + byte_offset;
            if at == 0 {
                writeln!(output, "    str w9, [x10]").unwrap();
            } else {
                writeln!(output, "    str w9, [x10, #{at}]").unwrap();
            }
        }
        Ok(())
    }

    fn slot(&self, inst: HirInst) -> Result<u32, String> {
        self.slots
            .get(&inst)
            .copied()
            .ok_or_else(|| format!("{}: no scalar slot for {}", self.name, inst))
    }

    fn inst_data(&self, inst: HirInst) -> &InstData {
        if inst.is_global() {
            self.program.global_arena().inst_arena().data_of(inst)
        } else {
            self.program.func_data(self.func).inst_data(inst)
        }
    }

    fn block_label(&self, block: BasicBlock) -> Result<usize, String> {
        self.block_labels
            .get(&block)
            .copied()
            .ok_or_else(|| format!("{}: unknown block", self.name))
    }

    fn parent_block(&self, inst: HirInst) -> Result<BasicBlock, String> {
        self.program
            .func_data(self.func)
            .layout()
            .parent_bb(inst)
            .ok_or_else(|| format!("{}: instruction has no parent block", self.name))
    }

    fn local_base(&self) -> u32 {
        self.outgoing_size
    }
}

fn emit_globals(
    program: &Program,
    globals: &HashMap<HirInst, String>,
    output: &mut String,
) -> Result<(), String> {
    if globals.is_empty() {
        return Ok(());
    }
    writeln!(output, "    .data").unwrap();
    for &inst in program.global_inst_layout() {
        let symbol = &globals[&inst];
        let data = program.global_arena().inst_arena().data_of(inst);
        let InstKind::GlobalAlloc(global) = data.kind() else {
            return Err("global layout contains a non-global allocation".into());
        };
        writeln!(
            output,
            "    .p2align {}",
            target_align(&data.ty().derefernce())?.trailing_zeros()
        )
        .unwrap();
        writeln!(output, "{symbol}:").unwrap();
        emit_global_initializer(program, global.init(), output)?;
    }
    Ok(())
}

fn emit_global_initializer(
    program: &Program,
    inst: HirInst,
    output: &mut String,
) -> Result<(), String> {
    let data = program.global_arena().inst_arena().data_of(inst);
    match data.kind() {
        InstKind::Integer(value) => writeln!(output, "    .word {}", value.value()).unwrap(),
        InstKind::Float(value) => {
            writeln!(output, "    .word {}", value.value().to_bits()).unwrap()
        }
        InstKind::ZeroInit => writeln!(output, "    .zero {}", target_size(data.ty())?).unwrap(),
        InstKind::Aggregate(aggregate) => {
            for &value in aggregate.value() {
                emit_global_initializer(program, value, output)?;
            }
        }
        _ => return Err(format!("unsupported global initializer: {:?}", data.kind())),
    }
    Ok(())
}

fn gep_step(ty: &HirType) -> Result<(HirType, u32), String> {
    match ty.kind() {
        TypeKind::Pointer(pointee) => Ok((pointee.clone(), target_size(pointee)?)),
        TypeKind::Array(element, _) => Ok((element.clone(), target_size(element)?)),
        _ => Err(format!("GEP on non-address type: {ty}")),
    }
}

fn allocate_value_slot(slots: &mut HashMap<HirInst, u32>, inst: HirInst, size: &mut u32) {
    *size = align_to(*size, 8);
    slots.insert(inst, *size);
    *size += 8;
}

fn target_size(ty: &HirType) -> Result<u32, String> {
    match ty.kind() {
        TypeKind::Int32 | TypeKind::Float32 => Ok(4),
        TypeKind::Pointer(_) | TypeKind::String => Ok(8),
        TypeKind::Array(base, len) => target_size(base).map(|size| size * *len as u32),
        _ => Err(format!("unsupported AArch64 object type: {ty}")),
    }
}

fn target_align(ty: &HirType) -> Result<u32, String> {
    match ty.kind() {
        TypeKind::Int32 | TypeKind::Float32 => Ok(4),
        TypeKind::Pointer(_) | TypeKind::String => Ok(8),
        TypeKind::Array(base, _) => target_align(base),
        _ => Err(format!("unsupported AArch64 object type: {ty}")),
    }
}

fn align_to(value: u32, align: u32) -> u32 {
    (value + align - 1) & !(align - 1)
}

fn cond_for(op: BinaryOp) -> Result<&'static str, String> {
    match op {
        BinaryOp::Eq => Ok("eq"),
        BinaryOp::NotEq => Ok("ne"),
        BinaryOp::Gt => Ok("gt"),
        BinaryOp::Lt => Ok("lt"),
        BinaryOp::Ge => Ok("ge"),
        BinaryOp::Le => Ok("le"),
        _ => Err(format!("not a comparison: {op:?}")),
    }
}

fn emit_i32(output: &mut String, reg: &str, value: i32) -> Result<(), String> {
    let bits = value as u32;
    let lo = bits & 0xffff;
    let hi = bits >> 16;
    writeln!(output, "    movz {reg}, #{lo}").unwrap();
    if hi != 0 {
        writeln!(output, "    movk {reg}, #{hi}, lsl #16").unwrap();
    }
    Ok(())
}

fn emit_sp_adjust(output: &mut String, op: &str, amount: u32) -> Result<(), String> {
    let mut remaining = amount;
    while remaining != 0 {
        let chunk = remaining.min(4095);
        writeln!(output, "    {op} sp, sp, #{chunk}").unwrap();
        remaining -= chunk;
    }
    Ok(())
}

fn emit_sp_address(output: &mut String, reg: &str, offset: u32) -> Result<(), String> {
    writeln!(output, "    mov {reg}, sp").unwrap();
    let mut remaining = offset;
    while remaining != 0 {
        let chunk = remaining.min(4095);
        writeln!(output, "    add {reg}, {reg}, #{chunk}").unwrap();
        remaining -= chunk;
    }
    Ok(())
}

fn emit_memory_load(output: &mut String, reg: &str, base: &str, offset: u32) -> Result<(), String> {
    emit_memory_access(output, "ldr", reg, base, offset)
}

fn emit_memory_store(
    output: &mut String,
    reg: &str,
    base: &str,
    offset: u32,
) -> Result<(), String> {
    emit_memory_access(output, "str", reg, base, offset)
}

fn emit_memory_access(
    output: &mut String,
    op: &str,
    reg: &str,
    base: &str,
    offset: u32,
) -> Result<(), String> {
    let scale = if reg.starts_with('x') { 8 } else { 4 };
    if offset % scale == 0 && offset / scale <= 4095 {
        writeln!(output, "    {op} {reg}, [{base}, #{offset}]").unwrap();
        return Ok(());
    }
    emit_address_offset(output, "x16", base, offset)?;
    writeln!(output, "    {op} {reg}, [x16]").unwrap();
    Ok(())
}

fn emit_address_offset(
    output: &mut String,
    destination: &str,
    base: &str,
    offset: u32,
) -> Result<(), String> {
    writeln!(output, "    mov {destination}, {base}").unwrap();
    let mut remaining = offset;
    while remaining != 0 {
        let chunk = remaining.min(4095);
        writeln!(output, "    add {destination}, {destination}, #{chunk}").unwrap();
        remaining -= chunk;
    }
    Ok(())
}
