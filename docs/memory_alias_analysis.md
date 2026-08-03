# 内存分析 / 别名分析（Memory & Alias Analysis）设计文档

> 状态：M49（内存对象/别名分析）、M50（完整 Purity 分析）、M53（IPSCCP
> 主存模拟）、M54（LICM load 外提）已实现（2026-08，见 TODO.md §4 与 Git
> 提交记录）；M51/M52/M55/M56 未做。本文档保留设计底稿与后续方向。

## 1. 背景与目标

当前 IR 层没有任何统一的内存分析。每个 pass 各自用一套保守近似处理
Load/Store/GEP，导致一类「内存感知」优化整体缺失或精度极低：

- IPSCCP 把所有内存相关指令一律置为 Bottom（注释原文：*"For now, we lack
  simulation of main memory. So all memory related stuff is considerd as
  variable."*）；
- DCE 把 Store/MemZero 一律视为 critical，永不删除（无 DSE）；
- GVN 的 load-CSE 按精确地址 key，且任意 store/call/memzero 全量失效
  （无 store-to-load forwarding，无按 root/offset 的别名细化）；
- LICM 拒绝外提 Load（AGENTS.md 明令：*"Do not hoist Load without
  alias/mod-ref reasoning"*）；
- pure_function 遇 Call 直接判非纯，`has_side_effect` 恒为 true。

目标：建立**跨函数**、覆盖**数组地址**与 **Load/Store 副作用**的统一分析
底座（内存对象/别名分析 + 函数 mod-ref 摘要），并以此驱动一组内存类优化：
IPSCCP 主存模拟、死 Store 消除（DSE）、Load 消除/转发（DLE/forwarding）、
指针槽消除（SROA 子集）、LICM load 外提等。

SysY 语言约束使建模比通用 C 简单得多：**无指针类型、无 `&` 取址、无堆
分配**。内存对象只有三类——全局（GlobalAlloc）、栈局部（Alloc）、数组形参
（地址）。栈内存对象大小与维度全部编译期已知（数组维度是 ConstExp），
「栈内存因此很好建模」（见 §3）。

## 2. 现状盘点（含代码位置与实测证据）

### 2.1 IPSCCP — 无主存模拟

`raana_ir/src/opt/passes/ipsccp.rs:196-200`：

```rust
// For now, we lack simulation of main memory. So all memory related stuff is
// considerd as variable.
InstKind::GetElemPtr(..) | InstKind::Alloc | InstKind::Load(..) => {
    merge_and_extend(node, Lattice::Bottom, &mut lattice_map);
}
```

`Store`/`MemZero` 直接跳过（`ipsccp.rs:290`），`GlobalAlloc` 在 Stage 0.1
只有一行 TODO（`ipsccp.rs:108`：`// TODO: What about global value?`）。
后果：`a[0] = 5; ... a[0]+1` 这类常量数组访问完全不折叠；常量 GEP
（如 `getelemptr %gv_K, (0, 0)`）地址本身也进不了格。

格是 i32 常量格（`Lattice::Constant(i32)`），浮点直接 Bottom——主存模拟
只对 i32 有收益，float 维持 Bottom 即可。

### 2.2 DCE — 无死 Store 消除

`raana_ir/src/opt/passes/dce.rs:25-54`：

```rust
fn is_critical(value: Inst, data: &FunctionData) -> bool {
    match data.inst_data(value).kind() {
        InstKind::Branch(..) | InstKind::Jump(..) | InstKind::Store(..)
        | InstKind::MemZero(..) | InstKind::Return(..) | InstKind::TailCall(..) => true,
        ...
        InstKind::Call(call) => has_side_effect(call.callee()),  // 恒 true
```

`has_side_effect` 是个恒 true 的占位（`dce.rs:19-22` 注释 TODO：
*"side-effet function rules"*）。Load 只能因无使用而被删，Store 永远保留。
「先写后写同址、中间无读」的死 store、「函数出口后无人再读的局部对象
store」都无法消除。

### 2.3 GVN — load-CSE 精度低、无 forwarding

`raana_ir/src/opt/passes/gvn.rs:215-256` `ScopedLoadLeaders`：按**精确地址
指令**（`Inst` id）为 key，`store_counter` 全局计数，任何 Store/Call/
MemZero（`gvn.rs:319-321`）都使**所有**地址的 leader 失效——无论是否
可能别名。且 key 是地址 Inst 而非「root+offset」，跨块相同地址的 load
只能靠 GVN 先把 GEP 编号去重后间接命中。

`gvn_pre.rs:43` 的 TODO 注释已经规划了演进路径：

> Add memory value numbering separately: exact-address load CSE and local
> store forwarding first, invalidating on uncertain stores, MemZero, or calls.
> Do not add load PRE until aliasing and speculation safety are modeled.
> Add function effect summaries (ReadNone/ReadOnly/WriteOrUnknown) ...

### 2.4 LICM — 拒绝 load

`raana_ir/src/opt/passes/licm.rs:44-54` `can_be_invariant` 只含
Integer/Float/Binary/Cast/GetElemPtr/Select。AGENTS.md 明确要求：
外提 Load 必须基于 alias/mod-ref 推理，这正是本计划要补的。

### 2.5 pure_function / GSP — 副作用信息的两个雏形

`analysis_passes/pure_function.rs`（54 行）：任何 `Call` 直接返回非纯
（注释 `TODO: Use graph algorithm to get better result`）；Load/Store/
GEP 只查 `is_global()`，不区分读写、不分析对象。

`passes/scalar_global_promotion.rs:85-133` 的 `call_analysis` 是**程序级
可能触及分析**：按 call graph 闭包传播每个函数 touched 的全局集合。但它
(a) 不区分读/写（读和写都算 touched，GSP 只提升标量全局所以够用）；
(b) 不覆盖数组全局。这是 mod-ref 摘要（M50）的直接前身。

### 2.6 后端调度 — 已有 MIR 层 root 模型（可作收益测量点）

`anon_armv8/src/sched/dag.rs`：

- `MemRoot { StackSp, StackFp, Global(HirInst), Unknown }`（dag.rs:26-31），
  `MemAccess { kind, root, offset: Option<i64>, size }`（dag.rs:33-39）；
- 内存依赖边只在「任一侧是 Store」时建立（dag.rs:316），`may_alias`
  判定见 dag.rs:610-617：不同 Global root 不相交、Global 与栈不相交；
- **统计已存在**：`DagBuildStats`（dag.rs:144-155）采集
  `known/unknown_root_accesses`、`memory_comparisons`、
  `disjoint_comparisons`、`may_alias_comparisons`、`max_memory_history`，
  并汇入 `taki_mir::stats`——这是量化「别名精度收益」的现成探针。

局限：MIR 层（post-RA），provenance 靠寄存器数据流传播（
`propagated_provenance` dag.rs:479），offset 仅常量 AMode；IR 层更精确的
GEP 仿射信息到不了这里。§5.3 已记录长块内存历史 O(M²) 的最坏复杂度。

### 2.7 实测证据（conv2d-1，-O1，`--emit ir`）

用 `./target/release/compiler --emit ir -O1 tests/perf/conv2d-1.sy` 复现：

**(a) 形参指针槽：热循环每轮重载形参指针。** 数组形参被前端存入局部指针槽，
每次访问都 load 回来：

```
define func <name = conv2d, ret_ty = (), params = (%24: *i32, %25: *i32, %26: *i32)>:
    %11 = alloc <type = **i32, size = 8>
    store %24, %11
    %v_Out = alloc <type = **i32, size = 8>
    store %25, %v_Out
    %v_K = alloc <type = **i32, size = 8>
    store %26, %v_K
    ...
    %63 = load %v_Out <type = *i32>          ; 内层循环里
    %147 = getelemptr %63, %146
    store %vid_4, %147
    ...
    %50 = load %11 <type = *i32>             ; 最内层 k 循环里
    %89 = getelemptr %50, %174
    %83 = load %89
    ...
    %88 = load %v_K <type = *i32>
    %82 = getelemptr %88, %184
    %68 = load %82
```

每个热循环迭代多 1-2 条 `ldr`（形参指针从栈槽重载）。这是「write-once /
read-many」的 Alloc 槽，M51 指针槽消除的直接目标；RISC-V 侧对应附 B 的
A4c（基址重载）与 B9（K 基址每轮重算）问题。

**(b) 冗余回写 store：把刚 load 的原值存回同一全局。** main 入口
`%100 = load %gv_repeat_factor`、`%99 = load %gv_N_eff`，两个全局在循环
期间只读不写，main 出口却：

```
    store %100, %gv_repeat_factor
    store %99, %gv_N_eff
```

这是「store 的值 == 该地址当前值（load 后无中间写）」——DSE 可删。
gv_repeat_factor / gv_N_eff 是指针/标量全局，GSP 只提升标量，这类
「全局槽回写」漏网。

**(c) 常量 GEP 与内联链。** `getelemptr %gv_K, (0, 0)` 偏移全常量；
`idx` 内联后出现 `entry_1_idx_inline_*` 多层 cont 块，GEP 链穿透多层——
偏移求值必须跨块、跨内联产物，而不是只看单个基本块。

## 3. SysY 内存模型与别名规则

### 3.1 对象模型（语言层事实，见 docs/SysY2022语言定义.md）

1. 无指针类型、无取址运算、无堆分配。值类型只有 `int`/`float` 与
   row-major 多维数组；数组维度均为编译期常量（`ConstExp`，非负整数）；
   数组形参只允许省略**第一维**，后续维度长度在形参类型中已知。
2. 标量参数按值传递；数组参数传首地址（`a[1]` 传子数组地址也合法）。
3. 内存对象三类：
   - `GlobalAlloc`：全局变量/数组（未初始化元素恒 0/0.0）；
   - `Alloc`：函数局部变量/数组（未初始化局部值**不确定**，前端仍保守
     `mem_zero` 清零，见 `soyo_compiler/src/frontend/ast.rs:319,526`）；
   - 数组形参：调用方传入的地址（首维长度未知，其余维度已知）。
4. C 互操作边界仅 sysylib 声明（`soyo_compiler/src/frontend/utils.rs`）：
   `getarray/putarray/getfarray/putfarray` 按数组参数读写；
   `_sysy_starttime/_sysy_stoptime` 读写计时状态；`putint/putch/putfloat/
   putf` 读标量参数、写 stdout；`getint/getch/getfloat` 读 stdin。

### 3.2 别名规则（无指针算术 ⇒ 可精确推导）

同一函数内，两个访问的 root 判定：

| root A \ root B | Alloc(j) | Global(g) | Param(q) | Unknown |
|---|---|---|---|---|
| Alloc(i), i≠j | 不相交 | 不相交 | **可能相交** | 可能 |
| Global(g1), g1≠g2 | 不相交 | 不相交 | **可能相交** | 可能 |
| Param(p) | 可能相交 | 可能相交 | **可能相交** | 可能 |
| Unknown | 可能 | 可能 | 可能 | 可能 |

关键推导：

- `Alloc(i) vs Alloc(j)`（i≠j）不相交：无指针算术，局部对象之间不可能
  互相指向；`Param(p) vs 本函数 Alloc(i)` 也不相交——调用者帧中的对象与
  被调者自身帧中的对象是两次独立分配，中间没有指针运算把它们连起来。
- `Global(g1) vs Global(g2)` 不相交：同上。
- `Param(p) vs Param(q)` / `Param(p) vs Global(g)` 可能相交：调用方可以
  把同一个数组（或全局数组）传给两个形参，也可以传全局数组给形参。
  保守处理。
- `Unknown`（地址来源不明，如 sysylib 返回值、load 出的指针）与一切相交。

同 root 内的细化（§5.3）：常量 GEP 偏移可直接求 `[off, off+size)`；
仿射索引用既有 `range.rs`（区间 + 零洞）求索引界，再乘 stride 得字节区间；
两区间不相交 ⇒ 不相交。数组形参首维未知 ⇒ 该维索引区间视为 full，但
第二维起的常量偏移仍可精确（stride 来自形参类型）。

### 3.3 需要识别的前端形态（M49 的地址还原）

- 标量局部：`load alloc` / `store val, alloc`（无 GEP）。
- 数组元素：`gep base, [i]`（base=Alloc/Global/形参）或
  `gep base, [0, i, j]`（`is_pointer_to_array` 时前端前插 0，见
  ast.rs:1315-1317）；标量 index 是任意 Inst（可能又是 GEP 的结果）。
- 形参指针槽：`alloc <**T>; store param, slot; ... load slot`（见 §2.7a）。
- 指针全局：`gv_state` 等存地址的全局（`store %24, %gv_state`）。
- `MemZero(dest, byte_len)`：整区间 store `[0, byte_len)`，语义上等同于
  对该 root 前缀的批量写。

## 4. 核心分析设计

### 4.1 M49：内存对象 / 别名分析（analysis pass）

新增 `raana_ir/src/opt/analysis_passes/memory.rs`，模块化输出，供任意
consumer 复用：

- `MemoryRoot`：`Alloc(Inst)` / `Global(Inst)` / `Param(usize)` /
  `Unknown`（Param 用形参索引，注意函数参数值 = entry 块参数，
  `FunctionData::params()`，见 AGENTS.md）。
- `Access { root, kind: Read|Write, offset: Range<i64>, size }`：
  由指令推导；`GetElemPtr` 递归求 GEP 的字节偏移表达式
  （常量直接求值；仿射 `add/mul` 保留符号表达式）。
- 区间别名判定：`may_alias(a, b)` —— 先按 §3.2 root 规则，root 相同再按
  区间；区间来自 range.rs（`IntRange` 对索引求界）× stride。区间不相交
  才返回「不相交」，其余一律「可能相交」（宁漏勿错）。
- 跨函数：形参 root 的地址对象由**调用侧**决定——M50 摘要自底向上传播
  「形参 p 实参可能指向的 root 集合」；环（递归）保守取并集。

复用：`dom_tree::v2`（跨块 load 判定）、`loop_analysis`、`induction_variable`
（IV 索引区间）、`range`、`opt/utils/gep.rs`、`opt/utils/cfg.rs`。
注意 AGENTS.md 的 IR 不变量：新分析用真实 `BasicBlock/Inst/Function`
handle，不引入第二套 ID；分析结果一律视为快照，CFG 变更后重建。

复杂度：单函数 RPO 一遍，每指令 O(D)（D=GEP 链深，实测 ≤3）；区间运算
O(1)；跨函数摘要 O(F+E)（call graph，见 M50）。

### 4.2 M50：函数副作用摘要（mod-ref，跨函数）

- 输入：call graph（`analysis_passes/call_graph.rs` 已有）+ M49 root 集合。
- 输出：每函数 `(read_set, write_set, unknown_read, unknown_write)`，
  集合元素为 root 或 root 上的一段区间；自底向上闭包：
  `f 的摘要 = 直接读写 ∪ (被调函数摘要按实参 root 绑定后并入)`；
  递归/互相调用用 worklist 迭代到不动点（上限同 IPSCCP 的迭代纪律）。
- sysylib 边界按 §3.1(4) 建模；`put*` 的 stdout 副作用记
  `unknown_write=false, external=true`（对内存别名无影响，但对 DCE 的
  call 裁剪有意义——`put*` 不能被裁）。
- 输出兼作：DCE 的 `has_side_effect` 替换、GVNPRE TODO 中的
  `ReadNone/ReadOnly/WriteOrUnknown` 摘要、GSP 的 touched 分析泛化
  （读写分离 + 数组全局覆盖）。

### 4.3 同 root 区间别名判定（示例）

```
%i = ...            ; 循环 IV, range.rs 给 [0, 100)
%p = gep %In, %i    ; root=Param(0)（调用侧绑定 gv_In）, offset=[0,400), size=4
%q = gep %Out, %i   ; root=Param(1), offset=[0,400), size=4
```

`Param(0) vs Param(1)`：可能相交（若调用方 `conv2d(In, In, ...)`），
跨 root 保守。但 `%p vs %p2 = gep %In, %i+1`：同 root，`[0,400)` 与
`[4,404)` 相交 ⇒ 可能别名；而 `gep %In, 0` 与 `gep %In, 100` 的区间
`[0,4)` 与 `[400,404)` 不相交 ⇒ 确定不相交。后者即可用于 DSE：
`store %v, gep %In, 0; ...; load gep %In, 100` 之间可以安全重排/删除。

## 5. 消费者优化设计

### 5.1 M51：指针槽消除 + 栈对象提升（SROA 子集）

- 现状/现象：§2.7(a)。前端把数组形参存入 `alloc <**T>` 指针槽，热循环
  每轮 `load` 重载；标量全局（gv_*）也有 store-once/load-many 形态。
- 变换（函数内，M50 摘要证明跨调用安全后）：
  1. **指针槽消除**：整个函数内某 Alloc 槽至多一次 store（store-once），
     用存储值替换所有 `load slot`，删槽及其 store——形参指针槽直接
     变成形参值；
  2. **栈对象提升**：局部 Alloc 的全部访问都经由常量 GEP 偏移、且地址
     未逃逸（未传给 call、未与 Param 混用）⇒ 逐元素提升为 SSA 值
     （写后读链由 M52 转发承接），对象整体可删。
- 收益：conv2d 最内层每轮省 `load %v_K` + `load %11` 两条 ldr；RISC-V
  侧 A4c/B9 类「基址每轮重载」随之消失。这是本计划收益/成本比最高的
  单项，建议最先落地。
- 注意：指针槽若发生第二次 store（罕见，如函数内改形参指向），退化为
  M52 的普通转发即可，不必硬做。

### 5.2 M52：Store-to-load forwarding + 死 Store 消除（DSE/DLE）

- 变换：
  1. **转发**：`store v, A` 之后、且到 `load A'` 之间无 may-alias 写，
     `may_alias(A, A')` 且同 root 同 offset（或 A 的区间包含 A' 的
     单元素区间）⇒ `load` 替换为 `v`；
  2. **死 store**：`store v, A` 之后到函数出口（或到确定覆盖它的下一个
     同址 store）之间无可能读到 A 的 load ⇒ 删除；「同址覆盖」用
     区间判定；
  3. **冗余回写**：`load A; ...; store v==load结果, A`（中间无写）⇒ 删
     ——§2.7(b) 的 conv2d main 尾部正是此形态；
  4. **死对象 store**：对某局部 Alloc 的 store，该对象在函数出口后
     不可达（地址未逃逸）且函数内后续无读 ⇒ 删整链（与 M51 重叠，
     优先走 M51）。
  - `MemZero` 视为整区间 store 参与 1-3（byte_len 已知）。
- 与 GVN 的关系：把 `ScopedLoadLeaders` 的「全局 store_counter 失效」改为
  按 M49 `may_alias` 失效（只失效可能被该 store 命中的地址），load-CSE
  精度随之提升——这是**零新增 pass** 的即时收益。

### 5.3 M53：IPSCCP 主存模拟

- 设计：在现有 i32 格之上加**每 root 内存格**
  `MemCell: Map<offset, MemLattice>`，`MemLattice ∈ {Top, Const(i32), Bottom}`：
  - `store v, gep root, off`：`off` 常量 ⇒ `cell[off] = v 的格值`；
    `off` 非常量 ⇒ 该 root 整根降级 Bottom（或按区间格记录）；
  - `load gep root, off`：常量 off ⇒ 取 `cell[off]`；非常量 ⇒ 整根
    读为 Bottom；
  - 跨调用：callee 的 store/load 按 M50 摘要对 root 的写集失效对应 cell
    （call 后按 `write_set ∩ root` 清格；`unknown_write` ⇒ 全清）；
  - 只对局部 Alloc / Global 建模；Param / Unknown root 直接 Bottom
    （形参地址指向谁不知道，建模无意义且不安全）。
  - `GlobalAlloc` 初始化值（`ZeroInit` 或常量聚合）作为 cell 初值。
- 收益：常量数组初始化后的元素折叠（`a[0]=5; b=a[0]*2` → 10）；
  全局标量数组元素常量传播（GSP 只覆盖标量全局）；常量 GEP 地址本身
  可进格（作为已知 root+offset，供 M49 消费）。
- 风险：格状态增大 → 定点轮数上升；用收敛门禁（现有
  `MAX_PIPELINE_ITERATIONS=100`，pass.rs:136）兜底，编译耗时对比见 §7。

### 5.4 M54：LICM load 外提（依赖 M50）

- 条件（AGENTS.md 明令的 alias/mod-ref 推理）：
  1. load 地址在循环内不变（GEP 全不变或经 IV 无关）；
  2. 循环内不存在 may-alias 的 store / MemZero / call（M50 摘要：
     循环涉及函数调用的 write_set 不含该 root 或 unknown_write=false）；
  3. load 无副作用（SysY load 天然无副作用）。
- 满足 ⇒ 外提到 preheader；与 M51 互补：M51 消指针槽，M54 消真正的
  内存 load（如全局数组元素、循环外写循环内读的数组段）。

### 5.5 M55：后端调度别名细化（P2，评估先行）

- 现状：sched/dag.rs root 模型 + 统计探针已就绪；IR 层 GEP 仿射信息
  到不了 MIR。
- 方案：先用现有统计量化「unknown root / may-alias 占比」——若 disjoint
  占比已经很高，收益有限，M55 降级为记录结论；若 may-alias 占主导，
  再把 M49 的 (root, offset) 随 MInst 下沉（anon_armv8 lower 时携带），
  与 §5.3 的内存历史数据结构优化合并实施。
- 约束：MIR 与 IR 的对应维护成本高，必须先用数据证明值得。

### 5.6 M56（可选）：数组全局的部分提升 / 循环不变量基址 hoist

- GSP 泛化：数组全局若「函数内只经常量 GEP 访问」且无逃逸，可把
  元素提升为 SSA（M51 的全局版，需要 M50 证明跨调用只读）；
- 与附 B A4c「全局基址提升进循环前寄存器」呼应——地址层面由
  pointer_strength_reduction 负责，元素层面由本项负责。

## 6. 探索中发现的额外优化机会（非用户点名，一并提出）

1. **前端指针槽模式的源头治理**（§2.7a）：形参 `alloc **T + store +
   per-access load` 是 lowering 层的系统性低效。即便不做 M51 pass，
   在 `soyo_compiler/src/frontend/` 直接让数组形参裸用形参值、省掉
   指针槽也是正收益（类似 §5.1「常量物化源头治理」的思想）。M51 是
   pass 层兜底，两者可并行评估。
2. **冗余回写 store**（§2.7b）：load 后原值存回同一位置。常见于
   「入口读全局 → 出口写回」的防逃逸模式。M52(3) 覆盖。
3. **MemZero 与 store 的统一建模**：前端对每个局部数组发
   `mem_zero(alloc, byte_len)`（ast.rs:319,526），是整区间写。DSE/
   forwarding/IPSCCP 都需把它当 store 处理，否则一切局部数组分析的
   起点都被一个「全区间写」挡住。
4. **与 §5.11（M48 幂等写外提）衔接**：M48 的「零 store」守卫拒绝了
   conv2d repeat 外层（巢内写 Out 但幂等）。有了 M49 区间别名 +
   M50 写集摘要，「写集幂等」判定才有基础——本计划是那条候选的
   前置依赖。
5. **与 M42（SIMD 依赖分析）衔接**：循环依赖/向量化合法性需要的
   「访问函数 + 跨迭代冲突」正是 M49 的 access 抽象 + 区间判定的
   超集。M49 落地后 M42 只补 reuse-distance/gcd 判定即可，无需重写。
6. **常量 GEP 折叠**：`getelemptr %gv_K, (0, 0)` 全常量偏移的地址
   可编译期求值；现在 IPSCCP 把 GetElemPtr 一律 Bottom。M53 顺带覆盖。
7. **sysylib 数组参数摘要**：getarray/putarray 会按参数长度读写数组，
   若按「unknown 写」处理会挡住所有跨 getarray 的转发；按 §3.1(4)
   建模后收益直接可见（读入后立即使用的数组元素可转发）。

## 7. 复杂度 / 运行耗时 / 收益评估

### 7.1 复杂度

| 项 | 复杂度 | 说明 |
|---|---|---|
| M49 别名分析 | O(N×D) / 函数 | RPO 一遍；D=GEP 链深（实测 ≤3） |
| M50 mod-ref 摘要 | O(F+E)×迭代 | call graph 自底向上；递归环 worklist 至不动点 |
| M51 指针槽消除 | O(N) | 每 Alloc 统计 store 次数 + 替换 |
| M52 DSE/转发 | O(N log N) | 块内扫描 + 支配树跨块；每 root 一个最近写 map |
| M53 IPSCCP 主存格 | O(N × Σcell) | 格大小 = 可达常量偏移数；定点轮数需监控 |
| M54 LICM load 外提 | O(循环体) | 复用既有 LICM 结构 |
| M55 调度细化 | 不增复杂度 | 内存历史比较仍是 O(M²)（§5.3 既有问题） |

无超线性编译开销；M53 是唯一可能拖慢定点收敛的项，必须监控。

### 7.2 运行耗时（编译时间）测量方法

- 基线：当前 release 编译器对 `tests/perf` 全量编译时间（中位数，
  多次采样，排除缓存）。
- 每 milestone 落地后同法复测，记录 Δ。
- 定点轮数：`MAX_PIPELINE_ITERATIONS` 内实际迭代次数统计（M53 前后
  对比）；用 `--log`/debug 输出或临时计数器，不进产物。
- 关注点：M53 格状态扩散导致「一次 store 变多次失效」的连锁；若
  编译时间劣化 >5% 或定点轮数翻倍，回退该子项精度（如限制建模对象数）。

### 7.3 收益评估方法

- **静态**：热循环指令数（TODO.md M26 起沿用的 awk 方法）；conv2d /
  many_mat_cal / matmul 系列逐函数对比。M51 预期每轮 -1~2 条 ldr。
- **动态**：gem5 sim_insts（已建 harness）与 qemu wall time（仅作
  正确性 + 相对参考，不声称实机收益，遵守 §1.3）。
- **别名精度探针**：sched DAG 统计（dag.rs:144-179）——
  `known vs unknown root`、`disjoint vs may_alias comparisons` 占比，
  是 M49/M55 效果的直接量化。
- **回归**：functional + h_functional 全量（每个 milestone）、
  perf on/off 差分、双 target × -O0/1/2 5 次 byte-identical
  （对齐 M47/M48 验收口径）。
- 预期收益排序（按成本/收益比）：M51（指针槽，收益最高、实现最窄）>
  M52（DSE/转发，含 GVN 失效细化）> M53（IPSCCP 主存，收益依赖常量
  数组形态的用例密度）> M54（LICM load）> M55（调度，先评估）。

## 8. 合规性（对照 docs/Illegal_optimization.md 与 AGENTS.md）

- 别名/副作用/可达性属于 AGENTS.md 明示合法依据：
  *"Optimization decisions may depend on general IR facts such as types,
  constants, CFG structure, dominance, loop structure, effects, alias
  information..."*。
- 本计划全部变换基于 IR 结构推导，不匹配函数名/变量名/测试用例特征，
  不依赖输入值——与 Illegal_optimization.md 三、四条的禁止项无交集。
- **红线**：不得因 SysY「未初始化局部变量值不确定」删除前端发的
  `mem_zero`，除非能证明该对象的每个读路径都被写覆盖（读未定义值会
  改变程序结果，删除清零不是「UB 利用」，而是改变可观察行为——
  比 UB 更严格，必须按可观察语义对待）。
- 所有分析默认保守：判不了就是 may-alias，宁漏勿错（对齐 §5 执行原则
  4「白名单 + 保守保留」）。
- 编译期不确定的数组索引（如形参首维）一律区间放宽，绝不假设对齐或
  上界。

## 9. 里程碑与依赖

```
M49 别名/内存对象分析（底座，无 IR 变换）
  └─> M50 函数 mod-ref 摘要（跨函数底座）
        ├─> M51 指针槽消除 + 栈对象提升   （收益最快）
        ├─> M52 DSE / 转发 / 冗余回写      （含 GVN 失效细化）
        ├─> M53 IPSCCP 主存模拟
        └─> M54 LICM load 外提
M55 后端调度别名细化（P2，评估先行，可独立于 M51-M54）
M56 数组全局部分提升（可选，依赖 M50/M51 稳定）
```

依赖方向：M49 → M50 → M51/M52/M53/M54；M53、M54 依赖 M50；M55 依赖
M49 但可与消费者解耦。每个 milestone 独立提交，验收即折叠进 TODO.md
摘要（§5 执行原则 1）。

## 10. 关键风险与缓解

| 风险 | 严重度 | 缓解 |
|---|---|---|
| 别名误判（把相交判成不相交）导致删错 store/load | 高 | 区间判定的「不相交」只接受确定证据；root 规则全部可证；on/off 差分 + 全量功能回归 |
| M53 格状态扩散拖慢定点收敛 | 中 | 收敛门禁 + 编译时间监控；必要时限制建模对象数/降级为整根格 |
| 形参指针槽消除后跨调用语义变化（callee 改形参指向？） | 中 | 指针槽 store-once 才消除；M50 摘要证明无逃逸/无二次写 |
| MemZero 与 store 交互建模错误 | 中 | MemZero 统一按整区间 store 建模并专项单测 |
| 数组形参首维未知导致区间放宽过度、收益消失 | 低 | 首维放宽但保留第二维起常量偏移；跨调用传播实参边界（M50） |
| M55 的 MIR/IR 对应维护成本高 | 低 | 先用现有 DAG 统计证明收益再动手；否则记录结论并关闭 |
