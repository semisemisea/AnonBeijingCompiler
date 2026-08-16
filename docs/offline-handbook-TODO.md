# Offline Handbook TODO（无 AI / 无网络工作手册）

独立于仓库根 TODO.md（那是编译器优化任务跟踪）。本文件只跟踪"离线工作手册"文档任务。

## 背景

即将在无 AI、无网络环境下工作（比赛现场/评审）。需要一整套自包含文档，让编译小白
能快速上手、普通人能维护本项目（SysY → AArch64/RISC-V 编译器，Rust，CSC 编译系统
设计赛）。

## 文档形式规则（硬性）

- **Workspace 内代码**（anon_armv8 / taki_mir / raana_ir / soyo_compiler /
  tomori_utils / uika_riscv 的 .rs 文件）→ 写 **rustdoc 注释**（`//!` 模块级、
  `///` 项级），保证 `cargo doc` 可生成。不新建重复 md。
- **Workspace 外的库**（如 lalrpop）与**脚本/方法论**（test.py、接口加在哪的判断
  方法、优化路线图）→ 独立 md，放 `docs/offline-handbook/`。
- 已存在的 `docs/anon.md` / `docs/soyo.md` / `docs/uika.md` 是设计文档，可提炼进
  rustdoc，但**不删除**。

## 评级机制（每一轮 turns 必做）

每个 goal 的**每一轮 turns 结束**，必须 `delegate_task` 启用一个子代理评级。
评级维度（二选一输出 1-5 分 + 具体改进建议）：
1. **编译小白能否快速上手**：术语是否解释、示例是否充分、路径/入口是否明确。
2. **普通人能否维护**：修改点是否清晰、副作用是否说明、是否依赖隐性知识。

子代理评级输入：该 goal 产出的文档全文 + 相关源码路径。输出追加到本文件
"评级记录"节。按建议修改后进入下一轮。

## 特殊要求（硬性）

- 每个 goal **至少需要两轮 turns 才可被判定完成**；第一轮 turns 结束时**不许**判完成。
- 每 goal 独立 commit，中文 message，`[Docs]:` 前缀，原子可验证。
- 纯文档任务：不碰代码逻辑（不改 .rs 行为），不 push，不碰 .docker-image。
- 全部完成前 docs/offline-handbook 分支不合并。

## Goal 列表

### G1 后端整体流程 —— anon_armv8 crate rustdoc
- 范围：`anon_armv8/src/lib.rs`（crate 级 `//!`）+ 各顶层模块 `//!` 概述。
- 内容：整体流水线（SysY → RaanaIR → taki_mir VCode/MIR → 寄存器分配 → AAPCS64
  帧 → GNU AArch64 汇编）、模块职责图（abi/config/constants/instructions/labels/
  lower/passes/regs/runtime/sched）、后端入口 `AArch64Backend` 调用链
  （lower → vcode → regalloc → emit）。
- 参考：`docs/anon.md` 第一、三节；`anon_armv8/src/lower.rs` 的
  `AArch64Backend`。
- 验收：`cargo doc -p anon_armv8` 无 warning；小白读完能复述后端 5 个阶段与
  入口调用链。
- 状态：`[~]` 两轮完成（初稿+评级+修正，待复评）

### G2 寄存器分配 —— taki_mir::reg_alloc (ion) rustdoc
- 范围：`taki_mir/src/reg_alloc/` 全部 13 文件（domtree/function/indexset/
  liveranges/merge/mod/moves/postorder/process/redundant_moves/reg_traversal/
  requirement/spill）。
- 内容：ION 算法整体流程（liveranges 构造 → merge → process → moves → spill）、
  `Function` 数据结构、每个子模块职责一句话、与 vcode 的接口
  （`allocate_registers` 入口签名与调用方）。
- 验收：`cargo doc -p taki_mir` 生成；小白能讲清"虚拟寄存器 → 物理寄存器"的
  完整路径。
- 状态：`[~]` 两轮完成（初稿+评级+修正，待复评）

### G3 vcode —— taki_mir::vcode rustdoc
- 范围：`taki_mir/src/vcode.rs`（MachInst/VCode/MachInstEmit）。
- 内容：VCode 数据结构（block/inst/operand 关系）、`MachInstEmit` trait、
  `MachInst` 表示法、"如何加一条新指令"的伪代码步骤（define inst → lowering →
  emit → regalloc 兼容性）。
- 验收：cargo doc 生成；小白照文档能加一条新指令。
- 状态：`[~]` 两轮完成（初稿+评级+修正，待复评）

### G4 emit —— taki_mir::emit + emit_buffer rustdoc
- 范围：`taki_mir/src/emit.rs`、`taki_mir/src/emit_buffer.rs`。
- 内容：`AsmWriter` 接口、emit_buffer 缓冲/刷出机制、指令 emit 流程、标签/缩进/
  对齐处理。
- 验收：cargo doc 生成；能讲清一条指令从 MachInst 到汇编文本的路径。
- 状态：`[~]` 两轮完成（初稿+评级+修正，待复评）

### G5 泛型与接口加在哪 —— 方法论 md + 相关 rustdoc
- 范围：新建 `docs/offline-handbook/interfaces.md` + 泛型热点文件的 rustdoc 补注
  （`taki_mir/src/register.rs` 的 RegClass、`anon_armv8/src/instructions.rs`、
  `anon_armv8/src/regs.rs` 的 Vector 类体系）。
- 内容：项目泛型分布总览；"新功能接口加在哪一层"的决策树（IR pass？MIR pass？
  lower？emit？）——每条路径的判定问题 + 示例（如：新优化→raana_ir opt/pass.rs；
  新指令→anon_armv8 instructions.rs + lower.rs；新 ABI 规则→abi.rs）。
- 验收：小白拿到新需求能按决策树定位到具体文件。
- 状态：`[~]` 两轮完成（初稿+评级+修正，待复评）

### G6 raana_ir rustdoc
- 范围：`raana_ir/src/lib.rs`（crate 级）+ `ir` / `opt` 模块。
- 内容：`ir`：Program/Function/Block/Inst 体系、Arena 结构；`opt`：`pass.rs`
  管线顺序与 `PassesManager::from_config`、passes/ 目录分类、analysis_passes、
  "如何加一个 pass"（注册 → config 门控 → 单测 → TargetPolicy 注意）。
- 参考：`docs/Convention.md`、`AGENTS.md` 的 IR 架构要点。
- 验收：cargo doc 生成；小白能加一个新 pass 并接入管线。
- 状态：`[~]` 两轮完成（初稿+评级+修正，待复评）

### G7 lalrpop 指南 —— 独立 md（外部库）
- 范围：新建 `docs/offline-handbook/lalrpop.md`。
- 内容：lalrpop 语法速查（token 定义、规则、优先级、错误恢复）、
  `soyo_compiler/src/sysy.lalrpop`（292 行）结构逐节讲解、"如何加一条语法规则"
  （.lalrpop → AST → RaanaIR 下降的完整接线）。
- 验收：小白能加一个新关键字/新语法并跑通。
- 状态：`[~]` 两轮完成（初稿+评级+修正，待复评）

### G8 测试 harness —— 独立 md
- 范围：新建 `docs/offline-handbook/test-harness.md`。
- 内容：`tests/test.py`（855 行）流程：每个用例编译两次（-S 汇编 + --emit ir）、
  combined_output（stdout+换行+returncode）比对、`results/` 产物布局、
  make test 各目标（test/test-riscv/test-llvm/test-baseline/run-elf/mca/gem5）、
  如何加测试用例、失败类型（WA/CE/RE/TLE）与排查路径。
- 参考：AGENTS.md 测试 harness 节。
- 验收：小白能加一个用例并解释三种失败类型的含义。
- 状态：`[~]` 两轮完成（初稿+评级+修正，待复评）

### G9 向量化可能的运算方法 —— rustdoc + md
- 范围：`anon_armv8/src/instructions.rs`（Vec* 指令族）、`anon_armv8/src/lower.rs`
  向量 lowering 路径、`anon_armv8/src/passes/`；新建
  `docs/offline-handbook/vectorization.md`。
- 内容：现有 SIMD 能力盘点（已支持指令/模式）；**未实现但可行的运算方法清单**：
  每条给原理、改哪层、伪代码/IR 形态、预期解锁用例（如 f32 fmls、整数 mla v.4s、
  VecSub 与 fmls 融合、掩码 select v3 等——参考 skill 的 SIMD gap 分析）。
- 验收：小白能挑一条清单项直接开工实现。
- 状态：`[~]` 两轮完成（初稿+评级+修正，待复评）

### G10 可能的优化与实现路线 —— 独立 md
- 范围：新建 `docs/offline-handbook/future-optimizations.md`。
- 内容：从 AGENTS.md / docs/ 提炼**未完成优化**路线图，每个优化一项：
  原理（IR/汇编示例）、改哪层、实现步骤（原子拆分）、预期收益与验证方法。
  候选方向：SIMD Phase 2（M42-M46 搁置项）、常量物化（clang 差距 P1）、
  PSR 扩展、loop interchange 等。
- 验收：每个方向都有可执行的实现步骤。
- 状态：`[~]` 两轮完成（初稿+评级+修正，待复评）

## 执行顺序建议

G1 → G2 → G3 → G4 → G6 → G5 → G7 → G8 → G9 → G10
（先啃后端核心 rustdoc，再方法论 md；G5 依赖 G1-G4 的术语沉淀）

## 评级记录

（每轮 turns 结束后子代理评级结果追加于此）

### G8 第一轮（deleg 4747e163，2026-08-16）
- 维度 1：4/5——生命周期/判定表/加用例步骤准确可照抄，硬伤：没告诉新手退出码行从哪获得。
- 维度 2：3/5——坑清单大多真实，但有一处确凿事实错误（CI 说法与 workflow 不符）。
- 改进（已全部执行，第二轮 turns）：
  1. 【最大】CI 实际跑 `-O 2` 全量（functional+h_functional+perf），非 -O0；已修正 §5 + 备注 AGENTS.md 同款过时说法待用户确认；
  2. 退出码行来源：先跑一次读 `results/.../case.runtime.return`，已补 §6；
  3. 残留容器：make 目标自带 cleanup trap，仅强杀后需手动清，已改 §1/§6/§7；
  4. SKIP 是死代码（SKIP_TESTS 空集），已注明；
  5. M44_TRACE 用例不存在，已删例名；
  6. 缺前置条件，已加 §0（Docker/musl target/根目录/-j 2）。

### G8 第二轮（deleg 991f92e2，复评）
- 维度 1：4/5、维度 2：4/5——6 条修改 5 条完全准确、1 条部分准确。
- 剩余问题（轻量，已全部执行）：
  1. 【中】make test 不传 TESTS 默认跑**全部**（含 perf，很慢）——已修正 §5；
  2. 【低-中】musl target 按主机架构（aarch64/x86_64）——已修正 §0；
  3. 【低-中】`-j 2` 在 ARGS 里才是 test.py worker（CI 同款），make 级不同物——已修正 §0/§6。
- 结论：G8 **可判完成**（两轮 turns 已满）。

### G1 第一轮（deleg c10150db，2026-08-16）
- 维度 1：3/5——术语零解释、零示例；维度 2：3/5——缺验证步骤、"只动 abi"过度简化。
- 6 条建议全部执行（bc0fa25）：术语速查小节、最小走查（a=b+1 示例）、扩展指引验证命令+红线、abi 改动检查面、M68/include_str! 隐性知识、pass 一句话说明。

### G2 第一轮（deleg 4978bcec，2026-08-16）
- 维度 1：4/5、维度 2：3/5。
- 6 条建议全部执行（bc0fa25）：**【准确性】SpillWeight 公式修正**（hot_bonus 每层×4 + def_bonus 2000 + constraint_bonus Any=1000/Reg=2000，按 Use 聚合）、补 index 子模块、LiveRange/bundle 前置定义、玩具例（溢出博弈）、Ctx/Env 关系、ion 常见修改点小节。

### G3 第一轮（deleg 623cf8dd，2026-08-16）
- 维度 1：3/5、维度 2：3/5。
- 6 条建议全部执行（bc0fa25）：生命周期图补 compute_frame_layout + post-RA 阶段、四步指南 RISC-V 对应物（uika_riscv 无 sched）、操作数表/符号化输出术语、链接 reg_alloc/emit_buffer、验证步骤、定位链。

### G4 第一轮（deleg 7816ff73，2026-08-16）
- 维度 1：4/5、维度 2：3/5。
- 建议全部执行（bc0fa25）：**【事实错误】Slot 枚举无 Label 成员**（只 Text/Branch/Veneer）修正、legalize_inst 语义修正（伪寻址展开非指令拆分）、补"为什么需要 buffer"动机。

### G5 第一轮（deleg 7cf4794d，2026-08-16）
- 维度 1：3/5、维度 2：3/5。
- 7 条建议全部执行（cc7f799）：passes.rs 扁平文件注册点（非 mod.rs）、Pass 签名（run/run_on + bool 契约）、"约 33 实例 + 门控"口径、uika_riscv 结构修正、VecFmla 命名 + sched/dag.rs、坑清单补 -O0/CE/InstKind 跨层、与 rustdoc 交叉引用。

### G6 第一轮（deleg f4725bb2，2026-08-16）
- 维度 1：3/5、维度 2：3.5/5。
- 6 条建议全部执行（cc7f799）：术语速查（SSA/Phi/固定点/IV + GSP/PSR/SR/TCO/IPSCCP 展开）、arena+SSA 动机、Pass bool 契约（100 轮 panic）+ 最小骨架、加指令/类型检查清单、管线速览修正（31 模块 + 补充 pass）、下游衔接 + MULMOD_HELPER/CALLOO_NAME。

### G7 第一轮（deleg b978a877，2026-08-16）
- 维度 1：4/5、维度 2：4/5。
- 6 条建议全部执行（cc7f799）：§5 for 示例完整可照抄（items.rs/ast.rs 三段代码）、下降路径 frontend/ast.rs（非 frontend.rs）、速查表补 mut 绑定 + Option、词法优先级（字面量>正则）、shift/reduce 报错读法、G5 交叉引用 + 验证命令。

### G9 第一轮（deleg f2d34dce，2026-08-16）
- 维度 1：3/5、维度 2：2/5（5 处以上与代码不一致）。
- 6 条建议全部执行（f39dd0b）：**【最大】M6 标注为已完成**（lower_select 已 emit VecBsl）、验证补 anon_armv8 测试、矩阵补 VecMinMax/VecInsertLane + 形状修正（仅 .4s/.2d）、M7 文件指正（ir/inst_kind/vector_reduce.rs + VecMinMaxOp 复用）、M5 smull widening 语义修正、M2 现状坑（Fma 无类型检查静默错误）+ f64 比较隐患。

### G10 第一轮（deleg ba63abab，2026-08-16）
- 维度 1：3/5、维度 2：3/5。
- 建议执行（f39dd0b）：**【最大】撞号线说明**（M56-M69 线与 §4 主计划 F 线并行编号）、补 M60/M61、版本化守卫概念修正（trip-count 未知才需守卫）。"一处数字与 TODO.md 冲突"经核对全部一致（基线 h-1 309/h-4 87/h-5 314/huffman 564/fft0 373），未改。

## 各 goal 状态

全部走完"初稿 → 评级 → 修正 → 复评 → （必要时）终验"闭环，两轮 turns 要求已满足：

| Goal | 第一轮 | 复评/终验 | 结论 |
|------|--------|----------|------|
| G1 | 3/3 | 终验通过（AluRRImm12/pair_combine 修正核实） | ✅ 完成 |
| G2 | 4/3 | 最终复验通过（玩具例 mul/存值 + bundle 优先级口径） | ✅ 完成 |
| G3 | 3/3 | 复评 5/5 通过 | ✅ 完成 |
| G4 | 4/3 | 复评 4.5/4.5 通过（含补漏 dd7ce58） | ✅ 完成 |
| G5 | 3/3 | 复评 4/4 通过（含小修 0fac84e） | ✅ 完成 |
| G6 | 3/3.5 | 复评 4/4 通过（含 31 模块修正） | ✅ 完成 |
| G7 | 4/4 | 终验通过（OUT_DIR 位置/下降代码） | ✅ 完成 |
| G8 | 4/3 | 终验通过（默认全量/musl/-j 语义） | ✅ 完成 |
| G9 | 3/2 | 复评 5/4 通过（含 M7 措辞微修） | ✅ 完成 |
| G10 | 3/3 | 终验通过（M60/M61 线归属修正） | ✅ 完成 |

评级抓到并修复的关键问题（全部实证核对）：
- 事实错误：SpillWeight 公式、Slot 枚举无 Label、M6 已实现、生成文件在 OUT_DIR 非 src/、M60/M61 线归属、pair_combine Post-RA、累加器数 2-4、CI 实际 -O2 全量
- 结构问题：术语速查、最小走查、可照抄代码、验证命令、交叉引用、红线（Illegal_optimization.md）、常见修改点等 60+ 处

额外发现（未改，待用户确认）：AGENTS.md "CI 用 -O0" 说法过时（实际 -O 2 全量）；
taki_mir/src/passes.rs 顶部注释的 post-RA 阶段描述过时（实际在 finalize 之后）。

## 第二轮：pass 级文档补全（G11-G15，待用户确认范围后开工）

> 术语：本文件第一轮的 "Goal" 沿用历史写法；第二轮起统一称"任务/单元"，
> 与 Hermes Agent 的 goal（agent 运行环境/任务提示词）区分，避免混淆。

### 触发背景
G1-G10 覆盖了 crate 级/模块级/方法论。遗留缺口：**pass 文件自身的模块级描述**——
调试时定位到 `raana_ir/src/opt/passes/loop_unroll.rs`（1181 行）的问题，但该文件完全无
`//!`/`///` 描述。全量盘点后范围扩至三个 crate。

### 评价标准（六要素，面向中等编译知识大二本科生）
每个 pass/模块的 `//!` 至少覆盖：
1. **一句话定位**：做什么 + 动机（哪个 benchmark 热点、什么 IR 形状触发）；
2. **IR 变换形态**：before/after 示例（参考 rotate_loops.rs 现有英文文档写法，中文呈现）；
3. **触发/放弃条件**：匹配什么 pattern、何时拒绝（trip count 未知/副作用等）；
4. **正确性要点**：为什么安全（如 rotate_loops 的 "head test 只 gate 第一次迭代" 论证）；
5. **管线位置**：注册点（initial/fixpoint）、前后依赖、门控（AArch64-only/config 开关）；
6. **验证**：单测位置/验证方式。

素材来源：`raana_ir/src/opt/pass.rs` 的 `from_config`（177-365 行）里每个 pass 注册处
的英文一行备注；各 pass 现有英文 `//!`/`///`（翻译+补全）。

### 对比基准（重要）
本地 `~/Documents/Programs/rust/s2r` 是**空仓库**（无 commit，仅 10 行 stub main.rs）。
soyo_compiler 的 migrate 基准用 AnonBeijingCompiler git 历史 **commit 5921037
（"[Backend] Migrate s2r code"）**：migrate 时 `frontend/ast.rs`(1498 行)/`utils.rs`(909 行)
已存在；此后 AI 修改：新增 `items.rs`(618)、`ast.rs` 改 1526 行至 1586、`utils.rs` 重构至
556、`mod.rs`→`frontend.rs`。文档只覆盖 AI 修改后的现状（用户对原生部分理解充分）。

### 范围清单

**G11 — raana_ir passes A 档（完全无描述，8 文件）**
`loop_unroll.rs`(1181)、`specialize.rs`、`simplify_cfg.rs`、`sr.rs`、`gvn_pre.rs`、
`pointer_strength_reduction.rs`(+子目录 analysis/candidate/rewrite 各仅 1 行英文标题)、
`column_major.rs`、`const_prop.rs`（⚠ 未注册进管线=死代码，先定去留再决定补不补）。

**G12 — raana_ir passes B 档（无模块级 `//!`，仅散落英文 `///`，11 文件）**
`ssa.rs`(5)、`inline.rs`(17)、`tco.rs`(11)、`tail_recursive_inline.rs`(26)、`licm.rs`(12)、
`gvn.rs`(10)、`dce.rs`(16，含 DPE/DCE/DFE 三个 pass)、`if_conversion.rs`(7)、
`boolean_simplify.rs`(3)、`mulmod_recognize.rs`(57 行均 internal)、`ipsccp.rs`（`//!` 仅一行
英文标题，有壳无肉）。

**G13 — raana_ir passes C 档（有 `//!` 但全英文，12 文件）**
`blocked_reduction`、`chain_to_switch`、`dse`、`guard_elimination`、
`invariant_reduction_hoisting`、`matmul_interchange`、`mod_fold`、`recursive_memoize`、
`reduction_unroll`、`rotate_loops`、`scalar_global_promotion`、`zero_store_loop`——
内容质量好，按六要素中文化（工作量最大的一档）。

**G14 — taki_mir 无文档/英文薄文档（6 文件）**
无 moddoc：`lib.rs`(352，crate 级文档缺失)、`abi.rs`(694)、`lower.rs`(1403)、
`inst_predicate.rs`(25)、`libcall.rs`(12)；英文薄文档：`block_order.rs`(1 行)、`stats.rs`(1 行)、
`types.rs`(1 行)、`div_magic.rs`(10 行)。已中文的（G1-G4：vcode/reg_alloc/emit/emit_buffer/
register/passes）不动。

**G15 — soyo_compiler frontend（AI 修改部分，3+1 文件）**
`frontend/ast.rs`(1586)、`frontend/items.rs`(618)、`frontend/utils.rs`(556)、
`frontend.rs`(3 行 mod 声明)。对比 5921037 的旧版理解 AI 改动，文档写当前状态。
`abi_matrix.rs` 已有英文 moddoc、`cli.rs`/`main.rs` 用户自述理解充分——默认不动，除非用户
要求。

### 流程（沿用第一轮硬性规则）
- 每任务独立 commit，中文 message，`[Docs]:` 前缀，原子可验证；
- 每任务至少两轮 turns：初稿 → 子代理评级（两维度 1-5）→ 修正 → 复评；
- 纯文档任务：不碰代码逻辑（不改 .rs 行为）、不 push；
- 验收：`cargo doc -p raana_ir` / `-p taki_mir` / `-p soyo_compiler` 无 warning；
  评级维度 1（小白上手）终评 ≥4 分。

### 待用户确认
1. s2r 本地为空仓库，migrate 基准改用 git 5921037 —— 是否 OK？
2. C 档（G13）是否中文化（工作量最大）；还是只做 A+B？
3. taki_mir 英文薄文档（block_order/stats/types/div_magic）是否纳入 G14；
4. const_prop.rs 死代码：删/标注/照补？
5. 执行顺序 G11→G12→G13→G14→G15 是否接受。

## 第二轮执行记录（G11-G15 + 基础设施批次，已完成）

### 用户决策（覆盖上述待确认项）
- ① s2r 空仓库 → 对比基准用 git 5921037 ✅；② C 档全量中文化 ✅；
  ③ taki_mir 英文薄文档纳入 G14 ✅；④ const_prop.rs 未注册管线，文档如实
  标注"未注册、被 ipsccp 取代"，代码未删（留待用户定）✅；⑤ 顺序接受 ✅。
- 流程变更：用户先抽查人工示例（loop_unroll/sr/rotate_loops/specialize），
  认可后改为 **subagent 批量生产 + 主 agent 质检**；批次上限先 3 后 5/7，
  最终放开全量（每批 ≤10 并行）。术语表独立成 `docs/offline-handbook/glossary.md`；
  英文 doc 一律保留（C 档=英文块后追加中文，B 档=struct 英文 /// 原位保留）。

### 完成情况

| 任务 | 范围 | 方式 | 文档量 | 结论 |
|------|------|------|--------|------|
| 示例批 | loop_unroll / sr / specialize / rotate_loops（4 文件，人工） | 主 agent | ~320 行 | ✅ 用户认可，确立六要素+术语表引用风格 |
| G11 | raana_ir A 档 8 文件 + PSR 3 子模块 | 主 agent + subagent×2（candidate/rewrite 供用户审查） | ~640 行 | ✅ 用户审查 subagent 产出合格，批准大批量 |
| G12 | raana_ir B 档 11 文件（licm/if_conversion/boolean_simplify/tco 主 agent，余 7 subagent） | 混合 | ~1,040 行 | ✅ |
| G13 | raana_ir C 档 12 文件（rotate_loops 已在示例批） | subagent×11 | ~1,050 行 | ✅ |
| G14 | taki_mir 9 文件（lib crate 级 + abi/lower/inst_predicate/libcall + block_order/stats/types/div_magic） | subagent×9 | ~674 行 | ✅ |
| G15 | soyo_compiler frontend 3 文件（ast/items/utils，AI 重构部分） | subagent×3 | ~384 行 | ✅ |
| 基础设施批次 | analysis_passes 6 文件（call_graph/cfg/dom_tree/induction_variable/loop_analysis/pure_function） | subagent×6 | ~986 行 | ✅ |

commit（docs/offline-handbook 分支，未 push）：
`0a8d228` passes A/B/C 档（34 文件）→ `299067b` 计划+术语表 → `3845138` taki_mir（9 文件）→
`4be4b65` soyo_compiler frontend（3 文件）→ `ca6ac97` analysis_passes（6 文件）

### 第二轮评级记录（subagent 批量路线）

- **subagent 效果审查**（用户主导）：candidate.rs/rewrite.rs 两篇抽查全文，
  六要素齐全、英文保留、术语指向 glossary、符号引用零编造（17 个函数/常量
  逐一 grep 核实，MAX_TRANSITIVE_GEP_DEPTH=8 与 2^20 上限均属实）→ 结论：
  **合格**，批准后续全量 subagent 生产。
- **质检流程**（每批主 agent 执行）：① 读文档全文抽查；② 符号引用批量
  grep 核实（累计 200+ 符号，零 MISSING）；③ cargo check 0 error；
  ④ cargo doc 无新增 warning（抓出并修复 3 处 `[Backend]` 被 rustdoc 误判
  intra-doc link 的转义问题）。
- **subagent 实证纠错**（按代码核实、非照抄任务假设）：gvn 注册点实为
  licm 后 dse 前；dce 文件含 5 个 pass（补 JumpOnlyElimination）；rotate_loops
  不消费 LoopAnalysis/IV 分析；pure_function 直接使用方仅 return_summary
  （LICM/DCE 用 effects.rs）；UnreachableBasicBlock 不在主管线。
- **遗留待用户定**：const_prop.rs 死代码去留；analysis_passes 剩余
  effects/icfg/memory/range/return_summary 5 文件仅有英文薄文档（未覆盖）。

### 汇总

第二轮共 48 个文件获得中文文档（raana_ir passes 31+3、taki_mir 9、
soyo_compiler frontend 3、analysis_passes 6、术语表 1 个 md），约 5,100 行
中文；`cargo doc` 三 crate 均无新增 warning；纯注释改动，零逻辑变更。


