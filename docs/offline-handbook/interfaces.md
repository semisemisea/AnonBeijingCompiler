# 接口加在哪：新功能的落点判断方法

> 离线工作手册 G5。本文回答一个问题：拿到一个新需求（新优化 / 新指令 / 新语法 /
> 新 ABI 规则），代码应该加在**哪一层、哪个文件**。先看总览，再走决策树。

## 0. 分层总览（先建立坐标系）

```
SysY2026 源码
  ├─ soyo_compiler（前端）：sysy.lalrpop 语法 → AST → RaanaIR 下降
  ├─ raana_ir（IR + 优化）：
  │  ├─ ir/        IR 定义（Program/Function/Inst/arena）
  │  ├─ opt/passes.rs     优化 pass 注册（31 个模块，约 33 个 pass 实例；chain_to_switch 等按 TargetPolicy 门控）
  │  ├─ opt/passes/       pass 实现（dce.rs 一个文件含多个 pass）
  │  ├─ opt/analysis_passes/ 分析（11 个，快照）
  │  ├─ opt/pass.rs        管线调度（PassesManager::from_config）
  ├─ taki_mir（共享后端基础设施）：
  │    vcode.rs   机器 IR（VCodeContainer<I>，指令泛型 I: VCodeInst）
  │    reg_alloc/ ION 寄存器分配（ion::run）
  │    emit.rs / emit_buffer.rs   汇编发射
  │    abi.rs     ABI trait（ABIMachineSpec，后端实现）
  ├─ anon_armv8（AArch64 后端）：
  │    lower.rs   指令选择（AArch64Backend: LowerBackend）
  │    instructions.rs  指令形式（MInst 枚举）
  │    abi.rs     AAPCS64 实现（AArch64Abi）
  │    regs.rs    物理寄存器策略
  │    passes/    目标相关 MIR pass（build_pipeline）
  │    sched/     Cortex-A53 调度模型
  └─ uika_riscv（RISC-V 后端）：只有 abi/instructions/labels/lib/lower/regs
       六个文件——**没有 passes/、sched/、config.rs、constants.rs**（RISC-V
       当前无 MIR pass 层与调度模型）
```

**判断口诀**：目标无关 → raana_ir；目标相关 → 对应后端；两个后端共用 → taki_mir；
改约定要同时看两个后端。

## 1. 泛型/接口热点（trait 加在哪）

| 泛型/接口 | 定义处 | 谁实现 | 何时需要动它 |
|-----------|--------|--------|--------------|
| `taki_mir::vcode::MachInst` | taki_mir | 每个后端的指令类型（`MInst`/`RvInst`） | **加新指令**时必实现：`get_operands`（上报操作数给分配器）、`is_move`、`is_term`、`rc_for_type`（类型→寄存器类）、`gen_jump` |
| `MachInstEmit` | taki_mir | 同上 | 加新指令时实现 `emit`（打印汇编文本） |
| `taki_mir::lower::LowerBackend` | taki_mir | `AArch64Backend`/RISC-V 后端 | **加新 IR 指令**或改指令选择时 |
| `taki_mir::abi::ABIMachineSpec` | taki_mir | `AArch64Abi` | **改调用约定/栈帧/参数布局**时 |
| `taki_mir::reg_alloc::function::Function` | taki_mir | VCode（已实现，一般不用动） | 基本不动 |
| `raana_ir::opt::pass::Pass` | raana_ir | 每个优化 pass | **加新优化**时实现 `run_on`（trait 提供按函数分发的 `run` 默认实现） |
| `raana_ir::ir::arena::Arena` | raana_ir | `ArenaContext`/`ArenaContextMut` | 基本不动 |
| `anon_armv8::regs` 工厂函数 | anon_armv8 | — | 改寄存器分配策略（MachineEnv）时 |

**通用判断**：要接入"寄存器分配/发射/pass 基础设施"的东西，就在 taki_mir 定义
trait、后端实现；要接入"IR 优化管线"的东西，就在 raana_ir 实现 `Pass`。

## 2. 决策树（新需求 → 落点）

```
新需求来了，按顺序问自己：
│
├─ Q1: 涉及新语法/新关键字/新内建函数？
│    → soyo_compiler/src/sysy.lalrpop（语法规则）
│    → 前端 AST 结构 → 下降到 RaanaIR（新增 InstKind 或复用现有）
│    └─ 若新增 InstKind：还要 lower（Q4 路径）+ 后端指令（Q5 路径）
│
├─ Q2: 是 IR 层优化（目标无关，如新 LICM 变体）？
│    → raana_ir/src/opt/passes/ 新建 my_pass.rs 实现 Pass
│    → raana_ir/src/opt/passes.rs（**扁平文件**，非目录 mod.rs）加一行
│      `pub mod my_pass;`
│    → pass.rs::from_config 挂进管线
│    → 需要分析先看 analysis_passes/ 有没有现成的（快照！）
│
├─ Q3: 是目标相关优化（只对 AArch64 生效）？
│    ├─ 作用在 IR 层 → raana_ir 实现 Pass + TargetPolicy 门控
│    │    （例：chain_to_switch 用 enable_chain_to_switch）
│    └─ 作用在 MIR 层 → anon_armv8/src/passes/ 新建 pass
│         → passes/mod.rs 的 build_pipeline 接入
│         → config.rs 的 AArch64CodegenConfig 加开关
│
├─ Q4: 需要新的 HIR→机器指令映射（新 IR 指令/新 lowering）？
│    → anon_armv8/src/lower.rs：AArch64Backend::lower / lower_branch
│    │    的 match 里加分支
│    └─ 若两个后端都需要 → 考虑 taki_mir/src/lower.rs 公共 lowering 工具
│
├─ Q5: 需要新机器指令（新汇编指令/新寻址模式）？
│    → anon_armv8/src/instructions.rs：MInst 枚举加变体 + 操作数类型
│    → 实现 MachInst（get_operands/is_move/is_term/rc_for_type/gen_jump）
│    → 实现 MachInstEmit（打印汇编）
│    → lower.rs 里选择它
│    → 需要调度 → sched/ 延迟表
│
├─ Q6: 改调用约定/参数传递/栈帧布局？
│    → anon_armv8/src/abi.rs（AArch64Abi 实现 ABIMachineSpec）
│    │    └─ RISC-V 同步 → uika_riscv 的 abi
│    └─ 栈帧通用逻辑 → taki_mir/src/abi.rs（FrameLayout）
│
├─ Q7: 改寄存器分配策略（哪些寄存器可分配/scratch 分配）？
│    → anon_armv8/src/regs.rs（MachineEnv、scratch 常量）
│    └─ 分配器本身 → taki_mir/src/reg_alloc/（一般不动）
│
├─ Q8: 改汇编输出格式/分支优化？
│    → taki_mir/src/emit.rs（AsmWriter）
│    → taki_mir/src/emit_buffer.rs（分支优化/veneer）
│
└─ Q9: 改测试/验证流程？
    → tests/test.py + Makefile（详见 test-harness.md）
```

## 3. 两条完整示例（照着抄）

### 示例 A：加一条新指令（如 `fmls` 向量乘减）

1. `anon_armv8/src/instructions.rs`：`MInst` 加 `Fmls { rd, rn, rm }` 变体
   （参考现有 `VecFmla`，instructions.rs:737）；
2. 实现 `MachInst`：`get_operands` 上报三个操作数（分配器需要知道）；
   `is_move`→None；`is_term`→None；`rc_for_type`→Vector 类；`gen_jump`
   →unreachable；
3. 实现 `MachInstEmit`：`emit` 里写 `fmls v{d}, v{n}, v{m}`；
4. `lower.rs`：在 `lower_binary`/`lower_fma` 的 match 里，当 op 匹配且目标
   f32 向量时产出 `VecFmls` 而不是 `VecFmla`+`Neg`；
5. `sched/`：`dag.rs` 加 `Fmls` 分支（定 SchedClass），`aarch53.rs` 的
   `instr_profile` 设延迟（数值表在这里，`dag.rs` 只按 SchedClass 建边）；
6. 验证：`cargo test -p taki_mir -p anon_armv8` + 单 case 差分
   （`make test functional/xxx.sy ARGS="-O 2"`）。

### 示例 B：加一个 IR 优化 pass（如"冗余 load 消除"）

1. `raana_ir/src/opt/passes/` 新建 `redundant_load_elim.rs`：
   `pub struct RedundantLoadElim;` 实现 `Pass`。`Pass` trait（pass.rs:90）
   签名是 `fn run(&mut self, program: &mut Program) -> bool`（按函数分发的
   默认实现）+ `fn run_on(&mut self, data: &mut ArenaContextMut) -> bool`——
   **新 pass 通常实现 `run_on`**，返回是否改变了 IR（固定点收敛信号）；
2. 需要内存分析 → 复用 `analysis_passes::memory`（注意它是快照，改 IR 后
   要重建）；
3. `raana_ir/src/opt/passes.rs`（扁平文件）加 `pub mod redundant_load_elim;`；
4. `pass.rs::from_config`：挂到固定点内（若需要迭代到不动点）或固定点外；
5. 目标无关 → 不需要 TargetPolicy；若只想 AArch64 生效则加门控字段；
6. 验证：inline 单测 → `cargo test -p raana_ir` → `make test ARGS="-O 2"`
   → `make test-riscv`（性能改动门禁，见 AGENTS.md）。

## 4. 常见坑（判断时先排除）

- **目标相关 pass 忘了门控** → RISC-V 回归（Q3 红色警告）；
- **分析快照过期**：CFG/支配/循环/IV 分析在 IR 修改后必须重建；
- **block 参数错位**：Phi 是 block 参数，`BlockArgRef` 位置必须匹配目标块
  `params()` 切片；
- **只改后端不查 IR**：语义改动可能更适合在 IR 层做（更通用）；
- **新 crate 依赖**：`dependencies/` 是 vendored 目录，新依赖必须先 vendor
  （`.cargo/config.toml`），离线环境尤其注意；
- **新增 `InstKind` 的跨层改动面**：除 lower（Q4/Q5）外，还要同步 IR dump
  （`raana_ir/src/fmt/`）与 LLVM 导出（`raana_ir/src/llvm/`），漏一处后端
  会 panic；
- **测试 harness 默认 -O0**：性能改动必须显式 `make test ARGS="-O 2"`；
  每用例编译两次（`--emit ir` dump 崩溃显示为 CE）；残留容器 `soyo-test`
  会导致跑错用例集（详见 test-harness.md）。

## 5. 与 rustdoc 的关系

本文件第 1 节的 trait 方法清单与 `taki_mir/src/vcode.rs` 的 rustdoc 表格
高度重复——**以 rustdoc 为准**（`cargo doc` 可查），本文件只保留"改什么
看哪"的落点列，trait 改方法名时只改 rustdoc 一处。
