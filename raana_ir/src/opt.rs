//! # 优化管线：pass 调度与实现
//!
//! IR 层全部优化的家。管线顺序与调度逻辑在 [`pass`]（`PassesManager::
//! from_config`），每个优化是一个 [`pass::Pass`] 实现，放 `passes/`；
//! 跨 pass 复用的**分析**放 `analysis_passes/`。
//!
//! ## 目录划分
//!
//! - [`pass`]：`Pass` trait、`ArenaContext`/`ArenaContextMut`（pass 读写 IR
//!   的句柄）、`PassesManager`（管线装配与固定点循环）。
//! - `passes/`（31 个模块）：具体优化。例：`simplify_cfg`、`licm`、`gvn`、
//!   `loop_unroll`、`rotate_loops`、`ipsccp`、`inline`、`tco`、
//!   `pointer_strength_reduction`、`chain_to_switch`（仅 AArch64）、
//!   `matmul_interchange`、`recursive_memoize` 等。
//! - `analysis_passes/`（11 个）：分析。例：`cfg`、`dom_tree`、`loop_analysis`、
//!   `induction_variable`、`effects`、`memory`、`pure_function`、`range`、
//!   `call_graph`、`icfg`、`return_summary`。分析是**快照**，IR 修改后必须
//!   重建。
//! - [`config`]：`PassesConfig`（优化级别/开关）、`TargetPolicy`（目标门控）。
//! - [`stats`]：pass 运行统计。
//! - [`utils`]：共享工具。
//!
//! ## 管线结构（from_config 速览）
//!
//! 缩写展开：**GSP**=标量全局提升（scalar global promotion）、**PSR**=指针
//! 强度削减（pointer strength reduction）、**SR**=强度削减（strength
//! reduction）、**TCO**=尾调用优化、**IPSCCP**=过程间稀疏条件常量传播、
//! **GVNPRE**=基于 GVN 的部分冗余消除。
//!
//! 初始阶段：SSA → Specialize → Inline → TCO → ColumnMajor → GSP；
//! 固定点内循环：IPSCCP, SimplifyCFG, LoopUnroll, RotateLoops, ZeroStoreLoop,
//! ChainToSwitch, LICM, GVN, PSR, SR, InvariantReductionHoisting,
//! ReductionUnroll, IfConversion, TCO, TailRecursiveInline,
//! BooleanSimplification, GVNPRE, DeadPhiElim, DCE。
//! 另有 DSE、GuardElimination、ModFold、MulmodRecognize（AArch64）、
//! RecursiveMemoize（M68）、BlockedReduction、MatmulInterchange（AArch64）、
//! DeadFunctionElimination 等按配置注册——**此处仅列主要 pass，完整顺序以
//! `pass.rs::from_config` 为准**。
//!
//! ## 如何加一个 pass（详细）
//!
//! 1. 在 `passes/` 新建 `my_pass.rs`，实现 [`pass::Pass`]——`Pass` trait
//!    签名（pass.rs:90）：`fn run(&mut self, program: &mut Program) -> bool`
//!    （按函数分发的默认实现）+ `fn run_on(&mut self, data: &mut
//!    ArenaContextMut) -> bool`。**新 pass 通常实现 `run_on`，且必须如实
//!    返回是否改变了 IR**——返回值是固定点收敛信号，恒返回 true 会在
//!    `MAX_PIPELINE_ITERATIONS = 100` 轮后 panic；新分析放 `analysis_passes/`；
//! 2. 在 `passes.rs`（扁平文件）注册模块；在 `pass.rs::from_config` 按优化
//!    级别挂进管线（固定点内：迭代到不动点；固定点外：只跑一遍）;
//! 3. **目标相关 pass 必须用 `TargetPolicy` 门控**（如
//!    `enable_chain_to_switch`），否则 RISC-V 回归；
//! 4. inline 单测 + `cargo test -p raana_ir` + `make test ARGS="-O 2"`
//!    + `make test-riscv`（性能改动门禁，见 AGENTS.md）。
//!
//! 最小实现骨架：
//!
//! ```rust,ignore
//! pub struct MyPass;
//! impl Pass for MyPass {
//!     fn run_on(&mut self, ctx: &mut ArenaContextMut) -> bool {
//!         // 通过 ctx.program / ctx.curr_func 改 IR；返回是否真的改了
//!         false
//!     }
//! }
//! ```

mod analysis_passes;
pub mod config;
pub mod pass;
mod passes;
pub mod stats;
pub mod utils;

// Re-exported so out-of-tree emitters (e.g. the LLVM writer) can recognize the
// compiler-provided `soyo_mulmod` modmul builtin without depending on the
// (private) pass module layout.
pub use passes::mulmod_recognize::MULMOD_HELPER;
// Re-exported for the AArch64 backend, which lowers calls to the M68
// `soyo_calloc` runtime cache allocator into embedded assembly.
pub use passes::recursive_memoize::CALLOO_NAME;

/// Opt crate prelude
pub mod prelude {
    // IR object.
    pub use crate::ir::Program;
    pub use crate::ir::Type;
    pub use crate::ir::arena::Arena;
    pub use crate::ir::builder_trait::*;
    pub use crate::ir::{BasicBlock, basic_block::BasicBlockData};
    pub use crate::ir::{BinaryOp, Inst, InstData, InstKind};
    pub use crate::ir::{Function, FunctionData};

    // Common Data Structure.
    pub use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
    pub use std::collections::VecDeque;

    pub use log::{debug, error, info, trace, warn};

    // Analysis pass
    pub use super::analysis_passes::*;
    // Pass trait object
    pub use super::pass::{ArenaContext, ArenaContextMut, Pass};
    // Pass
    pub use super::passes::*;
    // Type alias
    pub use super::utils::type_alias::*;
    // IDAllocator
    pub use super::utils::IDAllocator;
    // utils
    pub use super::utils;

    pub use utils::call::*;
    pub use utils::global_handle::*;
}
