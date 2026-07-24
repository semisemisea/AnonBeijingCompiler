/*
 * Portions of this module are adapted from regalloc2 0.15.1,
 * https://github.com/bytecodealliance/regalloc2/tree/v0.15.1.
 *
 * Released under the Apache License 2.0 with LLVM Exception. See the
 * repository LICENSE for the full license text. Local modifications adapt
 * regalloc2's interfaces and allocation utilities to taki_mir.
 */

//! Foundations for the local port of regalloc2's Ion allocator.
//!
//! This module is intentionally not wired into the production allocator until
//! the complete Ion pipeline is available.

mod cfg;
mod domtree;
mod function;
mod postorder;

pub use cfg::{CFGInfo, CFGInfoCtx};
pub use function::DenseVRegFunction;
