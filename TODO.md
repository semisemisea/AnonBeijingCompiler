# Cortex-A53 后端优化计划

本文档只记录尚未完成的工作。已完成里程碑只保留一行摘要，历史设计与实现细节
以 Git 提交记录和代码测试为准，不在这里重复维护。

> 进行中：§3 SIMD/NEON 优化计划（2026-08-09 依据 9 篇论文重新规划）——
> **milestone 5（内联向量零初始化）已完成（2026-08-09）**。
> **milestone 1（matmul1 掩码内核 j/k interchange 解锁）已完成（2026-08-10）**：
> matmul1 掩码内核完整向量化（-O2 语义 PASS、functional + h_functional 152/152、
> RISC-V 无新回归）。
> **milestone 2（P0）：conv2d 计算内核多臂 if 掩码向量化**（进行中）——2026-08-10
> 深入收益分析确认：conv2d-1 的 5×5 卷积内核占 83.3% 权重（N_eff=521 → 6.78M
> 乘加），是唯一高收益目标（向量化 3-4 倍 → 整体 ~60% 静态加速）。详细计划见
> §3.3。其余候选已实证低收益或受阻：checksum（3.3% 权重）需 load bound 提升但
> 暴露 LoopUnroll runtime-bound 展开 bug（85_long_code arrCopy 回归）；shape_header
> 放宽单独零收益；01_mm1 mm 内核已完全向量化（M70）；row_reduce/nonlinear 已
> 向量化；标量 min/max ISel（P1，IR 无实例，低价值）；M45 SLP（P1）。
> 待做：§5 主计划 E 剩余项（M51 指针槽/SROA、M55、M56）与 §6 后续候选。

## 已完成里程碑摘要

- **M1-M17**：MIR pass 基础设施、发射前 finalize、ABI 参数布局共享、pre-RA
  PeepholeCombine（MAC 融合）、post-RA PairCombine（LDP/STP）、依赖 DAG +
  Cortex-A53 list scheduler、可切换 pipeline（-O0/1/2）、CycleSimulator、
  `Removed` tombstone、slot-filling 双发启发、PMU harness、确定性门禁。
- **M18**：pre-RA DCE（worklist use-count 定点，白名单制，-O1 起默认开启）。
- **M19-M24**：入口参数 fixed-register live-in（`Args` 伪指令），消除 ABI
  home-slot store/load 往返；AArch64 + RISC-V × -O0/1/2 ABI 矩阵与 5 次确定性门禁。
- **M25-M29**：EmitBuffer 文本缓冲、分支优化四规则（R1-R4）、veneer 范围松弛
  （BRANCH14/19/26 与 RISC-V B/JAL）；分支发射重构（对照 Cranelift MachBuffer）
  ——RISC-V `CondBr` slot 化、`beqz/bnez` 收敛、veneer 全覆盖。
- **M30**：基准与验证基建（`scripts/perf_compare.sh` + `results/perf_compare/`）。
- **M31-M38**：if-conversion 推广 + `CondResult`/`CCmp` 抽象、循环旋转
  `rotate_loops` + 标志融合、GSP + LICM + 作用域化 load-CSE、`Mov` 32 位宽度、
  内联代价模型、if 链 → switch 决策树、TCO 扩展 + 死空块清理。huffman-01 静态
  687 → 599 → 580（决策树后 600），`read_bits` 149 条 < clang 161。
- **M39-M41b**：向量类型 + `RegClass::Vector` 基础设施、NEON MInst 全集、
  向量 ABI（v0-v7 参数/返回、v8-v15 callee-saved）、向量 IR 入口 + 完整 ISel
  lowering、向量 SchedClass（A53 NEON 参考值）。Phase 1 机器层通路完成。
- **M47**：`reduction_unroll` 标量多累加器 + 4× 部分展开（many_mat_cal 平方和
  4 条独立 `madd`）。
- **M48**：`invariant_reduction_hoisting` 外层不变归约外提（many_mat_cal-1/2/3
  R 循环消失，qemu ~82s → ~2s）。
- **主计划 E 大部分（M49/M50/M52/M53/M54）**：内存/别名分析底座 + 完整 Purity
  分析 + DSE（GSP 冗余回写删除/覆盖死 store/forwarding）+ IPSCCP 主存模拟 +
  LICM load 外提。conv2d 形参指针槽 load 全部提出热循环；23_json -O2 编译
  9min → 0.078s。M51/M55/M56 未做，见 §5。
- **M44 v2 全目标（2026-08-05，hermes 线）**：payload 白名单（Shl/Shr/Sar/
  And/Or/Xor/Div(f32)/Min/Max + 向量移位/除法 lowering + f32 splat 语法修正）、
  B3 内存累加（`is_elementwise_inplace`）、外层 IV passthrough、B2 test-at-top、
  select 掩码（VecSelect）。matmul 清零循环出 `dup+str q`。
- **B1 掩码寄存器归约（2026-08-07，49a2825）**：单臂 if + 寄存器累加器向量化
  （arm 内 `delta=binary(acc,rhs)` → 掩码化为 select，merge phi 同步向量化）。
  单测 +1，matmul1 掩码内核（P0 目标）经里程碑 1 的 j/k interchange 解锁后
  完整向量化（见 §3.2 里程碑 1）。
- **里程碑 1：matmul1 掩码内核 j/k interchange 解锁（2026-08-10，2dc0a17 +
  ab4b05b）**：find 放行 shell 中 j 依赖列指针 GEP + apply 步进指针参数支持
  （重写 body 的 ptr 读取为 3D GEP、drop ptr 参数、shell 死代码清理）+ stale
  used_by 清理（`replace_inst_with` 重建 branch 残留旧 Inst id 于 used_by，
  base_of 解引用已删 terminator → `UnknownBase`）。matmul1 掩码内核完整向量化：
  内层 j 循环 `ldr q ×3 + dup ×2 + mul/and/cmeq/eor/orr + add v + str q`，
  -O2 语义 PASS、functional + h_functional 152/152、RISC-V 无新回归。
- **M70 系列（2026-08-07，提交 4）**：LICM 支持 VectorSplat hoisting、向量
  mul+add → mla 融合、向量循环计数器 subs 融合、2x 展开 + ldp/stp pair。
  01_mm1 mm 内核 8 元素/轮（ldp q×2 + mla×2 + stp q + subs #8），qemu ~1200ms。
- **rebase 到 hermes + 相似代码整理（2026-08-09）**：13 提交 rebase 到
  `feat/loop-vectorize-hermes`（55c7221 编译时间优化跳过）；移除
  `VecArithRRR.is_float` 字段统一 `Fadd/Fsub/Fmul` 变体；修复 union_find
  test-at-top + B1 arm 归约的 CE/死循环（`b1_arm_test_at_top_unsupported`）。
  functional + h_functional -O2 152/152、0 CE。

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

IR 优化管线（`raana_ir/src/opt/pass.rs` 固定点循环）：
SSA → Specialize → Inline → TCO → ColumnMajor → GSP；固定点内：IPSCCP,
SimplifyCFG, LoopUnroll, RotateLoops, ZeroStoreLoop, ChainToSwitch,
LoopInterchange, LoopVectorize, LICM, GVN, DSE, PointerStrengthReduction,
GuardElimination, ModFold, StrengthReduction, MatmulInterchange,
InvariantReductionHoisting, ReductionUnroll, BlockedReduction, IfConversion,
TCO, TailRecursiveInline, BooleanSimplification, GVNPRE, DeadPhiElim, DCE。

关键代码：

- `raana_ir/src/opt/pass.rs`：`Pass` / `PassesManager`（`aarch64()` vs `default()`）。
- `raana_ir/src/opt/passes/`：IR pass（`loop_vectorize`/`loop_interchange`/
  `if_conversion`/`rotate_loops`/`licm`/`sr`/`dse`/`gvn`/`ipsccp` 等）。
- `raana_ir/src/opt/analysis_passes/`：`memory`/`effects`/`dependence`/
  `loop_analysis`/`induction_variable`/`dom_tree`/`cfg`。
- `anon_armv8/src/instructions.rs`：`MInst` 枚举；`lower.rs`：ISel。
- `anon_armv8/src/passes/`：dce、peephole_combine、pair_combine、chain_fusion、
  const_cse、list_scheduler（cortex-a53 guide 模型）。

### 1.2 向量化现状（LoopVectorize，M44 系列 + M70）

- 目标：最内层、rotated（test-at-bottom）/ test-at-top 计次循环，纯逐元素、连续
  4B 访问（NEON VF=4，i32/f32）。
- 已支持：向量常数除/模、模归约（scalar acc + addv）、B1 寄存器归约（vector acc
  + VectorReduce/addv）、单臂 if 掩码 store、运行时 bound（动态 counter + 标量
  tail）、向量 mul+add → VecMla（M70）、VectorSplat LICM 外提（M70）、2x 展开 +
  ldp/stp pair（M70）。
- 约束（宁漏勿错）：只最内层；test-at-top 只认恰 `[lt,br]`；rotated 只认
  `[jump]`；对齐未知时非对齐 ld/st；无 gather/scatter；AArch64-only。

### 1.3 当前向量化命中（2026-08-10 复扫，perf 语料 -O2 静态汇编）

| 用例 | 向量指令数 | 说明 |
|---|---|---|
| 01_mm1 | 14 | mm 内核 8 元素/轮（M70），验收达成 |
| matmul1 | 掩码内核向量化 | 掩码内核 j/k interchange 解锁 + 完整向量化（内层 j：ldr q ×3 + dup ×2 + mul/and/cmeq/eor/orr + add v + str q），sum + 清零循环也向量化 |
| h-10-01 | 9 | f32 循环 |
| conv2d-1 | 21 | 清零/零初始化 + row_reduce + nonlinear + sum 循环向量化；**5×5 计算内核（83.3% 权重）仍标量**（里程碑 2 目标） |
| crypto-1 | 2 | 既有向量化循环 |

### 1.4 当前拒绝分布（2026-08-10 复扫，M44_TRACE=1，dedup top）

- conv2d-1 计算内核（5 点卷积，while_entry_7）：`shape_body_not_2_blocks`（多臂
  if，需多臂 if 掩码合并）；其余 `shape_header_multi_inst` 多为已向量化循环的
  固定点噪声（init_kernel/row_reduce/nonlinear 等已向量化）。
- checksum 循环（`sum += Out[i]`）：bound=`N_eff²` 在 header 计算，需 load bound
  提升，但触发 LoopUnroll 对 runtime bound 循环的展开 bug（见 §3.3）。
- 01_mm1：mm 内核已完全向量化（M70），无剩余。
- 通用：`not_innermost`（外层循环，正常）、`tail_loop_skip`（本 pass 产物，
  防固定点再向量化）。

### 1.5 方法论约束（AGENTS.md）

- 真正的质量门禁是 Docker 测试 harness + 与 clang/gcc -O2 的静态代码量对比；
- 静态指令计数（`scripts/perf_compare.sh`）为确定性回归代理；gem5 A53 SE 为动态
  验证；QEMU 仅语义差分；
- 未获得 XCZU15EG 实机数据前，只声称"静态模型改进"，不声称实机收益；
- 任何 AArch64 改动必须同时验证 RISC-V 不回归（`make test-riscv`）；
- 不允许针对测试用例的优化（`docs/Illegal_optimization.md`）。

---

## 2. 里程碑进度（已收尾，压缩）

- **M44 v2 目标 1-4 执行记录**：见"已完成里程碑摘要"M44 v2 行；白名单 + 向量
  移位/除法 lowering + B3 同地址放宽 + passthrough + test-at-top + select 掩码
  全部落地。corpus 命中从 0 → 3/60（matmul 清零），后经 B1/M70 提升到当前 §1.3。
- **M44 v2 拒绝原因分类 / A2-A4 调研 / B1 细化计划 / 最短路 sum 计划**：已完成，
  细节归档到 Git 提交与代码注释，不重复维护。核心结论仍有效的部分已并入 §3.2
  里程碑 1 完成摘要与 §3.3 里程碑 2 现状根因。
- **M52 执行记录（DSE 三个变换 + 三个遗留 bug 修复）**：全部完成，见摘要。
- **§6 工作区未提交改动 / §7 vs clang 差距 / §8 A2 内联 / §9 全量审查 /
  §10 向量化接力（10.x 全部 session 记录）**：已归档或已完成，从本文档删除。

---

## 3. SIMD/NEON 优化计划（2026-08-09 规划，依据 9 篇 SIMD 论文）

> 论文输入：Diospyros(ASPLOS'21)、Isaria(ASPLOS'24)、Minotaur(OOPSLA'24)、
> SuperVectorization(PLDI'22)、Parsimony(CGO'23)、Autovesk(TACO'23)、
> Coyote(ASPLOS'23)、Qiwu(CGO'25)、CHOPPER(HPCA'23)。
> 详细论文 → 优化空间映射见 `docs/optimization_analysis.md`；本计划在其基础上
> 按 2026-08-09 的 rebase 后代码现状校正优先级与根因。

### 3.1 方向汇总（9 篇论文 → 8 项）

| 方向 | 论文 | 优先级 | 里程碑 |
|---|---|---|---|
| A. SLP 基本块向量化（M45） | SuperVectorization | P1 | 4 |
| B. matmul1 掩码内核（j/k interchange 解锁） | SuperVectorization | P0 | 1（已完成） |
| C. conv2d 计算内核多臂 if 掩码向量化 | SuperVectorization + Parsimony | P0 | 2 |
| D. 向量化盈利性成本模型 | Coyote | P2 | §6 候选 |
| E. 标量 min/max ISel + select→min/max | Minotaur | P1 | 3 |
| F. NEON 寄存器压力预算 | CHOPPER | P2 | §6 候选 |
| G. 非循环内联向量零初始化 | Minotaur | P1 | 5 |
| H. 回边 blockparam mov 消除 / 调度验证器 | Diospyros/Isaria/Minotaur | P2/P3 | §6 候选 |

### 3.2 里程碑 1（P0）：matmul1 掩码内核 —— j/k interchange 解锁（已完成）

**论文依据**：SuperVectorization §3.4（masked load/store + vector select）：
控制依赖转数据依赖（掩码选择），使向量化跨控制流。

**完成摘要（2026-08-10，2dc0a17 + ab4b05b）**：matmul1 掩码内核
（`if(a[i][k]*b[k][j]%2==0) temp += b[i][k]*a[k][j]`）经 j/k interchange 解锁并
完整向量化。根因（2026-08-09 实证）：该循环是 **k 循环**，`b[k][j]`（GEP 步进
指针 %69）与 `a[k][j]` 在 k 上 strided（4000B）——B1 无法向量化；**真正解锁是
j/k interchange**（i-j-k → i-k-j，内层 j 连续）。实现：find 放行 shell 中 j 依赖
列指针 GEP + apply 步进指针参数支持（识别 ptr_idx、重写 body 的 `getelemptr(ptr,X)`
为 3D GEP、drop ptr 参数、shell 死代码清理 + i 引用重写、E_k 死指令清理）+
stale used_by 清理。结果：内层 j 循环 `ldr q ×3 + dup ×2 + mul/and/cmeq/eor/orr +
add v + str q`，-O2 语义 PASS、152/152、RISC-V 无新回归。历史设计与细节以 Git
提交记录为准。

### 3.3 里程碑 2（P0）：conv2d 计算内核多臂 if 掩码向量化

**论文依据**：SuperVectorization（循环 co-iteration / 跨控制流打包：外层 IV 以
passthrough 线程化进内层）+ Parsimony（uniform/varying 分类：uniform 分支/值用
标量，varying 才向量）。对应 M44 拒绝分类 A2/A3。

**收益分析（2026-08-10 深入实证）**：
- conv2d-1 输入 state=4561、repeat_factor=1 → N_eff=521。各循环迭代权重：
  | 循环 | 迭代次数 | 权重 | 现状 |
  |---|---|---|---|
  | **conv2d 计算内核（5×5 卷积）** | 6.78M 乘加 | **83.3%** | 标量，未向量化 |
  | row_reduce | 542K | 6.7% | 已向量化 |
  | checksum | 271K | 3.3% | 标量，未向量化（需修 LoopUnroll） |
  | nonlinear | 271K | 3.3% | 已向量化 |
  | init_matrix | 271K | 3.3% | 含调用，难向量化 |
- **conv2d 计算内核是唯一高收益目标**：向量化后约 **3-4 倍内核加速 → 整体约
  60% 静态加速**。checksum 收益仅 ~2.5%（且被 LoopUnroll bug 阻塞），其余循环
  已向量化或低价值。
- 静态指令对比估算（c 循环，4 个 c 值）：标量 4 轮 ≈ 220 条（25 乘加 + 边界）
  vs 向量 1 轮 ≈ 40 条（25 掩码乘加 + 边界掩码 + Out store）→ 约 5 倍乘加密集
  区加速，保守估 3-4 倍。

**目标形态（conv2d 计算内核，c 循环向量化）**：
- c 循环 `while_entry_7`（`c < N_eff`，test-at-top，IV 步进 1）body 有 **52 个
  基本块**：25 次乘加（5×5 卷积，kr/kc 全展开）+ 25 个边界检查
  （`cc=c+kc-2` 的 `ge/lt/and/neq` 链）。
- 目标（VF=4）：
  ```
  sum_v = 0
  for (kr, kc) in 5×5:                    # 25 项展开
    cc_vec = c_vec + kc - 2
    mask = (cc_vec >= 0) & (cc_vec < N_eff) & rr_ok   # 向量掩码
    sum_v += (ldr q In[rr][cc_vec]) * splat(K[kr][kc]) & mask
  Out[r][c_vec] = sum_v                   # 连续 store
  ```
- 与现有 B1 的**互斥 `select`**（`tn = delta&m | fo = acc&~m`）不同，25 个
  (kr,kc) 是**串行掩码累加**（`sum_v += delta & mask`），逻辑更简单（无 fo
  分支），但需识别 25 个独立贡献并累加。
- In/Out 为 16B 对齐全局数组 ✓；c 循环访问连续 ✓；现有 B1 掩码基础设施
  （`tn/fo/sel` 组合 + `vector_operand` splat）可作扩展起点。

**现状根因（2026-08-10 代码实证）**：
- conv2d 计算内核被 `shape_body_not_2_blocks` 拦截——**多臂 if**（land_merge_10
  /then_23/end_12 的边界检查链，5 kc × 5 kr 全展开，52 个基本块）。
- conv2d-1 的 `shape_header_multi_inst`（26）**大多是已向量化循环的固定点噪声**
  （init_kernel/row_reduce/nonlinear 等已向量化）。
- **checksum 循环**（`sum += Out[i]`，bound=`N_eff²` 在 header 计算）：shape_header
  放宽放行后 apply 正确向量化，但需 **load bound 提升**（bound 定义在 header，
  runtime trip 引用 → lowering use-before-def）。提升后 checksum 向量化（conv2d
  +3 向量指令），但**触发 LoopUnroll runtime-bound 展开 bug**（85_long_code
  arrCopy 回归：bound=`load len` 被放行后 LoopUnroll 全展开成错误常量 store）。
  → checksum 方案暂缓，需先修 LoopUnroll（见 §4）。
- **shape_header 放宽（只放行纯计算 bound，不含 load）单独零收益**：conv2d 拒绝
  分布不变，perf 语料向量指令数全不变。

**实现步骤**：

*阶段 1 —— 识别与 gate（loop_vectorize）*
1. 识别 c 循环的多臂 if 累加结构：52 个基本块 body 的 25 个
   `br mask_kc, then_kc, end_kc` 模式（每 (kr,kc) 一个边界检查 + 乘加 + 累加
   合并）。新增 `M44` 通过路径（多臂掩码 plan），形状门控：test-at-top、
   IV 步进 1、25 个串行掩码累加。
2. 新增单测：合成 5 点卷积形态（连续 In/Out + 边界检查 + 累加），验证识别。

*阶段 2 —— 掩码乘加核心（loop_vectorize apply + 可能 lower）*
3. 扩展 ArmPlan/B1：单臂 → 多臂。对每个 (kr,kc)：
   - 向量边界掩码生成：`mask_kc = (c_vec + kc - pad) >= 0 & < N_eff`，
     与 `rr_ok`（标量 splat）合取。
   - `sum_v += (ldr q In[rr][cc_vec]) * splat(K[kr][kc]) & mask_kc`。
   - K[kr][kc] 是循环外常量（全局 K 数组），splat。
4. 掩码乘加 lowering：复用现有 `VecMla` + `and`（或新增掩码 mla 变体）。
   边界比较 lowering：向量 `cmeq/eor` 组合或新增 VecCmp。

*阶段 3 —— sum_v 累加 + Out store*
5. sum_v 向量累加器（25 次展开，header 参数 re-type）。
6. `Out[r][c_vec] = sum_v` 连续向量 store（B3 路径扩展 / VecStore）。
7. 标量尾循环处理（r = trip%4 余数，复用现有 tail）。

*阶段 4 —— 验证与回归*
8. `cargo test -p raana_ir`（新增多臂掩码单测）。
9. `make test functional h_functional ARGS="-O 2"`（152 用例）。
10. `make test-riscv ARGS="-O 2"`。
11. perf 静态指令数对比：conv2d-1 计算内核指令数应大幅下降（`scripts/perf_compare.sh`）。

**涉及文件**：
- `raana_ir/src/opt/passes/loop_vectorize.rs`（多臂掩码识别 + apply，核心）
- `raana_ir/src/opt/analysis_passes/dependence.rs`（多臂 if 的依赖/别名判定）
- `anon_armv8/src/lower.rs` + `instructions.rs`（向量掩码乘加 / 向量比较 lowering，
  若现有 `VecMla`+`and` 组合不够）
- `anon_armv8/src/sched/dag.rs`（新向量指令的调度内存边，若新增 MInst）

**验收**：
- conv2d-1 计算内核出 `ldr q ×N + dup v + mla v ×25 + and v ×25 + add v + str q`；
  `M44_TRACE=1` 复扫该 header 不再报 `shape_body_not_2_blocks`。
- conv2d-1 -O2 语义 PASS；`cargo test -p raana_ir`；`make test ARGS="-O 2"`
  functional + h_functional 无回归；`make test-riscv ARGS="-O 2"` 无回归；
  `scripts/perf_compare.sh` conv2d-1 静态计数显著下降（目标 ≥2 倍）。
- **禁止针对测试用例的优化**：多臂掩码识别须基于通用 IR 结构（边界检查 +
  串行累加），不得匹配 conv2d 函数名/常量。

**风险**：
- 向量掩码边界语义错误（`cc` 在向量内部分满足）——高：保守掩码 + on/off 差分 +
  语义 PASS。
- 25 次累加链 use-def / 指令环（GVN 递归，参考里程碑 1 的 3e5f5ee 教训）——高：
  严格依赖判定 + 后端 SSA 验证。
- In/Out 对齐（16B）——中：全局数组已验证对齐；若参数指针需 IPA 对齐证明。
- IR 复杂度（52 基本块 → 向量化）——中：阶段 1 先小 VF/子集验证，再全量。
- 掩码乘加 lowering 缺 VecCmp——中：复用 `cmeq/eor/and` 组合或新增。

**已完成的子项（压缩）**：
- **A3 test-at-top 多参数 passthrough（2026-08-09，2caf984）**：正确性改进
  （消除对 loop-invariant 常量 back-arg 的误拒），无 perf 收益。
- **01_mm1 mm 内核（M70）**：已完全向量化（`ldp q×2 + mla×2 + stp q`，8 元素/轮），
  验收达成。

### 3.4 里程碑 3（P1）：标量 min/max ISel + select→min/max 模式匹配

**论文依据**：Minotaur §1 例 2（`fsub;fcmp>0 → fcmp` 同类被 LLVM 漏掉的简单
模式；2024 年 LLVM 仍不会，已合入主线）。`select(a>b,a,b)` = `smin(a,b)` 省
1 条 cmp。对应 TODO §3.4-C。

**现状根因**：`anon_armv8/src/lower.rs:346` 对标量 `BinaryOp::Min/Max` 直接
`lowering_panic("scalar {:?} is unsupported; min/max is vector-only")`；
`smin/smax/fmin/fmax` MInst 仅向量版；`SelectCmp` MInst 已存在（csel/cset 用）。

**实现步骤**：
1. `instructions.rs`：新增标量 `Smin/Smax/Fmin/Fmax` MInst（或 MinMax 参数化
   复用向量 emitter），SchedClass 归入 `AluMisc`/`FpAddSub`。
2. `lower.rs`：对标量 `BinaryOp::Min/Max` 直接 ISel；对 `select(a>b, a, b)`
   形态做模式匹配 → smin/smax（cmp+csel 两指令合并）。
3. IR 层（可选辅助）：`boolean_simplify`/新 peephole 识别 `if(a>b) c=a` 直接
   Min 形态。
4. RISC-V：无 smin 指令，保留 select 展开（不影响）。

**涉及文件**：`anon_armv8/src/instructions.rs`、`lower.rs`、`regs.rs`。

**验收**：标量 min/max 单测（ISel + select→min/max 模式）；huffman/h-9/h-4/crc
等 csel 用例静态指令数下降；无 `min/max is vector-only` panic；双 target 回归。

**风险**：低。新增指令不发散既有 ABI；`fmin` 的 NaN 传播语义须与 select 一致
（用 `fcmp`+csel 语义对照验证）。

### 3.5 里程碑 4（P1）：M45 SLP 基本块向量化（`slp.rs` 新建）

**论文依据**：SuperVectorization（SLP 选择性打包 + 循环展开暴露相邻同构，局部
向量化天然支持）+ Parsimony（uniform/strided 形状分类用于访存选择）。

**现状**：`slp.rs` 尚不存在；`loop_unroll`（常数 trip 全展开）已合入主线，是
SLP 现成输入；conv2d `init_matrix`/`row_reduce`、01_mm/matmul 内层展开后存在
相邻同构标量 op。SysY 无三目（全量 tern=0），独立直通代码场景少，主要收益来自
"循环向量化 + 展开后的补充打包"。

**实现步骤**：
1. 新建 `raana_ir/src/opt/passes/slp.rs`：
   - 输入：基本块内相邻、类型一致、相互独立（无 use-def/内存冲突）的标量 op；
   - 打包：配对的 add/mul/load/store（菱形结构），4 路打包为 `<4 x T>`；
   - 依赖判定：复用 `DependenceAnalysis`/`EffectAnalysis`（宁漏勿错）。
2. 注册到 `pass.rs`：AArch64-only（`TargetPolicy.enable_chain_to_switch` 门控）；
   顺序按数据定：`LoopVectorize → LoopUnroll → SLP`。
3. lowering：SLP 产生的 `VectorSplat`/向量 load/store 复用 M44 通路；若需
   `ld1/st1` 非对齐路径（PE=0 容忍）。
4. `perf_compare.sh` 增加 SIMD 列。

**涉及文件**：`slp.rs`（新建）、`pass.rs`、`passes/mod.rs`、
`scripts/perf_compare.sh`。

**验收**：conv2d-1 内层出现 `ld1/fmla/st1`；`make test ARGS="-O 2"` + 
`make test-riscv ARGS="-O 2"` 无回归；静态计数与 gem5 sim_insts 记录进
`results/perf_compare/`。

**风险**：中。SLP 打包错误导致语义偏差 → 严格独立判定 + on/off 差分；展开后
代码量爆炸 → 复用现有 unroll 阈值；向量 load/store 对齐 → 非对齐路径（已存在）。

### 3.6 里程碑 5（P1）：非循环 NEON 内联向量零初始化 —— 已完成

**论文依据**：Minotaur 的"减少数据搬移/调用"主题；非循环 NEON 实证
（TODO §3.4-A）。独立小项，不依赖向量化 pass。

**现状（实施前）**：编译期已知大小的局部零初始化（`memzero` 常量长度）在
`lower_const_mem_zero` 中只内联 ≤16B（4 条标量 store）；16B-128B 走
`bl memset` 调用。

**实现（2026-08-09，2 commit）**：
1. `anon_armv8/src/lower.rs`：`lower_const_mem_zero` 增加向量批处理路径——
   16-128B（16B 倍数）时 `movi v0.4s,#0` + 每 16B 一次 `VecSt1`，替代
   `bl memset`。
2. **`anon_armv8/src/sched/dag.rs`：VecSt1/VecLd1 未进入 `memory_access`，
   调度器把它们当无内存依赖 → 与可别名标量 store 乱序，覆盖
   栈初始化（79_var_name 输出全 0）**。修复：`memory_access` 把
   VecLd1/VecSt1 建模为 `MemKind::Load/Store, Vec128, base 寄存器 provenance`
   （offset 0），使调度器对可别名标量 store 建立内存边。这是既有隐患
   （向量化器已发 VecSt1/VecLd1），被本里程碑的栈零初始化路径触发。
   新增 2 个单测：`vector_store_orders_against_aliasable_scalar_store`、
   `vector_load_reads_a_memory_access`。

**涉及文件**：`anon_armv8/src/lower.rs`、`anon_armv8/src/sched/dag.rs`。

**验收（全部通过）**：
- functional + h_functional -O2 **152/152**、0 CE（此前 79_var_name 因调度
  乱序 wrong answer）；
- functional 04/05/54/79 默认（-O0）PASS，RISC-V 04/79 -O2 PASS；
- 受益用例：32B（04/05）、40B、60B、64B（03_sort×3/77_substr）、80B、96B
  的局部零初始化出 `movi v0.4s,#0 + st1 {v0.16b}`，免 `bl`；
- perf 01_mm1/matmul1/h-10-01/conv2d-1 -O2 差分 PASS（无回归）。
- cargo test --workspace 全绿（uika 既有失败除外）。

**风险**：低。固定大小阈值防展开爆炸；浮点零初始化均为位级 0，`movi` 安全。

### 3.7 执行顺序与门禁

```
里程碑 1（matmul1 掩码内核，P0，rotated 多参数 B1）——已完成 2026-08-10
  → 里程碑 2（conv2d 计算内核多臂 if 掩码向量化，P0，大工程）——进行中
  → 里程碑 3（标量 min/max ISel，P1，最小，IR 无实例低价值，暂缓）
  → 里程碑 5（内联向量零初始化，P1，最小）——已完成 2026-08-09
  → 里程碑 4（M45 SLP，P1，最大）
```

每个里程碑独立 commit，前缀 `[Opt(IR)]` / `[Feat(Armv8)]` / `[Docs]`，相关处
引用里程碑号。

门禁（AGENTS.md）：
- `cargo test -p raana_ir`
- `make test ARGS="-O 2"`（functional + h_functional）
- `make test-riscv ARGS="-O 2"`
- `scripts/perf_compare.sh` 无静态计数回归
- AArch64 改动必须双 target 验证

---

## 4. 已知边界与遗留（2026-08-10 更新）

- **LoopUnroll 对 runtime-bound 循环的展开 bug（2026-08-10 发现，未修）**：
  conv2d checksum 循环（bound=`N_eff²` 在 header）需 load bound 提升才能向量化，
  但提升后 LoopUnroll 会把 runtime-bound 的 arrCopy 循环（85_long_code）全展开成
  错误常量 store（基线是标量循环）。bound load 源在循环内不被写（依赖分析确认），
  展开仍出错——LoopUnroll 的 `constant_trip_count` 对 runtime trip 处理有误。
  **修复它是解锁 checksum（+3 向量指令，~2.5% 整体收益）的前提**；优先级低于
  conv2d 计算内核（83.3%）。
- **test-at-top + B1 单臂 if 归约 = 保守拒绝**（`b1_arm_test_at_top_unsupported`，
  commit 238812e）：union_find 的 `if(parent[i]==i) clusters += 1` 形态（arm
  jump 带 binary 更新但 latch back arg 是 select phi）在 apply 侧未接线，GVN
  会死循环。matmul1 掩码内核是 rotated（不受影响）。
- **55c7221 编译时间优化未适配**（rebase 跳过）：hermes 的 run_on 每函数重建
  nonneg 集合，`dependence.rs` 仍 O(F²)。23_json 类大文件 -O2 编译时间敏感，后续
  按 hermes 新接口移植（`opt/pass.rs` + `analysis_passes/dependence.rs`）。
- **f32 向量化残余**：`f32_scalars_do_not_alias_live_vector_results` 测试适配
  GPR 常量 splat 路径后保留；h-10 系列已收敛（h-10-01 9 条向量指令）。

---

## 5. 主计划 E：内存分析 / 别名分析（剩余项）

已完成 M49/M50/M52/M53/M54（见摘要），剩余：

- **M51：指针槽消除 + 栈对象提升（SROA 子集，未做）**：write-once/read-many
  的 Alloc 槽（至多一次 store、无逃逸）→ 用存储值替换全部 load 并删槽；局部
  Alloc 全部访问经常量 GEP 偏移且未逃逸 → 逐元素提升为 SSA 值。M54 已捕获
  conv2d 形参指针槽 load 外提的主要收益；SROA 对"常量化元素访问的局部数组"
  仍有独立收益。
- **M55：后端调度别名细化（P2，评估先行，未做）**：先用 sched DAG stats 量化
  known/unknown root 与 disjoint/may-alias 占比；收益有限则关闭，may-alias 占
  主导才把 M49 结果随 MInst 下沉。
- **M56（可选）：数组全局部分提升 / 循环不变基址 hoist（未做）**：GSP 泛化，
  与 pointer_strength_reduction 的 A4c 呼应。

---

## 6. 后续候选（P2/P3，不在本次范围）

| 项 | 论文 | 状态 |
|---|---|---|
| C. 向量化盈利性成本模型（dup/shuffle/addv vs 标量 4× unrolled） | Coyote | 需先积累 M44/M45 数据 |
| F. NEON 寄存器压力预算（32 个 v 寄存器） | CHOPPER | 随向量化+展开规模上升后做 |
| D. non_unit_step 步长归一化 | Autovesk | A5，45 例 |
| G. 回边 blockparam mov 消除（`_and/_xor/_or` 回边 3 mov） | Diospyros/Isaria | M35 遗留 |
| H. 调度验证器闭环（verify_sched_deps） | Minotaur | §5.2 P2 |
| 外层循环向量化（not_innermost） | SuperVectorization | 跨块，大改 |
| VecCsel/bsl、向量比较 lowering | SuperVectorization | 随里程碑 2 伴生（掩码乘加，若 `VecMla`+`and`/`cmeq/eor` 组合不够才新增） |
| 批量 int↔float 转换 | — | 排除（perf 无实证） |
| ld2/ld3/ld4 交错存取（AoS→SoA） | — | 排除（perf 无交错布局） |

### 6.1 其他既有候选（§5 后续候选保留项）

- P1 常量/分支参数物化源头治理（DCE 兜底 → lowering 源头消除）。
- P2 调度启发式增强（load-use latency hiding、pair-aware、post-RA
  pressure tie-break）——需实机数据证明收益。
- P2 冷块沉底与布局（Cranelift `BlockLoweringOrder::is_cold()` 预留接口 +
  `CmpImm(0)+CondBr{Ne}`→`cbz` 融合）。
- P2 内存 DAG 复杂度（长块最坏 O(M²)）。
- P3 XCZU15EG 实机校准（依赖硬件访问）。
- P2 RISC-V 发射层纯改进（A1 局部跳转 / A2 立即数 / A3 `slt+beqz→blt`）
  与 IR 层地址 SR 补全（见附 B 归档）。
- M35 遗留回边 blockparam 拷贝消除（见 §6 G 项）。
- 5.11 泛化：整体循环巢外提（容忍幂等写，M48 后续）。

---

## 7. 总体执行原则

1. 每个 milestone 独立提交；`TODO.md` 在 milestone 完成后删除对应已完成细节，
   只保留后续工作。
2. 正确性验证、静态模型测试和实机性能测量分层维护，不互相替代。
3. 所有 AArch64 改动必须同时验证 RISC-V 不受影响。
4. 新 pass 默认走"白名单 + 保守保留"策略，宁漏勿错。
5. 未获得实机数据前，只能声称"静态模型改进"。
6. ABI/codegen 架构改动（M19-M24、M25-M29、M30-M38、M39-M46）不由优化 flag
   控制，任何优化级别都必须保持正确；优化规则本身由 `-O` 控制。
7. 发射层改造以行为等价为第一优先级，优化规则在等价基线上逐步开启。
8. 向量化只对 M42 依赖分析可证安全、或经 M43 运行时 versioning 保证的循环
   进行；无法证明时保守拒绝（宁漏勿错）。
9. 向量化 pass 只在 AArch64 注册；RISC-V 保持标量，双 target 回归。
10. 对齐未知时用非对齐 `ldr/str q` 或 `ld1/st1`，或 versioning，绝不在编译期
    假设 16 字节对齐。

---

## 8. 风险与缓解

| 风险 | 严重度 | 缓解措施 |
|------|--------|---------|
| 向量化依赖分析误判导致语义错误 | 高 | 保守依赖分析 + 只向量化可证安全循环 + on/off 差分 + 全量功能回归 |
| 向量寄存器压力导致大量 spill | 中 | Vector 独立分配域 + v8-v15 优先 + 先小 VF 验证 |
| 对齐假设错误导致 SIGBUS | 中 | 已知对齐才用对齐 `ld1/st1`；未知走非对齐或 versioning |
| RISC-V 无 NEON 引入回归 | 中 | 向量化 pass 按 target 注册；双 target 回归 |
| B1/test-at-top 归约 apply 未接线（GVN 死循环） | 高 | 保守 gate（`b1_arm_test_at_top_unsupported`）；里程碑 1 只做 rotated |
| 多臂 if 掩码累加语义错误（conv2d 边界） | 高 | 向量掩码 `cc` 边界检查 + on/off 差分 + 语义 PASS；25 次累加链依赖判定 + 后端 SSA 验证（参考 3e5f5ee 指令环教训） |
| header bound 指令提升破坏 use-def | 中 | layout/use-def 一致性规则；rotate 不兜底子类确认后再做（checksum 场景被 LoopUnroll bug 阻塞，见 §4） |
| regalloc ion 对 Vector class 支持缺口 | 中 | 先拆 vcode move/spill panic + ion 单测，再启用向量化 |
| 显式向量类型污染标量流水线 | 中 | 类型下沉到 MIR 后由 `reg_class_for_type` 分流，标量路径不动 |
| if-conversion 投机上提改变执行语义 | 中 | 仅纯整数算术 + head 支配 merge 才转换；on/off 差分 |
| GSP 提升全局破坏跨函数可见性 | 高 | 白名单 + 出口统一回写；保守宁漏勿错 |
| `ccmp` 链破坏 NZCV 使用顺序 | 中 | 条件仅限单用纯比较；emit 单测；on/off 差分 |
| 内联膨胀增加编译时间与代码体积 | 中 | 代价估计 + 阈值 + 深度限界；corpus 编译时间监控 |
| DCE 误删有隐式副作用的指令 | 高 | 白名单制；无 def 指令一律跳过；全量功能回归 |
| 缺失 register/NZCV/memory 依赖导致调度误编译 | 高 | 保守 catch-all barrier、on/off 差分、`verify_sched_deps`（待做） |
| QEMU wall time 被误用为 A53 性能数据 | 中 | QEMU 仅进入 correctness gate |
| 实机环境频率/温度噪声掩盖结果 | 中 | core pinning、paired samples、95% CI、环境元数据 |

---

## 附 A：RISC-V 栈参数非对齐访问（已修复，归档）

修复已实现（`uika_riscv/src/abi.rs` 的 `stack_slot_size` 改为
`|_| Self::word_bytes()`，psABI widening 合规），非对齐 131 → 0 处；QEMU
单测与 riscv functional+h_functional 全量通过。**FPGA 实机复跑仍待验证**：
`h_functional/39_fp_params` 在 BOOM 实机复跑确认 WA/RE 消除（见 §6.1
RISC-V 候选）。根因链与修复细节归档在 Git 提交历史（附 A 原文）。

## 附 B：RISC-V 跑分长耗时用例分析（已归档）

judge_rv64_8_2_03_00 实机跑分长耗时分析（many_mat_cal-1/2/3 106s、conv2d-1
58s 等）与系统性 codegen 问题（A1-A7、B8-B10）已归档到 Git 提交历史
（§7/§8/§9 原文）。候选行动项（P0-P3）并入 §6.1；其中 many_mat_cal 已由
M47/M48 消除（qemu 82s → 2s），conv2d 依赖 LICM/向量化（§3）。
