//! Implementation of *Interprocedural Sparse Condition Constant Propagation*
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ir::inst_kind::mem_zero::MemZeroLen;
use crate::opt::{
    analysis_passes::{
        effects::{EffectAnalysis, WriteRoot},
        icfg::{Edge, EdgeType},
        memory::{BaseEnv, MemObject},
    },
    prelude::*,
    utils::visit_and_replace,
};

pub struct IPSCCP;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Lattice {
    #[default]
    Top,
    Constant(i32),
    Bottom,
}

impl Lattice {
    fn merge(self, new: Lattice) -> Lattice {
        match (self, new) {
            (Lattice::Top, v) => v,
            (Lattice::Constant(lhs), rhs) => match rhs {
                Lattice::Top => self,
                Lattice::Constant(rhs) if lhs == rhs => self,
                _ => Lattice::Bottom,
            },
            (Lattice::Bottom, _) => Lattice::Bottom,
        }
    }

    fn update(&mut self, new: Lattice) -> bool {
        match (*self, new) {
            (Lattice::Top, _) if new != Lattice::Top => {
                *self = new;
                true
            }
            (Lattice::Constant(old), Lattice::Constant(new)) if old != new => {
                *self = Lattice::Bottom;
                true
            }
            (Lattice::Constant(..), Lattice::Bottom) => {
                *self = Lattice::Bottom;
                true
            }
            _ => false,
        }
    }
}

#[derive(Debug, Default)]
struct LatticeMap(FxHashMap<Node, Lattice>);

impl LatticeMap {
    fn new_var(&mut self, node: Node) {
        self.0.insert(node, Lattice::Bottom);
    }

    fn new_const(&mut self, node: Node, val: i32) {
        self.0.insert(node, Lattice::Constant(val));
    }

    fn get(&self, node: Node) -> Lattice {
        *self.0.get(&node).unwrap_or(&Lattice::Top)
    }

    // Return whether the original status is changed.
    fn insert_or_update(&mut self, node: Node, status: Lattice) -> bool {
        match self.0.entry(node) {
            std::collections::hash_map::Entry::Occupied(mut e) => e.get_mut().update(status),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(status);
                status != Lattice::Top
            }
        }
    }

    /// Overwrite the lattice unconditionally, returning whether it changed.
    /// Loads use this: their value is a snapshot of the simulated memory at
    /// the time of (re)scheduling, and merging an outdated snapshot with the
    /// current one (e.g. 0 read before a store, 6 after) would collapse to
    /// Bottom instead of refining to the latest value.
    fn insert_or_replace(&mut self, node: Node, status: Lattice) -> bool {
        match self.0.entry(node) {
            std::collections::hash_map::Entry::Occupied(mut e) if *e.get() != status => {
                e.insert(status);
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(status);
                status != Lattice::Top
            }
        }
    }
}
type EdgeSet = FxHashSet<Edge>;
type NodeSet = FxHashSet<Node>;

/// A constant-offset memory cell: a local stack object or a global object
/// at a byte offset. Only i32-sized accesses are modeled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CellKey {
    Local(Function, Inst, i64),
    Global(Inst, i64),
}

/// A memory root: an entire local stack object or global object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RootKey {
    Local(Function, Inst),
    Global(Inst),
}

impl CellKey {
    fn offset(self) -> i64 {
        match self {
            CellKey::Local(_, _, off) | CellKey::Global(_, off) => off,
        }
    }

    fn root(self) -> RootKey {
        match self {
            CellKey::Local(func, inst, _) => RootKey::Local(func, inst),
            CellKey::Global(inst, _) => RootKey::Global(inst),
        }
    }
}

/// Simulation of the "main memory" for constant-offset accesses (see
/// `docs/memory_alias_analysis.md` §5.3). Per cell we keep the per-writer
/// contributions so a store whose source refines Top -> Constant over the
/// worklist can recover; the folded value is the lattice meet of the
/// writers. Zero ranges model `MemZero` and zero-initialized globals.
#[derive(Default)]
struct MemState {
    /// cell -> writer store instruction -> contribution lattice.
    cells: FxHashMap<CellKey, FxHashMap<Inst, Lattice>>,
    /// root -> merged zero byte intervals [from, to).
    zero: FxHashMap<RootKey, Vec<(i64, i64)>>,
    /// root -> cells currently tracked (for MemZero range removal).
    root_cells: FxHashMap<RootKey, Vec<CellKey>>,
    /// root -> loads that read it (re-scheduled on any root change).
    root_loaders: FxHashMap<RootKey, Vec<Node>>,
    /// every root ever modeled (for "may write anything" invalidation).
    all_roots: FxHashSet<RootKey>,
    /// roots invalidated by an unknown write (unresolvable store
    /// destination, may-write call). A root in this set never folds loads
    /// again: the unknown write may have happened at any program point,
    /// so a later store to the same cell (even a constant one) cannot be
    /// trusted to be visible to earlier loads re-scheduled after it.
    cleared_roots: FxHashSet<RootKey>,
}

impl MemState {
    /// Folded value of a cell: meet of the writer contributions.
    fn cell_fold(&self, key: CellKey) -> Lattice {
        match self.cells.get(&key) {
            None => Lattice::Top,
            Some(writers) => writers.values().fold(Lattice::Top, |acc, &v| acc.merge(v)),
        }
    }

    /// The value a load of `key` reads: the cell if any store wrote it,
    /// else a zero-range value if covered, else Bottom (unknown).
    fn read(&self, key: CellKey) -> Lattice {
        // A root invalidated by an unknown write (dynamic-index store,
        // may-write call) never folds again: the write may sit between
        // any store and any load of the root, and re-scheduled loads
        // would otherwise read a cell state that belongs to a different
        // program point.
        if self.cleared_roots.contains(&key.root()) {
            return Lattice::Bottom;
        }
        let cell = self.cell_fold(key);
        if cell != Lattice::Top {
            return cell;
        }
        let root = key.root();
        let covered = self.zero.get(&root).is_some_and(|ranges| {
            ranges
                .iter()
                .any(|&(from, to)| key.offset() >= from && key.offset() < to)
        });
        if covered {
            Lattice::Constant(0)
        } else {
            Lattice::Bottom
        }
    }

    /// Record a store `writer` of `value` into `key`. A Top source writes
    /// an unknown value (Bottom contribution) but may recover when the
    /// source refines and the writer is re-visited. Returns whether the
    /// folded cell value changed.
    fn write(&mut self, key: CellKey, writer: Inst, value: Lattice) -> bool {
        let contribution = if value == Lattice::Top {
            Lattice::Bottom
        } else {
            value
        };
        let root = key.root();
        self.all_roots.insert(root);
        let writers = self.cells.entry(key).or_default();
        let before = writers.values().fold(Lattice::Top, |acc, &v| acc.merge(v));
        if writers.is_empty() {
            self.root_cells.entry(root).or_default().push(key);
        }
        writers.insert(writer, contribution);
        let after = writers.values().fold(Lattice::Top, |acc, &v| acc.merge(v));
        before != after
    }

    /// Drop every cell and zero range of `root` (a call or an unknown
    /// destination may have overwritten anything).
    ///
    /// `unknown_write` marks a *value-unknown* invalidation (dynamic-index
    /// store, may-write-anything call): the write may sit between any
    /// store and any load of the root, so the root must never fold loads
    /// again (a re-scheduled load would read a cell state belonging to a
    /// different program point). A `false` invalidation (call to a callee
    /// with known write roots) only drops the current cells; the callee's
    /// stores are modeled separately and may repopulate them.
    fn clear(&mut self, root: RootKey, unknown_write: bool) -> bool {
        let mut changed = false;
        if let Some(keys) = self.root_cells.remove(&root) {
            for key in keys {
                self.cells.remove(&key);
            }
            changed = true;
        }
        changed |= self.zero.remove(&root).is_some();
        if unknown_write {
            self.cleared_roots.insert(root);
        }
        changed
    }

    /// A `MemZero` of `len` bytes at `off` of `root` zeroes the range.
    ///
    /// Cells already written by stores are NOT dropped: the worklist does
    /// not process a block's instructions in layout order, so a MemZero may
    /// be visited after the stores of the same initialization sequence. The
    /// frontend always emits MemZero before the stores, so a cell that
    /// exists must reflect a store that is semantically later; the zero
    /// range only answers loads when no store has written the cell.
    fn mem_zero(&mut self, root: RootKey, off: i64, len: i64) -> bool {
        self.all_roots.insert(root);
        let ranges = self.zero.entry(root).or_default();
        let before_len = ranges.len();
        merge_zero_interval(ranges, off, off + len);
        ranges.len() != before_len
    }

    /// The value a store through an unresolvable address may target: the
    /// concrete roots from the points-to analysis, or `None` for "any".
    fn possible_targets(
        &self,
        analysis: &EffectAnalysis,
        func: Function,
        addr: Inst,
        ctx: &ArenaContext<'_>,
    ) -> Option<Vec<RootKey>> {
        match analysis.targets_of(ctx, func, addr) {
            Some(objects) => {
                let mut roots = Vec::new();
                for o in objects {
                    match o {
                        crate::opt::analysis_passes::effects::AbstractObject::Global(g) => {
                            roots.push(RootKey::Global(g));
                        }
                        crate::opt::analysis_passes::effects::AbstractObject::Alloc(cf, a)
                            if cf == func =>
                        {
                            roots.push(RootKey::Local(func, a));
                        }
                        _ => {}
                    }
                }
                Some(roots)
            }
            None => None,
        }
        .or_else(|| {
            // Unresolvable address: conservatively everything modeled.
            Some(self.all_roots.iter().copied().collect())
        })
    }
}

/// Merge `[from, to)` into a sorted, disjoint interval list.
fn merge_zero_interval(ranges: &mut Vec<(i64, i64)>, from: i64, to: i64) {
    let mut from = from;
    let mut to = to;
    let mut i = 0;
    while i < ranges.len() {
        let (l, r) = ranges[i];
        if r < from {
            i += 1;
            continue;
        }
        if l > to {
            break;
        }
        from = from.min(l);
        to = to.max(r);
        ranges.remove(i);
    }
    ranges.insert(i, (from, to));
}

/// Clear every cell the callee may write (in the caller's terms) and
/// re-schedule the affected loads. Unknown writers clear everything.
fn invalidate_call(
    analysis: &EffectAnalysis,
    state: &mut MemState,
    callee: Function,
    func: Function,
    mem_reschedule: &mut Vec<Node>,
) {
    let roots: Vec<RootKey> = match analysis.call_write_roots(callee, func) {
        Some(roots) => roots
            .into_iter()
            .map(|r| match r {
                WriteRoot::Global(g) => RootKey::Global(g),
                WriteRoot::Local(f, a) => RootKey::Local(f, a),
            })
            .collect(),
        None => state.all_roots.iter().copied().collect(),
    };
    let unknown = analysis.call_write_roots(callee, func).is_none();
    for root in roots {
        if state.clear(root, unknown) {
            if let Some(loaders) = state.root_loaders.get(&root) {
                mem_reschedule.extend(loaders.iter().copied());
            }
        }
    }
}

/// Resolve `addr` in `func` to a constant-offset cell on a modeled root,
/// using the base-object environment.
fn resolve_cell(
    env: &BaseEnv,
    ctx: &ArenaContext<'_>,
    func: Function,
    addr: Inst,
) -> Option<(CellKey, RootKey)> {
    let off = env.constant_offset(ctx, addr)?;
    match env.base_of(ctx, addr) {
        MemObject::Alloc(a) => {
            let key = CellKey::Local(func, a, off);
            Some((key, key.root()))
        }
        MemObject::Global(g) => {
            let key = CellKey::Global(g, off);
            Some((key, key.root()))
        }
        _ => None,
    }
}

mod solver;

fn mathematic_operation(op: BinaryOp, lhs: i32, rhs: i32) -> i32 {
    match op {
        BinaryOp::NotEq => (lhs != rhs) as i32,
        BinaryOp::Eq => (lhs == rhs) as i32,
        BinaryOp::Gt => (lhs > rhs) as i32,
        BinaryOp::Lt => (lhs < rhs) as i32,
        BinaryOp::Ge => (lhs >= rhs) as i32,
        BinaryOp::Le => (lhs <= rhs) as i32,
        BinaryOp::Add => lhs.wrapping_add(rhs),
        BinaryOp::Sub => lhs.wrapping_sub(rhs),
        BinaryOp::Mul => lhs.wrapping_mul(rhs),
        BinaryOp::Div => {
            assert_ne!(rhs, 0);
            lhs.wrapping_div(rhs)
        }
        BinaryOp::Rem => {
            assert_ne!(rhs, 0);
            lhs.wrapping_rem(rhs)
        }
        BinaryOp::And => lhs & rhs,
        BinaryOp::Or => lhs | rhs,
        BinaryOp::Xor => lhs ^ rhs,
        BinaryOp::Shl => lhs.wrapping_shl(rhs as u32),
        BinaryOp::Shr => (lhs as u32).wrapping_shr(rhs as u32) as i32,
        BinaryOp::Sar => lhs.wrapping_shr(rhs as u32),
        BinaryOp::Min => lhs.min(rhs),
        BinaryOp::Max => lhs.max(rhs),
    }
}

pub(super) fn fold_f32_to_i32(value: f32) -> Option<i32> {
    // The target conversions truncate toward zero for representable values.
    // Keep non-finite and out-of-range values as runtime casts because Rust's
    // saturating `as` conversion does not match the target instructions there.
    (value.is_finite() && value >= i32::MIN as f32 && value < i32::MAX as f32)
        .then_some(value as i32)
}

#[cfg(test)]
mod tests;
