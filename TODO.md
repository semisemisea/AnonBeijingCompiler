# Compiler TODO

Only pending work belongs in this file. Completed implementation notes and
historical measurements belong in commits, tests, or dedicated documentation.

---

# AArch64 栈指针调度改造（方案 C：架构级，参考 cranelift）

## 一、问题来源

`make mca ./results/perf/huffman-01.s` 在 Cortex-A53（双发射顺序核）上的指标：

| 指标 | 本编译器 | clang -O2 |
|---|---|---|
| IPC | 0.22 | 0.24 |
| Block RThroughput | 389.0 | 248.5 |
| Total Cycles (×100) | 319 801 | 204 602 |
| 指令数/迭代 | 717 | 485 |

每个函数 prologue 用 5–6 条指令完成"分配 32 字节栈帧"：

```asm
stp x29, x30, [sp, #-16]!   ; cycle 0-4
mov x17, sp                  ; cycle 1-4   ← 多余
movn x14, #0x1f              ; cycle 2-5   ← 多余
add x17, x17, x14            ; cycle 3-6   ← 多余
mov sp, x17                  ; cycle 4-7   ← 多余
add x29, sp, #48             ; cycle 5-8
```

clang 等价工作只要 2 条（`stp x29,x30,[sp,#-16]!` + `sub sp, sp, #32`），cycle 0–2 即结束。
全文件共 53 处 `mov x17,sp / movn x14 / mov sp,x17` 反模式。

## 二、根因

### 2.1 直接根因

`anon_armv8/src/abi.rs:420-437` 的 `append_sp_adjust()` 始终走"`x17 = sp + amount`，再 `mov sp, x17`"两步：

```rust
fn append_sp_adjust(insts: &mut SmallVec<[MInst; 16]>, amount: i64) {
    if amount == 0 { return; }
    append_add_constant(insts, Writable::from_reg(regs::int_reg(17)), Gpr::Sp, amount);
    insts.push(MInst::MovPhys {                              // ← 多余
        size: OperandSize::Size64,
        dst: Gpr::Sp,
        src: Gpr::Reg(regs::int_reg(17)),
    });
}
```

之所以要绕路，是因为 `MInst::AluRRImm12` 的字段类型是：

```rust
// anon_armv8/src/instructions.rs:370
AluRRImm12 {
    op: AluOp,
    size: OperandSize,
    dst: WritableReg,   // ← Writable<Reg>，不能表示 SP
    src: Gpr,           // ← Gpr 可以是 Gpr::Sp
    imm: Imm12,
}
```

`Writable<Reg>` 持有的是 `Reg`（虚拟/物理寄存器），而我们的设计里 SP 是
`regs::Gpr::Sp` 这个独立变体（`anon_armv8/src/regs.rs:34`），不能塞进 `Reg`。
结果：A64 原生支持的、单周期单发射的 `add sp, sp, #imm` 被拆成了 `mov→add→mov` 三条。

### 2.2 次要问题

`append_add_constant`（`abi.rs:446`）只用
`Imm12::new(amount, false).filter(|_| amount <= 0xfff)`，未利用 `lsl #12` 形式，
导致 4096–0xfff000 范围的栈帧也走 fallback 多条指令链。

## 三、关键观察

### 3.1 RISC-V 后端早已用 cranelift 风格

`uika_riscv/src/regs.rs:72` 已经把 SP 当成普通 `Reg`：

```rust
pub fn stack_reg() -> Reg { x_reg(2) }
// x_reg(enc) = Reg::from_physical_reg(PReg::new(enc, RegClass::Int))
```

整个 RISC-V 后端没有 `Gpr::Sp` 这种特殊变体，所有指令的 `dst: Writable<Reg>`
自然能装下。`zero_reg()` 同理（`x_reg(0)`）。

**结论：`taki_mir` 的 regalloc 框架早就支持"SP 当作普通 Reg"，AArch64 后端是仓库里唯一的例外。**

这是 AArch64 的 XZR/SP 共享硬件编码 31 带来的历史包袱——`Gpr` 枚举是为了在
类型层面区分这两种语义。

### 3.2 PReg 编码空间约束

cranelift 在 `inst/regs.rs:79` 把 SP 放在 `PReg::new(31 + 32, Int)`（hw_enc=63），
用 `hw_enc & 31` 在编码时还原成 31。

我们仓库里 `taki_mir/src/reg_alloc/reg.rs:32` 把 `(Int, 63)` 占用作 `PReg::INVALID`
哨兵：

```rust
pub const INVALID: u8 = ((RegClass::Int as u8) << Self::MAX_BITS) | (Self::MAX as u8); // = 63
```

并且 `Operand::new` 的 `FixedReg(preg)` 把 `0b1000000 | preg.hw_enc()` 塞进
7-bit `CONSTRAINT_BITS` 字段（`reg.rs:659`），hw_enc 必须 ≤ 63。

**所以 cranelift 同款 `31+32` 技巧需要先解决哨兵冲突。**

## 四、解决方案：方案 C（架构级）

参考 `wasmtime/cranelift/codegen/src/isa/aarch64/abi.rs:575 gen_sp_reg_adjust`，
把 AArch64 后端改成与 RISC-V 后端、cranelift 一致的"SP 是普通 Reg"风格。

### SP 落位策略：Path 1（换 INVALID 哨兵位置）

把 `PReg::INVALID` 从 `(Int, 63)` 挪到 `(Int, 62)`——hw_enc 62 在 AArch64/RISC-V
里都未使用（AArch64 最高 x30/LR，RISC-V 最高 x31）。只改一行常量 + 几处断言，
不动 bit-packing。SP 拿到 `(Int, 63)`，emit 时 `& 31` 还原成硬件编码 31。

RISC-V/AArch64 实际使用的 Int 编码最大到 31，因此 62 安全。

## 五、分阶段实施计划

### Phase 1 ── `taki_mir` 核心：腾出 PReg(Int, 63)  ✅ DONE

**文件：`taki_mir/src/reg_alloc/reg.rs:32`**

```rust
// before
pub const INVALID: u8 = ((RegClass::Int as u8) << Self::MAX_BITS) | (Self::MAX as u8);

// after  —— MAX 仍是 63，但 63 留给 SP；62 充当哨兵
pub const INVALID: u8 = ((RegClass::Int as u8) << Self::MAX_BITS) | (Self::MAX as u8 - 1);
```

**审计点：**
- `PReg::invalid()` 全部 10 处调用（`taki_mir/src/reg_alloc/ion/*`）：只把 invalid 当
  "无寄存器" 哨兵比较，具体编码无所谓，安全。
- `Allocation` 用 `kind` 字段区分 None/Reg/Stack，不靠 index 辨别 invalid，
  `Allocation::reg(PReg::new(63, Int))` 不会与 `Allocation::none()` 冲突。
- `Operand::FixedReg` 编码 `0b1000000 | preg.hw_enc()` 在 hw_enc=63 时仍能装进
  7-bit CONSTRAINT_BITS（边界值，可用）。

**新增测试：**
```rust
#[test]
fn preg_invalid_does_not_collide_with_sp_slot() {
    assert_ne!(PReg::invalid(), PReg::new(63, RegClass::Int));
}
```

**验证：** `cargo test -p taki_mir` 全绿。

### Phase 2 ── `anon_armv8`：引入 SP 为普通 Reg

#### 2.1 `anon_armv8/src/regs.rs` 新增

仿 RISC-V `stack_reg()`，加入：

```rust
/// SP 在 PReg 空间里的内部编码。hw_enc=63；硬件编码 = 63 & 31 = 31。
/// 与 cranelift 的 `inst/regs.rs:79 stack_reg()` 设计一致。
pub const SP_HW_ENC: u8 = 63;

pub const fn stack_preg() -> PReg {
    PReg::new(SP_HW_ENC as usize, RegClass::Int)
}

pub const fn stack_reg() -> Reg {
    Reg::from_physical_reg(stack_preg())
}

pub const fn writable_stack_reg() -> Writable<Reg> {
    Writable::from_reg(stack_reg())
}
```

#### 2.2 删除 `Gpr::Sp` 变体

`anon_armv8/src/regs.rs:34`：

```rust
// before
pub enum Gpr {
    Reg(Reg),
    Sp,
    Zr,
}

// after —— 与 RegOrZr 同型，可作类型别名
pub enum Gpr {
    Reg(Reg),
    Zr,
}
```

#### 2.3 `anon_armv8/src/instructions.rs:1501 emit_reg` 加 SP 特判

本仓库发文本汇编（非二进制），所以只需在文本输出层处理：

```rust
fn emit_reg(ctx: &mut dyn EmitContext, reg: Reg, size: OperandSize) -> core::fmt::Result {
    if reg.to_real_reg() == Some(regs::stack_preg()) {
        return write!(ctx, "sp");   // AArch64 不使用 wsp
    }
    match (reg.to_real_reg(), size) {
        (Some(preg), OperandSize::Size32) if preg.class() == RegClass::Int => {
            write!(ctx, "w{}", preg.hw_enc())
        }
        (Some(preg), _) if preg.class() == RegClass::Int => write!(ctx, "x{}", preg.hw_enc()),
        _ => ctx.write_reg(&reg),
    }
}
```

#### 2.4 `use_gpr` / `def_gpr`（`instructions.rs:899/914`）

保留对 `Gpr::Zr` 的 no-op；对 `Gpr::Reg(r)` 改为：若 `r == stack_reg()` 则 no-op
（SP 不参与分配），否则照常 `collector.reg_use(r)`。等价于今天 `MovPhys` 对
`Gpr::Sp` 的处理。

```rust
fn use_gpr(collector: &mut impl OperandVisitor, gpr: &mut Gpr) {
    if let Gpr::Reg(reg) = gpr {
        if *reg != regs::stack_reg() {
            collector.reg_use(reg);
        }
    }
}

fn def_gpr(collector: &mut impl OperandVisitor, gpr: &mut Gpr) {
    if let Gpr::Reg(reg) = gpr {
        if *reg != regs::stack_reg() {
            collector.reg_def_reg(reg);
        }
    }
}
```

### Phase 3 ── `anon_armv8`：放宽指令字段类型

把所有 `dst: WritableReg` + `src/lhs: Gpr` 的组合里，**`src/lhs` 改成普通 `Reg`**
（SP 由此可流入）。

| 指令 | 文件位置 | 现状 | 目标 |
|---|---|---|---|
| `MovPhys` | `instructions.rs:452` | `dst: Gpr, src: Gpr` | `dst: WritableReg, src: Reg` |
| `AluRRImm12` | `instructions.rs:370` | `dst: WritableReg, src: Gpr` | `dst: WritableReg, src: Reg` |
| `AluRRRExtend` | `instructions.rs:400` | `dst: WritableReg, lhs: Gpr, rhs: Reg` | `dst: WritableReg, lhs: Reg, rhs: Reg` |
| `AMode::{base: Gpr}` | `instructions.rs:232` 等 | `Gpr` | `Reg` |

ABI/lower 调用点（约 30 处）：
- `Gpr::Reg(x)` → `x`
- `Gpr::Sp` → `regs::stack_reg()`

`emit_gpr` 仍保留用于 `RegOrZr`/`Gpr::Zr` 文本输出。

#### 调用点清单（来自全文件扫描）

`anon_armv8/src/abi.rs`：
- `:222` `base: Gpr::Sp` → `base: regs::stack_reg()`
- `:236` `Gpr::Sp` → `regs::stack_reg()`
- `:255` `base: Gpr::Sp` → `regs::stack_reg()`
- `:307` `Gpr::Sp` → `regs::stack_reg()`
- `:370-372` `AMode::FrameSlot/SpOffset/OutgoingArg` base → `regs::stack_reg()`
- `:394-403` `legalize_amode` 中 `Gpr::Sp` 分支可删（直接用 stack_reg() 作 base）
- `:429` `Gpr::Sp` → `regs::stack_reg()`
- `:434-435` `MovPhys { dst: Gpr::Sp, src: Gpr::Reg(x17) }` → 见 Phase 4 重写
- `:458-468` 同上

`anon_armv8/src/lower.rs`：
- `:151` `src: Gpr::Reg(lhs)` → `src: lhs`
- `:379, 395, 459, 1675, 1682, 1690, 1699, 1714` 类似改写

`anon_armv8/src/instructions.rs`：
- `:1130, 1375, 1381` `Gpr::Zr` 保留
- `:1534-1536` `emit_gpr` 中 `Gpr::Sp` 分支删除（统一走 `emit_reg`）

### Phase 4 ── `anon_armv8/abi.rs`：cranelift 同款 `gen_sp_reg_adjust`

#### 4.1 新增 `Imm12::maybe_from_u64`

**文件：`anon_armv8/src/instructions.rs:25`**

照搬 `cranelift/.../inst/imms.rs:282`：

```rust
impl Imm12 {
    pub fn maybe_from_u64(val: u64) -> Option<Self> {
        if val & !0xfff == 0 {
            Some(Self { value: val as u16, shift12: false })
        } else if val & !(0xfff << 12) == 0 {
            Some(Self { value: (val >> 12) as u16, shift12: true })
        } else {
            None
        }
    }
    // 保留现有 new/value/shift12 不动
}
```

#### 4.2 重写 `append_sp_adjust`（`abi.rs:420`）

参考 `cranelift/.../abi.rs:575 gen_sp_reg_adjust`：

```rust
fn append_sp_adjust(insts: &mut SmallVec<[MInst; 16]>, amount: i64) {
    if amount == 0 { return; }
    let amount: i32 = amount.try_into().expect("frame adjustment fits in i32");
    let (abs, op) = if amount < 0 {
        (-amount as u64, AluOp::Sub)
    } else {
        (amount as u64, AluOp::Add)
    };

    if let Some(imm) = Imm12::maybe_from_u64(abs) {
        // add/sub sp, sp, #imm [, lsl #12]   ← 单条指令
        insts.push(MInst::AluRRImm12 {
            op,
            size: OperandSize::Size64,
            dst: regs::writable_stack_reg(),
            src: regs::stack_reg(),
            imm,
        });
    } else {
        // cranelift 同款 fallback：load_constant(tmp) + add sp, sp, tmp, uxtx
        let tmp = Writable::from_reg(regs::int_reg(regs::INT_POST_RA_SCRATCH[0]));
        insts.extend(materialize_integer_constant(abs, OperandSize::Size64, tmp));
        insts.push(MInst::AluRRRExtend {
            op,
            size: OperandSize::Size64,
            dst: regs::writable_stack_reg(),
            lhs: regs::stack_reg(),
            rhs: tmp.to_reg(),
            extend: ExtendOp::Uxtx,
            shift: 0,
        });
    }
}
```

#### 4.3 顺带改进 `append_add_constant`（`abi.rs:446`）

让 `StackAddr` 大偏移路径也用上 shift12 形式：

```rust
// before
if let Some(imm) = Imm12::new(amount as u16, false).filter(|_| amount <= 0xfff) { ... }

// after
if let Some(imm) = Imm12::maybe_from_u64(amount as u64) { ... }
```

### Phase 5 ── 验证

#### 5.1 单元测试（`anon_armv8/src/instructions.rs:1831 mod tests`）

```rust
#[test]
fn stack_reg_prints_as_sp() {
    let mut ctx = TestEmitContext::default();
    emit_reg(&mut ctx, regs::stack_reg(), OperandSize::Size64).unwrap();
    assert_eq!(ctx.0, "sp");
}

#[test]
fn imm12_handles_unshifted_form() {
    let imm = Imm12::maybe_from_u64(0xfff).unwrap();
    assert_eq!(imm.value(), 0xfff);
    assert!(!imm.shift12());
}

#[test]
fn imm12_handles_shifted_form() {
    let imm = Imm12::maybe_from_u64(4096).unwrap();
    assert_eq!(imm.value(), 1);
    assert!(imm.shift12());

    let imm = Imm12::maybe_from_u64(0xfff000).unwrap();
    assert!(imm.shift12());
}

#[test]
fn imm12_rejects_unrepresentable_values() {
    assert!(Imm12::maybe_from_u64(0x1000_0000).is_none());
    assert!(Imm12::maybe_from_u64(0xfff_001).is_none());
}
```

#### 5.2 `cargo test -p taki_mir -p anon_armv8`

所有现有 19 + 新增测试全绿。

#### 5.3 `make test perf`

重生成 `results/perf/huffman-01.s` 并运行，断言：
- `mov x17, sp / movn x14 / mov sp, x17` 53 处全部消失；
- prologue 从 5–6 周期降到 2 周期；
- `Block RThroughput` 从 389.0 显著下降（预期 ≤ 320）；
- 所有 perf 用例功能正确（runtime return = 0）。

## 六、改动半径汇总

| 文件 | 估计行数 | 性质 |
|---|---|---|
| `taki_mir/src/reg_alloc/reg.rs` | ~5 | 改一个常量 + 加测试 |
| `anon_armv8/src/regs.rs` | ~15 | 删 `Gpr::Sp`，加 `stack_reg()` 系列 |
| `anon_armv8/src/instructions.rs` | ~60 | 字段类型放宽、`emit_reg` 加特判、`Imm12::maybe_from_u64`、新单测 |
| `anon_armv8/src/abi.rs` | ~40 | 重写 `append_sp_adjust`、`append_add_constant`、修调用点 |
| `anon_armv8/src/lower.rs` | ~30 | `Gpr::Reg(x)` → `x`、`Gpr::Sp` → `stack_reg()` |
| **合计** | **~150** | 不跨 RISC-V 后端 |

## 七、已知风险点

### 7.1 `OperandConstraint::FixedReg(SP)`

当前编码 `0b1000000 | preg.hw_enc()` 在 hw_enc=63 时仍能装进 7-bit CONSTRAINT_BITS
（边界值，可用）。需 `debug_assert` 任何代码都不会构造
`OperandConstraint::FixedReg(stack_preg())`——SP 不参与分配，本就不该有这种约束。

### 7.2 `Allocation::index()` 与旧 INVALID 的索引冲突

SP 的 `preg.index()` = 63，旧 INVALID 也是 63。但 `Allocation` 用 `kind` 字段区分
None/Reg/Stack，不靠 index 辨别 invalid，安全。Phase 1 的回归测试覆盖这一点。

### 7.3 `MovK` 等 reuse-def 指令

SP 不会出现在这些指令里（语义上不合理），由指令构造点保证。

### 7.4 `emit_reg` 与 `emit_gpr` 的边界

删除 `Gpr::Sp` 后，`emit_gpr` 只剩 `Reg`/`Zr` 两个分支。`emit_reg` 增加 SP 特判后，
要保证调用 `emit_gpr` 的指令（如 `AluRRImm12.src`、`AluRRRExtend.lhs` 在 Phase 3 后）
在 `src/lhs == stack_reg()` 时走 `emit_reg` 而非 `emit_gpr`。这通过字段类型从 `Gpr` 改
为 `Reg` 自动保证。

## 八、参考实现

- `wasmtime/cranelift/codegen/src/isa/aarch64/abi.rs:575 gen_sp_reg_adjust`
- `wasmtime/cranelift/codegen/src/isa/aarch64/abi.rs:617 gen_prologue_frame_setup`
- `wasmtime/cranelift/codegen/src/isa/aarch64/inst/regs.rs:67 stack_reg`
- `wasmtime/cranelift/codegen/src/isa/aarch64/inst/imms.rs:282 Imm12::maybe_from_u64`
- `uika_riscv/src/regs.rs:72 stack_reg`（本仓库 RISC-V 后端，早已按 cranelift 风格实现）

## 九、预期收益

完成 Phase 1–5 后，huffman-01（以及所有 perf 用例）：

1. 每个函数 prologue/epilogue 从 5–6 条指令降到 2–3 条：
   ```asm
   stp x29, x30, [sp, #-16]!
   sub sp, sp, #32              ; ← 替代 mov x17, movn, add, mov 四条
   add x29, sp, #48
   ```
2. MCA `Block RThroughput` 从 389.0 显著下降（预期 ≤ 320）。
3. 文本层 `mov x17,sp / movn x14 / mov sp,x17` 模式全部消失。
4. AArch64 后端架构与 RISC-V 后端、cranelift 对齐，便于后续做更多优化（peephole、
   list-scheduling 等）。
