# Cortex-A53 后端优化计划

本文档只记录尚未完成的工作。M1-M18 已完成，历史设计与实现细节以 Git 提交记录
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
- pre-RA DCE（M18）：worklist use-count fixpoint，白名单制（纯 ALU/Mov/常量
  物化/纯 FP + dead load），`-O1` 起默认开启，`--enable/disable-mir-dce`
  开关，`DceStats` 统计。huffman-01 上 `-O1` 指令数 964 → 935（-3%），
  `mov w13, wzr` 等死代码全部消除。
- ABI 参数绑定基础设施（M19）：`taki_mir` 新增 `ArgPair { vreg, preg }`、
  `ABIMachineSpec::gen_args()`、`CalleeABI::reg_args`/`take_args()`；
  AArch64 与 RISC-V 各新增零字节 `MInst::Args` 伪指令（`reg_fixed_def`
  固定寄存器定义、空 emission、非 terminator、不在 DCE 白名单、verify
  校验固定寄存器类匹配）。尚未接入 lowering，行为不变。

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
  -> assembly emission
```

关键代码：

- `anon_armv8/src/passes/mod.rs`：按 `AArch64CodegenConfig` 注册 pass。
- `taki_mir/src/passes.rs`：`MIRPass` trait、pre-RA/post-RA 两阶段 pipeline。
- `anon_armv8/src/passes/dce.rs`：worklist use-count fixpoint DCE，白名单制。
- `anon_armv8/src/passes/peephole_combine.rs`：vreg use 计数 + MAC 融合，
  使用 `Removed` tombstone。
- `anon_armv8/src/instructions.rs`：`MInst` 枚举（约 60 个 variant）。
- `taki_mir/src/stats.rs`：函数级 / 编译单元级结构化统计。
- `soyo_compiler/src/cli.rs`：`-O` 映射与 `--enable/disable-*` 开关。

### 1.2 已知问题：入口参数无条件落栈

当前函数入口为每个寄存器参数无条件生成 home-slot 中转（如 `_and` 入口）：

```asm
str w0, [sp, #0]
str w1, [sp, #16]
ldr w12, [sp, #0]
ldr w5, [sp, #16]
```

这不是 RA 压力导致的 spill，而是 `CalleeABI` 的参数初始化方案：

```text
AAPCS64 入参寄存器 w0/w1
  -> 无条件保存到普通栈槽（计入 stackslots_size，非 spill_size）
  -> 从栈槽加载到参数虚拟寄存器
  -> RA 将参数虚拟寄存器分配为 w12/w5
```

根因排序：

1. `taki_mir/src/abi.rs` 的 `prealloc_reg_arg_spills()` /
   `gen_store_reg_args_to_stack()` / `gen_copy_arg_to_reg()` 无条件为寄存器
   参数分配 home slot 并生成 store/load（`taki_mir/src/abi.rs:535-601`）。
2. 入口参数没有 fixed-register live-in 表示，只能通过栈中转 materialize
   （`taki_mir/src/lower.rs:433-478` 的 `gen_arg_setup`）。
3. 现有 MIR pass（DCE / PeepholeCombine / PairCombine / ListScheduler）无法
   消除该模式：store 不可删、load 结果被使用、`Store+Load` 不是 `is_move()`。
4. 每个 `i32` 参数 home slot 独立按 16 字节对齐（`taki_mir/src/abi.rs:428-439`），
   既浪费栈空间，也因地址差为 16 而非 4 导致 PairCombine 无法形成 `stp`。
5. 未使用的寄存器参数也可能被保存（`gen_store_reg_args_to_stack` 遍历所有
   寄存器参数，不检查 `is_value_needed`）。

前端为源语言参数创建的 `alloc/store/load`（`soyo_compiler/src/frontend/ast.rs:144-155`）
在 `-O1/-O2` 下已被 SSA/mem2reg 消除（`raana_ir/src/opt/passes/ssa.rs`），不是上述
入口 `str/ldr` 的主要来源。

### 1.3 当前结论边界

- 静态 estimator 只用于确定性回归和相对启发式比较，不能替代实机测量。
- scheduler 与 estimator 共用同一模型，"模型内不退化"不等于"硬件不退化"。
- QEMU 仅用于语义差分，不能证明 A53 的 dual-issue / latency / throughput 收益。
- 所有 profile latency 为 ARM guide 推导值（DUI 0901），未经 XCZU15EG 实测校准。
- 未获得实机数据前，文档和提交信息只能声称"静态模型改进"，不能声称
  "XCZU15EG runtime 提升"。

---

## 2. 主计划：入口参数 fixed-register live-in（Args 伪指令）

对应"参数不落栈"的结构性修复。这不是性能 peephole，而是 ABI 参数表示方式的
架构改造，参考 Cranelift 的 `Args` 伪指令实现。M19-M24 依次推进。

### 2.1 参考实现：Cranelift 的 `Args` 伪指令

Cranelift 并未为 regalloc2 增加"无定义入口 live-in"特殊通道，而是在函数体第一条
放置零字节 `Args` 伪指令，并为每个寄存器参数记录 `reg_fixed_def(vreg, ABI preg)`：

- `ArgPair { vreg, preg }`：`wasmtime/cranelift/codegen/src/machinst/abi.rs:120-129`
- 寄存器参数只写入 `reg_args`，不生成 store/load：`abi.rs:1562-1584`
- 函数第一条插入 `Args`：`machinst/lower.rs:525-565`
- `Args` 使用 `reg_fixed_def`：`isa/aarch64/inst/mod.rs:794-798`
- `Args` 不输出机器码：`isa/aarch64/inst/emit.rs:2931-2934`
- 栈参数仍正常 load：`machinst/abi.rs:1585-1605`

```text
当前：
ABI preg -> 参数 home slot -> parameter vreg -> RA

目标：
Args pseudo: fixed_def(parameter vreg, ABI preg)
  -> RA 自动处理后续分配、复制、交换环和真实 spill
```

### 2.2 为什么采用该方案

项目已具备全部依赖机制，无需立即修改 allocator 核心：

- `OperandConstraint::FixedReg`：`taki_mir/src/reg_alloc/reg.rs:527`
- `reg_fixed_def()` / `reg_fixed_def_at_start()`：`reg.rs:813-820, 835-841`
- 调用返回值已用 fixed-def：`anon_armv8/src/instructions.rs:880-882`
- `RetVal` 已验证"只约束寄存器、不输出机器码"的 pseudo 模式：
  `instructions.rs:885, 1370`
- Ion parallel-move 环解析：`taki_mir/src/reg_alloc/moves.rs:41-136`
- Ion 用专用 scratch / 空闲寄存器 / 临时 spill slot 破环：
  `taki_mir/src/reg_alloc/ion/moves.rs:759-864`

暂不采用 allocator 原生 entry live-in（`Function::entry_liveins`）的原因：

- `compute_liveness` 明确拒绝 entry virtual live-in：`ion/liveranges.rs:355-362`
- VCode verifier 明确拒绝 entry block parameter：`vcode.rs:767`
- 无定义 live-in range 很难正确绑定 fixed requirement，`Use.slot` 假设约束
  来源于真实指令 operand；spill/split 后缺少写入初始值的路径，有读未初始化
  栈的风险。
- Cranelift 本身也没有该通道，说明 pseudo 方案是成熟长期架构而非 workaround。

### M19：引入通用 Args Pseudo 表示 ✅（commit 9ea625b）

已完成：`ArgPair`、`gen_args`、`take_args`、双后端 `MInst::Args` 及配套测试。
详见提交记录，不再重复维护。

### M20：将寄存器参数改为 Fixed Def

目标：正式消除寄存器参数的 home-slot store/load。

重写 `CalleeABI::gen_copy_arg_to_reg`（建议改名 `bind_arg_to_reg`，避免同时
"记录 pseudo"与"返回实际指令"两种语义）：

```rust
ArgSlot::Reg { reg, .. } => {
    self.reg_args.push(ArgPair {
        vreg: Writable::from_reg(into_reg),
        preg: reg.into(),
    });
}
ArgSlot::Stack { offset, ty } => {
    // 保留现有 incoming-stack load
}
```

重写 `LowerContext::gen_arg_setup`（`taki_mir/src/lower.rs:433-478`），替换
两阶段 store/load + 逆序翻转：

```text
遍历实际使用的 entry 参数
  register arg -> 收集 ArgPair
  stack arg    -> 生成 incoming load
finish 当前 lowering group
take_args() -> Args pseudo 位于函数体最前
```

关键顺序：`Args pseudo -> stack argument loads -> 普通 entry 指令`。VCode 当前
逆序构建，需对 `finish_ir_inst()` 规则写显式测试，不能仅靠源码顺序推断。

删除旧 home-slot 状态（确认新路径稳定后）：

- `CalleeABI::reg_arg_spillslots`、`prealloc_reg_arg_spills()`、
  `gen_store_reg_args_to_stack()`。
- `gen_spill_store()`/`gen_spill_load()` 本身可能仍有其他用途，不能因名称
  相同直接删除 ABI machine hook。

未使用参数：只有 `is_value_needed(param)` 的寄存器参数才进入 `Args`，同时修复
未使用参数仍分配 home slot / 生成 `str` / 强迫叶函数建栈帧的问题。

栈参数：超过 ABI 寄存器数量的参数继续走 `gen_incoming_arg_load`，不改变
AAPCS64 stack argument offset、caller outgoing 布局、tail-call stack 处理、
frame pointer 相对寻址。

提交边界：`[Refactor(MIR)]: Bind incoming register arguments with fixed defs`

### M21：Post-RA Pseudo 语义与调度集成

与 Cranelift 不同，本项目有 post-RA list scheduler，必须显式定义 `Args` 的
调度语义，不能只空 emission。

`Args` 在 `anon_armv8/src/sched/dag.rs` 中的语义：

```text
defs: 所有参数分配后的 physical registers
uses: none
latency: 0
resources: none
emitted ops: 0
barrier: false
```

- 不能当普通 `Alu`：静态 estimator 会凭空增加 cycle。
- 不建议当 full barrier：会不必要地限制 entry block 调度。
- 建议 `class: SchedClass::Nop`，但必须保留 `defs`，确保后续对参数物理寄存器
  的 use/def/WAR/WAW 关系正确。

顺带校正 `RetVal`：当前不输出机器码却被分类为 `SchedClass::Alu`
（`dag.rs:1046-1053`），会让 estimator 计算不存在的 ALU 操作。改为
`uses: return vreg, class: Nop, emitted ops: 0`。

不采用"RA 后把 `Args` 转 `Removed`"作为第一版：后续调度仍需要知道 entry
physical registers 的初始定义边界；保留 zero-cost pseudo 更利于调试与 verifier。

提交边界：`[Fix(AArch64)]: Model zero-width ABI pseudos in post-RA scheduling`

### M22：验证 Parallel Copy 与真实 Spill

整个改造最关键的正确性门禁。

同寄存器分配：

```text
incoming x0；Args fixed-def v0=x0；v0 后续仍分配 x0
预期：无 mov、无参数 stack slot、无参数 spill
```

普通重分配：

```text
incoming x0；v0 后续需要 x9
预期：allocator edit 生成 mov x9, x0，而不是 str/ldr 栈中转
```

参数交换环：

```text
arg0: x0 -> 后续要求 x1；arg1: x1 -> 后续要求 x0
预期：Ion 按 parallel semantics 破环，例如 tmp=x0; x0=x1; x1=tmp
scratch 选择顺序：专用 scratch -> 该 program point 空闲寄存器 -> 临时 spill slot
```

需覆盖 Int 与 Float 两个 register class。

高寄存器压力（参数跨 call 存活 + 大量 live value + call 破坏 caller-saved）：

```text
预期：值可搬到 callee-saved register；或生成真实 spill（位于 spill_size 区域，
不重新引入 ABI home slot）；prologue 正确保存新增 callee-saved register
```

多个 fixed constraints（同参数后续又遇到 fixed use，如 call 参数需要 x1）：

```text
验证 Ion 的 split / fixed requirement / inserted moves / redundant move
elimination 闭环
```

提交边界：`[Test(Regalloc)]: Cover incoming fixed defs and parallel argument moves`

### M23：ABI 与跨后端回归验证

AArch64 测试矩阵：

- 0 参数 / 1 个 `i32` / 1 个 `i64`/pointer / 1 个 `f32`。
- 8 个整数参数 / 8 个浮点参数 / 9 个整数参数（第 9 个从栈加载）。
- int/float 混合、未使用参数、参数跨普通 call 存活、跨 tail call。
- 递归调用、叶函数、有本地 `alloc` 的函数、真实 RA spill 的函数。
- 参数被源程序重新赋值。

RISC-V 测试矩阵（`a0-a7` fixed-def、超量 stack args、float regs、tail call、
混合、交换环）。

优化级别：该改造是 ABI correctness / codegen architecture，不由优化 flag 控制，
`-O0/-O1/-O2` 均不再为寄存器参数生成 home slot。注意区分：

```text
-O0 前端源语言参数的 alloc/store/load：允许（IR mem2reg 未运行）
ABI register home slot：不允许
```

Differential correctness：QEMU AArch64、RISC-V emulator、现有 functional cases、
tail-call cases、参数数量边界、随机小函数参数传递。

确定性：同一输入重复编译 5 次，汇编 byte-identical。

提交边界：`[Test]: Add cross-target incoming argument ABI gates`

### M24：Frame 与性能门禁

静态门禁：纯叶算术函数（无本地栈对象、无调用、无 RA spill）如
`int add(int a, int b) { return a + b; }`：

```text
目标收敛到接近：add w0, w0, w1; ret
至少满足：stackslots_size == 0；无参数 str/ldr；不因参数创建 frame；
无多余 register move 时不产生 edit
```

是否完全消除 frame pointer 取决于当前 prologue policy，但上述下限必须达成。

统计字段（`taki_mir/src/stats.rs`）：

```rust
pub struct AbiArgStats {
    pub register_args_bound: u64,
    pub unused_register_args_skipped: u64,
    pub incoming_stack_args_loaded: u64,
}

pub struct RegallocStats {
    pub spill_slots: u64,
    pub reg_to_reg_edits: u64,
    pub reg_to_stack_edits: u64,
    pub stack_to_reg_edits: u64,
}
```

不建议做"home slots eliminated"运行时计数：新架构中 home slot 根本不应存在，
应以 invariant/test 保证而非优化命中统计。

XCZU15EG 基准（M15 harness）：增加参数入口 microbenchmark——单/双/八参数叶
函数、参数跨 call、高压力真实 spill。预期收益：消除 store-forward latency、
减少 load/store 指令、减少 LSU 占用、缩小 frame、部分叶函数完全消除 frame
setup、为 list scheduler 提供更干净的基本块。

huffman 预期变化：`_and` / `_xor` / `_or` / `rotrN` / `rotlN` 入口的
`str w0, [sp, #0]; str w1, [sp, #16]; ldr ...; ldr ...` 全部消失。这比当前
调度器在 store/load 间插入独立指令更有效，因为直接消除了停顿源。

提交边界：`[Perf]: Add incoming argument frame and performance gates`

### 2.3 关键不变量

实施期间必须持续满足：

1. `Args` 必须是 entry block 第一条 pre-RA 指令。
2. 每个活跃的 register argument 恰好有一个 fixed def。
3. 未使用 register argument 不产生 `ArgPair`。
4. stack argument 仍由真实 load 定义。
5. `Args` 不输出机器码。
6. `Args` 在 scheduler 模型中消耗 0 cycle、0 resource。
7. fixed-def operand 的 register class 必须匹配 ABI preg。
8. allocator 把参数分配到 ABI preg 时不得生成 move。
9. 参数复制环必须由 parallel-move resolver 处理，禁止顺序裸 `mov`。
10. 只有 Ion 真正 spill 时才分配 `spill_size`。
11. ABI register arguments 不得增加 `stackslots_size`。
12. caller、callee、tail-call 必须继续使用同一个 `ArgSlot` 布局。

### 2.4 预计文件范围

核心修改：

- `taki_mir/src/abi.rs`
- `taki_mir/src/lower.rs`
- `anon_armv8/src/instructions.rs`
- `anon_armv8/src/abi.rs`
- `anon_armv8/src/sched/dag.rs`
- `uika_riscv/src/instructions.rs`
- `uika_riscv/src/abi.rs`

机械适配：

- `anon_armv8/src/passes/dce.rs`、`peephole_combine.rs`、`pair_combine.rs`
- 其他对 `MInst` 做 exhaustive match 的位置
- RISC-V 对应 match

测试与统计：

- `taki_mir/src/reg_alloc/moves.rs`
- `taki_mir/src/reg_alloc/ion/mod.rs`（或专用测试模块）
- `taki_mir/src/vcode.rs`
- `taki_mir/src/stats.rs`
- AArch64 / RISC-V ABI 测试
- `tests/` functional cases、`benchmarks/`

### 2.5 验收标准

- `cargo test --workspace` 全通过。
- AArch64 与 RISC-V differential correctness 全通过。
- 所有优化级别均不生成 ABI 参数 home-slot 往返。
- huffman 中寄存器参数入口的 `str/ldr` 全部消失。
- 纯叶算术函数 `stackslots_size == 0`。
- 参数交换环正确。
- 参数跨 call 和高压力 spill 正确。
- stack arguments 和 tail calls 无回归。
- post-RA estimator 不为 `Args`/`RetVal` 计算虚假 cycle。
- 汇编输出保持确定性。

---

## 3. 后续候选工作

按预期收益排序，均需在前一项验证后再启动。

### P1：常量 / 分支参数物化源头治理

DCE 是兜底；更优解是 lowering 时就不为未使用的 block param 和分支参数物化
常量与 `mov wzr`。DCE 落地后统计 `instructions_removed` 的构成，若某类来源
占主导，直接在 `taki_mir/src/lower.rs` 或 `anon_armv8/src/lower.rs` 消除源头。

### P1：双重分支化简

`cmp; b.eq 1f; b target; 1: b other` 可化简为单条条件分支，每个分支点省
1 条指令和 1 个前端槽。需处理 long-jump 形式的跳转范围约束。

### P2：phi 拷贝 coalescing

循环末尾的 `mov x5, x4; mov x12, x3` 并行拷贝链，可在 RA 后消除部分拷贝。

### P2：跨块 / 全局调度

块内调度对被 call 切碎的热点无能为力。候选方向：循环不变 load 外提
（`adrp+add+ldr gv_*` 全局量地址重算）、跨块 hoist。属大改动，需先完成
M19-M24 并重新评估收益空间。

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

### P3：XCZU15EG 实机校准（依赖硬件访问）

- 运行 `benchmarks/src/bench.c`，校准 latency / throughput / pairing 数据。
- 基于实测调整 guide-derived profile 值。
- 建立性能回归门禁。
- 回答：WAR/WAW/NZCV false dependency 是否允许 A53 同周期双发。
- 用 M24 的参数入口 microbenchmark 量化 `Args` 改造的实际收益。

---

## 4. 总体执行原则

1. 每个 milestone 独立提交；`TODO.md` 在 milestone 完成后删除对应已完成
   细节，只保留后续工作。
2. 正确性验证、静态模型测试和实机性能测量分层维护，不互相替代。
3. 所有 AArch64 改动必须同时验证 RISC-V 不受影响。
4. 新 pass 默认走"白名单 + 保守保留"策略，宁漏勿错。
5. 未获得实机数据前，只能声称"静态模型改进"。
6. ABI/codegen 架构改动（M19-M24）不由优化 flag 控制，任何优化级别都必须
   保持正确。

---

## 5. 风险与缓解

| 风险 | 严重度 | 缓解措施 |
|------|--------|---------|
| DCE 误删有隐式副作用的指令（flags、内存、call） | 高 | 白名单制；无 def 指令一律跳过；全量功能回归；on/off 差分 |
| dead load 删除改变 trap 行为 | 低 | SysY 语义下 load 地址必合法；如未来支持 volatile 需加例外 |
| DCE 破坏 SSA / operand 不变式 | 中 | `Removed` tombstone 沿用 M13 先例；pipeline 既有 verify 钩子 |
| 缺失 register/NZCV/memory 依赖导致调度误编译 | 高 | 保守 catch-all barrier、on/off 差分、`verify_sched_deps`（待做） |
| `Args` fixed-def 与后续分配冲突导致参数值丢失 | 高 | M22 专项覆盖同寄存器/重分配/交换环/跨 call/高压力 spill；parallel-move resolver 验证 |
| entry 参数 spill/split 后初值未写回，读到未初始化栈 | 高 | 只绑定 fixed-def 到真实 operand 的 range/bundle；不引入 allocator 原生 live-in；M22 spill 专项 |
| `Args`/`RetVal` 被 estimator 计为真实 ALU，污染模型 | 中 | `SchedClass::Nop` + 显式 emitted ops 0；M21 校正 |
| VCode 逆序构建下 `Args` 未排到函数体最前 | 中 | 对 `finish_ir_inst` 规则写显式顺序测试；post-lowering verify |
| 改造破坏 RISC-V 或 tail-call | 中 | M23 双 target + tail-call 矩阵；`ArgSlot` 布局不变 |
| `i32` 参数按 64 位拷贝（edit 用 I64）高 32 位未定义 | 低 | AAPCS 下消费端用 Size32 指令，低 32 位语义安全；M22 覆盖 w/x 混合 |
| slot/resource 模型错误导致硬件回归 | 高 | 模型内防退化仅作辅助，实机 gate 为准 |
| AArch64 配置改动破坏 RISC-V | 中 | 双 target 测试、RISC-V 拒绝 MIR flag |
| QEMU wall time 被误用为 A53 性能数据 | 中 | QEMU 仅进入 correctness gate |
| 实机环境频率/温度噪声掩盖结果 | 中 | core pinning、paired samples、95% CI、环境元数据 |
