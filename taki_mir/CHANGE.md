# 寄存器分配器实现 — 改动总结

基于 regalloc2/fastalloc 的反向线性扫描（SSRA）算法，为 `taki_mir` 的 MIR 层实现了完整的寄存器分配器。

## 文件改动

### 1. `src/abi.rs` — 修复编译

- 移除 `entity_impl!` 宏调用
- 手动展开 `StackSlot` 的 `EntityRef`、`Display`、`Debug` 实现

原因：`tomori_utils` 的 `entity_impl!` 宏在 edition 2024 下 `$crate::__core` 解析失败。

### 2. `src/reg_alloc.rs` — 模块声明

新增三个子模块声明：

- `pub mod vregset;`
- `pub mod lru;`
- `pub mod alloc;`

### 3. `src/reg_alloc/reg.rs` — 类型扩展（新增 ~280 行）

| 新增类型 | 说明 |
|----------|------|
| `SpillSlot::invalid()` / `is_invalid()` / `is_valid()` / `raw_bits()` | spillslot 有效性判断 |
| `Allocation` | 32位打包 (kind:3 + index:28)，`None \| Reg(PReg) \| Stack(SpillSlot)` |
| `AllocationKind` | `None \| Reg \| Stack` 枚举 |
| `InstPosition` | `Before \| After` — 指令前后位置 |
| `ProgPoint` | 32位打包 (inst_id:31 + pos:1)，`before(inst)` / `after(inst)` |
| `Edit` | `Move { from: Allocation, to: Allocation }` — 插入的搬移指令 |
| `Output` | 分配器输出：`allocs`, `inst_alloc_offsets`, `edits`, `num_spillslots` |

### 4. `src/reg_alloc/vregset.rs` — 新建（~85 行）

活跃虚拟寄存器双向链表集合，支持 O(1) 插入/删除：

- `VRegSet::with_capacity(num_vregs)` — 预分配哨兵节点
- `insert(vreg)` — 头部插入
- `remove(vreg_num)` — 按 vreg 索引删除
- `iter()` — 遍历所有活跃 vreg
- `VRegSetIter` — 迭代器实现

### 5. `src/reg_alloc/lru.rs` — 新建（~175 行）

每寄存器类的 LRU 淘汰缓存 + 辅助容器：

- **`Lru`**：基于 `Vec<LruNode>` 的环形双向链表，按 PReg 的 `hw_enc` 索引
  - `poke(preg)` — 标记为最近使用
  - `pop()` — 弹出最久未用的 PReg
  - `last(from: PRegSet)` — 从给定集合中找最久未用的 PReg
  - `remove()` / `insert_before()` — 内部链表操作

- **`PartedByRegClass<T>`**：`[T; 3]`，按 `RegClass::Int/Float/Vector` 索引

- **`Lrus = PartedByRegClass<Lru>`**：三个寄存器类各一个 LRU，构造方法 `Lrus::new(&int, &float, &vec)`

### 6. `src/reg_alloc/alloc.rs` — 新建（~1240 行）

核心反向线性扫描分配器，结构分为四层：

#### 辅助容器类型

- `PartedByOperandPos<T>` — `[T; 2]`，按 `Early/Late` 索引
- `PartedByExclusiveOperandPos<T>` — `[T; 3]`，按 `EarlyOnly/LateOnly/Both` 索引
- `ExclusiveOperandPos` — 独占阶段枚举
- `Operands` — 操作数切片包装，提供 `use_ops()`, `fixed()`, `late()`, `early()` 迭代器

#### 数据层

- **`VCodeRef`**：`VCodeContainer` 的简化视图，提供：
  - `block_insts(block) → (start, end)`
  - `inst_operands(inst) → &[Operand]`
  - `branch_blockparams(block, inst, succ_idx) → &[VReg]`
  - `block_succs(block) / block_preds(block)`
  - `is_branch(block, inst) / inst_clobbers(inst)`

- **`Stack`**：spillslot 分配器（对齐分配）
- **`Allocs`**：输出存储，`(inst_idx, op_idx) → Allocation` 映射

#### 状态层 — `State`

可变分配状态，每指令重置：

| 字段 | 说明 |
|------|------|
| `vreg_allocs: Vec<Allocation>` | 每个 vreg 的当前分配 |
| `vreg_spillslots: Vec<SpillSlot>` | 每个 vreg 的专用 spillslot |
| `vreg_in_preg: Vec<VReg>` | 每个 PReg 当前被哪个 vreg 占用 |
| `available_pregs: PartedByOperandPos<PRegSet>` | Early/Late 阶段可用寄存器集合 |
| `num_available_pregs: PartedByExclusiveOperandPos<PartedByRegClass<i16>>` | 可用数量计数 |
| `lrus: Lrus` | 三个寄存器类的 LRU 缓存 |
| `edits: Vec<(ProgPoint, Edit)>` | 输出 edit（反向收集，最终反转） |

核心方法：

- `evict_vreg_in_preg()` — 逐出 PReg 中的 vreg 到 spillslot
- `add_move()` — 插入搬移指令（自动处理 stack-to-stack + scratch）
- `freealloc()` — 释放 vreg（标记为 dead）
- `allocd_within_constraint()` — 检查当前分配是否满足约束
- `select_suitable_reg_in_lru()` — LRU 选寄存器
- `alloc_reg_for_operand()` — 为 Reg 约束操作数分配 PReg
- `alloc_operand()` — 为任意约束操作数分配（Reg/Stack）

#### 环境层 — `Env`

顶层上下文，组合 `VCodeRef` + `State` + `Allocs` + `live_vregs`。

#### 算法主流程

```
run(vcode, mach_env) → Output
│
├─ 构建 Env
├─ for block in (0..num_blocks).rev():
│   ├─ alloc_block(block):
│   │   ├─ for inst in block_insts(block).rev():
│   │   │   └─ alloc_inst(block, inst):
│   │   │       ├─ ① reset_available_pregs()        ← 重置每指令状态
│   │   │       ├─ ② 统计 any-reg 操作数
│   │   │       ├─ ③ 预留 fixed-reg
│   │   │       ├─ ④ 移除 clobber
│   │   │       ├─ ⑤ 逐出 fixed/clobber 冲突
│   │   │       ├─ ⑥ 分配 late 操作数（先 def 后 use）
│   │   │       ├─ ⑦ 分配 early 操作数（先 use 后 def）
│   │   │       ├─ ⑧ 插入 use 操作数的 before-move
│   │   │       └─ ⑨ 若分支指令: process_branch()
│   │   │           ├─ 将 branch args 放入 spillslots
│   │   │           ├─ 并行 move 解析（检测循环→scratch register）
│   │   │           └─ 生成块间 move edits
│   │   └─ reload_at_begin(block):
│   │       ├─ 释放 block params
│   │       ├─ 将 live-in vregs 设为 spillslot
│   │       ├─ 插入 reload moves
│   │       └─ 检查前置分支 fixed-reg def
│   └─ 下一 block
├─ 反转 edits（保证正序）
└─ 返回 Output
```

#### 并行 Move 解析

`resolve_parallel_moves()` 实现了与 regalloc2 相同的算法：

1. 去重、去自循环
2. 用二分查找检测是否存在 src→dst 重叠
3. 若无重叠 → 直接返回顺序 moves
4. 若有循环依赖 → 构建依赖图，拓扑排序
5. 遇到环 → 用 scratch register 破环：

   ```
   {A→B, B→A}  →  scratch ← A, B ← A, ... ← scratch
   ```

6. scratch register 优先从可用 PReg 选，其次用 scratch_regs[class]，最后回退到临时 spillslot

### 7. `src/lib.rs` — 无需改动

新增模块通过 `reg_alloc.rs` 自动引入。

## 使用方式

```rust
use taki_mir::reg_alloc::alloc::{run, VCodeRef};

let vcode_ref = VCodeRef {
    num_insts: ...,
    num_blocks: ...,
    num_vregs: ...,
    operands: &...,
    operands_range: &...,
    block_range: &...,
    // ... 其他字段
    spillslot_size: 4,
};

let output = run(&vcode_ref, &machine_env)?;
// output.allocs[..] — 每个操作数的分配结果
// output.edits[..] — 需插入的搬移指令
// output.num_spillslots — 需要的栈空间
```

## 与 fastalloc (regalloc2) 的关键差异

| 方面 | fastalloc | 本实现 |
|------|-----------|--------|
| 输入接口 | `Function` trait（动态分发） | `VCodeRef` 结构体（直接访问） |
| Inst/Block 类型 | 自己的 `Inst`/`Block` 类型 | `u32` 索引 |
| 操作数获取 | `func.inst_operands(inst)` | `vcode_ref.inst_operands(inst as usize)` |
| CFG 查询 | `func.block_succs(block)` | `vcode_ref.block_succs(block)` |
| 分支判断 | `func.is_branch(inst)` | `vcode_ref.is_branch(block, inst)` |
| SpillSlot 可见性 | `pub bits: u32` | `pub(crate) raw_bits()` 方法 |
| SpillSlot 创建 | `SpillSlot::new(index)` | 同（对齐分配在 Stack 内处理） |
| `$crate::__core` | 内部 crate 宏 | 不使用（已手动展开 `abi.rs` 中的宏调用） |
