//! # IR 定义：Program / Function / BasicBlock / Inst
//!
//! SSA 形式的高层中间表示。所有对象都存 arena（[`arena`]），句柄是**下标
//! 索引**：`Inst`、`Function`、`BasicBlock` 都是 newtype，通过
//! [`Arena`](arena::Arena) trait 查询数据（`inst_data`/`func_data`/…）。
//!
//! ## 层次
//!
//! ```text
//! Program（全局 arena：全局量 + 所有函数）
//!   └─ Function / FunctionData（局部 arena：块 + 指令）
//!        └─ BasicBlock（块参数 params + 指令序列 + terminator）
//!             └─ Inst / InstData（指令，kind 见 [inst_kind]）
//! ```
//!
//! - [`Program`]：整个编译单元（`program.rs`）。
//! - [`Function`]/[`FunctionData`]：函数与函数体（`function.rs`）。
//! - [`BasicBlock`]：SSA 基本块；**Phi 即块参数**，跳转时随参数传值
//!   （见 [basic_block]）。
//! - [`Inst`]/[`InstData`]：指令，语义由 [`InstKind`] 决定（[`inst_kind`]：
//!   Binary/Load/Store/Call/Select/Cast/Vector* 等）。
//! - [`types`]：类型（`Type`/`TypeKind`：标量 + 向量 V4I32/V4F32/V2F64 等）。
//!
//! ## 构建
//!
//! 不要手工构造数据，用 [`builder`] 的 builder 接口（`GlobalBuilder`/
//! `LocalBuilder`/`BasicBlockBuilder`，经 [`builder_trait`] 统一暴露），
//! 保证 arena 一致性。
//!
//! ## 修改注意
//!
//! - 结构边 vs 逻辑边、CFG/支配/循环分析是**快照**，改 IR 后必须重建
//!   （`docs/Convention.md`）；
//! - block 参数的位置必须匹配目标块 `params()` 切片（[`BlockArgRef`]），
//!   不是随便一个 index。
//!
//! ## 新增指令/类型的检查清单（防漏改）
//!
//! 加一个 `InstKind` 变体或 `TypeKind` 成员，以下位置**全部**要同步：
//!
//! 1. `inst_kind.rs`（或对应子模块）加变体；
//! 2. `instruction.rs`/`InstData`（若需要新字段）；
//! 3. `builder.rs` 加构造方法（builder 是唯一写入口）；
//! 4. `fmt/`（IR dump 打印，`--emit ir` 会崩）；
//! 5. `llvm/`（LLVM IR 导出，漏了 `make test-llvm` 会 CE）；
//! 6. 后端 lower：`anon_armv8/src/lower.rs` 与 `uika_riscv/src/lower.rs`
//!    （漏了会 unreachable panic）。

pub mod arena;
pub mod basic_block;
pub(crate) mod builder;
pub(crate) mod function;
pub mod inst_kind;
pub(crate) mod instruction;
pub mod layout;
pub(crate) mod program;
pub(crate) mod remap;
pub(crate) mod types;

pub mod builder_trait {
    pub use super::builder::{
        BasicBlockBuilder, BasicBlockBuilders, GlobalBuilder, GlobalInstBuilder, InfoQuery,
        InstInsert, LocalBuilder, LocalInstBuilder, ScalarInstBuilder,
    };
}

pub use basic_block::BasicBlock;
pub use builder::{BasicBlockBuilders, GlobalBuilder, LocalBuilder};
pub use function::{Function, FunctionData};
pub use inst_kind::{
    Aggregate, Binary, BinaryOp, BlockArgRef, Branch, Call, Cast, Fma, GetElemPtr, InstKind,
    Integer, Jump, Load, Return, Select, Store, TailCall, VectorExtractElement,
    VectorInsertElement, VectorReduce, VectorReduceOp, VectorSplat,
};
pub use instruction::{Inst, InstData};
pub use program::Program;
pub use types::{Type, TypeKind};
