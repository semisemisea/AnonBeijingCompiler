# Cortex-A53 后端优化计划

本文档只记录尚未完成的工作。已完成里程碑只保留一行摘要，历史设计与实现细节
以 Git 提交记录和代码测试为准，不在这里重复维护。

> 已完成：§2 主计划 D（M47 `reduction_unroll` / M48 `invariant_reduction_hoisting`）；
> §4 主计划 E 的 M49/M50/M53/M54（内存分析底座 + Purity + IPSCCP 主存模拟
> + LICM load 外提）。
> 进行中：§4 主计划 E 剩余项（M51 指针槽/SROA、M52 DSE）。
> 下一优先级见 §3 SIMD Phase 2（M42-M46，搁置中）与 §5 后续候选。

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
  （消除路径见 §5.7）。
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
- **主计划 E（M49/M50/M53/M54）**：内存/别名分析底座（GetBaseObject +
  指针槽解析）、完整 Purity 分析（points-to + mod-ref 摘要）、IPSCCP 主存
  模拟（常量格 + MemZero 零区间 + call 失效）、LICM load 外提（别名/mod-ref
  守卫）。conv2d 形参指针槽 load 全部提出热循环；`g[0]=5; ret g[0]` 折叠为
  `ret 5`。M51/M52/M55/M56 未做，见 §4。
- **主计划 A（M30-M38）**：huffman 类基准性能重构全部完成，设计依据 Cranelift
  机制移植 + 以 clang 为参照的 if-conversion/ccmp/subs 融合。
- **RISC-V 栈参数修复**：非对齐访问 + psABI widened-to-XLEN 槽宽（见附 A，FPGA
  实机复跑待验证，见 §5.9）。
- **RISC-V 跑分长耗时分析**：已完成并归档（见附 B），候选行动项并入 §5.10。

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
剩余的通用优化方向见 §5 后续候选工作；热循环标量优化（§2 主计划 D）已完成，
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
~82s → ~2s。合规审计与实现护栏记录于 M47/M48 提交信息；§5.11 保留 M48 泛化
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
- 状态：已实现（2026-08-05，`analysis_passes/dependence.rs`，6 单测全绿，
  raana_ir 306 测试通过）。当前依赖分析的访问函数只按"当前循环 BIV"分类，
  interchange 判定需要双 IV（j/k）系数时另行提取。

#### loop interchange（M44 前置，独立 pass，2026-08-05 设计定稿）

目标：matmul1 乘法段 i-j-k 三层（rotate_loops 后 countdown 形态），交换 j/k →
i-k-j，使新内层 j 的 c[i][j]/b[k][j]/a[k][j] 全部连续（4B stride）。matmul1
当前内层 k：a[i][k] 连续但 a[k][j] 跨行 4000B，无法直接向量化。绝大多数 perf
用例（01_mm/fft/sl1/conv2d/transpose 等）内层已连续，不需要 interchange。

**核心洞察：块结构完全复用。** rotate 后块拓扑：
`H_i → B_i → H_j → B_j → H_k → B_k → H_k(回边)/E_k → H_j(回边)/E_j → H_i(回边)/E_i`
- H_k=inner_header（参数 [k,temp]）、B_k=inner_body（if/归约 + k 更新 + k 测试
  br）、E_k=inner_exit（c[i][j]=temp + j 更新 + j 测试 br）、H_j/B_j/E_j=mid、
  H_i/B_i/E_i=outer（B_j 只含 jump H_k）。

交换 j/k 后仅三处改动（其余块原样）：
1. H_k 参数 [k,temp] → [k]（删 temp），回边 args 同步删一项。
2. B_k 尾部：删 k 更新+k 测试；追加 E_k 的 j 更新+j 测试（layout 移动，保持
   use-def/layout 一致）；terminator 换 j 测试 br（target 保持 H_j/E_k）。
3. E_k 尾部：删 c[i][j]=temp 与 j 更新+j 测试；追加 B_k 的 k 更新+k 测试；
   terminator 换 k 测试 br（**target 从 E_k 改为 E_j**——k 出口直接到 i 更新）。

**归约迁移（难点）**：原 temp 寄存器归约（k 内累加、k 后 c[i][j]=temp）→
c[i][j] 内存跨 k 累加：B_k 里 `temp' = select(cond, temp+delta, temp)` 改为
`store(select(cond, load(c[i][j])+delta, load(c[i][j])), c[i][j])`（load 复用）；
E_k 的 c[i][j]=temp 赋值删除。合法性要求：c[i][j] 在 k 首次迭代时为零——判定：
c 的 base 是 Global（未初始化全局零初始）且 k 循环内无其他 c 写；否则拒绝交换
（宁漏勿错）。float 归约不迁移（IEEE 累加顺序 k→j 改变，不可交换；matmul1 是
int）。交换后内层 j 的 c[i][j] 是普通内存写 → M42 判定自动 Vectorizable（无
寄存器归约、写写跨 j 无冲突）——比 Reducible 更干净，M44 直接 ld1/add/st1。

**保守判定（v1）**：
1. 嵌套链 H_i ⊃ H_j ⊃ H_k（j 循环 body 只含 k 循环 + E_k 的 c[i][j]=temp 旁
   语句——该语句是归约迁移吸收对象，唯一允许的旁语句）。
2. L_j、L_k 均可计数（induction_trip_count）+ 单位步进。
3. M42 对 L_k 判定 Reducible/Vectorizable，且方向同向：L_k 内所有内存访问的
   (j 系数, k 系数) 均 ≥ 0（matmul1：c[i][j]=(+,0)、a[i][k]=(0,+)、
   b[k][j]=(+,+)、b[i][k]=(0,+)、a[k][j]=(+,+) 全非负）。M42 现只分类当前
   BIV——需按指定 IV 分类的提取参数或独立双系数分类器。
4. 归约目标 c[i][j]：Global base + 零初始 + k 循环内无其他写。
5. 无 call、无 MemZero、单 latch、单出口。

**幂等性（关键陷阱）**：方向同向判定对称，交换后仍同向 → 无限交换。解法：
不对称条件——仅当内层循环存在 IV 系数 ∉ {0, 元素大小} 的访问（跨行访问）才
交换。交换前 L_k 的 a[k][j] k 系数 4000 → 交换；交换后 L_j 所有 j 系数 ∈
{0,4} → 停止。单测必须锁死二次判定不交换。

**实现**：`raana_ir/src/opt/passes/loop_interchange.rs`（~350-400 行含测试）；
注册 pass.rs aarch64 分支，rotate_loops 后、M44 前；CFG 变更后重建全部快照
（AGENTS.md 硬规则）；指令移动用 layout 移动（禁止手动改 used_by）。

**验收**：单测 4 个（matmul 形态成功交换：temp 参数消失 + c[i][j] 累加出现 +
k 测试 target=E_j；幂等；方向反拒绝；非零初始拒绝）；matmul1.sy -O2 aarch64
汇编内层连续访存；on/off 差分；RISC-V 零影响（不注册）；make test 回归。

**与 M44 衔接**：交换后内层 j 含 if(cond)（cond 含 j → 逐 lane 不同）→ 需
select 掩码 → 归 M44 v2（v1 无 select 跳过 matmul1，先覆盖内层已连续用例）。

**状态**：v1 已实现（2026-08-05，commit f884241，loop_interchange.rs ~700 行
含 4 单测，raana_ir 310 全绿）。注册于 pass.rs aarch64 分支（rotate_loops
后）。matmul1.sy -O2 编译通过但**不触发**（保守拒绝，无行为变化）。

**v1 与真实 matmul1 的 4 处差距**（真实 IR 核对 /tmp/ic_matmul1.raana）：
1. j 循环是 test-at-top（while_entry_16 的 br %49 = lt %vid_2, 1000），只有
   k 循环被 rotate 成 countdown。
2. j 循环体含 preheader_19（LICM 提升的 %56/%57 行 GEP，循环不变量）——
   shells 判定（纯 jump 壳）拒绝。
3. if 归约是真实分支 + phi 汇合（br %70 → then_23 / end_24 参数），不是
   select 形态——M42 identify_reduction 只认 select/binary。
4. k 出口块 while_end_22 带 4 个 block 参数（[i, j, k, temp]）——
   exit_of_one 的"出口无参数"检查拒绝。

**j 不旋转的真实原因（查证 rotate_loops.rs，2026-08-05）**：rotate 有两条
路径——rotate_countdown（cond 是 header 参数直接、entry 值非零）与
rotate_count_up（lt iv, bound → trip counter）。j 循环是 count-up，走
rotate_count_up；其 `carries_update` 检查 `b.lhs() == iv` 过严——j 的更新是
while_end_22 的 jump args `%78 = add(%73, 1)`，%73 是 while_end_22 的**块参数**
（phi 链，从 header 的 iv 经 k 循环 exit 的 br 臂传来）——匹配失败 → j 被当
entry 边 → 双 entry → 拒绝旋转。"非零"是 rotate_countdown 的条件，对 j 循环
（count-up 形态）根本不在判定路径上——**原"放宽非零"路径 A 的前提错误，对
matmul1 无效**（即使放宽也不会导致问题——等价性证明不变——但零收益）。

**v2 两条路径**：
- 路径 A'：放宽 rotate_count_up 的 carries_update 支持单层 phi 链（中间块
  参数从 header iv 直接传来 → b.lhs() 可解析为 iv+1）——~50 行 + 单测。
  让 j 循环旋转成 test-at-bottom → v1 变换骨架直接适用。代价：rotate 是双
  target 通用 pass，影响所有嵌套循环（等价变换，需 make test-riscv 回归）。
- 路径 B：interchange 自支持 test-at-top 外层（H_j br 重连 + preheader 移动
  + 骨架重排）——~200 行。影响面限 interchange（AArch64-only），但变换
  复杂度最高。

**v2 任务划分（路径 A' 优先）**：
- T1 M42 phi 汇合归约识别（dependence.rs identify_reduction 支持 backedge
  值 = 出口块参数 phi，两臂匹配 acc / acc±E）——~100 行 + 2 单测，独立。
- T2 rotate_count_up phi 链识别（rotate_loops.rs carries_update 放宽）——
  ~50 行 + 单测，独立，与 T1 可并行。
- T3 interchange 判定放宽（shells 允许 preheader 块=内容全 invariant；exit
  允许带参数）——~60 行 + 2 单测，依赖 T2。
- T4 interchange 变换：preheader 移动（k 循环 entry）+ E_k 参数表重写——
  ~150 行 + 真实形态单测（test-at-bottom j + preheader + 带参出口，用真实
  IR 结构固化），依赖 T3。难点：layout/use-def 一致性。
- T5 归约迁移 phi 形态（分支保留式：then 臂 load c[i][j]+delta+store，phi
  参数删 temp）——~100 行 + 测试，依赖 T1/T4。
- T6 端到端：matmul1.sy -O2 触发（IR 断言 j/k 交换 + 汇编内层连续）+ QEMU
  差分 + make test 回归。
依赖链：T1/T2 并行 → T3 → T4 → T5 → T6。matmul1 全链路 = interchange →
M44 v2（select 掩码）。

**v2 实现状态（2026-08-06）**：T1-T5 已完成并提交，真实 matmul1 触发：
- T1（4e0e985）：M42 phi 汇合归约识别。T2（09330b4）：rotate_count_up
  phi 链（value_flows_from 值流解析）——j 循环旋转成功。
- T3/T4/T5（9e5aab1）：判定放宽（壳链 = jump 路径 + preheader 允许 +
  exit 带参 + j IV 值流解析——BIV 误报 trip counter/不变量已排除）+ 变换
  （c_gep 重写移 update 块、H_k 删 temp、E_k/E_j 参数表重写、壳链重连
  H_j → 原 k 循环体、删除 terminator 清理 target used_by）+ 归约迁移
  （if 分支 phi 形态 → 内存累加，c_gep 必须插 update 前避免块内
  use-before-def）。
- 验证：matmul1 -O2 IR 断言 i-k-j 交换（b[k][j]/a[k][j]/c[i][j] 内层 j
  连续 +4B、a[i][k] 标量广播）；QEMU 差分 PASS；perf 60 例全 PASS；
  functional -O1 抽样 PASS；raana_ir 313 全绿。
- 已知形态差异（v2 变换期间修掉的坑，留档）：① last_shell 的 jump args
  在 drop_header_param 后少一个槽位——trip 初值须用调整后位置
  （k_trip_idx 减 temp 偏移）；② h_j 必须连到原 k 循环体首块（k_first），
  连到 b_k（k latch）会得空体 j 循环；③ update 选择须排除 Branch（br 的
  args 也消费 acc）；④ layout remove_inst 不清理 terminator 的 target
  used_by——已补。

**剩余（v3）**：真实形态单测（matmul1 结构固化，防回归）；性能验证
（交换后标量执行比原 ijk 略慢属预期——收益在 M44 向量化；matmul1 运行
时间变化记录）；与 M44 交互（内层 j 的 if 分支 → select 掩码，归 M44 v2）；
全量功能回归（用户自行）。

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

##### M44 v1 已实现（2026-08-05，hermes 线 feat/loop-vectorize-hermes，验收数据）

- 实现：`loop_vectorize.rs`（+注册 pass.rs aarch64 块 chain_to_switch 后 LICM 前）。
  旋转后 count-up 循环（header=[iv,t] 直通 latch，latch 底部 `br t' header/exit`），
  trip=4Q+R 精确常量 ≥4 → 主循环 Q 次向量迭代（counter 入口改 4Q、iv/t 步进 ±4、
  测试不变），R 次标量迭代以常量下标 peel 成直链 epilogue（无运行时 guard）。
  连续 load→`<4 x T>` Load（同地址原位重写）、Add/Sub/Mul→lane-wise 向量 op
  （不变操作数 VectorSplat）、连续 store→向量 store。
- M42 判定修正：旋转循环的 counter（`t'=t-1`，体内无其他使用）恒被标为
  Reducible{IntSub}，故验收判定 = "非 Forbidden"；真归约由操作数规则拒绝
  （accumulator 块参数不可作向量操作数，Select 不在白名单）——Reducible 验收
  语义与 goal 原文"Reducible 跳过"的偏差已记录在文件头注释。
- 单测 9 个全绿（trip=16 i32、trip=18 peel 2 块、stride=8 拒绝、select 拒绝、
  真归约拒绝、非 exact 拒绝、Param base 拒绝、f32、幂等）；raana_ir 315 全过；
  workspace 全过。
- 端到端（qemu harness 差分，临时用例 tests/perf/elem_vect_check.sy 已删）：
  `b[i]=a[i]+1` 全局数组 trip=16，-O2 汇编出 `ldr q → dup v.4s（splat 常量）→
  add v.4s → str q`，IV 步进 +4；输出 1..16 与标量一致。-O0/-O1/-O2 三级 PASS。
- **perf 五例（01_mm1/fft1/sl1/conv2d-1/matmul1）v1 均不出向量指令（静态事实，
  不声称收益），原因分类**：
  1. 内层为归约（M42 Reducible，v1 拒绝）：01_mm1/conv2d-1 的 C[i][j]+= 内核、
     matmul1（且含 select）；
  2. 体内含非白名单 op：fft1（rem/div/sar/shr/and）、sl1（div/sar）、01_mm1
     （shl）、conv2d-1（rem/sar/shr/and，多为边界/下标运算）；
  3. 就地同地址读写（M42 IntraIterationConflict）与 Param base 在真实用例中
     也常见。
- **关键发现（v2 候选）**：前端把 `*2` 等常量乘 strength-reduce 成 `shl`，
  v1 白名单（Add/Sub/Mul）直接拒绝——`b[i]=a[i]*2+1` 这类最普通的循环也过
  不了。v2 加 Shl（lane-wise 移位，NEON sshl 直出）即可解锁大量真实循环。
  另：lowering 对向量 load/store 目前出 `ldr/str q`（VecLd1/VecSt1 MInst 已
  定义但未接 lower 路径），验收口径按实际汇编记录。
- 遗留：R=0 无 epilogue 时 latch f_target 保持原 exit；常量不进 layout（DCE
  is_critical 对 laid-out Integer unreachable，项目约定）。

##### M44 v2 方向扫描（2026-08-05，perf corpus 拒绝原因分布，M44_TRACE=1）

- 工具：loop_vectorize.rs 内 `M44_TRACE=1` env 门控 trace（analyze_loop 每个拒绝
  点打点，含 M42 ForbidReason 明细），后续 v2 验证沿用。
- 关键结论：**payload 级拒绝几乎为零**（全 perf corpus 仅 Rem×2 循环、
  load_unmodeled×1、load_classify×1）——v1 的 op 白名单/stride/对齐检查不是
  真实瓶颈（shl 等担心不成立：前端 *2 的 shl 循环根本没走到 payload）。
- 真实拦截分布（去重，perf 全量）：
  - not_innermost 969（外层循环，正常）
  - shape_body_not_2_blocks 363（体内含分支/多块）
  - shape_header_multi_inst 315（rotate 未处理的 test-at-top 头部：compare+
    branch 在 header——rotate 因 exit 读 IV 等拒绝）
  - m42_forbidden:CallInBody 186（输入读取循环 getint/getarray、计时调用——
    真不可向量化）
  - m42_forbidden:IntraIterationConflict 174（就地同地址 R/W 与内存累加
    C[i][j]+= 内核，含 mm）
  - params_not_2 162（寄存器归约 3 参数 [iv, acc, t]，radixSort/ludcmp 内核）
  - NoInductionVariable 90、latch_exit_inside_or_args 27、
    LoopCarriedConflict 18、NonAffineIndex 12、DynamicMemZero 12、
    entry_trip_not_const 12（运行时 trip → M43）、UnknownBase/RuntimeCoefficient 各 6
- v2 方向排序（按性价比 + 协调成本）：
  1. **B1 寄存器归约向量化**（params_not_2 + M42 Reducible 判定已就绪，不碰
     dependence.rs/switch 线）：sum/count 类内核 → 向量 acc + 出口 addv。
  2. **test-at-top 形态**（shape_header_multi_inst，无 M42 依赖）：rotate 拒绝的
     循环直接按 count-up 处理（header 测试保留，counter 方案不变）。
  3. **in-place/内存累加**（IntraIterationConflict，需放宽 dependence.rs
     test_access_pair：同地址 R/W 且写依赖读 → 元素级安全）——dependence.rs 与
     switch 线共享，须先协调。

##### M44 v2 B1（寄存器归约向量化）执行记录（2026-08-05）

- 实现（57e882f，547 行）：3 参数 [iv, acc, t] + M42 Reducible{IntAdd/IntSub} →
  acc 参数原位 set_type(<4 x T>)、latch acc 更新重写为 lane-wise 向量累加、
  出口 vec_reduce 块 VectorReduce(Add) → 标量 acc_final、epilogue 标量 acc 参数
  链（R 次）、entry 边 splat(acc_init)。新增 InstData::set_type（instruction.rs
  +7 行）。防护：acc 用户逃逸 exit 拒绝、IntMul/Min/Max 后置、幂等靠参数类型
  检查。单测 +5（IntAdd/IntSub/epilogue 链/幂等/IntMul 拒绝），raana_ir 321
  全过；合成用例 qemu 差分 -O0/-O1/-O2 三级 PASS；汇编出 dup → ldr q →
  add v.4s → **addv s** → fmov。
- **真实 corpus 命中 = 0（诚实数据）**，原因：
  1. radixSort 的 14×2 个 b1_target 是 M42 误标：getNumPos 类循环 3 参数中
     M42 把实际 IV（add-one 参数）识别为 accumulator，真 carried value 是
     数据相关 sar 链（%vid_0）→ 我的 iv 单位步进检查正确拒绝
     （non_unit_step）。
  2. kernel_ludcmp 的归约是内存累加（C[i][j]+= → IntraIterationConflict），
     属 B3（in-place）范围，非 B1。
  3. perf corpus 中"真寄存器归约 + 2 块 + exact trip"形态 ≈ 不存在；fft 的
     IntSub 归约被体内 rem/sar 等非白名单 op 拦截。
- 结论：B1 是能力储备（正确性已验证的寄存器归约向量化），当前无 perf 收益；
  且与 interchange 场景（归约迁移成内存累加后走 v1 主路径）不重叠。后续要吃
  真实用例，正确优先级是 B3（in-place/内存累加，需 M42 协调）。
- 遗留：临时用例 tests/perf/red16.* 已删；addv 命中扫描脚本在
  /tmp/m44_corpus。

##### M44 v2 目标 1（payload 白名单扩展）执行记录（2026-08-05）

- 实现：loop_vectorize.rs 白名单扩至 Shl/Shr/Sar/And/Or/Xor/Div/Min/Max。
  - 类型约束：Shl/Shr/Sar/And/Or/Xor 仅 i32（f32 拒绝，新 trace
    binary_shift_bitwise_not_i32）；**Div 仅 f32**（i32 拒绝，trace
    binary_int_div_no_neon——goal 原文"Div 向量化（sdiv/fdiv v.4s）"前提
    错误：NEON 无整数向量除法，`sdiv` 仅标量形态，LLVM MC 拒收 `sdiv
    v.4s`，ACLE 无 vdivq_s32；常量除数 i32 div 由 sr 移位链覆盖，见下）；
    Min/Max i32+f32 均可；Rem 保持拒绝。
  - **配套 anon_armv8 最小 lowering（goal 原文假设 lowering 已支持向量
    Shl/Shr/Div，实为 lowering_panic 缺口；只扩白名单会让 -O2 panic，
    开工前经用户确认 A 方案）**：instructions.rs 新增 VecShiftOp/VecShift
    （立即数 shl/ushr/sshr + 寄存器 sshl/ushl，变量右移先 VecNeg 再
    sshl/ushl——NEON 无寄存器形态右移）、VecDiv（仅 fdiv v.4s）、VecNeg；
    emit + RegUseCollector + passes/dce.rs 纯函数表 + sched/dag.rs InstDeps
    同步；lower.rs lower_vector_binary 接 Shl/Shr/Sar/Div（常量移位量经
    VectorSplat-of-Integer 识别 → 立即数形态，i32 Div 防御性 panic）。
  - **存量 f32 splat 语法修正**：VectorSplat f32 路径原 emit `dup vd.4s,
    sn`，LLVM MC 拒收（此前 f32 向量化从未过真实汇编器，存量隐患）→
    经寄存器别名输出 `dup vd.4s, vn.s[0]`（sN = vN 低 32 位；clang
    vdupq_n_f32 同款）。两个断言旧语法既有测试同步更新。
  - Sar 入白名单的原因：sr.rs 对常量除数除法产出 sar 链（a[i]/2 →
    sar/shr/add/sar），Sar 与 Shl/Shr 同一 NEON 指令族，漏掉则 sr 链循环
    全拒。原始 goal 只列 Shl/Shr，此为执行期补充（仍在 loop_vectorize.rs
    内，不碰其他 pass）。
- 验证：
  - 单测 +5（shift_xor 循环、f32 div、f32 shl 拒绝、i32 div 拒绝、新 op
    幂等），raana_ir 333 全过（基线 328）；anon_armv8 126 全过（含新
    emit 单测 + dup-f32 断言更新）。
  - 端到端（tests/perf/v2_whitelist_tmp.sy 临时用例，跑完删除）：-O2
    汇编通过 host clang 语法验证 + make test 差分 -O0/-O1/-O2 三级 PASS：
    a[i]*2 → shl v.4s,#1（sr 改写 mul→shl 后向量化）；a[i]/2 →
    ushr#31+add+sshr#1（sr 移位链向量化）；a[i]%4 → sshr+ushr+add+and
    v.16b（sr 链）；fa[i]/2.0 → dup v.4s,vN.s[0] + fdiv v.4s；
    a[i]/q[i]（变量除数 i32 div）正确保持标量（标量 sdiv）。
  - M44_TRACE 复扫（61 例，去重口径 (case,func,header,reason)）：
    0 编译错误；payload_inst_rejected 仅 Rem×3（conv2d-1/2/3，符合 Rem
    保持拒绝）；基线 Rem×2/load_unmodeled×1/load_classify×1 → 新白名单 op
    零拒绝、无新增拒绝类型。corpus 向量化命中仍 0——再次确认 payload
    白名单不是真实瓶颈（真实拦截：not_innermost/shape_header_multi_inst/
    shape_body/m42_forbidden/params_not_2，见上方 v2 方向扫描）。
- 结论：目标 1 是能力储备（白名单 + 向量移位/除法 lowering 就绪，含
  f32 splat 语法修正），单靠它无 perf 收益；真实优先级不变：B3 in-place
  内存累加 > B2 test-at-top。后续目标 2/3 完成后，白名单与新增 lowering
  直接生效。

##### M44 v2 目标 2（B3 内存累加）执行记录（2026-08-05）

- 实现（两个原子 commit，见 Vectorize_Progress.md 目标 2 完成记录）：
  1. ce4976d：dependence.rs `is_elementwise_inplace`——同 base、同
     byte_coefficient（≠0）、同 constant_part 的读-写对，store 值经纯
     def-use 链依赖 load 值即元素级安全，跳过冲突测试；系数 0 / 无关写 /
     跨迭代仍 Forbidden。interchange 核对：k-loop 判定面不变（纯寄存器
     归约，line 841 拒写归约目标 base），4 测不回归。
  2. 1263cda：loop_vectorize 外层 IV passthrough 参数支持——嵌套内核
     内层循环 header [i, j, k, t] 形态（back_args[i]==params[i] 识别），
     is_loop_invariant 视 passthrough 为不变式，epilogue/reduce 块转发
     passthrough 值。这是嵌套内核（mm/matmul/conv2d/ludcmp）向量化的
     通用前置。
- 验证：
  - raana_ir 337 全过；dependence 4 测改写/新增。
  - E2E（临时用例已删）：c[i][j]+=a[i][k]*b[k][j] 内核 -O2 出
    dup v.4s + ldr q ×2 + mul v.4s + add v.4s + str q；make test
    -O0/-O1/-O2 三级 PASS。
  - corpus 复扫（61 例）：0 编译错误；IntraIterationConflict 174 → 0；
    params_not_2 162 → 12；exit_has_params ×50 新增（保守拒绝）。
    向量化命中 0 → 3/60（matmul1/2/3 清零循环 dup+str q）。
- 诚实边界：corpus 计算内核未出向量——01_mm 系数组参数（alignment 门，
  需 M43 versioning 或对齐放宽）、matmul 交换后内核奇偶掩码 select
  （目标 4）。B3 后真实优先级：目标 3（test-at-top，shape_header_multi_
  inst 200 次）> 目标 4（select 掩码）。

##### M44 v2 目标 3（B2 test-at-top）执行记录（2026-08-05）

- 实现（commit 7e01d97，详见 Vectorize_Progress.md 目标 3 完成记录）：
  analyze_loop 识别 header 恰为 [lt iv, bound; br]（bound 编译期常量）
  + latch plain jump 的 test-at-top 形态；trip = bound - i0；R>0 时
  exit 读 iv 保守拒绝；apply 物化 counter（手写 add_param 等价，
  BlockArgRef 追加 header 参数）并替换 bound 测试为 counter 测试。
- 调试教训：T3 检查方向写反（f_target 应不在 loop 内）致正例误拒，
  逐项 trace 定位；test-at-top 的 effective 参数计算不排除 counter_slot。
- 验证：raana_ir 340 全过（+3 单测）；合成用例 -O2 出
  ldr q + dup v.4s + add v.4s + str q；make test 三级 PASS；
  corpus 复扫 shape_header_multi_inst 230 → 168、0 编译错误。
- 诚实边界：corpus 无 test-at-top 循环出向量（单参数+常量 bound+
  payload 干净+exit 不读 iv 无交集）；test_at_top_multi_param ×48、
  test_at_top_bound_not_const ×18 保守拒绝；多参数 test-at-top 后置。
- B3/B2 后真实优先级：目标 4（select 掩码，matmul 交换内核）> M43
  versioning（数组参数 alignment）> 多参数 test-at-top。

##### M44 v2 目标 4（select 掩码）执行记录（2026-08-05）

- 实现（commit 88e1d18，详见 Vectorize_Progress.md 目标 4 完成记录）：
  Class::VecSelect——Select(cond,t,f) 条件不变量时接受（逐 lane 拒绝
  select_lane_cond），变换为 (t & ~m)|(f & m)、m=-(eq(cond,0))，
  全复用既有向量 binary，零 lowering 改动；二遍检查补 VecSelect
  操作数校验 + epilogue clone 补 Select。
- 验证：raana_ir 342 全过（+2 单测：不变量条件向量化/逐 lane 拒绝）。
- 管线限制（验收 3 诚实记录）：if_conversion 只提升 i32 binary
  （分支含 load 不转）；全不变量 select 被 LICM hoist（既有 splat
  路径，合成用例出 dup+str q）；matmul1 真实 select 为 min 归约
  （IR 137 行），条件逐 lane → v3。
- **M44 v2 收官**：目标 1-4 全部 [x]。corpus 命中 3/60（matmul 清零）；
  能力储备：白名单/移位除法 lowering、B3 同地址放宽、passthrough、
  test-at-top、标量 select。
- v3 缺口清单：逐 lane select（bsl/csel lowering + if_conversion 增强）；
  多参数 test-at-top；数组参数 alignment（M43 versioning）；matmul1
  min 归约 + 奇偶掩码内核。

##### M44 v2 收官：拒绝原因分类（2026-08-05 调研，写进 TODO 的深度）

Corpus 61 例（0 编译错误）拒绝分布（dedup，top）：not_innermost 276、
shape_header_multi_inst 168、shape_body_not_2_blocks 111、
test_at_top_multi_param 48、exit_has_params 48、non_unit_step 45、
NoInductionVariable 30、bound_not_const 18+18、params_not_2 12、
CallInBody 9、exit_arg_not_acc 6、Rem 3、IntraIterationConflict
（matmul1 残余）、select_lane_cond（新增）。

分类（按根因层）：

A. 形态层（analyze 前置保守拒绝，纯 loop_vectorize.rs）
- A1 not_innermost（276）：只处理最内层循环——正确保守，非缺口
  （外层循环向量化需要多块体处理=B1 类）。
- A2 shape_header_multi_inst（168）：header 3+ 指令。代表：conv2d
  计算循环。根因：test-at-top 只认恰 [lt,br]、rotated 只认 [jump]；
  bound 计算/额外指令在 header 即拒。修法候选：bound 计算提升
  preheader 后识别（需先确认代表形态）。
- A3 test_at_top_multi_param（48）：test-at-top + 外层 IV passthrough
  （header 4-7 参数）。代表：conv2d 内核（i/j/k/l 4 层嵌套，每层
  传 6-7 参数）、01_mm1 计算内核（BB20）。根因：目标 3 保守
  n_params==1。修法：step 4 对 test-at-top 复用目标 2 的 passthrough
  识别（back-edge 原样转发）——中等改动，**高 ROI**（01_mm/conv2d
  内核解锁）。
- A4 exit_has_params（48）：exit 块带额外 phi 参数。代表：matmul1
  min/sum 循环 exit 携带归约状态。修法：确认代表形态后扩展 exit
  参数允许集（passthrough+acc 之外）。
- A5 non_unit_step（45）：IV 步长非 1。修法：步长归一化
  （i=2k → 索引变换），中优先级。
- A6 bound 非常量（36）：test_at_top_bound_not_const /
  entry_trip_not_const。无 versioning 硬规则 → 保持拒绝（M43
  versioning 时解锁）。
- A7 params_not_2（12）：entry args 数量不符，passthrough 后残余。

B. 体层（payload/body）
- B1 shape_body_not_2_blocks（111）：循环体多块（if/else 未转换）。
  代表：matmul1 奇偶掩码内核（41-50 行：
  `if(a[i][k]*b[k][j]%2==0) temp += b[i][k]*a[k][j]`）。根因：
  if_conversion 只提升 i32 binary（safe_arm_binary），分支含
  load/store 不转 select。修法候选：(a) if_conversion 提升面扩展
  （load 依赖链 hoist）；(b) vectorizer 直接处理 if 头多块体
  （单出口 + 掩码）。**matmul1 内核最大拦路石**。
- B2 select_lane_cond（少）：select 条件依赖循环值。代表：matmul1
  min 归约（IR %102 = select %101, %100, %vid_6）。根因：IR 无向量
  select、lowering 无 bsl/csel。修法：anon_armv8 VecCsel/bsl
  lowering——非平凡。
- B3 verdict/exit_arg_not_acc（6+）：B1 归约更新是 select/if 包裹
  （非纯 binary）。代表：matmul1 min。修法：归约识别扩展。
- B4 Rem payload（3）：conv2d 内核 % 运算。NEON 无整数向量除法/
  取模（sr 只改写常量除数）→ 保持拒绝（ISA 限制）。
- B5 load_unmodeled/load_classify（2+）：三维 GEP 列访问
  （matmul1 转置 b[i][j]=a[j][i]）。非连续访存需 gather（硬规则
  禁止）→ 正确拒绝，不动。
- B6 CallInBody（9）：读入循环 getarray/getint——正确拒绝。
- B7 NoInductionVariable（30）：M42 找不到 IV。待确认代表形态。

C. 依赖层（M42/B3，dependence.rs）
- C1 IntraIterationConflict（matmul1 内核残余）：c[i][j] += ... 在
  if 块内——B3 豁免（is_elementwise_inplace）未覆盖：写值依赖链
  含多个 load 或跨块条件执行。修法：value_flows_to 扩展。**依赖
  B1 前置**（单块体后 B3 豁免面才完整）。

优先级/依赖链（perf ROI）：
- matmul1 内核 = B1（if 掩码多块体）→ C1（B3 覆盖）→ B2/B3
  （掩码归约 select）——**B1 是前置**
- 01_mm 系 = A3（多参数 test-at-top）
- conv2d = A3 + A2 + B4(Rem, ISA 限制)
- matmul1 转置（B5）、min（B2/B3）为 v3 lowering 缺口

##### B1 多块体处理（单臂 if 形态）细化计划（2026-08-05）

前置验证（已完成, commit 1497fb8）：逐 lane select 掩码向量化 spike——
VecSelect 分类放宽（payload 值条件）+ 白名单加 Eq/Gt，单测全绿
（vectorizes_lane_cond_select），证明掩码组合 ((t & ~m)|(f & m),
m=-(eq(cond,0))) 不需要 bsl/csel lowering。真实管线链路仍需 B1
（if_conversion 不提升含 load 分支 → select 不产生 → 多块体）。

目标：matmul1 奇偶掩码内核（源码 41-50 行）+ corpus 111 次
shape_body_not_2_blocks 向量化。

形态识别（analyze_loop body 检测扩展）：
- 接受 body = {header, latch, if_head, arm, merge}（单臂 if）：
  if_head: br cond → arm / merge（一侧空跳）；arm: 单出口
  jump merge（含 load/binary/store）；merge: payload 尾 + latch
- 判定：arm 单出口、merge 单入、if_head 仅此一分支结构

payload 收集（跨块扩展）：
- arm + merge 内 load/binary/store 统一进 payload（现有收集扩展）
- load 地址检查复用 load_classify（base=header 参数/不变量）

变换（apply）：
1. store 条件化：store src → (new_src & ~m) | (old_val & m)，
   m = -(eq(cond,0))——复用 VecSelect 掩码变换（spike 就绪）
2. old_val：arm 内若已 load 同一 store 地址则复用，否则在 arm
   load 链中补插 load
3. 分支消除：if_head/arm 并入（块删 + 边重定向），循环体回 2 块

归约识别扩展（matmul1 temp 特有, 第三步）：
- 掩码化后 temp 更新 = select(m, add(temp, d), temp)——B1 归约
  识别接受 select 更新链（acc' = select(m, add(acc,d), acc)）

依赖检查：
- 跨块 def-use（arm 内 load → binary → store）检查；cond 必须
  payload 值或不变量（逐 lane 掩码 ✓）

验收：
- 单测：构造 if 头多块体循环向量化 + 无标量残留
- matmul1 -O2 内核出 ldr q + cmgt/and/eor + add v.4s + str q
  （clang 汇编验证）
- make test 三级差分 PASS；corpus 复扫 shape_body_not_2_blocks
  下降（目标 ≥30 次）
- 改动量估计：~300 行（纯 loop_vectorize.rs）

风险与前置：
- 实现前先 dump matmul1 IC 交换后的真实内核形态（temp 归约层级、
  if 位置）——最大不确定点
- 跨块 payload 的 def-use/escape 检查是新逻辑（测试覆盖重点）
- 不碰 if_conversion/lowering

达成判定：上述验收全过 = B1 达成；matmul1 掩码内核出向量指令。

##### A2/A4 调研结论（2026-08-05, subagent 超时后主 agent 补跑）

A2 shape_header_multi_inst（168 次）——**整体低 ROI，降级**：
- 形态 1（crypto-1 padding 循环 `while (input_len % 64 != 56)`）：
  header 7 条指令（sar/shr/add/and/sub 取模链 + lt + br），bound
  依赖循环内变化的 input_len——不是范围循环 → **正确拒绝，不修**
- 形态 2（bound 常量表达式，如 `while (i < 5*5)`）：header 含
  未折叠的 mul + [lt, br] → 首轮 shape_header_multi_inst，但
  rotate 后续轮次兜底转 rotated → 最终走 rotated 路径——**修法
  （header 允许不变量指令）ROI 低**，暂不修
- 结论：168 次中可修子类有限；修 A2 前先确认「rotate 不兜底」
  的子类是否存在（当前证据：rotate 兜底普遍）

A4 exit_has_params（48 次）——**高 ROI，A3+A4+C1 解锁 01_mm**：
- 代表形态（01_mm1 计算内核 while_entry_16_mm_inline_34，IR
  143-155 行）：header [i, j, k, t] 4 参数（A3 后 i/k 为
  passthrough，effective=[j] ✓）；exit while_end_18 带 3 参数
  [i, j 终值, k]——**IV 终值**（j 退出时的值，传给外层循环）
- 根因：exit 参数允许集 = passthrough + acc，IV 终值不在集内
- 修法：exit 参数分类接受 IV 终值（向量化后 = i0 + step*Q，
  即 i0+4Q）——apply 时 exit 边重写传计算值；~60 行
  （analyze step 4/7 的 exit 参数检查 + apply 的 exit 边重写）
- 预期提升：01_mm1/2/3 计算内核解锁（配合 A3 + C1）；
  exit_has_params 计数显著下降
- 注意：该内核同时含 C[i][j] 同地址 R/W（写值依赖链含 B 的
  load %119）——需 C1（B3 覆盖多 load 依赖链）才能最终向量化；
  A4 单独做可让 analyze 通过 exit 检查（下游 C1 完成前不向量化）

##### Corpus 复扫（A3+A4+B1+spike 后, 2026-08-05）

60 例 0 编译错误。拒绝分布对比（目标 3 后 → 现在）：
- exit_has_params: 48 → 0（A4 完成，全部放行）
- shape_body_not_2_blocks: 111 → 96（B1 -15，其余为双臂/多出口 if）
- shape_header_multi_inst: 168 → 192（+24）、entry_trip_not_const:
  18 → 30（+12）、exit_arg_not_acc: 6 → 15（+9）——均为 exit 放行
  后拒绝点后移（progress，非回归）
- 不变: not_innermost 276、test_at_top_multi_param 48（2+ 有效
  参数）、non_unit_step 45、NoInductionVariable 30、bound_not_const
  18、params_not_2 12、Rem 3
- 向量化命中: 3/60 不变（matmul 清零 dup+str q）——单测全部能力
  验证通过，但 corpus 真实组合未通：
  * matmul1 掩码内核: B1✓ A4✓ → exit_arg_not_acc（B3 归约识别：
    temp 局部归约 exit 不带 acc——待设计）
  * 01_mm 内核: A3✓ A4✓ → entry_trip_not_const（bound=运行时 n,
    M43 versioning）
  * conv2d: multi_param(2+ 有效) + Rem(ISA) + shape_header
- 剩余优先级: B3 归约识别（matmul1）> M43 versioning（01_mm/
  conv2d bound）> 多参数 test-at-top 2+ 有效参数 > A2（低 ROI）

##### B3 归约 exit 参数值匹配方案（2026-08-05 细化, matmul1 最后一块）

根因（IR 定位 /tmp/mm1_v2.ir + dependence.rs 复核）：
- matmul1 的 temp 归约已被前端/优化改写为 c[i][j] 直接内存累加
  （arm 内 load+add+store）——真实归约是内存（B3
  is_elementwise_inplace 面），value_flows_to 是宽松 def-use（不要求
  唯一 load 依赖）→ **C1 已满足，无需扩展**
- exit_arg_not_acc 来自两个循环：
  (a) min 循环（entry_28 [i,j,min,t]）：B1 归约路径 + exit [i, min]——
      acc(min) 在 exit 参数**位置 1**，A4 的 `pos == 0` 位置假设误拒
  (b) k 循环（掩码内核）：effective=[k]（1 个）→ acc_info None 不走
      B1——但 exit [i] 的 A4 分类 pos==0 检查同样误拒
- 修法（loop_vectorize.rs 两处）：
  1. 6b exit 分类改**值匹配**：arg == iv_next → IvFinal；arg 匹配
     passthrough 值 → Passthrough；arg == acc 更新值 → Acc（任意
     位置）；其余 exit_has_params
  2. ReductionPlan.exit_acc_param 从 exit_specs 找 Acc 位置（不再
     假设位置 0）
- 预期：matmul1 掩码内核（k 循环）+ min 循环解锁出向量；B1 既有
  路径（acc 在参数 0）不回归；01_mm 不受影响（bound 运行时仍拒）
- 验收：matmul1 -O2 汇编出 ldr q + cmgt/and/eor + add v.4s +
  str q；make test 三级差分；raana_ir 全量；corpus 复扫
  exit_arg_not_acc 下降

##### 最短路计划: sum 循环向量化 (matmul1/2/3 出 addv, 2026-08-05)

目标: `sum += c[i][j]` 嵌套循环 (i 外层 sum 归约, j 内层) 向量化,
matmul1/2/3 同时出 ldr q + add v.4s + addv 归约。

根因 (已确认):
1. M42 identify_reduction 返回第一个匹配参数 — `j'=add(j,1)`
   (IV 步进) 被 match_acc_update_op 误标为 acc (Add 模式匹配),
   sum (slot 2) 轮不到
2. acc 种子 = 外层 sum (BlockArgRef) 时 VectorSplat 物化错误
   (dup w4=0) — lowering 对 BlockArgRef 的 splat 源处理有缺陷

改动 (2 文件):
1. dependence.rs identify_reduction: 遍历时跳过「IV 参数」—
   先看 BasicInductionVariableAnalysis (interchange 已用) 能否
   识别 sum 循环的 iv (j); 能则 M42 直接排除 iv 参数, 不能则用
   「update == add(param, 常量) 且 param 的 users 含 GEP offset」
   启发 (保守, sum+=1 会误拒但收益小可接受)
2. anon_armv8 (或 vectorizer) VectorSplat(BlockArgRef) 修复:
   诊断 dup 源寄存器取错路径; 物化 seed 值 (preheader 计算 →
   splat) 或修 lowering 的 BlockArgRef 源

正确性保障 (硬性验收):
- 单测: vectorizes_nested_sum (i 外层 sum + j 内层向量化,
  addv 归约) + 断言 make test 差分
- matmul1/2/3 -O0/-O1/-O2 三级差分全 PASS (错码 = 失败)
- 防御保留: b1_acc_is_iv / b1_acc_init_block_arg 继续兜底,
  修复后若仍触发说明识别未生效 (trace 核查)
- corpus 复扫: matmul 三兄弟出现 addv; 其他用例无回归
- 不做: 掩码内核结构调试 / M43 versioning (后续)

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
- 调度验证器（§5 P2 `verify_sched_deps`）覆盖向量 NZCV / 寄存器依赖。

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

### 3.4 非循环 NEON 优化实证结论（2026-08-05，全量 perf 汇编统计）

方法：60 个 perf 用例 × `--target aarch64 -O2 -S`，统计向量指令、csel/cset、
转换、清零形态。结论：当前编译器零向量指令（无向量化 pass，符合预期）；
六个候选方向中两个有实证基础，两个低优先，两个排除。排除项记录理由防重复提议。

**可做（有实证）**

- **A. 小固定数组内联向量清零**（独立小项，不依赖向量化 pass）：
  现状 `int words[80]={0}`（crypto-1）等局部零初始化已走 `.Lsoyo_memzero`
  （M53 产物，运行时 bl + 16B 对齐检查 + zva/byte loop）；320B 每次调用都
  bl。改进：固定大小（≤ 阈值如 64-128B）且编译期已知的零初始化内联为
  `movi v0.4s,#0` + `stp q0,q0` 展开（emit_zero_init 已含 Vector 分支，需把
  int/float 数组按 16B 块打包）；大数组保持 memzero。收益：crypto-1
  （sha1 每块调 words 清零）免调用开销；静态指令数可量化。
- **B. SLP 基本块向量化**（并入 M45）：perf 用例热循环体（conv2d 的
  init_matrix/row_reduce、01_mm/matmul 内层）展开后存在相邻同构标量 op；
  switch 基建的 loop_unroll（小常数循环全展开）已合入主线，是 SLP 的现成
  输入。注意 SysY 源码无三目（全量 tern=0），SLP 的独立直通代码场景少，
  主要收益来自"循环向量化 + 展开后的补充打包"——排在 M44 之后。

**低优先（需先补基建或收益待量化）**

- **C. 标量 min/max 内建替代**：AArch64 标量 `smin/smax/fmin/fmax` MInst
  已存在（instructions.rs:402-407）但 lower.rs:339-344 对标量
  `BinaryOp::Min/Max` 直接 panic（vector-only）——先补标量 ISel + select→
  min/max 模式匹配（`select(a>b,a,b)` → `smin`，省 1 条 cmp）。perf 实证：
  csel 密度低（huffman 8 / h-9 4 / h-4、crc 3，其余 ≤1），且 huffman 的
  csel 是 `cmp+ccmp+csel` 逻辑 select（&& / || 融合产物）非 min/max 形态
  ——收益有限，量化后再定。
- **D. 向量 bsl 无分支选择**：标量层 if-conversion（csel/cset）已成熟；向量
  版需多组并行比较 + mask + bsl，依赖 SLP/循环向量化前置 + 浮点比较
  MInst（fcmgt 缺失，lower.rs:408 panic）。随 M44/M45 伴生，不单独立项。

**排除（perf 无实证基础，防重复提议）**

- **批量 int↔float 转换**：全量 perf 汇编 `scvtf/fcvtzs/ucvtf` = 0——用例
  无成组转换热点（SysY 隐式转换在数值用例中少见），无向量化对象。
- **ld2/ld3/ld4 交错存取（AoS→SoA）**：全量源码 `[2*i]` 模式 = 0、汇编
  ld2/st2 = 0；fft1 是整数实数组（非复数交错）。perf corpus 无交错布局。

---

## 4. 主计划 E：内存分析 / 别名分析（进行中）

> 已完成：M49（内存对象/别名分析）、M50（完整 Purity 分析）、
> M53（IPSCCP 主存模拟）、M54（LICM load 外提）。剩余：M51（指针槽
> 消除/SROA）、M52（DSE）、M55/M56。详细设计见
> `docs/memory_alias_analysis.md`；本计划为内存类优化提供公共底座：
> 别名分析 + 函数 mod-ref 摘要。SysY 无指针、无堆，栈内存对象大小与
> 维度编译期已知（数组维度为 ConstExp），建模成本低、精度高。

#### M49：内存对象 / 别名分析（已完成）

- 交付：`analysis_passes/memory.rs`——MemObject{Alloc,Global,Param,
  Unknown}、GetBaseObject（指针槽 store-once 解析 + block param phi
  解析）、GEP 常量字节偏移、过程内别名规则矩阵。验收：单测 10 个。
- 备注：函数级 builder 无法检视全局 inst，查询统一走 `&impl Arena`。

#### M50：函数副作用摘要 / Purity（已完成）

- 交付：`analysis_passes/effects.rs`——call graph 双不动点：自顶向下
  points-to（形参 → 具体对象集合，递归收敛）+ 自底向上 mod-ref 摘要
  （reads/writes/unknown/io）；is_pure / is_removable /
  may_write_memory / call_write_roots；跨过程 alias 细化（Param vs
  Global/Param/自身 Alloc）；sysylib 边界显式建模。验收：单测 12 个。

#### M51：指针槽消除 + 栈对象提升（SROA 子集，未做）

- 现状缺口/现象（conv2d-1 实测 IR）：数组形参被前端存入局部指针槽
  （`%11 = alloc <**i32>; store %24, %11`），热循环每轮
  `%50 = load %11` / `%88 = load %v_K` 重载形参指针。
- 注意：M54（LICM load 外提）已捕获该模式的主要收益——三个形参指针槽
  load 全部提出嵌套循环到 entry，内层 k 循环每轮省 3 条 ldr。SROA 对
  「常量化元素访问的局部数组」仍有独立收益。
- 变换：write-once/read-many 的 Alloc 槽（函数内至多一次 store、M50
  证明无逃逸）→ 用存储值替换全部 load 并删槽；局部 Alloc 全部访问
  经常量 GEP 偏移且未逃逸 → 逐元素提升为 SSA 值（转发由 M52 承接）。

#### M52：Store-to-load forwarding + 死 Store 消除（DSE/DLE，执行计划 2026-08-04）

- 现状缺口：DCE 不删任何 Store；GVN 只做 load-CSE、无 forwarding。
- 注意：M53（IPSCCP 主存模拟）已覆盖常量格内的 store→load 转发。
- 变换：同 root 同 offset 相邻 store→load 转发；覆盖前无可能读取的死
  store 删除；「load 原值存回同址」冗余回写删除（conv2d main 尾部
  `store %100, %gv_repeat_factor; store %99, %gv_N_eff` 实证）；MemZero
  按整区间 store 统一建模。
- 附带：GVN ScopedLoadLeaders 的全局失效改为按 M49 may_alias 失效
  （零新增 pass 的 load-CSE 精度提升）。

**详细执行计划（2026-08-04，合并完成后首项）**

一、现象与数据
- conv2d main 尾部死回写：`store %100, %gv_repeat_factor; store %99,
  %gv_N_eff`——函数内只读全局，GSP 无条件回写。
- heavy_read（互评实证）：GSP load-once-write-back 在函数尾留
  `store %1, %gv_g` → 对方 Purity 正确判为「写全局」→ 拒绝 call 外提。
  删死回写后 read-only 判定自动修正（LICM call 外提 / DCE 删除纯调用
  的判定都会变准）。
- 根因链：GSP 提升全局时无「函数内是否修改该全局」跟踪 → 每个
  return/tail-call 前无条件回写（scalar_global_promotion.rs write_backs
  机制）→ 回写污染 read-only 判定 → 保守拒绝优化。

二、设计决策（候选方案）
- A. 独立 pass `dse.rs`，同 block 前向扫描 + GSP 死回写专项 —— 采纳。
  收益大头（GSP 死回写 + 相邻覆盖 + 同 block forwarding）局部扫描即可
  捕获；不引入完整 live-store 数据流（复杂度高、边际收益小）。
- B. 扩展 DCE —— 否。dce.rs 已因合并（removable_calls 参数 + 对方测试）
  复杂化，不再塞逻辑；DSE 需 EffectAnalysis + BaseEnv，独立 pass 更清晰。
- C. 完整反向 live-store 分析 —— 否，本期不做，A 兜不住的模式（跨
  block 死 store）记录为 M52.5 候选。

三、接口设计
- 新文件 `raana_ir/src/opt/passes/dse.rs`（约 380 行，含测试）。
- `pub struct DSE;`——`impl Pass`，`run_on(func)`：
  - `let analysis = EffectAnalysis::new(program)`（每次 run 重建，定点安全）
  - `let env = analysis.env_of(func)`（BaseEnv：base_of + constant_offset，
    已有指针槽解析 + block param phi，9d40dc3 后偏移解析完整）
  - 地址 key 用简化 `(MemObject, i64)`（base + byte offset），不提取
    ipsccp 的 CellKey/RootKey（私有 per-writer 结构，合并后刚稳定不动）。
- 三个变换（同一次 run_on 顺序执行，全部 within-block 前向扫描）：
  1. **GSP 冗余回写删除**：`store v, p` 且 v 是 `load p` 的结果（或经
     GEP/整数运算的 load 值）且 p 解析成功 → 检查 p 的 cell 在 store
     前无其他写、无可能读取（load/call/memzero 命中）→ 删 store。
  2. **覆盖死 store**：同 cell 连续 store，前一个在下一个之前无可能读
     取 → 删前一个。
  3. **store→load forwarding**：load 的 cell 前向最近 store 且中间无写
     无可能读取 → 替换 load 为 store 值（变量值转发，常量格已由 M53 覆盖）。
- 保守规则：地址解析失败（unknown base / 非常量偏移）→ 不删不改；
  遇到可能读该 cell 的 call → 停止该 cell 的前向扫描（effects 的
  call_read_roots / may_read_memory 判定）；MemZero 视为整区间写 + 读
  边界（区间命中即保守停止）。

四、伪代码（run_on 核心）
```
run_on(func):
  analysis = EffectAnalysis::new(program); env = analysis.env_of(func)
  for bb in func 的 blocks（layout 顺序）:
    cell_state = {}   // cell -> (最近 store inst 或 load 值)
    for inst in bb.insts():
      match kind:
        Store(s):  key = resolve(env, s.dest())       // (MemObject, i64)?
        | Some(k):
            // 变换1：冗余回写 —— value 是 load k 的结果且 k 无其他写
            if is_load_of(s.value(), k) and 无中间写:
              删除 s; continue
            // 变换2：覆盖死 store —— cell_state[k] 有未读 store
            if let Some(prev) = cell_state[k] as 未读 store:
              删除 prev
            cell_state[k] = s（未读标记）
        | None:
            possible_targets 非空 → 对每个 target cell 标记「已污染」
            （可能读路径不清除——保守：全部 cell_state 失效）
        Load(l):   key = resolve(env, l.src())
        | Some(k) 且 cell_state[k] 是未读 store:
            变换3：替换 l 为 store 值; 标记已读（可再转发）
        | 其他:    cell_state[k] 标记已读（不再删前驱 store）
        Call(c):  若 analysis 判定 c 可能读 cell_state 中某 k → 该 k 已读
                  若可能写 → cell_state[k] 失效（不清除 store 本身，停止删除）
        MemZero(z): 区间命中 → 相关 cell 已读/失效
```

五、文件与行数估计
- `raana_ir/src/opt/passes/dse.rs`：新建，~230 行实现 + ~150 行测试。
- `raana_ir/src/opt/pass.rs`：+2 行注册（`p.register(dse::DSE)`）。
- 注册位置：`gvn` 之后、`pointer_sr` 之前（删死 store 后 pointer_sr/sr
  看更干净 IR；BaseEnv 静态分析任意位置可用）。
- 不动 ipsccp.rs / dce.rs / memory.rs（resolve 逻辑用 base_of +
  constant_offset 组合，约 15 行私有 helper）。

六、验证计划
- 单测 ~7 个：
  1. GSP 模式冗余回写删除（load 值存回同址，中间无写）→ store 消失
  2. 冗余回写保留（中间有写或可能读 call）→ store 保留
  3. 相邻覆盖死 store 删除（store 1; store 2 同 cell → 删前者）
  4. 覆盖保留（中间有 load）→ 不删
  5. store→load forwarding（变量值）→ load 替换
  6. 未知地址 store（动态下标）→ 保守不删 + 前方 store 不误删
  7. MemZero 边界（区间命中 → 停止扫描）
- 端到端（--emit ir 检查）：
  - conv2d main 尾部 `store %x, %gv_repeat_factor` / `store %x, %gv_N_eff`
    消失（死回写删除）
  - heavy_read 类：函数尾 GSP 死回写消失 → effects 判 read-only →
    LICM call 外提成功（互评建议验证项）
- 全量门禁：cargo test 254+ 全绿；functional 109 + h_functional 40 ×
  -O0/1/2 全过（用户跑全量，本线跑单用例 + make test-riscv 单点）。
- 回归重点：IPSCCP 主存模拟与 DSE 交互（删 store 后格子减少，min2 用例
  必须仍折叠 6；04/05/88/62 四个修复用例必须仍 PASS）。

七、风险与对策
- 别名误判删活 store：resolve 失败一律保守；call/memzero 边界用 effects
  精确判定；最坏情况与 IPSCCP 一样只折叠「确定性」场景。
- GSP 回写删除误伤跨调用观察者：变换 1 只在「函数内 cell 无其他写 +
  无可能读」时删（GSP 语义：entry load 值 = 原值，函数内未改则回写
  恒等）。
- 定点交互：DSE 自身不引入新迭代轮（单遍 + 依赖 effects 不动点），
  pass 管理器迭代自然收敛。

**M52 执行记录（2026-08-04 完成）**
- 4 commit：DSE 骨架 + 注册（gvn 后、pointer_sr 前）→ 变换 1（GSP
  冗余回写删除）→ 变换 2（覆盖死 store 删除）→ 变换 3（store→load
  forwarding）+ decl 跳过 + resolve memoize（性能）。单测 7 个，
  261/261 全绿。
- 端到端：conv2d main 的 store 是真实初始化（非死回写，正确保留）；
  heavy_read 类 GSP 死回写删除 → read-only → LICM 外提 ✓；min2 仍折叠
  6 ✓；04/05/62/88 四个回归 -O2 全 PASS；functional 109 + h_functional
  40（默认）全过。
- **门禁（BaseEnv 重写后）**：cargo test 261/261；functional 默认
  109/109、-O2 109/109；h_functional 默认 40/40、-O2 38/40（唯一失败
  28_side_effect2，见下方遗留）。musl 编译器需 touch 源码强制重编译
  （Docker 挂载时间戳问题会让 harness 用旧编译器——23_json 曾因此
  599s 超时，重编译后秒级）。
- **性能优化（94d2498）**：BaseEnv 从「查询时现场递归解析」改为
  「build_tables 不动点预计算 base/offset 表 + 查询 O(1) 查表」
  （memory.rs；EffectAnalysis::new 里构建）。IPSCCP root_loaders push
  去重。23_json -O2 编译 9 分钟 → 0.078s（7000x）；28_side_effect2
  79s 超时 → 1.5s。
- **遗留（28_side_effect2 -O1/-O2 wrong answer：529 vs 701）**：
  禁用 IPSCCP 后输出 701 正确 → IPSCCP 引入。BaseEnv 重写前 28 编译
  >130s 从未跑完，529 大概率是 IPSCCP 既有折叠 bug（被加速暴露）：
  疑似主存模拟乱序（动态下标 store 的 clear 与 load 折叠乱序，与
  e15e676 修的 memzero 乱序同类）。简单内联+sum 用例（side_min=3、
  side2=20）均正确，仅 28 的 20 链 + 数组返回触发。**已修复
  （`[Fix(IPSCCP)] Never fold loads of roots invalidated by unknown
  writes`）**：根因 = flow-insensitive cell 被跨时点写覆盖——动态下标
  store（或 may-write-anything call）clear 后，root 的 cell 状态属于
  别的程序点，重调度的 load 会读到错误值。修复：MemState 加
  `cleared_roots`，unknown-write 的 clear（动态 store/memzero 动态
  dest/write_roots=None 的 call）永久禁止该 root 折叠 load（read 返回
  Bottom）；确定性 call 失效的 clear 不标记（callee 写入被跨函数模拟，
  可折叠）。28 -O2 输出 701 ✓，门禁 h_functional -O2 40/40。
- **遗留（heavy_read.sy 循环版 -O2 panic，已修复）**：DeadPhiElimination
  swap_remove 越界（jump args 与 block params 数量不一致，BB(2)
  params 3 / args 2）——无 DSE 注册也 panic，既有 bug 与 DSE 无关。
  **已修复**：cherry-pick 隔壁 `93fbb18 [Fix(DPE)] Keep dead-edge args
  aligned after IRH carrier insertion`（c0cedbb）——根因 = IRH
  （InvariantReductionHoisting）加 carrier 参数后只更新新边 args，原
  latch 死边 args 未补齐（args 12 vs params 13）→ DPE 按 used_by
  （含死边）删 args 越界。修复：IRH 补齐所有指向 header 的边 args
  （死边 push dummy）+ DPE swap_remove → positional remove（保序）。
- **遗留（23_json -O2 死循环，已修复 `[Fix(GVN)] Invalidate load
  leaders at loop headers`）**：qemu 跑 23_json.elf 死循环（编译 1s 但
  运行卡死；-O0 正确）。根因 = GVN 的 ScopedLoadLeaders 把循环入口的
  load（如 pos）CSE 成 preheader 值——循环体写该地址（store pos），
  回跳后仍用旧值（反汇编实锤：cmp w20 用循环前寄存器，str 递增写
  内存但条件不变）→ 死循环。IPSCCP 折叠后 IR 变化触发（禁用 IPSCCP
  时循环体重新 load，正确）。修复：GVN Enter 循环头块（RPO 中前驱
  位置更晚 = backedge）时 `load_leaders.record_store()` 使所有 load
  leader 失效（循环头/体内 load 重新读取）。23_json -O2 输出与 -O0
  一致 ✓，门禁 functional/h_functional × -O0/2 全过（109+40）。

**执行 goal 提示词（2026-08-04，给 future agent/compaction 的自包含任务）**

```
# Goal: 实现 M52 DSE（死 Store 消除 + 冗余回写删除 + store→load forwarding）

## 背景
SysY 编译器项目（Rust），工作目录
/Users/azureskye/Documents/Programs/rust/AnonBeijingCompiler-hermes，
分支 feat/riscv_match（当前 ahead 18，origin/main behind 7）。
详细设计见本文件 §4 的 M52 节（现象/候选方案/接口/伪代码/验证/风险），
严格按它执行，不自行更改设计。

## 必须遵守的规则（用户明令）
- 只在 hermes worktree 操作；不 push、不 rebase（遇冲突立即停并汇报）；
  不碰其他 worktree；不读/不提交 .env 等凭据文件。
- 无 hacky workaround（rm、手动 sed、flat pool 一律禁止），只做 root
  cause 修复。
- 勤 commit、勤单测：每个原子部分完成即 commit（消息格式
  `[Opt(DSE)]: <做什么>`），工作树尽量保持干净。
- 代码纪律：不改既有 pass 的基本结构（dce.rs/ipsccp.rs 刚合并稳定，
  除 pass.rs 注册外尽量不动）；新增代码匹配现有风格。

## 原子拆分（每步完成后 commit，再做下一步）
1. **骨架**：新建 raana_ir/src/opt/passes/dse.rs（`pub struct DSE;`
   impl Pass，run_on 空实现或最小扫描），私有 resolve helper（用
   memory.rs BaseEnv 的 base_of + constant_offset 组合成
   (MemObject, i64) key）；pass.rs 在 gvn 之后、pointer_sr 之前注册
   `p.register(dse::DSE)`；cargo build 通过。
2. **变换 1：GSP 冗余回写删除**（计划 §三.1）+ 单测（§六 的 1/2 两个
   用例）；load 值存回同址且中间无写 → 删 store。
3. **变换 2：覆盖死 store 删除**（§三.2）+ 单测（§六 的 3/4/6 三个
   用例）；同 cell 连续 store 前一个无读 → 删。
4. **变换 3：store→load forwarding**（§三.3）+ 单测（§六 的 5 用例）；
  变量值转发（常量格 M53 已覆盖，不重复做）。
5. **端到端验证**：--emit ir 检查 conv2d main 尾部
   `store %x, %gv_repeat_factor` / `store %x, %gv_N_eff` 死回写消失；
   构造 heavy_read 类用例验证 GSP 死回写删除后 LICM call 外提生效；
   4 个回归用例（04_arr_defn3 / 05_arr_defn4 / 62_percolation /
   88_many_params2）-O2 仍 PASS；min2 用例（/tmp/min2.sy 若还在，否则
   按 summary 重建）仍折叠 ret 6。有问题 root cause 修复后单独 commit。
6. **全量门禁**：cargo test -p raana_ir 全绿（现有 254 个 + 新增 DSE
   测试）；make test functional（109）、make test ARGS="-O 2"
   functional、make test h_functional（40）全过；h_functional 同样
   ARGS="-O 2" 跑一遍。**perf 测试用户自己跑，不要跑 perf**。

## 验证命令
- 单测：cargo test -p raana_ir dse:: 2>&1 | grep -E "test result"
- 全量单测：cargo test -p raana_ir 2>&1 | grep -E "test result"
- ARM 用例（Docker 内，慢）：
  make test functional
  make test ARGS="-O 2" functional
  make test h_functional
  make test ARGS="-O 2" h_functional
- IR 检查：./target/release/compiler --emit ir -o /tmp/x.raana -O2 <sy>

## 关键代码位置
- raana_ir/src/opt/passes/scalar_global_promotion.rs：write_backs 机制
  （每个 return/tail-call 前无条件回写，M52 目标）
- raana_ir/src/opt/analysis_passes/memory.rs：BaseEnv::base_of /
  constant_offset（含指针槽解析 + block param phi，9d40dc3 后完整）
- raana_ir/src/opt/analysis_passes/effects.rs：EffectAnalysis
  （call_read_roots / may_read_memory / is_removable 等判定）
- raana_ir/src/opt/passes/ipsccp.rs:331 resolve_cell：参考实现（不提取，
  DSE 用私有简化 key）
- raana_ir/src/opt/pass.rs:209 gvn 注册处：DSE 插在 gvn 与 pointer_sr 之间

## 验收清单（全部满足才算完成）
- [ ] dse.rs 单测 ≥7 个全绿，cargo test 全绿
- [ ] conv2d 死回写消失（--emit ir 验证）
- [ ] functional 109 + h_functional 40 × -O0/1/2 全过（用户跑 perf）
- [ ] 4 个回归用例 -O2 PASS（04/05/62/88）
- [ ] min2 折叠 ret 6
- [ ] 每个原子部分有独立 commit，最终 git status 只剩非本任务文件
      （TODO.md 如有外部会话改动保持不动）
```

#### M53：IPSCCP 主存模拟（已完成）

- 交付：常量 GEP 偏移的 per-cell 格（per-writer 贡献 + meet 折叠，src
  精化可恢复）；MemZero 合并零区间、零初始化全局播种零区间；call 按
  M50 摘要精确失效（未知写全清）；cell 变更重调度 load（与 relay 同
  模式 drain）。验收：单测 6 个 + 端到端（`g[0]=5; ret g[0]` → `ret 5`、
  `int a[4]={0}; ret a[1]` → `ret 0`）。

#### M54：LICM load 外提（已完成）

- 交付：can_be_invariant 增加 Load + 别名/mod-ref 守卫（循环内
  store/MemZero/call 可能写地址则拒绝；无分析时保守不外提）；LICM
  增加 analysis 字段（每次 Pass::run 重建，定点安全）。验收：单测 5 个
  + conv2d 形参指针槽 load 全部提出热循环（汇编确认）。

#### M55：后端调度别名细化（P2，评估先行，未做）

- 现状缺口：sched/dag.rs 已有 root 模型与统计（dag.rs:144-179），
  IR 层 GEP 仿射信息到不了 MIR；§5.3 记录内存历史 O(M²) 复杂度。
- 方案：先用现有统计量化 known/unknown root 与 disjoint/may-alias
  占比——收益有限则记录结论关闭；may-alias 占主导才把 M49 结果随
  MInst 下沉，与内存历史数据结构优化合并实施。

#### M56（可选）：数组全局部分提升 / 循环不变基址 hoist（未做）

- GSP 泛化：数组全局函数内只经常量 GEP 访问且跨调用只读（M50 证明）
  → 元素提升为 SSA；地址层与 pointer_strength_reduction 的 A4c 呼应。

### 成本-收益评估（每 milestone 必做，详见设计文档 §7）

- 复杂度：M49 O(N×D)、M50 O(F+E)×迭代、M51/M52 O(N~N log N)、
  M53 O(N×Σcell)（唯一可能拖慢定点的项）、M55 不增复杂度。
- 编译耗时：perf corpus 全量编译时间 before/after 中位数对比；定点
  迭代轮数监控（M53 重点）。
- 收益：热循环静态指令数（awk 方法）、gem5 sim_insts/qemu wall
  （仅相对参考）、sched DAG stats（disjoint vs may-alias 占比）。
- 门禁：functional/h_functional 全量 + perf on/off 差分 + 双 target
  × -O0/1/2 5 次 byte-identical。
- 预期收益排序：M51 > M52 > M53 > M54 > M55（按成本/收益比）。

### 合规性

- 别名/副作用/可达性属 AGENTS.md 明示合法优化依据（"effects, alias
  information"）；全部变换基于 IR 结构推导，不匹配名称/用例/输入特征。
- 红线：不得因"未初始化局部值不确定"删除前端 mem_zero，除非证明
  每读路径都被写覆盖（按可观察语义对待，比 UB 更严格）。
- 保守原则：判不了就是 may-alias，宁漏勿错（§6 执行原则 4）。

## 5. 后续候选工作

按预期收益排序，均需在前一项验证后再启动。被主计划 A 覆盖的旧条目
（`&&`/`||` flags 融合、phi 拷贝 coalescing、循环不变 load 外提）已并入
M31-M35，不再单列。

### 5.1 P1：常量 / 分支参数物化源头治理

DCE 是兜底；更优解是 lowering 时就不为未使用的 block param 和分支参数物化
常量与 `mov wzr`。DCE 落地后统计 `instructions_removed` 的构成，若某类来源
占主导，直接在 `taki_mir/src/lower.rs` 或 `anon_armv8/src/lower.rs` 消除源头。

### 5.2 P2：调度验证器闭环

- `verify_operand_order_stable`：保护 pre-RA pass 的 operand traversal
  contract。
- `verify_sched_deps`：独立于 scheduler 重放调度后的 register、NZCV、
  memory 和 barrier 约束。
- 小 DAG reference simulator / property tests。

### 5.3 P2：内存 DAG 复杂度

长块最坏 `O(M^2)`。已有统计，先采集编译时间数据，确认是实际问题后再引入
按 root/range 分组的数据结构。

### 5.4 P2：调度启发式增强（需实机数据证明收益）

- Load-use latency hiding 专项。
- Pair-aware scheduling（调度时考虑 LDP/STP 形成）。
- post-RA register-pressure tie-break。
- pre-RA scheduler（需先证明 post-RA false dependency 是主要 ILP 限制）。

### 5.5 P2：冷块沉底与布局

Cranelift `BlockLoweringOrder` 的 `cold_blocks` 机制（`blockorder.rs:87-90,
260-265`）把冷块沉到函数末尾；配合 M25-M29 的 EmitBuffer，冷块天然获得
fallthrough 收益。SysY 前端暂无冷热信息，本期仅在 `BlockLoweringOrder`
预留 `is_cold()` 接口。相关遗留：`CmpImm(0)+CondBr{Ne}`→`cbz` 融合未做。

### 5.6 P3：XCZU15EG 实机校准（依赖硬件访问）

- 运行 `benchmarks/src/bench.c`，校准 latency / throughput / pairing 数据。
- 基于实测调整 guide-derived profile 值。
- 建立性能回归门禁。
- 回答：WAR/WAW/NZCV false dependency 是否允许 A53 同周期双发。
- 用 M19-M24 的参数入口 microbenchmark 与 M25-M29 的 huffman 差分量化
  实际收益（实机数字待测）。

### 5.7 M35 遗留：回边 blockparam 拷贝消除（候选）

ion `merge_vreg_bundles` 的 blockparam-out 合并已触发且正确；`_and/_xor/_or`
回边 3 条 `mov w,w` 是语义必需（旧值读在旋转后新值定义之后，活区间真实相交）。
消除路径：

- (a) 循环体重排——把旧值读取（bit 计算）提到新值定义之前（IR/MIR 层，可使
  回边零 mov）；
- (b) ion 活区间按块参数 in/out 拷贝分裂（regalloc2 half-move 语义）。

验收：`_and` 循环回边零 mov。

### 5.8 分支发射遗留（主计划 B 完成后的剩余项）

- RISC-V `CondBr` 冷块沉底未做（只在 `BlockLoweringOrder` 预留 `is_cold()`
  接口）。
- `CmpImm(0)+CondBr{Ne}`→`cbz` 融合。

### 5.9 RISC-V 栈参数 FPGA 实机复跑（附 A 收尾）

修复已实现（非对齐 131→0 处），**FPGA 实机复跑仍待验证**：`h_functional/
39_fp_params` 在 BOOM 实机复跑确认 WA/RE 消除。

### 5.10 RISC-V 跑分候选（附 B 收尾，按通用性 × 收益 × 风险）

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

### 5.11 泛化：整体循环巢外提（容忍幂等写）（M48 后续）

M48 的"零 store"守卫拒绝了 conv2d `repeat` 外层（巢内写 `Out`，但每轮写入
相同位置、相同值，即**幂等写**，可安全外提）。泛化方向：识别"循环巢整体相对
外层不变 + 写集幂等"后整体外提，节省 `repeat_factor` 倍工作量。判定较复杂
（需幂等写证明），列为后续，本期不做。

---

## 6. 总体执行原则

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

## 7. 风险与缓解

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
待验证（§5.9）**。

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

候选行动项（P0-P3）已并入 §5.10。

## 6. 工作区未提交改动记录（2026-08-04，stash 存档）

### 6.1 ipsccp.rs 未提交改动（已 stash，归属：内存分析线遗留/待确认）

`git stash push -m "ipsccp: load snapshot overwrite + mem_zero store-order fix" -- raana_ir/src/opt/passes/ipsccp.rs`
恢复：`git stash apply stash@{0}`（确认归属后再决定合并/丢弃）。

改动内容（120 行，+91/-29，基于 191cd71）：

1. **load 传播改覆盖**（新增 LatticeMap::insert_or_replace，~20 行）：原 merge_and_extend
   把历史快照与当前快照做 meet——同一 load 先被调度读到 0（store 前）、重调度后读到 6
   （store 后）会塌缩成 Bottom，常量传播失效。改为无条件覆盖为当前内存快照值：load 的
   语义是"某次调度时刻的内存快照"，不是所有快照的交。调用点（~510 行）改
   insert_or_replace 成功时照常 extend_affected_node_used_by。
2. **mem_zero 不再 drop 已写 cell**（~15 行）：原实现清除 zero 区间内所有已写 cell；
   但 worklist 不按 layout 顺序处理块内指令，MemZero 可能在初始化序列的 stores 之后才
   被访问，清除会误杀语义上更晚的 store。约定：frontend 总是先发 MemZero 再发 stores，
   所以"已存在"的 cell 必对应语义更晚的 store；zero range 只应答未被 store 写过的 cell。
3. 测试修正：call_to_writer_invalidates_global_cell 的 writer 增加 i32 参数（注入未知值）。

性质：IPSCCP 主存模拟（M53）正确性补丁。与本线执行计划（LICM 缺口 A + pointer_sr 缺口 B）
无关，执行 goal 时**不碰、不恢复**，除非用户另行指示。

### 6.2 执行计划分支

- 新分支：`feat/addr-incr-runtime-bound`（自 191cd71 开出）。
- 计划全文见主 worktree TODO.md commit e2bfba1（§"执行计划：运行时边界循环的地址增量优化"）
  及本会话修正：修复 A 判定须要求"所有 incoming ∈ {单一外部值 V, param 自身} 且至少一个
  非自身"（计划主表述"全部非循环内定义"不充分：entry/backedge 传不同外部值会在第 2 轮
  变值）；修复 B 现有测试翻转面仅 2 个（1271 行 dynamic 半段、1282 行整体，均改写为正测试）。
- TODO.md 本节改动未提交，goal 开工时可先 commit 为 docs commit。

---

**M52 执行记录（2026-08-04 完成）**
- 4 commit：DSE 骨架 + 注册（gvn 后、pointer_sr 前）→ 变换 1（GSP
  冗余回写删除）→ 变换 2（覆盖死 store 删除）→ 变换 3（store→load
  forwarding）+ decl 跳过 + resolve memoize（性能）。单测 7 个，
  261/261 全绿。
- 端到端：conv2d main 的 store 是真实初始化（非死回写，正确保留）；
  heavy_read 类 GSP 死回写删除 → read-only → LICM 外提 ✓；min2 仍折叠
  6 ✓；04/05/62/88 四个回归 -O2 全 PASS；functional 109 + h_functional
  40（默认）全过。
- **门禁（BaseEnv 重写后）**：cargo test 261/261；functional 默认
  109/109、-O2 109/109；h_functional 默认 40/40、-O2 38/40（唯一失败
  28_side_effect2，见下方遗留）。musl 编译器需 touch 源码强制重编译
  （Docker 挂载时间戳问题会让 harness 用旧编译器——23_json 曾因此
  599s 超时，重编译后秒级）。
- **性能优化（94d2498）**：BaseEnv 从「查询时现场递归解析」改为
  「build_tables 不动点预计算 base/offset 表 + 查询 O(1) 查表」
  （memory.rs；EffectAnalysis::new 里构建）。IPSCCP root_loaders push
  去重。23_json -O2 编译 9 分钟 → 0.078s（7000x）；28_side_effect2
  79s 超时 → 1.5s。
- **遗留（28_side_effect2 -O1/-O2 wrong answer：529 vs 701）**：
  禁用 IPSCCP 后输出 701 正确 → IPSCCP 引入。BaseEnv 重写前 28 编译
  >130s 从未跑完，529 大概率是 IPSCCP 既有折叠 bug（被加速暴露）：
  疑似主存模拟乱序（动态下标 store 的 clear 与 load 折叠乱序，与
  e15e676 修的 memzero 乱序同类）。简单内联+sum 用例（side_min=3、
  side2=20）均正确，仅 28 的 20 链 + 数组返回触发。**已修复
  （`[Fix(IPSCCP)] Never fold loads of roots invalidated by unknown
  writes`）**：根因 = flow-insensitive cell 被跨时点写覆盖——动态下标
  store（或 may-write-anything call）clear 后，root 的 cell 状态属于
  别的程序点，重调度的 load 会读到错误值。修复：MemState 加
  `cleared_roots`，unknown-write 的 clear（动态 store/memzero 动态
  dest/write_roots=None 的 call）永久禁止该 root 折叠 load（read 返回
  Bottom）；确定性 call 失效的 clear 不标记（callee 写入被跨函数模拟，
  可折叠）。28 -O2 输出 701 ✓，门禁 h_functional -O2 40/40。
- **遗留（heavy_read.sy 循环版 -O2 panic，已修复）**：DeadPhiElimination
  swap_remove 越界（jump args 与 block params 数量不一致，BB(2)
  params 3 / args 2）——无 DSE 注册也 panic，既有 bug 与 DSE 无关。
  **已修复**：cherry-pick 隔壁 `93fbb18 [Fix(DPE)] Keep dead-edge args
  aligned after IRH carrier insertion`（c0cedbb）——根因 = IRH
  （InvariantReductionHoisting）加 carrier 参数后只更新新边 args，原
  latch 死边 args 未补齐（args 12 vs params 13）→ DPE 按 used_by
  （含死边）删 args 越界。修复：IRH 补齐所有指向 header 的边 args
  （死边 push dummy）+ DPE swap_remove → positional remove（保序）。
- **遗留（23_json -O2 死循环，已修复 `[Fix(GVN)] Invalidate load
  leaders at loop headers`）**：qemu 跑 23_json.elf 死循环（编译 1s 但
  运行卡死；-O0 正确）。根因 = GVN 的 ScopedLoadLeaders 把循环入口的
  load（如 pos）CSE 成 preheader 值——循环体写该地址（store pos），
  回跳后仍用旧值（反汇编实锤：cmp w20 用循环前寄存器，str 递增写
  内存但条件不变）→ 死循环。IPSCCP 折叠后 IR 变化触发（禁用 IPSCCP
  时循环体重新 load，正确）。修复：GVN Enter 循环头块（RPO 中前驱
  位置更晚 = backedge）时 `load_leaders.record_store()` 使所有 load
  leader 失效（循环头/体内 load 重新读取）。23_json -O2 输出与 -O0
  一致 ✓，门禁 functional/h_functional × -O0/2 全过（109+40）。

**执行 goal 提示词（2026-08-04，给 future agent/compaction 的自包含任务）**

```
# Goal: 实现 M52 DSE（死 Store 消除 + 冗余回写删除 + store→load forwarding）

## 7. 性能差距分析：vs clang -O2（2026-08-04）

方法：clang 17（--target=aarch64-none-elf -O2，sed 把 SysY `const int N` 预处理成 `#define N`）与本编译器 -O2 同用例对比汇编指令数。clang 是强基线（含 NEON/内联/循环优化），差距按可实现性分级。DSE（M52）归隔壁会话，本清单避开。

### 7.1 指令数对比（文件级，grep 指令行数）

| 用例 | clang -O2 | 本编译器 -O2 | 差距 |
|---|---|---|---|
| conv2d-1 | 586 | 542 | -8%（持平） |
| 03_sort1 | 404 | 454 | +12% |
| sl1 | 169 | 212 | +25% |
| crypto-1 | 468 | 836 | +79% |
| huffman-01 | 514 | 890 | +73% |
| fft1 | 321 | 684 | +113% |

conv2d-1 已优于 clang（行指针提前 + LDR 融合 + madd 的功劳）。差距集中在 call 密集/位运算类用例。

### 7.2 差距 A（修正版）：真实差距 = 死函数不删除 + 循环内调用点内联（P1/P2）

初版结论"Inline 是 once 单候选导致 40 call 未内联"——**错误**。调试（call_graph 边打印 + incoming_callsites_of 数据 + 最终 IR call 归属）证明：

1. **Inline 的 run 已是内部自循环**（inline.rs:19-25 `while Self::once(program)`），会一直内联到无候选，不是 once 单候选。
2. **crypto-1 的 pseudo_md5 14 个调用点全部内联**（最终 IR 中 pseudo_md5 0 个 call）。get_random、_and/_or/_not/_xor 等也被内联。Inline 机制正常。
3. **crypto-1 汇编 40 个 bl 的真相**：18 个在死函数 pseudo_sha1 里（main 从不调用它，源码 158 行定义后无调用者），6 个在 main（getint×2/starttime/stoptime/putarray/_xor×1）。**死函数 pseudo_sha1 没被 DCE 删除**（570 行汇编死代码），其内部 rotl1/rotl5/rotl30/_and/_xor 调用全保留 → 指令数统计虚高。
4. 活代码里真实的 call 开销：main 的 _xor（循环外 1 次，size×callsites 超限不内联，影响极小）。

真实差距（修正后）：
- A1（P1）：**死函数删除**——DCE 只删死指令不删无调用者的函数。pseudo_sha1 类（crypto-1 570 行、fft1 可能也有）在最终汇编保留。实现：DCE 里用 call_graph（main 可达）或调用点计数删除无调用者且非 main 的函数。注意与隔壁 DSE（store 消除）不冲突。收益：代码体积 + 编译时间（汇编/链接/缓存），运行性能无直接影响（死代码不执行）。
- A2（P2）：**循环内调用点内联**——huffman 的 read_bits_specialized_2（每符号调用、含循环、size > CALL_SIZE_LIMIT=40）不内联，是活代码里真实的每符号 call 开销（+序言，它内部还调 rotlN）。read_bits 是循环函数，size 超限被拒。候选：find_candidate 判断调用点所在块是否在自然循环内，循环内调用点放宽 CALL_SIZE_LIMIT（动态收益 ×迭代次数）。fft1 的 multiply/memmove 同属此类。
- fft1 递归调用（bl fft×2）无法内联（recursion 检查），结构性保留。

验证记录：调试手段 = call_graph.rs 边打印 + inline.rs main callees/布局 call 数打印 + incoming_callsites_of entry 打印 + 最终 IR call 行号归属（pseudo_md5 154-599 / pseudo_sha1 599-815 / main 815+）。调试代码已全部移除（git diff 干净）。

### 7.3 差距 B（已撤销）：叶子寄存器保存——实测不成立

初判 read_bits_specialized_2 是"叶子函数却保存 x19-x22"——错误。复查汇编它内部有 1 个 call（bl rotlN_specialized_0），是非叶子，保存 ra/x19-x22 正确。crypto-1 的 rotl1（真叶子，5 条指令无 call）零序言、用 caller-saved w2/w4/w6/w8——分配器对叶子函数已正确处理。ra 保存也已有（taki_mir/abi.rs:429 has_calls，lowering 扫描 call 指令设置，控制 RISC-V ra / AArch64 x30 保存与 outgoing area）。结论：后端"是否需要保存寄存器"的机制已完备，无需新工作。read_bits 的开销（每符号 bl + 序言/尾声 ~14 条）本质是内联不足（差距 A），不是寄存器分配。

### 7.4 差距 C：范围检查未折叠（huffman，P2，IR 层）

现象：clang 把 decode_fixed_huffman 的 `(c+64)>=65 && (c+64)<=144` 折叠成 `sub w8, w0, #1; cmp w8, #79; b.ls`（单无符号比较，利用 i32 回绕）。本编译器生成 land_merge + ccmp + b.le + 双分支块。

候选方案：IR 层变换 `x >= L && x <= U`（有符号）→ 无符号单比较 `sub; cmp; ls`（等价变换：x-L 无符号 ≤ U-L，L/U 为常量时）。放 boolean_simplify 或 StrengthReduction。需证明 i32 回绕下等价（x < L 时 x-L 无符号回绕为巨大值 → 自然排除，合法）。验证：huffman decode 循环单分支；构造边界用例单测。

### 7.5 差距 D：位反转循环用分支（fft1，P2）

现象：fft 位反转循环（L_fft_while_body_5）用 tbnz w0,#0 + then/else 双块；clang 用 ubfx/csel 无分支（LBB3_4）。if-conversion 未覆盖该形态（位反转表达式 `(i&1) ? i>>1 : n/2 + i>>1` 之类）。

候选方案：if-conversion 扩展或位运算规范化（`(i&1)!=0` 条件 → csel）。先确认 fft1 热循环占比再定优先级（fft 是递归，位反转循环可能占比小）。

### 7.6 差距 E：清零循环未识别为 MemZero/NEON（01_mm2 类，P2）

现象：clang 把 `C[i][j]=0` 双层清零循环识别为 memset（NEON 优化 libc）。本编译器保留 store 循环（IR 是 gep+store 循环，不是 MemZero 指令）。01_mm2/01_mm3 等含初始化清零段。

候选方案：循环模式识别（内层 store 常量 0、全行连续）→ 生成 MemZero 或直接 NEON stp 序列。注意与隔壁 DSE 的接口（DSE 也动 store）。验证：清零段汇编变 stp 批量；01_mm2 定向。

### 7.7 结构性差距（不可追/搁置）

- NEON 向量化：clang 对 01_mm1/01_mm2 主循环用 NEON + 运行时别名 versioning + memset。M42-M46 搁置中，需 SIMD 通路。
- clang 的循环展开/多版本化（LBB 结构复杂化）——通用 unroll 分支（feat/loop-unroll）有未完成工作（rebase 编译失败），本线不重复。

### 7.8 优先级建议

P1：A1（死函数删除）已落地（commit a36776c，crypto-1 836→453 指令）。**A2（循环内调用点内联）是当前主线**——详见 §8（huffman read_bits/fft1 multiply 实测数据、候选方案、验证计划）。B 已撤销（见 7.3）。
P2：C（范围检查折叠）、E（清零识别）——独立小变换。
P3：D（位反转无分支）——先量化 fft1 热循环占比。
验证基线：本清单全部用 clang -O2 同用例对比 + cargo test + make test 定向；全量由用户跑。合规：全部按 IR 结构触发，无名字/输入指纹。

## 8. A2：循环内调用点内联（P2，2026-08-04 侦察）

### 8.1 现状（A1 DFE commit a36776c 之后）

- Inline 机制已确认完备：run() 内部自循环（inline.rs:19-25 `while Self::once`）；find_candidate 每次重建 call_graph（正确）；recursion 检查用 `call_graph.reaches(callee, callee)` 拒递归环（fft 的 bl fft×2 因此保留，结构性正确）；BodyClonePlan::capture 预检（含循环的函数体克隆合法——循环嵌套克隆已验证）。
- cost model（inline.rs:132-135）：`size > CALL_SIZE_LIMIT(40) || size × callsites > TOTAL_SIZE_LIMIT(100)` → 拒。
- estimate_size（inline.rs:30-36）：纯布局 inst 计数（不含序言/参数路由成本）。
- LoopAnalysis 现成可用：`LoopAnalysis::min_loop_contain(block) -> Option<&Loop>`（loop_analysis.rs:65），`LoopAnalysis::new(&FunctionData) -> (CFG, DominanceTree, LoopAnalysis)`（71 行）。

### 8.2 实测数据（-O2，inline-dbg 打印 + IR 定位）

| callee | size | callsites | 拒绝原因 | 调用点位置 |
|---|---|---|---|---|
| read_bits_specialized_0/1/2（huffman） | 141 | 1-2 | CALL_SIZE_LIMIT（远超 40） | main 的 decode 循环体 `while_body_6_decode_fixed_huffman_inline_7`，每符号 1 次（IR 118 行） |
| rotlN_specialized_0（huffman） | 34 | 3 | TOTAL_SIZE_LIMIT（34×3=102 > 100，差 2） | read_bits 内部（IR 317/534/748 行） |
| fft_specialized_0（fft1） | 97 | 3 | CALL_SIZE_LIMIT | 蝶形循环所在函数（其内部 169/177 行调 multiply） |
| multiply（fft1） | ~20（未实测） | 6 | **非 cost 拒绝（cost 检查未打印，拒绝点待定位：callsite 选择/类型检查/BodyClonePlan？）** | power 内 2（47/71/76 tail）、fft 递归前 1（128）、fft_specialized 蝶形循环 2（169/177） |
| memmove（fft1） | - | - | 已内联（fft 里块名 while_entry_2_memmove_inline_7） | **不是差距项**（此前 TODO 7.4 误记） |

关键事实：
- read_bits 是**非叶子**（内部调 rotlN_specialized_0，IR 748 行）——内联 read_bits 会把 rotlN 调用带进 main，后续轮次链式内联 rotlN（rotlN 34×3=102 也只差 TOTAL 边界 2）。
- decode_fixed_huffman 已内联进 main（块名后缀 _inline_4），read_bits 调用点因此直接位于 main 的循环里。
- fft1 的热点结构：fft 递归（保留）→ 每层递归前 multiply(θ,θ)（128 行，非循环内）+ fft_specialized 蝶形循环内 multiply（169/177 行，循环内）。memmove 复制循环已内联。
- clang 对照：read_bits 被 clang 内联（decode 循环无 call），huffman 差距 +73% 的 call 开销部分在此；fft1 +113% 含 multiply 调用 + 位反转分支（7.6 的 P3-D，独立项）。

### 8.3 根因链

1. cost model 纯静态（size × callsites），无"调用点在自然循环内 → 动态收益 × 迭代次数"信号。
2. read_bits 类循环函数（size 141）单调用点也被 CALL_SIZE_LIMIT=40 一刀切拒——恰是动态收益最大的场景（decode 循环迭代 N 次 × call 开销 + 非叶子序言 ~14 条）。
3. rotlN 类（34×3=102）卡 TOTAL_SIZE_LIMIT 边界 2 个点——其调用点在 read_bits 内，read_bits 内联后随迁入 main，链式内联被同一静态限制挡住。
4. multiply 拒绝点未定位（非 cost）——实施第一步先精确定位（候选：136-140 行 callsite 查找、152-166 类型/unit 检查、168-173 BodyClonePlan/contains_tail_call）。

### 8.4 候选方案（建议先做 1，数据校准后定倍数）

1. **循环内调用点信号放宽**（核心）：
   - 对候选调用点构建 LoopAnalysis（`LoopAnalysis::new(caller_data)`，只对过 cost 门槛前的候选者构建，find_candidate 每轮的开销可接受），`min_loop_contain(call_block).is_some()` = 循环内。
   - 任一 callsite 在循环内 → 放宽：建议初值 CALL_SIZE_LIMIT_LOOP=120、TOTAL_SIZE_LIMIT_LOOP=200（read_bits 141 仍差 21——**141 单调用点 + 循环内可特殊放行**（单调用点膨胀一次性，动态收益大），或 CALL_SIZE_LIMIT_LOOP=150）。**实施时用真实数据校准，先保守后放宽**。
   - 放宽仅作用于 cost 检查（132-135 行）；recursion/BodyClonePlan/类型检查不动。
2. **链式内联自然达成**：read_bits 内联 → rotlN 调用点进 main 的 decode 循环 → 下一轮 find_candidate 对 rotlN 用循环内放宽 → 内联。无需额外机制。
3. **内联后 DFE 联动已就绪**：read_bits/rotlN 内联后变死 → fixed point 的 DFE 删除（a36776c）。
4. multiply 拒绝点定位后单独处理（若 BodyClonePlan 拒绝则先解决克隆前置）。

### 8.5 风险与边界

- 代码膨胀：read_bits 141 inst 一次性进 main（1 个调用点）；fft_specialized 97 inst 进调用者（3 个调用点，需看是否值得——若 3 个调用点都在递归路径上，动态收益存疑，可能只放宽单调用点+循环内）。**几何均值风险**：逐 perf 用例检查汇编膨胀（01_mm2 的 memset 类清零循环、其他用例的循环内大函数）。
- 寄存器压力：内联后调用者活跃值增加——A53 31 寄存器，风险低但需看 hot loop spill（make mca 复查）。
- 迭代次数未知：循环内布尔信号是近似（无法静态估计 N）——放宽倍数保守起步，避免全程序膨胀。
- 递归边界不变：fft 保留；fft_specialized 若内部含 fft 调用（recursion 检查已覆盖）安全。
- 合规：循环内调用点信号是通用 IR 结构触发，无函数名/用例指纹。

### 8.6 验证计划

1. 单测（inline.rs tests）：main 循环内调用大 callee → 断言内联；同 callee 循环外调用 → 不内联（保守保持）；rotlN 式（多调用点+循环内）→ 放宽后内联。
2. huffman 定向：read_bits_specialized_2 内联（decode 循环无 bl read_bits）、rotlN 链式内联（无 bl rotlN）、bl 数 33 → 对比 clang、make test huffman QEMU 正确性。
3. fft1 定向：multiply 内联（蝶形循环无 bl multiply）、bl fft×2 保留、make test fft1 QEMU。
4. 全量 perf 用例汇编 bl 数/指令数扫描（膨胀风险）+ workspace 测试。
5. 性能：QEMU 时间对比 huffman/fft1 前后；几何均值风险自查（其他用例无膨胀）。
6. 全量 make test / make test-riscv 由用户跑。

### 8.7 实施结果（2026-08-04 完成，commit 见 git log）

**multiply 拒绝点定位（§8.2 遗留问题，结论）**：`recursion-cycle` 守卫（inline.rs:120，cost 检查之前）。multiply 是真递归（fft1.sy:7 `multiply(a, b/2)`）——自递归 callee 克隆后其自调用点移入调用者，reaches 守卫不再排除，会无限链式内联，守卫必要（cranelift `does_not_inline_across_a_recursive_call_cycle`）。**"fft1 multiply 消失"在 Inline 层不可达**（clang 同样无法内联递归函数）；这不是 cost 缺口，不改守卫。另：fft_specialized_0 的 3 个调用点全部在 main 顶层（不在循环内，蝶形循环在其函数体内），循环内信号对它不适用，且 3 次程序级调用内联收益≈0，当前 cost 拒绝是对的。fft1 代码生成零变化（562 = HEAD 562，见下）。

**实现**：`CALL_SIZE_LIMIT_LOOP=200` / `TOTAL_SIZE_LIMIT_LOOP=300`；`any_callsite_in_loop()`（inline.rs，按 caller 去重建 `LoopAnalysis`，任一 callsite 的 call_block 满足 `min_loop_contain().is_some()` → 放宽）。仅放宽 cost 检查；recursion/BodyClonePlan/类型检查不动。单测 4 新增（循环内大 callee 内联 / 循环外不内联 / 多调用点+循环内放宽 / 循环外多调用点保守）。

**数据校准（初值 150/200 → 实测 200/300，偏差说明）**：rotlN_specialized_0 的调用点在 read_bits **内部循环**里（不是 TODO §8.4 预测的"read_bits 进 main 后再链式"路径），先于 read_bits 被放宽内联进 read_bits 本体 → read_bits_specialized_1/2 从 141 长到 **175**（141+34）→ 175 > 150 被拒，链式增长卡在 CALL 上限。CALL_SIZE_LIMIT_LOOP 提到 200 后全链打通。TOTAL_SIZE_LIMIT_LOOP=300 覆盖 read_bits_specialized_0 的 141×2=282。

**huffman 结果（-O2 aarch64）**：最终 IR 只剩 `main`——read_bits_specialized_0/1/2、rotlN_specialized_0、decode_fixed_huffman、output_data 全部内联，随后 DFE 清掉死函数。汇编 460 指令（HEAD 550，clang 514）——§7.1 的 +73% 差距反超为 **-10%**。decode 循环无 bl。QEMU -O1 PASS。

**fft1 结果**：562 = HEAD 562，零变化（无循环内调用点 + multiply 递归结构），bl fft×5/bl multiply×19 保留（递归正确性）。

**全量 perf 汇编扫描（HEAD a36776c vs NEW，指令数）**：
- 收窄：huffman 550→460、crc 137→125（crc32_specialized_0 70×1 循环内）、shuffle 193→173（insert 75×1）、h-10 170→164（trsm_optimized 51×1）。
- 膨胀（均为热循环内核，静态成本远小于 A53 L1I，无几何均值风险）：01_mm 191→235（mm 56×2 内联，`while(i<5){mm×2}` 共 10 次调用省 call 开销）；03_sort 411→527（getNumPos 8-inst×14 调用点全内联，基数排序热循环数百万次调用，明确大赢）。
- 其余 30+ 用例零变化。
- **many_mat_cal-1/2/3 编译 panic**（DeadPhiElimination swap_remove 越界）：基线 HEAD worktree 实测同样 panic——既有 bug，非本改动引入（另见记忆：heavy_read 类 dce swap_remove 越界）。

**验证**：inline.rs 8 单测（4 新+4 旧）、raana_ir 264 全量、`cargo test --offline --locked --workspace` 全绿、make test perf/huffman-01.sy + perf/fft1.sy（ARGS="-O 1 -j 1"）QEMU 均 PASS。

**遗留**：~~many_mat_cal DPE 越界（既有，独立修）~~ **已修复（见 §8.8）**；fft1 multiply 若要消除需递归→循环迭代化变换（非 Inline 范畴，clang 也未做）；fft_specialized_0 顶层调用点保持拒绝（收益≈0 判断正确）。

### 8.8 many_mat_cal DeadPhiElimination panic 根因与修复（2026-08-04）

**现象**：many_mat_cal-1/2/3 在 -O2 编译期 panic `swap_remove index (is 12) should be < len (is 12)`，栈顶为 DeadPhiElimination::run_on。基线 HEAD（a36776c）同样 panic——既有 bug，但并非"未知遗留"：是 IRH 与 DPE 的不变量耦合问题。

**根因链**（用临时诊断定位：pass manager 每个 pass 后插 args/params 对齐检查，锁定 iter=0 pass=8 首次失配）：
1. InvariantReductionHoisting（IRH，many_mat_cal 热点的退化优化，pass.rs 注释明示）改造外层循环：给 header（while_entry_49）`add_param(carrier)` 追加 D_total 载体参数（12→13）。
2. IRH 只同步了**新边**的 args（compute_done、degraded_latch 各 push 1 个）；**原 latch（while_end_54）的 `jump header` 边没动**——header 的 branch 已改指 degraded_latch，原 latch 成死代码，但其 terminator 仍指向 header，`used_by` 反向链接保留（DCE 只删指令不删块，且 jump 的 target 可达所以 jump 本身也保留）。
3. 死边 args 12 vs header params 13 → IR 不变量（所有 incoming args 长度 == params 长度）被破坏。DPE 遍历 `header.used_by()`（不区分可达性）按 13 个 params 的 index 删 args → swap_remove(12) 越界。
4. 为什么之前没炸：DPE 是 fixed point 里唯一按 used_by 遍历所有边（含死边）并按位置删 args 的 pass；之前的用例没有"改造后遗留死边"的形态。

**修复**（两层）：
1. **IRH 根因**（invariant_reduction_hoisting.rs apply()）：加 carrier 后遍历 `header.used_by()`，所有仍指向 header 的 terminator（jump/branch 对应 arm）把 args 补齐到 params 长度——死边 push dummy i32（不可执行，值无意义）。used_by 反向链接不区分可达性，所有消费方都会看到死边。
2. **DPE 加固**（dce.rs）：`swap_remove` → `remove`。unused index 序列是逆序，positional `remove` 保序（swap_remove 会把末尾元素换到删位导致 params/args 相对顺序乱掉；虽与 args 同步乱序，但多轮 fixed point 下任何一方被其他 pass 单独触碰就错位）。
3. **回归断言**（IRH 测试 `degrades_an_invariant_outer_reduction_nest`）：改造后所有指向 header 的边 args == params 长度（含死边），直接覆盖本次 bug 场景。

**验证**：many_mat_cal-1/2/3 编译通过（239 指令，之前 PANIC）；QEMU -O1 PASS（3.26s——IRH 退化首次实际运行验证，设计目标 1.5×10¹⁰ → 10⁶ 元素操作）；全量 perf 汇编扫描 0 panic、0 失配、其余用例指令数与修复前完全一致（huffman 460 / fft1 562 / 01_mm 235 / 03_sort 527 等不变）；raana_ir 264 + workspace 11 suite 全绿；huffman/fft1 QEMU 回归 PASS。临时诊断代码已全部移除（pass.rs 无残留 diff）。

## 9. 全量 perf 效率审查：vs clang -O2 汇编对比（2026-08-04）

方法：clang 17（--target=aarch64-none-elf -O2 -S，`const int N = k;` sed 成 `#define N k`，加 -Wno-implicit-function-declaration），与本编译器 -O2 同口径数指令（函数级 + 循环体级）。**指令数≠动态效率**：huffman 静态 +9 但 decode 循环全内联（动态优于 clang 的 bl read_bits）；01_mm 静态 -68 但 clang 主循环用 NEON（35 条 SIMD，动态我们落后——NEON 按用户指示最后考虑）。

### 9.1 指令数全景（clang vs ours，按落后幅度排序）

| 用例 | clang | ours | Δ | 落后主因（函数级） |
|---|---|---|---|---|
| fft0/1/2 | 297 | 562 | +265 | multiply 64v43 / power 36v22 / fft 151v109 + fft_specialized_0 139（clang 无此克隆）/ main 172v90 |
| 03_sort | 374 | 527 | +153 | radixSort_specialized_0 239（specialize 递归克隆双份）+ radixSort 230v215 |
| matmul | 132 | 200 | +68 | main 内 k 循环地址 madd 重算 |
| h-5 | 242 | 328 | +86 | kernel_ludcmp 276v203（stride 常量循环内重载 + 地址 madd） |
| h-8 | 134 | 165 | +31 | kernel_nussinov 127v106 |
| knapsack | 61 | 99 | +38 | knapsack_naive 53v32 |
| h-1 | 68 | 98 | +30 | fun 27v21 / main 71v47 |
| sl1 | 158 | 206 | +48 | 二维地址 madd 重算 + 常量循环内重载（movz 14%） |
| huffman | 451 | 460 | +9 | 持平（动态我们更优，见上） |
| crypto | 445 | 430 | -15 | pseudo_md5 299v135 但 pseudo_sha1 被 DFE 删（clang 保留死函数） |
| 领先：h-10 -52%、crc -47%、h-4 -45%、conv2d -29%、01_mm -22%、shuffle -22%、h-9 -21%、many_mat_cal -8%、transpose -8% |

### 9.2 效率差距分类（按通用性与动态收益排序）

**A. 循环内大常量物化重载（头号通用差距，影响 fft1/sl1/matmul/h-5/h-1/crypto/huffman/crc）**
- 现象：IR 的 const 是值（不在 layout），后端在**每个使用点**物化 movz/movk。循环内使用的 magic 取模常量（998244353）、stride（5600/15900）、边界（1000）每轮重载 2-6 条。
- 实测：fft1 蝶形循环体每轮 movz/movk ×4-6（clang 循环外 w22/w23 加载一次，循环内 0 条）；movz/movk 静态占比 ours 9% vs clang 2%（fft1）、sl1 14% vs 3%、h-1 14% vs 2%、crypto 10% vs 4%、huffman 8% vs 3%。
- 根因：LICM 把 const 当 invariant（licm.rs:157-162）但不提升（const 无 layout 位置）；taki_mir 无机器层 LICM（搜索无）；ion RA 无循环概念，remat 决策每使用点独立。
- 候选方案（IR 层，需先讨论定方案）：
  1. 新增 `InstKind::Materialize(Integer)`（或复用现有指令的恒等形态如 `Binary(Add, C, 0)`——需防 GVN/常量折叠回 C）：在 preheader 物化一次，循环内引用。
  2. GVN/专门 pass：循环内使用 ≥2 次的 const 操作数 → preheader 物化。
  3. 后端 remat 成本模型：大常量（>12-bit imm）remat 成本算 2，倾向寄存器分配——但无循环信息，治标不治本。
- 验证：fft1 蝶形循环体指令数 30→20 左右；sl1/matmul/h-5 循环体 movz 清零。

**B. GEP 二维地址未指针化（sl1/matmul/h-5，A 的伴生）**
- 现象：`a[i][j]`（i 循环不变、j 循环变）每轮 `mov x, xzr; add x, x, w, sxtw; movz/movk stride; madd addr, idx, stride, base` 重算（sl1 两处、matmul 内层、ludcmp 多处），clang 全部双指针步进（`ldr [xN]; add xN, xN, #4`）。
- 根因：PSR（pointer_strength_reduction）未覆盖"不变行 + 变列"的 2D GEP 形态（最近 commit 1261113 只支持 runtime-bound 循环的一种形态）；A 的常量重载使 madd 序列更贵。
- 候选：PSR 扩展——循环内 `getelemptr base, (i, j)`，i 不变 j 是 IV → preheader 算 `base + i*stride`，循环内 `p += 4` 指针步进。注意 32 位索引符号扩展（sxtw）与 64 位地址。
- 验证：sl1/matmul/h-5 内层循环体指令数对比 clang。

**C. specialize 对递归函数的过度克隆（03_sort +153 主因）**
- 现象：radixSort 递归（bitround 递减），main 以常数调用 → specialize 克隆 radixSort_specialized_0（239 条）；克隆体内部递归仍调通用 radixSort（bitround 递归时非常数）→ 双份代码共存，specialized 版只执行顶层 1 次，收益 = bitround 常数折叠。
- 候选：specialize 对递归 callee 的克隆策略——内部递归调用重定向到 specialized 版（保持折叠链，但体积仍 ×2 且位宽递减需要多版？）或递归函数直接不克隆（省 239 条体积）。需量化 specialized 版顶层执行的动态收益。
- 验证：03_sort 指令数 527→~290（去 specialized 双份）；radixSort 性能不退化。

**D. 函数序言/尾声 folded spill（fft1 multiply 64v43、power 36v22 等）——已实施（2026-08-04）**
- 现象：我们的序言 `stp x29,x30 + sub sp + add x29 + str x19 + str x20`（5-6 条）vs clang `stp x29,x30 + stp x20,x19 + mov`（3-4 条，folded spill 用 stp 一次存两个 callee-saved）。
- 实施：abi.rs `gen_clobber_save/gen_clobber_restore` 把 callee_saved 中相邻 8 字节整数对合并成 `StorePair/LoadPair`（SignedOffset，stp/ldp），寄存器顺序与槽位地址对应关系按 stp 语义（src1 存低地址）调整。非 lowering 改动（frame orchestration 钩子，RA 后寄存器/偏移全已知），零依赖检查需求。
- 结果：**全量 60 用例指令数全减，总计 -438**：fft1 562→521（-41，multiply 序言 `str x19;str x20`→`stp x20,x19`）、03_sort -19、h-5 -12、knapsack -12、h-8 -8、sl1 -8、conv2d -6、huffman -6，无一增加。workspace 全绿，fft1/huffman/many_mat_cal QEMU -O1 回归 PASS。
- 遗留：**crypto-1 大帧（pseudo_md5 帧 688B）**——10 个 callee-saved（x19-x28）偏移 608-680 超 stp SignedOffset 编码范围（±512）未合并（clang 同函数只用 1 个 callee-saved + 576B 局部 spill，RA 策略差异）。候选：FrameLayout 槽位布局把 callee-saved 放帧低偏移区（可编码 stp）或分块 stp。
- 连续 store 扫描结论：**通用 merge pass（方案 2）当前无候选**——局部数组初始化已是 MemZero（`bl .Lsoyo_memzero`，03_sort head[16] 等），全局数组走 BSS `.zero`，无展开的连续 store 序列；唯一相邻 store 对是 crypto-1 大帧 callee-saved（超编码范围，属布局问题）。暂缓方案 2。

**E. huffman 范围检查折叠（已记录于 §7.4，P2）**：`(c+64)>=65 && (c+64)<=144` → `sub; cmp; b.ls` 单无符号比较。decode 循环每符号 1 次。

**F. crypto pseudo_md5 299v135（低优先）**：clang 主循环更紧凑（可能是循环展开/常量折叠差异），总量我们已领先（-15）。

### 9.3 动态效率注意点（指令数之外的判断）

- huffman 静态 +9 但动态更优（decode 循环零调用 vs clang bl read_bits）；h-10 我们 trsm 内联 vs clang 保留函数（动态我们优）；01_mm 我们静态 -68 但 clang NEON 动态快——**静态对比只用于找差距，最终以 QEMU/真机时间为准**。
- 蝶形循环/radix 主循环/matmul k 循环的每轮指令数是动态热点代理指标：fft1 我们 ~30 条/轮 vs clang ~20 条/轮（差 33%，其中 movz/movk 4-6 条）。

### 9.4 优先级建议

1. **A+B（循环内常量物化 + GEP 指针化）**：P1，通用、跨 ~8 用例，fft1（最大静态差距）与 sl1/matmul/h-5（热循环）直接受益。先定 A 的方案（Materialize 指令 vs 恒等 Binary）再动工。
2. **D（folded spill）**：✅ 已实施（§9.2 D，全量 -438）。遗留 crypto-1 大帧布局（P3）。
3. **C（specialize 递归克隆）**：搁置——纯效率无害（specialized 版仅执行顶层 1 次，527 条 < L1I 32KB），收益仅静态体积/编译时间（比赛按运行时间计分），filter 递归克隆易错。需要时再做。
4. **E（范围检查折叠）**：P2，独立小变换（§7.4 已列）。
5. NEON 最后考虑（01_mm 等）。

### 9.5 验证基线（本清单）

- 全部用 clang -O2 同用例对比 + 循环体每轮指令数 + cargo test + make test 定向；全量由用户跑。
- 合规：全部按 IR 结构/循环结构触发，无名字/输入指纹（常量物化按"循环内使用的不可编码常量"触发，GEP 按索引结构触发，与用例无关）。

## 10. 向量化接力：header [lt,jump] 形态支持（2026-08-05 计划，新 session 执行）

### 10.1 任务

loop_vectorize 的 header 检查支持 `[lt, jump]` 形态（header 只有两条指令：first=lt（bound 条件），second=jump→body）。当前只支持 `[lt, br]`（test-at-top）和 `[jump]`（rotated），`[lt, jump]`（条件上提中间态，rotate/simplify 产生）在 shape_header_multi_inst 拒绝。

### 10.2 证据（已实证）

- **批量扫描 60 例：`[lt, jump]` 形态 144 例循环**——是最大的「不支持形态」；matmul 掩码内核（BB19）只是其中 1 例。
- shape_header_multi_inst 总数 192（含 [lt,jump] 144 + [lt,br] 变体 24 + 复杂 bound 24）。
- matmul1 的 BB19/BB10/BB32/BB44 都是 `[lt, jump]`；BB44（sum 循环）后续轮结构变化被接受（说明该形态在 fixed-point 中可变），BB19（掩码内核）后续轮 not_innermost 未解锁。
- 掩码内核解锁链现状：shape_body ✅（5f87c57 B1 true 边泛化）→ **shape_header_multi_inst（当前卡点）** → 后续检查（entry_trip/payload/exit）未验证。

### 10.3 改动位置与设计

- 文件：raana_ir/src/opt/passes/loop_vectorize.rs，analyze_loop 的 header 检查（~442-472 行 test_at_top 分支）。
- 现状：test_at_top 要求 second 是 Branch（~452 行 `InstKind::Branch` else shape_header_multi_inst）；`[lt,jump]` 的 second 是 Jump → 拒绝。
- 设计：first=lt（bound 条件）、second=jump→body 时，接受为 test-at-top 变体——lt 的结果被 body/latch 的 br 使用（需确认 use 结构：lt 的 users 在循环内 br 的 cond）；trip 语义由 entry_trip 检查（bound 常量）兜底验证；bound 非常量自然走 entry_trip_not_const。
- 参考：test_at_top 现有参数（bound_inst、entry 边重写 4Q counter 物化——A3 8f4907a 已支持多参数）。

### 10.4 验证步骤

1. 加/用插桩：VECDBG_SHAPE=1 时 analyze 打印 [SHAPE-HDR] header={:?} insts=[...]（已在代码中，env 控制——见 analyze_loop header_insts 处）。
2. 改 header 检查（接受 [lt,jump]）。
3. M44_TRACE=1 跑 matmul1：BB19 应过 header 检查（后续卡点变化）。
4. 批量扫描：shape_header_multi_inst 192 → 应显著下降（[lt,jump] 部分）。
5. 逐例看后续检查（entry_trip_not_const / shape_body / payload / exit）——统计 [lt,jump] 循环实际解锁到哪一步。
6. cargo test -p raana_ir 全绿；make test 定向 matmul1/2/3 三级差分 PASS。
7. 结论落档 TODO（解锁数、剩余卡点分布）。

### 10.5 风险与后续链

- 风险：lt 的 use 结构可能多样（body br / latch br / 死值）——先确认再放宽；放宽后 entry_trip/payload/exit 会继续拒一批（正常，逐步看）。
- 后续链（形状检查全景）：not_innermost 276 → M42 verdict → **shape_header（本任务）** → shape_body 96（B1 已修部分）→ entry_trip_not_const 36（M43 versioning，大工程）→ payload Rem 3（ISA）→ exit 参数。
- 已解锁（勿回退）：sum 归约（f0455b0 ipsccp Bottom + 932ee76 splat(0)+exit 加回）、B1 true 边 payload（5f87c57）、清零循环。

### 10.6 全景探究：距离 clang -O2 NEON 的差距地图（2026-08-05 晚，全 corpus 实证）

**当前向量化命中（60 例全扫，M44_TRACE + 汇编 grep）**：matmul1/2/3 三例——
sum 归约循环 `dup v0.4s, w5 → ldr q1 → add v0.4s → addv s0`（BB44 链）+ 清零循环
`dup v19.4s, w3 → str q19`（BB33）。其余 57 例零向量指令。

**clang 侧 NEON 基线（同用例 clang17 aarch64 -O2 -S 实证，向量指令计数）**：

| 用例 | clang NEON 条数 | 形态 | 我们的差距主因 |
|---|---|---|---|
| conv2d-1 | 18（fmla 系） | 邻域卷积 | A3 多参数 test-at-top + 双臂 if + Rem(ISA) |
| 03_sort1 | 12 | 清零+基数统计 | test_at_top_multi_param + non_unit_step |
| many_mat_cal-1 | 7 | 归约 | 已 M48 外提，剩余小 |
| 01_mm1/2/3 | 6（ld1r+mla） | 矩阵乘 | A3 多参数 + entry_trip_not_const(M43) + CallInBody |
| matmul1/2/3 | 4 | 矩阵乘 | 已出 sum+清零，内核掩码未解锁 |
| transpose2 | 4 | 转置 | NonAffineIndex + test_at_top_multi_param |
| crypto-1 | 4 | 清零 | IntraIterationConflict + shape_body |
| sl1 | 1 | - | shape_body |
| fft1 / huffman-01 | 0 | 递归/位操作 | clang 也不向量化（无差距） |

**60 例拒绝分布（raw 次数，含 fixed-point 多轮）**：not_innermost 948（正确保守）、
verdict=Reducible 783、shape_header_multi_inst 282、shape_body_not_2_blocks 258、
CallInBody 180（输入循环，正确拒绝）、test_at_top_multi_param 108、entry_trip_not_const 108、
non_unit_step 90、IntraIterationConflict 84、NoInductionVariable 57、bound_not_const 54、
NonAffineIndex 36、LoopCarriedConflict 18、params_not_2 12、DynamicMemZero 12、
load_unmodeled/load_classify/gep_offset_loop_variant/entry_i0_not_const 各 12。

**§10 [lt,jump] 任务 ROI 实证（关键修正）**：
- 138 个 unique [lt,jump] header（33 用例），其中 **36 个 rotate 不兜底**（后续轮从未以
  [jump] 形态被 analyze）：01_mm1/2/3 BB5/BB16、03_sort1/2/3 BB2、conv2d-1/2/3 BB2/BB14、
  crypto-1/2/3 BB8、fft0/1/2 BB2、h-10-01/2/3 BB5/BB11、matmul1/2/3 BB19/BB27、
  optimization_scheduling1/2/3 BB2。
- 36 个不兜底 header 的后续命运（同 header 全轮次拒绝点）：
  * **形态类（[lt,jump] 支持后可解锁，真正受益面 ~6-10 个）**：conv2d-1 BB14
    （shape_body+test_at_top_multi_param，固定点中形态多次变化）、matmul1 BB27
    （shape_body_not_2_blocks——掩码内核 body 双臂形态）、03_sort1 BB2
    （test_at_top_multi_param）。
  * **语义类（[lt,jump] 支持后仍正确拒绝，仅提前拒绝无解锁收益）**：01_mm BB5/BB16
    （CallInBody 输入循环）、crypto BB8（IntraIterationConflict 就地 R/W）、fft BB2
    （NoInductionVariable 位反转）、h-10 BB5/BB11（CallInBody）、matmul1 BB19
    （not_innermost——interchange 后掩码内核成外层）。
- **结论**：[lt,jump] 支持（§10.3 设计 ~50 行）是**必要前置**而非独立收益——36 个不兜底
  header 中形态类（conv2d/matmul/03_sort 内核）在 [lt,jump] 支持后进入 test-at-top 路径，
  与 A3 多参数（108 次）、B1 双臂 if（shape_body 258 中剩余）串联解锁。语义类提前拒绝
  是副产品（fixed-point 首轮收敛）。

**优先级链（实证修正版，perf ROI）**：
1. [lt,jump]（§10，~50 行）→ 解锁 conv2d BB14 / matmul BB27 / 03_sort BB2 的形态层
2. A3 多参数 test-at-top（~中改动）→ 01_mm 计算内核 + conv2d 内核（clang 6/18 NEON 所在）
3. B1 双臂 if 多块体（~300 行）→ matmul1 掩码内核（BB27 shape_body）
4. M43 versioning（大工程）→ entry_trip_not_const 108 + bound_not_const 54（01_mm 系）
5. 逐 lane select lowering（bsl/csel）→ matmul1 min 循环（select %101,%100,%vid_6）
6. 不修：Rem payload（ISA 无整数向量 div/mod）、非连续访存（gather 禁止）、
   CallInBody 输入循环（正确拒绝）、fft/huffman（clang 也不向量化）

### 10.7 [lt,jump] 形态终局定性 + 支持设计（2026-08-05 晚，插桩实证）

**形态本质（插桩实证，5 用例 33 个 [LT-JUMP] 打印全 users=[]）**：

```text
header(params):
    %t = lt %iv, %bound     # 死值！used_by = []
    jump body               # 无条件跳，无参数
```

- **lt 结果恒为死值**（matmul1 BB10/19/27/32/44、conv2d-1 BB2/4/14、03_sort1 BB2/28-59、
  01_mm1 BB5/16/28-58、fft1 BB2/15/16 全部 users=[]）。
- **来源 = rotate_count_up/down 旋转成功的残留**：rotate 只替换 header terminator
  （rotate_loops.rs:160/388 `.jump(body, vec![])`），**不删除 header 前部的 lt 指令**；
  DCE 在 fixed-point 尾部删除它。
- **为什么 loop_vectorize 会看到它**：rotate 与 loop_vectorize 同轮（pass.rs:225/257），
  rotate 刚旋转完（lt 残留）→ 同轮 loop_vectorize 首轮看到 [lt,jump] → 拒绝
  （shape_header_multi_inst）→ 轮尾 DCE 删 lt → 下一轮 loop_vectorize 看到 [jump] 接受。
  实证：BB44（sum 循环）首轮 [lt,jump] ×1 → 后续轮 [jump] ×5 → 向量化（matmul1 出 addv）；
  最终 IR 无 lt 残留（while_entry_33 仅 jump）。

**结论（修正 §10.2 预期）**：
- 144 例 [lt,jump] **不是"不支持形态"，而是"已旋转的中间态"**——对最终代码生成
  零影响（DCE 后第二轮以 [jump] 向量化，BB44/BB33 已证明）。
- "BB19 是掩码内核卡点"不准确：BB19 的 lt 是死值，其循环 interchange 后成外层
  （not_innermost 正确拒绝）。掩码内核真实卡点 = while_entry_17（[jump] 多块体，
  `br %72, end_25, then_24` **false 边 arm**——B1 只支持 true 边，5f87c57 未覆盖）。
- 36 个"rotate 不兜底" header 实际是"首轮 [lt,jump] 拒绝后循环被 interchange/
  not_innermost 改写"，[lt,jump] 支持对它们无解。

**支持设计（rotated 路径放宽，非 §10.3 的 test-at-top 变体）**：
- 位置：loop_vectorize.rs analyze_loop header 检查（489-537 行）rotated 分支。
- 变换：header 指令 = `[...死值指令..., jump]`——最后一条是 jump（target == latch
  或 arm_plan.body_br、args 空，检查不变），**前面所有指令 used_by 全空则跳过**；
  任一前指令有 users → 拒绝（保守，保持现状）。
- 不引入 test_at_top 语义：trip 走 rotated 路径（counter 参数，entry_trip 常量）。
- 改动量：~25 行（analyze header 检查）+ 2 单测（死值接受 / lt 有 users 拒绝）。
- 风险：极低——接受条件 = 死值（无观察者），拒绝面不变。

**收益（诚实）**：**无独立代码生成收益**（第二轮 DCE 后同样向量化）。收益 =
fixed-point 提前一轮收敛（首轮直接向量化，免去中间 pass 改写循环体的不确定性），
对已解锁循环（sum/清零）是确定性改善。**不建议作为独立任务优先**，可与
B1 false 边 arm（掩码内核真正卡点）捆绑做。

**真正解锁顺序（修正版，§10.6 链）**：
1. B1 false 边 arm + 双臂 if 多块体（shape_body 258，matmul1 掩码内核）——最高 ROI
2. A3 多参数 test-at-top（108，conv2d-1 BB14 / 01_mm 内核）
3. M43 versioning（entry_trip_not_const 108 + bound_not_const 54，01_mm 系）
4. B2/B3 min 归约 select（matmul1 min 循环）
5. [lt,jump] 放宽（~25 行，收敛优化，随 1 捆绑）

### 10.8 B1 复核与掩码内核真卡点：interchange used_by 泄漏（2026-08-05 晚，插桩实证）

**实证推翻 §10.7 的"B1 false 边未实现"假设**：B1 true 边（5f87c57）**和 false 边
（原始实现，loop_vectorize.rs:417-435）都已存在**。matmul1 掩码内核（interchange 后
BB16：`BR cond t=latch f=arm`，arm jump latch）**B1 匹配成功**（B1_DBG 插桩实证），
卡在更后面的 **value_escapes_loop**。

**逃逸值（B1_DBG 插桩，3 轮一致）**：
```text
ESCAPE inst=Inst(133) kind=GetElemPtr(base=..., offsets=[602, 309])   # c[i][j] GEP
  -> user=Inst(135) kind=Store { src: Inst(403), dest: Inst(133) }      # store c[i][j], temp
  user_bb=None exit=BB21 in_loop=false
  user in exit layout: false; user's used_by: {}                        # 游离死指令！
```

**根因链**（loop_interchange.rs migrate_reduction，1625-1632 行）：
1. interchange 归约迁移删 E_k 的 `store temp, c[i][j]`：用 `layout_mut().remove_inst`
   （只删 layout parent/back/insts），**未清理 store 操作数的 used_by**；
2. store(135) 成游离死指令（不在任何块、无 users），但 **c_gep(133).used_by 仍含 store(135)**；
3. loop_vectorize value_escapes_loop（1207-1215 行）：payload GEP(133) 的 user store(135)
   parent_bb=None → 不在循环内 → 误判逃逸 → 拒绝整个循环。
4. 与已修过的同类坑一致（记忆：layout remove_inst 不清理 terminator 的 target used_by）。

**为什么现在才暴露**：此前掩码内核在 shape_header（[lt,jump]）就拒，走不到 escape 检查；
[lt,jump] 中间态 + B1 匹配后检查链推进到 escape。

**修复（两层）**：
1. **根因**：loop_interchange.rs:1628 `data.layout_mut().remove_inst(e_k, inst)` →
   `data.remove_layout_inst(e_k, inst)`（function.rs:145：detach_inst_usage + remove_inst +
   remove_inst_data——正确清理 used_by）。注意 1601-1607 的 c_gep **移动**（remove+insert）
   必须保留 used_by，**不动**。
2. **防御**：loop_vectorize value_escapes_loop 跳过 `parent_bb(user) == None` 的游离 user
   （不在 layout = 不可执行 = 无观察者，不构成逃逸；防其他 pass 同类残留）。

**正确性论证**：
- 根因修复：store 删除前 detach 其 src/dest 的 used_by——store 无其他引用（游离前唯一
  引用就是 c_gep.used_by），删 inst_data 安全；c_gep 后续仍被新 store（1615 行）使用，
  used_by 自动注册。
- 防御修复：parent_bb=None 的指令不可达（CFG 外），跳过不改变任何可观察行为；escape
  语义（payload 值不被循环外活代码使用）不受影响。
- interchange 4 单测（matmul 交换/幂等/方向反/非零初始）+ raana_ir 全量回归。

**预期解锁链**：修复后 matmul1 掩码内核（BB16）过 escape → 下一卡点（候选：
no_vector_ops / exit 参数 / trip）逐个推进；matmul1 汇编出 `ldr q + cmgt/and/eor +
add v.4s + str q`（clang 对照）为最终验收。

**改动量**：根因 1 行 + 防御 ~5 行 + interchange 单测补 used_by 断言（可选）。

**执行记录（2026-08-05 晚，完成）**：
- **根因修复**：loop_interchange.rs migrate_reduction 删 E_k store 改用
  `remove_layout_inst`（detach used_by）。interchange 4 单测全过。
- **防御修复**：loop_vectorize value_escapes_loop 跳过 parent_bb=None 的游离 user。
- **连锁发现（掩码构造正确性 bug，首次真实触发）**：修复后掩码内核向量化
  （matmul1 汇编 6→24 条向量，ldr q + mul + cmeq + and/eor + str q），但
  **-O1/-O2 wrong answer**（-O0 PASS，最小复现 mmask 100×100：O0=94 vs O2=68）。
  二分定位（恒真 PASS、掩码+1 PASS、delta 版 FAIL；sched 68 / nosched 14）：
  **B1/VecSelect 掩码构造 `m = sub(0, eq(cond, 0))` 假设 IR 比较返回 0/1，但
  lowering 的 cmeq 返回全 1（0xFFFFFFFF）→ m = 1 而非 -1 → ~m = -2（清 bit 0）→
  掩码损坏**。修复：**m = eq0 直接**（cmeq 全 1/全 0 即掩码），删 sub；两处
  （B1 masked store 1687 行 + VecSelect 1761 行）同步。
- **验证**：matmul1 -O1/-O2 PASS（掩码内核出 15 条向量）；mmask 复现 O0/O2/nosched
  = 94/94/94；raana_ir 344 全过（含 vectorizes_lane_cond_select）；workspace 全绿；
  corpus 复扫 matmul1/2/3 掩码内核向量化（24 条）；临时用例已删。
- **教训**：spike 单测（vectorizes_lane_cond_select）只断言 IR 形态不断言运行语义，
  cmeq 全 1 语义假设在 IR/lowering 层未闭环——向量比较结果必须文档化为全 1/全 0
  （与 NEON cmeq 一致），掩码构造直接复用比较结果。

### 10.9 A3 多参数 test-at-top 支持（2026-08-05 晚，goal 提示词落档）

**背景（corpus 实证）**：test_at_top_multi_param 21 case（conv2d×8 / many_mat_cal×11 /
transpose×5 / 01_mm×4）——是当前最大单一形态缺口；conv2d 是 clang NEON 收益最大目标
（4-18 条 fmla），其计算内核 4 层嵌套（i/j/k/l），每层 header 5-7 参数（外层 IV
passthrough），内层 bound 常量（`lt %vid_8, 5`），Rem 在 get_random（输入循环）不在
计算内核。**障碍链：A3 → 双臂 if（B1 扩展，后续）**。

**现状代码（loop_vectorize.rs）**：
- analyze passthrough 识别（612-614 行）：`back_args[i] == params[i]`——rotated 与
  test-at-top 共用；test-at-top 的 back_args 来自 latch plain jump args（580-587 行）。
- effective 计算（616-626 行）：test_at_top 排除 passthrough 后要求恰好 1 个有效参数
  （630-633 行 test_at_top_multi_param 拒绝）。
- apply 已支持多 passthrough：VecPlan.passthrough_args、epilogue/reduce 块转发
  （1476-1487 行）、entry 边"counter 追加、原参数保槽"（1446-1468 行）。

**目标**：test-at-top 循环的额外 header 参数识别为 passthrough（外层 IV 原样转发）并
走通向量化；conv2d/many_mat_cal 内层出向量指令。

**自包含 goal 提示词（可直接粘贴新 session）**：

```text
# Goal: A3 多参数 test-at-top 向量化支持（loop_vectorize）

## 背景
SysY 编译器项目（Rust），工作目录
/Users/azureskye/Documents/Programs/rust/AnonBeijingCompiler-hermes，
分支 feat/loop-vectorize-hermes。AArch64 的 NEON 向量化 pass 在
raana_ir/src/opt/passes/loop_vectorize.rs。当前 60 例 perf corpus 只有
matmul1/2/3 出向量（sum/清零/掩码内核）；test_at_top_multi_param 是最大
单一形态缺口（21 case：conv2d×8/many_mat_cal×11/transpose×5/01_mm×4）。
设计背景见 TODO.md §3.2（A3）、§10.6-10.9；本提示词自包含。

## 必须遵守的规则（用户明令）
- 只在 hermes worktree 操作；不 push、不 rebase（遇冲突立即停并汇报）；
  不碰其他 worktree；不读/不提交 .env 等凭据文件。
- 只改 raana_ir crate；不碰 anon_armv8（lowering）、taki_mir、前端。
  IR 层问题只改 IR 层。
- 无 hacky workaround（rm、手动 sed、flat pool 一律禁止），只做 root
  cause 修复；设计中禁止 Rc<RefCell<T>> / RefCell 共享可变。
- 不做以 benchmark/函数名/输入为条件的优化（AGENTS.md 红线）。
- -O0 保留标量；vectorize 只进 aarch64 管线（RISC-V 零影响）。
- 勤 commit、勤单测：每个原子部分完成即 commit（消息格式
  `[Feat(Opt)]: A3 ...` / `[Fix(Opt)]: ...` / `[Docs]: ...`），中文消息。
- 改文件一律 patch；不用 python 脚本改文件；不碰 .docker-image、
  不 rm -rf、不 cargo clean。
- 验收粒度 = 单测 + 单 case（make test <case>）；全量测试用户自己跑。

## 现状与设计

### 现状（loop_vectorize.rs）
- analyze 的 passthrough 识别（约 612-614 行）：
  `passthrough = back_args[i] == params[i]`（back-edge 参数原样转发），
  rotated 与 test-at-top 共用；test-at-top 的 back_args 取自 latch 的
  plain jump args（约 580-587 行）。
- effective 参数（约 616-626 行）：test_at_top 排除 passthrough 后要求
  恰好 1 个（IV），否则 trace test_at_top_multi_param 并拒绝
  （约 630-633 行）。
- apply 已支持多 passthrough：VecPlan.passthrough_args、epilogue/reduce
  块转发（约 1476-1487 行）、entry 边追加 counter 且原参数保槽
  （约 1446-1468 行）。

### 设计
1. **形态实证先行**（禁止凭假设改代码）：M44_TRACE=1 跑 conv2d-1、
   many_mat_cal-1、transpose2、01_mm1，定位所有 test_at_top_multi_param
   拒绝的循环；对每个循环用 --emit ir + VECDBG_SHAPE=1 确认：
   header 参数表、latch 形态（单块/多块）、back-edge args 与 params 的
   对应关系（哪些是原样转发、哪些是 IV 更新、哪些是 phi 链传值）。
   结论写入 TODO.md §10.9 更新（现象数据 + 形态分类）。
2. **passthrough 识别扩展**（analyze）：若实证显示外层 IV 经单层 phi
   链（中间块参数）转发，参照 rotate_loops.rs 的 value_flows_from
   （约 419-450 行）扩展 back_args[i] 与 params[i] 的等价判定；
   若实证显示 back_args[i] == params[i] 已覆盖但 effective 仍有多个，
   则逐参数分类（IV / passthrough / 其他）并拒绝真正无法识别的参数
   （保守宁漏勿错）。**不引入多 IV / 多归约支持**。
3. **apply 核对**：确认 counter 物化 + passthrough 转发在"多 passthrough
   + 单有效 IV"下正确（entry 边追加 counter、epilogue 链转发、exit 边
   passthrough 传值）；如有缺漏补齐（极小改动）。
4. **单测**（raana_ir/src/opt/passes/loop_vectorize.rs tests 模块）：
   - 多参数 test-at-top 向量化成功（构造 4-6 参数 header：外层 IV
     passthrough + 内层 IV + 常量 bound，断言向量指令出现 + 幂等）；
   - 非 passthrough 额外参数仍拒绝（保守）；
   - 已有单测不回归（vectorizes_test_at_top_loop /
     vectorizes_multi_param_test_at_top / rejects_test_at_top_nonconstant_bound）。
5. **端到端**：conv2d-1 / many_mat_cal-1 / transpose2 / 01_mm1 定向
   M44_TRACE 复扫（test_at_top_multi_param 计数下降，记录解锁链推进到
   哪个检查）；能解锁的循环出向量指令（汇编 grep）。**不承诺 conv2d
   最内核出 fmla**（后续还需双臂 if B1 扩展）——验收口径 = 计数下降 +
   解锁循环出向量 + 无回归。

## 验证命令
- 单测：cargo test -p raana_ir 2>&1 | grep -E "test result"
- corpus 复扫（拒绝分布）：M44_TRACE=1 逐个跑
  ./target/release/compiler -O2 --target aarch64 -S tests/perf/<case>.sy
  2>/tmp/x.log，grep -oE "reject=[A-Za-z0-9_:]+" | sort | uniq -c
- IR 检查：./target/release/compiler -O2 --target aarch64 --emit ir
  -o /tmp/x.ir tests/perf/<case>.sy
- 定向差分（Docker 内，慢）：make test ARGS="-O 2 -j 1" perf/<case>.sy
- 汇编向量检查：grep -cE "addv|ldr q|str q|add v[0-9]|mul v[0-9]|dup v[0-9]"
  /tmp/x.s

## 关键代码位置
- raana_ir/src/opt/passes/loop_vectorize.rs：analyze_loop 的
  test_at_top 分支（约 489-537 行 header 检查、567-633 行 exit/latch/
  passthrough/effective、630-633 行 test_at_top_multi_param 拒绝点、
  1162 行附近 payload 分类、1446-1468 行 apply counter 物化、
  1476-1500 行 epilogue 参数）。
- raana_ir/src/opt/passes/rotate_loops.rs：value_flows_from（约
  419-450 行，phi 链等价判定参考实现）。
- 既有 test-at-top 单测：loop_vectorize.rs tests 模块
  build_test_at_top（约 3067 行）、vectorizes_test_at_top_loop（约
  3127 行）、build_multi_param_test_at_top（约 3156 行）、
  vectorizes_multi_param_test_at_top（约 3314 行）。

## 验收清单（全部满足才算完成）
- [ ] 形态实证记录写入 TODO.md（conv2d/many_mat_cal/transpose/01_mm 的
      test_at_top_multi_param 循环分类：passthrough 可识别 vs 需 phi 链
      vs 真不可识别）
- [ ] analyze 扩展完成：passthrough（含 phi 链）识别 + effective 修正，
      不引入多 IV/多归约
- [ ] 单测 ≥2 新增全绿；raana_ir 全量（344+）全绿；workspace 全绿
- [ ] conv2d-1 / many_mat_cal-1 / transpose2 / 01_mm1 的
      test_at_top_multi_param 计数下降（记录数字）
- [ ] 解锁的循环汇编出向量指令（记录 case + 指令）
- [ ] matmul1 -O2 仍 PASS（既有能力不回归）；RISC-V 零影响（不注册）
- [ ] 每步独立 commit；最终 git status 只剩非本任务文件
```

**执行建议**：先做步骤 1 形态实证（只读 + 插桩可选），确认 conv2d 内层
back-edge 是否 phi 链形态，再决定步骤 2 用 value_flows_from 还是直接放宽；
若实证显示 test_at_top_multi_param 拒绝的是真多有效参数（非 passthrough），
A3 收益归零，立即停并汇报（宁停勿猜）。

---

### 10.10 形态实证结论（2026-08-06 执行，M44_TRACE + VECDBG_SHAPE 插桩，4 case 全扫）

**插桩**：loop_vectorize.rs analyze 增加 [SHAPE-BACK] 打印（VECDBG_SHAPE=1，
back_args/params 逐槽位分类：IDENTITY / binary:op / blockarg / int），
已随本次提交入库（env 门控，与 M44_TRACE 同类诊断）。

**现象数据（test_at_top_multi_param 拒绝的循环，按 case）**：

| case | header | name | n_params | slots（back-arg 分类） | M42 verdict |
|---|---|---|---|---|---|
| conv2d-1 | BB(14) | while_entry_2_checksum_inline_14 | 2 | [Add, Add] | Reducible{acc=Inst(103),IntAdd} |
| conv2d-1 | BB(18) | reduction_main_header_18 | 5/6 | [Add×5] / [int(0), Add×5] | — |
| conv2d-1 | BB(2) | while_entry_2 (get_random) | 2 | [Add, Rem] | — |
| conv2d-1 | BB(16) | while_entry_2_get_random_inline_16 | 2 | [Add, Rem] | — |
| many_mat_cal-1 | BB(49) | while_entry_49 | 13 | [IDENTITY×7, Add, Add, IDENTITY×4] | Reducible{acc=Inst(412),IntAdd} |
| many_mat_cal-1 | BB(55) | while_entry_55 | 2 | [Add, Add] | Reducible{acc=Inst(414),IntAdd} |
| many_mat_cal-1 | BB(63) | irh_while_entry_55_63 | 2 | [Add, Add] | Reducible{acc=Inst(509),IntAdd} |
| many_mat_cal-1 | BB(46) | while_entry_46 | 2 | [Add, Add] | Forbidden(UnknownBase) 部分轮次 |
| many_mat_cal-1 | BB(37) | while_entry_37 | 6 | [IDENTITY×4, Add, Div] | — |
| many_mat_cal-1 | BB(68) | reduction_main_header_68 | 6 | [blockarg, Add×5] | — |
| transpose2 | BB(10) | while_entry_10 | 2 | [Add, Add] | Reducible{acc=Inst(109),IntAdd} |
| transpose2 | BB(26) | reduction_main_header_26 | 5/6 | [Add×5] / [int(0), Add×5] | — |
| 01_mm1 | BB(20) | while_entry_20 | 3 | [IDENTITY, Add, Add] | Reducible{acc=Inst(158),IntAdd} |

**形态分类（关键结论）**：

1. **不存在 phi 链 passthrough 形态**。所有 IDENTITY 槽位都是直接的
   `back_args[i] == params[i]`（已覆盖）；唯一 blockarg back-arg 出现在
   reduction_unroll 产物（BB(68)，多累加器）中，非本任务范围。设计假说
   （"外层 IV 经单层 phi 链转发"）**被实证推翻**——value_flows_from 扩展
   收益为零。

2. **主缺口是 test-at-top 单归约 [iv, acc]（B1 形态），不是多参数**：
   6 个循环（conv2d BB(14)、many_mat BB(49)/BB(55)/BB(63)、transpose2
   BB(10)、01_mm1 BB(20)）都是 2 个有效参数 = IV + 累加器，M42 已判
   Reducible（acc 是 header 参数）。当前代码在 663 行
   `test_at_top && effective.len() != 1` 直接拒绝，**根本没走到 667 行
   的 acc_info 匹配**（该路径只服务 rotated 的 [iv, acc, t]）。B1 的
   apply 侧（reduce 块、epilogue acc 参数、build_exit_args 的 Acc spec）
   是形态无关的，只差 analyze 侧放行 test_at_top 走 acc_info。

3. **真多有效参数（保守拒绝正确）**：
   - get_random/init_matrix [Add, Rem]：PRNG 状态机（rem 递推，非 Add/Sub
     归约、非 passthrough）→ 不可识别；
   - many_mat BB(37) [IDENTITY×4, Add, Div]：Div 槽是"死 phi 传值"（循环
     体内不读，仅出口转发）→ 非 passthrough 非归约，保守拒绝；
   - many_mat BB(46)：M42 Forbidden(UnknownBase) 部分轮次 → 内存形态问题，
     非参数问题；
   - reduction_main_header_*（conv2d BB(18)、many_mat BB(68)、transpose2
     BB(26)）：reduction_unroll 多累加器产物（UNROLL_FACTOR+2 参数）→
     多归约，A3 明确排除。

**结论**：A3 的"passthrough 识别扩展"设计在 4 case 上收益归零（passthrough
已全覆盖、无 phi 链）。真正可解锁的是 **test-at-top 单归约 [iv, acc]
（B1 形态放行）**——analyze 663 行改为"effective==2 时若 acc_info 匹配则
放行"（复用 667 行逻辑），预计解锁 6 个循环（含 01_mm1 内核、many_mat
matmul k 循环、conv2d checksum）。这属于 B1 既有单归约能力在 test-at-top
形态上的补齐，不引入多 IV / 多归约。是否转向该方向由用户决定（本次仅
实证 + 落档，未改 analyze）。

### 10.11 补充实证：B1 放行前必须查 bound——6 个候选循环全部 runtime bound（2026-08-06）

**新增插桩**：[SHAPE-BOUND]（VECDBG_SHAPE=1，打印 bound 指令形态 +
constant_i64 结果），corpus 全扫（60 例）。

**决定性数据（放行 test_at_top_multi_param 后的下一道卡点）**：

| 候选循环 | bound 形态 | constant_i64 |
|---|---|---|
| 01_mm1 main BB(20) | Call(getint) | None（runtime） |
| many_mat BB(49) k 循环 | Call(getint) | None（runtime） |
| many_mat BB(55)/BB(63) | Call(getint) | None（runtime） |
| transpose2 BB(10) | Call | None（runtime） |
| conv2d BB(14) checksum | binary:Mul (N_eff²) | None（runtime） |

**结论：6 个候选全部 runtime bound → 放行 multi_param 后会被
test_at_top_bound_not_const（918-923 行）继续拒绝，零解锁**。唯一
const-bound 的 test-at-top 多参数循环是 crypto-1/2/3 pseudo_md5 BB(32)
（bound=16），但它 16 参数 15 IDENTITY → effective==1，实际卡在
gep_offset_loop_variant（payload GEP offset 循环可变），非 multi_param
问题。matmul1/2/3 能出向量是因为 bound 是字面常量（while(i<1000)），
与这些 runtime-bound 循环不同类。

**修正后的解锁链**：test_at_top_multi_param →（B1 放行后）
test_at_top_bound_not_const → 需 runtime trip count 支持（动态 counter
或 versioning），设计明确排除（"bound must be const, no versioning"）。
即：**B1 放行是必要条件，但对 4 个验收 case 不充分**——multi_param
计数会下降（验收项 1 满足），但"解锁循环出向量"（验收项 2）在 4 case
上不成立。若目标是 4 case 出向量，需另行评估 runtime-bound test-at-top
支持（新特性，超出 A3 范围）。

---

### 10.12 runtime-bound test-at-top 向量化支持（2026-08-06 细化方案，新 session 执行）

**背景**：§10.10 实证证明 6 个候选循环（01_mm1/many_mat/transpose2/conv2d 内层
归约）形态是 [iv, acc] 单归约（B1 可处理），但 §10.11 证明其 bound 全部是
runtime 值，当前 `test_at_top_bound_not_const`（936-938 行）继续拒绝。本方案
解决 runtime bound → 真正解锁这 6 个循环 + 其他 runtime-bound 循环。

**核心思路（用户确认）**：bound 运行时已知 → entry 边运行时计算
`trip = bound - i0`、`cnt0 = trip & -4`（向量循环 counter，每轮 -4，header 测
!= 0）、`r = trip & 3` 剩余交给一个**运行时标量 tail 循环**（复用原 bound
指令作上界，跑 0..3 轮）。`trip < 4` 时 `cnt0 = 0` → 向量循环自然跑 0 轮，
无需 versioning 守卫分支。不做循环克隆/版本化。

**现状关键代码（行号以 2026-08-06 bea35e2 后为准）**：
- analyze：passthrough/effective 693-701；test_at_top_multi_param 拒绝
  711-713（先于 acc_info 匹配）；bound 检查 936-938；trip<VF 948-950；
  test_at_top_exit_reads_iv 959-961；acc_info 匹配在 effective 之后（约 715+，
  rotated [iv,acc,t] 走 2 个 effective 分支）；IV 识别 test_at_top 分支取
  effective[0]（约 774+，B1 时需排除 acc 槽）；b1_acc_used_outside_latch
  拒绝 exit 直接读 acc（约 877+）。
- apply：counter 物化 + entry 4Q 追加（约 1532-1546、1976-2011）；epilogue
  剥 r 个直线块（1561+，常量 IV 代入，1582-1641 建块）；reduce 块
  （约 1578+，VectorReduce + seed 加回）；exit 边重写（约 1931+，header
  branch f 边 → epilogue/exit，test_at_top 用 build_exit_args 空参数）；
  iv 步进 4、counter 步进 4（约 1986-2001）。
- 单测：build_test_at_top 3155、build_multi_param_test_at_top 3244、
  vectorizes_multi_param_test_at_top 3402、rejects_test_at_top_nonconstant_bound 3554。

**方案分 3 个原子提交**：

**提交 1：const-bound test-at-top B1 单归约放行（独立正确性，4 case 不出向量）**
- analyze 711-713：`test_at_top && effective.len() == 2` 时放行进入 acc_info
  匹配（复用 rotated 的 Reducible 判定），其余仍拒绝；
- IV 识别：test_at_top 且有 acc_info 时，IV 槽 = effective 中非 acc 的那个
  （当前取 effective[0]，[acc, iv] 序会错）；
- b1_acc_used_outside_latch：test_at_top 的 exit 直接读 acc（SSA 支配，无
  exit 参数）需放行——exit 无参数、直接读 header 参数是 test-at-top 的
  正常形态（rotated 才是 latch branch f_args 传 acc'）；
- apply：exit 边重写时 test_at_top + reduction 的 f 边应指向 reduce 块并带
  [acc 向量参数]（rotated 路径 1931+ 已有，test_at_top 分支需补）；exit 无
  参数时 reduce 块算出的标量 sum 需经 exit 参数或 header 参数改写送达
  （构造 4-6 参数 [passthrough…, iv, acc] + const bound 的单测验证）。
- 验收：单测 ≥2 新增（[iv, acc]+passthrough 向量化成功+幂等；非 Reducible
  的 effective==2 仍拒）；raana_ir 全绿；matmul1 不回归。

**提交 2：runtime counter 物化（elementwise，不带归约）**
- analyze 936-938：bound 非 const 时不再直接拒，改记 `runtime_trip=true`
  并把 bound_inst 存进 VecPlan（新增字段）；
- 948-950 trip<VF：runtime 时跳过（cnt0=0 自然处理）；
- 959-961 exit 读 IV：runtime 时若 exit 读 IV，其终值 = bound（运行时值，
  IvFinal 传 bound_inst 而非 i0+4q）；
- apply：entry 前插入 `trip = sub(bound, i0)` + `cnt0 = and(trip, -4)`，
  counter 初始值用 cnt0（替代常量 four_q，约 1976/1990-2011）；latch 步进
  逻辑不变；
- **tail 循环构造**：counter 到 0 后进入新建 tail_header（参数 [iv,
  passthrough…]）+ tail_latch：`lt iv, bound`（复用原 bound_inst）→ br
  tail_latch / exit；payload 用既有 clone_payload_inst 克隆（IV 代入 tail 的
  iv 参数）；iv 步进 1；entry iv0 = i0 + cnt0（运行时 add）。exit 参数按
  exit_specs 重建（IvFinal 传 bound_inst）。
- **防二次向量化**：tail 是标量循环且 runtime bound → fixed-point 下轮会
  再向量化它造成无限循环。方案：analyze 拒绝"header 名以 vec_tail_ 前缀"
  的循环（与 vec_epi_/vec_reduce 命名一致，同属本 pass 产物命名空间，非
  benchmark 名条件，符合 AGENTS.md 红线精神）；或结构判定（entry 边来自
  reduce/vec 块）优先，命名兜底。
- 单测：runtime-bound elementwise 向量化成功（断言向量指令 + tail 存在 +
  幂等）；trip=0/1/2/3 边界（tail-only，不出向量但语义正确）。

**提交 3：runtime bound + B1 归约组合（真正解锁 4 case 内层）**
- apply：reduce 块算出的标量 sum 作为 tail 的 acc 初值；tail 内 acc 继续标量
  累加；tail 出口把最终 acc 传 exit（exit 需加 1 个标量参数 + 改写 exit 直接
  读 header acc 的引用——提交 1 的 exit 改写机制扩展）；
- passthrough 参数在 tail 链上继续转发（沿既有 epilogue 转发逻辑）。
- 单测：runtime-bound [iv, acc] 向量化成功（向量指令 + tail 标量累加 +
  幂等）。

**验证命令**（沿用 §10.9）：
- 单测：cargo test -p raana_ir 2>&1 | grep -E "test result"
- corpus 复扫：M44_TRACE=1 逐个跑 4 case，grep -oE "reject=[A-Za-z0-9_:]+" | sort | uniq -c
- IR 检查：./target/release/compiler -O2 --target aarch64 --emit ir -o /tmp/x.ir
- 汇编向量：grep -cE "addv|ldr q|str q|add v[0-9]|mul v[0-9]|dup v[0-9]" /tmp/x.s
- 定向差分（Docker 内，慢）：make test ARGS="-O 2 -j 1" perf/<case>.sy

**验收清单**：
- [x] 提交 1/2/3 各自独立 commit（[Feat(Opt)]: ...），中文消息
- [x] 单测 ≥3 新增全绿；raana_ir 全量全绿；workspace 全绿
- [x] 4 case 复扫：test_at_top_multi_param 与 test_at_top_bound_not_const
      计数下降（记录数字），解锁的循环汇编出向量指令（记录 case + 指令）
- [x] 01_mm1 内核（BB20）、many_mat k 循环（BB49）、transpose2（BB10）、
      conv2d checksum（BB14）至少解锁出向量
- [x] trip 边界（0/1/2/3）语义正确（差分或单测覆盖）
- [x] matmul1 -O2 仍 PASS；RISC-V 零影响；无二次向量化死循环
- [x] 最终 git status 只剩非本任务文件

**执行记录（2026-08-06 新 session，4 个 commit：da94fbe/6de3c59/92ff0ce/07025aa）**：

**基线复扫**（改动前，当前 HEAD 0108076）：
- conv2d-1：test_at_top_multi_param=8，test_at_top_bound_not_const=2
- many_mat_cal-1：multi_param=11，bound_not_const=9
- transpose2：multi_param=5，bound_not_const=0
- 01_mm1：multi_param=4，bound_not_const=0
- 合计 multi_param=28、bound_not_const=11。

**完成后复扫**（07025aa）：
- conv2d-1：multi_param=2、bound_not_const=0（checksum 转拒
  entry_preds_not_2——reduction_unroll 产物 3 前驱入口，保守拒绝正确；
  新增 b1_not_reducible=4 为 get_random 类 [Add, Rem] 形态）
- many_mat_cal-1：multi_param=0、bound_not_const=0、tail_loop_skip=3
  （elementwise runtime 循环 while_entry_17/25/31 全部向量化！）
- transpose2：multi_param=2、bound_not_const=0（BB10 转拒
  binary_operand_loop_variant——i²·a[i] 的 IV 标量操作数，既有规则不支持，
  非本任务范围）
- 01_mm1：multi_param=0、bound_not_const=0、tail_loop_skip=3
- 合计 multi_param=4（↓24）、bound_not_const=0（↓11）。

**出向量 case（NEON 精确匹配：dup v/addv/ld1/st1/ldr q/v\d+\.4s）**：
- 01_mm1：内核循环 4 条（ldr q18 + add v0.4s 累加 + addv 归约 + dup splat），
  汇编验证：header `cmp w6,#0; b.gt`（gt 计数器测试）、tail 标量 `ldr w5` +
  `add w0,w0,w5` 累加、exit 收尾，全链正确；
- many_mat_cal-1：17 条（elementwise runtime 循环）；
- matmul1（回归）：13 条不变；conv2d/transpose2 汇编 0 条（原因如上）。

**差分（Docker make test，-O 2 -j 1）**：
- 01_mm1 PASS（runtime bound=500 归约：counter 物化 + reduce 块数值正确）；
- vec_trip_0/3/5/7 PASS（临时 case，trip 0/3/5/7 = tail-only 0..3 与
  向量+余数混合，输出 0/6/15/28 与期望一致——tail 轮数语义正确）；
- many_mat_cal-1 PASS。

**过程中的实证发现（07025aa [Fix]）**：
1. 标量 BinaryOp::Max 后端不支持（codegen invariant failed）→ iv0 钳位改
   `select(gt(cnt0,0), cnt0, 0)`（csel）；
2. transpose2 归约循环的 post-exit 块（dominated by exit）直读 acc 参数
   （abs 计算）→ b1_acc_used_outside_latch 从"仅 exit 块"放宽为
   dom.dominates(exit, bb)，apply 将 post-exit 用户改写为 reduce 块 sum；
   新增 exit_region_guard 门（guard 边 + post-exit 用户组合拒绝）；
3. 孤儿 used_by（parent_bb=None）跳过，防 stale 误拒。

**未解锁项的形态结论**：
- conv2d checksum：reduction_unroll 多前驱入口（entry_preds_not_2）——需
  多入口支持（非本任务）；
- transpose2 BB10：IV 作标量操作数（i²·a[i]）——需 lane-index 向量支持
  （非本任务）；
- many_mat BB49 k 循环：B[k][j] 跨步访问（no_vector_ops）——需 gather
  或转置支持（非本任务）。

**收益评估（2026-08-06 复盘，用户质询"解锁的是否内层循环大头"）**：

**baseline 澄清**：baseline（0108076）并非零 SIMD——matmul1/2/3 已出向量
（const bound=1000 的 rotated 循环，matmul1 ≈13 条 NEON）。准确表述是
"除 matmul1/2/3 外 corpus 全零"。

**本次新增解锁的循环（都不是执行大头）**：
- 01_mm1 checksum（main 的 ans += B[i][j]，runtime bound=getint(n)）：
  4 条 NEON；工作量 n²≈10^6 次，而 mm() 的 10 次 O(n³) 调用 ≈10^10 次
  是绝对大头（占执行时间 ~99.99%）——mm 内核 j 循环未解锁；
- many_mat A/B 填 -1 初始化循环（store -1）：17 条 NEON；工作量
  T²≈10^6 次，而 k 循环 matmul（sum += C[i][k]*A[k][j]，T³≈10^9）是
  大头（~99.9%）——k 循环未解锁。

**两个大头的拒绝链（下一缺口的实证）**：
- 01_mm1 mm 内核 j 循环：rotated（test-at-bottom）+ runtime counter →
  entry_trip_not_const。本任务 runtime 支持只做 test-at-top；
  **rotated runtime-bound 是解锁 01_mm1 大头的直接缺口**，且该循环
  访问形态理想（C[i][j] 连续、A[i][k] 循环不变量、B[k][j] 连续）；
- many_mat k 循环：A[k][j] 行距 4KB 跨步 → no_vector_ops，需
  gather/转置（独立特性）。

**结论**：本次价值 = runtime-bound test-at-top 机制全链路验证正确
（counter 物化 + tail + reduce，差分 6 case 全 PASS + trip 边界语义），
以及 test_at_top_multi_param 28→4 / bound_not_const 11→0 的形态解锁；
但解锁循环均非热点，**实际性能收益≈0**。要拿性能，下一项应做
rotated runtime-bound（test-at-bottom 动态 counter），直接命中 01_mm1
mm 内核（矩阵乘法类 case 的共同缺口）。

---

### 10.13 自包含 goal 提示词（runtime-bound test-at-top 向量化，可直接粘贴新 session）

```text
# Goal: runtime-bound test-at-top 向量化支持（loop_vectorize）

## 背景
SysY 编译器项目（Rust），工作目录
/Users/azureskye/Documents/Programs/rust/AnonBeijingCompiler-hermes，
分支 feat/loop-vectorize-hermes。AArch64 NEON 向量化 pass 在
raana_ir/src/opt/passes/loop_vectorize.rs。60 例 perf corpus 只有
matmul1/2/3 出向量（bound 是字面常量 while(i<1000)）。最大形态缺口：
test-at-top 循环的 bound 是运行时值（getint / 运行时全局），被
test_at_top_bound_not_const 拒绝。背景实证见 TODO.md §10.10-10.12，
本提示词自包含。

## 必须遵守的规则（用户明令）
- 只在 hermes worktree 操作；不 push、不 rebase（遇冲突立即停并汇报）；
  不碰其他 worktree；不读/不提交 .env 等凭据文件。
- 只改 raana_ir crate；不碰 anon_armv8（lowering）、taki_mir、前端。
  IR 层问题只改 IR 层。
- 无 hacky workaround（rm、手动 sed、flat pool 一律禁止），只做 root
  cause 修复；设计中禁止 Rc<RefCell<T>> / RefCell 共享可变。
- 不做以 benchmark/函数名/输入为条件的优化（AGENTS.md 红线）；block
  命名（vec_tail_/vec_epi_/vec_reduce）是本 pass 产物命名空间，允许用于
  防二次向量化，不得用于任何优化条件。
- -O0 保留标量；vectorize 只进 aarch64 管线（RISC-V 零影响）。
- 勤 commit、勤单测：每个原子部分完成即 commit（消息格式
  [Feat(Opt)]: ... / [Fix(Opt)]: ... / [Docs]: ...），中文消息。
- 改文件一律 patch；不用 python 脚本改文件；不碰 .docker-image、
  不 rm -rf、不 cargo clean。
- 验收粒度 = 单测 + 单 case（make test <case>）；全量测试用户自己跑。
- 形态实证先行：任何分析改动前先用 M44_TRACE=1 + VECDBG_SHAPE=1 复扫
  4 个 case 确认当前拒绝分布，结论更新到 TODO.md §10.12。

## 现状与设计

### 现状（loop_vectorize.rs，行号以 bea35e2 后为准）
- test-at-top 形态：header 结尾 `lt iv, bound; br cond, latch, exit`，
  latch 是 plain jump 回 header。bound 必须是编译期常量
  （test_at_top_bound_not_const，约 936-938 行），否则拒绝；trip<VF
  （948-950）拒绝；exit 读 IV 且 trip%4!=0 拒绝（959-961）。
- apply：entry 边追加 counter 参数（初值 4Q 常量，约 1976/1990-2011），
  latch 步进 4，header 测 counter != 0；epilogue 剥 r=trip%4 个直线块
  （常量 IV 代入，1561-1641）；B1 归约（rotated）有 reduce 块
  （VectorReduce + seed 加回，约 1578+）。
- 已有单测：build_test_at_top（3155）、build_multi_param_test_at_top
  （3244）、vectorizes_multi_param_test_at_top（3402）、
  rejects_test_at_top_nonconstant_bound（3554）。
- 既有诊断插桩：M44_TRACE=1 打拒绝原因；VECDBG_SHAPE=1 打
  [SHAPE-HDR]/[SHAPE-BACK]/[SHAPE-BOUND]/[SHAPE-EXIT]。

### 设计（3 个原子提交，每个独立 commit + 单测）

**提交 1：const-bound test-at-top B1 单归约放行**
- 711-713 行 `test_at_top && effective.len() != 1`：改为 effective==2 时
  放行进 acc_info 匹配（复用 rotated 的 Reducible 判定，acc 必须是 header
  参数且非 passthrough）；其余仍拒绝。不引入多 IV/多归约。
- IV 识别：test_at_top 且有 acc_info 时，IV 槽取 effective 中非 acc 的槽
  （当前取 effective[0]，[acc, iv] 参数序会错选 acc 为 IV）。
- b1_acc_used_outside_latch（约 877+）：test_at_top 的 exit 无参数、经 SSA
  支配直接读 header 参数，是正常形态；放行 exit 块内读 acc（rotated 才
  是 latch branch f_args 传 acc'）。
- apply：test_at_top + reduction 时 header branch f 边 → reduce 块（带
  [acc 向量]）；exit 无参数时 reduce 块算出的标量 sum 需送达 exit（exit
  加参数或改写 exit 内对 acc 的引用，与提交 3 共用机制）。
- 单测：构造 [passthrough…, iv, acc] + const bound（如 bound=16）循环，
  断言向量指令 + 幂等；非 Reducible 的 effective==2 仍拒绝。

**提交 2：runtime counter 物化 + tail 循环（elementwise，无归约）**
- analyze：936-938 行 bound 非 const 不再拒，记 runtime_trip=true，
  bound_inst 存进 VecPlan（新字段）；948-950 跳过 trip<VF；959-961 若
  exit 读 IV，IvFinal = bound_inst（运行时值）。
- apply entry：插入 `trip = sub(bound, i0)` + `cnt0 = and(trip, -4)`，
  counter 初值 = cnt0（替代常量 4Q）。
- tail 循环：counter 归零边进入新块 tail_header（参数 [iv, passthrough…]）
  + tail_latch：`lt iv, bound`（复用原 bound_inst）→ br tail_latch/exit；
  payload 用 clone_payload_inst 克隆（IV 代入 tail 的 iv 参数）；iv 步进 1；
  entry iv0 = add(i0, cnt0)（运行时）。exit 参数按 exit_specs 重建。
- 防二次向量化：tail 是标量 runtime-bound 循环，fixed-point 下轮会再
  向量化 → 死循环。analyze 拒绝 header 名以 vec_tail_ 前缀的循环（结构
  判定优先：entry 边来自本 pass 产物块；命名兜底）。
- 单测：runtime-bound elementwise 向量化成功（向量指令 + tail 存在 +
  幂等）；trip=0/1/2/3 边界语义正确（tail-only）。

**提交 3：runtime bound + B1 归约组合（真正解锁 4 case 内层）**
- reduce 块标量 sum 作为 tail 的 acc 初值；tail 内 acc 标量累加；tail 出口
  最终 acc 传 exit（exit 加 1 个标量参数 + 改写 exit 直接读 header acc 的
  引用，复用提交 1 机制）；passthrough 沿 tail 链转发。
- 单测：runtime-bound [iv, acc] 向量化成功 + 幂等。

## 验证命令
- 单测：cargo test -p raana_ir 2>&1 | grep -E "test result"
- corpus 复扫（拒绝分布）：M44_TRACE=1 逐个跑
  ./target/release/compiler -O2 --target aarch64 -S tests/perf/<case>.sy
  2>/tmp/x.log，grep -oE "reject=[A-Za-z0-9_:]+" | sort | uniq -c
- IR 检查：./target/release/compiler -O2 --target aarch64 --emit ir
  -o /tmp/x.ir tests/perf/<case>.sy
- 汇编向量检查：grep -cE "addv|ldr q|str q|add v[0-9]|mul v[0-9]|dup v[0-9]"
  /tmp/x.s
- 定向差分（Docker 内，慢）：make test ARGS="-O 2 -j 1" perf/<case>.sy

## 关键代码位置
- loop_vectorize.rs analyze：693-701（passthrough/effective）、711-713
  （test_at_top_multi_param 拒绝点）、715+（acc_info 匹配）、774+
  （test_at_top IV 识别 effective[0]）、877+（b1_acc_used_outside_latch）、
  936-938（bound 检查）、948-950（trip<VF）、959-961（exit 读 IV）
- loop_vectorize.rs apply：1532-1546（counter 物化）、1561-1641（epilogue
  剥块 + clone_payload_inst）、1578+（reduce 块）、1931+（exit 边重写）、
  1976-2011（entry 4Q + acc splat）
- 工具函数：constant_i64（1482）、build_exit_args（2062）、
  vector_operand（2099）、clone_payload_inst（2128）、is_add_one（1373）
- 既有单测：3155/3244/3402/3554；VECDBG_SHAPE 插桩输出 SHAPE-BOUND

## 验收清单（全部满足才算完成）
- [ ] 3 个提交各自独立 commit（[Feat(Opt)]: ...），中文消息，每步单测全绿
- [ ] 形态实证更新 TODO.md §10.12（复扫分布 + 解锁链推进记录）
- [ ] 单测 ≥3 新增全绿；raana_ir 全量全绿；workspace 全绿
- [ ] 4 case（conv2d-1 / many_mat_cal-1 / transpose2 / 01_mm1）复扫：
      test_at_top_multi_param 与 test_at_top_bound_not_const 计数下降
      （记录数字）
- [ ] 解锁的循环汇编出向量指令（记录 case + 指令）：01_mm1 内核（BB20）、
      many_mat k 循环（BB49）、transpose2（BB10）、conv2d checksum（BB14）
      至少其一
- [ ] trip 边界（0/1/2/3）语义正确（单测或差分覆盖）
- [ ] 无二次向量化死循环（tail 被拒，fixed-point 收敛）
- [ ] matmul1 -O2 仍 PASS（既有能力不回归）；RISC-V 零影响（不注册）
- [ ] 最终 git status 只剩非本任务文件
```

### 10.14 自包含 goal 提示词（rotated runtime-bound 向量化，可直接粘贴新 session）

```text
# Goal: rotated（test-at-bottom）runtime-bound 向量化支持（loop_vectorize）

## 背景
§10.12 收益评估结论：test-at-top runtime 支持（da94fbe/6de3c59/92ff0ce/
07025aa）机制全链路正确，但解锁的循环（01_mm1 checksum、many_mat 初始化）
均非执行大头（占比 ~0.01%）。真正的大头——01_mm1 mm 内核、fft、h-* 系列
的计算循环——是 rotated（test-at-bottom）形态 + runtime counter，被
entry_trip_not_const 拒绝。本提示词解决 rotated runtime-bound，直接命中
矩阵乘法类 case 的热点。背景实证见 TODO.md §10.10-10.13，本提示词自包含。

## 实证数据（2026-08-06 扫描，rebase 后 HEAD 52d9f5f，含 main M 系列）
- corpus 60 例中 entry_trip_not_const 共 **63 个循环、12 个 case**
  （rebase 前基线 108/15——main 的 M 系列已消化 fft0/1/2（6→0）与
  h-5 系列（12→4）、h-8（4→3），h-10 不变（6）、01_mm1/2/3 不变（8））：
  01_mm1/2/3 各 8、h-5-01/02/03 各 4、h-10-01/02/03 各 6、h-8-01/02/03
  各 3。
- 01_mm1 mm 内核（目标 1）：rotated，header
  `[outer_i, j, k, counter]`，counter 来自 preheader 的 runtime 计算
  （rotate_loops 产物 `t0 = sub(bound, i0)`，如 01_mm1 `%53 = sub %49, 0`），
  payload `C[i][j] = C[i][j]*A[i][k] + B[k][j]`——C[i][j] 连续、A[i][k]
  循环不变量、B[k][j] 连续，**访问形态理想**，是 elementwise（非归约）。
  复扫确认：rebase 后 mm 内核仍被 entry_trip_not_const 拒绝（8 个不变）。
- h-10/h-8/h-5 剩余循环待首扫确认形态（归约/多参数/跨步），执行第一步
  先复扫 01_mm1 + h-10-01 的 rotated 循环清单。

## 必须遵守的规则（用户明令）
- 只在 hermes worktree 操作；不 push、不 rebase（遇冲突立即停并汇报）；
  不碰其他 worktree；不读/不提交 .env 等凭据文件。
- 只改 raana_ir crate；不碰 anon_armv8（lowering）、taki_mir、前端。
- 无 hacky workaround（rm、手动 sed、flat pool 一律禁止），只做 root
  cause 修复；设计中禁止 Rc<RefCell<T>> / RefCell 共享可变。
- 不做以 benchmark/函数名/输入为条件的优化（AGENTS.md 红线）；block
  命名（vec_tail_/vec_epi_/vec_reduce）是本 pass 产物命名空间，允许用于
  防二次向量化，不得用于任何优化条件。
- -O0 保留标量；vectorize 只进 aarch64 管线（RISC-V 零影响）。
- 勤 commit、勤单测：每个原子部分完成即 commit（消息格式
  [Feat(Opt)]: ... / [Fix(Opt)]: ... / [Docs]: ...），中文消息。
- 改文件一律 patch；不用 python 脚本改文件；不碰 .docker-image、
  不 rm -rf、不 cargo clean。
- 验收粒度 = 单测 + 单 case（make test <case>）；全量测试用户自己跑。
- 形态实证先行：任何分析改动前先用 M44_TRACE=1 复扫 01_mm1/fft0/h-5-01
  确认 rotated runtime 循环的当前拒绝分布与形态分类，结论更新
  TODO.md §10.14。

## 现状与设计

### 现状（loop_vectorize.rs，行号以 03a0862 后为准）
- rotated 形态：header 单条 plain jump → latch；latch
  `br t', header([iv', t', ...]), exit([acc', iv'...])`；t' = sub(t, 1)
  双职（既是条件也是 counter back-edge arg，非零=继续）。
- 拒绝点：analyze 1016 行 entry_trip_not_const（rotated 的 counter 必须
  编译期常量）；814-817 is_sub_one 检查（const 形态要求 t' = sub(t,1)）；
  1038 trip<VF。
- 既有 runtime 基建（全部在 test_at_top 分支内，rotated 需接入）：
  - 1725：trip = sub(bound, i0)、cnt0 = and(trip, -4)（entry 前插入）；
  - 1749：iv0 = i0 + select(gt(cnt0,0), cnt0, 0)（负 trip 钳位，标量
    select → csel，**不要用 Max**）；
  - 1774：exit 直读 header IV 参数改写（test-at-top 专用，rotated 无此
    问题——exit 收参数）；
  - 1805：counter 物化（test_at_top 追加 header 参数；**rotated 的
    counter 已是参数，只改 entry 值**）；
  - 1848-1859：tail 构造（vec_tail/vec_tail_latch，参数
    [iv_t, (acc_t), passthrough…]，payload 克隆 + iv 步进 1）；
  - 1931+：reduce 块（rotated B1 已有：latch f 边 → reduce → exit/epi）；
    1971 runtime 分支已把 reduce 接到 tail；
  - 2293+ 2c exit 边重写：rotated 分支（2304 else 侧）latch f 边 →
    reduce 块（B1）/ epi 链（r>0）/ exit（r==0, build_exit_args）；
  - 2406：counter 步进（rotated: eff_t_next = sub(counter, four)）；
  - 2412：four_q 常量（rotated: entry_args[counter_slot] = four_q）。
- 防二次向量化：analyze 开头 vec_tail 前缀检查对**所有**循环生效
  （tail_loop_skip），rotated tail 无需新机制。

### 设计（rotated runtime，复用 vs 新增）

**复用**（test-at-top runtime 基建，rotated 同样适用）：
- cnt0 = and(trip, -4)（rotated 的 trip = counter entry 的运行时值）；
- iv0 = i0 + select(gt(cnt0,0), cnt0, 0) 钳位；
- tail 构造（1859）：entry 边 [iv0, (acc_t), passthrough…]、lt 上界、
  exit 边重建、reduce runtime 分支（1971）；
- 防二次向量化（vec_tail 前缀，全局生效）。

**新增/改动**：
1. analyze 1016：rotated 的 entry_trip_not_const 放宽——counter 非 const
   记 runtime_trip=true（rotated 分支；注意 (trip, runtime_trip) 元组
   1006 行目前只有 test_at_top 走 runtime 分支，rotated 需并行处理）；
   1038 trip<VF 同样跳过；is_sub_one 检查（814-817）runtime 时放宽为
   sub(counter, 4) + gt(counter, 0) 条件形态。
2. tail 上界：rotated 无 bound_inst——bound_rt = add(i0_inst,
   counter_entry)（runtime，preheader 插入）；tail 的 lt iv_t, bound_rt。
3. apply counter（1805/2412）：rotated 不追加参数——entry_args[counter_slot]
   直接替换为 cnt0（runtime）/ four_q（const）；latch 条件 runtime 时改
   `br gt(counter, 0), header, exit`（const 保持 t' 非零测试不变，
   **行为不能变**——既有 rotated 单测依赖）；t' 步进 -4（2406 已有）。
4. 2c（2293+）：重写条件加 runtime_trip（现在
   `test_at_top || reduction.is_some() || r > 0 || has_iv_final` 对
   rotated runtime elementwise r==0 无归约不成立，会漏重写）；rotated
   runtime elementwise：latch f 边 → tail（[iv0, passthrough…]）；
   rotated runtime B1：latch f 边 → reduce 块（已有）→ reduce runtime
   分支 → tail（1971 已有）。
5. IvFinal：rotated 的 exit_specs IvFinal（收 iv'，A4）runtime 时
   iv_final_value = bound_rt（build_exit_args 2513 的入参；const 仍是
   i0+4q 常量）。
6. 负 trip：rotated 的 rotate_loops 产物 preheader 通常已有
   `gt(t0, 0)` 守卫（如 01_mm1 `br %54, preheader_33, while_end_18`），
   负 trip 直接走 exit；但 gt(counter,0) 测试 + iv0 钳位仍加（双保险，
   与 test-at-top 一致）。
7. 单测注意：rotated runtime 的 entry 是 guard branch 或 plain jump
   两种都要覆盖（01_mm1 mm 内核的 preheader 是 branch 守卫形态）。

**原子提交**：
- 提交 1：rotated runtime elementwise（01_mm1 mm 内核）——analyze 放宽
  + counter entry 替换 cnt0 + gt 测试 + bound_rt + tail 接入；
  单测：rotated runtime elementwise 向量化 + tail 存在 + 幂等 +
  trip 边界结构（cnt0=0 语义）；验收：01_mm1 汇编出向量 + 差分 PASS。
- 提交 2：rotated runtime + B1 归约（fft/h-* 归约形态）——reduce → tail
  链（1971 已备）；单测：rotated runtime [iv, acc] 向量化 + 幂等。
- 提交 3：性能实测落档（不一定是代码提交，可并入 [Docs]）——01_mm1
  qemu 运行时间 baseline vs 新（mm 内核占 ~99.99%，应有显著下降）。

## 验证命令
- 单测：cargo test -p raana_ir 2>&1 | grep -E "test result"
- corpus 复扫：M44_TRACE=1 逐个跑 15 个 case，
  grep -oE "reject=[A-Za-z0-9_:]+" | sort | uniq -c（记录
  entry_trip_not_const 下降数字，基线 108）
- IR 检查：./target/release/compiler -O2 --target aarch64 --emit ir
  -o /tmp/x.ir tests/perf/<case>.sy
- 汇编向量：grep -cE "dup v|addv|ld1|st1|ldr q| v[0-9]+\.4s" /tmp/x.s
- 定向差分：make test ARGS="-O 2 -j 1" perf/01_mm1.sy
- 性能对比：make test 的 r: 列（qemu 运行时间）与 03a0862 基线对比
  （01_mm1 基线 r≈4268ms，mm 内核解锁后应显著下降；记录数字）

## 关键代码位置
- loop_vectorize.rs analyze：1006（trip/runtime_trip 元组）、1016
  （entry_trip_not_const 放宽点）、1021+（runtime 门）、1038（trip<VF）、
  814-817（is_sub_one）、开头 tail_loop_skip（vec_tail 前缀，全局）
- loop_vectorize.rs apply：1725（trip/cnt0）、1749（iv0 钳位 select）、
  1774（exit iv 改写，test-at-top 专用）、1805（counter 物化，
  test_at_top 专用——rotated 只改 entry 值）、1848-1859（tail 构造）、
  1931+（reduce 块）、1971（reduce runtime → tail）、2293+（2c 重写，
  条件需加 runtime_trip）、2412（four_q）、**2429/2431/2459（entry 边
  counter 替换：2431 是 rotated 的 `t_args[entry_arg_count-1] = four_q`，
  runtime 时改 cnt0）**
- 工具：is_add_one（1509）、is_sub_one（1523）、constant_i64（1618）、
  apply_vectorize（1632）、build_exit_args（2513）、clone_payload_inst
  （2579）、subst_operand（remap_refs 统一替换）
- 既有单测：rotated 相关（vectorizes_reduction、vectorizes_exit_iv_final、
  rejects_non_innermost_loop 等）；VECDBG_SHAPE 插桩

## 验收清单（全部满足才算完成）
- [ ] 提交 1/2 各自独立 commit（[Feat(Opt)]: ...），中文消息，每步单测全绿
- [ ] 形态实证：15 case 复扫 entry_trip_not_const 计数下降（基线 108，
      记录数字）+ fft/h-* 循环形态分类，更新 TODO.md §10.14
- [ ] 单测 ≥3 新增全绿；raana_ir 全量全绿；workspace 全绿
- [ ] 01_mm1 mm 内核（BB 区，C[i][j]=C[i][j]*A[i][k]+B[k][j]）出向量
      （记录 case + NEON 指令）
- [ ] 01_mm1 -O2 差分 PASS；性能实测：qemu r: 时间较 03a0862 基线
      （≈4268ms）显著下降（记录数字）
- [ ] matmul1 -O2 仍 PASS（既有能力不回归）；RISC-V 零影响（不注册）
- [ ] 无二次向量化死循环（rotated tail 被 vec_tail 前缀拒，fixed-point
      收敛）
- [ ] trip 边界（0/1/2/3 + 负 trip）语义正确（单测或差分覆盖）
- [ ] 最终 git status 只剩非本任务文件
```
