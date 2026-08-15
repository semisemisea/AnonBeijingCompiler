# 向量化（SIMD）：现有能力与可实现的运算方法

> 离线工作手册 G9。回答两个问题：① 编译器现在的 SIMD 能力到哪一步；② 还有哪些
> 向量运算方法**可以实现**（改哪里、怎么改、解锁什么用例）。配套 rustdoc：
> `anon_armv8/src/instructions.rs`（Vec* 指令族）、`anon_armv8/src/lower.rs`
> （lower_vector_* 路径）。

## 1. 现状：向量数据流

```
IR 层（raana_ir）：
  VectorSplat（标量广播成向量）/ VectorExtractElement / VectorInsertElement
  / VectorReduce（归约）/ Fma（乘加）
  向量类型：V4I32 / V4F32 / V2F64 / V2I64 等（raana_ir::ir::types）

lower 层（anon_armv8::lower）：
  lower_vector_binary     Binary 向量运算 → VecArithRRR / VecBitwise / VecCmp
  lower_vector_splat      VectorSplat → VecDup
  lower_vector_extract_element / lower_vector_insert_element → VecExtractLane / 插入
  lower_vector_reduce     VectorReduce → VecAddv（+ 标量提取）
  lower_fma               Fma → VecFmla
```

## 2. 已支持指令矩阵（instructions.rs，MInst Vec* 变体）

| 指令 | 运算 | 说明 |
|------|------|------|
| `VecMov` | 向量拷贝 | |
| `VecLd1` / `VecSt1` | 128 位加载/存储 | |
| `VecDup` | 广播（splat） | |
| `VecArithRRR` | Add / Sub / Mul | **仅 .4s/.2s/.4h 等；`.2d` 乘法会 panic**（AArch64 无 mul v.2d） |
| `VecFmla` | 乘加 acc+a\*b | 浮点 v.4s/v.2d；**无负号 fmls、无整数 mla** |
| `VecBitwise` | And / Orr / Eor | |
| `VecCmp` | Eq / Gt | **仅整数**；浮点比较（fcmgt 等）会 panic |
| `VecBsl` | 位选择（掩码 select） | |
| `VecCvt` | Scvtf / Fcvtzs | int↔float 转换 |
| `VecAddv` | 横向加（归约） | v4s → 标量 |
| `VecMovImm` | 立即数 | |
| `VecExtractLane` | 提取 lane | |

**结论**：能力覆盖"广播 + 逐元素算术 + 位运算 + 整数比较 + 归约"；**缺**：
乘减（fmls）、整数乘加（mla）、浮点比较、.2d 乘法、多种归约（maxv/minv）。

## 3. 可实现运算方法清单（按性价比排序）

### M1. fmls：浮点向量乘减（f32 a - b\*c）【高性价比】
- **原理**：NEON `fmls vd, vn, vm` = `vd - vn*vm`（乘减，AArch64 一条指令）。
- **改哪层**：① `instructions.rs`：`VecFmla` 加 `neg: bool` 字段或新增
  `VecFmls` 变体（emit 打印 `fmls`）；② `lower.rs` `lower_fma`：当 IR 形态是
  `acc - mul(lhs, rhs)`（Fma 的 acc 前有 Sub，或 Fma 语义为负）时选择 fmls；
  ③ 或在 `peephole_combine` 里把 `VecSub + VecMul` 融合成 fmls。
- **IR 形态**：`fma(sub(a, ...), ...)` 或 `sub(x, mul(y, z))`。
- **验证**：`make test h_functional/xxx ARGS="-O 2"` + 汇编核对出现 `fmls`。
- **解锁**：h-10 类 f32 乘减循环（当前是 mul+sub 两条）。

### M2. 整数 mla：v.4s 整数乘加【高性价比】
- **原理**：NEON `mla vd.4s, vn.4s, vm.4s`（整数乘加）或 `mls`（乘减）。
- **改哪层**：`VecFmla` 目前隐含浮点；扩成同时支持整数（emit 按类型选
  `fmla`/`mla`），`lower_fma` 对 `V4I32` 类型直接放行（现在整数 Fma 可能
  没走到向量路径）。
- **解锁**：h-5 类 i32 内积循环（clang 用 mla 的核心位置）。

### M3. 向量 MAC 融合（VecMul + VecSub/VecAdd → fmls/fmla）【中】
- **原理**：标量 madd/msub 融合已在 MIR `peephole_combine`（仅匹配标量 Mul）；
  向量路径没有。把同样的模式匹配扩展到 Vec 指令。
- **改哪层**：`anon_armv8/src/passes/peephole_combine.rs`（向量模式）+ 可选
  IR 层 `Fma` 折叠 pass（`raana_ir` 现无 Fma 折叠）。
- **注意**：浮点融合会改变舍入语义（fma 不中间舍入），SysY 无
  `-ffp-contract` 语义要求，本项目 clang 基线用 `-ffp-contract=off`——融合
  需与基线对照一致（clang 融合与否决定静态指令数对比）。

### M4. 浮点向量比较（fcmgt / fcmeq）【中】
- **原理**：NEON `fcmgt vd.4s, vn.4s, vm.4s` 等浮点比较。
- **改哪层**：`instructions.rs` `VecCmpOp` 加 `Fgt`/`Feq`/`Fge`…；
  `lower.rs` `lower_vector_binary` 去掉浮点 panic 分支，按 `is_float` 选 op。
- **解锁**：浮点掩码/条件路径（如浮点循环的边界掩码向量化）。

### M5. .2d 乘法（V2F64/V2I64 mul）【中低】
- **原理**：AArch64 无 `mul v.2d`；`fmul v.2d` 存在但整数需 `smull/umull`
  （产生 .1q 结果）。当前 `lower_vector_binary` 对 TwoD × Mul 直接 panic。
- **改哪层**：`lower.rs`——V2F64 用 `fmul.2d`；V2I64 走 smull/umull 或拆
  两个标量（取决于用例收益）。
- **解锁**：64 位向量算术循环（目前此类循环应极少，先验证有用例再实现）。

### M6. 逐 lane 掩码 select（VecBsl v3）【中低】
- **原理**：`VecBsl`（位选择）已存在；v2 是标量条件选择，v3 是**逐 lane**
  条件（比较结果作为掩码直接 bsl）。
- **改哪层**：`lower.rs` `lower_select`：向量类型 + 条件为向量比较结果 →
  直接 VecBsl；IR 层需先有向量 Select 形态。
- **解锁**：conv2d 边界、transpose 的逐元素条件路径。

### M7. 归约扩展（smaxv / sminv / fmaxv）【低】
- **原理**：`VecAddv` 已实现 addv 归约；`smaxv`/`sminv`/`fmaxv` 同类。
- **改哪层**：`instructions.rs` 加 `VecReduceOp` 扩展 + `lower_vector_reduce`
  按 `VectorReduceOp` 分派。
- **解锁**：min/max 归约循环（若 perf 语料有）。

### M8. 交错加载（ld2/ld3/ld4）【低】
- **原理**：NEON 结构化加载，解交错数组访问（AoS → SoA）。
- **改哪层**：`instructions.rs` VecLd1 扩展多寄存器形式 + lower 检测交错访问。
- **解锁**：RGB/复数数组类用例（perf 语料暂未见到，优先级最低）。

## 4. 实现前的通用步骤（照抄）

1. **确认形态**：`cargo test -p raana_ir` + 单 case 编译
   （`target/debug/compiler -S -O 2 --target aarch64 -o /tmp/out.s tests/perf/xxx.sy`
   + `--emit ir` 看 IR 形态是否匹配你的模式）；
2. **改 IR 层**（如需要）→ **改 instructions.rs**（新 op/变体）→ **改
   lower.rs**（选择）→ 需要调度则更新 `sched/` 延迟表；
3. **验证**：单 case 差分（输出一致）→ 汇编核对新指令出现 → `make test
   ARGS="-O 2"` 全量 → `make test-riscv`（若动共享层）→
   `scripts/perf_compare.sh` 看静态指令数 vs clang。

## 5. 红线

- 不做以 benchmark/函数名/输入为条件的优化（AGENTS.md）；
- 向量化 pass 只进 AArch64 管线（RISC-V 无 NEON），动共享层必须
  `make test-riscv`；
- 浮点融合（fma/fmls）与 clang 基线 `-ffp-contract=off` 的一致性要对照
  静态指令数，别为了指令数好看改变语义约定。
