# 编译优化空间分析：9 篇 SIMD 编译器论文 × AnonBeijingCompiler

> 文档类型：优化规划（Plan）
> 生成日期：2026-08-07
> 论文输入：Diospyros(ASPLOS'21)、Isaria(ASPLOS'24)、Minotaur(OOPSLA'24)、
> SuperVectorization(PLDI'22)、Parsimony(CGO'23)、Autovesk(TACO'23)、
> Coyote(ASPLOS'23)、Qiwu(CGO'25)、CHOPPER(HPCA'23)
> 决策：P0+P1 共五项推进；分析落档于本文件
> 关联：TODO.md §3（SIMD/NEON 主计划 C）、§5（后续候选工作）、AGENTS.md

---

## 1. 项目现状快照（代码实证，2026-08-07）

### 1.1 编译器架构

```
SysY → SoyoCompiler(前端/lalrpop) → RaanaIR(SSA HLIR + 优化 pass)
     → taki_mir(通用 MIR / ion 寄存器分配 / MIR pass 框架)
     → anon_armv8(AArch64 ISel + MIR passes + 调度器) | uika_riscv(RISC-V)
```

- 工作区 crate：`soyo_compiler`、`raana_ir`、`taki_mir`、`anon_armv8`、`uika_riscv`、
  `tomori_utils`、`sysylib`。工具链锁定 Rust 1.85.0。
- 生产 AArch64 路径：`SysY -> RaanaIR -> VCode/MIR -> ion RA -> AAPCS64 frame -> GNU AArch64 asm`。

### 1.2 IR 层已落地（raana_ir/src/opt/pass.rs）

初始（normalization）序列：`SSA → Specialize → Inline → TCO → ColumnMajor → GSP`；
AArch64 专属前置：`MulmodRecognize`、`RecursiveMemoize`（`enable_chain_to_switch` 门控）。

固定点内 20+ pass：`IPSCCP, SimplifyCFG, LoopUnroll, RotateLoops, ZeroStoreLoop,
ChainToSwitch, LoopInterchange, LoopVectorize, LICM, GVN, DSE,
PointerStrengthReduction, GuardElimination, ModFold, StrengthReduction,
MatmulInterchange, InvariantReductionHoisting, ReductionUnroll, BlockedReduction,
IfConversion, TCO, TailRecursiveInline, BooleanSimplification, GVNPRE,
DeadPhiElim, DCE, DeadFunctionElimination`。

### 1.3 向量化现状（LoopVectorize, M44 系列）

- 目标：最内层、rotated（test-at-bottom）/ test-at-top 计次循环，纯逐元素、连续 4B
  访问（NEON VF=4，i32/f32）。
- 已支持：向量常数除/模、模归约（scalar acc + addv）、B1 寄存器归约（vector acc +
  VectorReduce/addv）、单臂 if 掩码 store、运行时 bound（动态 counter + 标量 tail）、
  向量 mul+add → VecMla（M70）、VectorSplat LICM 外提（M70）。
- 约束（宁漏勿错）：只最内层；test-at-top 只认恰 `[lt,br]`；rotated 只认 `[jump]`；
  对齐未知时非对齐 ld/st；无 gather/scatter；AArch64-only。

### 1.4 MIR/后端层已落地（anon_armv8/src/passes/）

`dce`、`peephole_combine`（标量/向量 MAC 融合 + flag 融合 `subs/ands/tst`）、
`pair_combine`（LDP/STP）、`chain_fusion`、`const_cse`、`list_scheduler`
（cortex-a53 guide 模型 DUI 0901，含 pair load/store SchedClass）。

### 1.5 已知缺口（来源：TODO.md + 代码核实）

| 缺口 | 位置 | 状态 |
|---|---|---|
| SLP 基本块向量化 | `raana_ir/src/opt/passes/slp.rs` | 文件尚不存在（M45 未做） |
| matmul1 掩码内核（if 内 load/store） | `loop_vectorize.rs` B1 形态 + `dependence.rs` B3/C1 | `shape_body_not_2_blocks` 111 例 |
| test-at-top 多参数 passthrough | `loop_vectorize.rs` step 4 | `test_at_top_multi_param` 48 例 |
| 标量 smin/smax/fmin/fmax ISel | `anon_armv8/src/lower.rs:342` | 直接 `lowering_panic`（vector-only） |
| select→min/max 模式匹配 | `peephole_combine.rs` | 未做 |
| VecCsel/bsl、fcmgt | `anon_armv8/src/lower.rs:408` 附近 | 未做（随 SLP/M44 伴生） |
| 非循环内联向量零初始化 | `emit_zero_init` | 已留 Vector 分支，未接 int/float 数组 |
| 向量化盈利性成本模型 | `loop_vectorize.rs` | 纯形态识别，无成本 accept/reject |
| 回边 blockparam mov 消除 | `taki_mir` ion / MIR 层 | M35 遗留（`_and/_xor/_or` 回边 3 mov） |
| 调度验证器 | `anon_armv8/src/passes/` | §5.2 P2 未做 |

### 1.6 M44 复扫拒绝分布（61 例 perf 语料，dedup top）

`not_innermost` 276、`shape_header_multi_inst` 168、`shape_body_not_2_blocks` 111、
`test_at_top_multi_param` 48、`exit_has_params` 48、`non_unit_step` 45、
`NoInductionVariable` 30、`bound_not_const` 18+18、`params_not_2` 12、
`CallInBody` 9、`exit_arg_not_acc` 6、`Rem` 3。

### 1.7 方法论约束（AGENTS.md / TODO §1.3）

- 真正的质量门禁是 Docker 测试 harness + 与 clang/gcc -O2 的静态代码量对比；
- 静态指令计数（`scripts/perf_compare.sh`）为确定性回归代理；gem5 A53 SE 为动态
  验证；QEMU 仅语义差分；
- 未获得 XCZU15EG 实机数据前，只声称"静态模型改进"，不声称实机收益；
- 任何 AArch64 改动必须同时验证 RISC-V 不回归（`make test-riscv`）；
- 不允许针对测试用例的优化（`docs/Illegal_optimization.md`）。

---

## 2. 论文 → 优化空间映射

### 2.1 主题 1：代码生成从"手工维护"变为"自动化搜索"

- **Diospyros（ASPLOS'21）**：equality saturation 把向量化变为搜索问题；教训——
  手写 peephole/ISel 有系统性空白，搜索/合成可补全。
- **Isaria（ASPLOS'24）**：从 ISA 规格自动合成 rewrite rules + 相位调度；教训——
  规则/模式库可由规格离线生成，编译期无需手工维护。
- **Minotaur（OOPSLA'24）**：LLVM 漏掉 `fsub;fcmp>0 → fcmp`；教训——离线搜索 +
  缓存 + 形式化验证能让激进优化成立；用 uOps/周期成本而非简单计数。

**→ 映射到本项目**：
- MIR peephole（`peephole_combine.rs`）是手写模式，覆盖 MAC + flag fusion；可用
  离线合成补全（标量 select→min/max、双 `xor -1`、`fsub;fcmp` 类、掩码/位模式）。
- 验证链思想 → 调度验证器（§5.2 P2）、peephole 属性测试。

### 2.2 主题 3：向量化范式统一（SLP + 控制流 + SPMD 语义）

- **SuperVectorization（PLDI'22）**：Predicated SSA 让 SLP 跨基本块/循环打包；教训——
  SLP 的"选择性打包"天然支持局部向量化，if-conversion/predication 让向量化跨控制流。
- **Parsimony（CGO'23）**：shape analysis（indexed/varying）决定访存指令选择；
  教训——uniform/strided 分类是 NEON load/store 选择的关键。

**→ 映射到本项目**：
- M45 SLP 基本块向量化（`slp.rs` 新建，conv2d 直通代码）。
- matmul1 掩码内核（if 内 load/store → predicated vector）。
- A3 多参数 passthrough（外层 IV 线程化进内层循环 = 跨循环嵌套打包）。

### 2.3 主题 4：数据搬移/占用率必须进成本模型

- **Coyote（ASPLOS'23）**：pack 收益与数据搬移（FHE 旋转）联合搜索；教训——向量化
  决策必须含 shuffle/dup/addv 成本。
- **Qiwu（CGO'25）**：密文级"气泡"融合；教训——占用率（无效 lane）是优化对象。
- **CHOPPER（HPCA'23）**：粒度失配 + 溢出预算；教训——激进向量化 + 展开会放大
  寄存器/存储需求，超出容量即溢出。

**→ 映射到本项目**：
- 向量化盈利性成本模型（NEON dup/shuffle/addv vs 标量 4× unrolled）。
- NEON 寄存器压力预算（32 个 v 寄存器，`many_mat_cal` blocked reduction 已逼近）。
- 非循环内联向量零初始化（减少 bl/搬移，属"减少数据搬移"主题）。

### 2.4 方向汇总（8 项）

| 方向 | 论文 | 优先级 | 阶段 |
|---|---|---|---|
| A. SLP 基本块向量化（M45） | SuperVectorization | P1 | 本计划里程碑 4 |
| B. matmul1 掩码内核（B1→C1） | SuperVectorization | P0 | 本计划里程碑 1 |
| C. 向量化盈利性成本模型 | Coyote | P2 | 后续 |
| D. non_unit_step 步长归一化 | Autovesk | P2 | 后续 |
| E. Minotaur 式 peephole/ISel 补全 | Minotaur/Diospyros | P1（E1）/P3（E2） | 本计划里程碑 3 |
| F. NEON 寄存器压力预算 | CHOPPER | P2 | 后续 |
| G. 回边 blockparam mov 消除 | Diospyros/Isaria | P2 | 后续 |
| H. 调度验证器 / 验证链 | Minotaur | P3 | 后续 |

---

## 3. 本计划五项里程碑（详细实现计划）

### 里程碑 1（P0）：matmul1 掩码内核向量化 —— B1→C1 依赖链

**论文依据**：SuperVectorization 的 if-conversion + predicated vectorization：
把控制依赖转数据依赖（掩码选择），使向量化跨控制流。对应 M44 拒绝分类 B1→C1→B2/B3。

**目标形态**：`if(a[i][k]*b[k][j]%2==0) temp += b[i][k]*a[k][j]`
（matmul1 奇偶掩码内核，41-50 行）改为单块体 + 掩码向量化。

**现状根因**：
- `if_conversion` 只提升 i32 binary（`safe_arm_binary`），分支含 load/store 不转
  select → 循环体保持多块 → `shape_body_not_2_blocks`(111) 拒绝。
- `dependence.rs` B3 豁免（`is_elementwise_inplace`）未覆盖"写值依赖链含多个 load
  或跨块条件执行" → `IntraIterationConflict`（C1）。

**实现步骤**：
1. **if_conversion 提升面扩展**：把分支内 load 依赖链（`b[i][k]`/`a[k][j]` 的
   GEP+load）提升到分支之前，body 转为 select 形态（配合现有 `EffectAnalysis`/
   MemorySSA 校验别名安全——load 提升必须不改变内存读取副作用与顺序语义）。
2. **B3 豁免扩展**：`dependence.rs` 的 `value_flows_to` 支持"写值依赖链含多个
   load / 跨块条件执行"的判定（依赖 1 完成后单块体形态才完整）。
3. **向量器 B1 单臂 if 扩展**：现有单臂 if（`arm` 假边）扩展到任意条件 store 的
   掩码重写（`VecSelect`/mask 生成）。
4. 若 `%2==0` 条件仍因 NEON 无整数向量 mod 而受限：只掩码化 store 侧，模运算保持
   标量条件（正确但部分收益），或由 IR 层 `sr`/`mod_fold` 先改写。

**涉及文件**：`raana_ir/src/opt/passes/if_conversion.rs`、
`raana_ir/src/opt/analysis_passes/dependence.rs`、
`raana_ir/src/opt/passes/loop_vectorize.rs`、
`anon_armv8/src/instructions.rs`（VecCsel/bsl lowering，若需）。

**验收**：
- matmul1 内核出向量指令；`VECDBG_SHAPE`/`M44_TRACE=1` 复扫该形态不再报
  `shape_body_not_2_blocks`。
- `make test functional ARGS="-O 2"` + `make test-riscv ARGS="-O 2"` 无回归；
  `scripts/perf_compare.sh` 静态计数 ≤ 现基线；gem5 sim_insts 记录进
  `results/perf_compare/`。

**风险**：load 提升改变内存副作用顺序 → 用现有别名分析快照校验；掩码 bsl 生成需要
新增 VecCsel/bsl MInst lowering（非平凡，见 TODO §3.4-D），先以"条件 store 标量
保留、其余向量化"的最小正确形态落地。

---

### 里程碑 2（P0）：test-at-top 多参数 passthrough —— 01_mm/conv2d 内核

**论文依据**：SuperVectorization 的跨循环嵌套打包（外层 IV 以 passthrough 线程化进
内层循环）。对应 M44 拒绝分类 A3。

**目标形态**：test-at-top 循环 header 携带 `[iv, acc]` + 外层 IV passthrough
（4-7 参数）也可向量化。代表：conv2d 内核（i/j/k/l 4 层嵌套，每层 6-7 参数）、
01_mm1 计算内核（BB20）。

**现状根因**：`loop_vectorize.rs` step 4 对 test-at-top 保守 `n_params==1`，
未复用 rotated 形态已有的 passthrough 识别。

**实现步骤**：
1. step 4 对 test-at-top 复用 rotated 的 passthrough 判定：back-edge 参数
   `back_args[i] == params[i]`（原样转发）即 passthrough。
2. 放行参数集 = `[iv]` 或 `[iv, acc]`（B1）+ passthrough 集合；passthrough 进入
   `passthrough_args`，exit 边按现有 `ExitArgSpec::Passthrough` 转发。
3. 回归：确认 vectorize 后 passthrough 参数保持 i32 标量（不参与向量重定型），
   固定点收敛复用现有 idempotence 机制（`vec_tail`/`vec_epi_` 命名空间防护）。

**涉及文件**：`raana_ir/src/opt/passes/loop_vectorize.rs`（step 4/6b）。

**验收**：
- 01_mm1 计算内核、conv2d 内层出向量；`M44_TRACE=1` 复扫 `test_at_top_multi_param`
  计数下降。
- 双 target 回归；`cargo test -p raana_ir` 全绿（新增 test-at-top + passthrough
  单测）。

**风险**：低。改动集中于 test-at-top 参数识别，rotated 路径不动；用现有单元测试
覆盖（参考 M44 v2 已加的 rotated runtime-bound + B1 单测模式）。

---

### 里程碑 3（P1）：标量 select→min/max + smin/smax/fmin/fmax ISel

**论文依据**：Minotaur 的 peephole 补全思想——编译器漏掉的简单恒等/模式
（`select(a>b,a,b)` = `smin(a,b)`，省 1 条 cmp）。对应 TODO §3.4-C（低优先但最小改动）。

**现状根因**：`anon_armv8/src/lower.rs:342` 对标量 `BinaryOp::Min/Max` 直接
`lowering_panic("scalar min/max is vector-only")`；`smin/smax/fmin/fmax` MInst 仅
向量版（`instructions.rs:830`）；`SelectCmp` MInst 已存在（csel/cset 用）。

**实现步骤**：
1. `anon_armv8/src/instructions.rs`：新增标量 `Smin/Smax/Fmin/Fmax` MInst（或
   MinMax 参数化复用向量 emitter），SchedClass 归入 `AluMisc`/`FpAddSub`。
2. `anon_armv8/src/lower.rs`：对标量 `BinaryOp::Min/Max` 直接 ISel；对
   `select(a>b, a, b)` 形态做模式匹配 → smin/smax（含 `cmp+csel` 两指令合并）。
3. IR 层（可选辅助）：`boolean_simplify`/新 peephole 识别 `if(a>b) c=a` 直接
   Min 形态。
4. RISC-V：`uika_riscv` 对标量 min/max 保持现状（不影响；RISC-V 无 smin 指令，
   保留 select 展开）。

**涉及文件**：`anon_armv8/src/instructions.rs`、`anon_armv8/src/lower.rs`、
`anon_armv8/src/regs.rs`（SchedClass）、`raana_ir`（可选辅助 pass）。

**验收**：
- 新增 inline 单测（标量 min/max ISel + select→min/max 模式）；
- huffman/h-9/h-4/crc 等 csel 用例静态指令数下降；无 `min/max is vector-only`
  panic 用例；双 target 回归。

**风险**：低。新增指令不发散既有 ABI；select→min/max 模式匹配须保证有符号/无符号
与 NaN（浮点）语义正确（`fmin` 的 NaN 传播语义需与 select 一致——用
`fcmp`+csel 语义对照验证）。

---

### 里程碑 4（P1）：M45 SLP 基本块向量化

**论文依据**：SuperVectorization（SLP 选择性打包 + 循环展开暴露相邻同构，局部
向量化天然支持）+ Parsimony（uniform/stride 形状分类用于访存选择）。对应 TODO
§3.2-M45 与 §3.4-B（有实证）。

**现状**：`slp.rs` 尚不存在；`loop_unroll`（常数 trip 全展开）已合入主线，是 SLP
现成输入；conv2d `init_matrix`/`row_reduce`、01_mm/matmul 内层展开后存在相邻同构
标量 op。SysY 无三目（全量 tern=0），独立直通代码场景少，主要收益来自"循环向量化 +
展开后的补充打包"。

**实现步骤**：
1. 新建 `raana_ir/src/opt/passes/slp.rs`：
   - 输入：基本块内相邻、类型一致、相互独立（无 use-def/内存冲突）的标量 op；
   - 打包：配对的 add/mul/load/store（菱形结构），4 路打包为 `<4 x T>`；
   - 依赖判定：复用 `DependenceAnalysis`/`EffectAnalysis`（宁漏勿错，遵守
     §3.3 不变量：M42 可证安全才向量化）。
2. 注册到 `pass.rs`：AArch64-only（`TargetPolicy.enable_chain_to_switch` 门控）；
   顺序按数据定：`LoopVectorize → LoopUnroll → SLP`（先小规模 unroll 再 SLP）。
3. lowering：SLP 产生的 `VectorSplat`/向量 load/store 复用 M44/M39-M41b 通路；
   若需 `ld1/st1` 非对齐路径（PE=0 容忍）。
4. `perf_compare.sh` 增加 SIMD 列；记录 conv2d/01_mm/matmul 静态指令与 gem5
   sim_insts 相对标量基线变化。

**涉及文件**：`raana_ir/src/opt/passes/slp.rs`（新建）、`raana_ir/src/opt/pass.rs`、
`raana_ir/src/opt/passes/mod.rs`、`scripts/perf_compare.sh`。

**验收**：
- conv2d-1 内层出现 `ld1/fmla/st1`；
- `make test functional ARGS="-O 2"` + `make test-riscv ARGS="-O 2"` 无回归；
- 静态计数与 gem5 sim_insts 记录进 `results/perf_compare/`。

**风险**：SLP 打包错误导致语义偏差 → 严格独立判定 + on/off 差分；展开后代码量爆炸
→ 复用现有 unroll 阈值；向量 load/store 对齐 → 非对齐路径（已存在）。

---

### 里程碑 5（P1）：非循环 NEON 内联向量零初始化

**论文依据**：非循环 NEON 实证（TODO §3.4-A，已有实证基础）；Minotaur 的"减少
搬移/调用"主题。独立小项，不依赖向量化 pass。

**现状**：`int words[80]={0}`（crypto-1）等局部零初始化走 `.Lsoyo_memzero`（M53
产物，`bl` + 16B 对齐检查 + zva/byte loop）；每次调用都 `bl`。`emit_zero_init`
已含 Vector 分支，但 int/float 数组未按 16B 块打包。

**实现步骤**：
1. `anon_armv8`（或 emit 层）识别编译期已知大小的局部零初始化：
   - 大小 ≤ 阈值（64-128B）→ 内联 `movi v0.4s,#0` + `stp q0,q0` 展开；
   - 大数组 → 保持 memzero。
2. int/float 数组按 16B 块打包（复用 Vector 分支；i32×4 / f32×4 / i8×16 形状）。
3. 保持 -O0 标量基线（on/off 差分）。

**涉及文件**：`anon_armv8/src/lower.rs`（alloca 零初始化降低）、
`anon_armv8/src/instructions.rs`（Vector 零指令 emission）。

**验收**：
- crypto-1 sha1 每块清零免 `bl` 调用；静态指令数可量化下降；
- `make test functional ARGS="-O 2"` + `make test-riscv ARGS="-O 2"` 无回归；
  on/off 差分无行为差异。

**风险**：低。固定大小阈值防展开爆炸；浮点 `+0.0`/`-0.0` 语义（零初始化均为
位级 0，`movi` 安全）。

---

## 4. 后续候选（P2/P3，不在本次范围）

| 项 | 论文 | 状态 |
|---|---|---|
| C. 向量化盈利性成本模型 | Coyote | 需先积累 M44/M45 数据 |
| D. non_unit_step 步长归一化 | Autovesk | A5，45 例 |
| F. NEON 寄存器压力预算 | CHOPPER | 随向量化+展开规模上升后做 |
| G. 回边 blockparam mov 消除 | Diospyros/Isaria | M35 遗留 |
| E2. Minotaur 式 peephole 离线合成库 | Minotaur | 需成本模型成熟 |
| H. 调度验证器闭环 | Minotaur | §5.2 P2 |
| 外层循环向量化（not_innermost 276 例） | SuperVectorization | 跨块，大改 |
| VecCsel/bsl、fcmgt lowering | SuperVectorization | 随里程碑 1/4 伴生 |

---

## 5. 执行顺序与门禁

```
docs/optimization_analysis.md（本文件）
  → 里程碑 1（matmul1 掩码内核）
  → 里程碑 2（test-at-top passthrough）
  → 里程碑 3（标量 min/max ISel）
  → 里程碑 5（内联向量零初始化，小）
  → 里程碑 4（M45 SLP，大）

每个里程碑独立 commit：
  前缀 [Opt(IR)] / [Feat(Armv8)] / [Docs]；相关处引用里程碑号。

门禁（AGENTS.md）：
  cargo test -p raana_ir
  make test ARGS="-O 2"（functional + h_functional）
  make test-riscv ARGS="-O 2"
  scripts/perf_compare.sh 无静态计数回归
  AArch64 改动必须双 target 验证
```

---

## 6. 与既有 TODO 的衔接

- 本计划里程碑 1-2 是 TODO §3.2-M44 v2 剩余目标的延续（B1→C1、A3），完成后退行
  TODO 对应拒绝分类明细；
- 里程碑 4 对应 TODO §3.2-M45（SLP + 循环展开）；
- 里程碑 3、5 对应 TODO §3.4-C / §3.4-A（非循环 NEON 实证结论）的低成本落地；
- TODO §3.4 排除项（批量 int↔float 转换、ld2/ld3/ld4 交错存取）不重复提议；
- 完成每项后，TODO.md 按约定压缩成一行摘要。
