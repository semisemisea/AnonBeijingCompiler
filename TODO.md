# 代码生成重构 + ARM (Cortex-A53) 指令重排优化 总体计划

本文件为本次重构的完整设计/任务清单。原始英文/思考过程见 commit history。
新完成的工作应从本文件移除并落到 commit message / 文档 / 测试中。

---

## 0. 关键发现（本计划的依据）

| 主题 | 现状 | 对本计划的影响 |
|------|------|----------------|
| 架构血统 | `taki_mir` 已经是一个 **Cranelift 风格的后端**：`VCodeContainer<I>`、`MachInst` trait、移植自 regalloc2 的 **Ion 回溯分配器**、`ABIMachineSpec`、需求驱动的反向 lowering、`LowerBackend` trait。 | "参考 Cranelift 重构" = **完成未竟的清理工作**，而不是重写。 |
| MIR Pass 基础设施 | **完全缺失**。`taki_mir::compile`（`taki_mir/src/lib.rs:192-278`）是一个 90 行的单函数流水线，所有阶段都内联。无 `raana_ir::opt::pass::Pass`（`raana_ir/src/opt/pass.rs:87`）的对应物。 | 必须先建 MIR Pass 框架，才能挂载任何新 pass。 |
| 现有 MIR 优化 | **完全没有**。ISel 期间的 folding（MAC、shifted-RHS、magic-div、branch-tree→Cbz/Tbz）都内联在 `anon_armv8/src/lower.rs` 里。`legalize_inst` 被熔进**发射阶段**（`emit.rs:193-205`），不是一个独立 pass。 | 这些必须被抽取出来。 |
| VCode 可变性 | RA 后的 `write_back_allocs` **就地改写** VCode 的 Reg 字段（`vcode.rs:138-144`），与 Cranelift 不同（Cranelift 保持 VCode 不可变，edit-list 在发射时消费）。 | 我们保留这一设计（更快）；scheduler 在 write-back 之后运行。 |
| 后端代码重复 | `anon_armv8` 和 `uika_riscv` 在 frame layout、prologue/epilogue 骨架、call clobber、div magic 上几乎零共享。 | 中等范围重构要抽取到 `taki_mir`。 |
| README 已过期 | 第 46-50 行声称分配器是 "linear one-pass scan"，实际是 regalloc2 的 Ion 回溯分配器（`reg_alloc/ion/mod.rs:1-8`）。 | 要修正。 |

**核心设计结论（来自对 Cranelift 的研究）：** Cranelift **没有**机器指令调度器——因为它的目标（x86、AArch64 大核）都是乱序执行。Cortex-A53 是 **顺序双发射**，调度收益巨大。因此本 scheduler 的参照系是 GCC/LLVM 的 list scheduler，**而非 Cranelift**；其它所有方面（VCode 形状、edit-list 发射、trait 分层）继续沿用 Cranelift 风格。

---

## 1. 目标微架构模型 — Xilinx XCZU2EG / Cortex-A53 MPCore

权威数据源：ARM Cortex-A53 Software Optimization Guide (DUI 0901)。
对 XCZU2EG 的 2 个隔离测评核（Cortex-A53 MPCore）：

| 资源 | 吞吐 | 延迟 | 备注 |
|------|------|------|------|
| 整数 ALU | 2 / 周期（ALU0, ALU1） | 1 | 大多数 ALU 指令可双发 |
| 整数乘法（32×32→64, MADD） | 1 / 周期（MAC 流水线，仅 ALU0） | 3 | `mul`/`madd` 走 ALU0 |
| 整数除法 | 1 / 周期，非流水线 | 4–11（32位）、4–23（64位） | 阻塞后续指令 |
| L1 Load | 1 / 周期（LSU） | **2**（命中 L1） | 这是 scheduler 的主要优化对象 |
| L1 Store | 1 / 周期（LSU，与 load 共享） | 1 | load/store 不能双发 |
| 浮点 / NEON | 1 / 周期（FP 流水线，仅 ALU1） | 4–6 | FP 与 ALU0 互斥 |
| 分支 | 1 / 周期（独立单元） | 1 | 与 ALU0/ALU1 正交 |
| 双发限制 | 总计 2 条指令 / 周期；配对指令必须**无真依赖**，且路由到兼容的槽位 | | 驱动 scheduler 的优先级排序 |

XCZU2EG 的 **L1D$ 32 KB 4-way、L1I$ 32 KB 2-way、L2$ 1 MB 16-way shared** 不直接驱动**指令调度**——但 L2 命中会增加约 9 周期的额外 load 延迟；我们额外暴露一个 `aarch53_l2` 模型用于过拟合实验。

这些数字固化在 `anon_armv8/src/sched/aarch53.rs` 中，作为 `InstrProfile { class, latency, throughput, dual_issue_slot }` 的查表。

---

## 2. 新流水线架构（Before / After）

**当前**（`taki_mir/src/lib.rs:205-275`）：
```
lower.lower::<B>()  ─▶  verify  ─▶  regalloc::ion::run  ─▶  write_back_allocs
   ─▶  compute_frame_layout  ─▶  AsmWriter::write_function（legalize 也在这里）
```

**改造后**——显式的 pass 流水线对象，对称于 `raana_ir`：
```
VCodeBuilder::build
   ─▶ verify("post-lowering")
   ─▶ ┌─ MIR PASSES (pre-RA) ─────────────────────────────┐
   │  • PeepholeCombine     （anon_armv8：MAC/shifted-RHS/LDP）│
   │  • （未来 pre-RA passes）                                │
   └──────────────────────────────────────────────────────┘
   ─▶ verify("post-pre-RA-passes")
   ─▶ regalloc::ion::run  ─▶  write_back_allocs
   ─▶ ┌─ MIR PASSES (post-RA) ────────────────────────────┐
   │  • MaterializeEdits    （把 output.edits 落到 VCode）    │
   │  • LegalizeFinal       （从 emit 里抽出来）              │
   │  • ListScheduler       （anon_armv8：Cortex-A53 模型）   │
   │  • BranchPeephole      （可选，Cbz/Tbz 链折叠）          │
   └──────────────────────────────────────────────────────┘
   ─▶ verify("post-post-RA-passes")
   ─▶ compute_frame_layout
   ─▶ AsmWriter::write_function（清理后：没有内联 legalize）
```

引入一个 `MIRPassPipeline` 结构（镜像 `PassesManager`，`raana_ir/src/opt/pass.rs:112`），持有有序的 `Vec<Box<dyn MIRPass<I>>>`，按 pre-RA 和 post-RA 两段运行。Verify 在两个边界都跑一遍。

---

## 3. Phase 1 — MIR Pass 基础设施（前置依赖，~1 天）

**状态：✅ 已完成 (M1)**

**新文件：** `taki_mir/src/passes.rs`。

```rust
pub trait MIRPass<I: VCodeInst>: Send + Sync {
    fn name(&self) -> &'static str;
    fn run(&self, vcode: &mut VCodeContainer<I>, arena: ArenaContext) -> bool;
}

pub struct MIRPassPipeline<I: VCodeInst> {
    pre_ra: Vec<Box<dyn MIRPass<I>>>,
    post_ra: Vec<Box<dyn MIRPass<I>>>,
}
```

**已实现：**
- `MIRPass` trait + `MIRPassPipeline` 结构（pre-RA / post-RA 两段）
- `LowerBackend::mir_pipeline()` 关联函数，默认返回空 pipeline
- `compile()` 重构：在函数循环前构建 pipeline，在 post-lowering verify 后、regalloc 前调用 `run_pre_ra`；在 post-writeback verify 后、frame layout 前调用 `run_post_ra`
- 每个 pass 运行前后调用 `VCodeContainer::verify()` 做结构一致性检查
- 两个后端均使用默认空 pipeline（行为不变）
- 全部 200+ 测试通过；所有 functional .sy 文件在 AArch64 和 RISC-V 上均正常编译

**注意事项：**
- `verify_operand_order_stable` 暂未实现（pre-RA 阶段用 `verify()` 替代；等 M5 有真实 mutating pass 时再细化）
- post-RA 的 Reg 字段是物理寄存器，`get_operands` 的 `reg_maybe_fixed` 会走 `reg_fixed_nonallocatable` 路径（不调用 `add_operand`），所以闭包计数法在 post-RA 下不可用——M7 的 `MaterializeEdits` 需要考虑这一点

---

## 4. Phase 2 — 修复已知坏味道（~2 天）

### 4a. 把 legalization 从 emission 抽出来变成真正的 pass

目前 `AsmWriter::write_inst`（`emit.rs:193-205`）在打印期间调用 `ABIMachineSpec::legalize_inst(frame, inst)`。副作用：合法化后的指令永不物化进 VCode，所以 scheduler 看不到它们，debug dump（`lib.rs:277`）显示的是 pre-legalize 形式。

**改造：**
- 新增 `LegalizeFinal: MIRPass<I>`。在 post-RA 阶段运行。调用既有的 `legalize_inst`，但**把结果写回 VCode**。一条指令可能展开成多条（例如 AMode 物化）；`VCodeContainer` 新增 `replace_inst(at, Vec<I>)` 辅助函数。
- `AsmWriter::write_inst` 不再调用 `legalize_inst`。
- `legalize_inst` 签名变化：接收 `&FrameLayout`，返回 `SmallVec<[I; 2]>` 而不是 `(&str, I)`（某些 backend 目前返回文本片段——必须改成返回真实 inst）。

### 4b. 去重 AArch64 / RISC-V 之间重复的 ABI 代码

把两后端中**结构完全相同**的逻辑抽到 `taki_mir`（或新的 `taki_mir::abi_helpers` 子模块）：
- `compute_arg_locs` 的循环（当前每个 backend 各重复两遍：参数一遍、调用参数一遍）→ 一个泛型 `ArgLayoutPlanner { int_regs, fp_regs, stack_align }`，由 backend 提供寄存器列表。
- prologue/epilogue 骨架（prologue = 保存 frame + clobber-save；epilogue = 反向）→ `CalleeABI::gen_prologue` 中的 `gen_prologue_epilogue_skeleton()` 辅助，调用每个 backend 在 `ABIMachineSpec` 上的钩子。
- `call_clobbers`、`use_store_src` 等辅助 → 移到一个共享 `inst_common` 模块（镜像 cranelift 的 `machinst/inst_common.rs`）。
- 除法魔法数：**已经**共享（`taki_mir/src/div_magic.rs`）——很好。

### 4c. 修复 `LowerContext` 的封装泄漏

**状态：✅ 已完成 (M2)**

已实现：
- `LowerContext.vcode` 改为 `pub(crate)`，`VCodeBuilder.vcode` 改为 `pub(crate)`
- 新增 `LowerContext` 上的封装方法：`set_has_calls()`、`set_outgoing_arg_size(size)`、`arg_slot(idx)`
- 已有封装方法：`alloc_stackslot_or_get()`、`alloc_tmp()`、`emit()`、`put_value_in_reg()`、`result_reg()`
- AArch64 后端 5 处 + RISC-V 后端 11 处 `ctx.vcode.vcode.abi.*` 直接访问全部替换为封装方法
- 验证：`rg "ctx\.vcode\.vcode"` 在两个后端目录中返回零结果

### 4d. 修正 README

**状态：✅ 已完成 (M2)**

第 46-50 行改为："寄存器分配是 regalloc2 Ion 回溯分配器的移植"，并更新 §8 的 pass 列表，新增 MIR Pass pipeline 说明。

---

## 5. Phase 3 — Pre-RA PeepholeCombine Pass（~2-3 天）

**新文件：** `anon_armv8/src/passes/peephole_combine.rs`。在虚拟寄存器上工作（pre-regalloc），紧接 lowering 之后运行。

目前**内联在 `lower.rs` 中**、Phase 3 要**抽取/迁移**的 folding：

| 模式 | 当前来源 | Pre-RA 形式 |
|------|---------|-------------|
| `mul + add/sub → madd/msub` | `fold_mul_add_sub`（`lower.rs:1459`） | 匹配 `MInst::Mul` producer 紧接着 `AluRRR{Add,Sub}` consumer；发射 `MAdd`/`MSub` |
| `alu rR, rR, imm12` / `ImmShift` / `ImmLogic` 的选择 | `lower.rs:146-225` | **留在 lowering**（这是选择问题，依赖 HIR 操作数） |
| `alu + shifted-RHS` | `fold_shifted_rhs`（`lower.rs:1536`） | 匹配 `Shl`/`Lsr`/`Asr` producer；合并为 `AluRRRShift` |
| `LDP/STP 形成 | （不存在） | 匹配相邻 `Load`/`Store`、偏移为成对（±8 同 base reg）→ `LoadPair`/`StorePair`。**对 Cortex-A53 收益巨大**（单 LSU 周期）。 |
| `Cbz/Cbnz/Tbz/Tbnz` 条件树 | `select_branch_condition`（`lower.rs:969-…`） | **留在 lowering**（在 branch lowering 的入口） |
| 常量池 / `movz`+`movk` 合并 | `constants.rs::plan_integer_constant` | 已经很干净；不动 |

Peephole pass 模式：**两指令工作表扫描**。对每个 block 顺序扫描；对每个 `(producer, consumer)` 对尝试 combine 规则。Combine 后把 producer 替换为 `Nop`（随后被消除）或 `MInst::Nop` 占位，consumer 替换为合并形式。

Pre-RA combine 接口：
```rust
trait PeepholeRule<I: VCodeInst> {
    fn matches(producer: &I, consumer: &I) -> bool;
    fn apply(producer: I, consumer: I) -> SmallVec<[I; 2]>;   // 1 = 只改 consumer；2 = 两者合一
}
```
规则表位于 `anon_armv8/src/passes/peephole_rules.rs`。

---

## 6. Phase 4 — Post-RA ListScheduler Pass（核心，~4-5 天）

**新文件：** `anon_armv8/src/passes/list_scheduler.rs`，以及 `anon_armv8/src/sched/{mod.rs, dag.rs, aarch53.rs, model.rs}`。

### 6.1 工作形式

在 `write_back_allocs`（`lib.rs:239`）之后、`LegalizeFinal` 之后运行。此时：
- 所有 `Reg` 字段都是物理寄存器或 spill slot。
- `output.edits`（move 序列）**尚未**物化到 VCode。

**决定：先物化 edits，再调度。** 新增 `MaterializeEdits: MIRPass<I>` pass，把 `output.edits` 中的 `Edit::Move` 条目物化成真实的 `MInst::gen_move` 指令（或目标特定的 spill/reload 形式）插入 VCode。这之后 `AsmWriter` 的 edit 交错逻辑会变得很简单（最终可删）。同时也让 scheduler **可以隐藏 spill/reload 的延迟**——这在 Cortex-A53 上是个实在的收益。

（如果不物化，scheduler 必须调度 `InstOrEdit` 流——既丑陋又错失 spill 延迟隐藏。）

### 6.2 每个块的算法

对每个非 entry-prologue/epilogue 的 basic block：

1. **构建依赖 DAG。** 节点 = 指令索引。维护一个 last-writer map（按物理寄存器）+ last-memory-writer map（按 class）。

   依赖种类：
   - **RAW**（真依赖）：边的权重 = producer 延迟。
   - **WAW, WAR**：寄存器分配后很少见（allocator 已经基本消除），但仍以 0 延迟边加入以防万一。
   - **内存**：v1 保守处理。**v1：** 任何 `Load`/`Store` 与之前的 `Store`/`Load`/`Store` 保持顺序。**v2（stretch）：** 用 AMode 区分 stack 相对（`AMode::FrameSlot`、`SpOffset`）和 global（`Label`、`OutgoingArg`）；不同 slot 的 stack 访问可以自由重排。
   - **side-effect 屏障**：`Call`/`TailCall`/`Ret`/trap 类指令与之前的所有内存操作保持顺序，并作为其后的硬屏障。

2. **关键路径优先级。** 自底向上计算 `crit[n] = latency[n] + max(crit[successors])`。`crit` 最高的就绪指令优先调度（经典 Graham's list scheduling）。

3. **资源模型（Cortex-A53）。** 两条流水线槽，每条都有"类偏好"表。每个 `MInst` 变体映射到 `(slot, latency, throughput)`（在 `aarch53.rs`）。每个 cycle 跟踪每个资源消耗量。两条指令能双发仅当：无依赖且路由到兼容槽。

4. **调度循环。**
   ```
   cycle = 0; ready = {roots}; issued_at[n] = None
   while scheduled.count() < block.len():
       for each slot in [ALU0, ALU1, LSU, MAC, BR, FP]:
           pick highest-crit instruction in ready whose producer-results are ready
               (issued_at[p] + latency[p] <= cycle) and whose slot == this slot
           if found: schedule it, issued_at[n] = cycle, remove from ready, add newly-ready successors
       cycle += 1
   ```
   兜底：当 `cycle - last_progress > BUDGET`（如 64）时停止，剩余指令按原序发出（安全 fallback）。

5. **发射。** 把已调度指令按 `issued_at`（然后再按 slot 优先级）写回 `VCodeContainer.insts[block_range]`。调用 `recompute_cfg()`（block 边没变，但 operand ranges 要同步）。

6. **校验。** `verify("post-scheduler")` 重跑 SSA + operand-order 检查 + 新增 debug 检查 `verify_deps_respected`，确保没有真依赖边被跨过。

### 6.3 `MInst` 需要新增的钩子

加到 `MachInst` trait 上（`anon_armv8` 在 `instructions.rs` 实现）：

```rust
trait MachInst<I: VCodeInst> {
    // ... 已有 ...
    fn sched_class(&self) -> SchedClass;          // ALU0, ALU1, LSU_Load, LSU_Store, MAC, FP, Branch, Barrier
    fn is_mem_access(&self) -> bool { ... }       // 已存在
    fn mem_addr_key(&self) -> MemAddrKey;         // 用于 v2 别名分析: StackSlot(i) | Global | Unknown
}
```

`SchedClass` + 延迟表位于 `sched/aarch53.rs`。

### 6.4 Terminator 与 prologue/epilogue

- prologue/epilogue **绝不调度**（按 block 索引跳过；AsmWriter 已通过 `gen_prologue`/`gen_epilogue` 注入它们）。
- block 末尾的 terminator `MInst`（branch/return，`MachTerminator::*` 返回的）固定在 block 末尾，绝不移动。
- block 中部的 `Call`/`TailCall` 是屏障；它们留在原位，周围的调度被夹紧。

---

## 7. Phase 5 — 验证与测试（与 Phase 3-4 并行构建，~2 天）

1. **更强的 VCode verifier**（`vcode.rs:310-600`）：新增 `verify_operand_order_stable`（在 mutate 前后采集 operands，断言相等）和 `verify_sched_deps`（重放 scheduler 的依赖检查）。
2. **基于文件的测试**在 `tests/`：对每个 `.sy` 输入，同时发射 `--emit asm` 与 `--emit asm,sched`（加一个 `-O 2` 标志打开调度 pipeline）。diff 应该**只是重排**，永不改变语义。
3. **差分测试**在真实 XCZU2EG（若有）或 QEMU 上：跑两遍程序（`-O1` vs `-O1 -O2-sched`），断言输出相同。
4. **单元测试**针对 ARM Cortex-A53 Software Optimization Guide 的示例模式：load-use hiding、MAC folding、LDP pairing。每个测试断言调度的指令顺序符合预期。
5. **benchmark 套件**：矩阵乘法、深 load chain（经典的 load 调度赢家）、多项式求值。用 scheduler 的 `issued_at` 时间戳近似 cycles 数，记录改进幅度。

---

## 8. 文件改动清单（具体落地）

### 新文件
| 路径 | 用途 |
|------|------|
| `taki_mir/src/passes.rs` | `MIRPass` trait、`MIRPassPipeline`、pre-RA/post-RA 切分 |
| `anon_armv8/src/passes/mod.rs` | 模块根；`AArch64Backend::mir_pipeline()` 返回 `[PeepholeCombine, ListScheduler]` |
| `anon_armv8/src/passes/peephole_combine.rs` | Pre-RA peephole pass 驱动 |
| `anon_armv8/src/passes/peephole_rules.rs` | `PeepholeRule<I>` 表 |
| `taki_mir/src/passes/materialize_edits.rs` | 把 `output.edits` 物化进 VCode（共享） |
| `taki_mir/src/passes/legalize_final.rs` | 从 emit 阶段抽出的 legalization（共享） |
| `anon_armv8/src/passes/list_scheduler.rs` | Post-RA scheduler pass |
| `anon_armv8/src/sched/mod.rs` | `SchedClass`、`InstrProfile`、trait 钩子 |
| `anon_armv8/src/sched/aarch53.rs` | Cortex-A53 延迟/吞吐量表 |
| `anon_armv8/src/sched/dag.rs` | 每 block DAG 构建 + 关键路径 |
| `uika_riscv/src/passes/mod.rs` | 空 pipeline 占位 |

### 修改的文件
| 路径 | 改动 |
|------|------|
| `taki_mir/src/lib.rs` | 用 `pipeline.run_pre_ra` / `run_post_ra` 调用替换 `compile()` 的阶段内联；修正 log 阶段名 |
| `taki_mir/src/vcode.rs` | 新增 `replace_inst(at, Vec<I>)`、`recompute_cfg()`、`verify_operand_order_stable`；保留 `write_back_allocs` |
| `taki_mir/src/emit.rs` | 移除 `write_inst` 中的内联 `legalize_inst` 调用；edit 物化后简化 `block_insts_and_edits` |
| `taki_mir/src/lower.rs` | `LowerContext.vcode` 改 `pub(crate)`；新增有意的访问器 |
| `taki_mir/src/abi.rs` | 新增 `gen_prologue_epilogue_skeleton()` 辅助、`ArgLayoutPlanner` |
| `taki_mir/src/machinst_common.rs`（新增或合并进现有文件） | 共享 `call_clobbers`、`use_store_src` |
| `taki_mir/src/vcode.rs`（`MachInst` trait） | 新增 `sched_class()`、`mem_addr_key()` |
| `anon_armv8/src/instructions.rs` | `MInst` 实现 `sched_class()`/`mem_addr_key()`；新增 `MInst::LoadPair`/`StorePair`/`MAdd`/`MSub`/`AluRRRShift` 变体（从 pseudo 形式迁移） |
| `anon_armv8/src/abi.rs` | 精简到目标特定的钩子；其余移到共享 planner |
| `anon_armv8/src/lower.rs` | 移除 `fold_mul_add_sub`、`fold_shifted_rhs`（移到 peephole_rules.rs）；保留 branch-tree 和 imm-form 选择 |
| `anon_armv8/src/lib.rs` | 接入 `mir_pipeline()` |
| `uika_riscv/src/{abi,instructions,lib}.rs` | 镜像 AArch64 的 trait 新增（返回 `SchedClass::Unknown`）；精简 ABI |
| `soyo_compiler/src/main.rs` | 新增 `-O 2` / `--enable-sched` 标志 |
| `README.md` | 修正分配器描述 + 更新 pass 列表 + 新流水线图 |

---

## 9. 风险登记

| 风险 | 严重度 | 缓解措施 |
|------|--------|---------|
| Scheduler 在真依赖上重排 → 错误结果 | **高** | `verify_sched_deps` debug 检查每次调度后跑；差分测试；v1 内存模型保守（所有 memory 按序）；terminator 和 call 屏障固定。 |
| 操作数遍历顺序被 mutate 破坏 → `write_back_allocs` 静默地分配错误的寄存器 | **高** | 新 `verify_operand_order_stable` 断言；任何新增/改动 `MInst` 变体的 pass 都必须重跑该检查。 |
| `Ranges` operand ranges 在 `replace_inst` 插入多指令时损坏 | 中 | 集中辅助函数；增量修复改为一次完整的 `rebuild_operand_tables()`（参考已有的 `collect_operands_and_terminators` 模式）。 |
| `MaterializeEdits` 破坏 AsmWriter 的假设 | 中 | AsmWriter 当前仅前向消费 `output.edits`；物化后传一个空 edit 列表并迭代扁平 VCode。两条路径并存到稳定为止。 |
| Cortex-A53 延迟数字错误 | 低 | 在代码中标注 Optimization Guide 章节；表易于调整；保留 `generic_aarch64` profile 作为更安全的默认。 |
| 竞赛用例的编译时间回归 | 低 | Scheduler 是每 block O(n log n)；带 small budget 兜底；测量。 |
| 中等范围重构在只针对 AArch64 测试时破坏 RISC-V | 中 | CI 必须构建/测试两个 target；`uika_riscv` 的 pipeline 从空开始，逐步镜像 trait 新增。 |

---

## 10. 推荐执行顺序（里程碑）

| 里程碑 | 范围 | 验证 | 预估时间 |
|--------|------|------|---------|
| **M1** | Phase 1：`MIRPass` trait + 空 pipeline；重构 `compile()` 调用它 | 现有测试不变、全过 | 1 天 |
| **M2** | Phase 2 (4c, 4d)：修 `LowerContext` 封装 + README | clean build | 0.5 天 |
| **M3** | Phase 2 (4a)：抽取 `LegalizeFinal` | 汇编输出 byte-identical | 1 天 |
| **M4** | Phase 2 (4b)：AArch64/RISC-V ABI 去重 | 两 target 测试均过 | 1.5 天 |
| **M5** | Phase 3：Pre-RA `PeepholeCombine`（先做 MAC、shifted-RHS 规则） | 汇编 diff；新单元测试 | 2 天 |
| **M6** | Phase 3 stretch：加 `LoadPair`/`StorePair` 形成 | 单元测试 + benchmark | 1 天 |
| **M7** | Phase 4 a：`MaterializeEdits` pass | AsmWriter 仍正确 | 1 天 |
| **M8** | Phase 4 b：`sched/aarch53.rs` + DAG 构建器（不实际调度） | 单元测试 DAG 正确 | 2 天 |
| **M9** | Phase 4 c：list scheduler v1（保守内存模型） | 差分测试全过 | 2 天 |
| **M10** | Phase 4 d：调优 + v2 别名分析 + benchmark | cycles 改进可见 | 1-2 天 |
| **合计** | | | **~13-14 个工作日** |

M1 → M4（基础设施 + 清理）可以作为一个可评审的 PR 落地，然后再开始 Phase 3/4。

---

## 11. 待决策（开工前确认）

- [ ] **MaterializeEdits 步骤**：推荐做（隐藏 spill 延迟收益），但要确认是否接受 AsmWriter 的双路径并存期。
- [ ] **`-O` 级别方案**：保留现有 `-O1`（含 peephole），新增 `-O2` 加上 scheduler；还是直接把 scheduler 合进 `-O1`。
- [ ] **CI 触发**：是否要把 scheduler 差分测试加入 GitHub Actions（目前 `.github/` 已有配置）。
- [ ] **是否引入 ISLE 风格的 lowering 重写**：本计划**不**做（medium scope 保留手写 ISel）。如未来需要，可在 Phase 4 之后单独立项。
