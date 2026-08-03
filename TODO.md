# Cortex-A53 后端优化计划

本文档只记录尚未完成的工作。已完成里程碑只保留一行摘要，历史设计与实现细节
以 Git 提交记录和代码测试为准，不在这里重复维护。

> 进行中：§2 主计划 D（M47 `reduction_unroll` / M48 `invariant_reduction_hoisting`）
> 已完成。下一优先级见 §3 SIMD Phase 2（M42-M46，搁置中）与 §4 后续候选。

## 已完成里程碑摘要

- **M1-M17**：MIR pass 基础设施、发射前 finalize、ABI 参数布局共享、pre-RA
  PeepholeCombine（MAC 融合）、post-RA PairCombine（LDP/STP）、依赖 DAG +
  Cortex-A53 list scheduler、可切换 pipeline（-O0/1/2）、CycleSimulator、
  `Removed` tombstone、slot-filling 双发启发、PMU harness、确定性门禁。
- **M18**：pre-RA DCE（worklist use-count 定点，白名单制，-O1 起默认开启）。
- **M19-M24**：入口参数 fixed-register live-in（`Args` 伪指令），消除 ABI
  home-slot store/load 往返；AArch64 + RISC-V × -O0/1/2 ABI 矩阵与 5 次确定性门禁。
- **M25-M27**：EmitBuffer 文本缓冲、分支优化四规则（R1-R4）、veneer 范围松弛
  （BRANCH14/19/26 与 RISC-V B/JAL）。
- **M28-M29**：分支发射重构（对照 Cranelift MachBuffer）——RISC-V `CondBr`
  slot 化、`beqz/bnez` 收敛、veneer 全覆盖、buffer 行为测试与门禁。
- **M30**：基准与验证基建（`scripts/perf_compare.sh` + `results/perf_compare/`）。
- **M31**：if-conversion 推广（循环累加器，`head` 支配 `merge`）+ land/lor 折叠为
  `band/bor`。huffman-01 706 → 686。
- **M32**：`CondResult` 抽象 + `MInst::CCmp`（`cmp; ccmp; csel/cset/b.cc`，
  And=`#0,eq`、Or=`#4,ne`；ccmp 立即数 5 位、`Le` 的 `nzcv_making_cond_false`=`#0`）。
  huffman-01 686 → 640。
- **M33**：循环旋转 `rotate_loops`（头测试下沉 latch）+ 标志融合
  `SubsRRImm12`/`AndsRRImmLogic`/`TstRRImmLogic` 三条规则。回边
  `subs wX,wX,#1; b.eq/ne` 与 clang 同构。
- **M34**：GSP（标量全局提升，程序级可能触及分析）+ LICM（自然循环 + 前驱头
  提升）+ 作用域化 load-CSE。huffman-01 648 → 599；`read_bits` 149 条 < clang 161。
- **M35**：`Mov` 32 位宽度（i32 拷贝 `mov w,w`）；回边 blockparam 拷贝诊断完成
  （消除路径见 §4.7）。
- **M36**：内联代价模型（`estimate_size` × 调用点数预算 ≤ 100，递归环守卫）。
  huffman-01 599 → 580；`read_bits` 内 `rotlN` 全内联、热循环无 `bl` 无栈帧。
- **M37**：if 链 → switch 决策树（`chain_to_switch`，AArch64 专用）+ `chain_fusion`
  （pre-RA 删 split 块比较）。`rotrN/rotlN` 最坏 4 次 cmp；静态 580 → 600（动态
  cmp 深度变好）。
- **M38**：TCO 扩展 + 死空块清理（`remove_trivial_jump_block`），`output_data`
  无栈帧、尾调用形式、无死空块跳转。
- **M39**：向量类型 + `RegClass::Vector` 基础设施——`LoweredType` 向量位
  （`V4I32/V2I64/V4F32/V2F64`，marker bit 与标量不相交）、`VecMov`（`mov v,v`）、
  `MemoryType::Vec128`（`ldr/str q`）、Vector spillslot 2×8B、`machine_env`/
  `is_callee_saved`/`preg_name` 补 Vector（v8-v15 callee-saved）、拆 vcode
  move/spill 三处 panic。验收：显式向量值 RA（含 spill/move）无 panic、
  `cargo test --workspace` 全绿、双 target × -O0/1/2 5 次 byte-identical。
- **M40**：NEON MInst 全集——`VecLd1/St1`、`VecDup`、
  `VecArithRRR`、`VecFmla`、`VecBitwise`、`VecCmp`、`VecBsl`、`VecCvt`、
  `VecAddv`、`VecMovImm`、`VecExtractLane/InsertLane`、`VecMinMax`，emit/DCE/
  sched 全接入；`emit_vcode_assembly` 公开接口 + 直构 VCode→NEON 汇编端到端测试。
  ISel 收口（向量 IR 入口 + lower 分派）见 M40b。
- **M40b**：向量 IR 入口 + 完整 NEON ISel lowering——`Fma`/
  `VectorSplat`/`VectorExtractElement`/`VectorInsertElement`/`VectorReduce` 五个
  inst kind + `Binary/Cast/Select` 接受向量类型 + `BinaryOp::Min/Max`；`lower.rs`
  向量分派（add/sub/mul/and/or/xor/eq/gt/min/max/cvt/bsl/dup/lane/reduce）、
  `ldr/str q`（`MemoryType::Vec128`）+ 完整 AMode、`emit_zero_init` 向量零初始化；
  `VecFmla/VecBsl/VecInsertLane` 改为显式读操作数 + 自发射前导 copy（early def，
  SSA 单 def）。RISC-V/frontend 防御臂。验收：端到端向量函数（v0-v7 ABI）生成
  NEON 汇编、`-O0/1/2` × 5 次 byte-identical、双 target 标量回归。
- **M41**：向量 ABI——`ArgLayoutPlanner` Vector bank、向量参数
  占 NEON v0-v7/溢出 16B 槽、返回 v0、`DEFAULT_CLOBBERS` 含 NEON、callee-saved
  v8-v15 按 16B 槽保存恢复；ABI 单测通过。收尾见 M41b。
- **M41b**：向量 SchedClass（`VecArith/VecMul/VecFmla/VecLoad/VecStore/VecMov`，
  A53 NEON 参考值，与 FP 共用 FP_NEON pipe）+ `sched/dag.rs` 归入新类；向量功能
  用例与 QEMU 向量差分推迟到 Phase 2 向量化入口（见 §3）。
- **M47**：`reduction_unroll` 标量多累加器 + 4× 部分展开——识别单 BIV、单累加器、
  单纯块、无 store/call 的归约循环（纯 `acc±E` 与 if_conversion 产物
  `select(c, acc±E, acc)` 两种形态），preheader 加 `T>=4` 版本化守卫，主循环用
  4 个独立累加器打破串行累加依赖链，原循环保留为标量 epilogue 收 `T%4` 余数；
  bound 须严格支配循环头。验收：many_mat_cal-1 平方和循环出现 4 条独立 `madd`；
  functional+h_functional 149/149、perf 60/60（-O1/-O2）、byte-identical
  5×（-O0/1/2 × 双 target）、RISC-V 全量回归通过。
- **M48**：`invariant_reduction_hoisting` 外层不变归约外提——识别纯、不读外层
  计数器的嵌套归约巢（many_mat_cal 的 R 循环体），把整巢以 `acc=0` 克隆到
  preheader 跑一次得 `D_total`，外层循环体退化为 `acc += D_total`（fresh latch，
  只引用可达的 header 参数；计数器更新与步长从原 latch 提取）。验收：
  many_mat_cal-1/2/3 R 循环消失（或退化），qemu 下 ~82s → ~2s；functional+
  h_functional 149/149、perf 60/60（-O1/-O2）、byte-identical 5×、RISC-V 全量
  回归通过。
- **主计划 A（M30-M38）**：huffman 类基准性能重构全部完成，设计依据 Cranelift
  机制移植 + 以 clang 为参照的 if-conversion/ccmp/subs 融合。
- **RISC-V 栈参数修复**：非对齐访问 + psABI widened-to-XLEN 槽宽（见附 A，FPGA
  实机复跑待验证，见 §4.9）。
- **RISC-V 跑分长耗时分析**：已完成并归档（见附 B），候选行动项并入 §4.10。

huffman-01 静态指令数（awk 方法）基线：M26 687 → M34 599 → M36 580 → M37 600
（决策树静态 +20、动态 cmp 深度变好），详见 `results/perf_compare/`。

---

## 1. 当前执行基线

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
  -> assembly emission        (EmitBuffer 文本缓冲，M25 起)
```

IR 优化管线（`raana_ir/src/opt/pass.rs` 定点循环）：
SSA → Inline → TCO 之后，固定点内：IPSCCP、SimplifyCFG、GVN、SR（强度削减）、
IfConversion、TCO、BooleanSimplification、GVNPRE、DeadPhiElim、DCE。相关 pass
见 `raana_ir/src/opt/passes/`。

关键代码：

- `raana_ir/src/opt/pass.rs`：`Pass` / `PassesManager`（`aarch64()` vs `default()`）。
- `raana_ir/src/opt/passes/`：IR pass（`if_conversion`/`rotate_loops`/`licm`/
  `scalar_global_promotion`/`chain_to_switch`/`inline`/`simplify_cfg`/`tco` 等）。
- `raana_ir/src/opt/analysis_passes/`：`loop_analysis`/`induction_variable`/
  `dom_tree`/`cfg`（向量化依赖，见 §3）。
- `anon_armv8/src/passes/mod.rs`：按 `AArch64CodegenConfig` 注册 MIR pass。
- `taki_mir/src/passes.rs`：`MIRPass` trait、pre-RA/post-RA 两阶段 pipeline。
- `anon_armv8/src/instructions.rs`：`MInst` 枚举（约 60 个 variant）。
- `anon_armv8/src/lower.rs`：ISel（`lower_inst` 分派、`lower_select`/`lower_load`
  等）。
- `anon_armv8/src/regs.rs`、`abi.rs`：寄存器集 / AAPCS64 ABI。
- `taki_mir/src/types.rs`：`LoweredType`（SIMD lane 位已预留，见摘要 M39）。
- `taki_mir/src/reg_alloc/reg.rs`：`RegClass::{Int,Float,Vector}`。
- `anon_armv8/src/sched/aarch53.rs`：FP_NEON pipe 与 `Fp*` SchedClass。
- `taki_mir/src/emit_buffer.rs`：EmitBuffer（M25/M26/M27）。
- `soyo_compiler/src/cli.rs`：`-O` 映射与 `--enable/disable-*` 开关。

### 1.2 现状与差距

huffman-01 各函数差距（`read_bits`/`rotlN`/`output_data` 等）在 M30-M38 逐项
收敛，分函数差距表已随各里程碑完成归档，当前基线见 `results/perf_compare/`。
剩余的通用优化方向见 §4 后续候选工作；热循环标量优化（§2 主计划 D）已完成，
SIMD 见 §3。

### 1.3 当前结论边界

- 静态 estimator 只用于确定性回归和相对启发式比较，不能替代实机测量。
- scheduler 与 estimator 共用同一模型，"模型内不退化"不等于"硬件不退化"。
- QEMU 仅用于语义差分，不能证明 A53 的 dual-issue / latency / throughput 收益。
- 未获得实机数据前，文档和提交信息只能声称"静态模型改进"，不能声称
  "XCZU15EG runtime 提升"。

### 1.4 实机超时排查（已归档）

针对"实体机 TLE / qemu 正常"的排查结论（详见 git 提交 2ef0555）：

- **死循环：未发现**。209 个用例（functional/h_functional/perf）在 qemu 下全部
  确定性跑完；同一 ELF 在 qemu 与实机的指令语义一致，死循环若存在会在 qemu
  同样出现。`while (getint())` 等输入循环都靠输入末尾显式 0 终止，不依赖 EOF。
- **架构差异：仅一处，已修复**。内嵌 memzero 曾用 `id_aa64isar2_el1` 探测 MOPS
  （ARMv8.6 `setp/setm/sete`）。已删除 MOPS 分支，输出 100% ARMv8-A。
- **TLE 根因：性能差，属 IR 层优化缺口，非后端指令选择问题**。以 gcc -O2 为
  基线（qemu 实测，单输入）：huffman-01 ~3.5x、LUDCMP(h-5-01) ~2.1x、sl1/
  matmul1/03_sort ~1.6-1.8x、many_mat_cal-1 ~400x（gcc 将 R 外层循环内的整个
  T×T 内层循环识别为循环不变、外提后按 trip count 乘一次，我们逐次重算
  15.7e9 次；该差距已由 §2 主计划 D 的 M48 消除）。后端内层循环本身已足够紧
  （4-18 条，多与 gcc 持平或更短）。

---

## 2. 主计划 D：热循环标量优化（已完成）

M47 `reduction_unroll`（标量多累加器 + 4× 部分展开）与 M48
`invariant_reduction_hoisting`（外层不变归约外提 + 退化体）已全部落地并验收，
详见"已完成里程碑摘要"M47/M48 与 Git 历史，本节不再维护。many_mat_cal-1/2/3
的 R×T² 平方和热点从 ~1.5×10¹⁰ 元素操作降为单次 ~10⁶ + R 次平凡累加，qemu 下
~82s → ~2s。合规审计与实现护栏记录于 M47/M48 提交信息；§4.11 保留 M48 泛化
（容忍幂等写的整体循环巢外提）候选。

## 3. 主计划 C：SIMD/NEON 支持（M42-M46，搁置中）

### 3.1 Phase 1（M39-M41b）：机器层显式 NEON 通路（已完成）

向量类型 + `RegClass::Vector`、NEON MInst 全集、向量 ABI、向量 IR 入口 + 完整
ISel lowering、向量 SchedClass 已全部落地且验收通过，详见"已完成里程碑摘要"
M39-M41b。参考澄清：Cranelift 的 SIMD 是**显式降层**（wasm `v128` → NEON），
没有任何 loop unroll / vectorizer pass；clang/gcc 才做自动向量化
（loop vectorizer + SLP + unroll）。

### 3.2 Phase 2：IR 层自动向量化

> 搁置（2026-08 调整）：§2 主计划 D 已完成后，本节为下一优先级。

#### M42：循环依赖 / 别名分析（向量化合法性前置）

- 现状缺口：调度用保守别名模型（`anon_armv8/src/sched/dag.rs`）只能保正确，不能
  证明"循环无携带依赖、load/store 可向量化"。
- 新增 `raana_ir/src/opt/analysis_passes/dependence.rs`：基于访问函数（GEP 仿射
  index）做循环级依赖分析——同一迭代内与跨迭代的 load/store 冲突（reuse
  distance / gcd 测试）；输出每循环"可向量化 / 可归约 / 禁止"判定与原因。
- 复用已有分析：`loop_analysis`（自然循环 + preheader）、`induction_variable`
  （Add/Sub 步进）、`dom_tree`、`cfg`。
- 验收：对 many_mat_cal / conv2d-1 / matmul 内层循环能正确判定；宁漏勿错，无法
  证明一律保守拒绝（误报 = 0）。

#### M43：loop versioning / 运行时 guard

- SysY 的 trip count 与数组对齐编译期未知 → 向量化循环必须版本化：
  `if (n >= VF && aligned16(a) && aligned16(b)) { 向量主循环 } else { 标量回退 }`。
- 输出形状：标量入口 + 向量主循环（trip count 取下取整到 VF 的倍数）+ 标量
  epilogue（余数）；对齐检查可用 `tst x, #15` + 条件分支。
- 与既有 `rotate_loops`/`if_conversion` 交互：版本化在循环旋转后做，向量主循环
  体内条件已转 select。
- 验收：n < VF、未对齐、n % VF ≠ 0 边界用例 QEMU 差分正确；on/off（-O0 标量）
  差分无行为差异。

#### M44：loop vectorizer（核心 pass）

- 文件：`raana_ir/src/opt/passes/loop_vectorize.rs`（AArch64 专用注册，仿
  `chain_to_switch` 的 `PassesManager::aarch64` 分支）。
- 识别条件：可计数（trip count ≥ VF 可证或 versioning）、单出口、无 break、IV
  仿射（Add/Sub 步进，`induction_variable`）、体为纯标量运算 + load/store。
- 变换：
  - IV：步进 ×VF，循环条件按向量迭代计数；
  - load/store：连续 offset 的标量 load/store 合并为 `VecLd1/VecSt1`；
  - Binary/Arith → 对应向量 op；条件（select）→ `VecCsel/bsl`；
  - 归约累加器（`sum += a[i]`、`C[i][j] += A[i][k]*B[k][j]`）→ `fmla` 向量累加，
    退出前水平归约（`addv`）；
  - 对齐由 M42 分析结果 + M43 versioning 保证。
- 前置依赖：M42（依赖分析）、M43（versioning）、既有 `rotate_loops`/
  `if_conversion`/`licm`/`pointer_strength_reduction`。
- 验收：many_mat_cal / matmul 内层出现 `fmla` 与 `ld1/st1`；全 corpus QEMU 差分 +
  on/off 差分；-O0/1/2 × 双 target 5 次 byte-identical。

#### M45：SLP 基本块向量化 + 循环展开

- SLP（`raana_ir/src/opt/passes/slp.rs`）：把同一基本块内相邻、类型一致的独立
  标量运算打包为向量 op（配对 add/mul/load/store 的菱形结构）；补 loop
  vectorizer 覆盖不到的直通代码（conv2d 邻域、展开后的短链）。
- 循环展开（`raana_ir/src/opt/passes/loop_unroll.rs`）：常数 trip count（≤ 阈值）
  的小循环全展开；非常数循环按 2-4 倍部分展开，为 SLP 提供相邻迭代、为 A53
  双发射暴露 ILP（与 post-RA ListScheduler + slot-filling 配合）。
- 顺序：vectorize（M44）→ unroll → SLP；或先小规模 unroll 再 SLP（按基准数据定）。
- 验收：conv2d-1 内层出现 `ld1/fmla/st1`；静态指令数与 gem5 sim_insts 对照 clang
  记录在 `results/perf_compare/`。

#### M46：收尾与回归门禁

- 双 target × -O0/1/2 全量编译 + 5 次 byte-identical；functional/h_functional/
  perf 全量 QEMU 差分；on/off 差分无行为差异。
- `scripts/perf_compare.sh` 增加 SIMD 列；对 many_mat_cal/conv2d/matmul 记录静态
  指令数与 gem5 sim_insts 相对标量基线的变化（按 §1.3 原则，不声称实机收益）。
- 调度验证器（§4 P2 `verify_sched_deps`）覆盖向量 NZCV / 寄存器依赖。

### 3.3 关键不变量（SIMD）

1. 向量化只对 M42 依赖分析可证安全、或经 M43 运行时 versioning 保证的循环进行；
   无法证明时保守拒绝（宁漏勿错）。
2. `-O0` 保留标量形式作 on/off 差分基线；向量化/展开规则由 `-O1/-O2` 控制；
   M39-M41 机器层能力是 ABI/codegen 架构改动，任何优化级别都必须正确。
3. 向量化 pass 只在 AArch64 注册（同 `chain_to_switch`）；RISC-V 保持标量，
   双 target 回归。
4. 对齐未知时用非对齐 `ldr/str q`（PE=0 容忍，与 clobber 保存同路径）或非对齐
   `ld1/st1`，或 versioning，绝不在编译期假设 16 字节对齐。
5. 每个 milestone 独立提交，完成后删除 TODO 细节只留一行历史；静态模型改进
   不声称实机收益（§1.3）。

---

## 4. 后续候选工作

按预期收益排序，均需在前一项验证后再启动。被主计划 A 覆盖的旧条目
（`&&`/`||` flags 融合、phi 拷贝 coalescing、循环不变 load 外提）已并入
M31-M35，不再单列。

### 4.1 P1：常量 / 分支参数物化源头治理

DCE 是兜底；更优解是 lowering 时就不为未使用的 block param 和分支参数物化
常量与 `mov wzr`。DCE 落地后统计 `instructions_removed` 的构成，若某类来源
占主导，直接在 `taki_mir/src/lower.rs` 或 `anon_armv8/src/lower.rs` 消除源头。

### 4.2 P2：调度验证器闭环

- `verify_operand_order_stable`：保护 pre-RA pass 的 operand traversal
  contract。
- `verify_sched_deps`：独立于 scheduler 重放调度后的 register、NZCV、
  memory 和 barrier 约束。
- 小 DAG reference simulator / property tests。

### 4.3 P2：内存 DAG 复杂度

长块最坏 `O(M^2)`。已有统计，先采集编译时间数据，确认是实际问题后再引入
按 root/range 分组的数据结构。

### 4.4 P2：调度启发式增强（需实机数据证明收益）

- Load-use latency hiding 专项。
- Pair-aware scheduling（调度时考虑 LDP/STP 形成）。
- post-RA register-pressure tie-break。
- pre-RA scheduler（需先证明 post-RA false dependency 是主要 ILP 限制）。

### 4.5 P2：冷块沉底与布局

Cranelift `BlockLoweringOrder` 的 `cold_blocks` 机制（`blockorder.rs:87-90,
260-265`）把冷块沉到函数末尾；配合 M25-M29 的 EmitBuffer，冷块天然获得
fallthrough 收益。SysY 前端暂无冷热信息，本期仅在 `BlockLoweringOrder`
预留 `is_cold()` 接口。相关遗留：`CmpImm(0)+CondBr{Ne}`→`cbz` 融合未做。

### 4.6 P3：XCZU15EG 实机校准（依赖硬件访问）

- 运行 `benchmarks/src/bench.c`，校准 latency / throughput / pairing 数据。
- 基于实测调整 guide-derived profile 值。
- 建立性能回归门禁。
- 回答：WAR/WAW/NZCV false dependency 是否允许 A53 同周期双发。
- 用 M19-M24 的参数入口 microbenchmark 与 M25-M29 的 huffman 差分量化
  实际收益（实机数字待测）。

### 4.7 M35 遗留：回边 blockparam 拷贝消除（候选）

ion `merge_vreg_bundles` 的 blockparam-out 合并已触发且正确；`_and/_xor/_or`
回边 3 条 `mov w,w` 是语义必需（旧值读在旋转后新值定义之后，活区间真实相交）。
消除路径：

- (a) 循环体重排——把旧值读取（bit 计算）提到新值定义之前（IR/MIR 层，可使
  回边零 mov）；
- (b) ion 活区间按块参数 in/out 拷贝分裂（regalloc2 half-move 语义）。

验收：`_and` 循环回边零 mov。

### 4.8 分支发射遗留（主计划 B 完成后的剩余项）

- RISC-V `CondBr` 冷块沉底未做（只在 `BlockLoweringOrder` 预留 `is_cold()`
  接口）。
- `CmpImm(0)+CondBr{Ne}`→`cbz` 融合。

### 4.9 RISC-V 栈参数 FPGA 实机复跑（附 A 收尾）

修复已实现（非对齐 131→0 处），**FPGA 实机复跑仍待验证**：`h_functional/
39_fp_params` 在 BOOM 实机复跑确认 WA/RE 消除。

### 4.10 RISC-V 跑分候选（附 B 收尾，按通用性 × 收益 × 风险）

- P0 发射层纯改进（覆盖所有热循环，每回边省 2+ 条）：A1 局部跳转
  `la+jr→j`（超范围走既有 veneer）；A2 `li+ALU`→立即数指令（`addiw/slti`，
  `li 0` 用 zero 寄存器）；A3 `slt+beqz→blt`。风险低，指令数可直接统计。
- P1 IR 层地址 SR 补全：元素地址指针递增、不变行基址 hoist、全局基址
  提升进循环前寄存器（覆盖 A4 全家 + A7 的基址重载）。
- P2 `maddw` 融合（M 扩展）；内层调用内联（huffman/crc/fft1，查
  specialization 未内联原因）；A6 累加器 phi 拷贝消除。
- P3 conv2d 边界检查半条件 hoist（依赖 LICM 条件部分提升能力）。
- SIMD（§3）对 many_mat_cal / conv2d / matmul / transpose 的向量并行是
  A4/A5 标量优化之后的下一层收益来源。

### 4.11 泛化：整体循环巢外提（容忍幂等写）（M48 后续）

M48 的"零 store"守卫拒绝了 conv2d `repeat` 外层（巢内写 `Out`，但每轮写入
相同位置、相同值，即**幂等写**，可安全外提）。泛化方向：识别"循环巢整体相对
外层不变 + 写集幂等"后整体外提，节省 `repeat_factor` 倍工作量。判定较复杂
（需幂等写证明），列为后续，本期不做。

---

## 5. 总体执行原则

1. 每个 milestone 独立提交；`TODO.md` 在 milestone 完成后删除对应已完成
   细节，只保留后续工作。
2. 正确性验证、静态模型测试和实机性能测量分层维护，不互相替代。
3. 所有 AArch64 改动必须同时验证 RISC-V 不受影响。
4. 新 pass 默认走"白名单 + 保守保留"策略，宁漏勿错。
5. 未获得实机数据前，只能声称"静态模型改进"。
6. ABI/codegen 架构改动（M19-M24、M25-M29、M30-M38、M39-M46）不由优化 flag
   控制，任何优化级别都必须保持正确；优化规则本身由 `-O` 控制。
7. 发射层改造以行为等价为第一优先级，优化规则在等价基线上逐步开启。

---

## 6. 风险与缓解

| 风险 | 严重度 | 缓解措施 |
|------|--------|---------|
| 向量化依赖分析误判导致语义错误 | 高 | 保守依赖分析（gcd/reuse）+ 只向量化可证安全循环 + on/off 差分 + 全量功能回归 |
| 向量寄存器压力导致大量 spill | 中 | Vector 独立分配域 + v8-v15 优先 + 先小 VF 验证 |
| 对齐假设错误导致 SIGBUS | 中 | 已知对齐才用对齐 `ld1/st1`；未知走非对齐或 versioning |
| RISC-V 无 NEON 引入回归 | 中 | 向量化 pass 按 target 注册；双 target 回归 |
| regalloc ion 对 Vector class 支持缺口 | 中 | 先拆 vcode move/spill panic + ion 单测，再启用向量化 |
| 显式向量类型污染标量流水线 | 中 | 类型下沉到 MIR 后由 `reg_class_for_type` 分流，标量路径不动 |
| if-conversion 投机上提改变执行语义（除 div/rem 外算术无副作用，风险低） | 中 | 仅纯整数算术 + head 支配 merge 才转换；on/off 差分 + 全量功能回归 |
| GSP 提升全局破坏跨函数可见性 / 与调用交互 | 高 | 白名单（仅无取址、无"可能触及"调用的标量全局）；出口统一回写；保守宁漏勿错 |
| `ccmp` 链破坏 NZCV 使用顺序 | 中 | 条件仅限单用纯比较；emit 单测；on/off 差分 |
| RA 拷贝消除与并行拷贝求解器交互导致确定性回归 | 中 | 5 次 byte-identical 门禁；redundant_moves 语义保留 |
| 内联膨胀增加编译时间与代码体积 | 中 | 代价估计 + 阈值 + 深度限界；corpus 编译时间监控 |
| 别名链成环 / 截断后标签簿记错误 | 高 | 完整移植 Cranelift 不变量；专项单测；on/off 差分 |
| 多指令 MInst slot 化破坏"每 slot 4B"假设 | 中 | slot 粒度 = 单条指令；verify 断言发射 slot 数 == 指令数 |
| veneer 插入改变偏移导致松弛不收敛 | 中 | 单调性（只增不减）+ 快照收集/倒序插入 + 每轮全量范围断言 |
| 分支优化与 post-RA ListScheduler 交互 | 低 | 调度在 vcode 层（块内），EmitBuffer 只在块边界截断，块内顺序不变 |
| 汇编器对超范围分支报错 | 低 | `resolve` 保证发射前所有分支在范围内；veneer 全覆盖 |
| RISC-V B-type ±4KB 范围触发大量 veneer | 低 | veneer 仅在超范围时触发，`la+jr` 4 条/veneer |
| DCE 误删有隐式副作用的指令（flags、内存、call） | 高 | 白名单制；无 def 指令一律跳过；全量功能回归；on/off 差分 |
| 缺失 register/NZCV/memory 依赖导致调度误编译 | 高 | 保守 catch-all barrier、on/off 差分、`verify_sched_deps`（待做） |
| 发射层改造破坏 RISC-V 或 tail-call | 中 | M28 双 target + tail-call 矩阵回归；`ArgSlot` 布局不变 |
| QEMU wall time 被误用为 A53 性能数据 | 中 | QEMU 仅进入 correctness gate |
| 实机环境频率/温度噪声掩盖结果 | 中 | core pinning、paired samples、95% CI、环境元数据 |

---

## 附 A：RISC-V 栈参数非对齐访问（已修复，归档）

### 现象

Judge RISC-V 实机：`h_functional/39_fp_params` WA/RE（FPGA 输出 "Failed"），
QEMU 下通过；只有混合 32/64 位大量栈参数函数受影响。

### 根因链

1. `taki_mir/src/abi.rs` `ArgLayoutPlanner::compute`（51-89 行）对栈参数密集
   打包，`stack_offset` 只按 `stack_slot_size(ty)` 累加，无对齐填充；
2. `uika_riscv/src/abi.rs:166-178` 传入 `|ty| ty.size()`：float/int 槽 4 字节、
   指针槽 8 字节 → 跟在 32 位参数后的指针落在 `4 mod 8` 偏移；
3. 两侧布局一致、取值正确 → QEMU 全对，唯一症状是地址非对齐；
4. 实测 `/tmp/39_fp_params.s`：131 处 64 位访问落在 `4 mod 8` 地址
   （`params_mix` 26 + `main` 105），32 位访问 0 处非对齐；
5. BOOM 硬件不支持非对齐 ld/sd → 实机 RE / QEMU AC 分歧；
6. 栈帧与局部栈槽本身按 8/16 对齐，只有多栈参函数中招。

### 附带问题：psABI 不合规

RISC-V psABI 规定窄于 XLEN 的标量栈参数 **widened to XLEN bits**（RV64 每槽
8 字节、8 对齐）。当前 4 字节密集打包既非对齐、又违反 widening 规则，与 GCC
编译的 callee 互调时槽位取值错位。`anon_armv8` 早已用正确实现
（`anon_armv8/src/abi.rs:133` `|_| 8`，单测断言 [0,8,16]/24），是 RISC-V 侧
孤立回归。

### 修复（已实现）

`uika_riscv/src/abi.rs:176` 的 `stack_slot_size` 从 `ty.size()` 改为
`|_| Self::word_bytes()`（与 aarch64 一致）；同步更新
`argument_layout_preserves_scalar_stack_widths` 断言（[0,8,12,16]/24 →
[0,8,16,24]/32）；tail-call 路径与 `abi_matrix` 回归。状态：QEMU 单测与
riscv functional+h_functional 全量通过，非对齐 131→0 处；**FPGA 实机复跑
待验证（§4.9）**。

---

## 附 B：RISC-V 跑分长耗时用例分析（judge_rv64_8_2_03_00，已归档）

数据源：`judge_rv64_8_2_03_00.txt`（rv 实机 BOOM 跑分）。汇编证据用
`./target/release/compiler -S -O1 tests/perf/<case>.sy` 复现，生成物在
`/tmp/perf_analysis/`（临时目录，需重新生成）。

### 耗时排名（秒）

- many_mat_cal-1/2/3：106.7 / 106.0 / 105.0（三连，绝对大头）
- conv2d-1：58.4；knapsack_naive-1/2/3：39.8×3；matmul2：28.5
- transpose2：24.4；sl2：17.5；conv2d-2：15.9；h-4-03：15.7；matmul3：
  15.6；01_mm2：14.8
- crypto-1：11.5；huffman-01/02/03：9.3×3；01_mm3：9.6；sl1：8.7；
  h-1-03：8.6；crypto-2：8.1；matmul1：7.7
- 次长带：conv2d-3 5.3 / crypto-3 4.6 / crc×3 4.5 / fft1 4.4 / shuffle1
  4.2 / 01_mm1 4.3 / h-10-03 3.7 / 03_sort×3 3.1 / h-9-01 2.1

### 系统性 codegen 问题（所有用例热循环均受影响）

- A1 无条件跳转 `la t6,label; jr t6`（3 条）而非 `j`（1 条）。
- A2 不用立即数槽：`li 1; addw` → `addiw`；`li 0; slt` → `slt a,zero,b`。
- A3 循环条件 `slt+beqz`（2 条）→ `blt`（1 条）。
- A4 地址强度削减不完整且不对称：a. 内层元素地址每轮从 IV 重算而非指针递增；
  b. 循环不变行基址在 k 循环内重算；c. 全局基址每轮 `la` 重载；
  d. SR pass 存在但匹配面有限，漏了 slli+add 形状与全局基址。
- A5 `mulw+addw` 未融合 `maddw`（M 扩展）：many_mat_cal 矩阵乘、transpose2。
- A6 累加器 phi 拷贝往返：many_mat_cal 平方和、01_mm2，每轮 2 条 mv。
- A7 基址溢出到栈每轮重载：transpose2、fft1（寄存器压力导致 spill）。

### 用例特定

- B8 内层循环调用未内联：fft1 蝶形每元素 2-3 次 `call multiply`（递归倍增模乘）；
  huffman 每符号 `call read_bits_specialized_2`；crc 每字节 `call crc32_*`。
- B9 conv2d-1：边界检查 rr 半条件未 hoist；`cc>=0` 用 `li 0+slt+xori`；
  K[kr*5+kc] GEP 每轮重算。
- B10 knapsack_naive：零比较编译成 `li 0; subw; seqz` 三条；每帧 5 对 sd/ld。
- 已达标项：h-4-03 常量除法全部 magic-mul（div=0），剩余仅为 A1/A2 循环开销。

### 每用例主导瓶颈映射

- many_mat_cal(106s)：A4a+A1+A2+A5+A6（§2 主计划 D 已直接针对）
- conv2d-1(58s)：B9+A1/A2/A4
- knapsack(40s)：B10+A1/A2
- matmul2/01_mm2(28/15s)：A4b/A4c/A4a+A1/A2
- transpose2(24s)：A4a+A7+A1
- sl2(17s)：A4a(7次/轮)+A2
- crypto-1(11.5s)：A1(216)+A2(240)
- huffman(9.3s)：B8+A2(337)+A1(281)
- fft1(4.4s)：B8+A7
- crc(4.5s)：B8

候选行动项（P0-P3）已并入 §4.10。
