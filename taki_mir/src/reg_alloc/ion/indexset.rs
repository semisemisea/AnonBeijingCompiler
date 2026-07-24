/*
 * Adapted from regalloc2 0.15.1 src/indexset.rs.
 *
 * Released under the Apache License 2.0 with LLVM Exception. See the
 * repository LICENSE for the full license text.
 */

//! Sparse, unbounded sets of allocator indices.

use rustc_hash::FxHashMap;

const BITS_PER_WORD: usize = u64::BITS as usize;

/// A sparse bitset used for block liveness. Zero words are retained so a set
/// can be updated without reallocating while walking a CFG worklist.
#[derive(Clone, Default)]
pub struct IndexSet {
    words: FxHashMap<u32, u64>,
}

impl IndexSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, index: usize, value: bool) {
        let word = (index / BITS_PER_WORD) as u32;
        let bit = index % BITS_PER_WORD;
        if value {
            *self.words.entry(word).or_default() |= 1 << bit;
        } else if let Some(bits) = self.words.get_mut(&word) {
            *bits &= !(1 << bit);
        }
    }

    pub fn get(&self, index: usize) -> bool {
        self.words
            .get(&((index / BITS_PER_WORD) as u32))
            .is_some_and(|bits| bits & (1 << (index % BITS_PER_WORD)) != 0)
    }

    pub fn union_with(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for (&word, &bits) in &other.words {
            let ours = self.words.entry(word).or_default();
            changed |= bits & !*ours != 0;
            *ours |= bits;
        }
        changed
    }

    pub fn is_empty(&self) -> bool {
        self.words.values().all(|&bits| bits == 0)
    }

    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().flat_map(|(&word, &bits)| {
            SetBits(bits).map(move |bit| word as usize * BITS_PER_WORD + bit)
        })
    }
}

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

impl core::fmt::Debug for IndexSet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut values: Vec<_> = self.iter().collect();
        values.sort_unstable();
        values.fmt(f)
    }
}
