/*
 * This file was initially derived from the files
 * `js/src/jit/BacktrackingAllocator.h` and
 * `js/src/jit/BacktrackingAllocator.cpp` in Mozilla Firefox, and was
 * originally licensed under the Mozilla Public License 2.0. We
 * subsequently relicensed it to Apache-2.0 WITH LLVM-exception (see
 * https://github.com/bytecodealliance/regalloc2/issues/7).
 *
 * Since the initial port, the design has been substantially evolved
 * and optimized.
 *
 * Local provenance: copied from regalloc2 0.15.1 `src/ion/redundant_moves.rs`.
 * Local modification: imports refer to taki_mir's allocator types.
 */

//! Redundant-move elimination.
//!
//! # 冗余移动消除（Redundant Move Elimination）
//!
//! 分配器主循环（`process` 模块）结束后，`moves` 模块要把"同一虚拟寄存器不同
//! 活跃区间之间"以及"跨基本块参数边"上需要的值搬运，解析成一条条具体的 move：
//! 先按程序点把语义上**并行**的 move 集合（`moves.rs` 的 `ParallelMoves`）解析成
//! 可顺序执行的序列，必要时借道 scratch 寄存器 / 临时栈槽打破循环依赖。解析
//! 结果里常常混着**冗余 move**——目标位置此刻已经持有源值，搬了等于白搬。
//! 本模块的 `RedundantMoveEliminator` 就是这道"最后一关"过滤器：逐条判断
//! "这条 move 能不能删"，能删的（`elide == true`）就不发射，省下一条机器指令。
//!
//! ## 判定原理：模拟"每个位置里装着什么"
//!
//! 消除器在**同一基本块内部、按机器实际执行顺序**维护一张抽象状态表：每个分配
//! 位置（物理寄存器或栈槽）当前装着什么值、这个值最初来自哪个虚拟寄存器。沿着
//! 真实执行顺序模拟每条 move 的效果，就能回答"move 之后目标里的值是不是本来
//! 就在那里"，从而判定冗余：
//!
//! - 目标位置**已经是源的副本**（之前搬过一次、中途没人改写它）→ 冗余；
//! - 源位置**是目标的副本**（值是从目标搬出去的，现在搬回去）→ 冗余；
//! - `from == to` 的自移动且知道搬的是哪个 vreg（值本来就在原地）→ 冗余。
//!
//! 具体判定见 `process_move` 中的两条 `elide` 规则。
//!
//! ## 与移动解析的关系
//!
//! - 本模块运行在移动解析（`moves.rs` 的 `resolve_inserted_moves`）的**最后一步**：
//!   并行集合 → 顺序序列 → scratch 分配 →（本模块）冗余消除 → 生成最终 `Edits`。
//! - 消除器**按执行顺序**逐条接收最终序列里的 move（`process_move`）；状态在
//!   同一基本块的程序点之间延续，前面 move 的判定会改变后面 move 的判定，
//!   因此顺序颠倒会直接导致误判，调用方必须严格按序喂入。
//! - 解析器为打破并行依赖环而引入的临时搬运经常制造"搬出去又搬回来"的冗余，
//!   这正是本模块的主要战果来源。
//!
//! ## 状态的失效（保守性）
//!
//! 状态表只在同一基本块内可信，任何"悄悄改写位置内容"的事件都会让记录过期：
//! 指令对某个位置的 **def**、被调用约定 **clobber** 的寄存器、会被任意指令破坏
//! 的**专用 scratch 寄存器**，以及**跨基本块边界**（不同执行路径到达块入口时，
//! 位置里装什么不再可知）。调用方在每两个程序点之间调用 `clear_alloc` 处理
//! def / clobber，跨块时调用 `clear` 全量清空。失效一律**保守**：宁可少消除
//! 一条 move，也不能把语义必需的 move 误删。
//!
//! 通用术语（SSA、VCode 等）可查 `docs/offline-handbook/glossary.md`。

use rustc_hash::FxHashMap;
use smallvec::{SmallVec, smallvec};

use crate::reg_alloc::reg::{Allocation, VReg};

// 单个分配位置的值来源状态机：描述"这个位置当前装着什么"。
// 三个变体：
// - `Copy(a, v)`：装着"位置 a 的值"的副本。第一项是值的**直接来源**位置
//   （记录的是直接搬过来的那一步，不隔层）；第二项是这条副本链最初对应的
//   虚拟寄存器，可能未知（见 `process_move` 里 `dst_vreg` 的推导）——未知时记
//   `None`，之后该副本失效时便无法降级成 `Orig`。
// - `Orig(r)`：装着虚拟寄存器 r 的**原始值**——这个位置就是 r 的值"本来的家"。
// - `None`：未知 / 未追踪：可能是空槽，也可能是内容不在记录里
//   （`process_move` 查表时，从未记录过的位置一律当作 `None`）。
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RedundantMoveState {
    Copy(Allocation, Option<VReg>),
    Orig(VReg),
    None,
}

// 冗余移动消除器的状态。两个映射互为"正 / 反向"：
// - `allocs`：分配位置 → 该位置当前值的状态（Copy / Orig / None）。
// - `reverse_allocs`：位置 → 所有"装着它的副本"的位置列表。
//   反向表是为了**成片失效**：某个位置的内容一旦被改写（def / clobber），
//   所有装着它副本的位置会同时失去可信度；要逐条从 `allocs` 里找太慢，
//   直接查反向表即可。每个位置的副本通常很少（小向量，一般 0~2 个）。
#[derive(Clone, Debug, Default)]
pub struct RedundantMoveEliminator {
    // 正向映射：每个分配位置当前装着什么。
    allocs: FxHashMap<Allocation, RedundantMoveState>,
    // 反向映射：源位置 → 所有"装着该源位置副本"的位置。
    reverse_allocs: FxHashMap<Allocation, SmallVec<[Allocation; 4]>>,
}

// `process_move` 的判定结果：`elide == true` 表示这条 move 冗余、
// 调用方应跳过不发射；`false` 则照常发射。
#[derive(Copy, Clone, Debug)]
pub struct RedundantMoveAction {
    pub elide: bool,
}

impl RedundantMoveEliminator {
    // 处理一条即将发射的 move `from -> to`，返回是否应删除（`elide`）。
    //
    // **调用约定**：调用方（`moves.rs` 的 `resolve_inserted_moves`）必须按机器
    // 实际执行顺序逐条喂入（同一程序点内按并行解析产出的顺序、跨程序点按位置
    // 顺序）；每次调用都会把状态机推进到"这条 move 执行之后"的样子，顺序颠倒
    // 会让状态表失真、导致误判。
    //
    // **`to_vreg` 参数**：这条 move 搬的是哪个虚拟寄存器的值。并行移动解析出的
    // 每条 move 都知道自己搬的 vreg；但解析器引入的临时搬运（借道 scratch
    // 寄存器 / 临时栈槽）常常不知道，此时传 `None`。
    pub fn process_move(
        &mut self,
        from: Allocation,
        to: Allocation,
        to_vreg: Option<VReg>,
    ) -> RedundantMoveAction {
        // 取源、目标位置的当前状态；从未记录过的位置一律视为 None（未知）。
        let from_state = self
            .allocs
            .get(&from)
            .copied()
            .unwrap_or(RedundantMoveState::None);
        let to_state = self
            .allocs
            .get(&to)
            .copied()
            .unwrap_or(RedundantMoveState::None);

        // 特例：from == to 且知道 vreg——值本来就在原地，move 必然冗余。
        // 顺手把该位置刷新为"装着这个 vreg 的原始值"：先清掉它之前可能存在的
        // 副本记录（这个位置现在就是这个值"本来的家"了），再登记成 Orig。
        if from == to && to_vreg.is_some() {
            self.clear_alloc(to);
            self.allocs
                .insert(to, RedundantMoveState::Orig(to_vreg.unwrap()));
            return RedundantMoveAction { elide: true };
        }

        // 从源位置的当前状态推断：源里那个值最初属于哪个 vreg。
        // （Copy 时用链上记录的 vreg；Orig 时就是它自己；None 时未知。）
        let src_vreg = match from_state {
            RedundantMoveState::Copy(_, opt_r) => opt_r,
            RedundantMoveState::Orig(r) => Some(r),
            RedundantMoveState::None => None,
        };
        // move 之后目标里装的值对应的 vreg：优先用调用方给的 to_vreg，没有就沿
        // 副本链追溯到 src_vreg。这个 vreg 会写进新的 Copy 记录，将来该副本失效
        // 时，`clear_alloc` 靠它把副本"降级"成 Orig(vreg)。
        let dst_vreg = to_vreg.or(src_vreg);
        // 冗余判定的两条核心规则（命中任一即 elide）：
        // 1) 目标已经是源的副本：to_state 是 Copy(源 == from)，即之前已执行过
        //    from -> to；只要中途 to 没被改写（被改写会触发 clear_alloc 删掉
        //    这条记录），to 现在就还装着 from 的值 → 再搬一次是冗余。
        // 2) 源是目标的副本：from_state 是 Copy(源 == to)，即这个值是从 to 搬
        //    出去的，现在搬回 to 等于送回原处；to 的内容必然没变过（否则 from
        //    的这条 Copy 记录早被清掉了）→ 冗余。
        let elide = match (from_state, to_state) {
            (_, RedundantMoveState::Copy(orig_alloc, _)) if orig_alloc == from => true,
            (RedundantMoveState::Copy(new_alloc, _), _) if new_alloc == to => true,
            _ => false,
        };

        // 若这条 move 真的要执行：to 的内容将被改写，先把"装着 to 的副本"的
        // 位置以及 to 本身全部失效，防止后续判定基于过期信息
        // （保守：宁可少消除，不可错消除）。
        if !elide {
            self.clear_alloc(to);
        }

        // 更新状态机：move 之后（无论 elide 与否）to 里装的就是 from 的副本。
        // 被 elide 时这条事实同样成立——要么 to 本来就是 from 的副本，要么值被
        // "送回了家"，两种情况下 to 与 from 的值都相等——所以统一登记。
        // 只追踪涉及寄存器的 move：纯栈槽间的搬运要借道临时寄存器，中间状态
        // 无法用这张表建模，且收益很低（与上游 regalloc2 "Don't track
        // stack-to-stack copies" 一致，不追踪）。
        if from.is_reg() || to.is_reg() {
            self.allocs
                .insert(to, RedundantMoveState::Copy(from, dst_vreg));
            self.reverse_allocs
                .entry(from)
                .or_insert_with(|| smallvec![])
                .push(to);
        }

        RedundantMoveAction { elide }
    }

    // 清空全部追踪状态。跨基本块边界时必须调用：不同执行路径到达块入口时，
    // 每个位置里装什么不再可知，沿用旧状态会把别的路径上建立的值关系错当成
    // 当前路径的事实，导致错误消除。
    pub fn clear(&mut self) {
        self.allocs.clear();
        self.reverse_allocs.clear();
    }

    // 使 `alloc` 及其所有已记录的副本失效。调用时机：`alloc` 的内容即将被改写
    // ——指令对它写 def、它被调用约定 clobber、它作为专用 scratch 被任意指令
    // 破坏（见调用方 `redundant_move_process_side_effects`），或 `process_move`
    // 判定目标将被真正写入。改写之后，"alloc 装着什么"不可再信，更关键的是
    // "谁装着 alloc 的副本"也不可信（副本的值不再等于 alloc 的新内容）。
    //
    // 做法：查反向表拿到所有"装着 alloc 副本"的位置，逐一把它们从 `allocs` 中
    // 删除（它们重新变成"未知"）；最后删除 alloc 自己。被删的位置后续不会参与
    // 消除判定，保证结果保守、正确。
    //
    // 注：循环里对每个失效副本先尝试降级（还记得 vreg 就写成 Orig(vreg)、否则
    // 写成 None），但随后立即无条件 `remove`，这次降级写入实际上不会生效——
    // 这是从上游 regalloc2 0.15.1 逐字移植来的写法，净效果就是删除。
    pub fn clear_alloc(&mut self, alloc: Allocation) {
        if let Some(existing_copies) = self.reverse_allocs.get_mut(&alloc) {
            for to_inval in existing_copies.drain(..) {
                if let Some(val) = self.allocs.get_mut(&to_inval) {
                    match val {
                        RedundantMoveState::Copy(_, Some(vreg)) => {
                            *val = RedundantMoveState::Orig(*vreg);
                        }
                        _ => *val = RedundantMoveState::None,
                    }
                }
                self.allocs.remove(&to_inval);
            }
        }
        self.allocs.remove(&alloc);
    }
}
