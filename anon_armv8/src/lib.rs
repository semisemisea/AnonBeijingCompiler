//! # anon_armv8：AArch64 后端
//!
//! 把 RaanaIR（经 IR 优化后的 SSA HLIR）翻译为 GNU AArch64 汇编。目标平台为
//! 赛灵思 XCZU15EG（集成 Cortex-A53，ARMv8-A 64 位，支持 NEON 与单双精度浮点），
//! 详见 `docs/anon.md`。
//!
//! ## 术语速查（首次出现即理解）
//!
//! - **SSA**：静态单赋值——每个值只被定义一次，def-use 链直接，优化好做；
//! - **HLIR**（RaanaIR）：平台无关的高层中间表示（`raana_ir` crate），后端入口之前；
//! - **VCode**：机器指令级 IR（`taki_mir` crate），指令已选定目标形式；
//! - **MIR pass**：跑在 VCode 上的机器层优化（区别于跑在 RaanaIR 上的 IR pass）；
//! - **RA**：寄存器分配（register allocation）；**Pre-RA** = 分配之前跑，
//!   **Post-RA** = 分配之后跑；
//! - **ION**：本项目寄存器分配器（移植自 regalloc2 的回溯分配器，`taki_mir::reg_alloc::ion`）；
//! - **指令选择（lower）**：把一条 IR 指令翻译成一条（或几条）机器指令；
//! - **AAPCS64**：AArch64 的 C 调用约定（参数/返回值/栈帧规则）；
//! - **spill slot**：寄存器不够时把值暂存的内存槽；**callee-saved**：被调方
//!   负责保存的寄存器。
//!
//! ## 整体流水线
//!
//! ```text
//! SysY2026 源码
//!   → soyo_compiler（lalrpop 词法/语法）→ AST
//!   → raana_ir（SSA HLIR + IR 优化 pass）
//!   → anon_armv8::AArch64Backend::lower     ← 指令选择（本 crate 核心）
//!   → taki_mir VCode（类型化 MInst 指令流）
//!   → anon_armv8::passes::build_pipeline     ← 目标相关 MIR pass（Pre/Post-RA）
//!   → taki_mir::reg_alloc（ION 寄存器分配）
//!   → taki_mir::emit / emit_buffer           ← 汇编文本发射
//!   → GNU AArch64 汇编（gcc -march=armv8-a 汇编链接）
//! ```
//!
//! 共享的机器中间表示（VCode/MIR）、寄存器分配器与发射基础设施都在
//! [`taki_mir`]；本 crate 只持有 AArch64 特有部分：指令形式、调用约定、寄存器
//! 策略、目标相关 pass 与调度模型。
//!
//! ## 模块职责
//!
//! | 模块 | 职责 |
//! |------|------|
//! | [`abi`] | AAPCS64 调用约定：参数布局、栈帧（spill slot、callee-saved）、ABI 钩子 |
//! | [`config`] | 代码生成配置：各 MIR pass 开关、调度模型（[`AArch64SchedModel`]） |
//! | [`constants`] | 整数常量物化规划：MOVZ/MOVN/MOVK/logical immediate 的选择 |
//! | [`instructions`] | 类型化指令形式 [`instructions::MInst`] 与编码合法操作数（ALU/内存/向量/分支） |
//! | [`labels`] | 汇编标签：block/函数/全局量/内嵌符号 |
//! | [`lower`] | 指令选择：Raana HIR → VCode，入口 [`AArch64Backend`] |
//! | [`passes`] | 目标相关 MIR pass：chain fusion（链式 compare 折叠）、const CSE（常量物化去重+循环外提）、DCE、pair combine（load/store 合成 LDP/STP）、peephole（指令融合）、list scheduler（A53 顺序调度） |
//! | [`regs`] | AArch64 物理寄存器与分配策略：Gpr/Vector 类、scratch 寄存器、FP/LR |
//! | [`runtime`] | 内嵌汇编符号（memset/calloc）与汇编片段 |
//! | [`sched`] | Cortex-A53 调度模型：依赖图、延迟表（供 list scheduler 使用） |
//!
//! ## 入口调用链（一次编译的旅程）
//!
//! 1. **lower**：[`AArch64Backend`] 实现 [`taki_mir::lower::LowerBackend`]，
//!    `lower` 方法把每条 HIR 指令选择为 [`instructions::MInst`]，
//!    `lower_branch` 处理跳转/分支 terminator；ABI 相关由 [`abi::AArch64Abi`]
//!    决定（参数进哪些寄存器/栈、返回值如何传递）。
//! 2. **MIR passes**：[`passes::build_pipeline`] 按 [`AArch64CodegenConfig`]
//!    组装 Pre-RA pass（peephole 融合、pair 融合、chain fusion、const CSE、DCE）
//!    与 Post-RA pass（[`sched`] 驱动的 list scheduler）。
//! 3. **寄存器分配**：taki_mir 的 ION 分配器把虚拟寄存器映射到 [`regs`] 定义的
//!    物理寄存器；溢出槽由 [`abi`] 的栈帧布局决定。
//! 4. **发射**：每条 [`instructions::MInst`] 通过 `MachInstEmit` 写入 `AsmWriter`（emit_buffer），
//!    最终输出汇编文本。
//!
//! ## 最小走查（一条语句的旅程）
//!
//! 以 SysY `a = b + 1;` 为例（a/b 为 int）：
//!
//! ```text
//! IR（raana_ir）   %r1 = add %b, 1        ← Binary(Add)
//! lower 选择       AluRRR { op: Add, dst: v0, lhs: v1(b), rhs: Imm12(1) }
//! 寄存器分配后      x1 存 b，v0 → x0
//! emit 输出        add w0, w1, #1
//! ```
//!
//! 读懂 lower 的路径：先看 `lower.rs` 的 `lower`（按 `InstKind` 分派）→
//! 进入 `lower_binary`（标量/向量分派）→ 找到 `AluRRR` 的 emit（`instructions.rs`）。
//!
//! ## 扩展指引
//!
//! - 加一条新指令：在 [`instructions`] 定义类型化形式 → 在 [`lower`] 的
//!   `lower`/`lower_branch` 里选择它 → 实现 `MachInstEmit`（打印汇编）→ 如需
//!   调度信息更新 [`sched`] 的延迟表。**验证**：`cargo test -p anon_armv8`
//!   （emit 单测在 instructions.rs 内联）+ `target/debug/compiler -S -O 2
//!   --target aarch64 -o /tmp/out.s tests/perf/xxx.sy` 目检 `.s`。
//! - 加一个目标相关优化 pass：在 [`passes`] 新建模块并接入
//!   `build_pipeline`，用 [`AArch64CodegenConfig`] 加开关。**验证**：
//!   `make test ARGS="-O 2"` 且 **`make test-riscv ARGS="-O 2"`**（RISC-V
//!   回归是真实踩过的坑）。
//! - 改调用约定/栈帧：主要改动集中在 [`abi`]，但**需检查** [`regs`] 的寄存器
//!   保留假设（scratch/FP/LR）与 [`lower`] 中的调用展开代码；跨平台改动需
//!   同步 `uika_riscv` 的对应 abi 模块。
//!
//! ## 红线
//!
//! 禁止做以 benchmark/函数名/输入为条件的优化（`docs/Illegal_optimization.md`）；
//! 性能改动门禁见 `AGENTS.md`（cargo test → make test -O2 → make test-riscv
//! → perf_compare.sh）。

pub mod abi;
pub mod config;
pub mod constants;
pub mod instructions;
pub mod labels;
pub mod lower;
pub mod passes;
pub mod regs;
pub mod runtime;
pub mod sched;

pub use config::{AArch64CodegenConfig, AArch64SchedModel};
pub use lower::AArch64Backend;
