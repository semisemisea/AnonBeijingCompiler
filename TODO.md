# Cortex-A53 后端优化计划（M68：NEON 自动向量化）

本文档只保留 M68 相关未完成工作。M1-M67 已完成里程碑、历史设计与实现细节以
Git 提交记录和代码测试为准，不在这里维护。

> 进行中：NEON 自动向量化（重启 M42-M46 SIMD Phase 2），覆盖 h-4/h-8/matmul。
> 注意：bench.csv 两列均为**官方真机**（XCZU15EG/Cortex-A53）时间，不是 QEMU；
> 且该表是较早一轮（v2）快照——fft0（表 11.71s，当前 QEMU ~0.5s）与 huffman
> （表 89.83s，M59 前状态）两行已不反映当前构建，其余 h-* 行仍是真实差距。

## 已完成背景（M56-M67，已压缩成一行摘要）

- M56 纯函数调用 CSE（蝶形 `multiply` 3→2 次/轮）。
- M57 自尾递归转循环（h-1 族 152→95）。
- M58 旋转循环 GEP 强度削减（h-5-01 内层地址 4→2 条/轮）。
- M59 `_and/_xor/_or` bit-test 前置（huffman 静态 890→862，QEMU ~60.9s→31.7s）。
- M60 递归模乘识别改写：`multiply(a,b)` → `b<0 ? 慢路径 : (i64)a*b % P`，后端扩为
  `smull;sxtw;sdiv;msub`（fft0 QEMU 14.93s→~0.5s，静态 373→368，clang 359）。
- M61 过程间非负性分析与守卫消除（`soyo_mulmod` 纯化 + main 中 `power` CSE）。
- M62 蝶形循环常量物化 hoist：**受阻暂缓**——`b<0` 守卫 diamond 里物化点互不
  支配，需 MIR 块手术（已被 M65 的循环 hoist 方案覆盖）。
- M63 fft0/fft1 收敛验收：静态 368（clang 359）、QEMU ~0.5s/~1.9s，-O2 双 target
  全绿（AArch64 149+3 既有 CE、RISC-V 152/152）。
- M64 条件减法模折叠（`mod_fold.rs`）：`x % P`（P 常量非 2 幂）在
  `RangeAnalysis::range_before` 证 `x ∈ [0,2P)` 时改写为 `Select(x>=P, x-P, x)`
  （`sub;cmp;csel`）；同时给 `transfer_rem` 增加被除数非负感知。**对 h-4/fft0
  不生效**（被除数可为负 / 数组元素不可证），作为通用优化保留，静态零回归，
  -O2 双 target 152/152。
- M65 热循环常量物化 hoist + CSE（`anon_armv8/src/passes/const_cse.rs`）：
  `LoadImm`/`MovFromZero` 从自然循环（唯一 preheader + 循环总指令数 ≤60）提升到
  preheader 并去重同值常量。h-4 内层循环 44→33 条/轮（12 条 `movz/movk` 出循环），
  QEMU 实测 h-4-01 -11%、fft0 -6%、huffman 持平；大循环（huffman decode >60 条）
  跳过避免寄存器压力。静态 66/87/189/552/373（h-1/h-4/h-5/huffman/fft0）。
- M66 h-1 循环瘦身：评估确认不变式 hoist/死函数已分别被 M65/GSP/DFE 吸收，
  残留奇路径 select 化收益 ~5%；h-1 主差距是**递归路径节点数**（纯代码生成难
  闭合，需算法级专项调研）。交付 ConstCse `MovZ`/`MovN` 常量化通用扩展，静态
  零回归，双 target 152/152。
- M67 寄存器分块（`raana_ir/src/opt/passes/blocked_reduction.rs`）：识别
  test-at-bottom 倒数式 msub 归约环 `acc -= A[i][k]*B[k][j]`（countdown ctr +
  列指针按常量步长推进 + 纯 pass-through 参数），改写为 4 路累加器 + 4 路列指针
  主循环（guard `bound>=4`，主循环 `bound&~3` 次，epilogue 重进原环
  `bound&3` 次）。h-5 两内环均命中；QEMU 实测 h-5-01 **7.16s→4.49s（-37%）**，
  输出与 .out 逐字节一致；静态 h-5 189→314（分块代码体积，动态收益主导）；仅
  h-5 命中（matmul/01_mm1 内环为 store-load 累加器形状，不匹配，宁漏勿错）。

## 背景与根因（bench.csv h-* 差距定位，2026-08-05）

h-* 均为官方真机时间，差距是**真实代码生成差距**，共性归纳为三类：

- **热循环内常量每站点重新物化**（`movz/movk` + magic 序列）：M62 已记录。
  h-1 主循环每轮 2 对 `movz/movk`（%1000000007 的 magic+除数）；h-4 内层循环
  （`/tmp` 实测 44 条/轮）有 **12 条循环不变常量物化**（3 个 magic + 3 个除数 +
  3 个 max 边界 + 1001 等）；huffman/fft0 同构。
- **分支/循环不变式未消除**：h-1 每外轮重载 `gv_lim`（`adrp+add+ldr`）；
  h-8 每轮 3 个可部分提升的 `if`；h-1 递归节点仍 ~8-12 条/节点。
- **无 NEON 自动向量化 + 无分块**：h-5 内层循环（`results/perf_compare/*/h-5-01.s`
  第 80-89 行）8 条只有 1 条 `msub`，`A[k][j]` 列访问（stride n）内存延迟主导；
  h-8 内层 k 循环本质是连续向量规约。冠军普遍 NEON 4-wide + cache/register
  blocking。
- 已排除项：
  - h-* 均为整数标量 workload，不涉 FP。
  - h-4 的常数除法已有 32 位 magic 强度削减（`anon_armv8/src/lower.rs:2149`）。
  - `% 常量` 条件减法折叠（M64）**对 h-4 不成立**：`sum+f(x)+1` 的被除数可为负
    （`f(x)` 的 `t3*3` 回绕后 `%19491001` 可为负，实测 h-4-01 首个用例 f(x)<0），
    折叠会改变截断语义，宁漏勿错（详见 M64 完结记录）。
- 静态基线（2026-08-05 perf_compare）：h-1-01 95、h-5-01 194、huffman-01 862、
  fft0 368（clang 359）；M67 落地后 h-1 66、h-4 87、h-5 314（M67 分块代码体积，
  动态 -37%）、huffman 566、fft0 373。

## 目标与验收指标（M68）

- 静态/动态：h-8 内层生成向量指令（`ld1/mul/add`），h-4/h-8 动态 2x 以上；QEMU
  时间对照并回写 `results/bench.csv`。
- 门禁：`cargo test`（raana_ir 323 + anon_armv8 126 + taki_mir 59 + uika_riscv 38）；
  functional/h_functional `ARGS="-O 2"` 双 target（AArch64 152/152、RISC-V
  152/152）；`scripts/perf_compare.sh` 静态无回归；QEMU 差分 + on/off 差分。
- 仅 AArch64 的 pass 用 `TargetPolicy` 门控（见 `config.rs`/`pass.rs`），否则
  RISC-V 会回归。

## IR/MIR 流水线上下文

IR 优化管线（`raana_ir/src/opt/pass.rs` 定点循环）：SSA → Specialize →
`mulmod_recognize`（AArch64）→ Inline → TCO → ColumnMajor → GSP 之后，固定点内：
IPSCCP、SimplifyCFG、LoopUnroll、RotateLoops、ZeroStoreLoop（AArch64）、
ChainToSwitch（AArch64）、LICM、GVN、PSR、SR、InvariantReductionHoisting、
ReductionUnroll、**BlockedReduction**（M67，`config.blocked_reduction` 门控）、
IfConversion、TCO、TailRecursiveInline、BooleanSimplification、GVNPRE、
DeadPhiElim、DCE。相关 pass 见 `raana_ir/src/opt/passes/`。
AArch64 MIR 管线（`anon_armv8/src/passes/mod.rs::build_pipeline`，pre-RA）：DCE →
PeepholeCombine → ChainFusion → **ConstCse**（M65）；post-RA：PairCombine →
ListScheduler。

## M64：`% P` 条件减法折叠（已完成 2026-08-05，通用优化，h-4 不生效）

### 实现

- 新 IR pass `opt/passes/mod_fold.rs`：匹配 `Rem(x, P)`（P 为 i32 常量，非 2 的
  幂——2 的幂已有 `And` 形式），用 `RangeAnalysis::range_before` 证
  `x ∈ [0, 2P)` 后改写为 `Select(x >= P, x - P, x)`（后端 `sub;cmp;csel`，替代
  4-6 条 magic 序列）。先廉价预扫描"函数内存在非常量幂的常量除数 Rem"再建
  RangeAnalysis，避免无候选函数白付分析成本。
- `transfer_rem` 增加被除数非负感知：非负被除数的截断余数 ∈ `[0, |d|)`（原先
  一律 `(-|d|, |d|)`），供后续链式折叠/PSR/守卫消除复用。
- 注册：固定点内、GuardElimination 之后（`raana_ir/src/opt/pass.rs`）。
- 单测：mod_fold 4 个（`[0,7]` 折 `%5`、`[0,31]` 不折 `%5`、2 的幂不折、逐值
  等价）+ range 1 个（非负被除数余数符号）。

### 关键结论：对 h-4 不生效（原验收前提不成立）

计划原假设 `sum+f(x)+1 ∈ [0, 2·mod)`。实测 `f(x) = (t3 + t3*3/1000*1001)
% 19491001` 的 `t3*3` 在 t3 大时**i32 回绕**，`/1000;*1001;%` 结果可为负
（h-4-01 首个用例 x=2147483646 时 f(x) ≈ -3843057），故被除数可为负，
`x >= P ? x-P : x` 会改变截断余数语义，宁漏勿错。fft0 蝶形 `(a + mulmod) % P`
的被除数含数组元素 load（M61 已记录"数组元素不可证"），同样不折。两处均为
**保守正确拒绝**。

### 验收

- h-4 静态无变化（不折）；h-1/h-5/huffman/fft0 静态与回归前一致（零回归）；
  `cargo test -p raana_ir` 321 全绿；-O2 双 target 152/152。
- 保留意义：对"操作数可证已约减的模运算"通用生效（隐藏 perf 用例可能命中），
  预扫描使其成本≈0。

### 后续（非本轮）

- 若要覆盖 fft0：需**内存范围推断**（数组由 `%P` 约减值写入 → load 继承
  `[0,P)`），超出 M64 范围。
- h-4 的真实杠杆转 M65（每轮 12 条循环不变常量物化）与 M68（NEON）。

## M65：热循环常量物化共享/hoist（已完成 2026-08-05，h-4 主杠杆落地）

### 实现

- 新 pre-RA MIR pass `anon_armv8/src/passes/const_cse.rs`（`ConstCse`，注册于
  ChainFusion 之后）：对每个带**唯一 preheader** 的自然循环（回边求 header + 反向
  可达 ∪ 各回边集合 + 支配剪枝），把循环内 `LoadImm`/`MovFromZero` 提升到
  preheader（前驱唯一且支配 header、不在环内）并去重同值常量（首现为 leader，
  其余 uses 重定向到 leader 的 vreg，含 `branch_block_args` 边参数重写）。
- **尺寸门控** `HOIST_BODY_LIMIT=60`：仅循环总指令数 ≤60 才 hoist，避免大循环
  （huffman decode ~130 条）因寄存器压力产生循环内 spill（实测无门控时 huffman
  静态 +95、帧 64→160 且热循环内出现 str/ldr）。
- VCode 基建：`VCodeContainer::set_insts_and_block_range`（块级重排原子更新）、
  `rewrite_branch_block_args`（边参数 vreg 重写）；taki_mir 重导出
  `vcode::Ranges`。
- CLI：`--enable-const-cse`/`--disable-const-cse`（A/B 差分用）。
- 单测：`const_key` 分类（LoadImm/MovFromZero 命中，MovZ/Sxtw 拒绝）。

### 关键结论与验证

- h-4 内层循环 44→33 条/轮（12 条 `movz/movk` 出循环，hoist 到内层 preheader，
  该 preheader 在外层 while(T) 内，执行 3 次而非 1.4e8 次）。
- QEMU 动态（QEMU 容器内 3 次取均）：h-4-01 2573→2284ms（-11%）、fft0 577→544ms
  （-6%，6×P 蝶形去重）、huffman 57.6s→58.1s（持平，decode 循环被门控跳过）。
- 静态（perf_compare）：h-1 66、h-4 87、h-5 189、huffman 552、fft0 373；相对
  M64 前有 +5~+42 的前言/寄存器重排开销，热循环无新增 spill（01_mm1/matmul 均
  验证）。
- 门禁：`cargo test` 544 全绿（raana_ir 321 + anon_armv8 126 + taki_mir 59 +
  uika_riscv 38）；functional/h_functional `ARGS="-O 2"` 双 target 152/152。

### 遗留（非本轮）

- 尺寸门控 60 是启发式；可用循环活值数/可用寄存器数做更精确的决策。
- `Sxtw` 等依赖输入的常量序列未 hoist（fft0 的 sxtw 每站点仍物化）。

## M66：h-1 循环瘦身（已完成 2026-08-05，收敛于 M65 通用化，剩余差距转专项调研）

### 评估与结论

- **不变式 hoist 已被 M65 吸收**：h-1-01 主循环的 `gv_lim` 重载（`adrp;add;ldr`）
  与 %1000000007 的 magic/除数物化已由 M65 ConstCse + GSP 消除（`lim` 进 w5
  寄存器、magic/除数进 w1/w2/w4 一次物化，静态 66）。
- **死函数**：独立 `fun`（无调用）已被既有 DeadFunctionElimination 移除，汇编中
  无 `fun` 符号。
- 残留开销实测：奇路径每节点 `madd c1;cmp;b.le;lsl;add;cmp;b.le;mov;b` 的
  双上限检查（可选 select 化，预期静态 66→~60、动态 ~5%）；偶路径
  `add w,w,w,lsr#31;asr` 的符号折半（需证明递归循环 `n≥0` 才可去）。两者收益均
  远低于 h-1 主差距。
- **主差距不可由代码生成闭合**：h-1 是 1e8 次 i × 每 i 一条可长至数百节点的
  递归路径（`3n+1`/`4n+1` 可增长），动态大头是**路径节点总数**。纯代码生成对
  每节点 ~8-12 条已接近下限，冠军 ~5s 需要把每 i 的访问数降一个数量级（只能靠
  递归 DAG 的跨 i 值共享/缓存，属算法级，且须符合
  `docs/Illegal_optimization.md` 的通用性要求）。

### 交付

- ConstCse 的 `const_key` 扩展覆盖单步 `MovZ`/`MovN`（完整常量，`movk` 单条为
  部分值仍拒绝），单测更新：`movz 1→(32,1)`、`movn 0x2<<16→(32,~(0x2<<16))`、
  `movk`/`sxtw` 拒绝。静态全语料零回归（h-1 66、h-4 87、huffman 551、fft0 373）。
- 门禁：`cargo test -p anon_armv8` 126 全绿；functional/h_functional
  `ARGS="-O 2"` 双 target 152/152。
- 专项调研项（候选，非本轮）：递归 DAG 值缓存 / 非负性证明去符号折半 /
  奇路径双候选 select 化——按 `docs/Illegal_optimization.md` 通用性标准设计。

## M67：O(n³) 内核寄存器分块（已完成 2026-08-05，h-5 -37% 动态）

### 实现

- 新 IR pass `raana_ir/src/opt/passes/blocked_reduction.rs`（`BlockedReduction`，
  注册于 ReductionUnroll 之后、`PassesConfig.blocked_reduction` 门控 +
  `--enable/disable-blocked-reduction` A/B 开关）。
- 匹配形状（宁漏勿错）：两块 test-at-bottom 倒数环，header 参数为
  `pt..., k, acc, ctr, ptr`——`acc_back = sub(acc, mul(load(A[i][k]), load(*ptr)))`、
  `k_back = add(k,1)`、`ctr_back = sub(ctr,1)` 且为分支条件、`ptr_back =
  get_elem_ptr(ptr, const_stride)`；其余参数必须纯 pass-through；body 无
  store/call；`ctr` 初值（bound）支配环入口。**不**匹配 store-load 累加器形状
  （01_mm1/matmul 内环），保留给 M68。
- 改写：guard `bound>=4`；主循环 `bound&~3` 次，4 路累加器 + 4 路列指针
  （`ptr_l` 初值 `base + l*stride`、每轮 `+4*stride`）；主循环退出按
  `ctr_final = bound - k_m`（= `bound&3`）**分支**到原 header（epilogue）或原
  exit——原环是 test-at-bottom，余数为 0 时直接跳原 exit，否则多跑一次会越界
  （修复的关键 bug）。
- 单测：bound=8 折叠为 4 块结构 + guard 分支；bound=2 保留 epilogue。

### 验证

- h-5 两内环（lower/upper）均命中；QEMU 动态：h-5-01 **7165ms→4486ms（-37%）**
  （4 路列 load 重叠 miss + 4 路独立 msub 断链）。
- 正确性：h-5-01 输出与 `.out` 逐字节一致（stdout 全等，仅 harness 的
  returncode 行差异）；自包含 ludcmp 小矩阵（n=6/n=11、含余数路径）与 -O0 参考
  逐值一致。
- 静态：h-5 189→314（分块代码体积 + guard + epilogue，动态收益主导）；仅 h-5
  命中，matmul/many_mat_cal/01_mm1 零变化。
- 门禁：`cargo test` 508 全绿（raana_ir 323 + anon_armv8 126 + taki_mir 59）；
  functional/h_functional `ARGS="-O 2"` 双 target 152/152。

### 遗留（非本轮）

- 顶层缓存分块（tiling）未做：寄存器分块已把列 miss 降 4x，tiling 再把数据复用
  限制进 L2，可与 M68 NEON 叠加。
- h-8（nussinov）控制流复杂（每轮 3 个 if），本 pass 不匹配，转 M68。

## M68：NEON 自动向量化（重启 M42-M46 SIMD Phase 2，最大收益）

### 现状

- 后端已具备完整向量 lowering（`anon_armv8/src/lower.rs`：`lower_vector_binary`/
  `vector_shape`/splat/select/reduce，NEON `v0-v31` 寄存器、ABI 向量参数），但 IR
  从不产生向量类型——无 SLP/loop 向量化 pass，全部标量。
- 理想对象：h-4（纯整数 4-wide 无访存，风险最低）、h-8 内层 k 循环（连续 load
  规约）、huffman 位运算、matmul/many_mat_cal（乘加）。

### 实现

- IR 层 loop 向量化（**仅 AArch64**，`TargetPolicy` 门控）：识别 trip-count 已知、
  无回环依赖、访存连续/无别名冲突的内层循环，改写为 `<4 x i32>` 向量 op +
  `VectorReduce` 收尾；splat/select/reduce 后端已支持。
- 顺序：① h-4 型纯算术循环（无访存）→ ② h-8 k 循环（连续 load）→ ③ 分块后的
  h-5（M67 落地后）。
- 风险：mask/尾部处理、指针别名、向量化 `%`/`/` 常数 magic 序列、`VectorReduce`
  归约顺序（i32 可交换则顺序无关）。

### 验收

- h-4/h-8 内层循环生成 `mul/add/ld1/st1`，静态表记录对照；QEMU 动态改进。
- RISC-V 不注册，`make test-riscv` 必须零回归；双 target 全量回归。

## 优先级与路线图

| 里程碑 | 目标用例 | 预期收益 | 工作量/风险 |
|--------|----------|----------|-------------|
| M64 条件减法模折叠 | 通用（h-4 不适用） | 无当前语料收益，通用保留 | 低/低（已完成 2026-08-05） |
| M65 常量物化共享 | h-4/h-1/huffman/fft0 | h-4 内层 -11 条/轮，QEMU -11% | 中/中（已完成 2026-08-05） |
| M66 h-1 瘦身 | h-1 | 交付 MovZ/MovN 扩展；主差距转专项调研 | 低/低（已完成 2026-08-05） |
| M67 寄存器分块 | h-5 | QEMU -37%，输出全等 | 大/中（已完成 2026-08-05） |
| M68 NEON 向量化 | h-4/h-8/matmul/huffman | 2-4x | 大/高 |

建议顺序：M68（向量化）为唯一剩余大项，优先于 h-5 顶层 tiling（可与 NEON 叠加）；
h-1 专项调研（递归 DAG 值缓存 / 非负性证明 / 奇路径 select 化）独立跟进。

## 风险与缓解

| 风险 | 严重度 | 缓解 |
|------|--------|------|
| 向量化指针别名 / mask 尾部 | 中 | 保守别名检查；尾部标量回退 |
| RISC-V 回归 | 中 | 新 pass 按 target 注册（`enable_chain_to_switch` 门控模式）；双 target 回归 |
| BlockedReduction 误匹配非 h-5 环（已落地，理论风险） | 中 | 形状白名单（pass-through 参数 + countdown + 常量 stride）+ 双 target 回归已过 |
| h-1 递归 DAG 值缓存改动语义 | 高 | 只做通用值共享（`docs/Illegal_optimization.md`），全输入 QEMU 差分 + 逐值单测 |

## 交接

M64-M67 已收敛 h-4 常量物化与模折叠、h-1 代码生成侧、h-5 寄存器分块（-37%）；
剩余最大差距在 h-8/matmul 的标量内存内核——M68（NEON 自动向量化，重启
M42-M46）是最大杠杆。所有静态目标以 `scripts/perf_compare.sh` 的 clang 对照列为
准，动态以官方真机/QEMU 差分双验。bench.csv 回写待 M68 落地后一次性更新。
