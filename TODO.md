# Cortex-A53 指令调度下一阶段优化计划

本文档只记录尚未完成的工作。M1-M10 的 MIR pass 基础设施、发射前
finalize、ABI 参数布局共享、pre-RA MAC combine、post-RA load/store pair、
依赖 DAG、基础 Cortex-A53 list scheduler、保守内存别名模型和固定顺序 cycle
estimator 已完成，历史设计与实现细节以 Git 提交记录和代码测试为准，不在这里重复维护。

目标硬件是 Xilinx XCZU15EG 上的 Cortex-A53 MPCore。下一阶段的首要目标不是
继续叠加启发式，而是先建立可切换、可观测、与最终汇编一致并可由实机校准的
调度体系，再基于可信模型优化 ALU0/ALU1 配对、load-use latency hiding、pair
formation 和寄存器压力。

---

## 1. 当前实现基线

### 1.1 流水线

当前 AArch64 MIR pipeline：

```text
lowering
  -> pre-RA PeepholeCombine
  -> register allocation
  -> write_back_allocs
  -> frame layout
  -> finalize_for_emission
  -> post-RA PairCombine
  -> post-RA ListScheduler
  -> assembly emission
```

关键代码：

- `anon_armv8/src/passes/mod.rs`：注册 `PeepholeCombine`、`PairCombine` 和
  `ListScheduler`。
- `taki_mir/src/lib.rs`：运行 pre-RA/post-RA pipeline；post-RA pass 在
  `finalize_for_emission()` 后运行。
- `anon_armv8/src/passes/list_scheduler.rs`：按基本块进行 post-RA list
  scheduling，并用固定顺序 estimator 拒绝模型内退化。
- `anon_armv8/src/sched/dag.rs`：提取物理寄存器、NZCV、内存和 barrier 依赖。
- `anon_armv8/src/sched/aarch53.rs`：当前粗粒度 Cortex-A53 latency/class 模型。

### 1.2 已具备能力

- 基本块内调度，不跨 CFG edge。
- 物理寄存器 RAW、WAR、WAW 依赖。
- NZCV producer/consumer、WAR 和 WAW 依赖。
- 控制流、call、tail call、return 和未建模指令的保守 barrier。
- load/store、pair load/store 的保守内存顺序依赖。
- SP、FP、global、64-bit move、64-bit add/sub immediate 地址 provenance。
- 同一已知 root 的常量 byte range disjoint 判断。
- 不同 global 和 global-vs-stack 的 disjoint 判断。
- 未知地址、动态 register offset、SP-vs-FP、溢出和 writeback pair 的保守
  may-alias。
- 每周期最多双发；LSU、Mul/Div、`Other` 类共享资源约束。
- Div 按当前 latency 非流水化占用 Mul/Div 资源。
- critical-path-first ready queue。
- 固定顺序 completion-cycle estimator 和模型内防退化写回门槛。
- 静态 load-use 用例在当前模型中从 4 cycles 降到 3 cycles。

### 1.3 当前结论边界

- 当前实现可作为正确性优先的 post-RA scheduler 基线。
- 当前静态 estimator 可用于确定性回归测试和相对启发式比较。
- 当前 estimator 不能替代 XCZU15EG 实机周期测量。
- scheduler 和 estimator 共用同一模型，因此“模型内不退化”不能证明“硬件上
  不退化”。
- QEMU 可用于语义差分测试，不能用于证明 Cortex-A53 dual-issue、latency 或
  throughput 收益。

---

## 2. 已知问题与优先级

### P0：缺少可归因的 scheduler on/off 基线

当前 `-O` 只控制 IR pass manager，AArch64 MIR pipeline 不接收 optimization
level，因而无法在相同 IR、相同 RA、相同 emission 条件下分别测量：

```text
PeepholeCombine on/off
PairCombine on/off
ListScheduler on/off
PairCombine + ListScheduler 的组合效果
```

没有独立开关时，功能差分和性能差分都难以归因。

### P0：调度节点与最终汇编不一一对应

以下 `MInst` 可能发射多条真实指令：

- `LoadImm`：可能展开为多条 move-wide 指令。
- `LoadAddr`：发射 `ADRP + ADD`。
- `CmpSelect`：发射 compare + `csel`/`fcsel`/`cset`。
- `Cbz`、`Cbnz`、`Tbz`、`Tbnz`、`CondBr`：当前 long-jump 形式可能发射
  三条 branch。
- `PairCombine` 用 `MInst::Nop` 保持 VCode 指令数量稳定；如果 emission 仍输出
  真实 `nop`，pair formation 不会减少最终指令数和前端压力。

当前 estimator 主要按一个 `MInst` 等于一个 issue node 计算，因此会低估部分
block 的 emitted instruction count、前端占用和 completion cycles。

### P1：critical path 未使用 edge latency

`DepGraph` 保存 `(successor, edge_latency)`，但当前 critical path 使用：

```text
crit[node] = node_latency + max(crit[successor])
```

这对普通 RAW chain 经常近似正确，但会高估 WAR、WAW、memory order 等零权边
路径，影响 ready-node priority。下一步必须明确 critical path 表示 issue distance
还是 completion makespan，并按 edge latency 计算。

### P1：scheduler 与 estimator 时序语义未完全统一

- scheduler 检查 `issued + latency <= cycle`。
- estimator 检查 `issued + max(latency, 1) <= cycle`。
- scheduler 本轮不会重新扫描刚变为 ready 的 successor，因此零权边通常仍跨一个
  cycle，但该行为没有形成共享、显式的模型规则。

两者应共用统一的 earliest-issue 和 resource transition 实现，避免未来演化漂移。

### P1：微架构 profile 过粗

当前 `InstrProfile` 只有 `latency` 和 `class`。尚未显式表达：

- reciprocal throughput。
- resource occupancy。
- ALU0/ALU1 allowed slots。
- LSU、MAC、DIV、FP/NEON、branch 等 resource mask。
- emitted operation count。
- operand/result-specific latency。
- scalar 与 pair memory operation 差异。

`Other` 同时承载多种 FP 和未精确建模的指令，latency 4 只能作为保守占位，不能
作为最终性能模型。

### P1：Div32、Div64、FP 和 pair 未细分

- `SDiv` 保留 operand size，但 scheduler 统一映射为 latency 11 的 `Div`。
- 32-bit 与 64-bit divide latency 范围不同，并可能依赖 operand。
- FP move、add/sub、mul/div、compare、convert 统一映射到 `Other`。
- LDP/STP 与 LDR/STR 共用 load/store class 和 latency。
- PairCombine 只识别原始顺序中已相邻的 fixed-offset memory operations。

### P2：post-RA false dependency 与寄存器压力

post-RA scheduling 不会增加 allocator spill，但物理寄存器复用会产生额外 WAR/WAW
边。当前 priority 不考虑：

- ready node 会杀死多少 source register。
- 会产生多少新的 live destination。
- 当前 physical live-register delta。
- spill/reload criticality。
- original-order distance 和寄存器复用距离。

短期应先做 post-RA pressure-neutral tie-break；在有数据证明收益后，再评估 pre-RA
pressure-aware scheduling。

### P2：内存 DAG 最坏为平方复杂度

每个 memory operation 会扫描 barrier 以来全部 memory history。包含 `M` 个内存
操作的长基本块最坏为 `O(M^2)`。当前先增加统计和编译时间 benchmark，再决定是否
引入按 root/range 分组的数据结构。

### P2：调度后独立验证器尚未闭环

仍需实现：

- `verify_operand_order_stable`：保护 pre-RA pass 的 operand traversal contract。
- `verify_sched_deps`：独立于 scheduler 重放调度后的 register、NZCV、memory 和
  barrier 约束。
- 小 DAG reference simulator/property tests：验证 permutation、拓扑顺序、资源约束
  和 makespan。

---

## 3. 总体执行原则

1. 先建立开关、统计和基线，再改变调度策略。
2. 先统一模型内部语义，再增加 Cortex-A53 微架构细节。
3. 先使调度节点与最终 emitted assembly 对齐，再使用 estimator 做性能门禁。
4. 正确性验证、静态模型测试和实机性能测量分层维护，不互相替代。
5. 每个 milestone 独立提交，`TODO.md` 在 milestone 完成后删除对应已完成细节，只
   保留后续工作。
6. 所有 AArch64 调度改动必须同时验证 RISC-V 不受影响。
7. 未获得实机数据前，文档和提交信息只能声称“静态模型改进”，不能声称
   “XCZU15EG runtime 提升”。

---

## 4. M11：可切换 pipeline、统计和差分基线（✅ 已完成）

已实现：
- `AArch64CodegenConfig`：peephole / pair / scheduler / model 独立开关，显式传递。
- `-O0/1/2` 映射：O0=全关闭，O1=peephole+pair，O2=全部；显式 `--enable/disable-*` 覆盖。
- CLI 解析和优先级单元测试（8 种组合、冲突、`-O3` 拒绝、RISC-V flag 拒绝）。
- `CompileOutput { assembly, stats }`：结构化统计替代日志解析。
- 每函数统计：peephole MAC 数、pair 形成数、tombstone Nop 数。
- 每函数 scheduler 统计：block 总数/检查/跳过/identity/rejection/fallback、
  原始和调度后 completion/stall/single/dual/idle cycles、资源使用。
- DAG 统计：节点/边/按类型边数、memory known/unknown root、disjoint/may-alias
  比较数、最大 history 长度。
- 编译单元级聚合：所有计数器求和、max block nodes/history、fallback 列表。
- RISC-V 不受影响（`CodegenConfig = ()`，空 pipeline）。

未实现（后续 milestone）：
- `--codegen-stats PATH` JSON 输出（统计仅内存可用）。
- DAG build/schedule 耗时（结构已定义，未采集）。
- 差分测试 harness 的自动化脚本（需手动运行四种组合）。

---

## 5. M12：统一依赖时序、critical path 和 reference simulator（✅ 已完成）

已实现：
- `CycleSimulator`（`anon_armv8/src/sched/simulator.rs`）：共享 cycle/resource 模型。
- `earliest_issue_cycle()`：统一 scheduler 和 estimator 的 latency 规则，消除
  `latency` vs `max(latency, 1)` 差异。
- Critical path 使用 edge latency：`crit[i] = max(node_latency, max(edge.latency + crit[succ]))`。
- 确定性排序：critical path 降序 + 原始 index 升序 tie-break。
- `can_issue()`、`is_ready()`、`reserve()`、`completion_cycle()` 全部集中在 `CycleSimulator`。
- 移除旧的 `IssueResources` 和独立 `can_issue` 函数。
- 新增测试：零权边 critical path、确定性调度、scheduler/estimator 一致性。

---

## 6. M13：调度节点与最终汇编一致化（✅ 已完成）

已实现：
- 新增 `MInst::Removed` tombstone variant，发射时不产生任何输出。
- `PairCombine` 和 `PeepholeCombine` 使用 `Removed` 替代 `Nop`，消除 tombstone Nop。
- 统计字段更名：`tombstone_nops_created` → `tombstone_removed_created`。
- 所有 pattern match 和 DAG builder 已处理 `Removed`。
- 验证：`-O2` 编译的 assembly 中 `nop` 指令数为 0。

未实现（后续 milestone）：
- `LoadImm`/`LoadAddr`/`CmpSelect` expansion 或 bundle profile。
- 显式 emitted-op count 与 assembly 对照测试。

---

## 7. M14：精确 Cortex-A53 issue-slot 和资源模型

### 7.1 目标

将当前 class-only 模型升级为可表达 Cortex-A53 双发槽位、吞吐、占用和主要结构
hazard 的 profile，同时保留一个更保守的 generic AArch64 profile 作为回退。

### 7.2 Profile 结构

建议最小结构：

```rust
struct InstrProfile {
    result_latency: u32,
    reciprocal_throughput: u32,
    resource_occupancy: u32,
    allowed_slots: SlotMask,
    resources: ResourceMask,
    emitted_ops: u8,
}
```

若实测证明不同 operand/result 有不同 bypass latency，再扩展 operand-specific latency；
不要在没有数据前过度抽象。

### 7.3 资源与槽位

至少表达：

- `ALU0`
- `ALU1`
- `LSU`
- `MAC`
- `DIV`
- `FP_NEON`
- `BRANCH`
- `FRONTEND_ISSUE`

每周期总发射上限仍为 2，但合法配对由 slot/resource assignment 决定，而不是只检查
两个粗粒度 class 是否不同。

### 7.4 指令分类

至少拆分：

- simple integer ALU。
- shifted/extended integer ALU。
- compare、conditional select、move-wide。
- Mul、MAdd、MSub、SMulL。
- Div32、Div64。
- scalar integer load/store。
- scalar FP load/store。
- pair integer load/store。
- pair FP load/store。
- FP move。
- FP add/sub。
- FP multiply。
- FP divide。
- FP compare。
- int/FP conversion。
- branch。
- atomic bundle。
- full barrier。

### 7.5 数据来源

每条 profile 必须记录来源：

- ARM Cortex-A53 Software Optimization Guide 的章节/表格；或
- XCZU15EG microbenchmark 名称和结果版本。

文档数据与实测冲突时，保留两者并说明默认选择。硬件尚未验证的值要明确标记为
guide-derived，不得伪装为 measured。

### 7.6 Generic profile

增加保守的 `generic_aarch64` profile：

- 不假定精确 ALU0/ALU1 配对。
- 对未知或 compound instruction 使用保守 occupancy。
- 保证 correctness，但不承诺 Cortex-A53 最优。

CLI/backend 可选择 `cortex-a53` 或 `generic-aarch64`，未知 CPU 默认选择保守模型。

### 7.7 验收标准

- scheduler 可见的常用 MInst 不再落入无 operand 信息的 catch-all。
- 所有 profile 都有来源和直接测试。
- Div32/Div64、scalar/pair、integer/FP memory operation 分开。
- 合法 pairing 由 slot/resource assignment 判断。
- 构建 pairing matrix 测试，覆盖允许和禁止的双发组合。
- generic profile 和 Cortex-A53 profile 均可选择且生成合法代码。
- 静态 mixed-kernel reference simulator 能解释每个 stall 的 dependency 或 resource
  原因。

---

## 8. M15：XCZU15EG 实机校准与 benchmark harness

### 8.1 前置条件

M11-M14 完成后再把静态模型用于实机性能结论。若当前开发环境不能直接访问
XCZU15EG，本 milestone 仍应完成可部署 benchmark package、runner 和结果格式，由
外部硬件执行后回填数据。

### 8.2 实验环境记录

每次结果必须记录：

- board/SoC 型号和 revision。
- kernel、toolchain、linker 版本。
- Cortex-A53 core 编号。
- CPU governor 和固定频率。
- 是否隔离 core、关闭或控制其他 workload。
- cache warm/cold 策略。
- benchmark binary hash 和 compiler commit。
- scheduler/profile 配置。
- 样本数、warmup 次数和统计方法。

### 8.3 测量规则

- 固定到单个 A53 core。
- 固定 governor/frequency；无法固定时记录实际频率并拒绝不稳定结果。
- 先 warmup，再采样。
- 优先使用 PMU cycles 和 instructions；wall time 仅作辅助。
- 每个 case 至少 30 个独立样本，或增加循环次数直到置信区间稳定。
- 报告 mean、median、P95、standard deviation 和 95% confidence interval。
- scheduler on/off 使用 paired comparison，运行顺序随机化或交错，降低温度和系统
  漂移影响。
- QEMU 结果只进入 correctness report，不进入 Cortex-A53 performance report。

### 8.4 Microbenchmark 矩阵

#### Dependency latency

- integer add/sub/logical/shift chain。
- load-use chain：I32/I64/F32/F64。
- MUL、SMULL、MADD、MSUB chain。
- SDIV32、SDIV64，多组 operand pattern。
- FP add/sub/mul/div chain。
- compare-to-select、compare-to-branch。
- LDP result use 和 store-to-load forwarding。

#### Reciprocal throughput

- 多个独立 integer ALU。
- 独立 load、store、load/store mix。
- 独立 MUL/MADD。
- 独立 FP operations。
- scalar 与 pair memory operation。
- branch loop throughput。

#### Pairing matrix

- ALU + ALU。
- ALU + load/store。
- ALU + MUL/MADD。
- ALU + FP。
- ALU + branch。
- load/store + branch。
- MUL + branch。
- FP + branch。
- 每种组合分别测试无依赖和有依赖版本。

#### Cache profiles

- L1D-resident working set。
- L2-resident working set。
- pointer chasing/unknown latency 只用于观察，不直接驱动固定 latency scheduler。

### 8.5 校准输出

benchmark runner 输出机器可读结果，例如 JSON/CSV：

```text
benchmark
compiler_commit
profile
pass_configuration
sample_count
cycles_mean
cycles_median
cycles_p95
cycles_ci95_low
cycles_ci95_high
instructions_mean
```

profile 更新必须引用对应数据文件或版本，不直接把未经记录的数字写入源码。

### 8.6 验收标准

- benchmark package 可在 XCZU15EG 上一条命令构建/运行/导出结果。
- stable instruction latency 预测误差不超过 1 cycle。
- reciprocal throughput 预测误差不超过 10%。
- Div32/Div64 分别报告 min、median、P95、max 和 operand set。
- pairing matrix 对合法/冲突组合的分类 precision 和 recall 均达到 95% 以上。
- L1-resident mixed micro-kernel cycle prediction MAPE 不超过 10%。
- 所有 profile 数值可追溯到 guide 或版本化实测结果。

---

## 9. M16：调度启发式、pair 协同和寄存器压力

### 9.1 前置条件

M16 只能在 M12 的 reference simulator 和 M14/M15 的可信 profile 基础上调优。
否则新 heuristic 可能只是在优化错误模型。

### 9.2 Ready-node priority

逐项实验，不一次叠加全部 heuristic。候选 priority tuple：

```text
critical path descending
current-cycle slot compatibility descending
load-use latency hiding benefit descending
resource release benefit descending
register-pressure delta ascending
pair-formation benefit descending
original instruction index ascending
```

每加入一项都必须有独立开关或实验分支、静态最优 gap 数据和实机 benchmark，确认
收益后再固化。

### 9.3 Slot filling

- 第一条发射后，为第二条选择兼容 slot/resource 的 ready node。
- 比较“全局最高 critical path”与“能完成合法双发”的 trade-off。
- 不允许为了填槽延迟真正 critical RAW chain，除非模型预测 makespan 不增加。
- 对 branch 独立单元的配对仅在 control-flow 表示和硬件数据支持后启用。

### 9.4 Load-use 专项

- 识别即将产生 load-use stall 的 consumer。
- 在 load 与 consumer 之间优先安排不会延长关键路径的独立 instruction。
- 区分 scalar/pair、integer/FP load latency。
- 不对未知 cache miss latency做激进静态假设；默认模型仍针对 L1 hit。

### 9.5 Pair-aware scheduling

当前 PairCombine 只处理已相邻指令。候选改进：

1. 在 scheduler 中识别可形成 pair 的两个 memory nodes。
2. 对安全且不增加 makespan的相邻排列增加 pair bonus。
3. scheduler 后再运行 PairCombine。
4. compact tombstone，并重新验证 block metadata。

必须同时考虑：

- 两个 load destination 和 base hazard。
- memory alias/order edges。
- pair immediate 编码范围和方向。
- pair profile 是否真的优于两个 scalar operations。
- code size、front-end instruction count 和实机 cycles。

如果实测显示某类 LDP/STP 在 A53 上无收益或回归，应按 memory type/address form 禁用，
不能把 pair 数量本身作为成功指标。

### 9.6 Post-RA register-pressure tie-break

先实现低风险的局部评分：

- 调度该 node 后被最后一次使用、可视为 killed 的 physical source 数量。
- 新定义且仍有未来 use 的 destination 数量。
- 近似 live-register delta。
- spill reload 是否位于关键链。
- original-order distance。

该评分只用于 critical path 相同或接近时的 tie-break，不得覆盖明显更长的关键路径。

### 9.7 是否引入 pre-RA scheduler

只有满足以下条件才单独立项：

- post-RA false dependency 被统计证明为主要 ILP 限制。
- 高寄存器压力 workload 中存在可量化的 spill/调度 trade-off。
- 已具备 pressure tracker 和 pre/post-RA 差分验证。

pre-RA scheduler 不纳入 M16 默认范围，避免同时改变 RA 输入、spill 数量和指令顺序。

### 9.8 验收标准

- 每项 heuristic 都能单独启停并有 benchmark 归因。
- 节点数不超过 8 的随机 DAG 上，平均/最大 optimality gap 不劣于 M12 baseline。
- post-RA pressure tie-break 不改变 spill/reload 数量。
- 最大近似 physical live-register count 不高于 baseline，除非有显著实机收益。
- pair-aware scheduling 不增加 text size geomean。
- load-bound microbenchmark geomean cycles 相对 M15 baseline 至少改善 3%，且 95% CI
  不跨 0。
- 任一 heuristic 导致端到端 workload 回归超过 2% 且 95% CI 不跨 0 时，必须修复、
  限定适用范围或回退。

---

## 10. M17：端到端 benchmark、性能门禁和长期维护

### 10.1 Benchmark 集合

复用现有 functional case，并增加最小、稳定、可解释的 scheduler kernels：

- matrix multiply。
- DCT。
- polynomial evaluation。
- deep load-use chain。
- 多个独立 load + ALU。
- pointer chasing。
- memcpy-like load/store。
- pair-heavy stack access。
- pair-heavy global access。
- MUL/MADD chain 和 independent throughput。
- variable SDIV32/SDIV64。
- branch-heavy control flow。
- high register-pressure kernel。
- sort、BFS、DFS、DP 等现有端到端 workload。

每个 benchmark 标记主要瓶颈类别，避免只看总 geomean 而无法解释变化。

### 10.2 Correctness gate

- workspace unit tests 全部通过。
- AArch64 scheduler/pair on/off 差分输出全部一致。
- RISC-V unit/functional tests 全部通过。
- debug build 启用 `verify_sched_deps`。
- 对随机小 block 进行调度前后 interpreter/reference dependency 验证。
- assembly 必须能被 GNU AArch64 toolchain 接受并成功链接。

### 10.3 Performance gate

XCZU15EG 实机目标：

- load-bound 子集 cycles geomean 改善至少 5%。
- 全 benchmark cycles geomean 改善至少 2%。
- 任一 benchmark 回归超过 2% 且 95% CI 不跨 0，默认判失败。
- 可为已知、不可避免且有根因分析的 case 建立 allowlist；allowlist 必须包含 issue、
  负责人和复查日期。
- text size geomean 增长不超过 1%。
- 单个 case text size 增长超过 3% 必须解释。
- scheduler 阶段编译时间 P95 增量不超过 5%。
- 内存密集的大 block 不得出现不可接受的超线性编译时间增长。

### 10.4 模型质量 gate

- 静态预测的 scheduler delta 与实机 measured delta 方向一致率至少 90%。
- L1-resident benchmark absolute cycle prediction MAPE 不超过 10%。
- 当模型与硬件方向不一致时，性能门禁以硬件为准，并新增回归样例修正模型。
- 不用 QEMU wall time 校准 Cortex-A53 profile。

### 10.5 CI 分层

#### 普通 CI

- Rust unit/doc tests。
- AArch64/RISC-V compile and functional tests。
- scheduler on/off QEMU correctness differential。
- deterministic assembly/statistics check。
- random small-DAG property tests。

#### 硬件 CI 或定期实验

- XCZU15EG microbenchmarks。
- 端到端 performance suite。
- PMU cycles/instructions 和置信区间报告。
- 与最近稳定 baseline 和指定 release baseline 比较。

硬件结果不稳定或环境元数据变化时标记 invalid，不把噪声当回归。

### 10.6 验收标准

- 普通 CI 对所有 correctness gate 自动化。
- 硬件 benchmark 结果结构化、版本化并可复现。
- performance gate 能阻止有统计显著性的回归。
- 模型预测、代码尺寸、编译时间和实机 cycles 同时进入报告。
- 发布说明准确区分静态模型改进和实机验证收益。

---

## 11. 横向验证任务

这些任务与 M11-M17 并行推进，但必须在 M17 前完成。

### 11.1 `verify_operand_order_stable`

- 在 pre-RA pass 前后采集每条指令的 operand role、constraint 和遍历顺序。
- 允许 pass 删除/替换指令时，以明确的 transform contract 比较，而不是简单比较整个
  flattened table。
- `PeepholeCombine` 的 MAC folding 添加直接验证。
- 失败信息包含 function、block、instruction 和 pass name。

### 11.2 `verify_sched_deps`

- 输入原始 block、调度 permutation 和 dependency graph。
- 验证每条 DAG edge 在新顺序中保持拓扑顺序。
- 使用共享 cycle simulator 验证 latency/resource legality。
- 对 memory/barrier edge 输出可诊断原因。
- debug/test 默认启用；release 可关闭昂贵检查。

### 11.3 Instruction coverage audit

- 枚举所有 `MInst` variant。
- 对每个 variant 标记：operand extraction、flags、memory、barrier、profile、emitted-op
  count、测试覆盖。
- 新增 MInst variant 时测试必须要求更新该 audit，避免静默落入 catch-all barrier。
- catch-all 只作为 correctness fallback，并通过统计观察命中次数；正常 benchmark 中
  常见指令 catch-all 命中应为 0。

### 11.4 编译时间与复杂度

- 记录每 block DAG build 和 schedule 时间。
- 构造 100、500、1000 个 memory node 的 synthetic block。
- 观察 memory history `O(M^2)` 的实际阈值。
- 只有 profiling 显示问题后才引入按 `MemRoot` 分桶、interval structure 或 capped
  scheduling region，避免提前复杂化正确性敏感代码。

---

## 12. 风险与缓解

| 风险 | 严重度 | 缓解措施 |
|------|--------|---------|
| 缺失 register/NZCV/memory 依赖导致误编译 | 高 | 独立 `verify_sched_deps`、随机小 DAG、on/off 差分、保守 catch-all barrier |
| compound pseudo 模型与最终汇编不一致 | 高 | M13 expansion/bundle、emitted-op count 对照 assembly 测试 |
| slot/resource 模型错误导致硬件回归 | 高 | guide 来源、pairing microbenchmark、模型内防退化仅作辅助、实机 gate 为准 |
| PairCombine tombstone 发射真实 Nop | 中 | post-finalize compaction 或独立 tombstone variant、instruction-count gate |
| 过度优化 pair 数量但实际 cycles 回归 | 中 | scalar/pair 独立 profile、实机 benchmark、按类型和地址形式限制 |
| post-RA heuristic 延长 live range | 中 | pressure-neutral tie-break、live delta 统计、保持 spill 数不变 |
| pre-RA scheduler 增加 spill | 高 | 不纳入当前默认范围；另立 milestone 并设 spill gate |
| memory DAG 平方复杂度导致编译时间回归 | 中 | instrumentation、synthetic stress test、按数据决定优化 |
| QEMU wall time 被误用为 A53 性能数据 | 中 | 文档和报告分层，QEMU 仅进入 correctness gate |
| 实机环境频率/温度噪声掩盖结果 | 中 | core pinning、固定频率、paired samples、95% CI、环境元数据 |
| AArch64 配置改动破坏 RISC-V | 中 | compile API 默认配置、双 target CI、RISC-V 空/独立 pipeline 测试 |

---

## 13. 里程碑顺序与交付物

| Milestone | 核心交付物 | 依赖 | 预估 |
|-----------|------------|------|------|
| M11 | MIR pass 开关、codegen 配置、结构化统计、四组合差分基线 | 当前基线 | 1-2 天 |
| M12 | edge-latency critical path、共享 cycle simulator、reference/property tests | M11 | 2-3 天 |
| M13 | pseudo expansion/bundle、Nop compaction、emitted assembly 一致性 | M12 | 3-4 天 |
| M14 | ALU0/ALU1 slot、throughput/occupancy、Div/FP/pair 精确 profile | M13 | 3-5 天 |
| M15 | XCZU15EG benchmark harness、PMU 数据、profile 校准 | M11-M14 | 3-5 天，不含硬件排队 |
| M16 | slot filling、load-use、pair-aware、pressure tie-break heuristic | M15 | 3-5 天 |
| M17 | 端到端 benchmark、CI correctness/performance gate | M11-M16 | 2-4 天 |

推荐严格顺序：

```text
M11 -> M12 -> M13 -> M14 -> M15 -> M16 -> M17
```

`verify_operand_order_stable`、`verify_sched_deps`、instruction coverage audit 和编译时间
instrumentation 可与主线并行，但不得晚于对应 milestone 的验收。

---

## 14. 待决策

- [ ] `-O` 默认映射：采用 `O0=关闭、O1=peephole+pair、O2=全部`，还是保持当前
  默认行为并只增加显式 pass 开关。
- [ ] 是否能稳定访问 XCZU15EG；若不能，确认由哪个环境执行 M15 benchmark package。
- [ ] benchmark 结果存放位置：仓库内版本化 JSON/CSV，还是外部 artifact store。
- [ ] WAR/WAW/NZCV false dependency 是否允许 Cortex-A53 同周期双发；需要 guide 依据或
  microbenchmark 决定。
- [ ] `LoadAddr` 和 long branch 采用显式 MInst expansion，还是 atomic multi-op bundle。
- [ ] PairCombine tombstone 采用 VCode compaction，还是新增不发射的独立 `Removed`
  variant。
- [ ] 是否提供 `generic-aarch64` profile 作为默认，并要求用户显式选择
  `cortex-a53`；推荐提供并保守默认。
- [ ] 硬件性能门禁运行频率：每次 PR、每日、每周或 release 前。

在 M11 开始前只需确定第一项的 CLI 行为；其余决策可在对应 milestone 开始时根据
代码约束和实测条件确认。
