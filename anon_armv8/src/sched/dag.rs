//! Per-block dependency DAG for post-RA instruction scheduling.
//!
//! After `finalize_for_emission`, all register fields hold physical registers.
//! The DAG builder inspects instruction fields directly (not via `get_operands`,
//! which skips physical-register operands post-RA) to build RAW / WAW / WAR
//! edges plus conservative memory-dependency edges.

use rustc_hash::FxHashMap;

use taki_mir::reg_alloc::reg::PReg;
use taki_mir::register::Reg;

use crate::instructions::{AMode, AluOp, MInst, PairAMode};
use crate::labels::Label;
use crate::regs::{FP, OperandSize, RegOrZr, int_preg, stack_preg};
use crate::sched::aarch53::{InstrProfile, SchedClass, instr_profile};

mod graph;
mod inst_deps;
mod memory;

)

#[cfg(test)]
mod tests;
