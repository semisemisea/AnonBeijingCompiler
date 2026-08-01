# Cortex-A53 后端优化计划

本文档只记录尚未完成的工作。M1-M26 已完成，历史设计与实现细节以 Git 提交记录
和代码测试为准，不在这里重复维护。

已完成的能力概要：

- MIR pass 基础设施、发射前 finalize、ABI 参数布局共享（M1-M5）。
- pre-RA PeepholeCombine（MAC 融合）、post-RA PairCombine（LDP/STP）（M6）。
- 依赖 DAG、保守内存别名模型、基础 Cortex-A53 list scheduler（M7-M10）。
- 可切换 pipeline：`AArch64CodegenConfig`、`-O0/1/2` 映射、结构化统计（M11）。
- 共享 `CycleSimulator`、edge-latency critical path、确定性调度（M12）。
- `MInst::Removed` tombstone，消除 emitted Nop（M13）。
- 精确 Cortex-A53 slot/resource 模型、Div32/64、FP、pair 细分 profile（M14）。
- XCZU15EG PMU benchmark harness（M15，待实机运行）。
- Slot-filling dual-issue heuristic（M16）。
- 端到端验证门禁、确定性检查（M17）。
- pre-RA DCE（M18）：worklist use-count fixpoint，白名单制，`-O1` 起默认开启，
  `--enable/disable-mir-dce` 开关，`DceStats` 统计。huffman-01 上 `-O1`
  指令数 964 → 935（-3%）。
- 入口参数 fixed-register live-in（M19-M24）：`Args` 伪指令以 `reg_fixed_def`
  直接绑定寄存器参数，消除 ABI home-slot store/load 往返；post-RA 调度将
  `Args`/`RetVal` 建模为零周期 Nop；RA 并行拷贝与真实 spill 门禁；AArch64 +
  RISC-V × `-O0/1/2` ABI 矩阵与 5 次确定性；`AbiArgStats`/`RegallocStats`
  统计。纯叶函数 `add` 收敛为 `add w0, w0, w1; ret`（无 frame、无 str/ldr）。
- EmitBuffer 文本缓冲（M25）：`taki_mir` 通用层新增 `emit_buffer.rs`（Slot/
  BranchRef/LabelKind/BranchRec/别名链/labels_at_tail/latest_branches），
  发射流程改走 buffer（prologue/epilogue 同样经 buffer）；AArch64
  `CondBr/Cbz/Cbnz/Tbz/Tbnz/BCond/Jump` 改为结构化 Branch slot（2 指令形式，
  删除 `1f` 局部标号 hack），多指令 MInst（`LoadImm`/`LoadAddr`/`CmpSelect`）
  拆为逐指令 slot；RISC-V 文本输出逐字节不变。huffman-01 指令数 870 → 804
  （-8%），`-O0/1/2` × 双 target 全部确定性通过，全 corpus 编译+汇编通过。
- 分支优化四规则（M26）：`optimize_branches` 移植 Cranelift R1（fallthrough
  消除）/R2（标签穿线，防环）/R3（双 uncond 死跳删除）/R4（条件倒相合并），
  `LABEL_LIST_THRESHOLD` 防二次方；`-O` 门控（`-O0` 保留两指令形式作差分
  基线，`AArch64CodegenConfig::branch_opt`）；`BranchOptStats` 统计接入
  `compile_with_config`；修复 finish 渲染非单调 label offset 的缺陷（回归
  单测）。huffman-01 指令数 804 → 687（累计 -21%）：fallthrough=99、
  inverted=17、threaded=13、dead=1；R1-R4 专项单测 + abi_matrix 门禁通过。

备注：M27（veneer/resolve 松弛）在 M26 提交后已开始实现，工作区存在未提交
改动（`taki_mir/src/emit_buffer.rs`、`taki_mir/src/emit.rs`、
`anon_armv8/src/lower.rs`、`taki_mir/src/lower.rs`），尚未完成验证与提交。

目标硬件是 Xilinx XCZU15EG 上的 Cortex-A53 MPCore。

---

## 1. 当前实现基线

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
SSA → Inline → TCO 之后，固定点内：IPSCCP、SimplifyCFG、GVN、SR（强度
削减）、IfConversion、TCO、BooleanSimplification、GVNPRE、DeadPhiElim、
DCE。相关 pass 见 `raana_ir/src/opt/passes/`。

关键代码：

- `raana_ir/src/opt/passes/if_conversion.rs`：保守 if-conversion（3 种形状）。
- `anon_armv8/src/passes/mod.rs`：按 `AArch64CodegenConfig` 注册 MIR pass。
- `taki_mir/src/passes.rs`：`MIRPass` trait、pre-RA/post-RA 两阶段 pipeline。
- `anon_armv8/src/passes/peephole_combine.rs`：vreg use 计数 + MAC 融合。
- `anon_armv8/src/instructions.rs`：`MInst` 枚举（约 60 个 variant）。
- `anon_armv8/src/lower.rs`：ISel（`lower_select`/`select_branch_condition`）。
- `taki_mir/src/emit_buffer.rs`：EmitBuffer（M25 建，M26 分支规则，M27 WIP）。
- `taki_mir/src/emit.rs`：`AsmWriter::write_function` 逐块文本发射。
- `taki_mir/src/block_order.rs`：domtree RPO 块序（`lowered_order`）。
- `taki_mir/src/stats.rs`：函数级 / 编译单元级结构化统计。
- `soyo_compiler/src/cli.rs`：`-O` 映射与 `--enable/disable-*` 开关。

### 1.2 现状与差距总览（huffman-01 对照 clang -O2）

M26 基线：huffman-01 静态指令数 687（M25/M26 累计 -21%）。对照
`results/perf/huffman-01_clang.s`（clang -O2）与 `huffman-01.s`（本项目），
差距集中在 IR 层分支结构、后端标志融合、循环不变量与外提：

| 函数 | clang | 本项目 | 差距根因 |
|---|---|---|---|
| `_and/_xor/_or` 循环体（32 次迭代） | ~10 条/迭代：`ccmp`+`csel` 无分支 | ~19 条/迭代：`&&`/`\|\|` 分支+cset+二次分支；if 菱形分支+phi 拷贝；回边 5 条 `mov`；`sub;cmp;b.ne` 未融合 | if-conversion 保守；无 ccmp；无 subs/tst 融合；RA 拷贝未消除 |
| `rotrN/rotlN` | 二分比较树（最坏 ~3 次 cmp） | 8 次线性 cmp 链（内联后重复复制） | 无 if 链→switch/决策树 |
| `read_bits`（热点，2000×10⁵/5 调用） | 全局一次载入寄存器、出口统一写回；switch 表提取；无函数调用 | 循环内重复 `adrp+ldr` 全局；循环体 store 回写；热循环保留 `bl rotlN`（栈帧+8-cmp 链）；尾部内联 rotrN 8-cmp 链 | 缺 GSP/LICM；内联仅"单调用点"；无链→switch |
| `output_data` | `gv_out_num` 一次加载；尾调用 `b putch` | 重复加载 3 次；`bl putch`+栈帧 | 缺 load-CSE/GSP；TCO 未覆盖 if 链末尾调用 |
| `decode_fixed_huffman` | 等价结构 | 死空块跳转 `then_13: b while_entry_5` | simplify_cfg 缺口 |

根因分层：

- **IR 层（raana_ir）**：if_conversion 只支持 3 种保守形状，`reaches(merge,
  head)` 阻止循环累加器转换；land/lor 三角不折叠；无 LICM/GSP/load-CSE；
  内联仅"单调用点"；无 if 链→switch。
- **后端层（anon_armv8）**：无 `ccmp` 指令；`CmpSelect` 只支持直线
  cmp+csel/cset 配对；无 `subs`/`tst` 融合；`Mov` 按 64 位宽度发射。
- **RA 层（taki_mir ion 移植）**：回边 blockparam 拷贝未被 bundle 合并/
  冗余移动消除命中。

### 1.3 当前结论边界

- 静态 estimator 只用于确定性回归和相对启发式比较，不能替代实机测量。
- scheduler 与 estimator 共用同一模型，"模型内不退化"不等于"硬件不退化"。
- QEMU 仅用于语义差分，不能证明 A53 的 dual-issue / latency / throughput 收益。
- 所有 profile latency 为 ARM guide 推导值（DUI 0901），未经 XCZU15EG 实测校准。
- 未获得实机数据前，文档和提交信息只能声称"静态模型改进"，不能声称
  "XCZU15EG runtime 提升"。

---

## 2. 主计划 A：huffman 类基准的性能重构（M30-M38，对照 Cranelift 与 clang）

本计划与主计划 B（M27-M29 分支发射）无依赖，可并行。参考实现为
`../wasmtime/cranelift`；对 cranelift 明确不做、而 clang 做的部分
（if-conversion、ccmp、一般标量 subs 融合），以 clang 为参照实现，形成超越。

### 2.1 参考设计：Cranelift 的关键机制

| 机制 | cranelift 位置 | 移植落点 |
|---|---|---|
| `CondResult` 条件抽象（Zero/NotZero/Cond/And/Or） | `isa/aarch64/inst.isle:4882`（`emit_icmp`）、`lower.isle:2175`（`lower_cond_result_bool`）、`inst.isle:4847`（`cond_result_invert` De Morgan） | M32，anon_armv8 |
| flags 配对机制（`ProducesFlags`/`ConsumesFlags`/`ConsumesAndProducesFlags` + `with_flags*` 上下文） | `inst.isle:2663-2704`、`lower.isle:155-159` | M32/M33，anon_armv8 |
| aarch64 对 `&&`/`\|\|` 的 TODO（`br_cond_result` 仅回退 cset+and 物化） | `lower.isle:2137-2141` | M32 用 `ccmp` 超越（参照 clang） |
| egraph 内建 LICM（`elaborate_licm_hoist`，按 loop_stack 层级提升纯指令到前驱头） | `egraph/elaborate.rs:555-635` | M34，raana_ir |
| load 别名 CSE（`AliasAnalysis`+`LastStores`） | `egraph/mod.rs:546-551`、`alias_analysis.rs` | M34，raana_ir GVN 扩展 |
| `br_table` → 跳转表 | `lower.isle:3181-3190`、`inst.isle:5224`（`br_table_impl`） | M37，决策树+跳转表 |
| 内联代价模型（指令数估计 + 多调用点 + 递归深度限界） | `inline.rs` | M36，raana_ir |
| regalloc2 ion（bundle 合并/冗余移动/并行拷贝求解） | 已移植：`taki_mir/src/reg_alloc/ion/{merge,redundant_moves,moves}.rs` | M35，修复移植缺口 |
| 常量 phi / 死空块清理 | `remove_constant_phis.rs` | M38，simplify_cfg 增强 |
| cranelift 不做：if-conversion；一般标量 `subs` 融合 | 无对应 pass（aarch64 仅对 i128 addc/sbc 用 `with_flags`） | M31/M33，以 clang 为参照 |

### 2.2 目标架构

- **IR 层（raana_ir）**：`select` 成为一等公民——if-conversion 推广（循环
  累加器模式，dominance 判定投机安全）+ land/lor 三角折叠为 `band/bor`；
  GSP（标量全局提升）；LICM（自然循环 + 前驱头提升）；load-CSE（别名
  分析）；if 链→switch；内联代价模型。
- **后端层（anon_armv8）**：`CondResult` 抽象 + `CCmp` 指令（And/Or →
  `cmp; ccmp; csel/cset/b.cc`）；`subs`/`tst` 标志融合；switch 决策树/
  跳转表；TCO 尾调用。
- **RA 层（taki_mir）**：回边 blockparam 拷贝消除；`Mov` 32 位宽度。

### 2.3 关键不变量

1. 投机安全：if-conversion 只上提纯整数算术（排除 div/rem/副作用指令），
   且要求 head **支配** merge——不在未执行路径引入异常；
2. GSP 别名保守：函数内存在"可能触及该全局"的非白名单调用（白名单：内联
   后仅剩 runtime 调用如 `getarray`/`putch`/`starttime`）则不提升；
3. `ccmp` 只对单用、纯比较的 `band/bor` 条件生成；
4. `-O0` 保留分支形式作 on/off 差分基线；所有优化规则由 `-O1/-O2` 控制；
5. 每个 milestone 独立提交，完成后删除 TODO 细节只留一行历史；
6. 所有 AArch64 改动验证 RISC-V 不受影响（双 target 回归）；
7. 静态模型改进不声称实机收益（§1.3）。

### 2.4 里程碑

#### M30：基准与验证基建

- 已有：`make gem5-run` + `gem5/a53_se.py`（A53 模型统计 sim_insts）、
  `results/perf/` 全套产物。
- 新增：`scripts/perf_compare.sh`——对每个 milestone 自动产出 `.s` 指令数与
  gem5 `sim_insts` 对比表（`_orig`/`_sched`/`_clang` 对照）；raana 新 pass
  单测模板（仿 `if_conversion.rs` 测试结构）；记录 M31-M38 起点基线。
- 验收：一条命令产出全表；`cargo test --workspace` 全绿。

#### M31：if-conversion 推广 + land/lor 折叠（IR，最大单点收益）

- 文件：`raana_ir/src/opt/passes/if_conversion.rs`。
- 设计：
  1. **放宽 `reaches(merge, head)`（line 215）**：当 head 支配 merge、且所有
     被上提指令的操作数经 `available_at`（line 325）在 head 可用时允许转换
     ——覆盖循环累加器 `if (bit_a==1 && bit_b==1) result += power;`。
     `safe_arm_binary`（line 301）已排除 div/rem/副作用，仅 `reaches` 过保守。
     转换后 head 内为 `select(cond, result+power, result)`，merge 的 phi 由
     `apply()`（line 418-460）事务化删除。
  2. **新增第三候选形状：land/lor 三角** `br %c1, rhs, merge(0,..); rhs:
     %c2=…; jump merge(%c2,…)`，`%c1/%c2` 为 0/1 比较值 → 折叠为
     `band/bor(c1, c2)` 单指令，删 rhs 块（LLVM 语义；cranelift 无此 pass）。
  3. 之后由 `boolean_simplify` 归约 `select(cond,1,0)` → 比较本身。
- 验收：`_and/_xor/_or` 内循环无分支、无 phi 拷贝（先于 M32 每迭代减 ~8 条）；
  单测覆盖循环累加器与 `&&`/`||`；全 corpus 功能回归。

#### M32：CondResult + ccmp 后端机制

- 文件：`anon_armv8/src/lower.rs`（`lower_select`/`select_comparison`/
  `select_branch_condition`）、`anon_armv8/src/instructions.rs`、
  `anon_armv8/src/regs.rs`。
- 设计：
  1. 新增 `MInst::CCmp { size, lhs, rhs|imm, nzcv, cond }`；`CmpSelect` 扩展
     为链式（`cmp; ccmp…; csel/cset`，或新 `SelectCC`）；
  2. 移植 `CondResult` 抽象：`lower_select` 匹配 `cond = Band(b1,b2)/
     Bor(b1,b2)`（b 为单用纯比较，复用 `select_comparison` 的
     `sink_pure_single_use_chain` 机制）→ `cmp; ccmp; csel/cset`；
     `select_branch_condition` 同条件 → `cmp; ccmp; b.cc`；
  3. nzcv：And=`#0,eq`、Or=`#4,ne`（对照 clang `_and`/`_or`）；8/16 位、
     imm 操作数、反转路径补齐（移植 `cond_result_invert` 的 De Morgan）。
- 验收：`_and/_xor/_or` 内循环 ~10 条/迭代（与 clang 持平）；emit 单测
  （仿 `instructions.rs` `emits_adjacent_*` 系列）；`read_bits` 内联副本
  同样受益。

#### M33：标志融合 peephole

- 文件：`anon_armv8/src/passes/peephole_combine.rs`（现仅 MAdd/MSub 融合）、
  `instructions.rs`（`AluOp` 增 `Subs/Adds` flag 变体）。
- 设计（触发条件：结果单用、cmp 紧跟，仿 `combine_mac_in_block` 的 use
  计数）：
  - `sub r,#imm; cmp r,#0; b.cc` → `subs r,r,#imm; b.cc`；
  - `and r,r,#imm; cmp r,#0; b.eq/ne` → `tst r,#imm; b.*`；
  - 循环计数 `subs w8,w8,#1; b.ne`（消除 `_and` 等循环中的单独 cmp）。
- 验收：全部 `sub X,#1; cmp X,#0; b.ne` 消失；on/off 差分无行为差异。

#### M34：GSP + LICM + load-CSE（IR 层）

- 文件：`raana_ir/src/opt/passes/` 新增 `scalar_global_promotion.rs`、
  `licm.rs`；改 `gvn.rs`。
- 设计：
  1. **GSP**：无 `getelemptr`/取址、函数内无"可能触及该全局"的非白名单调用
     的标量全局 → load 变 SSA 参数、store 变 def、函数出口统一回写
     （clang 对 `bits/pos/size` 正是此形态：寄存器保持、出口一次写回）；
  2. **LICM**：新增 IR 级 domtree + 自然循环分析（仿 cranelift
     `loop_analysis.rs`），把纯不变量指令提升到循环前驱头（仿
     `elaborate.rs:555-635` 的 loop_stack 层级选择）；GSP 后全局读取已是
     SSA，天然可提升；
  3. **load-CSE**：GVN 扩展 load 值编号 + 简单别名分析（仿
     `alias_analysis.rs` `LastStores`）：无 intervening may-alias store 时
     同址 load 合并。
- 验收：`read_bits` 循环内无 `adrp/ldr` 重载、循环体内无 store（入口一次
  载入、出口一次回写）；`output_data` 全局只加载一次。

#### M35：RA 回边拷贝消除 + Mov 宽度

- 文件：`taki_mir/src/lower.rs`（blockparam 拷贝生成，line 431-521）、
  `taki_mir/src/reg_alloc/ion/merge.rs`、`redundant_moves.rs`。
- 设计：先诊断回边 5 条 `mov` 来源（lower 层按前驱插入的拷贝 vs ion bundle
  合并未命中）；(a) 分配前轻量 copy-propagation 折叠纯 `Mov`（LLVM 式）；
  (b) 修 merge 的 blockparam-out 合并路径。附带：`Mov` 按 vreg 类型选 32 位
  宽度（`mov w0,w2` 而非 `mov x0,x2`）。
- 验收：`_and/_xor/_or` 回边零 mov；5 次确定性门禁。

#### M36：内联代价模型

- 文件：`raana_ir/src/opt/passes/inline.rs`（现"仅单调用点"）。
- 设计：改为 cranelift `inline.rs` 风格：指令数代价估计 + 允许多调用点 +
  递归环按深度限界（保留 `does_not_inline_across_a_recursive_call_cycle`
  语义）；目标：`read_bits` 内 `rotlN` 的 3 处调用全部内联 → 热循环无
  `bl`/栈帧；内联后由定点管线 IPSCCP/GVN 折叠 `rotlN(1,5)` → `lsl #5`。
- 验收：`read_bits` 无 `bl rotlN`；corpus 编译时间与代码体积回归监控。

#### M37：if 链 → switch 决策树

- 文件：`raana_ir/src/opt/passes/` 新增 `chain_to_switch.rs`；
  `anon_armv8/src/lower.rs`。
- 设计：值域连续的 `if (x==k) return f(k);` 链 → `switch`（跨基本块值流
  分析）；后端小稠密 → 二分决策树（clang `rotrN` 形态，最坏 ~3 次 cmp），
  大 → 跳转表（仿 cranelift `br_table_impl`）。
- 验收：`rotrN/rotlN` 内联体最坏 3 次 cmp。

#### M38：TCO 与死块清理收尾

- 文件：`raana_ir/src/opt/passes/tco.rs`、`simplify_cfg.rs`。
- 设计：`output_data` 末尾 `bl putch` → 尾调用 `b putch`（扩展 TCO 对
  "if 链末尾调用"的可达性分析）；清 `then_13: b while_entry_5` 类死空块
  跳转（仿 cranelift `remove_constant_phis.rs`）。
- 验收：`output_data` 无栈帧、尾调用形式；无死空块跳转。

### 2.5 预计文件范围

核心修改：

- `raana_ir/src/opt/passes/if_conversion.rs`（M31）
- `raana_ir/src/opt/passes/` 新增 `scalar_global_promotion.rs`、
  `licm.rs`、`chain_to_switch.rs`（M34/M37）
- `raana_ir/src/opt/passes/{gvn.rs, inline.rs, tco.rs, simplify_cfg.rs}`
  （M34/M36/M38）
- `anon_armv8/src/instructions.rs`、`lower.rs`、`regs.rs`（M32/M33/M37）
- `anon_armv8/src/passes/peephole_combine.rs`（M33）
- `taki_mir/src/lower.rs`、`taki_mir/src/reg_alloc/ion/merge.rs`（M35）

机械适配：

- 其他对 `MInst` 做 exhaustive match 的位置（新增 CCmp/flag 变体后）
- `uika_riscv/src/instructions.rs`（确认不受 M32/M33 影响）

测试与统计：

- `raana_ir` 各新 pass 的单测（仿 `if_conversion.rs` tests）
- `anon_armv8` emit 单测（仿 `instructions.rs` `emits_adjacent_*`）
- `tests/` functional cases、`benchmarks/` 性能差分、`abi_matrix` 双 target
  回归、`scripts/perf_compare.sh` 对比表

### 2.6 验收标准（总）

- `cargo test --workspace` 全通过；AArch64 + RISC-V × `-O0/1/2` 编译成功且
  5 次 byte-identical；on/off 差分无行为差异。
- huffman-01：`_and/_xor/_or` 内循环 ~10 条/迭代；`read_bits` 无全局重载、
  无 `bl rotlN`、无栈帧；回边零 mov；`rotrN/rotlN` 内联体最坏 3 次 cmp；
  `output_data` 尾调用形式。
- 静态指令数与 gem5 `sim_insts` 相对 M26 基线（687）继续下降；按 §1.3
  原则记录，不声称实机收益。

---

## 3. 主计划 B：分支发射重构（M27-M29，对照 Cranelift MachBuffer）

### 3.1 遗留局限（M26 之后）

1. 无偏移/范围概念：分支假定目标都在对应 `LabelKind` 范围内，超范围会
   汇编失败（当前 corpus 未触发，但无保证）——M27 加 veneer 兜底；
2. RISC-V `CondBr` 仍是 5 条 `la t6, X; jr t6` trampoline + `1f` hack
   ——M28 改为 slot；
3. 冷块沉底未做（只在 `BlockLoweringOrder` 预留 `is_cold()` 接口）。

### 3.2 参考实现：Cranelift 的 MachBuffer

核心在 `../wasmtime/cranelift/codegen/src/machinst/buffer.rs`，模块注释
（1-107 行）本身就是设计文档。要点（M25/M26 已移植部分略）：

- **latest-branches 窥视栈 + 四规则**（`optimize_branches`，`:999-1271`）：
  R1 fallthrough 消除（`:1057`）、R2 标签别名/穿线（`:1137`，防环 `:1170`）、
  R3 双无条件下冗余删除（`:1207`）、R4 条件+无条件翻转（`:1222`，
  `add_cond_branch(..., inverted)` `:856-890`）——M26 已完成，RISC-V 未适配。
- **范围与 veneer**：`LabelUse` trait（AArch64 `inst/mod.rs:2937-2956`）：
  Branch14（tbz，±1MB）、Branch19（b.cond/cbz，±1MB）、Branch26（b/bl，
  ±128MB）；`deadline`/`island`/`emit_veneer` 机制（`buffer.rs:142-210,
  1310-1329, 1544-1567`）——M27 主体。
- **VCode 驱动**（`vcode.rs:736-1132`）：冷块沉底（`final_order` +
  `cold_blocks`）→ `bind_label`（触发 `optimize_branches`）→ `island_needed`
  前瞻 → `finish()` 解析 fixup。

我们发射文本 .s 且指令定长 4B，因此比 Cranelift 简单：无需字节 fixup/patch
与 deadline/island 前瞻，偏移**精确可算**，用单调松弛循环即可。

### 3.3 veneer 与范围松弛设计（M27 主体，已定决策）

- 第一阶段：所有分支按 `Branch19/26` 直接发射，不做 trampoline；
- 第二阶段（resolve）：slot 定长（4B）→ 精确计算每标签偏移；找出超范围
  的条件分支；
- 第三阶段：超范围分支改发两指令形式（条件+无条件），veneer 插在**分支
  自身之后**——分支是块终结符，之后必然是块边界，无 fallthrough 进入；
  若该分支已被优化成单条件且另一目标为 fallthrough，则先强制两指令形式
  再插 veneer；
- 第四阶段：重算偏移，重复直至稳定（单调收敛，通常 1-2 轮）。此循环替代
  Cranelift 的 deadline/island 前瞻，定长指令下完全等价且更简单。

### 3.4 里程碑

#### M27：范围检查 + veneer 松弛（进行中，工作区未提交）

- 工作区已有：`Slot::Veneer(VeneerRec)`、`LabelRef::{Block,Veneer}`、
  `LabelKind::in_range`、`slot_len`、veneer 符号命名（`.L{func}_veneer_{id}`）。
- 待完成：`resolve` 松弛循环（四阶段）、Branch14/19/26 范围驱动、装配器
  集成（`taki_mir/src/emit.rs`、`anon_armv8/src/lower.rs`）。
- 验收：合成 >1MB 代码块用例验证 veneer 正确且松弛收敛（≤3 轮）；QEMU
  差分通过；全 benchmark 编译成功无汇编器超范围报错；完成后独立提交。

#### M28：RISC-V 适配

- `CondBr` → `beqz/bnez` 倒相 slot；`LabelKind`：B-type ±4KB / JAL ±1MB；
  veneer 用 `la t6,X; jr t6`；清理 `1f` hack。
- 验收：RISC-V 全部用例 5 次 byte-identical；小用例 asm 检查 `CondBr`
  收敛为 1 条 `beqz/bnez`；QEMU 差分通过；`abi_matrix` 双 target 回归。

#### M29：测试、门禁与收尾

- 移植 Cranelift buffer 行为测试思路：fallthrough 消除、条件翻转、穿线链、
  别名防环、超范围 veneer、截断后标签簿记。
- 确定性门禁、on/off 差分、QEMU 语义差分纳入 `tests/`；性能差分表记录
  静态模型改进（按 §1.3 原则）。
- 遗留项登记：冷块沉底、`CmpImm(0)+CondBr{Ne}`→`cbz` 融合。

---

## 4. 后续候选工作

按预期收益排序，均需在前一项验证后再启动。被主计划 A 覆盖的旧条目
（`&&`/`||` flags 融合、phi 拷贝 coalescing、循环不变 load 外提）已并入
M31-M35，不再单列。

### P1：常量 / 分支参数物化源头治理

DCE 是兜底；更优解是 lowering 时就不为未使用的 block param 和分支参数物化
常量与 `mov wzr`。DCE 落地后统计 `instructions_removed` 的构成，若某类来源
占主导，直接在 `taki_mir/src/lower.rs` 或 `anon_armv8/src/lower.rs` 消除源头。

### P2：调度验证器闭环

- `verify_operand_order_stable`：保护 pre-RA pass 的 operand traversal
  contract。
- `verify_sched_deps`：独立于 scheduler 重放调度后的 register、NZCV、
  memory 和 barrier 约束。
- 小 DAG reference simulator / property tests。

### P2：内存 DAG 复杂度

长块最坏 `O(M^2)`。已有统计，先采集编译时间数据，确认是实际问题后再引入
按 root/range 分组的数据结构。

### P2：调度启发式增强（需实机数据证明收益）

- Load-use latency hiding 专项。
- Pair-aware scheduling（调度时考虑 LDP/STP 形成）。
- post-RA register-pressure tie-break。
- pre-RA scheduler（需先证明 post-RA false dependency 是主要 ILP 限制）。

### P2：冷块沉底与布局

Cranelift `BlockLoweringOrder` 的 `cold_blocks` 机制（`blockorder.rs:87-90,
260-265`）把冷块沉到函数末尾；配合 M27-M29 的 EmitBuffer，冷块天然获得
fallthrough 收益。SysY 前端暂无冷热信息，本期仅在 `BlockLoweringOrder`
预留 `is_cold()` 接口。

### P3：XCZU15EG 实机校准（依赖硬件访问）

- 运行 `benchmarks/src/bench.c`，校准 latency / throughput / pairing 数据。
- 基于实测调整 guide-derived profile 值。
- 建立性能回归门禁。
- 回答：WAR/WAW/NZCV false dependency 是否允许 A53 同周期双发。
- 用 M19-M24 的参数入口 microbenchmark 与 M25-M29 的 huffman 差分量化
  实际收益（实机数字待测）。

---

## 5. 总体执行原则

1. 每个 milestone 独立提交；`TODO.md` 在 milestone 完成后删除对应已完成
   细节，只保留后续工作。
2. 正确性验证、静态模型测试和实机性能测量分层维护，不互相替代。
3. 所有 AArch64 改动必须同时验证 RISC-V 不受影响。
4. 新 pass 默认走"白名单 + 保守保留"策略，宁漏勿错。
5. 未获得实机数据前，只能声称"静态模型改进"。
6. ABI/codegen 架构改动（M19-M24、M25-M29、M30-M38）不由优化 flag 控制，
   任何优化级别都必须保持正确；优化规则本身由 `-O` 控制。
7. 发射层改造以行为等价为第一优先级，优化规则在等价基线上逐步开启。

---

## 6. 风险与缓解

| 风险 | 严重度 | 缓解措施 |
|------|--------|---------|
| if-conversion 投机上提改变执行语义（除 div/rem 外算术无副作用，风险低） | 中 | 仅纯整数算术 + head 支配 merge 才转换；on/off 差分 + 全量功能回归 |
| GSP 提升全局破坏跨函数可见性 / 与调用交互 | 高 | 白名单（仅无取址、无"可能触及"调用的标量全局）；出口统一回写；保守宁漏勿错 |
| `ccmp` 链破坏 NZCV 使用顺序（与现有 `CmpSelect` 邻接配对机制整合） | 中 | 条件仅限单用纯比较；emit 单测；on/off 差分 |
| RA 拷贝消除与并行拷贝求解器交互导致确定性回归 | 中 | 5 次 byte-identical 门禁；redundant_moves 语义保留 |
| 内联膨胀（多调用点 + 递归深度）增加编译时间与代码体积 | 中 | 代价估计 + 阈值 + 深度限界；corpus 编译时间监控 |
| 别名链成环 / 截断后标签簿记错误（M27） | 高 | 完整移植 Cranelift 不变量；专项单测；on/off 差分 |
| 多指令 MInst slot 化破坏"每 slot 4B"假设（M27） | 中 | slot 粒度 = 单条指令；verify 断言发射 slot 数 == 指令数 |
| veneer 插入改变偏移导致松弛不收敛（M27） | 中 | 单调性（只增不减）+ 最大迭代上限（≤3 轮）+ 每轮全量范围断言 |
| 分支优化与 post-RA ListScheduler 交互 | 低 | 调度在 vcode 层（块内），EmitBuffer 只在块边界截断，块内顺序不变 |
| 汇编器对超范围分支报错 | 低 | `resolve` 保证发射前所有分支在范围内；veneer 全覆盖 |
| RISC-V B-type ±4KB 范围触发大量 veneer | 低 | veneer 仅在超范围时触发，`la+jr` 4 条/veneer |
| DCE 误删有隐式副作用的指令（flags、内存、call） | 高 | 白名单制；无 def 指令一律跳过；全量功能回归；on/off 差分 |
| 缺失 register/NZCV/memory 依赖导致调度误编译 | 高 | 保守 catch-all barrier、on/off 差分、`verify_sched_deps`（待做） |
| 发射层改造破坏 RISC-V 或 tail-call | 中 | M28 双 target + tail-call 矩阵回归；`ArgSlot` 布局不变 |
| QEMU wall time 被误用为 A53 性能数据 | 中 | QEMU 仅进入 correctness gate |
| 实机环境频率/温度噪声掩盖结果 | 中 | core pinning、paired samples、95% CI、环境元数据 |
