use std::{
    collections::{
        HashMap,
        hash_map::Entry::{Occupied, Vacant},
    },
    num::NonZeroU32,
};

use crate::armv8::{
    instruction::{BasicBlock, Function, Program},
    operand::{Inst, InstKind, Register, RegisterKind},
};
use crate::prelude::*;

struct Query {
    vreg_counter: u32,
    inst_counter: u32,
    inst_def: HashMap<HirInst, Register>,
    uses: HashMap<Register, Vec<Inst>>,
    defs: HashMap<Register, Vec<Inst>>,
    pool: HashMap<Inst, InstKind>,
}

impl Query {
    fn new() -> Query {
        Query {
            vreg_counter: 65,
            inst_counter: 1,
            inst_def: HashMap::new(),
            uses: HashMap::new(),
            defs: HashMap::new(),
            pool: HashMap::new(),
        }
    }

    fn new_vreg(&mut self, kind: RegisterKind) -> Register {
        let ret = Register::new_virtual(self.vreg_counter, kind);
        self.vreg_counter += 1;
        ret
    }

    // Create a new virtual register
    // Bind to definition of a HIR instruction
    // Return it for more use
    fn bind_def(&mut self, inst: HirInst, ctx: &ArenaContext<'_>) -> Register {
        let ty = ctx.inst_data(inst).ty();
        let reg = self.new_vreg(if ty.is_i32() {
            RegisterKind::I32
        } else if ty.is_f32() {
            RegisterKind::F32
        } else {
            RegisterKind::I64
        });
        self.inst_def.insert(inst, reg);
        reg
    }

    fn get_vreg(&mut self, inst: HirInst, ctx: &ArenaContext<'_>) -> Register {
        match ctx.inst_data(inst).kind() {
            HirInstKind::Integer(..) | HirInstKind::Float(..) => self.bind_def(inst, ctx),
            _ => *self.inst_def.get(&inst).unwrap(),
        }
    }

    fn get_vreg_or_default(&mut self, inst: HirInst, ctx: &ArenaContext<'_>) -> Register {
        match ctx.inst_data(inst).kind() {
            HirInstKind::Integer(..) | HirInstKind::Float(..) => self.bind_def(inst, ctx),
            _ => {
                if let Some(&reg) = self.inst_def.get(&inst) {
                    return reg;
                }
                self.bind_def(inst, ctx)
            }
        }
    }

    fn push(&mut self, bb: &mut BasicBlock, inst_kind: InstKind) {
        let inst = Inst(unsafe { NonZeroU32::new_unchecked(self.inst_counter) });
        self.inst_counter += 1;
        inst_kind
            .uses()
            .for_each(|r| self.uses.entry(r).or_default().push(inst));
        inst_kind
            .def()
            .for_each(|r| self.defs.entry(r).or_default().push(inst));
        self.pool.insert(inst, inst_kind);
        bb.insts.push(inst);
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
        convert_global_inst(global_inst, &context, &mut result);
    }
    for &func in p.function_layout() {
        context.curr_func.replace(func);
        convert_function(func, &context, &mut result, &mut q);
    }
    result
}

fn convert_global_inst(inst: HirInst, ctx: &ArenaContext<'_>, p: &mut Program) {
    let HirInstKind::GlobalAlloc(ga) = ctx.inst_data(inst).kind() else {
        unreachable!()
    };
    let init_ty = ctx.inst_data(inst).ty().derefernce();
    match ctx.inst_data(ga.init()).kind() {
        HirInstKind::Integer(int) => {
            p.global.push(InstKind::GlobalInitI32 {
                init: vec![int.value()],
            });
        }
        HirInstKind::Float(float) => {
            p.global.push(InstKind::GlobalInitF32 {
                init: vec![float.value().to_bits()],
            });
        }
        HirInstKind::ZeroInit => {
            if init_ty.array_base_scalar_type().is_i32() {
                p.global.push(InstKind::GlobalInitI32 {
                    init: vec![0; init_ty.array_flatten_length()],
                });
            } else {
                p.global.push(InstKind::GlobalInitF32 {
                    init: vec![0.0f32.to_bits(); init_ty.array_flatten_length()],
                });
            }
        }
        HirInstKind::Aggregate(agg) => {
            use raana_ir::ir::inst_kind::Aggregate;

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
            fn recursive_f32(agg: &Aggregate, init: &mut Vec<u32>, ctx: &ArenaContext<'_>) {
                for &elem in agg.value() {
                    let elem_data = ctx.inst_data(elem);
                    match elem_data.kind() {
                        HirInstKind::Aggregate(agg) => {
                            recursive_f32(agg, init, ctx);
                        }
                        HirInstKind::Float(float) => init.push(float.value().to_bits()),
                        _ => {
                            todo!()
                        }
                    }
                }
            }
            if init_ty.array_base_scalar_type().is_i32() {
                let mut v = Vec::new();
                recursive_i32(agg, &mut v, ctx);
                p.global.push(InstKind::GlobalInitI32 { init: v });
            } else {
                let mut v = Vec::new();
                recursive_f32(agg, &mut v, ctx);
                p.global.push(InstKind::GlobalInitF32 { init: v });
            }
        }
        _ => unreachable!(),
    }
}

fn convert_function(func: HirFunction, ctx: &ArenaContext<'_>, p: &mut Program, q: &mut Query) {
    let mut mir_function = Function {
        name: ctx.func_data(func).name().to_string(),
        blocks: vec![],
        stack_size: 0,
    };
    for bb in ctx.program.func_data(func).layout().basicblocks() {
        convert_basic_block(bb, ctx, &mut mir_function, q);
    }
    p.funcs.push(mir_function);
}

fn convert_basic_block(
    layout: &HirBasicBlockLayout,
    ctx: &ArenaContext<'_>,
    func: &mut Function,
    q: &mut Query,
) {
    let mut bb = BasicBlock {
        name: ctx.bb_data(layout.bb()).name().to_string(),
        insts: vec![],
    };
    for &inst in layout.insts() {
        convert_local_inst(inst, ctx, &mut bb, q);
    }
    func.blocks.push(bb);
}

fn convert_local_inst(inst: HirInst, ctx: &ArenaContext<'_>, bb: &mut BasicBlock, q: &mut Query) {
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
        HirInstKind::Binary(binary) => convert_binary(binary, ctx, bb, q),
        HirInstKind::Jump(jump) => convert_jump(jump, ctx, bb, q),
        HirInstKind::Branch(branch) => todo!(),
        HirInstKind::Cast(cast) => {
            let reg = q.bind_def(inst, ctx);
            convert_cast(cast, ctx, bb, q, reg)
        }
        HirInstKind::Return(ret) => todo!(),
        HirInstKind::GetElemPtr(get_elem_ptr) => todo!(),
        HirInstKind::GetPtr(get_ptr) => todo!(),
        HirInstKind::Store(store) => todo!(),
        HirInstKind::Load(load) => todo!(),
        HirInstKind::Call(call) => todo!(),
    }
}

fn convert_binary(binary: &Binary, ctx: &ArenaContext<'_>, bb: &mut BasicBlock, q: &mut Query) {
    // INFO: Binary could only accept scalar type.
    // So we hardcode 32bit here.

    // INFO:: If immediate appear at either side, it will automatically loaded into a register.
    let lhs = q.get_vreg(binary.lhs(), ctx);
    let rhs = q.get_vreg(binary.rhs(), ctx);
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

fn convert_jump(jump: &Jump, ctx: &ArenaContext<'_>, bb: &mut BasicBlock, q: &mut Query) {
    let bb_data = ctx.bb_data(jump.target());
    let params = bb_data
        .params()
        .iter()
        .zip(jump.args())
        // Because:
        // - parameter could possibly have not been visited yet until we visiting jump.
        // - parameter could have already been visited because other jump/branch instruction.
        // so we use special method.
        .map(|(&param, &arg)| (q.get_vreg_or_default(param, ctx), q.get_vreg(arg, ctx)))
        .collect::<Vec<_>>();
    let target_name = bb_data.name().to_string();
    q.push(bb, InstKind::b { label: target_name });
}

fn convert_cast(
    cast: &Cast,
    ctx: &ArenaContext<'_>,
    bb: &mut BasicBlock,
    q: &mut Query,
    rd: Register,
) {
    // INFO: Cast is assured to have i32 -> f32 or f32 -> i32.
    // No identical cast should appear.
    let src_ty = ctx.inst_data(cast.src()).ty();
    let rs = q.get_vreg(cast.src(), ctx);
    if src_ty.is_i32() {
        q.push(bb, InstKind::scvtf { rd, rs });
    } else {
        q.push(bb, InstKind::fcvtzs { rd, rs });
    };
}

fn can_produce_value(val: HirInst, data: &ArenaContext<'_>) -> bool {
    if data.inst_data(val).ty().size() == 0 {
        return false;
    }

    matches!(
        data.inst_data(val).kind(),
        HirInstKind::FuncArgRef(..)
            | HirInstKind::BlockArgRef(..)
            | HirInstKind::Cast(..)
            | HirInstKind::Alloc
            | HirInstKind::Load(..)
            | HirInstKind::GetPtr(..)
            | HirInstKind::GetElemPtr(..)
            | HirInstKind::Binary(..)
            | HirInstKind::Call(..)
    )
}
