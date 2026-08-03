# Cortex-A53 后端优化计划

本文档只记录尚未完成的工作。已完成里程碑只保留一行摘要，历史设计与实现细节
以 Git 提交记录和代码测试为准，不在这里重复维护。

> 进行中：§2 主计划 E——matmul 标量收敛（M49-M55 已完成），
> 以 `01_mm1` 为基线收敛内层循环到 gcc 标量水平（内层 ~10 条/element 含
> `subs;b.ne` + 索引折叠，零循环 `bl memset`）。SIMD Phase 2（M42-M46）
> 仍搁置，见 §3。

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
- **M54**：地址折叠 / 冗余消除——`MInst::Sxtw`（`mov xzr; add xzr` → `sxtw`）；
  内层 ~10 条/element、`[x,x,sxtw#2]` 折叠确认。functional 109/109、
  h_functional 40/40、RISC-V 109/109。
- **M55**：收尾与回归门禁——-O2 双 target 全量门禁通过（functional 109/109、
  h_functional 40/40、RISC-V 109/109、perf 4/4）；修复 count-up 旋转的 exit
  区域支配回归（sort 族 CE → 全绿）；`01_mm1` 内层保持 `subs;b.ne` + `madd`。
- **M53**：零 store 循环 → MemZero/memset——`MemZero` 动态长度支持 +
  `zero_store_loop` pass（AArch64）；`mm` 零 C 循环变 `bl .Lsoyo_memzero`。
  functional 109/109、h_functional 40/40、RISC-V 109/109。
- **M52**：count-up 循环旋转 + `subs` 融合——`rotate_count_up`（守卫 + 倒计时
  header param + exit 参数化重映射）；内层 j 循环变 `subs x,#1; b.ne`（~9
  条/element）。functional 109/109、h_functional 40/40、RISC-V 109/109。
- **M50**：PSR 触发（含 loop-invariant header 参数支持）+ LICM 同类修复——
  `C[i][j]`/`B[k][j]` 指针化、`A[i][k]` GEP 外提；内层 j 循环 ~20 → ~8
  条/element。functional 109/109、h_functional 40/40、RISC-V 109/109。
- **M49**：SSA 参数/指针 alloca 提升扩展（L1 根因）——`variable_analysis`
  放开为单机器字类型 + 逃逸检查；`mm` 参数栈重载消失、基址进寄存器。functional
  109/109、h_functional 40/40、RISC-V 109/109、双 target byte-identical。
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
- **M41（机器层部分）**：向量 ABI——`ArgLayoutPlanner` Vector bank、向量参数
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
- **M45 U1**：小常量精确 trip-count 循环全展开——支持正向/反向、非单位步进、
  zero-trip 与 loop-carried header 参数；保留最终失败 header visit，限制 8 次迭代/
  64 条非终结指令，复杂 CFG 保守拒绝；双 target QEMU 与 5 次确定性门禁通过。

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
SSA → Inline → TCO 之后，固定点内：IPSCCP、SimplifyCFG、LoopUnroll、RotateLoops、
LICM、GVN、SR（强度削减）、IfConversion、TCO、BooleanSimplification、GVNPRE、
DeadPhiElim、DCE。相关 pass 见 `raana_ir/src/opt/passes/`。

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
剩余的通用优化方向见 §4 后续候选工作；热循环标量优化（主计划 D，M47/M48）已完成，
SIMD 见 §3。

### 1.3 当前结论边界

- 静态 estimator 只用于确定性回归和相对启发式比较，不能替代实机测量。
- scheduler 与 estimator 共用同一模型，"模型内不退化"不等于"硬件不退化"。
- QEMU 仅用于语义差分，不能证明 A53 的 dual-issue / latency / throughput 收益。
- 未获得实机数据前，文档和提交信息只能声称"静态模型改进"，不能声称
  "XCZU15EG runtime 提升"。

### 1.4 实机超时排查（已归档）

结论（详见 git 提交 2ef0555）：无死循环；ARMv8.6 MOPS 探测已删除，输出 100%
ARMv8-A；TLE 根因是 IR 层优化缺口而非后端指令选择。其中 many_mat_cal 的 ~400x
差距已由主计划 D 的 M48 消除，其余见主计划 E（§2）/§3/§4。

---

## 2. 主计划 E：matmul 标量收敛（L1+L2，进行中）

以 `01_mm1`（1024 阶矩阵乘：`C[i][j] = C[i][j]*A[i][k] + B[k][j]` +
`A[i][k]==1` 短路）为基线，对照 gcc -O2 产物（`results/perf/01_mm1_gcc.s`）。
当前 `results/perf/01_mm1.s` 是过期 -O0 产物，仅用于量化差距；落地前先重建
-O2 基线（M49 前置，见 §2.4）。

### 2.1 现状差距（-O0 产物 vs gcc -O2）

| 项 | 当前产物 | gcc -O2 |
|----|----------|---------|
| `mm` 内层 j 循环 | ~20 条/element（每轮重算 3 个地址 + 3 次栈重载） | 标量 ~7 条/element、向量 ~1 条/element（`ld1r`+`mla` 8-wide） |
| 零 C 循环 | ~9 条/element 逐元素 `str` | `bl memset` |
| 循环测试 | `cmp` + `b.lt`（每轮独立比较） | `subs` + `b.ne` 融合 |
| 参数/基址 | 每轮 `ldr x,[sp]` 重载 | callee-saved 寄存器 + 递增指针 |
| 最终求和 | 标量累加、每行 `adrp` 重载 | `addv` SIMD 水平归约 |

### 2.2 根因定位（IR 层，非后端指令选择）

`raana_ir/src/opt/passes/ssa.rs:316` 的 `variable_analysis` 只收集
`ty.is_scalar()`（i32/f32，`ir/types.rs:131`）的 alloca 参与提升。`mm` 的参数
alloca（`%v_A/%v_B/%v_C`）类型是 `**[i32;1024]`（**指针**），被排除在 SSA
提升之外 → 参数驻留栈槽，内层循环每轮 `ldr x,[sp]` 重载；GEP base 定义在循环
体内 → 挡住 PSR（base 须 header 可用）与 LICM。这是标量差距的结构性原因。

### 2.3 目标与验收指标

- ✅ M49：`mm` 无 `alloc`/`store`/`load %v_*`；内层 j 循环无栈重载。
- ✅ M50：内层 j 循环收敛到 ~8-9 条/element（C/B 基址寄存器 + `A[i][k]`
  外提到 j 循环前 + `madd` + `str`）。
- M52 后循环测试变 `subs`+`b.ne`；M53 后零循环变 `bl memset`。
- 全量回归：functional/h_functional 149/149、perf 60/60、-O0/1/2 × 双 target
  5 次 byte-identical、RISC-V 全量不受影响。
- `scripts/perf_compare.sh 01_mm1` 静态指令数对照 gcc 记录到
  `results/perf_compare/`。

### 2.4 里程碑

#### M49：SSA 参数/指针 alloca 提升扩展（L1 根因修复）✅

- 状态：**已完成**。`variable_analysis`（`ssa.rs`）放开为单机器字类型
  （i32/f32/pointer）+ 新增 `alloca_does_not_escape` 逃逸检查（仅 `Load`/
  `Store` 使用才提升）。`mm` 参数 alloca（`**[i32;1024]`）全部提升，入口无
  `alloc`/`store %v_*`，内层 j 循环栈重载消失（基址进寄存器）。
- 验收通过：functional 109/109、h_functional 40/40、perf/01_mm1 PASS、
  RISC-V functional 109/109、-O0/1/2 × 双 target byte-identical 5×、
  新增单测 `promotes_pointer_slot_alloca`。详见 git 提交历史。

#### M50：PSR 触发验证与 cost model 校准（L1）✅

- 状态：**已完成**。`find_candidate` 命中外层 j 循环的 `C[i][j]`/`B[k][j]` 两组
  GEP（指针递减），并扩展 `available_at_header` + `apply_candidate` 支持
  **loop-invariant header 参数**（回边直通参数，preheader edge arg 代入初始
  指针）。cost model 无需校准（break-even ≈2 trips ≤ 4）。
- 同时修复 **LICM** 的同类问题：`solve` 识别直通 header 参数并在外提时用 entry
  arg 代入操作数 → `A[i][k]` GEP 外提到 j 循环前（load 因潜在别名保守留循环内）。
- 验收通过：内层 j 循环收敛到 **~8 条/element**（C/B 基址寄存器 + `madd` +
  `str`，`[x, w, sxtw #2]` 折叠）；`main` 求和循环无循环内 `adrp`；functional
  109/109、h_functional 40/40、perf/01_mm1 PASS、RISC-V 109/109、双 target
  byte-identical。新增单测：PSR `substitutes_a_passthrough_invariant_header_parameter_in_the_initial_pointer`、
  LICM `hoists_an_invariant_referencing_a_passthrough_header_parameter`。

#### M51：LICM 循环不变 load 核对（L1）✅

- 状态：**已完成（核对）**。M50 修复后 LICM 已能外提循环不变 GEP 基址
  （`main` 求和循环 base 进循环前寄存器，`[x1, w2, sxtw #2]` 索引 load）。
  关于"循环不变地址 load 外提"：`A[i][k]` load 地址循环不变，但循环内对 C
  有 store，无别名分析时无法证明不 alias → **保守不外提**（与既有
  `hoists_pure_binary_ops_but_not_memory_side_effects` 约束一致）。别名分析
  属 M42（SIMD 前置），后续再评估。无需新增代码。

#### M52：count-up 循环旋转 + `subs` 融合（L2a）✅

- 状态：**已完成**。`rotate_loops` 新增 `rotate_count_up`（与既有 countdown
  旋转并列）：识别 `br lt(i, bound), body, exit` + 回边 `add i, 1`；新增倒计时
  header param `t`（初值 `bound - i0`），preheader 加 `t > 0` 守卫（保 trip=0
  语义），latch 变 `subs t,#1; b.ne`。exit 若读 header 参数则参数化 + 用
  `remap_refs` 重映射（守卫/latch 分别传 entry/末次迭代值，SSA 支配正确）。
- 验收通过：内层 j 循环出现 `subs x,#1; b.ne`（~9 条/element，含 j 更新）；
  零 C 循环、i 循环、main 5 次 mm 循环均旋转；trip=0 守卫单测 + QEMU 差分
  （functional 109/109、h_functional 40/40、perf 01_mm1/01_mm2/01_mm3/
  huffman-01 PASS、RISC-V 109/109、双 target byte-identical）。新增单测：
  `rotates_count_up_loop_into_guarded_countdown`、
  `gives_the_exit_block_parameters_when_it_reads_the_induction_variable`。

#### M53：零 store 循环 → MemZero/memset（L2b）

- 文件：新增 `raana_ir/src/opt/passes/zero_store_loop.rs`（AArch64 注册，仿
  `chain_to_switch` 的 `PassesManager::aarch64` 分支）
- 改动：
  1. 识别内层循环体仅 `store 0, gep(base,(i,j))`、索引步进 1、trip 数可证 →
     内层转 `MemZero(row_ptr, n*4)`；两层皆零 → 合并 `MemZero(C, n*n*4)`；
  2. 后端已有 `lower_mem_zero` → `bl memset`
     （`anon_armv8/src/lower.rs:1032,1102`，`taki_mir/src/libcall.rs`）；
  3. 只转换纯内部循环，不破坏 `A[i][k]==1` 短路分支结构。
- 验收：`.s` 出现 `bl memset`；与逐元素结果 QEMU 差分一致；`-O0` 保留标量
  形式作 on/off 差分基线。

#### M53：零 store 循环 → MemZero/memset（L2b）✅

- 状态：**已完成**。`MemZero` 扩展支持**动态长度**（`MemZeroLen::Const/Value`，
  涉及 IR 类型、builder、inst_usage、remap_refs、DCE、aarch64/riscv lowering、
  runtime 链接、LLVM/fmt writer）；新增 `zero_store_loop` pass（AArch64 注册，
  在 `rotate_loops` 之后）：匹配倒计时零 store 循环（体 = 单条 `store 0` 到
  `gep(base, row_offsets, j)` + j/t 更新），collapse 为 `MemZero(C[i], n*4)`。
- 验收通过：`mm` 零 C 循环变 `bl .Lsoyo_memzero`（逐行 memset，同 gcc 形态）；
  functional 109/109、h_functional 40/40、perf 01_mm1/01_mm2/01_mm3 PASS、
  RISC-V 109/109、双 target byte-identical。新增单测：`converts_a_zeroing_countdown_loop_to_memzero`、
  `refuses_loops_that_store_a_nonzero_value`。

#### M54：地址折叠 / 冗余消除（L2c）✅

- 状态：**已完成**。消除 `mov xzr; add xzr, w, sxtw` 冗余对——新增
  `MInst::Sxtw`（emit/reg_uses/DCE/sched 全接入），GEP 非 1/2/4/8/16 stride
  索引的 32→64 符号扩展直接 `sxtw xd, wm`（preheader 每个基址省 1 条）。
  单动态索引折叠 `[x, x, sxtw #2]` 已确认工作（内层 j 循环 B/C 索引 load）；
  双索引 `i<<4096` 因 `lsl #12` 超 extended-reg 范围保持两段，未退化。
- 验收通过：`mm` 内层 j 循环 ~10 条/element（C 地址被 load+store 共用无法折
  叠，`subs;b.ne` 保持）；`mov xzr` 清零对全部消失；functional 109/109、
  h_functional 40/40、perf/01_mm1 PASS、RISC-V 109/109、双 target
  byte-identical。新增单测 `emits_sign_extension`。

#### M55：收尾与回归门禁 ✅

- 双 target × -O0/1/2 全量编译 + 5 次 byte-identical；
  functional/h_functional/perf 全量 QEMU 差分；on/off 差分无行为差异。
- `scripts/perf_compare.sh 01_mm1` 记录静态指令数。
- 新增单测：指针 alloca 提升、count-up 旋转、zero-store、`subs` 融合、PSR
  触发、exit 区域读取循环值时拒绝旋转。
- 修复 -O2 门禁暴露的旋转回归：`rotate_count_up` 的 guard 使 pre-header 直接
  流入 exit，exit 可达区域内（含 exit 之后的块）若直接使用 header 参数或循环内
  定义值，会失去支配定义（VCode SSA 校验失败）。修复为：exit 区域
  （`exit_region`，沿 successor 展开、遇循环块停止）内任何非 exit 块使用循环值
  即拒绝旋转；exit 自身仍可参数化；exit 读取非参数循环值亦拒绝。`29_break`
  （exit 有 break/continue 等额外前驱时拒绝参数化）与
  `58_sort_test4`/`16_k_smallest`/`20_sort`（swap 块读取 `min`/`j` 循环值）均修复。
  全量 -O0/1/2 × 双 target 通过；`01_mm1` 内层仍为 `subs;b.ne` + `madd`。

### 2.5 与 §3 SIMD 的交接

本计划把 matmul 收敛到标量基线（内层 ~8-9 条/element）。向量化
（§3 M42-M46，搁置）在标量收敛后启动：`C[i][j]=C[i][j]*a+B[k][j]`（a 为循环
不变）是 `ld1r`+`mla` 的天然形态，最终求和循环是 `addv` 的天然形态；M52 的
count-up 旋转与 M50 的指针形式是 M44 loop vectorizer 的 IV 前置。

### 2.6 本计划风险与缓解

| 风险 | 严重度 | 缓解 |
|------|--------|------|
| 指针 alloca 提升逃逸判定漏判导致语义错误 | 高 | 仅 `Load`/`Store` 使用才提升 + 全量差分 + on/off 差分 |
| PSR cost model 误判亏损而拒绝 | 中 | 按实际迭代量级校准阈值；多 GEP 收益核算单测 |
| count-up 旋转 trip=0/1 边界错误 | 中 | 头测试语义保留（入口非零证明）+ 专项单测 |
| LICM load 外提扩大化改变跨调用可见性 | 中 | 仅循环不变地址 + 白名单；宁漏勿错 |
| RISC-V 引入回归 | 中 | 新 pass 按 target 注册；双 target 回归 |

## 3. 主计划 C：SIMD/NEON 支持（M42-M46，搁置中）

### 3.1 Phase 1（M39-M41b）：机器层显式 NEON 通路（已完成）

向量类型 + `RegClass::Vector`、NEON MInst 全集、向量 ABI、向量 IR 入口 + 完整
ISel lowering、向量 SchedClass 已全部落地且验收通过，详见"已完成里程碑摘要"
M39-M41b。参考澄清：Cranelift 的 SIMD 是**显式降层**（wasm `v128` → NEON），
没有任何 loop unroll / vectorizer pass；clang/gcc 才做自动向量化
（loop vectorizer + SLP + unroll）。

### 3.2 Phase 2：IR 层自动向量化

> 搁置（2026-08 调整）：主计划 D（M47/M48）已完成；当前优先级为 §2 主计划 E
> 标量收敛，本节保持搁置。

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

#### M45：SLP 基本块向量化 + 循环部分展开

- SLP（`raana_ir/src/opt/passes/slp.rs`）：把同一基本块内相邻、类型一致的独立
  标量运算打包为向量 op（配对 add/mul/load/store 的菱形结构）；补 loop
  vectorizer 覆盖不到的直通代码（conv2d 邻域、展开后的短链）。
- U1 已完成：`raana_ir/src/opt/passes/loop_unroll.rs` 对常数精确 trip count（≤8，
  展开后非终结指令≤64）的规范双块循环全展开。
- 后续：非常数循环按 2-4 倍部分展开，为 SLP 提供相邻迭代、为 A53 双发射暴露
  ILP（与 post-RA ListScheduler + slot-filling 配合）；需先把 opt level/target policy
  传入 IR pass manager，避免在 `-O1` 固定点中反复展开。
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

`uika_riscv/src/abi.rs:176` 栈槽宽从 `ty.size()` 改为 `word_bytes()`（与 aarch64
一致），非对齐 131→0 处；QEMU 单测与 riscv functional+h_functional 全量通过。
根因链与修复细节见 git 提交历史；FPGA 实机复跑待验证（§4.9）。

---

## 附 B：RISC-V 跑分长耗时用例分析（已归档）

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

- A1 无条件跳转 `la t6,label; jr t6`（auipc+addi+jr=3 条）而非 `j`
  （jal x0=1 条）。每循环回边、每分支目标都付。数量：huffman 281、
  crypto 216、conv2d-1 111、many_mat_cal 80、03_sort1 78。BOOM 分支
  代价高，收益被放大。
- A2 不用立即数槽：`li 1; addw` → `addiw`；`li 1; subw` → `addiw -1`；
  `li 0x40; slt` → `slti`；`li 0; slt a,b` → `slt a,zero,b`。li 数量：
  huffman 337、crypto 240、conv2d 130、crc 122、many_mat_cal 83。
- A3 循环条件 `slt+beqz`（2 条）→ `blt`（1 条）。配合 A1 循环头实际
  8 条、理想 2 条。
- A4 地址强度削减不完整且不对称：
  - a. 内层元素地址每轮从 IV 重算（addw+slli+add）而非指针递增：sl2
    每轮 7 次邻域重算（42 行循环体 21 行地址运算）、many_mat_cal
    C[i][k]、01_mm2、h-10-03、shuffle1 value/nextvalue、transpose2
    j*colsize mul、matmul2。
  - b. 循环不变行基址在 k 循环内重算：matmul2 每轮 `mul i×4000`（i 在
    k 循环不变）、01_mm2/h-10-03 行基址。
  - c. 全局基址每轮 `la` 重载：matmul2 gv_c、01_mm2 gv_B、shuffle1
    gv_value+gv_nextvalue、h-10-03 gv_B。
  - d. SR 部分生效（many_mat_cal A[k][j] 已指针 +0x1000、transpose2
    i*rowsize 已提出 j 循环）→ pass 存在但匹配面有限，漏了 slli+add
    形状与全局基址。
- A5 `mulw+addw` 未融合 `maddw`（M 扩展）：many_mat_cal 矩阵乘内循环、
  transpose2 ans 循环。
- A6 累加器 phi 拷贝往返：many_mat_cal 平方和 `mv s2,s5; addw; mv s5,s2`、
  01_mm2 同。每轮 2 条 mv。
  - **根因（2026-08-04 已定位，regalloc 层，非 phi 拷贝问题）**：
    - IR 是规范 blockparam（%vid_9），Add lowering 干净 3 操作数（AluRRR 无
      constraint），MIR 的 addw 是 in-place（rd==rs1，merge 保证）。
    - 真正的机制：sum 的 blockparam merge 把全函数生命周期并成一个 bundle
      （dense v292），**但 sum 最后被 `putint(%vid_9)` 当调用参数**——lower_call
      （uika_riscv/src/lower.rs:986）把 arg 的 vreg 直接 pin 到 a0
      （reg_fixed_use，无独立 arg-copy vreg）。
    - compute_requirement（requirement.rs:149）把 bundle 内任意 fixed use 折叠成
      整条 bundle 的 FixedReg(a0) → sum 全程被钉死 a0 → 与同 range 内其他 call
      参数（getarray 地址 pp247 等）冲突 → 被迫 split 级联（bundle 292→301→
      304/307，split 于 pp246/253/278，见 RUST_LOG trace）→ 循环携带段 s5、
      循环体段 s2 分裂 → 每轮 2 条桥接 mv。
    - 对照：IV (a6) 短 range、无 call-arg use → 单寄存器零拷贝，证明机制本身
      没问题，是 fixed-use 毒化长 bundle。
  - **候选方案**：
    - A1（推荐，已细化 2026-08-04）：
      - **改动点**：uika_riscv/src/lower.rs `lower_call`（arg 循环约
        975-999 行，`put_value_in_reg` 之后、ArgSlot::Reg 分支）——判定为
        "长生命周期"的参数先 `ctx.emit(MInst::Mov { src: arg_reg, dst: fresh })`
        （fresh = `ctx.alloc_tmp(arg_ty)`），CallArgPair.vreg 用 fresh；
        fixed-a0 只落在 fresh 的微小 range 上。`lower_tail_call` 同步。
      - **判定规则（初版，数据驱动，实现后按 corpus 结果收敛）**：
        `arena.inst_data(arg).used_by().len() > 1 || arg 是 blockparam`
        （blockparam 集 = 所有非 entry block 的 params，LowerContext 初始化时
        建一次 HashSet）。依据：poison 源 v210/v235 均为多层 use（blockparam
        传参 + call 参数，used_by≥2）→ 规则命中 → 两条 fixed-a0 同时摘除 →
        合并 bundle 无 fixed use → 自由分配 → 循环内 0 拷贝。短生命周期参数
        （常量 remat、刚算的地址，used_by=1）不 copy → 保持现状（fixed
        机制直接落 a0，零/一 move）。
      - **预期收益/风险**：mmc1/mmc2/01_mm2 累加循环 2 mv/iter→0；crc 类
        （loop-carried 状态传内层 call）copy 恰好等于现在的 ABI move，无回归；
        huffman（常量参数）不命中，无变化。风险点：内层 call 的 blockparam
        参数若当前已直接落 a0（无 move），copy 会 +1 mv/iter——用 crc/
        huffman/fft1 的 .s diff 验证，若有回归则收紧规则（只 blockparam 或
        只多 use）。
      - **验证**：①mmc1/mmc2/01_mm2 循环 mv 计数（2→0）；②crc1-3/
        huffman-01-03/fft1 的 .s 指令数 diff（预期无回归）；③全 corpus .s
        指令数统计；④make test-riscv functional QEMU 差分；⑤RUST_LOG
        trace 复查 sum bundle requirement 变 Register、split 级联消失。
    - A2（allocator 侧）：compute_requirement/split 隔离 fixed use，不折叠整条
      bundle。动分配器核心，风险高，需 regalloc2 对照。
  - **验证计划**：mmc1/mmc2/01_mm2 内层循环 mv 计数（2→0）；make test-riscv
    functional 差分；全 corpus .s 指令数统计。
- A7 基址溢出到栈每轮重载：transpose2 matrix 基址 `ld 0(sp)` 每轮 2 次、
  fft1 数组基址每轮 1 次（寄存器压力导致 spill）。

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

