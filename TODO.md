# Cortex-A53 后端优化计划（M60-M63：fft0/fft1 递归模乘收敛）

本文档只保留 M60-M63 相关未完成工作。M1-M59 已完成里程碑、历史设计与实现细节以
Git 提交记录和代码测试为准，不在这里维护。

> 进行中：fft0/fft1 的**递归模乘瓶颈**（M60-M63）。**M60 已完成**：递归模乘改写
> + 后端内联扩展落地，fft0 静态 373 → 368（clang 359）、QEMU 14.93s → ~0.5s，
> -O2 双 target 全量回归通过。**M61 已完成**：过程间非负性分析与守卫消除
> （fft0 守卫保守保留，`soyo_mulmod` 纯化带来 main 中 `power` CSE）。**M62 受阻**：
> 蝶形循环常量物化因守卫 diamond 结构无法被保守 CSE 共享，需 MIR 块手术 hoist，
> 暂缓。SIMD Phase 2（M42-M46）仍搁置。

## 已完成背景（M56-M59，fft0 相关上下文）

- M56 纯函数调用 CSE：蝶形 `bl multiply` 3→2 次/轮（fft0/fft1 静态 698 → 642）。
- M57 自尾递归转循环（h-1 族 152 → 95）；M58 旋转循环 GEP 强度削减（h-5-01
  188 → 195 含内层地址 4→2 条/轮）；M59 `_and`/`_xor`/`_or` bit-test 前置
  （huffman-01 静态 890 → 862，QEMU ~60.9s → 31.7s）。
- 均已通过 -O2 双 target 全量回归；完整摘要见 Git 历史。

## 背景与根因（2026-08-04 定位）

- fft0 d=2^16，3 次 FFT 共 2^15×16×3 ≈ 1.57M 次蝶形；每轮 2 次
  `multiply(wn,y)` / `multiply(wn,w)`（蝶形循环见 `results/perf/fft0.s` 253-293
  行）。
- `multiply(a,b)` 是线性递归：b≈2^30 时 ~29 层，每层完整 ABI prologue/epilogue，
  顺序核上串行；合计 ~9e7 递归栈帧、~3G 动态指令。
- 语义上 `multiply(a,b)` 对 b≥0 恒等于 `(i64)a*b % P`（归纳可证：模对加法分配，
  `2x` 与 `+a` 均在模下闭合）。clang -O2 也保留递归（静态同构），冠军编译器把
  它识别为模乘改写为 `smull + i64 常数模`（~9 条指令，动态 ~500x）。
- 已排除项：`main` 尾缩放循环的 `power(d, P-2)` 已被 LICM 提升到 preheader
  （当前代码验证在 `preheader_7`，仅算一次），不是差距来源。

## 目标与验收指标（M60-M63）

- fft0/fft1 标量收敛：静态逼近 clang、动态消除递归模乘，QEMU 时间对照
  `results/bench.csv` 记录（fft0 14.93s → ~0.5s）。
- 门禁：`cargo test -p raana_ir`（316 全绿）；functional/h_functional `ARGS="-O 2"`
  双 target（AArch64 149+3 既有 CE、RISC-V 152/152）；`scripts/perf_compare.sh`
  静态无回归；QEMU 差分 + on/off 差分。

## IR 流水线上下文（M60 pass 注册位置）

IR 优化管线（`raana_ir/src/opt/pass.rs` 定点循环）：SSA → Specialize → **Inline**
→ TCO → ColumnMajor → GSP 之后，固定点内：IPSCCP、SimplifyCFG、LoopUnroll、
RotateLoops、ZeroStoreLoop（AArch64）、ChainToSwitch（AArch64）、LICM、GVN、PSR、
SR、InvariantReductionHoisting、ReductionUnroll、IfConversion、TCO、
TailRecursiveInline、BooleanSimplification、GVNPRE、DeadPhiElim、DCE。相关 pass
见 `raana_ir/src/opt/passes/`。**仅 AArch64 的 pass 必须用
`TargetPolicy.enable_chain_to_switch` 门控，否则 RISC-V 会回归。**

## M60：模乘递归识别与 64 位宽乘改写（核心，已完成 2026-08-05）

### 实现状态

**已实现**：

- `raana_ir/src/opt/passes/mulmod_recognize.rs`：识别 + 改写 +
  `MULMOD_HELPER="soyo_mulmod"`，含 6 个单测（识别、改写+声明 helper、拒绝杂散
  指令、负 b 语义=0、正 b 快路径逐值一致、守卫 cond 在 entry layout 的回归）。
- 注册：`raana_ir/src/opt/pass.rs` `PassesManager::aarch64` 的 `register_initial`
  （Specialize 后、Inline 前）；`raana_ir/src/opt/passes.rs` mod 声明；RISC-V
  不注册（`enable_chain_to_switch` 门控）。
- `anon_armv8/src/lower.rs`：`MULMOD_BUILTIN` + `lower_mulmod_builtin`，调用
  `soyo_mulmod` 扩为 `smull; sxtw; sdiv; msub`（32×32→64 精确乘法恒不溢出，
  b≥0 时与递归逐值一致），`HirFunction` 加入 import。
- 结果：fft0 静态 373 → 368（clang 359）；QEMU 14.93s → ~0.5s；`multiply` 汇编
  无 `bl multiply`。

**关键 bug 与修复（2026-08-05）**：

- 症状：改写后的 `multiply` 经 `Inline` 内联后程序被破坏——合入 fft0 的 ELF 在
  QEMU 下死循环；最小复现 IR 出现 `br %2, entry.., mulmod_fast..`（`%2` 未定义、
  真目标自环、循环体/累加器丢失）。
- 根因：rewrite 用 `new_local_inst()` 创建守卫 `cond`（Binary Lt）但**没有插入
  entry block 的 layout**，成为孤儿值。IPSCCP 只从 block layout 建 worklist，
  孤儿条件永远停留在"未访问"格值，分支两侧都不沿边 → 循环体/回边被当作不可达
  丢弃 → 终止循环被误判为无限循环。`zero`（Integer 常量）保持孤儿是正常的
  （Integer 从不在 layout 中，dump 内联显示为字面量）。
- 修复：`data.layout_mut().insert_inst(entry, cond)` 把守卫条件插入 entry（在
  branch 之前）。新增回归单测 `guard_condition_lives_in_the_entry_layout`。
- 另发现：functional/60_sort_test6、75_max_flow、85_long_code 的 `mem_zero` 动态
  byte_len panic 是 **HEAD 上已有的 release 构建问题**（DSE 对动态长度 MemZero 调
  `byte_len()`），与 M60 无关。

**快路径方案决策（2026-08-05 实测）**：

- 方案 A（multiply-high 魔术数）已完整实现（`signed_magic_i64` + `SMulH` MInst +
  `smulh; asr; add-round; msub` 序列，div_magic 单测 63 全绿、已知 P 魔术数逐值
  一致），但实测回归：静态 413（vs sdiv 368）、QEMU 1.38s（vs 0.46s）——因为每
  个内联展开点都在热循环内重新物化 64 位魔术数（4 条 movz/movk）+ 除数（2 条），
  QEMU 原生执行 `sdiv`。已回退为通用 `sdiv` 路径，待 M62 有循环不变常量 hoist
  后再启用魔术数。

### 识别规则

单函数 `f(a, b)`，body 严格匹配：
`b==0 → 0; b==1 → a % P; else cur = f(a, b/2); cur = (cur+cur) % P;
b 奇 ? (cur+a) % P : cur`；P 为编译期常量且 P>0。形状不匹配 / 多个递归调用 /
P 非常量一律不改写（宁漏勿错，白名单制）。注册时（post-Specialize）形状为
`eq %b,0` 守卫、`div %b,2` 折半、`rem %b,2` 奇偶；已兼容后续 SR 的
`shr;add;sar`/`and 0x80000001` 形式。

### 改写

`f(a,b) = b < 0 ? slow_path(保留原递归/转循环) : (i64)a*b % P`。正确性论证：
`smull x,a,b` 是 32×32→64 精确乘法，恒不溢出；b≥0 时 `(i64)a*b % P` 与递归
逐值一致（归纳），故守卫只需 `b < 0` 一个条件，无需范围证明即可保证全输入
正确；负 b 走慢路径保持原语义。

快路径后端支持（二选一，优先 A）：

- A：补 i64 常数除/模的 64 位 multiply-high 序列——`taki_mir/src/div_magic.rs`
  增加 `signed_magic_i64`，`anon_armv8/src/lower.rs` 新 lowering（对照 clang：
  `mov x8,#magic; movk; smulh; asr; add; mov w9; movk; msub` ≈ 8-10 条）。
  通用收益：任意 i64 常量除/模。
- B（若 IR 对 i64 `rem` 支持有缺口）：新增专用 `MulMod` MInst 直达 `smull` +
  i64 魔术数序列。

（现状：`soyo_mulmod` 由后端扩为 `smull; sxtw; sdiv; msub`。方案 A 魔术数已实现但
实测回归，暂用 `sdiv`，见上方"快路径方案决策"。）

### 验收

- `multiply` 汇编无 `bl multiply`；蝶形循环每轮 `smull`+`sdiv`+`msub`。
- 负 b（含 INT_MIN）、b∈[0,P)、b≥2^31 边界单测：快/慢路径与原递归逐值一致
  （IR 层单测 `guard_path_matches_negative_b_semantics` +
  `fast_path_matches_recursion_for_positive_b` + QEMU 差分）。
- fft0 QEMU 运行时间 14.93s → ~0.5s；perf_compare 静态 373 → 368。
- `cargo test -p raana_ir`（311 全绿）+ functional/h_functional -O2 双 target
  回归（AArch64 149 通过 + 3 个既有 CE；RISC-V 152/152）。

## M61：过程间非负性分析与守卫消除（已完成 2026-08-05）

**结果**：分析基建与守卫消除 pass 落地并全绿，但 fft0 的守卫**没有实际摘除**
（保守保留，见下）；附带收益是 `soyo_mulmod` 声明为纯函数后 main 中重复的
`power(5, (mod-1)/d)` 调用被 GVN CSE。

**已实现**：

- `raana_ir/src/opt/analysis_passes/return_summary.rs`（新）：
  - `nonneg_preserving_functions`：返回值摘要——纯函数的结果在"所有 i32 参数
    ≥ 0"时 ≥ 0（co-inductive fixpoint，`soyo_mulmod` 为基例，`multiply`/`power`
    经此继承）。
  - `always_nonneg_params`：调用点参数摘要——函数的哪个 i32 参数在**每个**调用点
    都 ≥ 0（greatest fixpoint）。
  - `nonneg_in_function`：函数内前向非负数据流（块参数取各入边 meets；仅处理
    Rem/Sar/Shr/And/Div-正常量/Select/soyo_mulmod/非负保持调用 + 符号折半链
    `sar(add(x,shr(x,31)),1)`）。
- `raana_ir/src/opt/passes/guard_elimination.rs`（新）：`br (x < 0), A, B` 在
  `x` 可证 ≥ 0 时折叠为 `jump B`；固定点内每轮程序级分析一次，逐函数折叠。
- `pure_function.rs`：`soyo_mulmod`（无 body 声明）特判为纯函数。
- `range.rs`：Call 值进入 `ValueDef::Call` + 调用摘要（`soyo_mulmod` 双操作数
  ≥ 0 时返回 [0,P)；非负保持 callee 全参 ≥ 0 时返回 [0,i32::MAX]）；entry 参数
  可注入非负（`nonneg_params`）；`range_of_fresh` 重推导已 settle 的 def。
- 单测：return_summary 4 + guard_elimination 1；`cargo test -p raana_ir` 316 全绿；
  -O2 双 target 全量回归通过（AArch64 149+3 既有 CE、RISC-V 152/152）。

**fft0 为何守卫未摘除（保守正确）**：可证链在以下处断裂——a) d 倍增循环用
`shl`（可回绕，保守不证）→ d 非负不可证 → main 的 `fft(..., power(5, (mod-1)/d))`
的 w 参数不可证 → fft 的 w 参数非"所有调用点 ≥ 0"；b) 蝶形 `multiply(wn, y)` 的
y 是数组元素（load），非负不可证 → 守卫正确保留。语义上这些值确实非负，但布尔
非负分析无法证明，需要真正的区间分析（M62 的常量 hoist 后或后续增强）。

**条件减法未做**：`(x ± p') % P → subs/csel` 需要证明 x ∈ [0,P)，数组元素不可证，
保守保留除法（与计划预期一致）。

**遗留**：RangeAnalysis 的 `solve` 对跨块 block-param 依赖的 Call 值会先记录
`full` 再 end-join（join(full,·)=full 无法纠正），导致 `range_of` 对这类值偏保守
（`range_of_fresh` 缓解）；不影响 PSR（其用 edge/before 状态）。

## M62：蝶形/点乘循环后端瘦身（静态对齐 clang）——**受阻，暂缓**

**现状（2026-08-05）**：静态 368 vs clang 359。蝶形循环体每轮 ~40 条（clang
LBB2_7 ~15 条），大头是**每个 modmul 站点各自物化常量**：`P`（0x3B8001301）6 处
+ 魔术数（0x1135C811）2 处 = 每轮 16 条 `movz/movk`，另加 3 个 `sdiv`。

**尝试与结论**：

- 已实现并回退一个 pre-RA 常量 CSE pass（`ConstCse`：跨块按支配关系共享
  `LoadImm`/`Sxtw`，含 VCode 边参数重写 + SSA verify 通过）。**它不生效**：
  每个 modmul 站点在 `b < 0` 守卫的**互斥分支**（diamond）里，任一站点的物化都
  不支配其他站点（零分支路径绕过它），保守支配规则正确拒绝共享。
- 真正要共享必须**插入**一个物化到支配整个 diamond 的循环头块（MIR 块手术：
  插入指令 + 重建 block_range），风险高；且 fft0 已在 368（clang 359）、QEMU
  ~0.5s，边际收益有限。暂缓，留待有 MIR 级 LICM/常量 hoist 基础设施后做。

**剩余项**（待 MIR hoist 基础设施）：
- 常数 `P` 物化 hoist 到循环头（支配 diamond），复用同一常量（配合 M60 决策：
  hoist 到位后再启用 M60 方案 A 的 multiply-high 魔术数路径）。
- 蝶形循环寄存器分配 / `mov` 搬运消除（对照 clang 的 LBB2_7 形状）。
- `memmove`（fft 每级拷贝 n 元素，逐元素标量循环）合并为 `ldp/stp` 对。
- 可选：蝶形循环 2× 部分展开，跨迭代重叠 `wn` 串行依赖链（配合 post-RA
  ListScheduler）。
- 验收：perf_compare 静态记录 ≤ clang 或注明剩余差距项；QEMU 差分 + 双 target
  回归。

## M63：fft0/fft1 收敛验收（2026-08-05）

**结果**：递归模乘瓶颈（动态大头）已消除，fft0/fft1 从"仍慢"清单移除。

- 实测（-O2 aarch64，harness）：fft0 静态 368（clang 359）、QEMU ~0.5s
  （bench 14.93s）；fft1 静态 368、QEMU ~1.9s，均 PASS。
- 门禁：`cargo test -p raana_ir` 316 全绿；functional/h_functional `ARGS="-O 2"`
  AArch64 149 通过 + 3 既有 CE（`mem_zero` release panic，HEAD 上已有）；
  RISC-V 152/152；QEMU 差分 PASS。
- 剩余静态差距（~40 vs clang ~15 条/轮蝶形循环）由 M62 跟踪（受阻，见上）；
  动态剩余为 QEMU/原生测量放大 + NEON（转 §SIMD）。

## 风险与缓解

| 风险 | 严重度 | 缓解 |
|------|--------|------|
| RISC-V 引入回归 | 中 | 新 pass 按 target 注册；双 target 回归 |
| mulmod 识别误匹配（把非模乘递归改写） | 高 | 严格 body 形状匹配（白名单）+ 全输入 QEMU 差分 + 逐值单测 |
| i64 常数模 magic 序列不精确（溢出/舍入） | 高 | 仅在 M62 重新启用魔术数时相关；对照 clang `smulh` 序列 + 边界单测 |
| 守卫 `b<0` 分支使快路径失速 | 低 | 单条件分支，预测器覆盖；M61 非负分析可消除可证处（fft0 因 d 循环 `shl` 保守保留） |
| 范围分析摘要污染调用方 | 中 | 摘要只对纯函数开放；宁漏勿错 |
| 改写产生孤儿守卫条件，被 IPSCCP 误判为不可达（M60 已修） | 高 | rewrite 把 `cond` 插入 entry layout + 回归单测；全量 QEMU 差分 |
| MIR 常量 hoist 破坏 VCode 边参数/block_range（M62 风险） | 高 | 暂缓不做；做时需 SSA verify + 双 target 全量回归 |

## 交接

fft0/fft1 动态瓶颈已收敛；剩余静态差距（M62，受阻）与 NEON（主计划 C，
M42-M46，搁置中）。
