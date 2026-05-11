use std::{collections::HashMap, sync::atomic::AtomicU32};

use super::{operand::Inst, prelude::*};
use crate::mir::{
    instruction::{BasicBlock, Function, Program},
    operand::{MemAddr, MirRegister},
};

pub struct ArenaContext<'a> {
    program: &'a HirProgram,
    curr_func: Option<HirFunction>,
}

impl Arena for ArenaContext<'_> {
    fn local(&self) -> &crate::ir::arena::LocalArena {
        self.program
            .func_data(self.curr_func.unwrap())
            .local_arena()
    }

    fn local_mut(&mut self) -> &mut crate::ir::arena::LocalArena {
        unimplemented!()
    }

    fn global(&self) -> &crate::ir::arena::GlobalArena {
        self.program.global_arena()
    }

    fn global_mut(&mut self) -> &mut crate::ir::arena::GlobalArena {
        unimplemented!()
    }
}

struct Query {
    vreg: u32,
    map: HashMap<HirInst, MemAddr>,
}

impl Query {
    fn new() -> Query {
        Query {
            vreg: 0,
            map: HashMap::new(),
        }
    }

    fn new_vreg(&mut self) -> MirRegister {
        let ret = MirRegister::Virtual(self.vreg);
        self.vreg += 1;
        ret
    }

    fn insert(&mut self, inst: HirInst, vreg: MemAddr) {
        self.map.insert(inst, vreg);
    }

    pub fn get(&self, k: &HirInst) -> MemAddr {
        *self.map.get(k).unwrap()
    }
}

pub fn convert_program(p: &HirProgram) -> Program {
    let mut result = Program {
        funcs: Vec::new(),
        global: Vec::new(),
    };
    let mut context = ArenaContext {
        program: p,
        curr_func: None,
    };
    let mut q = Query::new();
    for &global_inst in p.global_inst_layout() {
        convert_global_inst(global_inst, &context, &mut result, &mut q);
    }
    for &func in p.function_layout() {
        context.curr_func.replace(func);
        convert_function(func, &context, &mut result);
    }
    result
}

fn convert_global_inst(inst: HirInst, ctx: &ArenaContext<'_>, p: &mut Program, q: &mut Query) {
    let HirInstKind::GlobalAlloc(ga) = ctx.inst_data(inst).kind() else {
        unreachable!()
    };
    let init_ty = ctx.inst_data(inst).ty().derefernce();
    match ctx.inst_data(ga.init()).kind() {
        HirInstKind::Integer(int) => {
            p.global.push(Inst::GlobalInitI32 {
                init: vec![int.value()],
            });
        }
        HirInstKind::Float(float) => {
            p.global.push(Inst::GlobalInitF32 {
                init: vec![float.value()],
            });
        }
        HirInstKind::ZeroInit => {
            if init_ty.array_base_scalar_type().is_i32() {
                p.global.push(Inst::GlobalInitI32 {
                    init: vec![0; init_ty.array_flatten_length()],
                });
            } else {
                p.global.push(Inst::GlobalInitF32 {
                    init: vec![0.0; init_ty.array_flatten_length()],
                });
            }
        }
        HirInstKind::Aggregate(agg) => {
            use crate::ir::inst_kind::Aggregate;

            fn recursive_i32(agg: &Aggregate, init: &mut Vec<i32>, ctx: &ArenaContext<'_>) {
                for &elem in agg.value() {
                    let elem_data = ctx.inst_data(elem);
                    match elem_data.kind() {
                        HirInstKind::Aggregate(agg) => {
                            recursive_i32(agg, init, ctx);
                        }
                        HirInstKind::Integer(int) => init.push(int.value()),
                        _ => {
                            todo!()
                        }
                    }
                }
            }
            fn recursive_f32(agg: &Aggregate, init: &mut Vec<f32>, ctx: &ArenaContext<'_>) {
                for &elem in agg.value() {
                    let elem_data = ctx.inst_data(elem);
                    match elem_data.kind() {
                        HirInstKind::Aggregate(agg) => {
                            recursive_f32(agg, init, ctx);
                        }
                        HirInstKind::Float(float) => init.push(float.value()),
                        _ => {
                            todo!()
                        }
                    }
                }
            }
            if init_ty.array_base_scalar_type().is_i32() {
                let mut v = Vec::new();
                recursive_i32(agg, &mut v, ctx);
                p.global.push(Inst::GlobalInitI32 { init: v });
            } else {
                let mut v = Vec::new();
                recursive_f32(agg, &mut v, ctx);
                p.global.push(Inst::GlobalInitF32 { init: v });
            }
        }
        _ => unreachable!(),
    }
}

pub fn convert_function(func: HirFunction, ctx: &ArenaContext<'_>, p: &mut Program) {
    let mut mir_function = Function {
        name: ctx.func_data(func).name().to_string(),
        blocks: vec![],
        stack_size: 0,
    };
    for bb in ctx.program.func_data(func).layout().basicblocks() {
        convert_basic_block(bb, ctx, &mut mir_function);
    }
    p.funcs.push(mir_function);
}

pub fn convert_basic_block(
    layout: &HirBasicBlockLayout,
    ctx: &ArenaContext<'_>,
    func: &mut Function,
) {
    let mut bb = BasicBlock {
        name: ctx.bb_data(layout.bb()).name().to_string(),
        insts: vec![],
    };
    for &inst in layout.insts() {
        convert_local_inst(inst, ctx, &mut bb);
    }
}

pub fn convert_local_inst(inst: HirInst, ctx: &ArenaContext<'_>, bb: &mut BasicBlock) {
    match ctx.inst_data(inst).kind() {
        HirInstKind::Aggregate(..)
        | HirInstKind::BlockArgRef(..)
        | HirInstKind::FuncArgRef(..)
        | HirInstKind::GlobalAlloc(..)
        | HirInstKind::Undef
        | HirInstKind::ZeroInit
        | HirInstKind::Integer(..)
        | HirInstKind::Float(..) => unreachable!(),
        // For Alloc instruction, we don't have to do anything.
        HirInstKind::Alloc => {}
        // Generate Binary Instruction
        HirInstKind::Binary(binary) => convert_binary(binary, ctx, bb),
        HirInstKind::Jump(jump) => todo!(),
        HirInstKind::Branch(branch) => todo!(),
        HirInstKind::Cast(cast) => todo!(),
        HirInstKind::Return(ret) => todo!(),
        HirInstKind::GetElemPtr(get_elem_ptr) => todo!(),
        HirInstKind::GetPtr(get_ptr) => todo!(),
        HirInstKind::Store(store) => todo!(),
        HirInstKind::Load(load) => todo!(),
        HirInstKind::Call(call) => todo!(),
    }
}

fn convert_binary(binary: &Binary, ctx: &ArenaContext<'_>, bb: &mut BasicBlock) {
    match binary.op() {
        BinaryOp::Add => todo!(),
        BinaryOp::Sub => todo!(),
        BinaryOp::Mul => todo!(),
        BinaryOp::Div => todo!(),
        BinaryOp::Rem => todo!(),
        BinaryOp::NotEq => todo!(),
        BinaryOp::Eq => todo!(),
        BinaryOp::Gt => todo!(),
        BinaryOp::Lt => todo!(),
        BinaryOp::Ge => todo!(),
        BinaryOp::Le => todo!(),
        BinaryOp::And => todo!(),
        BinaryOp::Or => todo!(),
        BinaryOp::Xor => todo!(),
        BinaryOp::Shl => todo!(),
        BinaryOp::Shr => todo!(),
        BinaryOp::Sar => todo!(),
    }
}
