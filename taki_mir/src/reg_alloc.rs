//! # taki_mir 寄存器分配子系统
//!
//! 把 VCode 里的**虚拟寄存器**（VReg，数量无上限）映射到目标机器的**物理
//! 寄存器**（PReg，数量固定），必要时把装不下的值**溢出**（spill）到栈槽。
//! AArch64 与 RISC-V 后端共用这套分配器，与目标无关的部分都在本模块。
//!
//! ## 子模块
//!
//! | 模块 | 职责 |
//! |------|------|
//! | [`function`] | [`function::Function`] trait：客户端（VCode）必须实现的函数视图，分配器只通过它读取程序 |
//! | [`index`] | `define_index!` 宏生成的稠密下标类型（VRegIndex/Block/Inst/InstRange 等） |
//! | [`reg`] | 寄存器模型：[`reg::RegClass`]（Int/Float/Vector）、[`reg::PReg`]（物理寄存器）、[`reg::VReg`]（虚拟寄存器）、[`reg::MachineEnv`]（机器环境）、[`reg::Output`]（分配结果） |
//! | [`ion`] | ION 回溯分配器（移植自 regalloc2）：核心分配算法，入口 [`ion::run`] |
//! | [`moves`] | `ParallelMoves` 并行移动工具（真正的 Output 回写发生在 `taki_mir/src/lib.rs` 编译流程） |
//!
//! ## 两个核心概念（先理解再读流程）
//!
//! - **LiveRange（活跃区间）**：一个值从定义点覆盖到最后一次使用的连续指令
//!   区间；
//! - **bundle**：同一 VReg 的多个 LiveRange 合并后的集合，是分配博弈的**原子
//!   单位**（权重、驱逐、溢出都以 bundle 计）。
//!
//! ## 一次分配的数据流
//!
//! ```text
//! VCode（虚拟寄存器指令流，实现 Function trait）
//!   → ion::run(func, machine_env)
//!       ├─ 归一化：客户端 VReg → 稠密 VReg（DenseVRegFunction）
//!       ├─ liveness：计算每个 VReg 的活跃区间（LiveRange）并合并成 bundle
//!       ├─ 主循环：按溢出权重处理每个 bundle，尝试分配寄存器；冲突则
//!       │   驱逐/分裂/溢出（process_bundles）
//!       ├─ 解析：为 block 参数和跨边界的值插入 move（move resolution）
//!       └─ 溢出：为溢出的 bundle 分配栈槽（spillslot allocation）
//!   → Output（每个 VReg 的物理寄存器/栈槽 + 插入的 move 指令）
//!   → 回写进 VCode，供后续 emit 阶段使用
//! ```
//!
//! 主调用点在 `taki_mir/src/lib.rs`（`compile` 流程）：
//! `ion::run(&vcode, machine_env)`。详见 [`ion`] 模块文档。

pub mod function;
pub mod index;
pub mod ion;
pub mod moves;
pub mod reg;

pub use reg::Output;
