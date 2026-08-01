# Cortex-A53 后端优化计划

本文档只记录尚未完成的工作。M1-M24 已完成，历史设计与实现细节以 Git 提交记录
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
  veneer（M27）与 RISC-V slot 化（M28）尚未完成。

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

关键代码：

- `anon_armv8/src/passes/mod.rs`：按 `AArch64CodegenConfig` 注册 pass。
- `taki_mir/src/passes.rs`：`MIRPass` trait、pre-RA/post-RA 两阶段 pipeline。
- `anon_armv8/src/passes/dce.rs`：worklist use-count fixpoint DCE，白名单制。
- `anon_armv8/src/passes/peephole_combine.rs`：vreg use 计数 + MAC 融合。
- `anon_armv8/src/instructions.rs`：`MInst` 枚举（约 60 个 variant）。
- `taki_mir/src/emit.rs`：`AsmWriter::write_function` 逐块文本发射。
- `taki_mir/src/block_order.rs`：domtree RPO 块序（`lowered_order`）。
- `taki_mir/src/stats.rs`：函数级 / 编译单元级结构化统计。
- `soyo_compiler/src/cli.rs`：`-O` 映射与 `--enable/disable-*` 开关。

### 1.2 已知问题：分支发射无条件使用 3 指令 trampoline

问题背景与量化见 §2.1，本计划（M25-M29）的主攻对象。

### 1.3 当前结论边界

- 静态 estimator 只用于确定性回归和相对启发式比较，不能替代实机测量。
- scheduler 与 estimator 共用同一模型，"模型内不退化"不等于"硬件不退化"。
- QEMU 仅用于语义差分，不能证明 A53 的 dual-issue / latency / throughput 收益。
- 所有 profile latency 为 ARM guide 推导值（DUI 0901），未经 XCZU15EG 实测校准。
- 未获得实机数据前，文档和提交信息只能声称"静态模型改进"，不能声称
  "XCZU15EG runtime 提升"。

---

## 2. 主计划：Fallthrough 长期最优重构（EmitBuffer，参考 Cranelift MachBuffer）

### 2.1 问题量化（M26 基线）

M25/M26 已把 AArch64 分支发射改为 EmitBuffer 结构化 Branch slot，并开启
R1-R4 优化：huffman-01 指令数 870 → 687（-21%）。剩余工作集中在超范围
分支（veneer）与 RISC-V：

| 模式 | 数量 | 当前状态 | 目标 | 可省 |
|---|---|---|---|---|
| 分支已收敛为单条（R1/R4） | 82 处优化 | - | - | 已完成 |
| 超范围条件分支（>±1MB） | 0（当前 corpus） | 直接发射，无范围检查 | veneer 兜底 | 正确性保障 |
| RISC-V `CondBr`（5 条 `la+jr`） | 每处 | trampoline 文本 | `beqz/bnez` + veneer | 每处 ~3 条 |

热循环收益已兑现：`_and`/`_or` 循环内每次少 1-2 条分支指令。

**M26 之后仍未解决的局限**（M27/M28 的主攻对象）：

1. 无偏移/范围概念：分支假定目标都在对应 `LabelKind` 范围内，超范围会
   汇编失败（当前 corpus 未触发，但无保证）；
2. RISC-V `CondBr` 仍是 5 条 `la t6, X; jr t6` trampoline（M28 改为 slot）；
3. RISC-V 侧 `1f` hack 仍在（M28 清理）；
4. 冷块沉底未做（只在 `BlockLoweringOrder` 预留 `is_cold()` 接口）。

### 2.2 参考实现：Cranelift 的 MachBuffer

核心在 `../wasmtime/cranelift/codegen/src/machinst/buffer.rs`，模块注释
（1-107 行）本身就是设计文档。要点：

**2.2.1 单遍发射 + fixup（不关心布局）**
`MachInst::emit` 把指令字节写进 `MachBuffer`，分支目标用符号化 `MachLabel`，
`use_label_at_offset()` 记录 fixup（`buffer.rs:791-807`）；每个块首
`bind_label()`（`:726-747`）。发射方不需要知道块序与目标距离。

**2.2.2 latest-branches 窥视栈（`optimize_branches`，`:999-1271`）**
`bind_label` 内部调用（`:745`），函数末尾再调一次（`vcode.rs:1132`）。
只操作"尾部连续分支"（latest_branches 栈），可安全截断。四条规则：

- **R1 fallthrough 消除**（`:1057`）：分支目标解析 == 当前尾部偏移 →
  `truncate_last_branch()` 整条删除（分支到自身 fallthrough 是 no-op）。
- **R2 标签别名/穿线**（`:1137`）：尾部是无条件分支且其起始处绑定了标签 →
  把那些标签全部 alias 到分支目标（防环检查 `:1170`），等效删除空块；
  配合 R3 可吞噬 RA 未插入 move 的空 edge block。
- **R3 双无条件下冗余删除**（`:1207`）：`b; b` 相邻且第二个起始无标签 →
  删第二个（不可达）。
- **R4 条件+无条件翻转**（`:1222`）：`cond_br L2; b L3` 且 `L2` 解析为当前
  尾 → 截断 `b L3`，把条件分支字节替换为**预编码的倒相字节**，目标改为
  L3。发射方在 `add_cond_branch(..., inverted)`（`:856-890`）中提供条件分支
  的两种编码（AArch64 `inst/emit.rs:3081-3102`）。
- 阈值保护 `LABEL_LIST_THRESHOLD`（`:1041`）防止长串 `goto next` 标签合并
  导致的二次方行为。

**2.2.3 范围与 veneer**
`LabelUse` trait（AArch64 `inst/mod.rs:2937-2956`）：Branch14（tbz，±1MB）、
Branch19（b.cond/cbz，±1MB）、Branch26（b/bl，±128MB）、Adr21、PCRel32；
每个类型声明正负范围、patch 掩码、veneer 支持与大小（`:2958-3048`）。
`deadline`/`island`/`emit_veneer` 机制（`buffer.rs:142-210, 1310-1329,
1544-1567`）在安全点（块间或 jump-around 之后，`:850-857`）插入长跳 veneer。

**2.2.4 VCode 驱动**（`vcode.rs:736-1132`）
冷块沉底（`final_order` + `cold_blocks`，`:759-770`）→ 每块 `bind_label`
（触发 `optimize_branches`，`:872`）→ 每条指令后 `island_needed` 前瞻检查
（`:851-857`）→ 尾部 `optimize_branches`（`:1132`）→ `finish()` 解析 fixup。

### 2.3 关键洞察：我们的约束使问题比 Cranelift 更简单

我们发射**文本 .s**（由系统汇编器解析符号），且 **AArch64/RISC-V 每条指令
固定 4 字节**。由此：

| Cranelift 机制 | 我们是否需要 | 原因 |
|---|---|---|
| 字节级 fixup/patch | 否 | 标签保持符号化 `.L_xxx`，由汇编器解析 |
| deadline/island 前瞻 | 否（可选） | 指令定长 → 偏移**精确可算**，用单调松弛循环即可 |
| latest-branches + 截断 + 倒相 + 别名 | **是** | 这是算法内核，与发射介质无关 |
| 倒相字节预编码 | 是（文本级等价） | `b.eq` ↔ `b.ne` 只是模板替换 |
| veneer | 是（简化版） | 文本级：插入一条 `b target` 模板 |

结论：目标架构是**文本级 MachBuffer**——每个 slot = 一条 4 字节指令的
文本模板，Cranelift 的算法内核原样移植，但无需字节编码器、无需 deadline
机器。

### 2.4 目标架构：EmitBuffer（taki_mir 通用层，RISC-V 同步受益）

#### 2.4.1 数据模型（新文件 `taki_mir/src/emit_buffer.rs`）

```rust
/// 一个 slot = 一条定长指令的文本模板（发射时逐 slot 输出一行）
pub enum Slot {
    Text(String),                        // 普通指令
    Branch(BranchRef),                   // 分支指令：只含 (前缀, 条件, 目标标签)
    Veneer { target: LabelId },          // 目标后端生成的 veneer 模板组
}

pub struct BranchRef {
    pub prefix: String,                  // 如 "b.eq " / "cbz w0, " / "tbz w0, #3, "
    pub inv_prefix: String,              // 倒相模板（条件分支必填）
    pub target: LabelId,
    pub kind: LabelKind,                 // Branch14/19/26（范围）
    pub is_cond: bool,
}

pub struct EmitBuffer {
    slots: Vec<Slot>,                    // 尾部可截断
    labels: Vec<LabelState>,             // Unbound | Bound(idx) | Alias(target)
    latest_branches: Vec<BranchRec>,     // 尾部连续分支栈（含 start/end slot、labels_at_this_branch）
    labels_at_tail: Vec<LabelId>,
    veneers: Vec<VeneerRec>,             // 松弛阶段使用
}
```

关键点：分支 slot 只存 `(prefix, 条件, 目标)` 三元组——**倒相 = 换
`inv_prefix`，改目标 = 改 `target`，删分支 = 截断 slot**，都是 O(1) 文本级
操作，不需要字节 patch。

#### 2.4.2 API（对齐 MachBuffer，由 `EmitContext` 暴露给 `MInst::emit`）

```rust
impl EmitContext for EmitBuffer {
    fn put_inst(&mut self, text: String);                      // 普通 slot
    fn put_branch(&mut self, prefix, inv_prefix, label, kind); // add_cond_branch 等价
    fn put_uncond_branch(&mut self, prefix, label, kind);      // add_uncond_branch 等价
}
impl EmitBuffer {
    pub fn bind_label(&mut self, id: LabelId);            // 内部调用 optimize_branches
    pub fn optimize_branches(&mut self);                  // 四条规则移植
    pub fn resolve(&mut self);                            // 范围检查 + veneer 松弛
    pub fn finish(self) -> String;                        // 渲染文本（含别名解析）
}
```

#### 2.4.3 optimize_branches 四条规则移植（对照 buffer.rs 行号）

- R1（`:1057`）：`resolve(target) == tail` → 弹出 BranchRec、截断 slot、
  合并 `labels_at_tail` 与 `labels_at_this_branch` 回退（移植
  `truncate_last_branch` `:892-996` 的标签簿记）；
- R2（`:1137`）：无条件分支且 `labels_at_this_branch` 非空且
  `resolve(target) != start` → 全部 `label_aliases[l] = target`（防环检查
  保留，`:1170`）；
- R3（`:1207`）：无条件分支接无条件分支 → 截断后者；
- R4（`:1222`）：`cond(L2) + uncond(L3)` 且 `resolve(L2) == tail` →
  截断 uncond、条件分支换 `inv_prefix`、`target = L3`；
- `LABEL_LIST_THRESHOLD` 防二次方保护（`:1041`）保留。
- 调用时机与 Cranelift 相同：`bind_label` 内 + 函数末尾。

#### 2.4.4 veneer 与范围松弛（比 Cranelift 更简单）

- `LabelKind`（AArch64）与 `inst/mod.rs:2937` 对齐：
  `Branch14`(±1MB, tbz)、`Branch19`(±1MB, b.cond/cbz)、`Branch26`(±128MB,
  b/bl)；
- **第一阶段**：所有分支按 `Branch19/26` 直接发射，不做 trampoline；
- **第二阶段（resolve）**：slot 定长（4B）→ 精确计算每标签偏移；找出
  超范围的条件分支；
- **第三阶段**：超范围分支改发两指令形式（条件+无条件），veneer 插在
  **分支自身之后**——分支是块终结符，之后必然是块边界，无 fallthrough
  进入；若该分支已被优化成单条件且另一目标为 fallthrough，则先强制两指令
  形式再插 veneer；
- **第四阶段**：重算偏移，重复直至稳定（单调收敛，通常 1-2 轮）。此循环
  替代 Cranelift 的 deadline/island 前瞻，定长指令下完全等价且更简单。

#### 2.4.5 发射集成改造点

| 文件 | 改动 |
|---|---|
| `taki_mir/src/emit_buffer.rs`（M25 已建） | EmitBuffer 核心；R1-R4 已完成（M26）；M27 填 `resolve` 松弛 |
| `taki_mir/src/vcode.rs` | `end_inst/put_branch/put_uncond_branch`（M25 已完成）；`MachInst` trait 增加 veneer 生成接口（M27）；verify 断言 slot 粒度（M27） |
| `taki_mir/src/emit.rs` | `write_function` 已改走 buffer（M25）；`optimize_branches` 函数尾调用（M26）；M27 传 veneer 接口 |
| `anon_armv8/src/instructions.rs` | 分支 MInst 已改 Branch slot、`1f` hack 已删（M25）；M27 无改动 |
| `anon_armv8/src/labels.rs` | `Label::block()` 访问器（M25 完成） |
| `anon_armv8/src/lower.rs` | 可选：`CmpImm(0)+CondBr{Ne}` 兜底（`:860-870`）在 slot 层识别为 `cbz` |
| `uika_riscv/src/instructions.rs` | `CondBr` 改为 slot（`beqz/bnez` 倒相）；veneer 用 `la t6,X; jr t6`（B-type ±4KB / JAL ±1MB） |
| `taki_mir/src/stats.rs` | `BranchOptStats` 已接入（M26）；`veneers_inserted` 由 M27 填充 |

#### 2.4.6 顺带收益（同构改造附带解决）

- 35 条真死跳转（`b next_block`）→ R1 消除；
- 空 edge block（RA 未插入 move）→ R2 别名自动吞噬，无需 MIR 层空块删除
  pass；
- RISC-V 的 5 指令 `CondBr` 降为 1-2 条（`beqz/bnez` + 必要时 veneer）；
- 后续可基于 slot 层做更多分支窥视（cbz 融合、反向条件选择等）。

### 2.5 关键不变量

1. 每个 slot 恰好对应一条 4 字节指令（`MInst::emit` 发射多个 slot 时，
   verify 断言 slot 数 == 指令数）。
2. `latest_branches` 尾部连续、按偏移升序、无重叠（同 buffer.rs:110-122）。
3. `labels_at_tail` 精确且完备；`labels_at_this_branch` 完整记录绑定在
   分支起始处的标签。
4. 标签别名不得成环（`resolve` 跟随链时防环检查）。
5. 截断只发生在缓冲尾部；块内指令顺序不变（ListScheduler 不受影响）。
6. veneer 只插在无 fallthrough 的位置（分支之后）；被 R1 消除的分支不得
   残留 veneer 需求。
7. 发射前所有分支目标均在对应 `LabelKind` 范围内。
8. 输出确定性：同输入、同 flag 组合，5 次 byte-identical。
9. 优化规则只在 `-O1/-O2` 开启；`-O0` 走等价两指令形式，作为 on/off
   差分回归基线。
10. AArch64 与 RISC-V 共用 EmitBuffer，行为一致；RISC-V 不受 AArch64
    配置开关影响。

### 2.6 里程碑（M27-M29）

每个 milestone 独立提交；完成后在 TODO.md 删除对应细节，只保留一行历史
（同 M19-M24 惯例）。已定决策：EmitBuffer 放 `taki_mir` 通用层；范围策略
采用"±1MB 内直跳、超范围才 veneer"；冷块沉底本期不做，仅在
`BlockLoweringOrder` 预留 `is_cold()` 接口；M25/M26 已完成并独立提交。

#### M27：范围检查 + veneer 松弛

- `resolve` 松弛循环：slot 定长（4B）→ 精确计算每标签偏移 → 找出超范围
  分支 → veneer 插在分支自身之后（块终结符，无 fallthrough）；若该分支
  已被优化成单条件且另一目标为 fallthrough，先强制两指令形式再插 veneer；
  重算偏移重复直至稳定（单调收敛，≤3 轮）。
- `Cbz/Cbnz/Tbz/Tbnz` 已接入 slot（M25），veneer 前缀由 Branch14/19 类型
  驱动。
- 验收：合成 >1MB 代码块用例验证 veneer 正确且松弛收敛（≤3 轮）；QEMU
  差分（`tests/test.py`）通过；全 benchmark 编译成功无汇编器超范围报错。

#### M28：RISC-V 适配

- `CondBr` → `beqz/bnez` 倒相 slot；`LabelKind`：B-type ±4KB / JAL ±1MB；
  veneer 用 `la t6,X; jr t6`。
- `abi_matrix` AArch64 + RISC-V × `-O0/1/2` 全回归。
- 验收：RISC-V 全部用例 5 次 byte-identical；小用例 asm 检查（h-1-01 等）
  `CondBr` 收敛为 1 条 `beqz/bnez`；QEMU 差分通过。

#### M29：测试、门禁与收尾

- 移植 Cranelift buffer 行为测试思路：fallthrough 消除、条件翻转、穿线链、
  别名防环、超范围 veneer、截断后标签簿记。
- 确定性门禁、on/off 差分、QEMU 语义差分纳入 `tests/`。
- 性能差分表：全 `benchmarks/` 用例的 `.s` 指令数 vs clang（记录静态模型
  改进，按 §1.3 原则不声称实机收益）。
- TODO.md 收尾；文档记录设计决策与遗留项（冷块沉底、CmpImm+CondBr→cbz
  融合）。

### 2.7 预计文件范围

核心修改：

- `taki_mir/src/emit_buffer.rs`（M25 已建）
- `taki_mir/src/emit.rs`（M25 已改）
- `taki_mir/src/vcode.rs`（M25 已改）
- `taki_mir/src/stats.rs`
- `anon_armv8/src/instructions.rs`（M25 已改）
- `anon_armv8/src/labels.rs`（M25 已改）

机械适配：

- 其他对 `MInst` 做 exhaustive match / `EmitContext` 实现的位置
- `anon_armv8/src/passes/*`（DCE / PeepholeCombine / PairCombine 对 emit
  无依赖，仅确认不改）
- `uika_riscv/src/instructions.rs`、`uika_riscv/src/abi.rs`

测试与统计：

- `taki_mir/src/emit_buffer.rs` 内嵌单测（或专用测试模块）
- `tests/` functional cases、`benchmarks/` 性能差分
- `abi_matrix` 双 target 回归

### 2.8 验收标准

- `cargo test --workspace` 全通过；AArch64 + RISC-V × `-O0/1/2` 编译成功且
  5 次 byte-identical。
- huffman-01 静态指令数 870 → 687（-21%，M26 已达成；M27/M28 目标为
  RISC-V 侧同等收敛），热循环（`_and`/`_or` 每轮）少 1-2 条分支。
- AArch64 `.s` 输出不再出现 `1f` 局部标号 trampoline；所有分支为直接
  `b.cond`/`b`（超范围场景为 veneer 形式）；RISC-V `1f` 在 M28 清理。
- QEMU differential 全通过（`tests/test.py`）。
- 所有 benchmark 无汇编器"branch out of range"错误。
- `BranchOptStats` 有统计值；`-O0` 与 `-O1` on/off 差分无行为差异。

---

## 3. 后续候选工作

按预期收益排序，均需在前一项验证后再启动。

### P1：常量 / 分支参数物化源头治理

DCE 是兜底；更优解是 lowering 时就不为未使用的 block param 和分支参数物化
常量与 `mov wzr`。DCE 落地后统计 `instructions_removed` 的构成，若某类来源
占主导，直接在 `taki_mir/src/lower.rs` 或 `anon_armv8/src/lower.rs` 消除源头。

### P1：短路 `&&`/`||` 的 flags 融合（ccmp / merge-phi 分支折叠）

`_and`/`_or`/`_xor` 热循环体内 `cset → cmp → b` 的 bool 物化链（每 bit
迭代 ~20 条 vs clang 的 `ccmp`+`csel` 13 条）。根因：IR 里 `&&` 是
`zext i1→i32` + `icmp ne ...,0` + 汇聚 phi（`phi [0],[zext]`），
`select_branch_condition`（`anon_armv8/src/lower.rs:951`）只处理单一比较的
直线形式。候选方向：扩展 lowering 识别 merge-phi 形式在 predecessor 上按
flags 直接分支；或新增 `ccmp` 融合。与 M25-M29 的 EmitBuffer 无依赖，
可并行设计。

### P2：phi 拷贝 coalescing

循环末尾的 `mov x5, x4; mov x12, x3` 并行拷贝链，可在 RA 后消除部分拷贝。

### P2：跨块 / 全局调度

块内调度对被 call 切碎的热点无能为力。候选方向：循环不变 load 外提
（`adrp+add+ldr gv_*` 全局量地址重算）、跨块 hoist。属大改动。

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
260-265`）把冷块沉到函数末尾；配合 M25-M29 的 EmitBuffer，冷块天然获得
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

## 4. 总体执行原则

1. 每个 milestone 独立提交；`TODO.md` 在 milestone 完成后删除对应已完成
   细节，只保留后续工作。
2. 正确性验证、静态模型测试和实机性能测量分层维护，不互相替代。
3. 所有 AArch64 改动必须同时验证 RISC-V 不受影响。
4. 新 pass 默认走"白名单 + 保守保留"策略，宁漏勿错。
5. 未获得实机数据前，只能声称"静态模型改进"。
6. ABI/codegen 架构改动（M19-M24、M25-M29）不由优化 flag 控制，任何优化
   级别都必须保持正确；分支优化规则本身由 `-O` 控制。
7. 发射层改造以行为等价为第一优先级，优化规则在等价基线上逐步开启。

---

## 5. 风险与缓解

| 风险 | 严重度 | 缓解措施 |
|------|--------|---------|
| 别名链成环 / 截断后标签簿记错误 | 高 | 完整移植 Cranelift 不变量（`labels_at_tail` 精确完备、`truncate_last_branch` 簿记）；专项单测；on/off 差分 |
| 多指令 MInst（movz+movk、adrp+add、cmp+csel）slot 化破坏"每 slot 4B"假设 | 中 | slot 粒度 = 单条指令，`emit` 顺序写多个 slot；verify 断言发射 slot 数 == 指令数 |
| veneer 插入改变偏移导致松弛不收敛 | 中 | 单调性（只增不减）+ 最大迭代上限（≤3 轮）+ 每轮全量范围断言 |
| 分支优化与 post-RA ListScheduler 交互 | 低 | 调度在 vcode 层（块内），EmitBuffer 只在块边界截断，块内顺序不变 |
| 汇编器对超范围分支报错 | 低 | `resolve` 保证发射前所有分支在范围内；veneer 全覆盖 |
| RISC-V B-type ±4KB 范围触发大量 veneer | 低 | veneer 仅在超范围时触发，`la+jr` 4 条/veneer |
| DCE 误删有隐式副作用的指令（flags、内存、call） | 高 | 白名单制；无 def 指令一律跳过；全量功能回归；on/off 差分 |
| 缺失 register/NZCV/memory 依赖导致调度误编译 | 高 | 保守 catch-all barrier、on/off 差分、`verify_sched_deps`（待做） |
| 发射层改造破坏 RISC-V 或 tail-call | 中 | M28 双 target + tail-call 矩阵回归；`ArgSlot` 布局不变 |
| QEMU wall time 被误用为 A53 性能数据 | 中 | QEMU 仅进入 correctness gate |
| 实机环境频率/温度噪声掩盖结果 | 中 | core pinning、paired samples、95% CI、环境元数据 |
