# Cortex-A53 后端优化计划（M69：NEON 自动向量化）

本文档只保留未完成工作：M69（NEON 自动向量化，原 M68 NEON 顺延）为进行中
主项。M1-M68 已完成里程碑以一行摘要维护（见下），历史设计与实现细节以 Git
提交和代码测试为准。

> 进行中：M69 NEON 自动向量化。M68（纯递归记忆化，h-1 主杠杆）已落地：
> h-1 族 QEMU 22.79s→6.5-8.4s（真机估算 ~1s < 2s 冠军线），152/152 双 target
> 全绿。
> 注意：bench.csv 两列均为**官方真机**（XCZU15EG/Cortex-A53）时间，不是 QEMU；
> 且是较早一轮（v2）快照——fft0（表 11.71s，当前 QEMU ~0.5s）与 huffman
> （表 89.83s，M59 前状态）两行已不反映当前构建；h-1-03 的 31.07s 为 M57 前
> （静态 152）状态，当前 h-1 静态 309（M68 fun_memo，动态 22.79s→6.5-8.4s）。
> 另：§4 主计划 F 的矩阵乘标量优化（M57/M58 matmul，与下方 M56-M68 的编号
> 不同轨）已落地——many_mat_cal-1 qemu 35.2s → 7.8s，见文末专节。

## 已完成里程碑（M56-M68，一行摘要）

- M56 纯函数调用 CSE（蝶形 `multiply` 3→2 次/轮）。
- M57 自尾递归转循环（h-1 族 152→95）。
- M58 旋转循环 GEP 强度削减（h-5-01 内层地址 4→2 条/轮）。
- M59 `_and/_xor/_or` bit-test 前置（huffman 静态 890→862，QEMU ~60.9s→31.7s）。
- M60 递归模乘识别改写：`multiply(a,b)` → `b<0 ? 慢路径 : (i64)a*b % P`，后端扩为
  `smull;sxtw;sdiv;msub`（fft0 QEMU 14.93s→~0.5s，静态 373→368，clang 359）。
- M61 过程间非负性分析与守卫消除（`soyo_mulmod` 纯化 + main 中 `power` CSE）。
- M62 蝶形循环常量物化 hoist：受阻暂缓（已被 M65 的循环 hoist 方案覆盖）。
- M63 fft0/fft1 收敛验收：静态 368、QEMU ~0.5s/~1.9s，-O2 双 target 全绿。
- M64 条件减法模折叠（`mod_fold.rs`）：`x % P` 证 `x ∈ [0,2P)` 时改写为
  `Select(x>=P, x-P, x)`；`transfer_rem` 非负被除数感知。h-4/fft0 不生效（被除数
  可为负/数组元素不可证），作为通用优化保留，静态零回归，双 target 152/152。
- M65 热循环常量物化 hoist + CSE（`anon_armv8/src/passes/const_cse.rs`）：
  `LoadImm`/`MovFromZero` 从自然循环（唯一 preheader + ≤60 条）提升到 preheader
  并去重。h-4 内层 44→33 条/轮，QEMU h-4-01 **-11%**、fft0 **-6%**、huffman 持平。
- M66 h-1 循环瘦身：收敛于 M65 通用化（`lim` 进寄存器、magic/除数一次物化，
  静态 66，比 clang 84 更紧凑）；交付 ConstCse `MovZ`/`MovN` 常量化扩展；主差距
  确认是**递归路径节点数**，转 M68 记忆化专项。
- M67 寄存器分块（`raana_ir/src/opt/passes/blocked_reduction.rs`）：识别
  test-at-bottom 倒数式 msub 归约环，改写为 4 路累加器 + 4 路列指针主循环
  （guard `bound>=4`，余数分支重进原环，test-at-bottom 修复）。h-5-01 QEMU
  **7.16s→4.49s（-37%）**，输出逐字节一致；仅 h-5 命中（宁漏勿错）。
- M68 纯递归记忆化（`raana_ir/src/opt/passes/recursive_memoize.rs` + AArch64
  `soyo_calloc` 内嵌分配器）：识别"纯自递归 + 加性累加参数 + 循环内单调用点"
  的结构，改写为按 K 查表的 `f_memo(K, C, cache, size)`（打包 `(val<<2)|tag`，
  运行时 `calloc` 缓存，OOB/宽度越界走原逻辑）。h-1 族 QEMU **22.79s→6.5-8.4s
  （3x）**，真机估算 ~1s（<2s 冠军线，clang 17.10s QEMU 被反超 2.6x）；静态
  66→309（fun_memo 体积）；`cargo test -p raana_ir` 330、双 target 152/152。

## h-1 差距定量（2026-08-05 复测，修正早期节点统计）

早期"153M 节点"是模拟 bug（降序遍历时奇节点的更大子节点尚未算出被当 0）。
真实量级（lim=50M，QEMU 多 lim 校准 139-224M 节点/s 稳定）：

- **非记忆化节点总数 31.6 亿**，平均链长 **63**、最长 **448**（Collatz 式单路径链）。
- QEMU -O2 实测（M68 前）：我们 **22.79s** / clang（`make test-baseline`）**17.10s**；
  M68 记忆化落地后我们 **6.5-8.4s**（真机估算 ~1s，clang 真机 ~2.57s）。
- **记忆化后工作量 8300 万**（50M 起点 + 33M 链步）→ **63x 削减**。
- 朴素记忆化 C（int16 缓存，100MB）QEMU **3.74s**、输出 199680725 全等 →
  真机 ~0.6-1.2s，已把 2s 目标甩开 2-4 倍。
- **冠军 <2s 只能是记忆化**：非记忆化 31.6 亿节点 × ~8 条 ≈ 250 亿条指令，A53
  @~1.2GHz、IPC 0.6-0.7 物理下限 ~2.5-3.5s；"最佳 2.57s"记录本身就必须是
  记忆化实现（250 亿条 / 2.57s ≈ 9.7 GIPS ≈ 6.5 IPC，物理不可能）。

### 两个关键结构洞见（让记忆化变简单）

1. **4n+1 分支运行时恒死**：4n+1 > 3n+1，若 3n+1 > lim 则 4n+1 必 >lim（实测
   B=0）→ `fun(n)` 是**单路径** Collatz 式链，每 n 至多一个孩子，缓存实现极简。
2. **屏障语义**：链撞到"奇 n 且 3n+1 > lim"返回常量 7 并**丢弃已累计深度**，屏障
   性沿链向上传播（祖先全为 7）。缓存必须区分"合法深度 7"与"屏障"，且深度可
   **>253**（max 448）——int8 缓存会溢出（实测踩坑：depth 254 存成 255 撞屏障
   哨兵），必须 int16+tag 或 16 位编码。

## M68：纯递归函数记忆化（已完成 2026-08-05）

### 落地结果

- h-1 族 QEMU **22.79s→6.5-8.4s（约 3x）**，真机估算 ~1s（<2s 冠军线达成）；
  clang -O2 QEMU 17.10s 被反超约 2.6x；h-1-01（lim=100M，400MB 缓存）~15s、
  h-1-02（lim=10M，40MB）~1.2s、h-1-03（lim=50M，200MB）~6.5-8.4s（QEMU 波动大）。
- 动态工作量 31.6 亿 → 8300 万（63x），静态 66→309（fun_memo 体积）。
- 门禁全绿：`cargo test`（raana_ir 330 / anon_armv8 126 / taki_mir 59 /
  uika_riscv 38）、functional+h_functional `ARGS="-O 2"` 双 target 152/152、
  perf_compare 非 h-1 语料静态零回归（h-4 87、h-5 314、fft0 373、huffman 564）。

### 实现要点（`raana_ir/src/opt/passes/recursive_memoize.rs`）

- 触发：纯自递归 + 加性累加参数（`C+const` 递归实参 / `ret C+k` / `ret C` /
  `ret 常量` 屏障）+ 循环内单调用点（键参数是前向 IV，`iv <= bound` 有界）。
  全部按结构识别，无函数名/输入值匹配（合规）；宁漏勿错。
- 改写：新函数 `f_memo(K, C, cache, size)`：entry 做 `1<=K<size` 边界 + 缓存
  probe，命中按 `tag==LEAF ? C+val : val` 返回；未命中走原函数体克隆（自递归
  重定向到 f_memo），每个 return 前做带守卫的缓存回填。残差/递增量从克隆入口
  的累加参数**结构化推导**（不线程化 C，减少跨调用存活值 → 少 3 个被调用方
  保存寄存器，prologue 从 7 降到 4 个 callee-saved，实测 QEMU 11.36s→6.5s）。
- 缓存：打包 `i32` 数组 `(val<<2)|tag`，`0`=空、`1`=叶残差、`2`=固定值；
  `val` 需 <2^30，运行时 `&0xC0000000==0` 检查，越界不缓存（正确性不受影响）。
  分配：AArch64 `anon_armv8/src/runtime/calloc.S` 内嵌 `.Lsoyo_calloc`（`uxtw`
  到 64 位后 tail-call glibc `calloc`），lower_call 重定向 + `needs_calloc`。
- 边界：OOB K、子节点越界、宽度越界全部退化为原逻辑（不缓存但值正确）；h-1
  中 4n+1 分支运行时恒死（B=0）是单路径链，但不依赖该事实。

### 剩余（Phase D，未做）

- QEMU 6.5-8.4s 仍未到 ≤4s 参照：命中路径仍是一次完整函数调用（~28 条）；
  若要逼近 C 的 3.74s，需把命中 probe 内联进调用方循环，或做迭代回填省递归。
 - 缓存内存：h-1-01 需 400MB（4 字节/条目）；int16+tag 打包可减半。
- `f_memo` 仍是非尾递归自调用（调用后做回填），tail_recursive_inline 不适用。

## M69：NEON 自动向量化（进行中，重启 M42-M46 SIMD Phase 2）

M68 记忆化已落地（h-1 唯一进 2s 路径）；NEON 覆盖 h-4/h-8/matmul。

### 现状

- 后端已具备完整向量 lowering（`anon_armv8/src/lower.rs`：`lower_vector_binary`/
  `vector_shape`/splat/select/reduce，NEON `v0-v31`、ABI 向量参数），但 IR 从不
  产生向量类型——无 SLP/loop 向量化 pass，全部标量。
- 理想对象：h-4（纯整数 4-wide 无访存，风险最低）、h-8 内层 k 循环（连续 load
  规约）、huffman 位运算、matmul/many_mat_cal（乘加）。

### 实现

- IR 层 loop 向量化（**仅 AArch64**，`TargetPolicy` 门控）：识别 trip-count 已知、
  无回环依赖、访存连续/无别名冲突的内层循环，改写为 `<4 x i32>` 向量 op +
  `VectorReduce` 收尾。
- 顺序：① h-4 型纯算术循环 → ② h-8 k 循环（连续 load）→ ③ 分块后的 h-5
  （M67 落地后）。
- 风险：mask/尾部处理、指针别名、向量化 `%`/`/` 常数 magic 序列、`VectorReduce`
  归约顺序。

### 验收

- h-4/h-8 内层循环生成 `mul/add/ld1/st1`，静态表记录对照；QEMU 动态改进。
- RISC-V 不注册，`make test-riscv` 必须零回归；双 target 全量回归。

## IR/MIR 流水线上下文

IR 优化管线（`raana_ir/src/opt/pass.rs`）：SSA → Specialize → `mulmod_recognize`
（AArch64）→ **Memoize**（M68 新增，initial、inline 之前）→ Inline → TCO →
ColumnMajor → GSP 之后，固定点内：IPSCCP、SimplifyCFG、LoopUnroll、RotateLoops、
ZeroStoreLoop（AArch64）、ChainToSwitch（AArch64）、LICM、GVN、PSR、SR、
InvariantReductionHoisting、ReductionUnroll、**MatmulInterchange**（§4 主计划 F
M58，AArch64 门控，`sr` 后、`invariant_reduction_hoisting` 前）、
BlockedReduction（M67）、IfConversion、TCO、TailRecursiveInline、
BooleanSimplification、GVNPRE、DeadPhiElim、DCE。相关 pass 见
`raana_ir/src/opt/passes/`。
AArch64 MIR 管线（`anon_armv8/src/passes/mod.rs::build_pipeline`，pre-RA）：DCE →
PeepholeCombine → ChainFusion → ConstCse（M65）；post-RA：PairCombine →
ListScheduler。

静态基线（2026-08-05 perf_compare）：h-1 309（M68 fun_memo，动态 3x）、h-4 87、
h-5 314（M67 分块代码体积，动态 -37%）、huffman 564、fft0 373（clang 359）；
clang h-1-03 84。

## 优先级与路线图

| 里程碑 | 目标用例 | 预期收益 | 工作量/风险 |
|--------|----------|----------|-------------|
| M69 NEON 向量化 | h-4/h-8/matmul/huffman | 2-4x | 大/高 |
| M68 Phase D 记忆化微调 | h-1 族 | QEMU 6.5s→≤4s（命中路径内联 / 缓存减半） | 中/中 |
| §4 主计划 F：M59 j 循环寄存器阻塞 | many_mat_cal | 1.5-2x | 中/中 |

建议顺序：M69（NEON）为下一大项；M68 Phase D（命中路径内联、int16+tag 缓存）
是 h-1 剩余的可选打磨，可与 NEON 穿插。§4 主计划 F（M57/M58 matmul 标量）已
落地 M57/M58，M59 可在 i-k-j 形状上独立做寄存器阻塞。bench.csv 回写待 M69
落地后一次性更新。

## 风险与缓解

| 风险 | 严重度 | 缓解 |
|------|--------|------|
| 向量化指针别名 / mask 尾部（M69） | 中 | 保守别名检查；尾部标量回退 |
| RISC-V 回归 | 中 | 新 pass 按 target 注册；双 target 回归 |
| M68 合规风险（函数名/输入值匹配） | 高 | 只按结构触发；缓存尺寸来自运行时 bound，不写死 |
| M68 缓存内存（h-1-01 400MB） | 中 | int16+tag 打包减半（Phase D） |
| M68 `soyo_calloc` 在 LLVM 路径未定义 | 低 | test-llvm 不在门禁；LLVM emitter 需声明或文档注明 |
| M58 就地改写正确性（Mout==M2 别名） | 中 | 行缓冲写回发生在 k 循环结束后，逐条结构论证（见 §4） |

## 交接

M64-M67 已收敛 h-4 常量物化与模折叠、h-1 代码生成侧、h-5 寄存器分块（-37%）。
M68 纯递归记忆化已落地：h-1 族 QEMU 22.79s→6.5-8.4s（真机估算 ~1s，冠军线
<2s 达成），动态工作量 63x，双 target 152/152，非 h-1 静态零回归。最大差距
转为 M69 NEON（h-4/h-8/matmul）。§4 主计划 F 的矩阵乘标量优化（M57/M58）也已
落地（many_mat_cal-1 qemu 35.2s → 7.8s）。所有静态目标以 `scripts/perf_compare.sh`
的 clang 对照列为准，动态以官方真机/QEMU 差分双验。

---

# 附录：§4 主计划 F —— 矩阵乘热循环标量优化（M57-M61）

> 说明：本节的 M57-M61 与上文 M56-M69 是**两条独立优化线**，编号并行
> （本节 M57=reduction_unroll 推广、M58=matmul 循环交换；上文 M57=自尾递归
> 转循环、M58=旋转循环 GEP 强度削减，均已完成）。本节记录 many_mat_cal 矩阵
> 乘标量优化计划，M57/M58 已落地，M59-M61 待做。

## 背景与量化

以 `many_mat_cal-1`（输入 `1024 15000`，T=1024，R=15000）为准分析：

- 计时区间内工作量：
  - 元素级循环（-1 填充 ×2、`C=A*2+B*3`、`C=(C²+7)/3`）：各 ~1M，合计 ~4M 次
    → **可忽略**；
  - 矩阵乘 `A[i][j] = Σ_k C[i][k]*A[k][j]`：**T³ ≈ 1.07e9 MACs → 绝对主导**；
  - R×T² 平方和（1.57e10 次）：**已被 M48 外提消除**（R 循环退化为平凡
    `acc += D_total`）。
- 当前瓶颈 = 矩阵乘内层 k 循环，两大致命点：
  1. **缓存不友好**：内层访问 `A[k][j]` 列步长 4096B（一行 1024 int），每次
     load 基本必 miss → 实测 ~100-200 周期/迭代，1.07e9 次 ≈ 100-150s，与
     bench.csv 149.56s 吻合；
  2. **串行累加链**：单累加器 `%vid_12` 的 `madd` 链，A53 上 latency-bound，
     且 `reduction_unroll`（要求恰好 2 个 header 参数）**不匹配** 3 参数循环
     （k, acc, ptr）——现有 M47 没作用到矩阵乘内层。
- 目标：标量手段把 many_mat_cal 压到约 1-3s（qemu 参考），与冠军 0.28s 的
  残余差距交给 SIMD（上文 M69 NEON，本计划不做）。

## M57：`reduction_unroll` 推广——指针携带型归约循环（✅ 已完成 2026-08-05）

> 状态：已实现并落地。`reduction_unroll.rs` 泛化到 3 参数，识别指针 IV 的常量
> GEP 步进；主循环 4 个独立累加器 + 4 路指针 lane（`base + {0,stride,2s,3s}`），
> 指针回边推进 `4*stride`。矩阵乘内层从单条串行 `madd` 变为 4 条独立 `madd`。
> 验收：many_mat_cal-1 qemu 计时 ~25s → ~8.6s；functional/h_functional 全量
> -O2 152 通过（0 fail/CE/RE/TLE）；RISC-V -O2 全量回归通过；`cargo test -p
> raana_ir` 301 全绿（含新增 `unrolls_a_pointer_carrying_reduction_loop` 单测）。
> 历史方案细节见 Git 提交。

- **现状缺口**：`reduction_unroll.rs` 要求 header 参数恰好 2 个
  （acc + trip IV）；矩阵乘内层是 3 个（`%vid_11=k`, `%vid_12=acc`,
  `%257=ptr`），ptr 每轮 `getelemptr %257, 1024` 步进（一个元素步长 1024 个
  元素 = 一行）。
- **变换**：放宽到 3 参数——识别「trip IV 步进 1 + 纯体 + acc±E + 指针 IV
  按常量 GEP 偏移步进」。主循环用 4 个独立累加器 + 4× 展开，ptr 每轮步进
  4×1024，4 个 lane 的指针 = `ptr + {0,1024,2048,3072}`；原循环保留为标量
  epilogue 收 `T%4` 余数。版本化守卫 `T>=4` 与 M47 相同。
- **正确性**：体纯（仅 load，无 store/call/memzero）→ 克隆无副作用；加法模
  2³² 结合/交换（与 M47 同论据）；指针 lane 化是纯地址代数，不影响访存语义。
- **文件**：`raana_ir/src/opt/passes/reduction_unroll.rs`（泛化 find_candidate
  与 LaneMapper）。
- **收益**：内层 madd 依赖链 4 路并行，约 **2-4×**；但不解决列访问 miss
  （见 M58）。

## M58：矩阵乘循环交换 i-j-k → i-k-j + 输出行缓冲（✅ 已完成 2026-08-05）

> 状态：已实现并落地。`matmul_interchange.rs` 识别 i-j-k GEMM 巢（内层为
> 3 参数指针携带型归约 `acc += C[i][k] * A[k][j]`，j 循环含 `store acc →
> A[i][j]`），改写为 i-k-j + 栈上输出行缓冲（`alloc`+`memzero`，每次 i 清零），
> 并生成写回循环 `A[i][j] = buf[j]`。内层从「A 列访问（4KB 步长 miss）」变为
> `A[k][j]` 行连续访问，`cik = C[i][k]` 外提到 j 循环外。
> 验收：many_mat_cal-1（T=1024，R=15000）qemu 计时 35.2s → 7.8s（约 4.5×）；
> functional 112/112、h_functional 40/40 -O2 通过；RISC-V -O2 回归通过；
> `cargo test -p raana_ir` 303 全绿（含 matmul_interchange 2 个单测，fixture
> 复刻真实基准「i 值经 j_header 参数穿通」的形状）。

- **模式识别**（通用 GEMM 模式，非按用例名匹配，合规）：外层 i 循环 → 中层
  j 循环 → 内层 k 归约循环：`acc += M1[i][k] * M2[k][j]`，随后
  `store acc → Mout[i][j]`；其中 M1/M2 为 2D 数组（全局或参数，3-offset GEP，
  行主序），内层循环体纯（仅 load）。对 `Mout == M2` 的**就地**形态单独支持
  （见下"就地正确性"）。
- **变换**：
  ```
  for i:  for j:  A[i][j] = Σ_k C[i][k] * A[k][j]
  ──改写为──
  for i:
    acc[0..T) = 0                    // 输出行缓冲（栈上 T×4B）
    for k:
      cik = C[i][k]                  // 提升出 j 循环：每个 (i,k) 只 load 1 次
      for j:  acc[j] += cik * A[k][j]  // A 行连续访问（L1/L2 友好）
    for j:  A[i][j] = acc[j]         // 整行连续写
  ```
- **就地正确性论证**：原语义 `A[i][j]` 在 k 循环结束后才写。处理行 i 时：
  - `k < i` 读到的是**新**值（前面 i 迭代已整行写完）；改写后同样读到新值 ✓
  - `k == i` 读到的是**旧**行 i（本 i 尚未写）；改写后行缓冲在 k 循环结束时
    才写回 A，读旧值 ✓
  - `k > i` 读到旧值 ✓
  - 且每个 (i,j) 的乘积累加顺序与原循环完全一致（外层 k、内层 j），无重结合
    → i32 溢出语义逐位一致 ✓
  - 结论：i-k-j + 行缓冲**语义保持**，无需别名拒绝。若 `Mout` 与 M1/M2 无
    别名（非就地）则更可直接交换，无需缓冲。
- **保守规则**：内层体出现 store/call/memzero，或 M1/M2 中任一个非 2D 仿射
  索引（动态下标/未知 base）→ 拒绝。判不准一律不交换（宁漏勿错）。
- **实现要点**：
  - i IV 通过 `crow = gep C (0,i)` 的结构化第二偏移提取（BIV 被 i-loop 回边的
    `BlockArgRef` 形态破坏）；循环边界用 `forward_strict_bound` 从 header 分支
    的 `iv < bound` 比较提取，要求 i/j/k 三个循环边界一致。
  - 真实基准中 i 值经 j_header 参数穿通（i-latch 的 `i = add <j-header 参数> ,1`）。
    移除旧 j/k 块后该操作数悬空——必须把悬空的 `BlockArgRef` 操作数重写为
    `i_iv`，否则 IPSCCP 把 i-loop IV 折叠为常量 0 → 循环条件恒真 → 无限循环。
    单测 fixture 复刻此穿通形状回归。
  - 旧 j/k 块（j_header/j_body/j_latch/k_header/k_body）删除 + i-latch 回边
    悬空参数按 i-header 参数位置重接。
- **文件**：`raana_ir/src/opt/passes/matmul_interchange.rs`；复用
  `preheader`/`CFG`/`LoopAnalysis`/`induction_variable`/`remap`（与
  M47/M48 同套基建）。注册：`sr` 之后、`invariant_reduction_hoisting` 之前
  （交换后先见 pristine i-j-k 巢；后续 LICM/GVN 再优化新内层）。
- **验收**：`--emit ir` 检查 many_mat_cal 矩阵乘变为 i-k-j + 行缓冲；
  qemu 计时 35.2s → 7.8s；functional/h_functional 双 target 全量通过；
  确定性/RISC-V 门禁见 M61。

## M59：内层 j 循环寄存器阻塞 + 部分展开（可与 M58 合并，待做）

- 在 M58 产出的 i-k-j 形状上，把内层 j 循环按 2-4 路展开 + 2-4 个独立 j
  累加器（寄存器阻塞）：一次迭代同时累加 `acc[j]`、`acc[j+1]`、…，隐藏
  `A[k][j]` load 延迟、暴露 A53 双发射、促成 `ldp`/`stp` 配对。
- 复用 M57 的多累加器机制，或并入 M58 的生成代码（同一 pass 内直接生成
  展开体）。该形状下 `A[k][j]`、`A[i][j]` 均为连续访问，展开无新增缓存风险。
- **验收**：内层 j 循环出现 2-4 条独立 `madd` + 连续 `ldr` 对；指令数对比
  基线记录于 `results/perf_compare/`。
- **收益**：约 **1.5-2×**（load 延迟隐藏 + 双发射利用率）。

## M60：元素级循环标量展开（低优先，可选）

- C 变换循环（`(C²+7)/3`，除法已 magic-multiply）与 -1 填充循环各 ~1M 次，
  相对 1.07e9 的矩阵乘可忽略；仅当 M58 落地后时间占比上升再展开（2-4 路，
  纯标量，复用 `loop_unroll` 或手写生成）。
- **收益**：预计很小，排在 M57-M59 之后。

## M61：验证与门禁（每里程碑必做）

- 正确性：`--emit ir` 形状检查（M57 内层 4 累加器 / M58 i-k-j + 行缓冲）；
  QEMU 语义差分（-O0 标量基线 vs -O1/-O2）；functional 109 + h_functional 40
  全过（含 -O2）。
- 确定性：双 target × -O0/1/2 各 5 次 byte-identical；RISC-V 全量回归。
- 性能：`results/perf/` 重新生成 many_mat_cal-1/2/3、matmul1-3、01_mm1-3、
  conv2d-1；记录 qemu 计时与 gem5 sim_insts；`scripts/perf_compare.sh`
  增加对应列。
- 收益预期：many_mat_cal 从 bench.csv 149.56s / 当前 qemu 25s → **约 1-3s
  （qemu）**；残余差距即 NEON（上文 M69，本计划外）。

## 合规性

- 全部变换为**通用循环/数组模式**（归约展开、循环交换、寄存器阻塞），不匹配
  函数名、输入特征、硬编码结果，符合 `docs/Illegal_optimization.md`。
- i32 重结合仅在加/乘（模 2³² 结合/交换）下允许，`/`、`%` 不做（元素级
  循环不动其除法）。
- M58 就地形态依赖「输出行写发生在其 k 循环结束后」这一结构事实，逐条按
  上节论证；无法证明时保守拒绝。

## 期望收益总览

| 里程碑 | 针对 | 期望加速 |
|---|---|---|
| M57 | 串行 madd 链 | 2-4×（✅ 已完成） |
| M58 | A 列访问 cache miss | 实测 ~4.5×（✅ 已完成） |
| M59 | load 延迟隐藏 | 1.5-2× |
| M60 | 元素级循环（低优先） | 少量 |
| M61 | 验证与门禁 | — |

M57/M58 已落地（M58 实测 many_mat_cal-1 qemu 35.2s → 7.8s）；M59 在 M58 的
i-k-j 形状上做 j 循环寄存器阻塞，可独立提交。每个 milestone 完成后删除本节
对应细节，只留一行历史。
