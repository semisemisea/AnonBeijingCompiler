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

impl Pass for IPSCCP {
    fn run(&mut self, program: &mut Program) -> bool {
        // Stage 0: Variables initialization.
        let mut edge_visited = EdgeSet::default();
        let mut node_visited = NodeSet::default();
        let mut edge_worklist: VecDeque<Edge> = VecDeque::default();
        let mut node_worklist: VecDeque<Node> = VecDeque::default();
        let mut lattice_map = LatticeMap::default();

        // Whole-program purity / alias analysis feeding the main-memory
        // simulation below (constant-offset cells on local and global
        // roots, zero ranges, per-call invalidation).
        let analysis = EffectAnalysis::new(program);
        let mut state = MemState::default();
        {
            // Zero-initialized globals answer constant-offset loads with 0
            // until a store overwrites them.
            let ctx = ArenaContext {
                program,
                curr_func: Some(program.get_main_function()),
            };
            for &g in program.global_inst_layout() {
                let InstKind::GlobalAlloc(global_alloc) = ctx.inst_data(g).kind() else {
                    continue;
                };
                if matches!(
                    ctx.inst_data(global_alloc.init()).kind(),
                    InstKind::ZeroInit
                ) {
                    let size = ctx.inst_data(g).ty().derefernce().size() as i64;
                    let root = RootKey::Global(g);
                    state.all_roots.insert(root);
                    merge_zero_interval(state.zero.entry(root).or_default(), 0, size);
                }
            }
        }

        // Stage 0.1
        // Default: Set every integer to const, float to bottom, else remain top.
        for &func in program.function_layout() {
            let arena = ArenaContext {
                program,
                curr_func: Some(func),
            };
            for (&inst, data) in arena.inst_datas() {
                let node = Node::new(func, inst);
                match data.kind() {
                    InstKind::Integer(int) => lattice_map.new_const(node, int.value()),
                    InstKind::Float(..) => lattice_map.new_var(node),
                    _ => {}
                }
            }
        }

        // TODO: What about global value?

        // Stage 0.2
        // Build ICFG. Ready to start the worklist algorithm.
        let icfg = icfg::ICFG::new(program);
        let main_func = program.get_main_function();
        let entry_bb_layout = program.func_data(main_func).layout().entry_bb().unwrap();
        let first_inst = *entry_bb_layout.insts().get_first().unwrap();
        let start_node = Node::new(main_func, first_inst);

        // A virtual edge to start the loop
        let start_edge = Edge {
            edge_type: EdgeType::Normal,
            src: start_node,
            dst: start_node,
        };
        edge_worklist.push_back(start_edge);
        edge_visited.insert(start_edge);
        let mut block_visited = FxHashSet::default();

        // Stage 1: Worklist algorithm.
        while !edge_worklist.is_empty() || !node_worklist.is_empty() {
            if let Some(edge) = edge_worklist.pop_front() {
                let Edge { dst, .. } = edge;
                let parent_bb = program
                    .func_data(dst.func)
                    .layout()
                    .parent_bb(dst.inst)
                    .unwrap();
                block_visited.insert(Block::new(dst.func, parent_bb));
                // first time visit the node
                if node_visited.insert(dst) {
                    node_worklist.push_back(dst);
                }
            }

            // Tail-call relay nodes whose lattice was updated by the Return arm
            // need re-scheduling so their TailCall arm can forward the callee's
            // return value upward. They cannot be pushed to `node_worklist`
            // directly inside the node-processing block because the closures
            // (`merge_and_extend` et al.) hold a mutable borrow of it; collect
            // them here and drain after those closures are dropped.
            let mut relay_targets: Vec<Node> = Vec::new();
            // Loads whose memory cells changed (stores, MemZero, calls) are
            // re-scheduled the same way.
            let mut mem_reschedule: Vec<Node> = Vec::new();

            if let Some(node) = node_worklist.pop_front() {
                let push_edge = |edge: Edge| {
                    edge_visited
                        .insert(edge)
                        .then(|| edge_worklist.push_back(edge));
                };
                let Node { func, inst } = node;
                let data = program.func_data(func);
                let mut extend_affected_node_used_by = |node: Node| {
                    let data = program.func_data(node.func);
                    node_worklist.extend(
                        data.inst_data(node.inst)
                            .used_by()
                            .iter()
                            .filter(|&&inst| {
                                data.layout().parent_bb(inst).is_some_and(|b| {
                                    block_visited.contains(&Block::new(node.func, b))
                                })
                            })
                            .map(|&inst| Node::new(node.func, inst)),
                    );
                };
                let mut merge_and_extend =
                    |node: Node, status: Lattice, lattice_map: &mut LatticeMap| -> bool {
                        if lattice_map.insert_or_update(node, status) {
                            extend_affected_node_used_by(node);
                            true
                        } else {
                            false
                        }
                    };
                match data.inst_data(inst).kind() {
                    // These instruction define a scalar and !never! appear in the layout
                    InstKind::Aggregate(..)
                    | InstKind::GlobalAlloc(..)
                    | InstKind::Undef
                    | InstKind::ZeroInit
                    | InstKind::Integer(..)
                    | InstKind::Float(..)
                    | InstKind::BlockArgRef(..) => unreachable!(
                        "instruction {:?} with data {:?} should never appear in the layout",
                        inst,
                        data.inst_data(inst)
                    ),
                    // Addresses are not i32 constants; keep them variable.
                    InstKind::GetElemPtr(..) | InstKind::Alloc => {
                        merge_and_extend(node, Lattice::Bottom, &mut lattice_map);
                    }
                    // Loads read the simulated memory: a constant-offset
                    // cell on a local/global root folds to the stored value
                    // (or 0 under a zero range); everything else is
                    // conservatively Bottom.
                    InstKind::Load(load) => {
                        let env = analysis.env_of(func);
                        let ctx = ArenaContext {
                            program,
                            curr_func: Some(func),
                        };
                        match resolve_cell(env, &ctx, func, load.src()) {
                            Some((key, root)) => {
                                let value = state.read(key);
                                // Overwrite: the load mirrors the current
                                // memory snapshot, not a meet of historical
                                // snapshots.
                                if lattice_map.insert_or_replace(node, value) {
                                    extend_affected_node_used_by(node);
                                }
                                // Register the load for re-scheduling when
                                // its root changes. Deduplicate: a load that
                                // is (re)processed many times must not grow
                                // the loader list unboundedly.
                                let loaders = state.root_loaders.entry(root).or_default();
                                if !loaders.contains(&node) {
                                    loaders.push(node);
                                }
                            }
                            None => {
                                merge_and_extend(node, Lattice::Bottom, &mut lattice_map);
                            }
                        }
                    }
                    InstKind::Binary(binary) => {
                        let status = match (
                            lattice_map.get(Node::new(func, binary.lhs())),
                            lattice_map.get(Node::new(func, binary.rhs())),
                        ) {
                            (Lattice::Bottom, _) | (_, Lattice::Bottom) => Lattice::Bottom,
                            (Lattice::Constant(lhs), Lattice::Constant(rhs)) => {
                                Lattice::Constant(mathematic_operation(binary.op(), lhs, rhs))
                            }
                            _ => Lattice::Top,
                        };
                        merge_and_extend(node, status, &mut lattice_map);
                    }
                    InstKind::Select(select) => {
                        let cond = select.cond();
                        let status_of = |node: Node| lattice_map.get(node);
                        let status = match status_of(Node::new(func, cond)) {
                            Lattice::Top => Lattice::Top,
                            Lattice::Constant(constant) => status_of(Node::new(
                                func,
                                if constant != 0 {
                                    select.if_true()
                                } else {
                                    select.if_false()
                                },
                            )),
                            Lattice::Bottom => status_of(Node::new(func, select.if_true()))
                                .merge(status_of(Node::new(func, select.if_false()))),
                        };
                        merge_and_extend(node, status, &mut lattice_map);
                    }
                    // Vector operations never carry an i32 constant lattice; the
                    // frontend (and any future vectorizer) emits them on vector
                    // operands, which are always Top here.
                    InstKind::Fma(..)
                    | InstKind::VectorSplat(..)
                    | InstKind::VectorExtractElement(..)
                    | InstKind::VectorInsertElement(..)
                    | InstKind::VectorReduce(..) => {
                        // These vector ops cannot be constant-folded, but
                        // their results are *definitely* not constants.
                        // Marking them Top (undef) lets the optimistic SCCP
                        // merge fold a loop block-param to the entry edge's
                        // constant (e.g. `sum` -> 0 for `sum += c[i][j]`
                        // nested loops), silently zeroing the accumulator.
                        // Bottom (overdefined) is the correct lattice value.
                        merge_and_extend(node, Lattice::Bottom, &mut lattice_map);
                    }
                    InstKind::Jump(jump) => {
                        let params = data.bb_data(jump.target()).params();
                        for (&arg, &param) in jump.args().iter().zip(params) {
                            let arg_status = lattice_map.get(Node::new(func, arg));
                            merge_and_extend(Node::new(func, param), arg_status, &mut lattice_map);
                        }
                    }
                    InstKind::Branch(branch) => {
                        let cond = branch.cond();
                        let cond_status = lattice_map.get(Node::new(func, cond));
                        let worklist = match cond_status {
                            Lattice::Top => [None, None],
                            Lattice::Bottom => [
                                Some((branch.t_target(), branch.t_args())),
                                Some((branch.f_target(), branch.f_args())),
                            ],
                            Lattice::Constant(c) => {
                                if c != 0 {
                                    [Some((branch.t_target(), branch.t_args())), None]
                                } else {
                                    [Some((branch.f_target(), branch.f_args())), None]
                                }
                            }
                        };
                        for (target, args) in worklist.into_iter().flatten() {
                            let params = data.bb_data(target).params();
                            for (&arg, &param) in args.iter().zip(params) {
                                let arg_status = lattice_map.get(Node::new(func, arg));
                                merge_and_extend(
                                    Node::new(func, param),
                                    arg_status,
                                    &mut lattice_map,
                                );
                            }
                        }
                    }
                    InstKind::Cast(cast) => {
                        let status = if data.inst_data(inst).ty().is_i32() {
                            match data.inst_data(cast.src()).kind() {
                                InstKind::Float(float) => fold_f32_to_i32(float.value())
                                    .map_or(Lattice::Bottom, Lattice::Constant),
                                _ => Lattice::Bottom,
                            }
                        } else {
                            Lattice::Bottom
                        };
                        merge_and_extend(node, status, &mut lattice_map);
                    }
                    // Stores write into the simulated memory: a resolvable
                    // constant-offset cell records the source lattice; an
                    // unresolvable destination clears everything it may
                    // target (via the points-to analysis).
                    InstKind::Store(store) => {
                        let env = analysis.env_of(func);
                        let ctx = ArenaContext {
                            program,
                            curr_func: Some(func),
                        };
                        let value = lattice_map.get(Node::new(func, store.src()));
                        match resolve_cell(env, &ctx, func, store.dest()) {
                            Some((key, root)) => {
                                if state.write(key, inst, value) {
                                    if let Some(loaders) = state.root_loaders.get(&root) {
                                        mem_reschedule.extend(loaders.iter().copied());
                                    }
                                }
                            }
                            None => {
                                let roots =
                                    state.possible_targets(&analysis, func, store.dest(), &ctx);
                                for root in roots.unwrap_or_default() {
                                    if state.clear(root, true) {
                                        if let Some(loaders) = state.root_loaders.get(&root) {
                                            mem_reschedule.extend(loaders.iter().copied());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    InstKind::MemZero(mem_zero) => {
                        let env = analysis.env_of(func);
                        let ctx = ArenaContext {
                            program,
                            curr_func: Some(func),
                        };
                        // A runtime-length MemZero (M53 zero-store loops)
                        // covers an unknown range: conservatively invalidate
                        // the whole root instead of folding a zero interval.
                        let len = match mem_zero.byte_len_len() {
                            MemZeroLen::Const(n) => Some(*n as i64),
                            MemZeroLen::Value(_) => None,
                        };
                        match (resolve_cell(env, &ctx, func, mem_zero.dest()), len) {
                            (Some((key, root)), Some(len)) => {
                                if state.mem_zero(root, key.offset(), len) {
                                    if let Some(loaders) = state.root_loaders.get(&root) {
                                        mem_reschedule.extend(loaders.iter().copied());
                                    }
                                }
                            }
                            (Some((_, root)), None) => {
                                if state.clear(root, true) {
                                    if let Some(loaders) = state.root_loaders.get(&root) {
                                        mem_reschedule.extend(loaders.iter().copied());
                                    }
                                }
                            }
                            (None, _) => {
                                let roots =
                                    state.possible_targets(&analysis, func, mem_zero.dest(), &ctx);
                                for root in roots.unwrap_or_default() {
                                    if state.clear(root, true) {
                                        if let Some(loaders) = state.root_loaders.get(&root) {
                                            mem_reschedule.extend(loaders.iter().copied());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    InstKind::TailCall(tail_call) => {
                        // A tail call transfers control to the callee just like a
                        // regular call, so its actual arguments must flow into the
                        // callee's formal parameters. Without this, the only
                        // argument source IPSCCP would see is the (non-tail) call
                        // site, causing parameters that vary across recursive tail
                        // calls to be mis-propagated as constants.
                        let callee = tail_call.callee();
                        let callee_data = program.func_data(callee);
                        if !callee_data.layout().is_decl() {
                            for (&arg, &param) in tail_call.args().iter().zip(callee_data.params())
                            {
                                let arg_status = lattice_map.get(Node::new(func, arg));
                                merge_and_extend(
                                    Node::new(callee, param),
                                    arg_status,
                                    &mut lattice_map,
                                );
                            }
                        }
                        // Relay: a tail call forwards the callee's return value
                        // directly to the current function's caller (the frame is
                        // reused). The callee's Return arm deposits its return value
                        // into *this* node's lattice (see the Return arm below);
                        // propagate it further along this node's outgoing Return
                        // edges, which connect to the caller's call site.
                        let node_status = lattice_map.get(node);
                        for Edge { dst, edge_type, .. } in icfg.outgoing_edges_of(node) {
                            if edge_type != EdgeType::Return {
                                continue;
                            }
                            if let Some(cs) = icfg.call_site_before(dst) {
                                merge_and_extend(
                                    Node::new(dst.func, cs.call),
                                    node_status,
                                    &mut lattice_map,
                                );
                            }
                        }
                        // The tail callee may write memory; invalidate the
                        // cells it can reach.
                        invalidate_call(&analysis, &mut state, callee, func, &mut mem_reschedule);
                    }
                    InstKind::Call(call) => {
                        let callee = call.callee();
                        let callee_data = program.func_data(callee);
                        if callee_data.layout().is_decl() {
                            merge_and_extend(node, Lattice::Bottom, &mut lattice_map);
                        } else {
                            for (&arg, &param) in call.args().iter().zip(callee_data.params()) {
                                let arg_status = lattice_map.get(Node::new(func, arg));
                                merge_and_extend(
                                    Node::new(callee, param),
                                    arg_status,
                                    &mut lattice_map,
                                );
                            }
                        }
                        // The callee may write memory; invalidate the cells
                        // it can reach (unknown writers clear everything).
                        invalidate_call(&analysis, &mut state, callee, func, &mut mem_reschedule);
                    }
                    InstKind::Return(ret) => {
                        if let Some(ret_val) = ret.value() {
                            let ret_val_status = lattice_map.get(Node::new(func, ret_val));
                            let outgoing_edges = icfg.outgoing_edges_of(node);
                            for Edge { dst, .. } in outgoing_edges {
                                // For a regular call, the Return edge lands at the
                                // call's continuation and `call_site_before` resolves
                                // the call site whose `.call` node receives the value.
                                // For a tail call, the edge lands at the tail-call
                                // instruction itself (the relay node), which is
                                // deliberately absent from `callsite_by_continuation`
                                // — deposit the value directly into that node.
                                let target = match icfg.call_site_before(dst) {
                                    Some(cs) => Node::new(dst.func, cs.call),
                                    None => dst,
                                };
                                if merge_and_extend(target, ret_val_status, &mut lattice_map) {
                                    // The relay node's lattice was set externally
                                    // (by us, not by its own evaluation), so it will
                                    // not be revisited through `used_by`. Re-schedule
                                    // it so its TailCall arm can forward the value
                                    // upward along the tail-call chain.
                                    if matches!(
                                        program
                                            .func_data(target.func)
                                            .inst_data(target.inst)
                                            .kind(),
                                        InstKind::TailCall(..)
                                    ) {
                                        relay_targets.push(target);
                                    }
                                }
                            }
                        }
                    }
                }
                // update edges.
                match data.inst_data(inst).kind() {
                    InstKind::Branch(branch) => {
                        let cond = branch.cond();
                        let cond_status = lattice_map.get(Node::new(func, cond));
                        let construct_edge = |block| Edge {
                            edge_type: EdgeType::Normal,
                            src: Node::new(func, inst),
                            dst: Node::new(
                                func,
                                *data.layout().basicblock(block).insts().get_first().unwrap(),
                            ),
                        };
                        let edges = match cond_status {
                            Lattice::Top => [None, None],
                            Lattice::Bottom => [Some(branch.t_target()), Some(branch.f_target())],
                            Lattice::Constant(constant) => {
                                if constant != 0 {
                                    [Some(branch.t_target()), None]
                                } else {
                                    [Some(branch.f_target()), None]
                                }
                            }
                        };
                        edges
                            .into_iter()
                            .flatten()
                            .map(construct_edge)
                            .for_each(push_edge);
                    }
                    InstKind::Return(..) => {}
                    InstKind::TailCall(..) => {
                        // Push the Call edge so the callee's entry becomes
                        // reachable. Return edges are consumed by the relay logic
                        // in the TailCall lattice arm, not pushed here.
                        icfg.outgoing_edges_of(Node::new(func, inst))
                            .filter(|e| e.edge_type == EdgeType::Call)
                            .for_each(push_edge);
                    }
                    _ => {
                        icfg.outgoing_edges_of(Node::new(func, inst))
                            .for_each(push_edge);
                    }
                }
            }
            // Closures borrowing `node_worklist` are now dropped; safe to extend.
            node_worklist.extend(relay_targets.drain(..));
            node_worklist.extend(mem_reschedule.drain(..));
        }

        let mut changed = false;

        let const_replace_list = lattice_map
            .0
            .iter()
            .filter_map(|(&node, &lattice)| {
                if let Lattice::Constant(c) = lattice {
                    Some((node, c))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        for (node, val) in const_replace_list {
            let mut arena = ArenaContextMut {
                program,
                curr_func: Some(node.func),
            };
            if matches!(
                arena.inst_data(node.inst).kind(),
                InstKind::Call(..) | InstKind::BlockArgRef(..) | InstKind::TailCall(..)
            ) {
                let has_uses = !arena.inst_data(node.inst).used_by().is_empty();
                if !has_uses {
                    continue;
                }
                let integer = arena.new_local_inst().integer(val);
                visit_and_replace(&mut arena, node.inst, integer);
                changed = true;
            } else {
                arena.replace_inst_with(node.inst).integer(val);
                let data = program.func_data_mut(node.func);
                let Some(parent_bb) = data.layout().parent_bb(node.inst) else {
                    continue;
                };
                data.detach_layout_inst(parent_bb, node.inst);
                changed = true;
            }
        }

        let mut useless_conditional_branch = vec![];
        for &func in program.function_layout() {
            let data = program.func_data(func);
            for bb_layout in program.func_data(func).layout().basicblocks() {
                let terminator = *bb_layout.insts().get_last().unwrap();
                if let InstKind::Branch(branch) = data.inst_data(terminator).kind() {
                    if let InstKind::Integer(..) = data.inst_data(branch.cond()).kind() {
                        useless_conditional_branch.push(Node::new(func, terminator));
                    }
                }
            }
        }

        for node in useless_conditional_branch {
            let Node { func, inst } = node;
            let data = program.func_data_mut(func);
            let InstKind::Branch(branch) = data.inst_data(inst).kind() else {
                unreachable!()
            };
            let InstKind::Integer(int) = data.inst_data(branch.cond()).kind() else {
                unreachable!()
            };
            let (target, args) = if int.value() == 0 {
                (branch.f_target(), branch.f_args().to_vec())
            } else {
                (branch.t_target(), branch.t_args().to_vec())
            };
            data.replace_inst_with(inst).jump(target, args);
            changed = true;
        }

        let mut remove_list = vec![];
        for &func in program.function_layout() {
            let data = program.func_data(func);
            remove_list.extend(
                data.layout()
                    .basicblocks()
                    .iter()
                    .map(|l| l.bb())
                    .filter(|&bb| {
                        bb != data.layout().entry_bb().unwrap().bb()
                            && data.bb_data(bb).used_by().is_empty()
                            // A block may have become unreachable while its
                            // non-terminator instructions still feed values
                            // into reachable blocks (e.g. LICM-hoisted GEPs
                            // used by a surviving loop body). Removing it then
                            // destroys live values and leaves dangling
                            // operands. Only remove blocks whose every
                            // instruction is itself unused.
                            && data
                                .layout()
                                .basicblock(bb)
                                .insts()
                                .iter()
                                .all(|&inst| data.inst_data(inst).used_by().is_empty())
                    })
                    .map(|bb| Block::new(func, bb))
                    .collect::<Vec<_>>(),
            )
        }
        for bb in remove_list {
            let data = program.func_data_mut(bb.func);
            data.remove_layout_basicblock(bb.block);
            changed = true;
        }

        changed
    }
}

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
