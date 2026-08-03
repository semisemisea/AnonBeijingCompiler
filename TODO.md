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
- veneer 范围松弛（M27）：`LabelKind` 增加 `in_range(from,to)`（BRANCH14/
  19/26 与 RISC-V B/JAL 常量），每分支 slot 携带 reach；`resolve()` 对
  超范围分支快照收集后从后向前插入 veneer（条件分支倒相后指向 veneer
  end 标签使 false 路径跳过 veneer、无条件分支指向 veneer 起始标签），
  同步移位其后的 `label_offsets`，逐轮重算偏移至稳定；后端提供
  `veneer_lines`（AArch64：`b`；`adrp+add+br x16` 兜底）。单测覆盖前后向、
  多 veneer 小 reach 收敛、>1MB BRANCH19 用例与标签移位渲染；全 corpus
  QEMU 差分通过。
- 基准与验证基建（M30）：`scripts/perf_compare.sh` 一键产出每 milestone
   的 `.s` 指令数对比表（current/orig/sched/clang + gem5 sim_insts 列，
   统计方法统一为 awk 指令计数，静态数字仅作模型级回归）；M31-M38 起点
   基线记录在 `results/perf_compare/`。
- if-conversion 推广 + land/lor 折叠（M31）：`finish_candidate` 的
   `reaches(merge, head)` 守卫放宽为 `head` 支配 `merge`（循环累加器
   `if (bit_a==1 && bit_b==1) result += power` 由分支 + phi 拷贝转为
   `select`）；三角 arm 从单条指令推广为单用链（`and`+`eq` 等），新增
   第三候选形状：`br c1, rhs, merge(0)` / `br c1, merge(c1), rhs` 折叠为
   `band/bor(c1, c2)` 并删 rhs 块（要求 0/1 比较值）。huffman-01
   706 → 686：`_and/_or/_xor` 循环体无分支（`cset`+`band/bor`+`csel`），
   与 clang 结构一致；与 clang 的差距（`ccmp`、`subs` 融合）留给 M32/M33。
   单测覆盖累加器、land/lor 折叠、链式 arm；abi_matrix 的 R4 用例改用
   call-arm 形状；全 corpus QEMU 差分通过。
- CondResult + ccmp 后端机制（M32）：`taki_mir::lower` 新增
   `sink_pure_single_use_pair` 原子下沉两个单用纯比较；`anon_armv8` 新增
   `MInst::CCmp { size, lhs, rhs|imm, nzcv, cond }` 与
   `CmpSelect::ccmp`（链式 `cmp; ccmp…; csel/cset`），`lower_select`/
   `select_branch_condition` 对 `band/bor(b1,b2)`（单用纯比较）生成
   `cmp; ccmp; csel/b.cc`，And=`#0,eq`、Or=`#4,ne`（对照 clang `_and`/
   `_or`），branch 路径用 `cond_result_invert` 的 De Morgan 反转。
   踩坑两处：(a) `ccmp` 立即数是 5 位（0..=31）而非 12 位，超范围需
   `movz`+寄存器回退（`ccmp_operands`）；(b) `nzcv_making_cond_false(Le)`
   原为 `#8`（N=1,V=0 → `N!=V` → LE 真）应落 `#0`（Z=0、N=V），否则
    `while (ch >= 48 && ch <= 57)` 的 and 链在 ccmp 未执行时误入数字循环
   体导致 SIGSEGV（BFS/DFS/DSU 回归）。新增 `ccmp_nzcv_fallbacks_*`
   单测逐一验证 14 条件 × true/false 回退值；huffman-01 686 → 640
   （`_and/_xor/_or` 内循环 `cmp; ccmp; csel`，8 条/迭代与 clang 持平）；
   全 corpus QEMU 差分、40/40 h_functional、109/109 functional 通过。
- 标志融合 + 循环旋转（M33）：IR 层新增 `rotate_loops`（循环旋转：
   头测试下沉到 latch，`header: br v, body, exit` 改为
   `latch: br v', header(v'), exit`；仅当所有非回边 pred 传入经证明
   非零常量时删除头测试，否则宁漏勿错）。后端 peephole 新增
   `SubsRRImm12`/`AndsRRImmLogic`/`TstRRImmLogic` 变体与三条融合规则：
   (a) `sub r,#imm; cmp r,#0; b.cc` → `subs`（仅 Eq/Ne/Mi/Pl，subs 不保
   C/V）；(b) `and r,r,#imm; cmp r,#0` → `ands`（结果存活）或 `tst`
   （结果死；排除 Hs/Lo/Hi/Ls）；(c) 回边 latch 的 `sub r,r,#imm; b T`
   + `T: cmp r,#0; b.cc` → `subs`（验证 T 的块参数即 sub 结果，flags
   跨块边由调度器 NZCV-WAW/RAW 边保证顺序）。huffman-01
   `_and/_xor/_or` 循环体 `lsl; subs wX,wX,#1; b.eq/ne` 与 clang
   `adds; b.lo` 同构；单测覆盖 IR 旋转（常量入口/零入口拒绝）与三条
   融合（含 tst vs ands、条件排除）。已知遗留：h_functional
   35_math.sy 在 -O2 下为旧有 FP 分歧（负参数 Newton 迭代混沌发散，
   M30-M33 行为一致，-O0 通过，非本里程碑引入）。
- GSP + LICM + load-CSE（M34）：`scalar_global_promotion` 把不可观测的
   标量全局以 SSA 形式穿过函数（入口一次载入、出口统一回写；call-graph
   "可能触及"分析 + 无取址 + 标量检查，宁漏勿错）；`licm` 对单前驱头
   自然循环提升纯不变量指令（块参数仅当所有入边同值才视为不变量，提升
   时替换为入边值）；`gvn` 增加作用域化 load-CSE（任何 store/call 使
   leader 失效）。huffman-01 648 → 599：`read_bits` 循环内零
   `adrp/ldr`/store（入口一次载入、出口一次回写，149 条 < clang 161）；
   `output_data` 全局只加载一次；abi_matrix R4 用例因写回布局改变改断言
   ccmp 链 + 写回；workspace 298、functional/h_functional ×
   -O0/1/2、RISC-V 全通过。

备注：M27/M30/M31/M32/M33/M34 已完成并独立提交；M28（RISC-V slot 化）待做。

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
- `taki_mir/src/emit_buffer.rs`：EmitBuffer（M25 建，M26 分支规则，M27 veneer）。
- `taki_mir/src/emit.rs`：`AsmWriter::write_function` 逐块文本发射。
- `taki_mir/src/block_order.rs`：domtree RPO 块序（`lowered_order`）。
- `taki_mir/src/stats.rs`：函数级 / 编译单元级结构化统计。
- `soyo_compiler/src/cli.rs`：`-O` 映射与 `--enable/disable-*` 开关。

### 1.2 现状与差距总览（huffman-01 对照 clang -O2）

M34 基线：huffman-01 静态指令数 599（awk 方法，M33 基线 648）。对照
`results/perf/huffman-01_clang.s`（clang -O2），差距集中在 RA 拷贝、
内联与决策树：

| 函数 | clang | 本项目 | 差距根因 |
|---|---|---|---|
| `_and/_xor/_or` 循环体（32 次迭代） | ~10 条/迭代：`ccmp`+`csel` 无分支，回边 `adds;b.lo` | 8 条/迭代：`cmp`+`ccmp`+`csel` + 回边 `subs;b.eq`（M33 后与 clang 同构） | 回边 blockparam 拷贝（M35） |
| `rotrN/rotlN` | 二分比较树（最坏 ~3 次 cmp） | 8 次线性 cmp 链（内联后重复复制） | 无 if 链→switch/决策树（M37） |
| `read_bits`（热点，2000×10⁵/5 调用） | 全局一次载入寄存器、出口统一写回；switch 表提取；无函数调用 | 149 条（< clang 161）：入口一次载入、出口一次回写；热循环保留 `bl rotlN`（栈帧+8-cmp 链） | 内联仅"单调用点"（M36） |
| `output_data` | `gv_out_num` 一次加载；尾调用 `b putch` | `gv_out_num` 一次加载一次写回（M34）；`bl putch`+栈帧 | TCO 未覆盖 if 链末尾调用（M38） |
| `decode_fixed_huffman` | 等价结构 | 死空块跳转 `then_13: b while_entry_5` | simplify_cfg 缺口（M38） |

根因分层（M32 已修后端 `ccmp`，M33 已修循环计数 `subs` 融合，M34 已修
GSP/LICM/load-CSE，M36 已修内联代价模型，M37 已修 if 链决策树，余下）：

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

#### M30：基准与验证基建（已完成）

- `scripts/perf_compare.sh` 一键产出 `.s` 指令数对比表（current/orig/
  sched/clang + gem5 sim_insts 列，gem5 统计复用 harness 产物）；
  M31-M38 起点基线在 `results/perf_compare/`；`cargo test --workspace`
  全绿。已完成独立提交。

#### M31：if-conversion 推广 + land/lor 折叠（已完成）

- `reaches(merge, head)` 放宽为 `head` 支配 `merge`（循环累加器转
  `select`）；三角 arm 推广为单用链；新增 land/lor 折叠为
  `band/bor(c1, c2)`（要求 0/1 比较值），删 rhs 块。
- 结果：huffman-01 706 → 686；`_and/_or/_xor` 循环体无分支；
  单测覆盖累加器、land/lor、链式 arm；abi_matrix R4 用例改用 call-arm
  形状；全 corpus QEMU 差分通过。已完成独立提交（93f43a8）。

#### M32：CondResult + ccmp 后端机制（已完成）

- `MInst::CCmp` + `CmpSelect::ccmp` 链式生成；`lower_select`/
  `select_branch_condition` 对单用纯比较的 `band/bor` 生成
  `cmp; ccmp; csel/b.cc`（And=`#0,eq`、Or=`#4,ne`）。
- 踩坑：ccmp 立即数仅 5 位（超范围 `movz`+寄存器回退）；`Le` 的
  `nzcv_making_cond_false` 应为 `#0`（BFS/DFS/DSU SIGSEGV 根因）。
- 结果：huffman-01 686 → 640；`_and/_xor/_or` 内循环 8 条/迭代与 clang
  持平；`ccmp_nzcv_fallbacks` 单测覆盖 14 条件；全 corpus QEMU 差分、
  40/40 h_functional、109/109 functional 通过。已独立提交。

#### M33：标志融合 + 循环旋转（已完成）

- IR 层新增 `rotate_loops`（raana_ir）：`while (v) { body; v = f(v); }`
  中非回边入口传非零常量时，头测试下沉到 latch（`br v', header(v'), exit`），
  头块退化为直通；入口值不可证非零则拒绝旋转。
- 后端新增 `SubsRRImm12`/`AndsRRImmLogic`/`TstRRImmLogic` 与三条融合规则
  （块内 sub/and + `cmp r,#0` + CondBr；回边 latch + 测试块的跨块 subs）。
- 结果：`_and/_xor/_or` 回边 `subs wX,wX,#1; b.eq/ne`（clang 同构）；
  单测覆盖旋转与融合；RISC-V × functional/h_functional 无回归；
  35_math.sy -O2 FP 分歧为 M30 起旧有（-O0 通过）。已独立提交。

#### M34：GSP + LICM + load-CSE（已完成）

- **GSP**（`scalar_global_promotion.rs`）：程序级"可能触及"分析（call
  graph 传递闭包）；仅标量、无取址、被调用方不触及的全局，把值以 SSA
  形式穿过函数（入口一次载入、store 变 def、每个 return 前统一回写），
  用与 SSA pass 相同的 dom-frontier + 参数插入 + domtree 前序值栈穿线。
  陷阱：值栈的 store 压栈须在子树结束时弹出；`LocalBuilder` 无法寻址
  全局，需用 program-aware arena 建 load/store。`read_bits` 循环体不再
  有 `adrp/ldr` 与 store（入口 4 个全局一次载入、出口一次回写，与 clang
  同构）；`output_data` 的 `gv_out_num` 只加载一次。
- **LICM**（`licm.rs`）：自然循环（`prece[h]` 前驱中 `h` 支配 `m` 的
  回边）+ 回边反向工作列表求循环体；单一非循环前驱头；纯
  Binary/Cast 且操作数不变量的指令提升；块参数仅当所有入边传同一不变
  量指令时视为不变量，**提升时把参数替换为入边值**（否则提升出的指令
  引用只在循环边上定义的值，破坏 SSA 支配性）。
- **load-CSE**（`gvn.rs`）：作用域化 load leader（按地址指令）+ 全局
  store 计数器（任何 store/call 使 leader 失效）；同址、无介入 store 的
  load 合并。
- 结果：huffman-01 648 → 599（-49）；`read_bits` 149 条 vs clang 161；
  abi_matrix 的 R4 用例因 GSP 写回改变布局改为断言 ccmp 链 + 写回；
  workspace 298、functional/h_functional × -O0/1/2、RISC-V 全通过
  （35_math.sy -O2 FP 分歧仍为旧有）。已独立提交。

#### M35：RA 回边拷贝消除 + Mov 宽度（已完成 Mov 宽度；回边拷贝已完成诊断）

- **Mov 宽度（已完成）**：`Edit::Move` 携带目标 vreg 索引（ion 局部编号在
  `ion::run` 出口经 `original_vreg` 映射回原函数编号），`finalize_for_emission`
  按 `vreg_types` 选宽度：i32 拷贝发射 `mov w,w`（清零上半），不再一律
  `mov x,x`。`_and/_xor/_or` 回边拷贝 3 条均为 `mov w,w`。
- **回边 blockparam 拷贝（诊断结论）**：ion `merge_vreg_bundles` 的
  blockparam-out 合并路径已被触发且逻辑正确；对旋转后循环体，
  from-vreg（新值，如 `asr` 结果 [10,24]）与 param（旧值，[8,13+]）的
  活区间**真实相交**——`bit_a = a%2` 在旋转（`a/2`）之后才读旧 `a`，
  旧值必须活到 `and`，新值在 `asr` 即定义 → 不能同寄存器，拷贝是语义
  必需的。M33 时代 1 条拷贝是因为循环体首条 `mov x5,x3` 提前改名旧值
  缩短其活区间（代价是该 mov 本身）。消除路径：(a) 循环体重排——把
  旧值读取（bit 计算）提到新值定义之前（IR/MIR 层，可使回边零 mov）；
  (b) ion 活区间按块参数 in/out 拷贝分裂（regalloc2 的 half-move 语义）。
- 验收（调整）：i32 拷贝宽度正确（5 次确定性门禁）；回边拷贝数如实
  记录，不夸大；`_and` 循环回边 3 条 `mov w,w`（M33 时代为 2 条
  `mov x,x` 含体首改名拷贝，净指令数 24 对 22 差在入口多 1 条 + 拷贝
  宽度变 w）。

#### M36：内联代价模型（已完成）

- 文件：`raana_ir/src/opt/passes/inline.rs`。
- 设计（仿 cranelift `inline.rs` 代价估计）：移除"仅单调用点"限制，改为
  `estimate_size`（块内指令数之和）与调用点数（`be_called_at` 收集）的
  预算：单调用点 helper 只要 `size ≤ 40` 即内联；多调用点函数仅当
  `size × 调用点数 ≤ 100` 才内联（防多调用点叶函数膨胀程序）。递归环
  守卫（`reaches(callee, caller)`）保持不变，保留
  `does_not_inline_across_a_recursive_call_cycle` 语义。
- 结果：`read_bits` 内 `rotlN` 的 2 处调用全部内联（8-cmp 链 ×2），热循环
  无 `bl`、无栈帧（无调用即无需 callee-saved 保存/恢复），出口一次写回
  保留；huffman-01 599 → 580（-19）；内联后 `rotlN(1,5)` 常量折叠留待
  M37 决策树。corpus 编译时间与代码体积无异常放大。

#### M37：if 链 → switch 决策树（已完成）

- 文件：`raana_ir/src/opt/passes/chain_to_switch.rs`；
  `anon_armv8/src/passes/chain_fusion.rs`。
- **IR 层**：检测 `%t = eq x, k; br %t, handler, next` 线性链（每块仅
  测试+终结符、常量互异、长度 ≥ 4、非头块无参数），转为平衡决策树：
  内部节点为 (check, split) 块对（`eq x, k → handler` / `lt x, k →
  左子树, 右子树`），叶子 `eq → handler` 否则落 default；链头就地成为
  树根（保留其参数——函数参数或内联克隆参数——入边无需改接）。
- **后端**（`chain_fusion`，pre-RA，`-O1/2` 开启）：split 块（单前驱、
  恰好 `CmpImm(x,k); CondBr`）的比较与 check 块同值同寄存器时删除——
  其分支改读 check 块的标志（跨块 NZCV，分支不破坏标志），形成 clang
  形态 `cmp; b.eq case; b.lt left; b.ge right`。
- 结果：`rotrN/rotlN`（8 case）最坏 4 次 cmp（= clang；8 case 的完美
  二叉树最坏深度即 4，验收"~3"按此如实记录），平均 2.6（线性 4.5、
  clang ~3.1）；`read_bits` 内联链同样成树。**静态指令数 580 → 600
  （+20）**：树的分裂块与叶子到 default 的边增加分支，静态变差、动态
  （cmp 深度与平均）变好；相对 M26 基线 687 仍下降。RISC-V 不注册
  `chain_to_switch`（其条件物化到寄存器，树无法摊销），pipeline 按
  target 选择（`aarch64_ref` vs `default_ref`）。

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

## 3. 主计划 B：分支发射重构（M28-M29，对照 Cranelift MachBuffer）

### 3.1 遗留局限（M27 之后）

1. RISC-V `CondBr` 仍是 5 条 `la t6, X; jr t6` trampoline + `1f` hack
   ——M28 改为 slot；
2. 冷块沉底未做（只在 `BlockLoweringOrder` 预留 `is_cold()` 接口）。

### 3.2 参考实现：Cranelift 的 MachBuffer

核心在 `../wasmtime/cranelift/codegen/src/machinst/buffer.rs`，模块注释
（1-107 行）本身就是设计文档。M25/M26/M27 已移植：EmitBuffer 文本 slot、
latest-branches 四规则（R1-R4）、`LabelKind` reach 与 veneer 松弛循环。
VCode 驱动（`vcode.rs:736-1132`）的冷块沉底与 island 前瞻未移植——我们
发射文本 .s 且指令定长 4B，偏移精确可算，M27 的单调松弛循环已覆盖
island 前瞻的功能。

### 3.3 里程碑

#### M27：范围检查 + veneer 松弛（已完成）

- `LabelKind::in_range` + BRANCH14/19/26 常量；`resolve()` 快照收集超范围
  分支、倒序插入 veneer（条件倒相指 end 标签、无条件指起始标签）、同步
  `label_offsets` 移位、逐轮松弛至稳定；后端 `veneer_lines`。单测覆盖
  前后向、多 veneer 收敛、>1MB BRANCH19、标签移位渲染；全 corpus QEMU
  差分通过；已独立提交（33ac708）。

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

## 7. RISC-V 栈参数非对齐访问（BOOM 实机 RE，QEMU 不可见）

### 现象

- Judge RISC-V 实机运行：`h_functional/39_fp_params` WA/RE（FPGA 输出
  "Failed"），其余 139/140 functional + 60/60 perf 全过。QEMU 下同一
  case 通过，输出哈希正确。
- 只有混合 32/64 位大量栈参数的函数受影响；纯 float / 纯 int /
  纯指针参数函数（`params_f40`、`params_f40_i24`、`params_fa40`）在
  QEMU 与实机均正常。

### 根因链

1. `taki_mir/src/abi.rs` `ArgLayoutPlanner::compute`（51-89 行）对栈参数
   密集打包：`stack_offset` 只按 `stack_slot_size(ty)` 累加，**无任何
   对齐填充**。
2. `uika_riscv/src/abi.rs:166-178` `compute_call_arg_loc` 传入
   `|ty| ty.size()`：float/int 槽 4 字节、指针槽 8 字节。于是跟在
   32 位参数后面的指针参数落在 `4 mod 8` 偏移上。
3. callee 侧经 `s0(=entry sp)` 读栈参数（`ld s7, 64(s0)`、`ld a3, 124(s0)`
   等），caller 侧经 `sp` 写 outgoing args（`sd a1, 1132(sp)` 等），两侧
   布局一致、取值正确——所以 QEMU 全对，**唯一症状是地址非对齐**。
4. 实测 `/tmp/39_fp_params.s`：131 处 64 位访问落在 `4 mod 8` 地址
   （`params_mix` 26 处 + `main` 105 处），32 位访问 0 处非对齐。
5. BOOM 硬件不支持非对齐 ld/sd（缺 M-mode trap handler 时直接异常），
   QEMU user-mode 静默放行 → 实机 RE / QEMU AC 的分歧。
6. 栈帧本身 16 对齐（432/640/928/1024/1504 均 16 的倍数），局部栈槽
   `allocate_stackslot` 按 `stack_align()` 逐对象 round 到 8 字节
   （`taki_mir/src/abi.rs:452-464`），局部区无此问题——因此只有
   多栈参函数中招。

### 附带问题：psABI 不合规

RISC-V psABI（riscv-cc.adoc Integer Calling Convention）明确定义：窄于
XLEN 的标量在栈上传参时 **widened to XLEN bits**（整数按符号扩展、
浮点上位未定义），RV64 即每个栈参数槽 8 字节、8 对齐，GCC/Clang 每参数
占满 8 字节。当前 4 字节密集打包既非对齐、又违反 widening 规则：与 GCC
编译的 callee 互调时不仅地址非对齐，槽位取值也会错位。目前 sysylib
函数参数 ≤ 2 个全走寄存器所以没暴露，属潜在隐患。

注意：`anon_armv8` 早就用了正确实现（`anon_armv8/src/abi.rs:133`
`|_| 8`，单测 `aapcs64_overflow_arguments_use_fixed_eight_byte_slots`
断言 [0,8,16]/24）。同一 `ArgLayoutPlanner`、同一设计意图，aarch64
写对了、riscv 写成了 `ty.size()`——这是 RISC-V 侧的孤立回归，不是
通用层缺陷。

### 候选方案

- A（唯一正确方案）：`uika_riscv/src/abi.rs:176` 的 `stack_slot_size`
  闭包从 `ty.size()` 改为 `|_| 8`（与 aarch64 完全一致，注释引 psABI
  widening 规则）。这不是"代价"：8 字节槽就是规范定义的唯一形态，
  40 float 调用参数区 128→256B 是合规布局本身，不是修复带来的开销。
  调用方/被调方/尾调用共用同一 planner 输出，偏移自动一致；float 栈
  参数仍以 sw/flw 存取低 4 字节，上位未定义，规范允许，无需改存取宽度。
  性能影响为零（访存指令数不变，仅帧多 4B × 栈上 32 位参数数）。
- B（被规范否决）：只做 `align_up` 不统一槽宽。即使消除非对齐，float
  栈参槽 4 字节仍违反 widened-to-XLEN 规则，与 GCC 编译的 callee 互调
  取值错误；且混合步长布局（槽序 ≠ 偏移/8）难推理、易再错。淘汰。
- A 需同步更新 `uika_riscv/src/abi.rs:465-481`
  `argument_layout_preserves_scalar_stack_widths`：断言
  `[0, 8, 12, 16]` / `stack_size 24` → `[0, 8, 16, 24]` / `32`。
- 回归 tail-call 路径（`uika_riscv/src/lower.rs:1016-1035` 复用同一
  ArgSlot 布局）与 `abi_matrix` 门禁。

### 验证计划

1. 单测：构造 `compute_call_arg_loc` 混合类型序列（如 9×i32 + 8×f32 +
   指针），断言所有 64 位槽 offset % 8 == 0、32 位槽 offset % 4 == 0。
2. `make test-riscv h_functional/39_fp_params.sy`（或全量）QEMU 通过；
   静态检查 `.s`：`ld/sd/fld/fsd` 偏移全部 8 对齐（脚本扫描）。
3. 确认 `make test`（aarch64）无回归。
4. 有 FPGA 通道时实机复跑 39_fp_params。

状态：已实现（`|_| Self::word_bytes()` + 单测更新，见 git diff）。QEMU
单测与 riscv functional+h_functional 全量通过，39_fp_params.s 非对齐
131→0 处；FPGA 实机复跑仍待验证。

---

## 8. RISC-V 跑分长耗时用例分析（judge_rv64_8_2_03_00）

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
- A7 基址溢出到栈每轮重载：transpose2 matrix 基址 `ld 0(sp)` 每轮 2 次、
  fft1 数组基址每轮 1 次（寄存器压力导致 spill）。

### 用例特定

- B8 内层循环调用未内联：
  - fft1：蝶形内层每元素 2-3 次 `call multiply`，multiply 为递归倍增
    模乘（b 减半 ~30 层，每层 32B 帧 + 4 对 sd/ld）——fft1 绝对热点。
  - huffman-01/02/03：每符号 `call read_bits_specialized_2`。
  - crc1/2/3：每字节 `call crc32_specialized_0`。
  - 已走 specialization 机制但未内联进循环，查内联阈值/形状限制。
- B9 conv2d-1：边界检查 rr 半条件（只随 kr 变）未 hoist 出 kc 循环；
  cc>=0 用 `li 0 + slt + xori`；K[kr*5+kc] GEP 每轮重算。
- B10 knapsack_naive（指数递归）：零比较编译成 `li 0; subw; seqz` 三条
  （应为 `seqz` 一条）；`li 1; subw` 应为 `addiw`；每帧 5 对 sd/ld +
  48B。指数复杂度下每省 1 条都被 2^N 放大。
- 已达标项：h-4-03 常量除法全部 magic-mul（div=0，19 mul），剩余仅为
  A1/A2 循环开销。

### 每用例主导瓶颈映射

- many_mat_cal(106s)：A4a+A1+A2+A5+A6
- conv2d-1(58s)：B9+A1/A2/A4
- knapsack(40s)：B10+A1/A2
- matmul2/01_mm2(28/15s)：A4b/A4c/A4a+A1/A2
- transpose2(24s)：A4a+A7+A1
- sl2(17s)：A4a(7次/轮)+A2（运行时除法无法消除）
- crypto-1(11.5s)：A1(216)+A2(240)
- huffman(9.3s)：B8+A2(337)+A1(281)
- fft1(4.4s)：B8+A7
- crc(4.5s)：B8

### 优先级与候选方案（通用性 × 收益 × 风险）

- P0 发射层纯改进（覆盖所有热循环，每回边省 2+ 条）：A1 局部跳转
  la+jr→j（超范围走既有 veneer）；A2 li+ALU→立即数指令（addiw/slti，
  li 0 用 zero 寄存器）；A3 slt+beqz→blt。风险低，指令数可直接统计。
- P1 IR 层地址 SR 补全：元素地址指针递增、不变行基址 hoist、全局基址
  提升进循环前寄存器（覆盖 A4 全家 + A7 的基址重载）。
- P2 maddw 融合（M 扩展）；内层调用内联（huffman/crc/fft1，查
  specialization 未内联原因）；A6 累加器 phi 拷贝消除。
- P3 conv2d 边界检查半条件 hoist（依赖 LICM 条件部分提升能力）。

### 验证计划

1. P0 每项改造后：全 corpus 重编，脚本统计 .s 指令数下降 + 每循环回边
   指令数；QEMU 差分（`make test-riscv functional h_functional`）。
2. P1 用 sl2 / many_mat_cal / matmul2 的内层循环指令数（42→约 24 等）
   量化；BOOM 实机复跑头部用例确认（QEMU 时间不可作为性能依据）。
3. P2 内联用 huffman/crc/fft1 的 .s call 计数清零 + 实机耗时对比。
4. 所有优化保持通用触发条件，禁止按用例名/函数名匹配（AGENTS.md）。
