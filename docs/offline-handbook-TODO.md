# Offline Handbook TODO（无 AI / 无网络工作手册）

独立于仓库根 TODO.md（那是编译器优化任务跟踪）。本文件只跟踪"离线工作手册"文档任务。

## 背景

即将在无 AI、无网络环境下工作（比赛现场/评审）。需要一整套自包含文档，让编译小白
能快速上手、普通人能维护本项目（SysY → AArch64/RISC-V 编译器，Rust，CSC 编译系统
设计赛）。

## 文档形式规则（硬性）

- **Workspace 内代码**（anon_armv8 / taki_mir / raana_ir / soyo_compiler /
  tomori_utils / uika_riscv 的 .rs 文件）→ 写 **rustdoc 注释**（`//!` 模块级、
  `///` 项级），保证 `cargo doc` 可生成。不新建重复 md。
- **Workspace 外的库**（如 lalrpop）与**脚本/方法论**（test.py、接口加在哪的判断
  方法、优化路线图）→ 独立 md，放 `docs/offline-handbook/`。
- 已存在的 `docs/anon.md` / `docs/soyo.md` / `docs/uika.md` 是设计文档，可提炼进
  rustdoc，但**不删除**。

## 评级机制（每一轮 turns 必做）

每个 goal 的**每一轮 turns 结束**，必须 `delegate_task` 启用一个子代理评级。
评级维度（二选一输出 1-5 分 + 具体改进建议）：
1. **编译小白能否快速上手**：术语是否解释、示例是否充分、路径/入口是否明确。
2. **普通人能否维护**：修改点是否清晰、副作用是否说明、是否依赖隐性知识。

子代理评级输入：该 goal 产出的文档全文 + 相关源码路径。输出追加到本文件
"评级记录"节。按建议修改后进入下一轮。

## 特殊要求（硬性）

- 每个 goal **至少需要两轮 turns 才可被判定完成**；第一轮 turns 结束时**不许**判完成。
- 每 goal 独立 commit，中文 message，`[Docs]:` 前缀，原子可验证。
- 纯文档任务：不碰代码逻辑（不改 .rs 行为），不 push，不碰 .docker-image。
- 全部完成前 docs/offline-handbook 分支不合并。

## Goal 列表

### G1 后端整体流程 —— anon_armv8 crate rustdoc
- 范围：`anon_armv8/src/lib.rs`（crate 级 `//!`）+ 各顶层模块 `//!` 概述。
- 内容：整体流水线（SysY → RaanaIR → taki_mir VCode/MIR → 寄存器分配 → AAPCS64
  帧 → GNU AArch64 汇编）、模块职责图（abi/config/constants/instructions/labels/
  lower/passes/regs/runtime/sched）、后端入口 `AArch64Backend` 调用链
  （lower → vcode → regalloc → emit）。
- 参考：`docs/anon.md` 第一、三节；`anon_armv8/src/lower.rs` 的
  `AArch64Backend`。
- 验收：`cargo doc -p anon_armv8` 无 warning；小白读完能复述后端 5 个阶段与
  入口调用链。
- 状态：`[ ]` 待执行（0 轮）

### G2 寄存器分配 —— taki_mir::reg_alloc (ion) rustdoc
- 范围：`taki_mir/src/reg_alloc/` 全部 13 文件（domtree/function/indexset/
  liveranges/merge/mod/moves/postorder/process/redundant_moves/reg_traversal/
  requirement/spill）。
- 内容：ION 算法整体流程（liveranges 构造 → merge → process → moves → spill）、
  `Function` 数据结构、每个子模块职责一句话、与 vcode 的接口
  （`allocate_registers` 入口签名与调用方）。
- 验收：`cargo doc -p taki_mir` 生成；小白能讲清"虚拟寄存器 → 物理寄存器"的
  完整路径。
- 状态：`[ ]` 待执行（0 轮）

### G3 vcode —— taki_mir::vcode rustdoc
- 范围：`taki_mir/src/vcode.rs`（MachInst/VCode/MachInstEmit）。
- 内容：VCode 数据结构（block/inst/operand 关系）、`MachInstEmit` trait、
  `MachInst` 表示法、"如何加一条新指令"的伪代码步骤（define inst → lowering →
  emit → regalloc 兼容性）。
- 验收：cargo doc 生成；小白照文档能加一条新指令。
- 状态：`[ ]` 待执行（0 轮）

### G4 emit —— taki_mir::emit + emit_buffer rustdoc
- 范围：`taki_mir/src/emit.rs`、`taki_mir/src/emit_buffer.rs`。
- 内容：`AsmWriter` 接口、emit_buffer 缓冲/刷出机制、指令 emit 流程、标签/缩进/
  对齐处理。
- 验收：cargo doc 生成；能讲清一条指令从 MachInst 到汇编文本的路径。
- 状态：`[ ]` 待执行（0 轮）

### G5 泛型与接口加在哪 —— 方法论 md + 相关 rustdoc
- 范围：新建 `docs/offline-handbook/interfaces.md` + 泛型热点文件的 rustdoc 补注
  （`taki_mir/src/register.rs` 的 RegClass、`anon_armv8/src/instructions.rs`、
  `anon_armv8/src/regs.rs` 的 Vector 类体系）。
- 内容：项目泛型分布总览；"新功能接口加在哪一层"的决策树（IR pass？MIR pass？
  lower？emit？）——每条路径的判定问题 + 示例（如：新优化→raana_ir opt/pass.rs；
  新指令→anon_armv8 instructions.rs + lower.rs；新 ABI 规则→abi.rs）。
- 验收：小白拿到新需求能按决策树定位到具体文件。
- 状态：`[ ]` 待执行（0 轮）

### G6 raana_ir rustdoc
- 范围：`raana_ir/src/lib.rs`（crate 级）+ `ir` / `opt` 模块。
- 内容：`ir`：Program/Function/Block/Inst 体系、Arena 结构；`opt`：`pass.rs`
  管线顺序与 `PassesManager::from_config`、passes/ 目录分类、analysis_passes、
  "如何加一个 pass"（注册 → config 门控 → 单测 → TargetPolicy 注意）。
- 参考：`docs/Convention.md`、`AGENTS.md` 的 IR 架构要点。
- 验收：cargo doc 生成；小白能加一个新 pass 并接入管线。
- 状态：`[ ]` 待执行（0 轮）

### G7 lalrpop 指南 —— 独立 md（外部库）
- 范围：新建 `docs/offline-handbook/lalrpop.md`。
- 内容：lalrpop 语法速查（token 定义、规则、优先级、错误恢复）、
  `soyo_compiler/src/sysy.lalrpop`（292 行）结构逐节讲解、"如何加一条语法规则"
  （.lalrpop → AST → RaanaIR 下降的完整接线）。
- 验收：小白能加一个新关键字/新语法并跑通。
- 状态：`[ ]` 待执行（0 轮）

### G8 测试 harness —— 独立 md
- 范围：新建 `docs/offline-handbook/test-harness.md`。
- 内容：`tests/test.py`（855 行）流程：每个用例编译两次（-S 汇编 + --emit ir）、
  combined_output（stdout+换行+returncode）比对、`results/` 产物布局、
  make test 各目标（test/test-riscv/test-llvm/test-baseline/run-elf/mca/gem5）、
  如何加测试用例、失败类型（WA/CE/RE/TLE）与排查路径。
- 参考：AGENTS.md 测试 harness 节。
- 验收：小白能加一个用例并解释三种失败类型的含义。
- 状态：`[ ]` 待执行（0 轮）

### G9 向量化可能的运算方法 —— rustdoc + md
- 范围：`anon_armv8/src/instructions.rs`（Vec* 指令族）、`anon_armv8/src/lower.rs`
  向量 lowering 路径、`anon_armv8/src/passes/`；新建
  `docs/offline-handbook/vectorization.md`。
- 内容：现有 SIMD 能力盘点（已支持指令/模式）；**未实现但可行的运算方法清单**：
  每条给原理、改哪层、伪代码/IR 形态、预期解锁用例（如 f32 fmls、整数 mla v.4s、
  VecSub 与 fmls 融合、掩码 select v3 等——参考 skill 的 SIMD gap 分析）。
- 验收：小白能挑一条清单项直接开工实现。
- 状态：`[ ]` 待执行（0 轮）

### G10 可能的优化与实现路线 —— 独立 md
- 范围：新建 `docs/offline-handbook/future-optimizations.md`。
- 内容：从 AGENTS.md / docs/ 提炼**未完成优化**路线图，每个优化一项：
  原理（IR/汇编示例）、改哪层、实现步骤（原子拆分）、预期收益与验证方法。
  候选方向：SIMD Phase 2（M42-M46 搁置项）、常量物化（clang 差距 P1）、
  PSR 扩展、loop interchange 等。
- 验收：每个方向都有可执行的实现步骤。
- 状态：`[ ]` 待执行（0 轮）

## 执行顺序建议

G1 → G2 → G3 → G4 → G6 → G5 → G7 → G8 → G9 → G10
（先啃后端核心 rustdoc，再方法论 md；G5 依赖 G1-G4 的术语沉淀）

## 评级记录

（每轮 turns 结束后子代理评级结果追加于此）
