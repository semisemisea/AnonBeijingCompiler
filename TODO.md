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

目标硬件是 Xilinx XCZU15EG 上的 Cortex-A53 MPCore。

---

## 1. 当前实现基线

### 1.1 流水线

当前 AArch64 MIR pipeline：

```text
lowering
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
- `anon_armv8/src/passes/peephole_combine.rs`：vreg use 计数 + MAC 融合，
  使用 `Removed` tombstone（pre-RA DCE 的直接参照实现）。
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

## 2. M18：Pre-RA 死代码消除（DCE）

### 2.1 动机

对 huffman-01 调度前后汇编的对比分析（60/896 行变动，纯重排、指令数不变）
表明：当前后端的热点块被 call 和分支切碎，块内调度收益有限（估计 <2%）。
同时输出中存在明显死代码，删除它们能直接减少指令数，收益确定且大于重排：

1. `movz w3, #0x1` 紧跟 `movz w3, #0x20`：两个不同 vreg 的常量，前者零使用，
   RA 后巧合分配到同一物理寄存器。
2. `mov w13, wzr` / `mov w7, wzr`：从未被使用的 vreg（大概率来自 block param
   或分支参数物化）。
3. 链式死亡：消费者被删后，其生产者随之变死。

### 2.2 已决策事项

| 决策点 | 结论 |
|--------|------|
| 是否删除结果未使用的 load | 是。`Ldr`/`LdrPair` 等结果全死时可删（AArch64 load 架构上无副作用，SysY 语义安全，且死 load 仍占 LSU 和发射槽） |
| 默认优化级别 | `-O1` 起开启（纯收益、无调度风险，与 peephole/pair 同级）；`-O0` 关闭 |
| 阶段 | 只做 pre-RA。观察到的死代码均为 vreg 零使用，pre-RA 可全部捕获；RA 本身不引入死代码，post-RA DCE 预期收益极低 |

### 2.3 实现内容

#### 2.3.1 新增 pass：`anon_armv8/src/passes/dce.rs`（约 200 行）

算法：worklist 驱动的 use-count fixpoint DCE。pre-RA VCode 保持 SSA，
每个 vreg 只定义一次，因此 use-count = 0 即死。

1. 全函数统计每个 vreg 作为 `Use` 操作数出现的次数，参照
   `PeepholeCombine::build_vreg_use_counts`（`get_operands` 遍历 +
   `OperandKind::Use` 过滤）。该 helper 可抽为共享函数或直接复制。
2. 扫描全部指令：指令属于可删白名单、至少有一个 def、且所有 def 的 vreg
   use-count 均为 0 → 标记为 `MInst::Removed`；同时把该指令所有 use 操作数
   的计数递减，计数因此归零的 vreg 的 def 指令加入 worklist。
3. 处理 worklist 直至为空（处理链式死亡）。

多 def 指令（如 `LdrPair`）：必须所有 def 都死才可删；任一 def 存活则整条保留。

#### 2.3.2 可删白名单（纯指令 + dead load）

- 整数 ALU：`AluRRR`、`AluRRRR`、`AluRRImm12`、`AluRRImmLogic`、
  `AluRRImmShift`、`AluRRRShift`、`AluRRRExtend`。
- 乘除：`SDiv`（AArch64 除零不 trap，返回零，删除安全）、`SMulL`、`MAdd`、
  `MSub`。
- 数据移动与常量物化：`Mov`、`MovPhys`、`LoadImm`、`MovZ`、`MovN`、`MovK`、
  `MovFromZero`。
- 地址计算：`LoadAddr`（ADRP+ADD，纯）、`StackAddr`。
- 选择：`CSet`、`CmpSelect`（cmp+csel 是原子单位，flags 内部消化，dst 死则
  整条可删）。
- 纯 FP：`FMov`、`FMovFromZero`、`FAlu`、`Scvtf` 等不改变全局状态的 FP 操作。
- dead load：`Ldr`、`LdrPair` 等所有 load variant，结果全死时可删。

#### 2.3.3 永不删除

- terminator：`BCond`、`Cbz`、`Cbnz`、`Tbz`、`Tbnz`、`CondBr`、`Jump`、`Ret`
  （pass 不变式要求 terminator 保持在 block 末尾）。
- `Call` / tail call（副作用、返回值约定）。
- 所有 store：`Str`、`StrPair` 等（内存副作用）。
- flags 定义者：`CmpRR`、`CmpImm`、`FCmp`（隐式定义 NZCV，无 vreg def，但
  被后续 BCond/CSet 消费，绝不能删）。实现规则：**没有任何 def 的指令一律
  跳过**，天然覆盖此类。
- `Nop`：保留真实指令语义（M13 决策，`Removed` 才是 tombstone）。
- `MInst::Removed`：跳过。
- 任何无法确认纯性的 variant：保守保留，宁漏勿错。

#### 2.3.4 pass 不变式

- 遵守 `MIRPass` contract（`taki_mir/src/passes.rs`）：不改变剩余指令的
  operand 遍历顺序；`Removed` tombstone 已有 pre-RA 先例（PeepholeCombine），
  RA 与 verifier 均可处理。
- 不触碰 CFG side tables，无需 `recompute_cfg`。
- `run` 返回是否有指令被标记为 `Removed`。

#### 2.3.5 基础设施接线

- `anon_armv8/src/config.rs`：`AArch64CodegenConfig` 增加 `dce: bool`。
- `anon_armv8/src/passes/mod.rs`：`build_pipeline` 中 DCE 注册在
  PeepholeCombine **之前**（先清死代码，peephole 看到更干净的指令流；
  peephole 融合要求 single-use，不会新产生死代码，无需第二轮 DCE）。
- `soyo_compiler/src/cli.rs`：
  - 新增 `--enable-mir-dce` / `--disable-mir-dce`；
  - `aarch64_codegen_config()` 映射：`-O0` 关，`-O1`/`-O2` 开；
  - 显式 flag 覆盖 `-O` 的既有优先级规则同样适用于 DCE；
  - RISC-V target 拒绝该 flag（与现有 MIR flag 行为一致）；
  - 更新现有 CLI 配置解析测试，补充 DCE 组合。
- `taki_mir/src/stats.rs`：新增 `DceStats { ran, changed,
  instructions_removed }`，挂入 `FunctionCodegenStats`，并纳入编译单元级
  聚合。

#### 2.3.6 测试

单元测试（`dce.rs` 内或配套测试文件）：

- 死 `MovZ`/`Mov` 被标记为 `Removed`。
- 活依赖链（def 有真实使用）完整保留。
- `Call`、store、`CmpRR`/`CmpImm`、terminator 一律保留。
- `LdrPair` 双 def：一个存活则整条保留；两个全死则删除。
- dead `Ldr` 被删除。
- 链式死亡：a 只被死指令 b 使用，b 删后 a 也被删（fixpoint）。
- 无 def 指令（Cmp 类）不触发 panic、不删除。
- pass 关闭时（`dce: false`）VCode 完全不变。

端到端验证：

- 重编 `results/perf/huffman-01.sy`：确认三类死代码消失、text 指令数下降，
  并与 scheduler on/off 组合交叉验证（DCE 与调度正交）。
- `cargo test --workspace` 全绿（15 个 test suite）。
- 5 个 functional case 在 O0/O1/O2 下编译并做正确性回归。
- RISC-V O0/O2 编译不受影响。
- 确定性：相同输入相同配置多次编译 byte-identical。

#### 2.3.7 文档与提交

- `TODO.md`：M18 完成后删除本节细节。
- `README` 优化级别表增加 DCE 列（O0 关 / O1 开 / O2 开）。
- 独立提交，信息遵循 `[Feat(AArch64)]: ...` 风格；提交前
  `cargo fmt --all -- --check` 与 `cargo test --workspace` 通过。

### 2.4 验收标准

- huffman-01 中 `movz w3,#0x1`（后接 `movz w3,#0x20`）、`mov w13,wzr`、
  `mov w7,wzr` 等死代码不再出现。
- `-O1` 输出的 text 指令数严格小于等于 `-O0`，且无任何功能回归。
- 全部既有测试通过，RISC-V 不受影响。

---

## 3. 后续候选工作

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

## 4. 总体执行原则

1. 每个 milestone 独立提交；`TODO.md` 在 milestone 完成后删除对应已完成
   细节，只保留后续工作。
2. 正确性验证、静态模型测试和实机性能测量分层维护，不互相替代。
3. 所有 AArch64 改动必须同时验证 RISC-V 不受影响。
4. 新 pass 默认走"白名单 + 保守保留"策略，宁漏勿错。
5. 未获得实机数据前，只能声称"静态模型改进"。

---

## 5. 风险与缓解

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
