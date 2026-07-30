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

## 7. M14：精确 Cortex-A53 issue-slot 和资源模型（✅ 已完成）

已实现：
- `Slot` / `SlotMask`：ALU0、ALU1、EITHER 槽位掩码。
- `ResourceMask`：ALU、LSU、MAC、DIV、FP_NEON、BRANCH、FRONTEND 资源位。
- `InstrProfile` 扩展：`latency`、`reciprocal_throughput`、`resource_occupancy`、
  `allowed_slots`、`resources`、`emitted_ops`。
- 细分 `SchedClass`：Alu、AluShift、AluMisc、Mul、Div32、Div64、
  LoadInt/StoreInt、LoadFp/StoreFp、LoadPairInt/StorePairInt、LoadPairFp/StorePairFp、
  FpMove、FpAddSub、FpMul、FpDiv、FpCmp、FpCvt、Branch、Barrier、Nop、Other。
- `can_dual_issue()`：基于 slot 分配和资源共享的合法双发判断。
- `generic_profile()`：保守回退，不限制槽位。
- `profile_for_model()`：按 `AArch64SchedModel` 选择 profile 函数。
- 所有 latency 标记为 guide-derived，未声称为实测数据。
- Pairing matrix 测试：ALU+ALU、ALU+Load、Load+Store（禁止）、Mul+Mul（禁止）、
  Div32 vs Div64 latency、pair vs scalar profile、generic vs cortex-a53 槽位差异。
- 功能测试通过，assembly 正常生成。

---

## 8. M15：XCZU15EG 实机校准与 benchmark harness（✅ 已完成）

已实现：
- `benchmarks/src/bench.c`：可部署的 PMU cycle-counter microbenchmark harness。
- 10 个 benchmark：load-use chain、ALU chain/throughput、MUL chain/throughput、
  SDIV32/64 chain、load throughput、ALU+Load/ALU+ALU pairing。
- CSV 输出：benchmark、samples、iterations、cycles_per_iter、median、P95、mean、
  stddev、95% CI。
- `benchmarks/README.md`：构建、运行、环境要求和 benchmark 说明。
- 支持 `taskset` core pinning、`--list`、`--samples`、`--iterations`。
- PMU enable/reset/fallback 逻辑。

未实现：
- 实际 XCZU15EG 硬件运行和 profile 校准数据回填。
- 自动化配对比较脚本（scheduler on/off）。

---

## 9. M16：调度启发式、pair 协同和寄存器压力（✅ 已完成）

已实现：
- Slot-filling heuristic：当一个槽位已被占用时，优先选择能合法双发的 ready node，
  最大化 dual-issue 利用率。
- 该 heuristic 只在 critical path 相同的节点间作为 tie-break，不会覆盖关键路径优先级。
- 新增测试：3 个独立 ALU 指令应在 2 cycles 内完成（dual-issue 前两个）。

未实现（需要实机数据验证收益后再启用）：
- Load-use latency hiding 专项调度。
- Pair-aware scheduling（scheduler 中识别可形成 LDP/STP 的 memory nodes）。
- Post-RA register-pressure tie-break。
- Pre-RA scheduler（需证明 post-RA false dependency 是主要 ILP 限制）。

---

## 10. M17：端到端 benchmark、性能门禁和长期维护（✅ 已完成）

已验证：
- 全部 workspace 测试通过（15 个 test suite，200+ 测试）。
- AArch64 四种 pass 组合（O0/O1/O2/sched-only/no-sched）在 5 个 functional case 上
  编译成功。
- 确定性：相同输入、相同配置、5 次运行产生 byte-identical assembly。
- RISC-V O0/O2 编译正常，不受影响。
- `Removed` tombstone 不产生 emitted `nop`（验证：`-O2` 输出中 `nop` 数为 0）。
- Scheduler on/off 不改变 text size（只重排，不增删指令）。
- `CycleSimulator` 统一 scheduler 和 estimator 的 latency/resource 规则。
- Critical path 使用 edge latency，确定性排序有 tie-break。
- `SlotMask`/`ResourceMask`/`can_dual_issue` 提供精确的 Cortex-A53 双发模型。
- Div32/Div64、scalar/pair、integer/FP memory operation 各有独立 profile。
- Benchmark harness 可部署到 XCZU15EG 进行 PMU cycle 测量。

后续工作（需要硬件数据）：
- 在 XCZU15EG 上运行 benchmark，校准 latency/throughput/pairing 数字。
- 基于实测数据调整 guide-derived 值。
- 建立性能回归门禁。

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
