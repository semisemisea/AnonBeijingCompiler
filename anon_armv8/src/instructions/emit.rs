//! MInst emission dispatch split by instruction group.

use taki_mir::vcode::EmitContext;

use super::MInst;

mod alu;
mod branch;
mod memory;
mod neon;

pub(super) fn emit_alu(inst: &MInst, ctx: &mut dyn EmitContext) -> core::fmt::Result {
    alu::emit(inst, ctx)
}

pub(super) fn emit_branch(inst: &MInst, ctx: &mut dyn EmitContext) -> core::fmt::Result {
    branch::emit(inst, ctx)
}

pub(super) fn emit_memory(inst: &MInst, ctx: &mut dyn EmitContext) -> core::fmt::Result {
    memory::emit(inst, ctx)
}

pub(super) fn emit_neon(inst: &MInst, ctx: &mut dyn EmitContext) -> core::fmt::Result {
    neon::emit(inst, ctx)
}
