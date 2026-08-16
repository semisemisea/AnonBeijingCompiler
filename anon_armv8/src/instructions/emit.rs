//! MInst emission dispatch split by instruction group.

mod alu;
mod branch;
mod memory;
mod neon;

pub(super) use alu::emit as emit_alu;
pub(super) use branch::emit as emit_branch;
pub(super) use memory::emit as emit_memory;
pub(super) use neon::emit as emit_neon;
