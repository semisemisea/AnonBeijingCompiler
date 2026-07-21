use std::{collections::HashMap, fmt::Write};

use raana_ir::ir::{
    arena::Arena,
    basic_block::BasicBlock,
    inst_kind::{BinaryOp, InstKind},
    Function as HirFunction, Inst as HirInst, Program, Type as HirType, TypeKind,
};

use crate::abi::Signature;

pub fn compile_program_to_asm(program: &Program) -> Result<String, String> {
    let mut output = String::from("    .text\n");
    for &func in program.function_layout() {
        let data = program.func_data(func);
        if !data.layout().is_decl() {
            FunctionLowerer::new(program, func)?.emit(&mut output)?;
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
    block_labels: HashMap<BasicBlock, usize>,
    outgoing_size: u32,
    local_size: u32,
}

impl<'a> FunctionLowerer<'a> {
    fn new(program: &'a Program, func: HirFunction) -> Result<Self, String> {
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
                                .map(|arg| data.inst_data(*arg).ty().clone())
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
                    writeln!(
                        output,
                        "    str w{}, [sp, #{}]",
                        reg.to_physical_reg().unwrap().hw_enc(),
                        self.local_base() + offset
                    )
                    .unwrap();
                }
                crate::abi::ValueLocation::Stack { offset: incoming } => {
                    writeln!(output, "    ldr w9, [x29, #{}]", 16 + incoming).unwrap();
                    writeln!(output, "    str w9, [sp, #{}]", self.local_base() + offset).unwrap();
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
                self.value_into(output, branch.cond(), "w9")?;
                let true_copy = format!(
                    ".L{}_branch_true_{}",
                    self.name,
                    self.block_label(self.parent_block(inst)?)?
                );
                writeln!(output, "    cbnz w9, {true_copy}").unwrap();
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
                    self.value_into(output, value, "w0")?;
                }
                writeln!(output, "    b .L{}_epilogue", self.name).unwrap();
                Ok(())
            }
            InstKind::Call(call) if inst_data.ty().is_i32() || inst_data.ty().is_unit() => {
                let signature = Signature::new(
                    &call
                        .args()
                        .iter()
                        .map(|arg| data.inst_data(*arg).ty().clone())
                        .collect::<Vec<_>>(),
                );
                for (&arg, location) in call.args().iter().zip(signature.args) {
                    match location {
                        crate::abi::ValueLocation::Reg(reg) => {
                            self.value_into(
                                output,
                                arg,
                                &format!("w{}", reg.to_physical_reg().unwrap().hw_enc()),
                            )?;
                        }
                        crate::abi::ValueLocation::Stack { offset } => {
                            self.value_into(output, arg, "w9")?;
                            writeln!(output, "    str w9, [sp, #{offset}]").unwrap();
                        }
                    }
                }
                let callee = self.program.func_data(call.callee()).name();
                writeln!(output, "    bl {callee}").unwrap();
                if !inst_data.ty().is_unit() {
                    self.store_value(output, inst, "w0")?;
                }
                Ok(())
            }
            InstKind::Alloc => Ok(()),
            InstKind::Store(store) if data.inst_data(store.src()).ty().is_i32() => {
                self.value_into(output, store.src(), "w9")?;
                self.address_into(output, store.dest(), "x10")?;
                writeln!(output, "    str w9, [x10]").unwrap();
                Ok(())
            }
            InstKind::Load(load) if inst_data.ty().is_i32() => {
                self.address_into(output, load.src(), "x10")?;
                writeln!(output, "    ldr w9, [x10]").unwrap();
                self.store_value(output, inst, "w9")
            }
            _ => Err(format!(
                "{}: unsupported instruction {:?}",
                self.name,
                inst_data.kind()
            )),
        }
    }

    fn value_into(&self, output: &mut String, inst: HirInst, reg: &str) -> Result<(), String> {
        let data = self.program.func_data(self.func).inst_data(inst);
        match data.kind() {
            InstKind::Integer(value) => emit_i32(output, reg, value.value()),
            InstKind::ZeroInit if data.ty().is_i32() => {
                writeln!(output, "    mov {reg}, wzr").unwrap();
                Ok(())
            }
            _ if data.ty().is_i32() => {
                writeln!(
                    output,
                    "    ldr {reg}, [sp, #{}]",
                    self.local_base() + self.slot(inst)?
                )
                .unwrap();
                Ok(())
            }
            _ => Err(format!("{}: expected i32 value for {}", self.name, inst)),
        }
    }

    fn address_into(&self, output: &mut String, inst: HirInst, reg: &str) -> Result<(), String> {
        if let Some(offset) = self.allocs.get(&inst) {
            writeln!(output, "    add {reg}, sp, #{}", self.local_base() + offset).unwrap();
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
            self.value_into(output, arg, "w9")?;
            writeln!(output, "    str w9, [sp, #{}]", index * 8).unwrap();
        }
        for (index, &param) in params.iter().enumerate() {
            writeln!(output, "    ldr w9, [sp, #{}]", index * 8).unwrap();
            self.store_value(output, param, "w9")?;
        }
        Ok(())
    }

    fn store_value(&self, output: &mut String, inst: HirInst, reg: &str) -> Result<(), String> {
        writeln!(
            output,
            "    str {reg}, [sp, #{}]",
            self.local_base() + self.slot(inst)?
        )
        .unwrap();
        Ok(())
    }

    fn slot(&self, inst: HirInst) -> Result<u32, String> {
        self.slots
            .get(&inst)
            .copied()
            .ok_or_else(|| format!("{}: no scalar slot for {}", self.name, inst))
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
    if amount > 4095 {
        return Err(format!(
            "stack frame exceeds initial AArch64 immediate range: {amount}"
        ));
    }
    writeln!(output, "    {op} sp, sp, #{amount}").unwrap();
    Ok(())
}
