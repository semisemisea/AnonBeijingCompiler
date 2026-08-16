//! AArch64 selection from Raana HIR into generic VCode.

use rustc_hash::FxHashSet;

use raana_ir::ir::{
    Binary, BinaryOp, Call, Cast, Fma, GetElemPtr, InstKind, Load, Return, Select, Store, TailCall,
    Type as HirType, TypeKind, VectorExtractElement, VectorInsertElement, VectorReduce,
    VectorReduceOp, VectorSplat,
    arena::Arena,
    inst_kind::{MemZero, MemZeroLen},
};
use taki_mir::{
    abi::{ABIMachineSpec, ArgSlot, CallArgPair, CallRetPair, RetPair, StackAMode},
    block_order::{LoweredBlock, MirBlockIndex},
    div_magic::{MagicCorrection, signed_magic_i32},
    lower::{
        LowerBackend, LowerContext, LoweredOutput, analyze_gep, fold_gep_constant_offset,
        sink_gep_into_address,
    },
    prelude::{ArenaContext, HirFunction, HirFunctionData, HirInst},
    reg_alloc::reg::PReg,
    register::Writable,
    vcode::MachInst,
};

use crate::{
    abi::AArch64Abi,
    instructions::{
        AMode, AluOp, CCmpStep, Cond, ExtendOp, FpuOp, Imm12, ImmLogic, ImmShift, MInst,
        MemoryType, SelectCmp, SelectValue, ShiftOp, VecArithOp, VecBitOp, VecCmpOp, VecCvtOp,
        VecMinMaxOp, VecShape, invert_cond,
    },
    labels::Label,
    regs::{self, OperandSize, RegOrZr},
    runtime::{self, EmbeddedSymbol},
};
use taki_mir::register::Reg;

pub struct AArch64Backend;

mod arith;
mod branch;
mod call;
mod memory;
mod vector;

use arith::{lower_binary, lower_cast};
use branch::{lower_select, select_branch_condition};
use call::{lower_call, lower_return, lower_tail_call};
use memory::{lower_alloc, lower_get_elem_ptr, lower_load, lower_mem_zero, lower_store};
use vector::{
    lower_fma, lower_vector_extract_element, lower_vector_insert_element, lower_vector_reduce,
    lower_vector_splat,
};

impl LowerBackend for AArch64Backend {
    type MInst = MInst;
    type CodegenConfig = crate::config::AArch64CodegenConfig;

    fn lower(ctx: &mut LowerContext<Self::MInst>, inst: HirInst) -> LoweredOutput {
        let arena = ctx.arena;
        match arena.inst_data(inst).kind() {
            InstKind::BlockArgRef(..)
            | InstKind::Aggregate(..)
            | InstKind::GlobalAlloc(..)
            | InstKind::Undef
            | InstKind::Integer(..)
            | InstKind::Float(..) => {
                unreachable!("constants and argument references are rematerialized by LowerContext")
            }
            // Scalar arithmetic and casts.
            InstKind::Binary(binary) => lower_binary(ctx, arena, inst, binary),
            InstKind::Cast(cast) => lower_cast(ctx, arena, inst, cast),
            // Select and condition-chain lowering.
            InstKind::Select(select) => lower_select(ctx, arena, inst, select),
            // Memory addressing, loads, stores, and zeroing.
            InstKind::Alloc => lower_alloc(ctx, arena, inst),
            InstKind::GetElemPtr(gep) => lower_get_elem_ptr(ctx, arena, inst, gep),
            InstKind::Load(load) => lower_load(ctx, arena, inst, load),
            InstKind::Store(store) => lower_store(ctx, arena, inst, store),
            InstKind::MemZero(mem_zero) => lower_mem_zero(ctx, arena, mem_zero),
            InstKind::ZeroInit => unreachable!("zero initialization is lowered by its store"),
            // Calls, tail calls, and returns.
            InstKind::Call(call) => lower_call(ctx, arena, inst, call),
            InstKind::TailCall(tail_call) => lower_tail_call(ctx, arena, tail_call),
            InstKind::Return(ret) => lower_return(ctx, arena, ret),
            // NEON vector lowering.
            InstKind::Fma(fma) => lower_fma(ctx, arena, inst, fma),
            InstKind::VectorSplat(splat) => lower_vector_splat(ctx, arena, inst, splat),
            InstKind::VectorExtractElement(extract) => {
                lower_vector_extract_element(ctx, arena, inst, extract)
            }
            InstKind::VectorInsertElement(insert) => {
                lower_vector_insert_element(ctx, arena, inst, insert)
            }
            InstKind::VectorReduce(reduce) => lower_vector_reduce(ctx, arena, inst, reduce),
            InstKind::Jump(..) | InstKind::Branch(..) => {
                unreachable!("terminators are lowered by LowerBackend::lower_branch")
            }
        }
    }

    fn lower_branch(
        ctx: &mut LowerContext<Self::MInst>,
        inst: raana_ir::opt::prelude::Inst,
        target: &[MirBlockIndex],
    ) {
        let arena = ctx.arena;
        match arena.inst_data(inst).kind() {
            InstKind::Jump(jump) => {
                for &arg in jump.args() {
                    ctx.put_value_in_reg(arg);
                }
                let &[target] = target else {
                    unreachable!("jump must have one lowered successor");
                };
                ctx.emit(MInst::gen_jump(target));
            }
            InstKind::Branch(branch) => {
                for &arg in branch.t_args().iter().chain(branch.f_args()) {
                    ctx.put_value_in_reg(arg);
                }
                let &[true_target, false_target] = target else {
                    unreachable!("branch must have two lowered successors");
                };
                if select_branch_condition(ctx, inst, branch.cond(), true_target, false_target) {
                    return;
                }

                let cond = ctx.put_value_in_reg(branch.cond());
                ctx.emit(MInst::Cbnz {
                    size: OperandSize::Size32,
                    reg: cond,
                    true_label: Label::from_block(true_target),
                    false_label: Label::from_block(false_target),
                });
            }
            kind => {
                ctx.lowering_panic(
                    "AArch64 branch selection",
                    format!("non-terminator HIR instruction {kind:?} cannot select a branch"),
                    None,
                    Some(arena.inst_data(inst).ty()),
                );
            }
        }
    }

    fn data_section_directive() -> &'static str {
        ".section .data"
    }

    fn bss_section_directive() -> &'static str {
        ".section .bss"
    }

    fn text_section_directive() -> &'static str {
        ".section .text"
    }

    fn global_directive() -> &'static str {
        ".globl"
    }

    fn word_directive() -> &'static str {
        ".word"
    }

    fn zero_directive() -> &'static str {
        ".zero"
    }

    fn preg_name(preg: PReg) -> &'static str {
        regs::preg_name(preg)
    }

    fn format_block_label(lb: &LoweredBlock, func_data: &HirFunctionData) -> String {
        let function = func_data.name().replace('%', "_");
        match lb {
            LoweredBlock::Orig { block } => {
                format!(
                    ".L_{function}_{}",
                    func_data.bb_data(*block).name().replace('%', "_")
                )
            }
            LoweredBlock::Edge {
                pred,
                succ,
                succ_idx,
            } => format!(
                ".L_{function}_{}_to_{}_edge_{}",
                func_data.bb_data(*pred).name().replace('%', "_"),
                func_data.bb_data(*succ).name().replace('%', "_"),
                succ_idx
            ),
        }
    }

    fn emit_long_jump(ctx: &mut LowerContext<Self::MInst>, target: MirBlockIndex) {
        ctx.emit(MInst::gen_jump(target));
    }

    fn runtime_assembly(program: &taki_mir::prelude::HirProgram) -> Option<String> {
        runtime::assembly(program)
    }

    fn mir_pipeline(
        config: &Self::CodegenConfig,
    ) -> taki_mir::passes::MIRPassPipeline<Self::MInst> {
        crate::passes::build_pipeline(config)
    }

    fn branch_opt_enabled(config: &Self::CodegenConfig) -> bool {
        config.branch_opt
    }

    fn veneer_lines(kind: taki_mir::emit_buffer::LabelKind, target: &str) -> Vec<String> {
        use taki_mir::emit_buffer::LabelKind;
        match kind {
            // A conditional branch fell out of ±1MB: the (inverted) branch
            // reaches the adjacent veneer; the veneer's `b` covers ±128MB.
            LabelKind::BRANCH14 | LabelKind::BRANCH19 => vec![format!("b {target}")],
            // `b` itself fell out of ±128MB: materialize the address through
            // the linker scratch register x16 (practically unreachable).
            LabelKind::BRANCH26 => vec![
                format!("adrp x16, {target}"),
                format!("add x16, x16, :lo12:{target}"),
                "br x16".to_owned(),
            ],
            other => unreachable!("AArch64 does not emit {other:?} branches"),
        }
    }
}

#[cfg(test)]
mod tests;
