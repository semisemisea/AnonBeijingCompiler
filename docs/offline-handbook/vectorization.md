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

lower 层（anon_armv8::lower/，lower.rs 是 facade）：
  lower/vector.rs   lower_vector_binary → VecArithRRR / VecBitwise / VecCmp
                    lower_vector_splat → VecDup
                    lower_vector_extract_element → VecExtractLane
                    lower_vector_insert_element → VecInsertLane
                    lower_vector_reduce → VecAddv
                    lower_fma → VecFmla
  lower/branch.rs   lower_select（向量 → VecBsl）
```

## 2. 已支持指令矩阵（instructions.rs，MInst Vec* 变体）

| 指令 | 运算 | 说明 |
|------|------|------|
| `VecMov` | 向量拷贝 | |
| `VecLd1` / `VecSt1` | 128 位加载/存储 | |
| `VecDup` | 广播（splat） | |
| `VecArithRRR` | Add / Sub / Mul | **形状仅 `.4s`/`.2d` 两种**（`vector_shape` 只产出这两个，其他形状直接 panic）；Add/Sub 两种都可用，**Mul 仅 .4s**（.2d 乘法 panic） |
| `VecFmla` | 乘加 acc+a\*b | 浮点 v.4s/v.2d；**无负号 fmls、无整数 mla** |
| `VecBitwise` | And / Orr / Eor | |
| `VecCmp` | Eq / Gt | **仅整数**；f32 比较会 panic，**但 f64（V2F64）比较不 panic**（`is_float` 只匹配 f32，会静默生成整数比较——隐患） |
| `VecBsl` | 位选择（掩码 select） | |
| `VecMinMax` | Smin/Smax/Umin/Umax/Fmin/Fmax | 元素级 min/max（lower_vector_binary 已支持） |
| `VecCvt` | Scvtf / Fcvtzs | int↔float 转换 |
| `VecAddv` | 横向加（归约） | 仅 .4s 整数 addv；f32→faddp 缺失、.2d 缺失 |
| `VecMovImm` | 立即数 | |
| `VecExtractLane` | 提取 lane | |
| `VecInsertLane` | 插入 lane | |

**结论**：能力覆盖"广播 + 逐元素算术 + 位运算 + 整数比较 + min/max + 归约"；
**缺**：乘减（fmls）、整数乘加（mla）、浮点比较（f32/f64 都有缺口）、.2d 乘法、
多种归约（maxv/minv）。

## 3. 可实现运算方法清单（按性价比排序）

### M1. fmls：浮点向量乘减（f32 a - b\*c）【高性价比】
- **原理**：NEON `fmls vd, vn, vm` = `vd - vn*vm`（乘减，AArch64 一条指令）。
- **改哪层**：① `instructions.rs`：`VecFmla` 加 `neg: bool` 字段或新增
  `VecFmls` 变体（emit 打印 `fmls`）；② `lower/vector.rs` `lower_fma`
  （133 行起）：当前实现对 `Fma` 无脑 emit `VecFmla`（acc 是显式 SSA 读，
  Fma 本身无 neg 字段）——需要先识别 `acc - mul(lhs,rhs)` 形态（IR 层 Fma
  折叠或 lower 时看 lhs 是否来自 Sub），或在 `peephole_combine` 里把
  `VecSub + VecMul` 融合成 fmls。
- **IR 形态**：`fma(sub(a, ...), ...)` 或 `sub(x, mul(y, z))`。
- **验证**：`cargo test -p taki_mir -p anon_armv8` + `make test
  h_functional/xxx ARGS="-O 2"` + 汇编核对出现 `fmls`。
- **解锁**：h-10 类 f32 乘减循环（当前是 mul+sub 两条）。

### M2. 整数 mla：v.4s 整数乘加【高性价比】
- **原理**：NEON `mla vd.4s, vn.4s, vm.4s`（整数乘加）或 `mls`（乘减）。
- **现状坑（先修这个）**：`Fma`（`raana_ir` fma.rs）和 `lower/vector.rs`
  `lower_fma` 目前都**不检查类型**——V4I32 的 Fma 一旦出现会**静默生成错误
  的 fmla 汇编**而不是 panic。第一步应在 `lower_fma` 加类型分派：整数 emit
  `mla`/`mls`、浮点 emit `fmla`/`fmls`（或先 panic 兜底）。
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
  `lower/vector.rs` `lower_vector_binary` 去掉 f32 panic 分支（58 行起），
  按 `is_float`（20 行）选 op；**顺带修 V2F64 比较的静默隐患**
  （`is_float` 只匹配 f32）。
- **解锁**：浮点掩码/条件路径（如浮点循环的边界掩码向量化）。

### M5. .2d 乘法（V2F64/V2I64 mul）【中低】
- **原理**：AArch64 无 `mul v.2d`。`fmul v.2d` 存在（浮点）；但 `smull`/
  `umull` 是 `.2s × .2s → .2d` 的 **widening 乘法**，不能直接做
  V2I64×V2I64——V2I64 乘法实际只能拆标量（或前端降成 V4I32）。
- **改哪层**：`lower.rs`——V2F64 用 `fmul.2d`；V2I64 拆标量（先验证有
  用例再实现）。
- **解锁**：64 位向量算术循环（此类循环应极少，先验证有用例）。

### M6. 逐 lane 掩码 select（VecBsl v3）【✅ 已完成，勿重复实现】
- **现状**：`lower/branch.rs` 的 `lower_select`（15-23 行）对向量类型**已
  直接 emit `VecBsl`**（注释 "select(mask, if_true, if_false) over vectors:
  bit-select"）。真正缺的只是 M4 的浮点比较掩码（比较产生 mask 的路径）。

### M7. 归约扩展（smaxv / sminv / fmaxv）【低】
- **原理**：`VecAddv` 已实现 addv 归约；`smaxv`/`sminv`/`fmaxv` 同类。
- **改哪层**：IR 层 `VectorReduceOp` 加成员（定义在
  `raana_ir/src/ir/inst_kind/vector_reduce.rs`，目前只有 `Add`）+ 后端
  `lower_vector_reduce` 分派。注意：`VecMinMaxOp` 支撑的是**逐元素**
  min/max，横向 maxv/minv 需新增类 `VecAddv` 的 MInst 变体（如 `VecMaxv`）
  + emit，可复用其 op 命名但不能直接复用指令。现状限制：
  `lower_vector_reduce` 仅 .4s 整数 addv，f32→faddp 缺失、.2d 缺失。
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
3. **验证**：`cargo test -p taki_mir -p anon_armv8`（含 instructions.rs
   内联 emit 单测）→ 单 case 差分（输出一致）→ 汇编核对新指令出现 →
   `make test ARGS="-O 2"` 全量 → `make test-riscv`（若动共享层）→
   `scripts/perf_compare.sh` 看静态指令数 vs clang。

## 5. 红线

- 不做以 benchmark/函数名/输入为条件的优化（AGENTS.md）；
- 向量化 pass 只进 AArch64 管线（RISC-V 无 NEON），动共享层必须
  `make test-riscv`；
- 浮点融合（fma/fmls）与 clang 基线 `-ffp-contract=off` 的一致性要对照
  静态指令数，别为了指令数好看改变语义约定。
