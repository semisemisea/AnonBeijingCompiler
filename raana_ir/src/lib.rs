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
//! ## 术语速查
//!
//! - **SSA**（静态单赋值）：每个值只定义一次，def-use 链直接，数据流分析
//!   （GVN/IPSCCP 等）变简单；
//! - **Phi**：合并多个汇入路径的值的伪指令——本实现里 Phi 即 **block 参数**
//!   （跳转时随参数传值）；
//! - **固定点（不动点）**：反复跑整条管线，直到一轮内没有任何 pass 再改变 IR；
//! - **支配（dominate）**：CFG 上所有到达某点的路径都经过的节点；
//! - **IV**（induction variable）：循环中每轮按固定步长变化的变量。
//!
//! ## 为什么 arena + SSA（动机）
//!
//! arena + 下标句柄：pass 增删指令不失效他人引用、无借用冲突、句柄可廉价
//! 复制、快照分析可整体重建。SSA：def-use 唯一，GVN/IPSCCP 等分析直接
//! 遍历 use 链即可。
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
//!
//! ## 下游衔接
//!
//! ```text
//! Program → anon_armv8::AArch64Backend::lower（或 uika_riscv 的 lower）
//!         → taki_mir VCode → 寄存器分配 → 汇编
//! ```
//!
//! 注意 [`opt`] 里两个 re-export 是给后端的**协议符号**：`MULMOD_HELPER`
//! （LLVM writer 识别 `soyo_mulmod` 内建）、`CALLOO_NAME`（AArch64 后端把
//! M68 记忆化的 `soyo_calloc` 调用降成内嵌汇编）——**新增此类编译器内建
//! 函数必须同步 re-export**。

#![allow(clippy::new_without_default)]
pub mod fmt;
pub mod ir;
pub mod llvm;
pub mod opt;
