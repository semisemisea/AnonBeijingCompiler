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
