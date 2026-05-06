use std::collections::{HashMap, HashSet};

use itertools::Itertools;
use raana_ir::ir::{
    Aggregate, BasicBlock, BinaryOp, Function, FunctionData, Inst as IrInst, InstKind, Program,
    Type, arena::Arena,
};

use crate::backend::armv8::{
    codegen::{
        epilogue::Epilogue,
        generate_asm::GenerateAsm,
        register_alloc::{AllocationState, RegisterAllocation},
        register_manager::RegisterManager,
    },
    inst::{
        AddSubImm, AddSubOperand, CsetCondition, Inst, LoadSaveOffset, LogicOperand, MovOperand,
        MoveWideImm, MoveWideImmShift, ShiftSize,
    },
    register::{Bit, IReg, IntRegister, Register},
};

type List = Vec<Inst>;

pub struct AsmGenContext {
    // buffer: asm text.
    // buf: String,
    inst_list: List,
    indent_level: usize,
    func_stack: Vec<Function>,
    stack_slots: HashMap<IrInst, usize>,
    reg_pool: RegisterManager,
    curr_inst: Option<IrInst>,
    epilogue_stack: Vec<Epilogue>,
    allocation: RegisterAllocation,
}

// macro_rules! push {
//     ($list: expr, $item: expr) => {
//         let len = $list.len();
//         $list.push_back(len, $item).unwrap();
//     };
// }

macro_rules! import_reg_and_inst {
    () => {
        #[allow(unused)]
        use Inst::*;
        #[allow(unused)]
        use Register::*;
    };
}
const SHIFT_WIDTH: usize = 2;

impl AsmGenContext {
    pub fn new() -> AsmGenContext {
        AsmGenContext {
            inst_list: List::new(),
            indent_level: 0,
            func_stack: Vec::new(),
            stack_slots: HashMap::new(),
            reg_pool: RegisterManager::new(),
            curr_inst: None,
            epilogue_stack: Vec::new(),
            allocation: RegisterAllocation::new(),
        }
    }

    pub fn bb_params<'a>(&self, bb: BasicBlock, program: &'a Program) -> &'a [IrInst] {
        self.curr_func_data(program).bb_data(bb).params()
    }

    pub fn get_bb_name(&self, bb: BasicBlock, program: &Program) -> String {
        let curr_func = self.curr_func_data(program);
        let func_name = curr_func.name().strip_prefix("@").unwrap();
        let bb_name = curr_func.bb_data(bb).name().strip_prefix("%").unwrap();
        format!(".L_{}_{}", func_name, bb_name)
    }

    pub fn insert_inst(&mut self, val: IrInst, offset: usize) {
        self.stack_slots.insert(val, offset);
    }

    // pub fn stack_slots_debug(&self, func: &FunctionData) {
    //     for (&k, &v) in self.stack_slots.iter() {
    //         let kind = func.dfg().value(k);
    //         eprintln!("{:?} {} {}", k, kind.ty(), v);
    //     }
    // }

    pub fn get_inst_offset(&self, val: IrInst) -> Option<usize> {
        self.stack_slots.get(&val).copied()
    }

    pub fn register_or_offset(&mut self, val: IrInst) -> Option<usize> {
        match self.allocation.get(&val).unwrap() {
            AllocationState::Register(register) => {
                self.reg_pool.push_register(*register);
                None
            }
            AllocationState::Stack(offset) => Some(*offset),
        }
    }

    // pub fn get_val_storage(&self, val: Inst) -> Option<AllocationState> {
    //     todo!()
    // }

    pub fn generate(mut self, program: &Program) -> List {
        // Target platform is 32bit.
        // So before actual generation we set the size of ptr.
        // @Dustin-Jiang: Seems to be deprecated in raana::ir
        // Type::set_ptr_size(4);

        self.incr_indent();
        self.writeln(".data");
        for &glob_inst in program.global_inst_layout() {
            let glob_inst_data = program.inst_data(glob_inst);
            let name = glob_inst_data
                .name()
                .clone()
                .unwrap()
                .strip_prefix('%')
                .unwrap()
                .to_string();
            self.writeln(&format!(".globl {name}",));

            self.decr_indent();
            self.writeln(&format!("{name}:"));
            self.incr_indent();

            let InstKind::GlobalAlloc(glob_alloc) = glob_inst_data.kind() else {
                unreachable!();
            };
            let init_val = glob_alloc.init();
            let init_val_data = program.inst_data(init_val);
            match init_val_data.kind() {
                InstKind::ZeroInit => {
                    self.writeln(&format!(".zero {}", init_val_data.ty().size()));
                }
                InstKind::Integer(int) => {
                    self.writeln(&format!(".word {}", int.value()));
                }
                InstKind::Aggregate(agg) => {
                    use Aggregate;

                    // Create a recursive function to handle
                    fn recursive(agg: &Aggregate, ctx: &mut AsmGenContext, program: &Program) {
                        for &elem in agg.value() {
                            let elem_data = program.inst_data(elem);
                            match elem_data.kind() {
                                InstKind::Aggregate(agg) => {
                                    recursive(&agg, ctx, program);
                                }
                                InstKind::Integer(int) => {
                                    ctx.writeln(&format!(".word {}", int.value()))
                                }
                                _ => {
                                    todo!()
                                }
                            }
                        }
                    }
                    recursive(&agg, &mut self, program);
                }
                _ => {}
            };
        }
        self.decr_indent();
        self.writeln("");

        for &func in program.function_layout() {
            // skip if it's declaration
            if program.func_data(func).layout().entry_bb().is_none() {
                continue;
            };

            let name = program.func_data(func).name().strip_prefix("@").unwrap();
            self.incr_indent();
            self.writeln(".text");
            self.writeln(&format!(".globl {name}"));
            self.decr_indent();

            self.push_func(func);
            let func_data = program.func_data(func);
            func_data.generate(program, &mut self);
            self.pop_func();
            self.writeln("");
        }
        self.inst_list
    }

    #[inline]
    pub fn push_func(&mut self, func: Function) {
        self.func_stack.push(func);
    }

    pub fn writeln(&mut self, string: &str) {
        self.inst_list.push(Inst::_string {
            str: string.to_string(),
            indent_level: self.indent_level,
        });
    }

    pub fn write_inst(&mut self, inst: Inst) {
        self.inst_list.push(inst);
    }

    /// Generate the prologue of a function.
    ///
    /// `offset` is the size of the stack frame, which must be 16-byte aligned.
    ///
    /// `call_ra` indicates whether the function will call other functions, which determines
    /// whether we need to save the return address register `x30`.
    ///
    /// `callee_usage` is the set of callee-saved registers used in the function. Note that `x29`
    /// and `x30` are also callee-saved registers, but should not be included.
    ///
    /// When `call_ra` is true, `offset` must includes the space for `x29` and `x30` registers.
    pub fn prologue(&mut self, offset: usize, call_ra: bool, callee_usage: HashSet<Register>) {
        // AArch64 要求 sp 始终保持 16 字节对齐
        assert!(
            offset % 16 == 0,
            "AArch64 stack frame size must be 16-byte aligned"
        );

        let sp = Register::I(IReg(Bit::b64, IntRegister::sp));
        let x29 = Register::I(IReg(Bit::b64, IntRegister::x29));
        let x30 = Register::I(IReg(Bit::b64, IntRegister::x30));
        let offset = offset as i32;

        // 分配栈帧
        if offset != 0 {
            self.add_imm(sp, -offset, sp);
        }

        // ARMv8中x29和x30储存在低地址端
        // [sp]     : old x29
        // [sp + 8] : old x30
        if call_ra {
            self.save_word(x29, 0, sp);
            self.save_word(x30, 8, sp);
            self.add_imm(x29, 0, sp);
        }

        // 保存寄存器
        let mut stack_used: i32 = 0;
        for &reg in callee_usage.iter().sorted() {
            // 依次向下填充：[sp + offset - 16], [sp + offset - 24] ...
            self.save_word(reg, offset - (stack_used + 8), sp);
            stack_used += 8;
        }

        self.epilogue_stack.push(Epilogue {
            offset,
            call_ra,
            callee_usage,
            finished_once: false,
        });
    }

    #[inline]
    pub fn incr_indent(&mut self) {
        self.indent_level += 1;
    }

    #[inline]
    pub fn decr_indent(&mut self) {
        self.indent_level -= 1;
    }

    pub fn pop_func(&mut self) -> Option<Function> {
        self.func_stack.pop()
    }

    pub fn curr_func_hanlde(&self) -> &Function {
        self.func_stack.last().unwrap()
    }

    pub fn curr_func_data<'a>(&self, program: &'a Program) -> &'a FunctionData {
        program.func_data(*self.curr_func_hanlde())
    }

    pub fn load_to_para_register(&mut self, program: &Program, val: IrInst, reg: Register) {
        import_reg_and_inst!();
        // FIXME: maybe incorrect use of curr_func_data
        let data = self.curr_func_data(program).inst_data(val);
        match data.kind() {
            InstKind::Integer(int) => {
                self.load_imm(int.value());
            }
            // InstKind::FuncArgRef(arg_ref) if arg_ref.index() < 8 => {
            //     use Register::a0;
            //     let reg = (a0 as u8 + arg_ref.index() as u8).try_into().unwrap();
            //     self.alloc_para_reg(reg);
            // }
            _ if !data.ty().is_unit() => match self.allocation.get(&val).unwrap() {
                AllocationState::Register(register) => self.mv(*register, reg),
                AllocationState::Stack(offset) => {
                    let sp = Register::I(IReg(Bit::b64, IntRegister::sp));
                    self.load_word(reg, *offset as _, sp);
                }
            },
            _ => (),
        }
    }

    pub fn load_to_register(&mut self, program: &Program, val: IrInst) {
        if val.is_global() {
            self.load_address(
                program
                    .inst_data(val)
                    .name()
                    .clone()
                    .unwrap()
                    .strip_prefix('%')
                    .unwrap()
                    .to_string(),
            );
        } else {
            // local values, use inst_data directly.
            let data = self.curr_func_data(program).inst_data(val);
            match data.kind() {
                InstKind::Integer(int) => {
                    self.load_imm(int.value());
                }
                // InstKind::FuncArgRef(arg_ref) if arg_ref.index() < 8 => {
                //     use Register::a0;
                //     let reg = (a0 as u8 + arg_ref.index() as u8).try_into().unwrap();
                //     self.alloc_para_reg(reg);
                // }
                InstKind::Undef => {
                    self.undef_take_temp();
                }
                _ if !data.ty().is_unit() => {
                    // eprintln!(
                    //     "{:?} {:?}",
                    //     val,
                    //     self.curr_func_data(program).dfg().value(val).kind()
                    // );
                    match self.allocation.get(&val).unwrap() {
                        AllocationState::Register(register) => {
                            self.reg_pool.push_register(*register)
                        }
                        AllocationState::Stack(offset) => {
                            self.load_word_sp(*offset as _);
                        }
                    }
                    // let offset = self.get_inst_offset(val).unwrap() as i32;
                    // self.load_word_sp(offset);
                }
                _ => (),
            }
        }
    }

    pub fn curr_inst_mut(&mut self) -> &mut Option<IrInst> {
        &mut self.curr_inst
    }

    pub fn alloc_ret_reg(&mut self) {
        self.reg_pool.alloc_ret();
    }

    pub fn alloc_para_reg(&mut self, reg: Register) {
        assert!(reg.is_arg());
        self.reg_pool.push_register(reg)
    }

    pub fn pop_epilogue(&mut self) {
        self.epilogue_stack.pop();
    }

    pub fn multiply(&mut self) {
        import_reg_and_inst!();
        let rhs = self.reg_pool.take_register();
        let lhs = self.reg_pool.take_register();
        let ans = self.reg_pool.alloc_temp();
        self.write_inst(mul {
            rd: ans,
            rs1: lhs,
            rs2: rhs,
        });
    }

    pub fn add_op(&mut self) {
        import_reg_and_inst!();
        let rhs = self.reg_pool.take_register();
        let lhs = self.reg_pool.take_register();
        let ans = self.reg_pool.alloc_temp();
        self.write_inst(add {
            rd: ans,
            rs1: lhs,
            rs2: AddSubOperand::Register(rhs),
        });
    }

    pub fn add_sp(&mut self) {
        import_reg_and_inst!();
        let sp = Register::I(IReg(Bit::b64, IntRegister::sp));
        let rhs = self.reg_pool.take_register();
        let ans = self.reg_pool.alloc_temp();
        self.write_inst(add {
            rd: ans,
            rs1: sp,
            rs2: AddSubOperand::Register(rhs),
        });
    }

    pub fn add(&mut self, rd: Register, rs1: Register, rs2: Register) {
        import_reg_and_inst!();
        self.write_inst(add {
            rd,
            rs1,
            rs2: AddSubOperand::Register(rs2),
        });
    }

    pub fn allocation_mut(&mut self) -> &mut RegisterAllocation {
        &mut self.allocation
    }

    pub fn curr_inst(&self) -> Option<IrInst> {
        self.curr_inst
    }
}

impl AsmGenContext {
    // undef should not have any memory moves when being efficiency.
    pub fn undef_take_temp(&mut self) {
        self.reg_pool.alloc_temp();
    }

    #[inline]
    pub fn load_imm(&mut self, imm: i32) {
        import_reg_and_inst!();
        let temp_reg = self.reg_pool.alloc_temp();
        if (-32768..32768).contains(&imm) {
            self.write_inst(mov {
                rd: temp_reg,
                src: MovOperand::Immediate(MoveWideImm::Imm16 {
                    value: imm as u16,
                    shift: MoveWideImmShift::B0,
                }),
            });
        } else {
            let immz = (imm & 0xFFFF) as u16; // Low 16 bits
            let immk = ((imm >> 16) & 0xFFFF) as u16; // High 16 bits
            self.write_inst(movz {
                rd: temp_reg,
                imm: MoveWideImm::Imm16 {
                    value: immz,
                    shift: MoveWideImmShift::B0,
                },
            });
            self.write_inst(movk {
                rd: temp_reg,
                imm: MoveWideImm::Imm16 {
                    value: immk,
                    shift: MoveWideImmShift::B16,
                },
            });
        }
    }

    pub fn save_word_at_curr_inst(&mut self) {
        self.save_word_at_inst(self.curr_inst.unwrap());
    }

    pub fn save_word_at_inst(&mut self, val: IrInst) {
        match self.allocation.get(&val).unwrap() {
            AllocationState::Register(register) => {
                let source = self.reg_pool.take_register();
                self.mv(source, *register)
            }
            AllocationState::Stack(offset) => {
                self.save_word_with_offset(*offset as _);
            }
        }
    }

    pub fn save_word(&mut self, rs2: Register, imm: i32, rs1: Register) {
        import_reg_and_inst!();
        if (-2048..2048).contains(&imm) {
            self.write_inst(sdr {
                rd: rs2,
                rs: rs1,
                offset: LoadSaveOffset::Imm12(imm as i16),
            });
        } else {
            self.load_imm(imm);
            let imm_reg = self.reg_pool.take_register();
            self.add(imm_reg, rs1, imm_reg);
            self.write_inst(sdr {
                rd: rs2,
                rs: imm_reg,
                offset: LoadSaveOffset::Imm12(0),
            });
        }
    }

    #[inline]
    pub fn save_word_with_offset(&mut self, offset: i32) {
        import_reg_and_inst!();
        if (-2048..2048).contains(&offset) {
            let temp_reg = self.reg_pool.take_register();
            let sp = Register::I(IReg(Bit::b64, IntRegister::sp));
            self.write_inst(sdr {
                rd: temp_reg,
                rs: sp,
                offset: LoadSaveOffset::Imm12(offset as i16),
            });
        } else {
            self.load_imm(offset);
            self.add_sp();
            let add_temp = self.reg_pool.take_register();
            let temp_reg = self.reg_pool.take_register();
            self.write_inst(sdr {
                rd: temp_reg,
                rs: add_temp,
                offset: LoadSaveOffset::Imm12(0),
            });
        }
    }

    #[inline]
    pub fn save_word_at_address(&mut self) {
        import_reg_and_inst!();
        let val_reg = self.reg_pool.take_register();
        let address_reg = self.reg_pool.take_register();
        self.write_inst(sdr {
            rd: val_reg,
            rs: address_reg,
            offset: LoadSaveOffset::Imm12(0),
        });
    }

    pub fn load_word(&mut self, rd: Register, offset: i32, rs: Register) {
        import_reg_and_inst!();
        if (-2048..2048).contains(&offset) {
            self.write_inst(ldr {
                rd,
                rs,
                offset: LoadSaveOffset::Imm12(offset as i16),
            });
        } else {
            self.load_imm(offset);
            self.add_sp();
            let add_temp = self.reg_pool.take_register();
            self.write_inst(ldr {
                rd,
                rs: add_temp,
                offset: LoadSaveOffset::Imm12(0),
            });
        }
    }

    pub fn add_imm(&mut self, rd: Register, imm: i32, rs: Register) {
        import_reg_and_inst!();
        if (-2048..2048).contains(&imm) {
            let imm12 = AddSubOperand::Immediate(AddSubImm::Imm12(imm as i16));
            self.write_inst(add {
                rd,
                rs1: rs,
                rs2: imm12,
            })
        } else {
            self.load_imm(imm);
            let imm_reg = self.reg_pool.take_register();
            self.write_inst(add {
                rd,
                rs1: rs,
                rs2: AddSubOperand::Register(imm_reg),
            });
        }
    }

    #[inline]
    pub fn load_word_sp(&mut self, offset: i32) {
        import_reg_and_inst!();
        let sp = Register::I(IReg(Bit::b64, IntRegister::sp));
        if (-2048..2048).contains(&offset) {
            let temp_reg = self.reg_pool.alloc_temp();
            self.write_inst(ldr {
                rd: temp_reg,
                rs: sp,
                offset: LoadSaveOffset::Imm12(offset as i16),
            });
        } else {
            self.load_imm(offset);
            self.add_sp();
            let add_temp = self.reg_pool.take_register();
            let temp_reg = self.reg_pool.alloc_temp();
            self.write_inst(ldr {
                rd: temp_reg,
                rs: add_temp,
                offset: LoadSaveOffset::Imm12(0),
            });
        };
    }

    pub fn load_address(&mut self, label: String) {
        import_reg_and_inst!();
        let temp_reg = self.reg_pool.alloc_temp();
        // use adrp + add to load address of global variable.
        self.write_inst(adrp {
            rd: temp_reg,
            label: label.clone(),
        });
        self.write_inst(add {
            rd: temp_reg,
            rs1: temp_reg,
            rs2: AddSubOperand::AddrLo12(label),
        });
    }

    pub fn load_from_address(&mut self) {
        import_reg_and_inst!();
        let address_reg = self.reg_pool.take_register();
        let value_reg = self.reg_pool.alloc_temp();
        self.write_inst(ldr {
            rd: value_reg,
            rs: address_reg,
            offset: LoadSaveOffset::Imm12(0),
        });
    }

    pub fn binary_op(&mut self, op: BinaryOp) {
        import_reg_and_inst!();
        let rhs = self.reg_pool.take_register();
        let lhs = self.reg_pool.take_register();
        let res = self.reg_pool.alloc_temp();
        match op {
            // ARMv8 uses the sign bits of the result of a cmp instruction to determine
            // the condition flags, so we can use cmp + cset to implement these comparisons.
            BinaryOp::NotEq => {
                self.write_inst(cmp {
                    rs1: lhs,
                    rs2: AddSubOperand::Register(rhs),
                });
                self.write_inst(cset {
                    rd: res,
                    condition: CsetCondition::NE,
                });
            }
            BinaryOp::Eq => {
                self.write_inst(cmp {
                    rs1: lhs,
                    rs2: AddSubOperand::Register(rhs),
                });
                self.write_inst(cset {
                    rd: res,
                    condition: CsetCondition::EQ,
                });
            }
            BinaryOp::Gt => {
                self.write_inst(cmp {
                    rs1: lhs,
                    rs2: AddSubOperand::Register(rhs),
                });
                self.write_inst(cset {
                    rd: res,
                    condition: CsetCondition::GT,
                });
            }
            BinaryOp::Lt => {
                self.write_inst(cmp {
                    rs1: lhs,
                    rs2: AddSubOperand::Register(rhs),
                });
                self.write_inst(cset {
                    rd: res,
                    condition: CsetCondition::LT,
                });
            }
            BinaryOp::Ge => {
                self.write_inst(cmp {
                    rs1: lhs,
                    rs2: AddSubOperand::Register(rhs),
                });
                self.write_inst(cset {
                    rd: res,
                    condition: CsetCondition::GE,
                });
            }
            BinaryOp::Le => {
                self.write_inst(cmp {
                    rs1: lhs,
                    rs2: AddSubOperand::Register(rhs),
                });
                self.write_inst(cset {
                    rd: res,
                    condition: CsetCondition::LE,
                });
            }
            BinaryOp::Add => self.write_inst(add {
                rd: res,
                rs1: lhs,
                rs2: AddSubOperand::Register(rhs),
            }),

            BinaryOp::Sub => self.write_inst(sub {
                rd: res,
                rs1: lhs,
                rs2: AddSubOperand::Register(rhs),
            }),
            BinaryOp::Mul => self.write_inst(mul {
                rd: res,
                rs1: lhs,
                rs2: rhs,
            }),
            BinaryOp::Div => self.write_inst(sdiv {
                rd: res,
                rs1: lhs,
                rs2: rhs,
            }),
            BinaryOp::Rem => {
                self.write_inst(sdiv {
                    rd: res,
                    rs1: lhs,
                    rs2: rhs,
                });
                self.write_inst(mul {
                    rd: res,
                    rs1: res,
                    rs2: rhs,
                });
                self.write_inst(sub {
                    rd: res,
                    rs1: lhs,
                    rs2: AddSubOperand::Register(res),
                });
            }
            BinaryOp::And => self.write_inst(and {
                rd: res,
                rs1: lhs,
                rs2: LogicOperand::Register(rhs),
            }),
            BinaryOp::Or => self.write_inst(orr {
                rd: res,
                rs1: lhs,
                rs2: LogicOperand::Register(rhs),
            }),
            BinaryOp::Xor => self.write_inst(eor {
                rd: res,
                rs1: lhs,
                rs2: LogicOperand::Register(rhs),
            }),
            BinaryOp::Shl => self.write_inst(lsl {
                rd: res,
                rs1: lhs,
                rs2: ShiftSize::Register(rhs),
            }),
            BinaryOp::Shr => self.write_inst(lsr {
                rd: res,
                rs1: lhs,
                rs2: ShiftSize::Register(rhs),
            }),
            BinaryOp::Sar => self.write_inst(asr {
                rd: res,
                rs1: lhs,
                rs2: ShiftSize::Register(rhs),
            }),
        }
    }

    pub fn ret(&mut self) {
        import_reg_and_inst!();
        let source = self.reg_pool.take_register();
        let x0 = Register::I(IReg(Bit::b64, IntRegister::x0));
        self.write_inst(mov {
            rd: x0,
            src: MovOperand::Register(source),
        });
        self.epilogue_stack
            .last_mut()
            .unwrap()
            .mark()
            .clone()
            .finish(self);
    }

    fn mv(&mut self, source: Register, dest: Register) {
        import_reg_and_inst!();
        self.write_inst(mov {
            rd: dest,
            src: MovOperand::Register(source),
        });
    }

    pub fn void_ret(&mut self) {
        self.epilogue_stack
            .last_mut()
            .unwrap()
            .mark()
            .clone()
            .finish(self);
    }

    pub fn jump(&mut self, bb: BasicBlock, program: &Program) {
        import_reg_and_inst!();
        self.write_inst(b {
            label: self.get_bb_name(bb, program),
        });
    }

    pub fn if_jump(&mut self, true_bb: BasicBlock, false_bb: BasicBlock, program: &Program) {
        import_reg_and_inst!();
        let cond_reg = self.reg_pool.take_register();
        self.write_inst(cbnz {
            rs: cond_reg,
            label: self.get_bb_name(true_bb, program),
        });
        self.write_inst(cbz {
            rs: cond_reg,
            label: self.get_bb_name(false_bb, program),
        });
    }
}
