# Cortex-A53 后端优化计划

本文档只记录尚未完成的工作。已完成里程碑只保留一行摘要，历史设计与实现细节
以 Git 提交记录和代码测试为准，不在这里重复维护。

> 进行中：主计划 F——perf corpus 全面收敛（2026-08 起），以 gcc/clang -O2
> 汇编（`results/baseline/perf/`）与 `results/bench.csv` 评测为参照，逐用例
> 收敛热循环到 clang 水平。第一步 M56（纯函数调用 CSE）实现完成、回归门禁
> 进行中；后续 M57-M59 见 §2。SIMD Phase 2（M42-M46）仍搁置，见 §3。

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
- **M31-M38（主计划 A）**：huffman 类基准性能重构——if-conversion 推广、
  `CCmp`/`Subs`/`Ands`/`Tst` 标志融合、countdown 循环旋转、GSP 全局标量提升 +
  LICM + 作用域化 load-CSE、`Mov` 32 位宽度、内联代价模型、if 链 → switch 决策树、
  TCO 扩展 + 死空块清理。huffman-01 706 → 580。
- **M39-M41b（SIMD Phase 1）**：向量类型 + `RegClass::Vector`、NEON MInst 全集、
  向量 IR 入口 + 完整 ISel lowering、向量 ABI、向量 SchedClass，机器层显式 NEON
  通路全部落地。
- **M45 U1**：小常量精确 trip-count 循环全展开——支持正向/反向、非单位步进、
  zero-trip 与 loop-carried header 参数；限制 8 次迭代/64 条非终结指令。
- **M47（主计划 D）**：`reduction_unroll` 标量多累加器 + 4× 部分展开——单 BIV
  归约循环分裂 4 条独立 `madd`；many_mat_cal-1 平方和循环出现 4 条独立 `madd`。
- **M48（主计划 D）**：`invariant_reduction_hoisting` 外层不变归约外提——
  many_mat_cal R 循环整巢克隆到 preheader 跑一次得 `D_total`，外层退化为
  `acc += D_total`；qemu 下 ~82s → ~2s。
- **M49**：SSA 参数/指针 alloca 提升扩展（L1 根因）——`variable_analysis`
  放开为单机器字类型 + 逃逸检查；`mm` 参数栈重载消失、基址进寄存器。
- **M50**：PSR 触发（含 loop-invariant header 参数）+ LICM 同类修复——内层
  j 循环 ~20 → ~8 条/element。
- **M51**：LICM 循环不变 load 核对——`A[i][k]` load 无别名证明前保守不外提。
- **M52**：count-up 循环旋转 + `subs` 融合——内层 j 循环变 `subs x,#1; b.ne`。
- **M53**：零 store 循环 → MemZero/memset——`zero_store_loop` pass，零 C 循环
  变 `bl .Lsoyo_memzero`。
- **M54**：地址折叠 / 冗余消除——`MInst::Sxtw`，`mov xzr; add xzr` 对消失。
- **M55（主计划 E 收尾）**：双 target × -O0/1/2 全量门禁通过；修复 count-up 旋转
  的 exit 区域支配回归（sort 族 CE → 全绿）；`01_mm1` 内层保持 `subs;b.ne`+`madd`。
- **RISC-V 栈参数修复**：非对齐访问 + psABI widened-to-XLEN 槽宽（FPGA 实机
  复跑待验证，见 §4.9）。
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
SSA → Inline → TCO 之后，固定点内：IPSCCP、SimplifyCFG、LoopUnroll、RotateLoops、
ZeroStoreLoop（AArch64）、ChainToSwitch（AArch64）、LICM、GVN、PSR、SR、
InvariantReductionHoisting、ReductionUnroll、IfConversion、TCO、BooleanSimplification、
GVNPRE、DeadPhiElim、DCE。相关 pass 见 `raana_ir/src/opt/passes/`。

关键代码：

- `raana_ir/src/opt/pass.rs`：`Pass` / `PassesManager`（`aarch64()` vs `default()`）。
- `raana_ir/src/opt/passes/`：IR pass（`if_conversion`/`rotate_loops`/`licm`/
  `scalar_global_promotion`/`chain_to_switch`/`inline`/`simplify_cfg`/`tco`/
  `gvn`/`dce`/`pure_function` 等）。
- `raana_ir/src/opt/analysis_passes/`：`loop_analysis`/`induction_variable`/
  `dom_tree`/`cfg`/`pure_function`（向量化依赖，见 §3）。
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

### 1.2 现状与差距（2026-08 基线）

`results/bench.csv` 是旧提交二进制的评测结果（M49-M55 之前），many_mat_cal
~148s / matmul ~96s / huffman ~97s。当前代码在 QEMU 下已大幅改善，与
`results/baseline/perf/`（clang -O2）实测对比：

| 类别 | 用例 | 当前 vs clang |
|------|------|---------------|
| **已快于 clang** | many_mat_cal（~3.5x）、shuffle1、sl1、sl2、crc、crypto | — |
| **仍慢 1.4-1.5x** | h-1-01/02/03 | 尾递归未转循环（见 M57） |
| **仍慢 1.6x** | h-5-01/02/03 | 内层地址重算（见 M58） |
| **仍慢 1.2x** | huffman-01/02/03 | `_and` 循环微差（见 M59） |
| **仍慢 2x** | fft0/fft1 | 纯调用未 CSE（M56 已修复主要部分） |
| **仍慢 2-2.5x（SIMD 依赖）** | 01_mm、03_sort1、fft0 | clang 用 NEON，见 §3 |

静态指令数（ours vs clang -O2 基线，M56 之后）：
fft1 642 vs 359、crypto-1 966 vs 535、huffman-01 890 vs 604、h-1-01 152 vs 98、
01_mm1 215 vs 435、03_sort1 439 vs 479、h-5-01 188 vs 327、many_mat_cal-1
323 vs 365、transpose2 130 vs 196、conv2d-1 436 vs 694。

### 1.3 当前结论边界

- 静态 estimator 只用于确定性回归和相对启发式比较，不能替代实机测量。
- scheduler 与 estimator 共用同一模型，"模型内不退化"不等于"硬件不退化"。
- QEMU 仅用于语义差分，不能证明 A53 的 dual-issue / latency / throughput 收益。
- 未获得实机数据前，文档和提交信息只能声称"静态模型改进"，不能声称
  "XCZU15EG runtime 提升"。

### 1.4 实机超时排查（已归档）

结论（详见 git 提交 2ef0555）：无死循环；ARMv8.6 MOPS 探测已删除，输出 100%
ARMv8-A；TLE 根因是 IR 层优化缺口而非后端指令选择。其中 many_mat_cal 的 ~400x
差距已由主计划 D 的 M48 消除，其余见主计划 F（§2）/§3/§4。

---

## 2. 主计划 F：perf corpus 全面收敛（进行中）

以 `results/bench.csv` 评测与 `results/baseline/perf/`（gcc/clang -O2 汇编）为
双参照，把 perf corpus 中"仍慢于 clang"的用例逐项收敛到 clang 水平。SIMD 依赖
用例（01_mm / 03_sort1 / fft0）收敛到标量极限后转 §3。

### 2.1 根因汇总（2026-08-04 定位）

1. **纯函数调用未 CSE**（fft0/fft1 的 2x）：`fft` 蝶形循环里
   `multiply(wn, y)` 以相同参数出现两次（`arr[i]=(x+multiply(wn,y))%mod;`
   `arr[i+n/2]=(x-multiply(wn,y)+mod)%mod;`）。GVN 之前对所有 `Call` 一律
   `eliminable=false`，clang 则 CSE 掉第二次调用。蝶形循环每轮 `bl multiply`
   从 3 次降为 2 次（与 clang 同构）。→ **M56 已修复**。
2. **尾递归未转循环**（h-1-01/02/03 的 1.4-1.5x）：clang 把 `fun` 的自尾递归
   完全内联成 `main` 内的迭代循环（`tbnz`/`asr`/`cinc` 单循环，零调用零栈帧）；
   我们循环体内仍保留 `bl fun`。→ **M57**。
3. **内层地址强度削减不完整**（h-5-01/02/03 的 1.6x）：LU 分解内层循环每轮用
   `sxtw x; movz x,#0x15e0; madd x,x,x,base; add x,x,off` 重算 `A[k][j-1]`
   地址（~3 条/element 纯地址开销）；clang 用指针递增 `add x,x,x13` +
   `[x,#5600]` 常量偏移折叠。→ **M58**。
4. **`_and` 循环形态微差**（huffman-01/02/03 的 1.2x）：clang `_and` 每轮
   15 条（`cinc` 融合 `x&1` 与取负判号），我们 17 条（`add x,x,lsr#31;
   asr` 两步）。→ **M59**。
5. **SIMD 依赖**（01_mm / 03_sort1 / fft0 的 2-2.5x）：clang 用
   `mla v0.4s` / `sdiv` 向量化与 8-wide 展开，标量已到极限。→ §3（搁置）。

### 2.2 目标与验收指标

- 以 `01_mm` 为 matmul 标量基线（M55 已收敛），其余用例逐项逼近 clang 静态
  指令数与 QEMU 运行时间。
- 全量回归：functional/h_functional 149/149、perf 60/60、-O0/1/2 × 双 target
  5 次 byte-identical、RISC-V 全量不受影响。
- `scripts/perf_compare.sh` 对每个收敛用例记录静态指令数到 `results/perf_compare/`。

### 2.3 里程碑

#### M56：纯函数调用 CSE（实现完成，回归门禁进行中）

- 文件：
  - `raana_ir/src/opt/analysis_passes/pure_function.rs`：重写为程序级纯度分析
    `pure_functions(program) -> HashSet<Function>`——least fixpoint over 调用图，
    正确处理递归/互递归（fft1 的 `multiply` 自递归）；新增 `is_library_function`
    （11 个 runtime 入口）+ `locally_pure`（无全局 load/store/memzero/gep）；
    **声明无体函数保守视为非纯**（有 `entry_bb` 才可能纯）。
  - `raana_ir/src/opt/passes/gvn.rs`：`ValueKey` 新增 `Call { result_ty, callee,
    args: Vec<ValueNumber> }`；`number()` 对纯 callee 按 (callee, args) 编号并
    `eliminable=true`（可入 ScopedLeaders 做 CSE）；`ValueNumbering::new` 接收
    `pure_callees`。
  - `raana_ir/src/opt/passes/dce.rs`：`has_side_effect`/`is_critical` 改用纯度集，
    使 CSE 后无使用的死纯调用可被 DCE 清除（否则后端仍会发射）。
  - `raana_ir/src/ir/function.rs`：`Function` derive 增加 `PartialOrd, Ord`
    （`ValueKey` 排序需要）。
- 效果（静态指令数，M56 前 → 后）：fft1/fft0 698 → 642（`bl multiply` 25 → 21，
  蝶形循环 3 → 2 次/轮，与 clang 对齐）；01_mm1 266 → 215；03_sort1 467 → 439；
  h-5-01 201 → 188；transpose2 160 → 130；crc/crypto 均下降。
- 验收状态：`cargo test -p raana_ir` 235/235 通过；functional+h_functional
  152/152（-O0/1/2）、RISC-V 152/152（-O2）、perf 关键用例（huffman/h-1/h-5/
  fft/mm1/sort/crc/crypto/transpose2）全 PASS。**关键修复**：纯度分析原实现把
  "经指针参数读写内存" 视为不可见，导致 `QuickSort`/`exgcd`/`my_memset` 等经
  指针参数改内存的函数被误判为纯，其调用被 DCE 删除 → -O2 functional 11 个
  wrong answer；新增 `caller_visible_ptr`（GEP/Cast 溯源到全局或函数参数即非纯）
  后全绿。顺带修复 `zero_store_loop` 暴露的潜在 bug（`row_offsets` 为空时不得
  对既有 block 参数调用 `insert_before_terminator`，否则 block param 混入
  `insts()` 使 DCE 崩溃；空偏移时直接复用 base）。

#### M57：h-1 尾递归转循环（目标 1.4-1.5x → ~1x）

- 根因：`fun` 自尾递归（`return fun(n/2, dep+1)` 等三分支）在 `main` 循环体内
  被多次 `bl fun` 调用（含栈帧）；clang 完全内联成迭代循环。
- 方案（TCO / 内联层）：
  1. 识别"循环体内以相同形状调用某个纯自尾递归函数、且调用是各分支的尾位置"
     的形态；把该调用替换为对函数体副本的循环（`tbnz w,#0` / `asr` /
     `add w,w,#1` 单循环，零调用零帧）。
  2. 与既有 TCO（`tco.rs`）与 specialization（`specialize.rs`）衔接：先在
     `main` 内联 `fun` 一层后，识别剩余 `bl fun` 自调用并转回边。
  3. 守卫条件：仅当被调用函数是纯的、参数是标量、且调用点支配循环出口时转换；
     宁漏勿错。
- 验收：h-1-01/02/03 静态指令数 152 → ~100（对齐 clang）；`main` 循环内无
  `bl fun`；QEMU 时间 h-1-01 70s → ~48s；functional/h_functional/perf 全量
  回归 + RISC-V 回归 + byte-identical。
- 风险：误把非尾位置调用转循环 → 语义错误；仅转尾位置 + 全量差分 + on/off 差分。

#### M58：h-5 内层指针 SR（目标 1.6x → ~1x）

- 根因：LU 分解内层循环（`w -= A[i][k] * A[k][j-1]`）每轮
  `sxtw x25,w12; movz x26,#0x15e0; madd x25,x25,x26,x1; add x25,x25,w23,sxtw#2`
  重算 `A[k][j-1]` 地址；clang 用 `add x21,x21,x13`（步长 5600）指针递增 +
  `[x,#5600]` 折叠行内常量偏移，并 2× 展开 `ldp`。
- 方案：
  1. `pointer_strength_reduction.rs` 扩展匹配"基址 + k*stride + const_off"形状的
     GEP（stride 为编译期常量、k 为 IV），改为单条指针递增 + 常量偏移折叠。
  2. 依赖既有 count-up 旋转（M52）与循环旋转后的倒计时形态。
- 验收：内层循环 9 条 → ~5 条/element（`ldr w,[x,#5600]` + `msub` +
  `subs;b.ne`）；h-5 静态 188 → 与 clang 对齐；QEMU 11.7s → ~7s；全量回归。
- 风险：stride 非常量 / 多索引 GEP 误判；仅常量 stride + 单 IV 匹配。

#### M59：huffman `_and`/`_xor`/`_or` 循环微调（目标 1.2x → ~1x）

- 根因：clang `_and` 每轮 15 条，用 `cinc` 融合 `x&1`（对负数的
  `cmp x,#0; cinc x,x,lt`），我们 17 条（`add x,x,lsr#31; asr` 两步判号）。
- 方案：IR/MIR 层识别 `x<0 ? x&1 : x&1` 与 `(x+(x>>31))>>1` 等价形态，lowering
  选 `cmp;cinc`（或 IR 层规整为同一规范化形式供后端统一发射）。
- 验收：huffman 静态 890 → ~800；`_and` 循环 17 → 15 条；QEMU 110s → ~91s；
  全量回归。
- 风险：判号语义在 `INT_MIN` 边界的等价性；专项单测 + on/off 差分。

### 2.4 与 §3 SIMD 的交接

01_mm / 03_sort1 / fft0 标量收敛到 clang 标量水平后，启动 §3 向量化：
`C[i][j]=C[i][j]*a+B[k][j]`（a 循环不变）是 `ld1r`+`mla` 天然形态，最终求和是
`addv` 天然形态；M52 的 count-up 旋转与 M50 的指针形式是 M44 loop vectorizer
的 IV 前置。

### 2.5 本计划风险与缓解

| 风险 | 严重度 | 缓解 |
|------|--------|------|
| 纯度分析误判（CSE 语义错误） | 高 | 仅无体/无全局内存访问才纯 + 全量差分 + on/off 差分 |
| 尾递归转循环误转非尾位置 | 高 | 仅尾位置 + 纯函数守卫 + 全量差分 |
| PSR 常量 stride 误判 | 中 | 仅编译期常量 stride + 单 IV 匹配；宁漏勿错 |
| huffman 判号边界（INT_MIN） | 中 | 专项单测 + 等价性证明 + on/off 差分 |
| RISC-V 引入回归 | 中 | 新 pass 按 target 注册；双 target 回归 |

## 3. 主计划 C：SIMD/NEON 支持（M42-M46，搁置中）

### 3.1 Phase 1（M39-M41b）：机器层显式 NEON 通路（已完成）

向量类型 + `RegClass::Vector`、NEON MInst 全集、向量 ABI、向量 IR 入口 + 完整
ISel lowering、向量 SchedClass 已全部落地且验收通过，详见"已完成里程碑摘要"
M39-M41b。参考澄清：Cranelift 的 SIMD 是**显式降层**（wasm `v128` → NEON），
没有任何 loop unroll / vectorizer pass；clang/gcc 才做自动向量化
（loop vectorizer + SLP + unroll）。

### 3.2 Phase 2：IR 层自动向量化

> 搁置（2026-08 调整）：主计划 D（M47/M48）与主计划 E（M49-M55）已完成；当前
> 优先级为 §2 主计划 F 标量收敛，本节保持搁置。

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
