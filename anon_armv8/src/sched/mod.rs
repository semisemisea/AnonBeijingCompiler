//! Cortex-A53 instruction scheduling support.
//!
//! Provides the dependency model, latency table, and DAG builder that the
//! post-RA list scheduler (`passes::list_scheduler`) uses to reorder
//! instructions within basic blocks for the in-order Cortex-A53 pipeline.
//!
//! ## Target: Xilinx XCZU15EG / Cortex-A53 MPCore
//!
//! The Cortex-A53 is a dual-issue, in-order core with an 8-stage pipeline.
//! Key scheduling concerns:
//!
//! | Resource | Throughput | Latency |
//! |----------|-----------|----------|
//! | Integer ALU | 2 / cycle | 1 |
//! | Integer Mul/MAdd | 1 / cycle | 3 |
//! | Integer Div | 1 / cycle (non-pipelined) | 4–23 |
//! | L1 Load | 1 / cycle | **2** |
//! | L1 Store | 1 / cycle (shared with Load) | 1 |
//! | Branch | 1 / cycle | 1 |
//!
//! The primary optimization target is **load-use latency hiding**: a dependent
//! ALU op issued immediately after a load stalls 1 cycle. Filling that slot
//! with an independent instruction is the single biggest win on this core.

pub mod aarch53;
pub mod dag;
pub mod simulator;

pub use aarch53::{InstrProfile, SchedClass, instr_profile};
pub use dag::{DepGraph, InstDeps, MemKind, inst_deps};
