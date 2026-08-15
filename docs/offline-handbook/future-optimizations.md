# 未来优化路线图：可实现的优化与实现路线

> 离线工作手册 G10。从仓库 `TODO.md` / AGENTS.md 提炼的**未完成优化**清单，
> 每项给出：原理（IR/汇编形态）、改哪层、实现步骤、预期收益、验证方法。
> 离线环境下按本节即可独立开工。所有收益数据来自 2026-08-05 静态基线
> （`scripts/perf_compare.sh`，对照 clang -O2）。

## 路线图总览

| 优先级 | 项 | 目标用例 | 预期收益 | 风险 |
|--------|----|---------|---------|------|
| P0 | M69 NEON 自动向量化 | h-4/h-8/matmul/huffman | 2-4× | 大/高 |
| P1 | M68 Phase D 记忆化微调 | h-1 族 | QEMU 6.5s→≤4s | 中 |
| P1 | M59 j 循环寄存器阻塞 | many_mat_cal | 1.5-2× | 中 |
| P2 | 向量运算方法扩展 | 见 vectorization.md | 视用例 | 低-中 |

## 1. M69：NEON 自动向量化（IR 层 loop 向量化）【P0 下一大项】

**现状**：后端已具备完整向量 lowering（`lower_vector_binary`/`vector_shape`/
splat/select/reduce，NEON v0-v31，ABI 向量参数），但 **IR 从不产生向量类型**
——没有 SLP/loop 向量化 pass，全部标量。

**原理**：识别 trip-count 已知、无回环依赖、访存连续/无别名冲突的内层循环，
改写为 `<4 x i32>`（或 `<4 x f32>`）向量 op + `VectorReduce` 收尾。

**实现步骤**：
1. 新 pass `loop_vectorize.rs`（`raana_ir/src/opt/passes/`），**仅 AArch64**
   （`TargetPolicy` 门控，RISC-V 不注册）；
2. 候选筛选：trip-count 已知（版本化守卫）、循环体纯（仅 load/算术，无
   store/call/memzero）、访存连续（GEP 步进 = 元素大小）；
3. 变换：4 路展开 + `VectorSplat`/向量 Binary/`VectorReduce` 收尾；尾部标量
   回退（T%4 余数）；
4. 顺序建议：① h-4 型纯算术循环（无访存，风险最低）→ ② h-8 内层 k 循环
   （连续 load 归约）→ ③ 分块后的 h-5；
5. 风险处理：mask/尾部（标量回退）、指针别名（保守检查）、向量化 `%`/`/`
   常数 magic 序列、`VectorReduce` 归约顺序（整数加模 2³² 可结合，浮点不可）。

**验收**：h-4/h-8 内层循环生成 `mul/add/ld1/st1`；静态表记录对照；
`make test-riscv` 零回归；双 target 全量回归。

## 2. M68 Phase D：记忆化微调（h-1 族）【P1】

**现状**：M68 纯递归记忆化已落地（h-1 QEMU 22.79s→6.5-8.4s，动态 63×），
距目标 ≤4s 仍有差距。

**方向**：
- **命中路径内联**：把记忆化命中检查/返回路径内联进调用方（减少 call 开销）；
- **缓存减半**：int16 + tag 打包（当前 h-1-01 缓存 400MB，打包可减半）。

**风险**：M68 有合规红线——**只按结构触发**（函数名/输入值匹配是违规的）；
缓存尺寸来自运行时 bound，不写死。改动必须保持双 target 152/152。

**验收**：h-1 族 QEMU 动态计时改进；非 h-1 静态零回归。

## 3. M59：内层 j 循环寄存器阻塞 + 部分展开【P1】

**现状**：M57（reduction_unroll 3 参数化，4 路累加器）+ M58（i-j-k → i-k-j
循环交换 + 输出行缓冲）已落地：many_mat_cal-1 qemu 35.2s → 7.8s。残余差距
= 内层 j 循环的累加器仍受寄存器压力限制，未做阻塞。

**原理**：i-k-j 形状下，内层 j 循环对输出行缓冲 `acc[j]` 做连续累加——把
`cik` 的乘加按 j 分块（寄存器阻塞），减少重复 load `A[k][j]`（L1 命中已好，
进一步压内存指令数）。

**实现**：`raana_ir/src/opt/passes/`（如 `reduction_unroll.rs` 扩展或新
pass）：识别 i-k-j 巢 + 行缓冲写回形态，j 循环内按 4-8 个累加器分块；
M58 的就地改写正确性约束（`Mout == M2` 别名，行缓冲写回在 k 循环结束后）
必须保持。

**验收**：many_mat_cal-1 qemu 继续下降（目标标量手段 1-3s，残余差距交给
M69 NEON）；`make test ARGS="-O 2"` 152 通过 + RISC-V 回归。

## 4. 向量运算方法扩展【P2】

详见 `vectorization.md`（G9）：fmls 乘减、整数 mla v.4s、VecMul+VecSub 融合、
浮点向量比较、.2d 乘法、逐 lane 掩码 select、归约扩展、交错加载。每条都给了
"改哪层 + 验证"。

## 5. 通用实现纪律（每个优化开工前读）

1. **先确认形态**：`--emit ir` 看 IR 是否符合模式（预告-失败循环被点名批评
   过的教训：先批量实证形态假设再承诺）；
2. **量化目标**：先算清解锁循环占 case 执行时间的比例——差分全 PASS 但解锁
   非热点 = 收益≈0；热点 = runtime-bound 循环才算数；
3. **门禁**：`cargo test -p raana_ir` → `make test ARGS="-O 2"`（functional+
   h_functional）→ `make test-riscv`（动共享层必跑）→ `scripts/perf_compare.sh`
   静态无回归；
4. **合规**：不做以 benchmark/函数名/输入为条件的优化（`docs/Illegal_
   optimization.md`）；
5. **文档**：落地后更新仓库根 TODO.md（只保留未完成项，完成项压一行摘要）。

## 6. 关键上下文（改管线前必读）

- IR 管线（`raana_ir/src/opt/pass.rs`）：SSA → Specialize → mulmod_recognize
  （AArch64）→ Memoize → Inline → TCO → ColumnMajor → GSP；固定点内：IPSCCP,
  SimplifyCFG, LoopUnroll, RotateLoops, ZeroStoreLoop(A64), ChainToSwitch(A64),
  LICM, GVN, PSR, SR, InvariantReductionHoisting, ReductionUnroll,
  MatmulInterchange(A64), BlockedReduction, IfConversion, TCO,
  TailRecursiveInline, BooleanSimplification, GVNPRE, DeadPhiElim, DCE。
- AArch64 MIR 管线（`anon_armv8/src/passes/mod.rs::build_pipeline`）：
  pre-RA：DCE → PeepholeCombine → ChainFusion → ConstCse；post-RA：
  PairCombine → ListScheduler。
- 静态基线（2026-08-05）：h-1 309、h-4 87、h-5 314、huffman 564、fft0 373
  （clang 359）；clang h-1-03 84。
