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

## 1. 目标微架构模型 — Xilinx XCZU15EG / Cortex-A53 MPCore

权威数据源：ARM Cortex-A53 Software Optimization Guide (DUI 0901)。
对 XCZU15EG 的隔离测评核（Cortex-A53 MPCore）：

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

XCZU15EG 的 **L1D$ 32 KB 4-way、L1I$ 32 KB 2-way、L2$ 1 MB 16-way shared** 不直接驱动**指令调度**——但 L2 命中会增加约 9 周期的额外 load 延迟；我们额外暴露一个 `aarch53_l2` 模型用于过拟合实验。

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

**状态：✅ 已完成 (M3，与 M7 合并实现)**

已实现 `VCodeContainer::finalize_for_emission(&mut self, output: &Output)`，一步完成 MaterializeEdits + LegalizeFinal：

1. 遍历每个 block，用 `output.block_insts_and_edits` 获取原始指令+edit 交错序列
2. 对每条指令调用 `I::ABISpec::legalize_inst(frame, inst)` 物化伪地址
3. 对每个 edit-move 生成真实的 move/spill/reload 指令并 legalize
4. 构建新的 `insts` 数组和 `block_range`，重建 `inst_is_branch`/`inst_is_ret`
5. 清理不再需要的 operand 表（`operands`、`operands_range`、`clobbers`）

**compile() 流程变更：**
- `compute_frame_layout` 移到 post-RA pipeline 之前（legalize 需要 frame offset）
- `finalize_for_emission` 在 frame layout 之后调用，消费 `output`
- post-RA pipeline（scheduler 等）在 finalize 之后运行，看到的是已物化+合法化的 VCode
- `AsmWriter::write_function` 不再接收 `output`，直接迭代 VCode per-block

**注意：M3 和 M7 合并的原因：**
LegalizeFinal 可能改变指令数量（1→N 展开），这会使 regalloc Output 的 ProgPoint 索引失效。
因此必须先物化 edits（消费 Output），再 legalize。两步共享同一个 Output 消费点，合并到
`finalize_for_emission` 是最干净的设计。

**emitter 简化：**
- 移除了 ~90 行的 edit-interleaving 逻辑
- `write_inst`（带 legalize）仅用于 prologue/epilogue（ABI 生成、含 SpOffset）
- `print_inst`（无 legalize）用于 VCode 指令（已 finalized）
- `verify()` 更新以支持 post-finalize 状态（operand 表已清空）

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

**状态：✅ 已完成 (M5，MAC 部分)**

**新文件：**
- `anon_armv8/src/passes/mod.rs` — pass 模块根，`build_pipeline()` 注册 `PeepholeCombine`
- `anon_armv8/src/passes/peephole_combine.rs` — MAC folding pass

**已实现：**
- `PeepholeCombine: MIRPass<MInst>` 注册为 AArch64 pre-RA pass
- MAC folding 规则：`AluRRR{Mul} + AluRRR{Add} → MAdd`，`AluRRR{Mul} + AluRRR{Sub,rhs=mul_dst} → MSub`
- 安全检查：producer 的 dst 虚拟寄存器在整个函数中仅被使用一次（通过 `build_vreg_use_counts` 遍历所有指令的 `get_operands` 构建 use-count map）
- `VCodeContainer::rebuild_operand_tables()` — pre-RA pass 改变指令后重建 operand/clobber/terminator 表
- `compile()` 在 `run_pre_ra` 后调用 `rebuild_operand_tables()` 确保 regalloc 看到准确的 operand 信息
- 新增 VCodeContainer 访问器：`inst_mut()`, `num_blocks()`, `block_inst_range()`, `num_insts()`

**注意事项：**
- HIR 层的 `fold_mul_add_sub` 已经在 lowering 时捕获大部分 MAC 模式
- MIR 层的 peephole 补充了 lowering 遗漏的情况（如 IR 优化后新出现的 Mul+Add 模式）
- `vreg_alias` 在本项目中从未使用（`add_alias` 从未被调用），所以 `rebuild_operand_tables` 使用 identity resolver

---

## 6. Phase 4 — Post-RA ListScheduler Pass（核心，~4-5 天）

**状态：✅ 已完成 (M8+M9，v1 保守模型)**

**新文件：**
- `anon_armv8/src/sched/mod.rs` — 模块根，类型导出
- `anon_armv8/src/sched/aarch53.rs` — Cortex-A53 延迟/吞吐量表
- `anon_armv8/src/sched/dag.rs` — 每 block 依赖 DAG 构建 + 关键路径
- `anon_armv8/src/passes/list_scheduler.rs` — post-RA list scheduler pass

**架构：**

1. **依赖提取**（`sched/dag.rs::inst_deps`）：
   - 直接检查 MInst 字段提取物理寄存器 def/use（post-RA 下 `get_operands` 跳过物理寄存器，不可用）
   - 每个 variant 映射到 `SchedClass`（Alu/Mul/Div/Load/Store/Branch/Barrier/Nop/Other）
   - 内存操作标记 `MemKind::Load/Store`

2. **DAG 构建**（`DepGraph::build`）：
   - RAW 边权重 = producer 延迟
   - WAW / WAR 边权重 = 0
   - 内存依赖保守：load-after-store 有序，store-after-load/store 有序
   - Call/Return/TailCall 是 barrier，序列化所有前后指令

3. **关键路径**：`crit[i] = latency[i] + max(crit[successors])`

4. **List scheduling**（`list_scheduler::schedule`）：
   - 按 critical path 降序排就绪节点
   - 节点在 `issue[producer] + latency[producer] <= cycle` 时变为 data-ready
   - 兜底：超过 `20 * n` cycle 未发射则按原序发出剩余节点

5. **安全保证**：
   - terminator 固定在 block 末尾（`find_terminator_offset`）
   - ≤ 2 条指令的 block 跳过
   - 发射后重建 `inst_is_branch`/`inst_is_ret`
   - verify 支持 post-finalize 状态（跳过 `verify_strict_ssa`，因 operand 表已清空）

**Cortex-A53 延迟表：**
| Class | Latency |
|-------|---------|
| Alu | 1 |
| Mul (mul/madd/msub/smull) | 3 |
| Div (sdiv) | 11 |
| Load (ldr/ldp) | **2** |
| Store (str/stp) | 1 |
| Branch | 1 |
| Barrier (call/ret) | 1 (但序列化) |

**主要优化目标：** load-use 延迟隐藏。Cortex-A53 的 L1 load 延迟为 2 周期，紧跟的依赖 ALU 指令会停顿 1 周期。Scheduler 通过在 load 和 consumer 之间插入独立指令来隐藏这个延迟。

**v2（stretch）尚未实现：**
- 基于栈槽/global 的内存别名分析（当前保守：所有内存有序）
- 精确 ALU0/ALU1 槽位配对（基础双发宽度和 LSU/MAC 资源约束已实现）
- LoadPair/StorePair 形成（M6）

**M10 correctness 加固已完成：**
- 显式建模 NZCV producer/consumer 依赖
- 保留全部未决 register reader 和 load，修复 WAR / store-after-load 漏边
- barrier 与整个前缀、后缀串行；控制流不再遗漏寄存器和 NZCV use
- load destination 只作为 def；pair pre/post-index writeback base 同时作为 use/def
- 每周期最多双发，且 LSU、MAC/Div、FP 类资源每周期各最多一条；非流水化 Div 按延迟占用 MAC/Div 资源
- DAG 和 issue model 新增针对性单元测试

---

## 7. Phase 5 — 验证与测试（与 Phase 3-4 并行构建，~2 天）

1. **更强的 VCode verifier**（`vcode.rs:310-600`）：新增 `verify_operand_order_stable`（在 mutate 前后采集 operands，断言相等）和 `verify_sched_deps`（重放 scheduler 的依赖检查）。
2. **基于文件的测试**在 `tests/`：对每个 `.sy` 输入，同时发射 `--emit asm` 与 `--emit asm,sched`（加一个 `-O 2` 标志打开调度 pipeline）。diff 应该**只是重排**，永不改变语义。
3. **差分测试**在真实 XCZU15EG（若有）或 QEMU 上：跑两遍程序（`-O1` vs `-O1 -O2-sched`），断言输出相同。
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
