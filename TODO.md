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
- LLVM 路径注意：`--emit llvm` 会输出未定义的 `soyo_calloc`（test-llvm 不在
  门禁内，需在 LLVM emitter 里处理或文档声明）。
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
InvariantReductionHoisting、ReductionUnroll、BlockedReduction（M67）、
IfConversion、TCO、TailRecursiveInline、BooleanSimplification、GVNPRE、
DeadPhiElim、DCE。相关 pass 见 `raana_ir/src/opt/passes/`。
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

建议顺序：M69（NEON）为下一大项；M68 Phase D（命中路径内联、int16+tag 缓存）
是 h-1 剩余的可选打磨，可与 NEON 穿插。bench.csv 回写待 M69 落地后一次性更新。

## 风险与缓解

| 风险 | 严重度 | 缓解 |
|------|--------|------|
| 向量化指针别名 / mask 尾部（M69） | 中 | 保守别名检查；尾部标量回退 |
| RISC-V 回归 | 中 | 新 pass 按 target 注册；双 target 回归 |
| M68 合规风险（函数名/输入值匹配） | 高 | 只按结构触发；缓存尺寸来自运行时 bound，不写死 |
| M68 缓存内存（h-1-01 400MB） | 中 | int16+tag 打包减半（Phase D） |
| M68 `soyo_calloc` 在 LLVM 路径未定义 | 低 | test-llvm 不在门禁；LLVM emitter 需声明或文档注明 |

## 交接

M64-M67 已收敛 h-4 常量物化与模折叠、h-1 代码生成侧、h-5 寄存器分块（-37%）。
M68 纯递归记忆化已落地：h-1 族 QEMU 22.79s→6.5-8.4s（真机估算 ~1s，冠军线
<2s 达成），动态工作量 63x，双 target 152/152，非 h-1 静态零回归。最大差距
转为 M69 NEON（h-4/h-8/matmul）。所有静态目标以 `scripts/perf_compare.sh` 的
clang 对照列为准，动态以官方真机/QEMU 差分双验。
