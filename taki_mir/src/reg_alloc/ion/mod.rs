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
mod data_structures;
mod domtree;
mod function;
mod indexset;
mod liveranges;
mod merge;
mod postorder;
mod reg_traversal;
mod requirement;

pub use cfg::{CFGInfo, CFGInfoCtx};
pub use data_structures::{BlockParamIn, BlockParamOut, CodeRange, LiveBundle, LiveRange, Use};
pub use function::DenseVRegFunction;
pub use indexset::IndexSet;
pub use liveranges::{Liveness, SpillWeight, build_live_ranges, compute_liveness};
pub use merge::{BundleSet, merge_vreg_bundles};
pub use reg_traversal::RegTraversalIter;
pub use requirement::{Requirement, RequirementConflict, RequirementConflictAt};
