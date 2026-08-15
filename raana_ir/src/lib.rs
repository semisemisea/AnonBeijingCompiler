//! # raana_ir：SSA 形式的高层中间表示（HLIR）与优化管线
//!
//! 前端（soyo_compiler）把 SysY2026 源码降成 [`ir::Program`]（SSA 形式），本
//! crate 在其上运行全部 **IR 层优化 pass**，然后交给后端（anon_armv8 /
//! uika_riscv）翻译为机器码。**约定**：目标无关的优化都在这里做；目标相关的
//! 优化（如 AArch64 的 chain-to-switch）必须用 [`opt::config::TargetPolicy`]
//! 门控，否则 RISC-V 会回归。
//!
//! ## 模块
//!
//! | 模块 | 职责 |
//! |------|------|
//! | [`ir`] | IR 定义：Program/Function/BasicBlock/Inst 体系、Arena 分配、builder 接口 |
//! | [`opt`] | 优化管线：pass 注册与调度（[`opt::pass`]）、pass 实现（`passes/`）、分析 pass（`analysis_passes/`）、配置（[`opt::config`]） |
//! | [`fmt`] | IR dump（`--emit ir` 输出文本 IR，调试用） |
//! | [`llvm`] | LLVM IR 导出（`LlvmWriter`，对照 clang 输出用） |
//!
//! ## IR 设计要点（详见 [`ir`] 模块文档）
//!
//! - **Arena 分配**：指令/块/函数全部放在 arena 里，用**下标索引**（`Inst`/
//!   `Function`/`BasicBlock` 都是 newtype 索引）引用，不用指针；
//! - **SSA + block 参数**：Phi 是 block 参数（`BlockArgRef`），不变量是
//!   引用位置必须匹配目标块的 `params()` 切片；
//! - **快照分析**：CFG/支配/循环/IV 分析是快照，任何修改都会使其失效，
//!   改完必须重建（见 `docs/Convention.md`）。
//!
//! ## 如何加一个 pass（快速版）
//!
//! 1. 在 `passes/`（或 `analysis_passes/`）新建模块实现 [`opt::pass::Pass`]；
//! 2. 在 `opt/pass.rs` 的 `PassesManager::from_config` 里按目标优化级别挂进
//!    管线（固定点内/外位置有讲究，见 [`opt`] 模块文档）；
//! 3. 若只对特定目标生效，用 `TargetPolicy` 门控；
//! 4. 写 inline 单测（`#[cfg(test)]`），跑 `cargo test -p raana_ir`。

#![allow(clippy::new_without_default)]
pub mod fmt;
pub mod ir;
pub mod llvm;
pub mod opt;
