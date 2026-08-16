/*
 * Adapted from regalloc2 0.15.1 src/indexset.rs.
 *
 * Released under the Apache License 2.0 with LLVM Exception. See the
 * repository LICENSE for the full license text.
 */

//! Sparse, unbounded sets of allocator indices.
//!
//! 稀疏、无界（按需增长）的分配器下标集合。ION 分配器内部几乎不用指针/引用，
//! 而是统一用「下标」指代各类对象（虚拟寄存器 VReg、活跃区间、bundle、物理
//! 寄存器等），因此经常需要「某个下标在不在集合里」这种查询——块级活跃性
//! 分析（liveins / liveouts）就是本集合的主要用户（见 `liveranges.rs`）。

use rustc_hash::FxHashMap;

// 每个「字」（u64）容纳的位数。下标 index 落在第 index/64 个字、第
// index%64 位，set/get 里都用这套换算（64 是 2 的幂，编译器会把它
// 优化成移位与掩码，无需担心除法开销）。
const BITS_PER_WORD: usize = u64::BITS as usize;

/// A sparse bitset used for block liveness. Zero words are retained so a set
/// can be updated without reallocating while walking a CFG worklist.
/// 布局：HashMap 的键是字号（index / 64），值是该字的 64 位位图，其中第 b
/// 位表示下标 index = 字号*64 + b 在集合中。键只在实际置位时才创建，所以
/// 下标空间再大也只占用与「元素个数」成正比的内存——这就是「稀疏」；
/// 「无界」指集合没有容量上限，下标可以一直往上长。
///
/// 在分配器中的用途：块级活跃性分析（`liveranges.rs` 的 worklist 算法）为
/// 每个基本块维护一个 liveins / liveouts 集合，第 i 位对应 VRegIndex = i
/// 的虚拟寄存器；块间传播就是对前驱/后继的集合做并集运算（`union_with`）。
#[derive(Clone, Default)]
pub struct IndexSet {
    // 字号 -> 64 位位图。用 FxHashMap（rustc_hash 提供的快速非加密哈希）
    // 而非标准 HashMap，纯粹是编译器内部的性能考量。
    words: FxHashMap<u32, u64>,
}

impl IndexSet {
    /// 空集合。等价于 `Default`；分配器里每个块的 liveins / liveouts 都从
    /// 空集起步，再由 worklist 算法逐轮并集收敛。
    pub fn new() -> Self {
        Self::default()
    }

    /// 把下标 index 置入（value = true）或移出（value = false）集合。
    /// 置位时若该字尚不存在则自动创建（`entry().or_default()`）；清位时
    /// 只把位清零而**不删除**字——零字被保留下来，这样在 CFG worklist
    /// 反复更新的过程中，HashMap 不会因键的增删频繁触发扩容/收缩重分配，
    /// 集合的内存位置保持稳定（对应上方英文注释里的 "Zero words are
    /// retained"）。
    pub fn set(&mut self, index: usize, value: bool) {
        let word = (index / BITS_PER_WORD) as u32;
        let bit = index % BITS_PER_WORD;
        if value {
            *self.words.entry(word).or_default() |= 1 << bit;
        } else if let Some(bits) = self.words.get_mut(&word) {
            *bits &= !(1 << bit);
        }
    }

    /// 查询下标 index 是否在集合中：字不存在（从未置过位）即视为不在，
    /// 无需显式区分「空字」和「缺失的字」。
    pub fn get(&self, index: usize) -> bool {
        self.words
            .get(&((index / BITS_PER_WORD) as u32))
            .is_some_and(|bits| bits & (1 << (index % BITS_PER_WORD)) != 0)
    }

    /// 原地求并集：把 other 中所有置位的下标并入 self，并返回本次是否
    /// 真的发生了变化（self 至少新增了一个位）。
    /// 返回值是 worklist 算法的收敛关键：`liveranges.rs` 把当前块的 liveout
    /// 并入前驱块的 liveout 后，只有 changed 为真才把前驱重新入队继续传播；
    /// 否则该前驱已稳定，不必再扫一遍。
    pub fn union_with(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for (&word, &bits) in &other.words {
            let ours = self.words.entry(word).or_default();
            changed |= bits & !*ours != 0;
            *ours |= bits;
        }
        changed
    }

    /// 集合是否为空。注意不能只查 `words.len() == 0`——因为零字会被保留
    /// （见 set），必须逐个检查所有字是否全为 0。
    pub fn is_empty(&self) -> bool {
        self.words.values().all(|&bits| bits == 0)
    }

    /// 迭代所有在集合中的下标。整体无序：每个字内部从低位到高位，但字与字
    /// 之间按 HashMap 的迭代顺序；需要有序输出时自行排序（Debug 实现就是
    /// 这么做的）。`SetBits` 负责枚举单个字内部的置位。
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().flat_map(|(&word, &bits)| {
            SetBits(bits).map(move |bit| word as usize * BITS_PER_WORD + bit)
        })
    }
}

// 单个 u64 字的置位枚举器：每次 next 产出当前最低的一个置位位号
// （trailing_zeros），随即用 `x &= x - 1` 把最低置位清掉——这是经典的
// 「清除最低置位」位技巧。迭代次数与置位数成正比而非与 64 成正比，且天然
// 按位号升序产出。
struct SetBits(u64);
impl Iterator for SetBits {
    type Item = usize;
    fn next(&mut self) -> Option<Self::Item> {
        core::num::NonZeroU64::new(self.0).map(|bits| {
            let bit = bits.trailing_zeros() as usize;
            self.0 &= self.0 - 1;
            bit
        })
    }
}

// 调试输出：收集全部下标、排序后按 Vec 的格式打印（如 {1, 3, 5}），
// 便于在 trace 日志里直观查看某个块的活入/活出集合。
impl core::fmt::Debug for IndexSet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut values: Vec<_> = self.iter().collect();
        values.sort_unstable();
        values.fmt(f)
    }
}
