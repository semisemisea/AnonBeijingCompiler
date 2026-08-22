//! # 指令谓词：HIR 指令的分类判断
//!
//! 定位链：SysY 源码 → RaanaIR（平台无关 SSA，`raana_ir` crate）→ **lower**
//! （指令选择，`taki_mir/src/lower.rs`）→ VCode（机器指令级）→ 汇编。本模块
//! 提供对 **RaanaIR 指令**（`HirInst`，即 `raana_ir` 的 `Inst` 句柄）的
//! 一组分类谓词：判断一条指令是否带副作用、是否终结基本块、是否转移控制流。
//!
//! ## 为什么需要
//!
//! 指令本体存放在每个函数的 arena 里（`ArenaContext` 实现了 `raana_ir` 的
//! `Arena` trait），拿到 `HirInst` 句柄后要先经 `inst_data(inst).kind()`
//! 解引用才能看到 `InstKind`。`raana_ir` 的 `InstKind` 上已有一组原子谓词
//! （`is_call`/`is_load`/`is_store`/`is_mem_zero`/`is_terminator`/`is_branch`，
//! 见 `raana_ir` 的 `ir/inst_kind.rs`），本模块把它们透传、组合成
//! `ArenaContext` 的方法。这样调用方（lower、块布局）不必各自手动解引用
//! arena，也不用在多处重复写相同的 `matches!` 分支——「同一判断散落多处、
//! 改一处漏一处」是这类谓词最容易踩的坑。
//!
//! ## 提供的谓词
//!
//! | 方法 | 判定的指令 | 语义 |
//! |------|-----------|------|
//! | `has_side_effect_when_lowering` | `Call`/`Load`/`Store`/`MemZero`/`Return`/`TailCall` | lowering 视角下「有副作用」：涉及内存读写或控制流转移 |
//! | `is_terminator` | `Jump`/`Branch`/`Return`/`TailCall` | 结束当前基本块的指令（terminator） |
//! | `is_branch` | `Jump`/`Branch` | 转移到其他基本块的指令（有 CFG 后继） |
//!
//! 注意 `is_terminator` 与 `is_branch` 的差别：`Return`/`TailCall` 结束基本块
//! 但没有 CFG 后继，因此是 terminator 而不是 branch，lower 对二者的处理
//! 方式也不同（见下）。前两者只是 `InstKind` 原子谓词的薄包装；
//! `has_side_effect_when_lowering` 是唯一的**复合**判断：除了
//! `is_call`/`is_load`/`is_store`/`is_mem_zero`，还把 `Return`/`TailCall`
//! 也算作有副作用。
//!
//! ## 副作用判断为什么这么定义
//!
//! 纯函数调用（返回值未使用、无内存副作用）在 **HLIR 阶段已经做过 DCE**：
//! 在那里，「返回值未使用的纯函数调用」会被当作无副作用直接删除。因此进入
//! lowering 后剩下的 `Call` 一律保守地视为有副作用，不再尝试重新判断纯度——
//! 纯度分析是 HLIR 的职责，lower 只需要知道「这条指令不能乱动」。
//!
//! ## 谁在用
//!
//! - `lower.rs`（指令选择）：预处理时按副作用给指令划分颜色区间
//!   （`inst_color`，副作用指令开启新颜色），保证「块内指令逆序 lower、
//!   常量链在块首物化」这类重排不越过副作用边界；逆序扫描基本块指令时用
//!   `is_branch` 跳过分支（分支在此之前已单独选择，`Return` 无 CFG 后继，
//!   作为普通根指令 lowering）；下沉候选判定用 `is_terminator`/
//!   `has_side_effect_when_lowering` 排除终结指令与带副作用的 producer
//!   （它们不可下沉）。
//! - `block_order.rs`（CFG 布局）：用 `is_branch` 判断原基本块的最后一条指令
//!   是否为分支，决定该块按 `LoweredBlock::Orig` 还是 `Edge` 处理。
//!
//! 寄存器分配（`reg_alloc`）**不**依赖本模块：它工作在 VCode 机器指令层
//! （`MachInst`），对分支的判断走 `Function` trait 的 `is_branch`（`vcode`
//! 的 `inst_is_branch` 表），与本模块的 HIR 层面判断互不重叠。

use crate::prelude::*;

impl ArenaContext<'_> {
    /// INFO: Call is consider to having side effect because do DCE in HLIR already.
    /// In HLIR's DCE, pure function with unused return value can be treated
    /// as having no side effect.
    pub fn has_side_effect_when_lowering(&self, inst: HirInst) -> bool {
        let inst = self.inst_data(inst).kind();
        inst.is_call()
            || inst.is_load()
            || inst.is_store()
            || inst.is_mem_zero()
            || matches!(inst, InstKind::Return(..) | InstKind::TailCall(..))
    }

    pub fn is_terminator(&self, inst: HirInst) -> bool {
        let inst = self.inst_data(inst).kind();
        inst.is_terminator()
    }

    pub fn is_branch(&self, inst: HirInst) -> bool {
        let inst = self.inst_data(inst).kind();
        inst.is_branch()
    }
}
