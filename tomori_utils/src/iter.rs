//! A double-ended iterator over entity references and entities.
//! with a merged file of
//! A double-ended iterator over entity references.
//!
//! When `core::iter::Step` is stabilized, `Keys` could be implemented as a wrapper around
//! `core::ops::Range`, but for now, we implement it manually.

use crate::entity::EntityRef;
use core::iter::Enumerate;
use core::marker::PhantomData;
use core::slice;
use std::vec;

/// Iterate over all keys in order.
pub struct Iter<'a, K: EntityRef, V>
where
    V: 'a,
{
    enumerate: Enumerate<slice::Iter<'a, V>>,
    unused: PhantomData<K>,
}

impl<'a, K: EntityRef, V> Iter<'a, K, V> {
    /// Create an `Iter` iterator that visits the `PrimaryMap` keys and values
    /// of `iter`.
    pub fn new(iter: slice::Iter<'a, V>) -> Self {
        Self {
            enumerate: iter.enumerate(),
            unused: PhantomData,
        }
    }
}

impl<'a, K: EntityRef, V> Iterator for Iter<'a, K, V> {
    type Item = (K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        self.enumerate.next().map(|(i, v)| (K::new(i), v))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.enumerate.size_hint()
    }
}

impl<'a, K: EntityRef, V> DoubleEndedIterator for Iter<'a, K, V> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.enumerate.next_back().map(|(i, v)| (K::new(i), v))
    }
}

impl<'a, K: EntityRef, V> ExactSizeIterator for Iter<'a, K, V> {}

/// Iterate over all keys in order.
pub struct IterMut<'a, K: EntityRef, V>
where
    V: 'a,
{
    enumerate: Enumerate<slice::IterMut<'a, V>>,
    unused: PhantomData<K>,
}

impl<'a, K: EntityRef, V> IterMut<'a, K, V> {
    /// Create an `IterMut` iterator that visits the `PrimaryMap` keys and values
    /// of `iter`.
    pub fn new(iter: slice::IterMut<'a, V>) -> Self {
        Self {
            enumerate: iter.enumerate(),
            unused: PhantomData,
        }
    }
}

impl<'a, K: EntityRef, V> Iterator for IterMut<'a, K, V> {
    type Item = (K, &'a mut V);

    fn next(&mut self) -> Option<Self::Item> {
        self.enumerate.next().map(|(i, v)| (K::new(i), v))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.enumerate.size_hint()
    }
}

impl<'a, K: EntityRef, V> DoubleEndedIterator for IterMut<'a, K, V> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.enumerate.next_back().map(|(i, v)| (K::new(i), v))
    }
}

impl<'a, K: EntityRef, V> ExactSizeIterator for IterMut<'a, K, V> {}

/// Iterate over all keys in order.
pub struct IntoIter<K: EntityRef, V> {
    enumerate: Enumerate<vec::IntoIter<V>>,
    unused: PhantomData<K>,
}

impl<K: EntityRef, V> IntoIter<K, V> {
    /// Create an `IntoIter` iterator that visits the `PrimaryMap` keys and values
    /// of `iter`.
    pub fn new(iter: vec::IntoIter<V>) -> Self {
        Self {
            enumerate: iter.enumerate(),
            unused: PhantomData,
        }
    }
}

impl<K: EntityRef, V> Iterator for IntoIter<K, V> {
    type Item = (K, V);

    fn next(&mut self) -> Option<Self::Item> {
        self.enumerate.next().map(|(i, v)| (K::new(i), v))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.enumerate.size_hint()
    }
}

impl<K: EntityRef, V> DoubleEndedIterator for IntoIter<K, V> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.enumerate.next_back().map(|(i, v)| (K::new(i), v))
    }
}

impl<K: EntityRef, V> ExactSizeIterator for IntoIter<K, V> {}

/// Iterate over all keys in order.
pub struct Keys<K: EntityRef> {
    pos: usize,
    rev_pos: usize,
    unused: PhantomData<K>,
}

impl<K: EntityRef> Keys<K> {
    /// Create a `Keys` iterator that visits `len` entities starting from 0.
    pub fn with_len(len: usize) -> Self {
        Self {
            pos: 0,
            rev_pos: len,
            unused: PhantomData,
        }
    }
}

impl<K: EntityRef> Iterator for Keys<K> {
    type Item = K;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos < self.rev_pos {
            let k = K::new(self.pos);
            self.pos += 1;
            Some(k)
        } else {
            None
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let size = self.rev_pos - self.pos;
        (size, Some(size))
    }
}

impl<K: EntityRef> DoubleEndedIterator for Keys<K> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.rev_pos > self.pos {
            let k = K::new(self.rev_pos - 1);
            self.rev_pos -= 1;
            Some(k)
        } else {
            None
        }
    }
}

impl<K: EntityRef> ExactSizeIterator for Keys<K> {}
