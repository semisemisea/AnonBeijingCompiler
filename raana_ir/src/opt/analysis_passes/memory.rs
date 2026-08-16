//! Base-object (GetBaseObject) and intra-procedural alias analysis for SysY IR.
//!
//! SysY has no pointer type, no address-of operator, no casts and no heap
//! allocation. Every pointer value therefore ultimately derives from one of
//! three base objects:
//!
//! - a function-local stack object (`InstKind::Alloc`),
//! - a global object (`InstKind::GlobalAlloc`),
//! - an array parameter (a pointer-typed entry-block parameter).
//!
//! The only indirection in the IR is the frontend's *pointer slot* pattern
//! (`alloc <**T>; store %param, %slot; ... load %slot`), which `BaseEnv`
//! resolves through a store-once slot map. Block parameters (phi values)
//! that carry pointers are resolved through their incoming edge arguments.
//!
//! The static alias rules follow the SysY memory model (see
//! `docs/memory_alias_analysis.md` §3.2): different local objects never
//! alias each other, locals never alias globals, and a local never aliases
//! an argument of the same function. Argument-vs-global and
//! argument-vs-argument pairs are conservatively `MayAlias` here; the
//! interprocedural points-to refinement lives in `analysis_passes/effects.rs`.
//!
//! ---
//!
//! ## 补充说明（中文）
//!
//! 一句话定位：这是 raana_ir 的**过程内基对象（base-object）别名分析**。SysY 没有指针
//! 类型、没有取地址、没有强制转换、没有堆分配，所以每条「地址形态」的指令
//! （`Alloc`、`GlobalAlloc`、`GetElemPtr`、`Load`、`BlockArgRef`）最终都源自三种基对象
//! 之一；本模块把每条地址指令归约成「基对象 + 常量字节偏移」，再用「基对象不相交 +
//! 偏移区间不重叠」两条规则回答「两条内存访问是否可能重叠」。它是别名信息的过程内
//! 地基：`effects.rs` 在其上叠加过程间 points-to 精化，`dse.rs` / `ipsccp.rs` 直接复用
//! 它的地址解析做存储建模。文中术语（固定点、pass、SSA、块参数/phi 等）见
//! `docs/offline-handbook/glossary.md`。
//!
//! ## 数据结构
//!
//! - `MemObject`：基对象枚举，四种变体：
//!   - `Alloc(Inst)`：函数局部栈对象，由某条 `Alloc` 指令标识；
//!   - `Global(Inst)`：全局对象，由某条 `GlobalAlloc` 指令标识；
//!   - `Param(usize)`：本函数入口块第 `usize` 个参数（SysY 的数组参数 = 指针）；
//!   - `Unknown`：来源未知（可能是任何东西）。`is_unknown()` 判断是否为它。
//! - `AliasResult`：别名判定结果，三值：`NoAlias`（绝无重叠字节）/ `MayAlias`
//!   （可能重叠，保守）/ `MustAlias`（证明访问同一字节）。`may_alias()` = 结果不是
//!   `NoAlias`，即「必须当作冲突处理」。
//! - `BaseEnv`：**每函数一份**的过程内基址环境，内部五张表：
//!   - `param_position`：块参数 → `(所属块, 位置下标)`，供经入边实参解析块参数；
//!   - `slots`：指针槽表——「恰好写一次」的局部 `Alloc` → 写入的值（前端
//!     `alloc <**T>; store %param, %slot` 数组实参下沉模式）；
//!   - `entry_params`：函数入口参数列表（`FunctionData::params()`）；
//!   - `base_table`：每条地址形态指令的基对象（预计算）；
//!   - `offset_table`：每条地址形态指令的常量字节偏移，`None` 条目 = 偏移不能
//!     静态确定（预计算）。
//!
//! ## API 详解
//!
//! 构造与预计算：
//!
//! - `BaseEnv::new(data: &FunctionData) -> BaseEnv`：只读扫描一个函数的数据：登记所有
//!   块参数的位置，并识别 store-once 指针槽。只需要 `FunctionData`、不需要 arena 就能
//!   看到全局指令。
//! - `BaseEnv::build_tables(&mut self, arena, func)`：预计算 `base_table` 与
//!   `offset_table` 的固定点 pass，**必须在 `new` 之后调用一次**（传入能触达全局指令
//!   的 arena，如 `ArenaContext`）；之后所有查询都是 O(1) 查表，不再有逐查询递归。
//!
//! 查询（全部 `&self` 只读）：
//!
//! - `base_of(&self, arena, ptr) -> MemObject`：`ptr` 的基对象。O(1) 查表；
//!   `GlobalAlloc` 直接返回 `Global(ptr)`（绕过表）；表里没有的指令（或解析为未知）
//!   保守返回 `Unknown`。
//! - `constant_offset(&self, arena, ptr) -> Option<i64>`：`ptr` 相对其基对象的字节
//!   偏移；裸基对象（`Alloc` / `GlobalAlloc` / 入口参数）为 0；GEP 链上任一维下标不是
//!   编译期常量则返回 `None`。`GlobalAlloc` 直接返回 `Some(0)`。
//! - `alias(&self, arena, a, b) -> AliasResult`：两条地址 `a`、`b` 的过程内别名判定。
//!   先判 `a == b`（同一条指令 → `MustAlias`），再取双方基对象走 `alias_with_bases`。
//! - `alias_with_bases(&self, arena, a, b, base_a, base_b) -> AliasResult`：带预计算
//!   基对象的判定，供 `effects.rs` 复用（它先用 points-to 精化「参数 vs 参数 /
//!   参数 vs 全局」组合，剩余组合回退到这里）。判定矩阵见「正确性 / 边界」。
//!
//! 内部实现（私有，仅供理解）：
//!
//! - `compute_base(&self, arena, ptr, work) -> Option<MemObject>`：基对象固定点的单步
//!   求值，规则见「算法」。`work` 是「指令 → 当前基对象」的工作表。
//! - `compute_offset(&self, arena, ptr, work) -> Option<Option<i64>>`：偏移固定点的
//!   单步。外层 `None` = 依赖未就绪（挂起）；`Some(None)` = 已定为非常量；
//!   `Some(Some(off))` = 常量偏移。
//! - `offset_alias(&self, arena, a, b) -> AliasResult`：同基对象时的偏移区间精化。
//!
//! 模块级辅助函数：
//!
//! - `access_size(arena, addr) -> i64`：经 `addr` 一次访问的字节数（其指针类型的
//!   pointee 大小）。目前只在 `offset_alias` 内部使用（`pub` 但全仓库无外部调用）。
//! - `integer_constant(arena, inst) -> Option<i32>`：`inst` 是 `Integer` 常量时返回
//!   其值。注意：`sr.rs` / `column_major.rs` / `boolean_simplify.rs` /
//!   `induction_variable.rs` 等文件里也有同名 `integer_constant`，那是**各自的局部
//!   副本**，与本模块导出的这个无关（见「使用方清单」的反例）。
//!
//! ## 算法
//!
//! ### 基对象解析（`compute_base` 单步）
//!
//! 三种基对象的来源规则：
//!
//! 1. `Alloc` → `Alloc(ptr)`；`GlobalAlloc` → `Global(ptr)`——这两种是基对象的
//!    「源头」，直接定案；
//! 2. `GetElemPtr(gep)` → 沿 `gep.base()` 追到基对象（base 是全局时短路为
//!    `Global(base)`）；
//! 3. 其余指令一律 `Unknown`，除两种特殊形态：
//!    - `Load`：源地址在指针槽表 `slots` 中 → 取槽内存入值的基对象；否则 `Unknown`
//!      （从任意地址读出的指针来源不可知，不能瞎猜）；
//!    - `BlockArgRef`：若是入口参数 → `Param(下标)`；否则查 `param_position` 定位
//!      所属块与位置，遍历该块 `used_by` 中每条 `Jump` / `Branch` 入边，取对应位置
//!      的实参并求其基对象——所有入边**一致**才定案，任一条未解析、为 `Unknown` 或
//!      彼此冲突 → `Unknown`。
//!
//! ### 指针槽模式（store-once slot）
//!
//! `BaseEnv::new` 扫描全函数指令：只把「目标是函数局部 `Alloc`」的 `Store` 当作候选；
//! 同一槽被写**两次**或曾被 `MemZero` 触碰 → 移出 `slots` 并标记 ambiguous（不可
//! 解析）；全局目标直接跳过（`FunctionData` 看不到全局指令，且只有局部 `Alloc` 才能
//! 当指针槽）。`Load` 只有从这类「恰好写一次」的槽读出时才能解析回写入值，否则
//! 保守 `Unknown`。
//!
//! ### 块参数经入边实参解析
//!
//! RaanaIR 的 phi 就是块参数（`BlockArgRef`），其值由每条入边实参提供。解析一个块
//! 参数：在 `used_by` 里找所有跳到该块的 `Jump` / `Branch`，按下标取实参，再求每个
//! 实参的基对象；全部入边一致才用，否则 `Unknown`。这正是循环回边（phi 自引用）能
//! 收敛的关键——见下。
//!
//! ### 固定点收敛
//!
//! `build_tables` 对 base 与 offset 各跑一轮不动点：反复对每条地址指令求
//! `compute_base` / `compute_offset`，直到整表不再变化。工作表用「`None` = 未解析
//! （可继续精化）」与「`Some(Unknown)` = 已定未知（不再变化）」区分，因此循环 phi
//! 链不会震荡，而是收敛到保守的 `Unknown`。偏移表同理，`Some(None)` = 已定为
//! 非常量。GEP 偏移 = 基偏移 + Σ(常量下标 × `gep_index_stride` 给出的字节步长)，
//! 任一维下标非常量（经 `integer_constant` 提取）或加法溢出即整体非常量。
//!
//! ## 使用方清单
//!
//! - `analysis_passes/effects.rs`（过程间层，**核心使用方**；import
//!   `{AliasResult, BaseEnv, MemObject}`）：`EffectAnalysis::new` 为每个有 body 的函数
//!   `BaseEnv::new` + `build_tables`，存入 `envs` 表，经 `env_of(func)` 对外暴露；用
//!   `base_of` 把地址映射成抽象对象集合（`targets_of`，约 252 行）；`alias` 用
//!   points-to 集合精化「参数 vs 参数 / 参数 vs 全局」组合，其余回退
//!   `alias_with_bases`（约 463 行）——本模块是它的过程内地基，它是本模块的
//!   interprocedural 细化。
//! - `opt/passes/dse.rs`（死存储消除；import `{BaseEnv, MemObject}`）：经
//!   `analysis.env_of(func)` 取 `BaseEnv`，在 `resolve_cell`（约 148 行）里用
//!   `constant_offset` + `base_of` 把地址归约为 `Cell = (MemObject, i64)`；只认
//!   `Alloc` / `Global` 有单元语义，参数指针与未知基址返回 `None`（调用方按「可能
//!   别名一切」保守处理）。
//! - `opt/passes/ipsccp.rs`（过程间稀疏条件常量传播；import `{BaseEnv, MemObject}`）：
//!   同样经 `analysis.env_of(func)` 取 `BaseEnv`，在 `resolve_cell`（约 449 行）里把
//!   `Load` / `Store` / `MemZero` 的地址解析成 `(CellKey, RootKey)` 做内存单元建模。
//! - `opt/passes/licm.rs`（循环不变代码外提）：**不直接 import 本模块**，经
//!   `EffectAnalysis::alias`（effects.rs 的过程间别名）判断循环内 store / MemZero
//!   是否可能与外提的 load 冲突（licm.rs:759 起）——间接使用。
//!
//! 反例（grep 实证，避免误认）：`opt/utils/gep.rs` 里的局部变量 `constant_offset`、
//! `pointer_strength_reduction_cost.rs` / `sr.rs` / `column_major.rs` /
//! `boolean_simplify.rs` / `induction_variable.rs` / `pointer_strength_reduction/` 里的
//! `integer_constant` 都是各文件**自己的局部定义**，与本模块无关；全仓库 import
//! `analysis_passes::memory::...` 的只有 effects.rs / dse.rs / ipsccp.rs 三个文件。
//!
//! ## 正确性 / 边界
//!
//! 别名判定矩阵（`alias_with_bases`，对应 `docs/memory_alias_analysis.md` §3.2）：
//!
//! | 基对象组合 | 结果 | 理由 |
//! | --- | --- | --- |
//! | 任一方 `Unknown` | `MayAlias` | 来源未知，必须保守 |
//! | `Alloc(x)` vs `Alloc(y)`（x≠y） | `NoAlias` | 不同局部对象永不重叠 |
//! | `Global(x)` vs `Global(y)`（x≠y） | `NoAlias` | 不同全局对象永不重叠 |
//! | `Alloc` vs `Global` | `NoAlias` | 局部永不别名全局 |
//! | `Alloc` vs `Param` | `NoAlias` | 局部永不别名本函数实参（实参在别处） |
//! | `Param` vs `Param` / `Param` vs `Global` | `MayAlias` | 无 points-to 信息，保守；interprocedural 细化在 `effects.rs` |
//! | 同一基对象 | `offset_alias` 精化 | 见下 |
//!
//! 同基精化（`offset_alias`）：两侧偏移都取到常量后，用 `access_size` 得到访问区间；
//! 区间不相交（`off_a + size_a <= off_b` 或反之）→ `NoAlias`；起点相同 → `MustAlias`；
//! 其余 → `MayAlias`。任何一侧偏移非常量（如 GEP 带变量下标）→ 直接 `MayAlias`。
//!
//! 其它边界：
//!
//! - `a == b`（同一条指令）→ `MustAlias`，不经过基对象表；
//! - **快照性**：`BaseEnv` 与调用图分析一样是快照，任何 IR 修改（新增 store 改写槽、
//!   增删 GEP、改块参数）都会使其过期。实践中它随 `EffectAnalysis` 每次 `Pass::run`
//!   重建，pass 内不可跨修改复用 env；
//! - 指针槽只在「恰好写一次且未被 `MemZero` 触碰」时成立，条件不满足就退回
//!   `Unknown`——宁可保守也不给错答案；
//! - 块参数要求所有入边一致，这是循环收敛与正确性的前提；
//! - 与 `effects.rs` 的分工：本模块是**过程内**规则，参数相关组合一律保守
//!   `MayAlias`；`effects.rs` 用调用点的 points-to 集合把 `Param(i)` 展开成具体对象
//!   集合，两集合不相交即可判 `NoAlias`——这是 §3.2 规则的 interprocedural 细化，
//!   不是本模块的替代。
//!
//! ## 验证
//!
//! 本文件 `mod tests`（约 492-800 行）共 10 个单元测试，按主题分四组：
//!
//! - 基对象解析：`base_of_alloc_and_global`、`base_of_gep_walks_to_base`、
//!   `base_of_resolves_frontend_pointer_slot`、`base_of_load_of_non_slot_is_unknown`；
//! - 常量偏移：`constant_offset_of_flat_and_multi_dim_geps`、
//!   `constant_offset_rejects_variable_index`；
//! - 别名矩阵：`intra_alias_rules_matrix`、`same_base_constant_offsets_disentangle`；
//! - 块参数：`block_param_resolves_through_uniform_incoming_edges`、
//!   `conflicting_block_param_edges_yield_unknown`。
//!
//! 另由 `effects.rs` 的测试间接覆盖（其 `alias` 用例逐项断言 interprocedural 精化，
//! 如 931 / 976 / 1075 行），以及 `cargo test -p raana_ir` 全量回归。

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    ir::{BasicBlock, Function, FunctionData, Inst, InstKind, arena::Arena},
    opt::utils::gep::gep_index_stride,
};
/// The base object a pointer value ultimately derives from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemObject {
    /// A function-local stack object, identified by its `Alloc` instruction.
    Alloc(Inst),
    /// A global object, identified by its `GlobalAlloc` instruction.
    Global(Inst),
    /// The function's array parameter at the given index.
    Param(usize),
    /// Anything else (unknown provenance).
    Unknown,
}

impl MemObject {
    pub fn is_unknown(&self) -> bool {
        matches!(self, MemObject::Unknown)
    }
}

/// Alias classification between two memory accesses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasResult {
    /// The two accesses can never touch overlapping bytes.
    NoAlias,
    /// The two accesses may touch overlapping bytes (conservative).
    MayAlias,
    /// The two accesses provably touch the same bytes.
    MustAlias,
}

impl AliasResult {
    /// True when the pair must be treated as conflicting.
    pub fn may_alias(self) -> bool {
        self != AliasResult::NoAlias
    }
}

/// Per-function environment for base-object tracing.
///
/// Built once per function; all queries are read-only afterwards. The
/// base/offset tables are precomputed by [`BaseEnv::build_tables`] with a
/// fixed-point pass over the function's instructions and block parameters
/// (call it once after construction, with an arena that can reach global
/// instructions). Queries are then O(1) table lookups — no per-query
/// recursion.
pub struct BaseEnv {
    /// Position of each block parameter within its owning block, used to
    /// resolve pointer-valued block parameters through incoming edges.
    param_position: FxHashMap<Inst, (BasicBlock, usize)>,
    /// Pointer slots: an `Alloc` written exactly once (the frontend's
    /// `alloc <**T>; store %param, %slot` pattern) -> the stored value.
    slots: FxHashMap<Inst, Inst>,
    /// The function's entry parameters (`FunctionData::params()`).
    entry_params: Vec<Inst>,
    /// Precomputed base object of every address-shaped instruction.
    base_table: FxHashMap<Inst, MemObject>,
    /// Precomputed byte offset of every address-shaped instruction
    /// (`None` entry = offset not statically known).
    offset_table: FxHashMap<Inst, Option<i64>>,
}

impl BaseEnv {
    pub fn new(data: &FunctionData) -> BaseEnv {
        let mut param_position = FxHashMap::default();
        for bb_layout in data.layout().basicblocks() {
            let bb = bb_layout.bb();
            for (index, &param) in data.bb_data(bb).params().iter().enumerate() {
                param_position.insert(param, (bb, index));
            }
        }

        // Store-once pointer slots. A slot written more than once, or
        // touched by MemZero, is not resolvable. Global destinations are
        // skipped: only function-local Allocs can be pointer slots, and
        // `FunctionData` cannot inspect global instructions.
        let mut stored: FxHashMap<Inst, Inst> = FxHashMap::default();
        let mut ambiguous: FxHashSet<Inst> = FxHashSet::default();
        for bb_layout in data.layout().basicblocks() {
            for &inst in bb_layout.insts() {
                match data.inst_data(inst).kind() {
                    InstKind::Store(store) => {
                        let dest = store.dest();
                        if !dest.is_global()
                            && matches!(data.inst_data(dest).kind(), InstKind::Alloc)
                        {
                            if ambiguous.contains(&dest) {
                                continue;
                            }
                            if stored.insert(dest, store.src()).is_some() {
                                stored.remove(&dest);
                                ambiguous.insert(dest);
                            }
                        }
                    }
                    InstKind::MemZero(mem_zero) => {
                        let dest = mem_zero.dest();
                        if !dest.is_global()
                            && matches!(data.inst_data(dest).kind(), InstKind::Alloc)
                        {
                            stored.remove(&dest);
                            ambiguous.insert(dest);
                        }
                    }
                    _ => {}
                }
            }
        }

        BaseEnv {
            param_position,
            slots: stored,
            entry_params: data.params().to_vec(),
            base_table: FxHashMap::default(),
            offset_table: FxHashMap::default(),
        }
    }

    /// The base object `ptr` ultimately derives from.
    ///
    /// O(1) lookup into the table precomputed by [`Self::build_tables`];
    /// instructions missing from the table (or global objects, which are
    /// resolved directly) conservatively yield `Unknown`.
    pub fn base_of<A: Arena + ?Sized>(&self, arena: &A, ptr: Inst) -> MemObject {
        if matches!(arena.inst_data(ptr).kind(), InstKind::GlobalAlloc(..)) {
            return MemObject::Global(ptr);
        }
        self.base_table
            .get(&ptr)
            .copied()
            .unwrap_or(MemObject::Unknown)
    }

    /// Precompute the base and offset tables with a fixed-point pass.
    ///
    /// Must be called once after construction (with an arena that can reach
    /// global instructions, e.g. `ArenaContext`); afterwards all queries
    /// are O(1) lookups. The work table distinguishes "not yet resolved"
    /// (`None`) from "resolved to unknown" (`Some(Unknown)`), so cyclic
    /// phi chains (loop backedges) converge conservatively to `Unknown`
    /// instead of oscillating.
    pub fn build_tables<A: Arena + ?Sized>(&mut self, arena: &A, func: Function) {
        let data = arena.func_data(func);
        let mut insts: Vec<Inst> = Vec::new();
        for bb_layout in data.layout().basicblocks() {
            insts.extend(bb_layout.insts().iter().copied());
            insts.extend(arena.bb_data(bb_layout.bb()).params().iter().copied());
        }

        // ---- Base fixed point. ----
        let mut base_work: FxHashMap<Inst, Option<MemObject>> =
            insts.iter().map(|&i| (i, None)).collect();
        loop {
            let mut changed = false;
            for &ptr in &insts {
                let next = self.compute_base(arena, ptr, &base_work);
                let cur = base_work.get_mut(&ptr).unwrap();
                if *cur != next {
                    *cur = next;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        self.base_table = base_work
            .into_iter()
            .map(|(k, v)| (k, v.unwrap_or(MemObject::Unknown)))
            .collect();

        // ---- Offset fixed point. ----
        let mut off_work: FxHashMap<Inst, Option<Option<i64>>> =
            insts.iter().map(|&i| (i, None)).collect();
        loop {
            let mut changed = false;
            for &ptr in &insts {
                let next = self.compute_offset(arena, ptr, &off_work);
                let cur = off_work.get_mut(&ptr).unwrap();
                if *cur != next {
                    *cur = next;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        self.offset_table = off_work
            .into_iter()
            .map(|(k, v)| (k, v.flatten()))
            .collect();
    }

    /// Fixed-point step for the base of `ptr`.
    ///
    /// `work` maps every address-shaped instruction to its current base
    /// (`None` = not resolved yet). Dependencies that are not yet resolved
    /// keep the current entry pending; `Some(Unknown)` is a settled
    /// unknown (no further refinement).
    fn compute_base<A: Arena + ?Sized>(
        &self,
        arena: &A,
        ptr: Inst,
        work: &FxHashMap<Inst, Option<MemObject>>,
    ) -> Option<MemObject> {
        match arena.inst_data(ptr).kind() {
            InstKind::Alloc => Some(MemObject::Alloc(ptr)),
            InstKind::GlobalAlloc(..) => Some(MemObject::Global(ptr)),
            InstKind::GetElemPtr(gep) => {
                if matches!(
                    arena.inst_data(gep.base()).kind(),
                    InstKind::GlobalAlloc(..)
                ) {
                    return Some(MemObject::Global(gep.base()));
                }
                work.get(&gep.base()).copied().flatten()
            }
            InstKind::Load(load) => match self.slots.get(&load.src()) {
                Some(&stored) => {
                    if matches!(arena.inst_data(stored).kind(), InstKind::GlobalAlloc(..)) {
                        Some(MemObject::Global(stored))
                    } else {
                        work.get(&stored).copied().flatten()
                    }
                }
                None => Some(MemObject::Unknown),
            },
            InstKind::BlockArgRef(..) => {
                if let Some(index) = self.entry_params.iter().position(|&p| p == ptr) {
                    return Some(MemObject::Param(index));
                }
                let Some(&(block, index)) = self.param_position.get(&ptr) else {
                    return Some(MemObject::Unknown);
                };
                let mut result: Option<MemObject> = None;
                let mut all_set = true;
                let mut saw_independent_edge = false;
                for &user in arena.bb_data(block).used_by() {
                    let arg = match arena.inst_data(user).kind() {
                        InstKind::Jump(jump) if jump.target() == block => {
                            jump.args().get(index).copied()
                        }
                        InstKind::Branch(branch) => {
                            if branch.t_target() == block {
                                branch.t_args().get(index).copied()
                            } else if branch.f_target() == block {
                                branch.f_args().get(index).copied()
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                    let Some(arg) = arg else {
                        return Some(MemObject::Unknown);
                    };
                    // A back edge that forwards the parameter itself, or a
                    // GEP of the parameter itself (a loop-carried pointer
                    // bump), carries the *same* base as this parameter — it
                    // must not introduce a self-dependency that otherwise
                    // stalls the fixed point. Skip it: the base is decided
                    // by the independent incoming edges.
                    if arg == ptr
                        || matches!(
                            arena.inst_data(arg).kind(),
                            InstKind::GetElemPtr(gep) if gep.base() == ptr
                        )
                    {
                        continue;
                    }
                    saw_independent_edge = true;
                    match work.get(&arg) {
                        None => all_set = false,
                        Some(Some(MemObject::Unknown)) => return Some(MemObject::Unknown),
                        Some(Some(m)) => match result {
                            Some(prev) if prev != *m => return Some(MemObject::Unknown),
                            _ => result = Some(*m),
                        },
                        Some(None) => all_set = false,
                    }
                }
                if all_set {
                    result.or(Some(MemObject::Unknown))
                } else if !saw_independent_edge {
                    // Only self-referential back edges (no independent
                    // incoming value): the base is genuinely unprovable.
                    Some(MemObject::Unknown)
                } else {
                    None
                }
            }
            _ => Some(MemObject::Unknown),
        }
    }

    /// Fixed-point step for the byte offset of `ptr`.
    ///
    /// The work value is `Option<Option<i64>>`: outer `None` = pending,
    /// `Some(None)` = settled non-constant, `Some(Some(off))` = constant.
    fn compute_offset<A: Arena + ?Sized>(
        &self,
        arena: &A,
        ptr: Inst,
        work: &FxHashMap<Inst, Option<Option<i64>>>,
    ) -> Option<Option<i64>> {
        match arena.inst_data(ptr).kind() {
            InstKind::Alloc | InstKind::GlobalAlloc(..) => Some(Some(0)),
            InstKind::BlockArgRef(..) => {
                // ABI parameters (the function's entry block) are bare
                // pointers: offset zero.
                if self.entry_params.iter().any(|&p| p == ptr) {
                    return Some(Some(0));
                }
                let Some(&(block, index)) = self.param_position.get(&ptr) else {
                    return Some(None);
                };
                let mut result: Option<i64> = None;
                let mut all_set = true;
                for &user in arena.bb_data(block).used_by() {
                    let arg = match arena.inst_data(user).kind() {
                        InstKind::Jump(jump) if jump.target() == block => {
                            jump.args().get(index).copied()
                        }
                        InstKind::Branch(branch) => {
                            if branch.t_target() == block {
                                branch.t_args().get(index).copied()
                            } else if branch.f_target() == block {
                                branch.f_args().get(index).copied()
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                    let Some(arg) = arg else {
                        return Some(None);
                    };
                    match work.get(&arg) {
                        None => all_set = false,
                        Some(Some(None)) => return Some(None),
                        Some(Some(Some(off))) => {
                            if result.is_none() {
                                result = Some(*off);
                            } else if result != Some(*off) {
                                return Some(None);
                            }
                        }
                        Some(None) => all_set = false,
                    }
                }
                if all_set { Some(result) } else { None }
            }
            InstKind::GetElemPtr(gep) => {
                let base_off = if matches!(
                    arena.inst_data(gep.base()).kind(),
                    InstKind::GlobalAlloc(..)
                ) {
                    Some(Some(0))
                } else {
                    work.get(&gep.base()).copied().flatten()
                };
                let mut off = match base_off {
                    None => return None, // base pending
                    Some(None) => return Some(None),
                    Some(Some(o)) => o,
                };
                for (pos, &idx) in gep.offsets().iter().enumerate() {
                    let Some(idx_value) = integer_constant(arena, idx) else {
                        return Some(None);
                    };
                    let Some(stride) = gep_index_stride(arena, ptr, pos) else {
                        return Some(None);
                    };
                    let Some(sum) = off.checked_add(idx_value as i64 * stride.byte_stride) else {
                        return Some(None);
                    };
                    off = sum;
                }
                Some(Some(off))
            }
            InstKind::Load(load) => match self.slots.get(&load.src()) {
                Some(&stored) => {
                    if matches!(arena.inst_data(stored).kind(), InstKind::GlobalAlloc(..)) {
                        Some(Some(0))
                    } else {
                        work.get(&stored).copied().flatten()
                    }
                }
                None => Some(None),
            },
            _ => Some(None),
        }
    }

    /// Byte offset of `ptr` relative to its base object, or `None` when any
    /// index along the GEP chain is not a compile-time constant.
    ///
    /// The base object itself (a bare Alloc/GlobalAlloc/parameter) has
    /// offset zero; block parameters resolve through their incoming edge
    /// arguments (precomputed by [`Self::build_tables`]).
    pub fn constant_offset<A: Arena + ?Sized>(&self, arena: &A, ptr: Inst) -> Option<i64> {
        if matches!(arena.inst_data(ptr).kind(), InstKind::GlobalAlloc(..)) {
            return Some(0);
        }
        self.offset_table.get(&ptr).copied().flatten()
    }

    /// Intra-procedural alias result between two addresses, refined by
    /// constant-offset interval disjointness on the same base.
    ///
    /// Argument-vs-argument and argument-vs-global pairs stay conservative
    /// `MayAlias`; the interprocedural refinement is `EffectAnalysis::alias`
    /// in `analysis_passes/effects.rs`.
    pub fn alias<A: Arena + ?Sized>(&self, arena: &A, a: Inst, b: Inst) -> AliasResult {
        if a == b {
            return AliasResult::MustAlias;
        }
        let base_a = self.base_of(arena, a);
        let base_b = self.base_of(arena, b);
        self.alias_with_bases(arena, a, b, base_a, base_b)
    }

    /// Same as [`Self::alias`] with precomputed bases.
    pub fn alias_with_bases<A: Arena + ?Sized>(
        &self,
        arena: &A,
        a: Inst,
        b: Inst,
        base_a: MemObject,
        base_b: MemObject,
    ) -> AliasResult {
        use MemObject::*;
        match (base_a, base_b) {
            (Unknown, _) | (_, Unknown) => AliasResult::MayAlias,
            (Alloc(x), Alloc(y)) if x != y => AliasResult::NoAlias,
            (Global(x), Global(y)) if x != y => AliasResult::NoAlias,
            (Alloc(..), Global(..)) | (Global(..), Alloc(..)) => AliasResult::NoAlias,
            (Alloc(..), Param(..)) | (Param(..), Alloc(..)) => AliasResult::NoAlias,
            // Argument pairs are conservatively MayAlias without points-to.
            (Param(..), Param(..)) | (Param(..), Global(..)) | (Global(..), Param(..)) => {
                AliasResult::MayAlias
            }
            // Same base: refine by constant offsets.
            _ => self.offset_alias(arena, a, b),
        }
    }

    /// Same-base refinement: constant offsets and access sizes decide
    /// NoAlias (disjoint byte intervals) or MustAlias (same start offset);
    /// anything else stays MayAlias.
    fn offset_alias<A: Arena + ?Sized>(&self, arena: &A, a: Inst, b: Inst) -> AliasResult {
        let (Some(off_a), Some(off_b)) = (
            self.constant_offset(arena, a),
            self.constant_offset(arena, b),
        ) else {
            return AliasResult::MayAlias;
        };
        let size_a = access_size(arena, a);
        let size_b = access_size(arena, b);
        if off_a + size_a <= off_b || off_b + size_b <= off_a {
            AliasResult::NoAlias
        } else if off_a == off_b {
            AliasResult::MustAlias
        } else {
            AliasResult::MayAlias
        }
    }
}

/// Size in bytes of the memory access through `addr` (the pointee size of
/// the address's pointer type).
pub fn access_size<A: Arena + ?Sized>(arena: &A, addr: Inst) -> i64 {
    arena.inst_data(addr).ty().derefernce().size() as i64
}

/// The value of `inst` when it is an `Integer` constant, else `None`.
pub fn integer_constant<A: Arena + ?Sized>(arena: &A, inst: Inst) -> Option<i32> {
    match arena.inst_data(inst).kind() {
        InstKind::Integer(value) => Some(value.value()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{
            Program, Type, arena::Arena, builder_trait::*, inst_kind::GetElemPtr,
        },
        opt::pass::ArenaContext,
    };

    fn env_of(program: &Program, func: crate::ir::Function) -> (BaseEnv, ArenaContext<'_>) {
        let data = program.func_data(func);
        let mut env = BaseEnv::new(data);
        let ctx = ArenaContext {
            program,
            curr_func: Some(func),
        };
        env.build_tables(&ctx, func);
        (env, ctx)
    }

    /// A fresh zero-initialized global of the given pointee type.
    fn new_global(program: &mut Program, pointee: Type) -> Inst {
        let init = program.new_value().zero_init(pointee);
        program.new_value().global_alloc(init)
    }

    #[test]
    fn base_of_alloc_and_global() {
        let mut program = Program::new();
        let global = new_global(&mut program, Type::get_i32());
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let alloc = data.new_local_inst().alloc(Type::get_i32());
        data.layout_mut().insert_inst(entry, alloc);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.base_of(&ctx, alloc), MemObject::Alloc(alloc));
        assert_eq!(env.base_of(&ctx, global), MemObject::Global(global));
    }

    #[test]
    fn base_of_gep_walks_to_base() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let param = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let gep = data.new_local_inst().get_elem_ptr(param, vec![zero]);
        data.layout_mut().insert_inst(entry, gep);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.base_of(&ctx, gep), MemObject::Param(0));
    }

    #[test]
    fn base_of_resolves_frontend_pointer_slot() {
        // `%slot = alloc <**T>; store %param, %slot; %p = load %slot`
        // must trace back to the parameter (the frontend array-argument
        // lowering pattern).
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let param = data.params()[0];
        let slot = data
            .new_local_inst()
            .alloc(Type::get_i32().reference().reference());
        data.layout_mut().insert_inst(entry, slot);
        let store = data.new_local_inst().store(param, slot);
        data.layout_mut().insert_inst(entry, store);
        let load = data.new_local_inst().load(slot);
        data.layout_mut().insert_inst(entry, load);
        let zero = data.new_local_inst().integer(0);
        let gep = data.new_local_inst().get_elem_ptr(load, vec![zero]);
        data.layout_mut().insert_inst(entry, gep);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.base_of(&ctx, load), MemObject::Param(0));
        assert_eq!(env.base_of(&ctx, gep), MemObject::Param(0));
    }

    #[test]
    fn base_of_load_of_non_slot_is_unknown() {
        // A pointer load whose source is not a store-once slot (here: a
        // value loaded through a parameter-derived address) must yield
        // Unknown instead of a wrong base object.
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let param = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let addr = data.new_local_inst().get_elem_ptr(param, vec![zero]);
        data.layout_mut().insert_inst(entry, addr);
        let loaded = data.new_local_inst().load(addr);
        data.layout_mut().insert_inst(entry, loaded);
        let slot = data
            .new_local_inst()
            .alloc(Type::get_i32().reference().reference());
        data.layout_mut().insert_inst(entry, slot);
        // The slot receives a value whose provenance is unknown; it must not
        // be treated as a resolvable pointer slot.
        let store = data.new_local_inst().store(loaded, slot);
        data.layout_mut().insert_inst(entry, store);
        // The pointer load from the slot: the slot was written by a
        // parameter-derived address, so it must resolve to Unknown, not Param.
        let reload = data.new_local_inst().load(slot);
        data.layout_mut().insert_inst(entry, reload);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.base_of(&ctx, reload), MemObject::Unknown);
    }

    #[test]
    fn constant_offset_of_flat_and_multi_dim_geps() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();

        // Local `[i32; 5][i32; 5]` array. Following the frontend's
        // pointer-to-array convention the leading zero index selects the
        // array itself; then index 1 strides 20 bytes and index 2 strides 4.
        let arr_ty = Type::get_array(Type::get_array(Type::get_i32(), 5), 5);
        let alloc = data.new_local_inst().alloc(arr_ty);
        data.layout_mut().insert_inst(entry, alloc);
        let zero = data.new_local_inst().integer(0);
        let one = data.new_local_inst().integer(1);
        let two = data.new_local_inst().integer(2);
        let gep = data
            .new_local_inst()
            .get_elem_ptr(alloc, vec![zero, one, two]);
        data.layout_mut().insert_inst(entry, gep);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.constant_offset(&ctx, alloc), Some(0));
        assert_eq!(env.constant_offset(&ctx, gep), Some(1 * 20 + 2 * 4));
    }

    #[test]
    fn constant_offset_rejects_variable_index() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference(), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let param = data.params()[0];
        let index = data.params()[1];
        let zero = data.new_local_inst().integer(0);
        let gep = data.new_local_inst().get_elem_ptr(param, vec![zero]);
        data.layout_mut().insert_inst(entry, gep);
        let gep_var = data.new_local_inst().get_elem_ptr(param, vec![index]);
        data.layout_mut().insert_inst(entry, gep_var);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.constant_offset(&ctx, gep), Some(0));
        assert_eq!(env.constant_offset(&ctx, gep_var), None);
    }

    #[test]
    fn intra_alias_rules_matrix() {
        let mut program = Program::new();
        let global = new_global(&mut program, Type::get_i32());
        let global2 = new_global(&mut program, Type::get_i32());
        let function = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let param = data.params()[0];
        let alloc_a = data.new_local_inst().alloc(Type::get_i32());
        let alloc_b = data.new_local_inst().alloc(Type::get_i32());
        data.layout_mut().insert_inst(entry, alloc_a);
        data.layout_mut().insert_inst(entry, alloc_b);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.alias(&ctx, alloc_a, alloc_b), AliasResult::NoAlias);
        assert_eq!(env.alias(&ctx, alloc_a, global), AliasResult::NoAlias);
        assert_eq!(env.alias(&ctx, alloc_a, param), AliasResult::NoAlias);
        assert_eq!(env.alias(&ctx, global, global2), AliasResult::NoAlias);
        assert_eq!(env.alias(&ctx, global, param), AliasResult::MayAlias);
        assert_eq!(env.alias(&ctx, param, param), AliasResult::MustAlias);
    }

    #[test]
    fn same_base_constant_offsets_disentangle() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        // A local i32 array base; offsets 0, 4, 8 are pairwise disjoint.
        let alloc = data
            .new_local_inst()
            .alloc(Type::get_array(Type::get_i32(), 4));
        data.layout_mut().insert_inst(entry, alloc);
        let zero = data.new_local_inst().integer(0);
        let one = data.new_local_inst().integer(1);
        let two = data.new_local_inst().integer(2);
        let g0 = data.new_local_inst().get_elem_ptr(alloc, vec![zero]);
        let g1 = data.new_local_inst().get_elem_ptr(alloc, vec![one]);
        let g2 = data.new_local_inst().get_elem_ptr(alloc, vec![two]);
        data.layout_mut().insert_inst(entry, g0);
        data.layout_mut().insert_inst(entry, g1);
        data.layout_mut().insert_inst(entry, g2);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.alias(&ctx, g0, g0), AliasResult::MustAlias);
        assert_eq!(env.alias(&ctx, g0, g1), AliasResult::NoAlias);
        assert_eq!(env.alias(&ctx, g1, g2), AliasResult::NoAlias);
        assert_eq!(env.alias(&ctx, g0, g2), AliasResult::NoAlias);
    }

    #[test]
    fn block_param_resolves_through_uniform_incoming_edges() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let alloc = data.new_local_inst().alloc(Type::get_i32());
        data.layout_mut().insert_inst(entry, alloc);

        let head = data
            .new_basic_block()
            .basic_block("head".into(), vec![Type::get_i32().reference()]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        data.layout_mut().push_bb_back(head);
        data.layout_mut().push_bb_back(exit);

        let param = data.bb_data(head).params()[0];
        let jump = data.new_local_inst().jump(head, vec![alloc]);
        data.layout_mut().insert_inst(entry, jump);
        let ret_head = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(head, ret_head);
        let ret_exit = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret_exit);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.base_of(&ctx, param), MemObject::Alloc(alloc));
        assert_eq!(env.constant_offset(&ctx, param), Some(0));
    }

    #[test]
    fn conflicting_block_param_edges_yield_unknown() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let alloc_a = data.new_local_inst().alloc(Type::get_i32());
        let alloc_b = data.new_local_inst().alloc(Type::get_i32());
        data.layout_mut().insert_inst(entry, alloc_a);
        data.layout_mut().insert_inst(entry, alloc_b);

        let head = data
            .new_basic_block()
            .basic_block("head".into(), vec![Type::get_i32().reference()]);
        let mid = data.new_basic_block().basic_block("mid".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        data.layout_mut().push_bb_back(mid);
        data.layout_mut().push_bb_back(head);
        data.layout_mut().push_bb_back(exit);

        let param = data.bb_data(head).params()[0];
        let jump_a = data.new_local_inst().jump(head, vec![alloc_a]);
        data.layout_mut().insert_inst(entry, jump_a);
        let jump_b = data.new_local_inst().jump(head, vec![alloc_b]);
        data.layout_mut().insert_inst(mid, jump_b);
        let ret_head = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(head, ret_head);
        let ret_exit = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret_exit);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.base_of(&ctx, param), MemObject::Unknown);
    }

    #[test]
    fn loop_carried_bumped_row_pointer_resolves_through_back_edge() {
        // A loop header row-pointer parameter whose back edge forwards a GEP
        // of the parameter itself (a pointer bump) must still resolve to the
        // alloc base carried by the entry edge. Before the fix the
        // self-referential back edge stalled the base fixed point, so the
        // header parameter (and every load through it) came out Unknown.
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();

        let base_alloc = data.new_local_inst().alloc(Type::get_i32());
        data.layout_mut().insert_inst(entry, base_alloc);
        let zero = data.new_local_inst().integer(0);
        data.layout_mut().insert_inst(entry, zero);
        let row = data.new_local_inst().raw(GetElemPtr::new_data(
            base_alloc,
            vec![zero],
            Type::get_i32().reference(),
        ));
        data.layout_mut().insert_inst(entry, row);
        let stride = data.new_local_inst().integer(4);
        data.layout_mut().insert_inst(entry, stride);

        let head = data
            .new_basic_block()
            .basic_block("head".into(), vec![Type::get_i32().reference()]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        data.layout_mut().push_bb_back(head);
        data.layout_mut().push_bb_back(exit);

        let param = data.bb_data(head).params()[0];
        let entry_jump = data.new_local_inst().jump(head, vec![row]);
        data.layout_mut().insert_inst(entry, entry_jump);
        // Back edge: `%param' = gep %param, 4` forwarded to the header.
        let bumped = data.new_local_inst().raw(GetElemPtr::new_data(
            param,
            vec![stride],
            Type::get_i32().reference(),
        ));
        data.layout_mut().insert_inst(head, bumped);
        let back_jump = data.new_local_inst().jump(head, vec![bumped]);
        data.layout_mut().insert_inst(head, back_jump);
        let ret_head = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(head, ret_head);
        let ret_exit = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret_exit);

        let (env, ctx) = env_of(&program, function);
        assert_eq!(env.base_of(&ctx, param), MemObject::Alloc(base_alloc));
    }
}
