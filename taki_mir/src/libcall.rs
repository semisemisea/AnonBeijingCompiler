//! # LibCall：运行时库调用（libcall）集合
//!
//! 定位链：SysY 源码 → RaanaIR（平台无关 SSA，`raana_ir` crate）→ VCode
//! （机器指令级）→ 汇编。RaanaIR 的部分指令（如内存清零 `MemZero`/
//! `MemZeroLen`）在后端 lower 阶段除了展开成指令序列，也可以展开成**对
//! 运行时库函数的调用**（libcall）；本模块就是这类调用的枚举清单。
//!
//! `LibCall` 每个变体对应一个外部符号：`symbol()` 返回该调用在汇编里
//! 使用的符号名（链接器可见）。目前只有 `Memset` 一个变体，符号名为
//! `memset`。类型派生 `Copy`，因此可以直接嵌进标签/指令结构里携带。
//!
//! ## 谁在使用
//!
//! - RISC-V 后端（`uika_riscv`）：lower 阶段遇到大尺寸或长度运行时才
//!   确定的清零指令时，发出 `MInst::Call`，按 ABI 把 `a0/a1/a2` 分别设为
//!   目标地址、填充值 0、字节数，调用目标记作
//!   `Label::LibCall(LibCall::Memset)`；`labels.rs` 的 `emit()` 调用
//!   `symbol()` 把它写成汇编里的 `call memset`。
//! - AArch64 后端（`anon_armv8`）**不**经过本模块：它用自己的
//!   `EmbeddedSymbol::Memset`（内嵌汇编 `memzero` 助手），因此新增 libcall
//!   时只需考虑 RISC-V 侧。
//!
//! ## 为什么作为 libcall 而不是内联
//!
//! 长度在编译期已知且很小的清零会直接内联成几条 store（`sw`）展开，不进
//! 函数调用；但当长度很大、或运行时才确定时，内联成固定序列要么导致代码
//! 爆炸，要么需要自己生成循环，都不划算。此时调用 C 运行库自带的
//! `memset`：实现经过手工优化（按字/向量宽度批量写），生成的代码量也小，
//! 代价只是一次带 ABI 参数传递的函数调用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibCall {
    Memset,
}

impl LibCall {
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::Memset => "memset",
        }
    }
}
