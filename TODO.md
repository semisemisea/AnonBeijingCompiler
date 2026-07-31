# Cortex-A53 后端优化计划

本文档只记录尚未完成的工作。M1-M17 已完成，历史设计与实现细节以 Git 提交记录
和代码测试为准，不在这里重复维护。

已完成的能力概要：

- MIR pass 基础设施、发射前 finalize、ABI 参数布局共享（M1-M5）。
- pre-RA PeepholeCombine（MAC 融合）、post-RA PairCombine（LDP/STP）（M6）。
- 依赖 DAG、保守内存别名模型、基础 Cortex-A53 list scheduler（M7-M10）。
- 可切换 pipeline：`AArch64CodegenConfig`、`-O0/1/2` 映射、结构化统计（M11）。
- 共享 `CycleSimulator`、edge-latency critical path、确定性调度（M12）。
- `MInst::Removed` tombstone，消除 emitted Nop（M13）。
- 精确 Cortex-A53 slot/resource 模型、Div32/64、FP、pair 细分 profile（M14）。
- XCZU15EG PMU benchmark harness（M15，待实机运行）。
- Slot-filling dual-issue heuristic（M16）。
- 端到端验证门禁、确定性检查（M17）。
- pre-RA DCE（M18）：worklist use-count fixpoint，白名单制（纯 ALU/Mov/常量
  物化/纯 FP + dead load），`-O1` 起默认开启，`--enable/disable-mir-dce`
  开关，`DceStats` 统计。huffman-01 上 `-O1` 指令数 964 → 935（-3%），
  `mov w13, wzr` 等死代码全部消除。

目标硬件是 Xilinx XCZU15EG 上的 Cortex-A53 MPCore。

---

## 1. 当前实现基线

### 1.1 流水线

当前 AArch64 MIR pipeline：

```text
lowering
  -> pre-RA DeadCodeElim      (-O1 起)
  -> pre-RA PeepholeCombine   (-O1 起)
  -> register allocation
  -> write_back_allocs
  -> frame layout
  -> finalize_for_emission
  -> post-RA PairCombine      (-O1 起)
  -> post-RA ListScheduler    (-O2 起)
  -> assembly emission
```

关键代码：

- `anon_armv8/src/passes/mod.rs`：按 `AArch64CodegenConfig` 注册 pass。
- `taki_mir/src/passes.rs`：`MIRPass` trait、pre-RA/post-RA 两阶段 pipeline。
- `anon_armv8/src/passes/dce.rs`：worklist use-count fixpoint DCE，白名单制。
- `anon_armv8/src/passes/peephole_combine.rs`：vreg use 计数 + MAC 融合，
  使用 `Removed` tombstone。
- `anon_armv8/src/instructions.rs`：`MInst` 枚举（约 60 个 variant）。
- `taki_mir/src/stats.rs`：函数级 / 编译单元级结构化统计。
- `soyo_compiler/src/cli.rs`：`-O` 映射与 `--enable/disable-*` 开关。

### 1.2 当前结论边界

- 静态 estimator 只用于确定性回归和相对启发式比较，不能替代实机测量。
- scheduler 与 estimator 共用同一模型，"模型内不退化"不等于"硬件不退化"。
- QEMU 仅用于语义差分，不能证明 A53 的 dual-issue / latency / throughput 收益。
- 所有 profile latency 为 ARM guide 推导值（DUI 0901），未经 XCZU15EG 实测校准。
- 未获得实机数据前，文档和提交信息只能声称"静态模型改进"，不能声称
  "XCZU15EG runtime 提升"。

---

## 2. 后续候选工作

按预期收益排序，均需在前一项验证后再启动。

### P1：常量 / 分支参数物化源头治理

DCE 是兜底；更优解是 lowering 时就不为未使用的 block param 和分支参数物化
常量与 `mov wzr`。DCE 落地后统计 `instructions_removed` 的构成，若某类来源
占主导，直接在 `taki_mir/src/lower.rs` 或 `anon_armv8/src/lower.rs` 消除源头。

### P1：参数不落栈

函数入口将参数 `str` 到栈再立即 `ldr` 回来（如 `_and` 入口），产生
store→load forwarding 停顿。应在 lowering / ABI 层让短生命期参数直接保留在
寄存器，从根因消除，而不是靠调度器绕开。

### P1：双重分支化简

`cmp; b.eq 1f; b target; 1: b other` 可化简为单条条件分支，每个分支点省
1 条指令和 1 个前端槽。需处理 long-jump 形式的跳转范围约束。

### P2：phi 拷贝 coalescing

循环末尾的 `mov x5, x4; mov x12, x3` 并行拷贝链，可在 RA 后消除部分拷贝。

### P2：跨块 / 全局调度

块内调度对被 call 切碎的热点无能为力。候选方向：循环不变 load 外提
（`adrp+add+ldr gv_*` 全局量地址重算）、跨块 hoist。属大改动，需先完成
P1 项并重新评估收益空间。

### P2：post-RA 调度器

RA 引入的 spill/reload 与 callee-saved save/restore 无法被 pre-RA 调度拉开。
在 P1 完成后再评估。

### P2：调度验证器闭环

- `verify_operand_order_stable`：保护 pre-RA pass 的 operand traversal
  contract。
- `verify_sched_deps`：独立于 scheduler 重放调度后的 register、NZCV、
  memory 和 barrier 约束。
- 小 DAG reference simulator / property tests。

### P2：内存 DAG 复杂度

长块最坏 `O(M^2)`。已有统计，先采集编译时间数据，确认是实际问题后再引入
按 root/range 分组的数据结构。

### P2：调度启发式增强（需实机数据证明收益）

- Load-use latency hiding 专项。
- Pair-aware scheduling（调度时考虑 LDP/STP 形成）。
- post-RA register-pressure tie-break。
- pre-RA scheduler（需先证明 post-RA false dependency 是主要 ILP 限制）。

### P3：XCZU15EG 实机校准（依赖硬件访问）

- 运行 `benchmarks/src/bench.c`，校准 latency / throughput / pairing 数据。
- 基于实测调整 guide-derived profile 值。
- 建立性能回归门禁。
- 回答：WAR/WAW/NZCV false dependency 是否允许 A53 同周期双发。

---

## 3. 总体执行原则

1. 每个 milestone 独立提交；`TODO.md` 在 milestone 完成后删除对应已完成
   细节，只保留后续工作。
2. 正确性验证、静态模型测试和实机性能测量分层维护，不互相替代。
3. 所有 AArch64 改动必须同时验证 RISC-V 不受影响。
4. 新 pass 默认走"白名单 + 保守保留"策略，宁漏勿错。
5. 未获得实机数据前，只能声称"静态模型改进"。

---

## 4. 风险与缓解

| 风险 | 严重度 | 缓解措施 |
|------|--------|---------|
| DCE 误删有隐式副作用的指令（flags、内存、call） | 高 | 白名单制；无 def 指令一律跳过；全量功能回归；on/off 差分 |
| dead load 删除改变 trap 行为 | 低 | SysY 语义下 load 地址必合法；如未来支持 volatile 需加例外 |
| DCE 破坏 SSA / operand 不变式 | 中 | `Removed` tombstone 沿用 M13 先例；pipeline 既有 verify 钩子 |
| 缺失 register/NZCV/memory 依赖导致调度误编译 | 高 | 保守 catch-all barrier、on/off 差分、`verify_sched_deps`（待做） |
| slot/resource 模型错误导致硬件回归 | 高 | 模型内防退化仅作辅助，实机 gate 为准 |
| AArch64 配置改动破坏 RISC-V | 中 | 双 target 测试、RISC-V 拒绝 MIR flag |
| QEMU wall time 被误用为 A53 性能数据 | 中 | QEMU 仅进入 correctness gate |
| 实机环境频率/温度噪声掩盖结果 | 中 | core pinning、paired samples、95% CI、环境元数据 |
