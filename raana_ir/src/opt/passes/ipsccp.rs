//! Implementation of *Interprocedural Sparse Condition Constant Propagation*
use rustc_hash::{FxHashMap, FxHashSet};

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
}

impl MemState {
    /// Folded value of a cell: meet of the writer contributions.
    fn cell_fold(&self, key: CellKey) -> Lattice {
        match self.cells.get(&key) {
            None => Lattice::Top,
            Some(writers) => writers
                .values()
                .fold(Lattice::Top, |acc, &v| acc.merge(v)),
        }
    }

    /// The value a load of `key` reads: the cell if any store wrote it,
    /// else a zero-range value if covered, else Bottom (unknown).
    fn read(&self, key: CellKey) -> Lattice {
        let cell = self.cell_fold(key);
        if cell != Lattice::Top {
            return cell;
        }
        let root = key.root();
        let covered = self
            .zero
            .get(&root)
            .is_some_and(|ranges| {
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
        let before = writers
            .values()
            .fold(Lattice::Top, |acc, &v| acc.merge(v));
        if writers.is_empty() {
            self.root_cells.entry(root).or_default().push(key);
        }
        writers.insert(writer, contribution);
        let after = writers
            .values()
            .fold(Lattice::Top, |acc, &v| acc.merge(v));
        before != after
    }

    /// Drop every cell and zero range of `root` (a call or an unknown
    /// destination may have overwritten anything).
    fn clear(&mut self, root: RootKey) -> bool {
        let mut changed = false;
        if let Some(keys) = self.root_cells.remove(&root) {
            for key in keys {
                self.cells.remove(&key);
            }
            changed = true;
        }
        changed |= self.zero.remove(&root).is_some();
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
    for root in roots {
        if state.clear(root) {
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
                        merge_and_extend(node, Lattice::Top, &mut lattice_map);
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
                                let roots = state
                                    .possible_targets(&analysis, func, store.dest(), &ctx);
                                for root in roots.unwrap_or_default() {
                                    if state.clear(root) {
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
                        let len = mem_zero.byte_len() as i64;
                        match resolve_cell(env, &ctx, func, mem_zero.dest()) {
                            Some((key, root)) => {
                                if state.mem_zero(root, key.offset(), len) {
                                    if let Some(loaders) = state.root_loaders.get(&root) {
                                        mem_reschedule.extend(loaders.iter().copied());
                                    }
                                }
                            }
                            None => {
                                let roots = state
                                    .possible_targets(&analysis, func, mem_zero.dest(), &ctx);
                                for root in roots.unwrap_or_default() {
                                    if state.clear(root) {
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
                        invalidate_call(
                            &analysis,
                            &mut state,
                            callee,
                            func,
                            &mut mem_reschedule,
                        );
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
                        invalidate_call(
                            &analysis,
                            &mut state,
                            callee,
                            func,
                            &mut mem_reschedule,
                        );
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
    (value.is_finite() && value >= i32::MIN as f32 && value < i32::MAX as f32).then(|| value as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{
            BinaryOp,
            builder::{
                BasicBlockBuilder, GlobalInstBuilder, LocalInstBuilder, ScalarInstBuilder,
            },
        },
        llvm::LlvmWriter,
        opt::pass::ArenaContextMut,
    };

    fn build_float_cast(value: f32) -> (Program, Function, Inst, Inst) {
        let mut program = Program::new();
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let data = program.func_data_mut(main);
        let entry = data.add_entry_block();
        let source = data.new_local_inst().float(value);
        let cast = data.new_local_inst().cast(source, Type::get_i32());
        let ret = data.new_local_inst().ret(Some(cast));
        data.layout_mut().insert_inst(entry, cast);
        data.layout_mut().insert_inst(entry, ret);
        (program, main, cast, ret)
    }

    fn assert_float_cast_folds(value: f32, expected: i32) {
        let (mut program, main, cast, ret) = build_float_cast(value);

        assert!(IPSCCP.run(&mut program));

        let data = program.func_data(main);
        assert!(matches!(
            data.inst_data(cast).kind(),
            InstKind::Integer(integer) if integer.value() == expected
        ));
        assert_eq!(data.layout().parent_bb(cast), None);
        let InstKind::Return(ret) = data.inst_data(ret).kind() else {
            panic!("expected return instruction")
        };
        assert_eq!(ret.value(), Some(cast));
    }

    #[test]
    fn folds_in_range_float_literals_to_i32() {
        assert_float_cast_folds(3.75, 3);
        assert_float_cast_folds(-2.75, -2);
        assert_float_cast_folds(i32::MIN as f32, i32::MIN);
    }

    #[test]
    fn keeps_non_finite_and_out_of_range_float_casts() {
        for value in [i32::MAX as f32, 1.0e10, f32::INFINITY, f32::NAN] {
            let (mut program, main, cast, _) = build_float_cast(value);

            assert!(!IPSCCP.run(&mut program));
            assert!(matches!(
                program.func_data(main).inst_data(cast).kind(),
                InstKind::Cast(..)
            ));
        }
    }

    /// Build a self-recursive tail-call function and verify that IPSCCP does
    /// not mis-propagate a parameter that varies across recursive tail calls.
    ///
    /// ```text
    /// fun(n: i32, dep: i32) -> i32:
    ///     if n == 0: return dep      // base case — dep must stay variable
    ///     else: tail_call fun(n - 1, dep + 1)
    /// main(): return fun(2, 0)
    /// ```
    ///
    /// With correct tail-call argument propagation, `dep` receives both
    /// `Constant(0)` (from main) and `Constant(1)` (from the recursive
    /// `dep+1`), converging to `Bottom`. Without the fix, IPSCCP would only
    /// see the non-tail call `fun(2, 0)` and constant-propagate `dep` to `0`,
    /// replacing the `ret dep` with `ret 0`.
    #[test]
    fn tail_call_args_prevent_constant_mispropagation() {
        let mut program = Program::new();

        // fun(n: i32, dep: i32) -> i32
        let fun = program.new_function(
            Type::get_i32(),
            "fun".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        let dep_param = {
            let data = program.func_data_mut(fun);
            let entry = data.add_entry_block();
            let params = data.bb_data(entry).params();
            let n = params[0];
            let dep = params[1];

            let base = data.new_basic_block().basic_block("base".into(), vec![]);
            let rec = data.new_basic_block().basic_block("rec".into(), vec![]);
            data.layout_mut().push_bb_back(base);
            data.layout_mut().push_bb_back(rec);

            // entry: br (n == 0), base, rec
            // (Integer constants are not placed in the layout — IPSCCP
            // initialises their lattice in Stage 0.1.)
            let zero = data.new_local_inst().integer(0);
            let cond = data.new_local_inst().binary(BinaryOp::Eq, n, zero);
            let br = data
                .new_local_inst()
                .branch(cond, base, vec![], rec, vec![]);
            data.layout_mut().insert_inst(entry, cond);
            data.layout_mut().insert_inst(entry, br);

            // base: ret dep
            let ret_dep = data.new_local_inst().ret(Some(dep));
            data.layout_mut().insert_inst(base, ret_dep);

            // rec: tail_call fun(n - 1, dep + 1)
            let one = data.new_local_inst().integer(1);
            let nm1 = data.new_local_inst().binary(BinaryOp::Sub, n, one);
            let one2 = data.new_local_inst().integer(1);
            let depp1 = data.new_local_inst().binary(BinaryOp::Add, dep, one2);
            let tc = data.new_local_inst().tail_call(fun, vec![nm1, depp1]);
            data.layout_mut().insert_inst(rec, nm1);
            data.layout_mut().insert_inst(rec, depp1);
            data.layout_mut().insert_inst(rec, tc);

            dep
        };

        // main() -> i32: return fun(2, 0)
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        {
            let data = program.func_data_mut(main);
            let entry = data.add_entry_block();
            let two = data.new_local_inst().integer(2);
            let zero = data.new_local_inst().integer(0);
            let call = data
                .new_local_inst()
                .call_with_type(fun, vec![two, zero], Type::get_i32());
            let ret = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(entry, call);
            data.layout_mut().insert_inst(entry, ret);
        }

        IPSCCP.run(&mut program);

        // After IPSCCP, `dep` must still be a BlockArgRef (not folded to an
        // Integer constant). If the tail-call arm were missing, dep's lattice
        // would be Constant(0) and `replace_inst_with` would have mutated its
        // data to Integer(0).
        let data = program.func_data(fun);
        assert!(
            matches!(data.inst_data(dep_param).kind(), InstKind::BlockArgRef(..)),
            "dep parameter was constant-propagated — tail-call arg propagation is broken"
        );
    }

    #[test]
    fn constant_block_param_replaces_uses_without_mutating_param() {
        let mut program = Program::new();
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let (param, ret) = {
            let data = program.func_data_mut(main);
            let entry = data.add_entry_block();
            let target = data
                .new_basic_block()
                .basic_block("target".into(), vec![Type::get_i32()]);
            data.layout_mut().push_bb_back(target);

            let seven = data.new_local_inst().integer(7);
            let jump = data.new_local_inst().jump(target, vec![seven]);
            data.layout_mut().insert_inst(entry, jump);

            let param = data.bb_data(target).params()[0];
            let ret = data.new_local_inst().ret(Some(param));
            data.layout_mut().insert_inst(target, ret);
            (param, ret)
        };

        assert!(IPSCCP.run(&mut program));

        let data = program.func_data(main);
        assert!(matches!(
            data.inst_data(param).kind(),
            InstKind::BlockArgRef(..)
        ));
        let InstKind::Return(ret_data) = data.inst_data(ret).kind() else {
            panic!("expected return instruction")
        };
        let value = ret_data.value().expect("return should have a value");
        assert!(matches!(data.inst_data(value).kind(), InstKind::Integer(int) if int.value() == 7));

        let mut writer = LlvmWriter::new(&program);
        writer.write().unwrap();
        let llvm = writer.finish();
        assert!(llvm.contains("phi i32 [ 7,"), "{llvm}");
        assert!(!llvm.contains("  7 = phi"), "{llvm}");
    }

    #[test]
    fn constant_function_param_replaces_uses_without_mutating_abi_param() {
        let mut program = Program::new();
        let callee = program.new_function(Type::get_i32(), "callee".into(), vec![Type::get_i32()]);
        let (param, ret) = {
            let data = program.func_data_mut(callee);
            let entry = data.add_entry_block();
            let param = data.params()[0];
            let ret = data.new_local_inst().ret(Some(param));
            data.layout_mut().insert_inst(entry, ret);
            (param, ret)
        };

        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        {
            let data = program.func_data_mut(main);
            let entry = data.add_entry_block();
            let seven = data.new_local_inst().integer(7);
            let call = data
                .new_local_inst()
                .call_with_type(callee, vec![seven], Type::get_i32());
            let ret = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(entry, call);
            data.layout_mut().insert_inst(entry, ret);
        }

        assert!(IPSCCP.run(&mut program));

        let data = program.func_data(callee);
        assert!(matches!(
            data.inst_data(param).kind(),
            InstKind::BlockArgRef(..)
        ));
        assert!(matches!(
            data.inst_data(data.params()[0]).kind(),
            InstKind::BlockArgRef(..)
        ));
        let InstKind::Return(ret_data) = data.inst_data(ret).kind() else {
            panic!("expected return instruction")
        };
        let value = ret_data.value().expect("return should have a value");
        assert!(matches!(data.inst_data(value).kind(), InstKind::Integer(int) if int.value() == 7));

        let mut writer = LlvmWriter::new(&program);
        writer.write().unwrap();
        let llvm = writer.finish();
        assert!(llvm.contains("define i32 @callee(i32 %"), "{llvm}");
        assert!(!llvm.contains("define i32 @callee(i32 7)"), "{llvm}");
    }

    // --- main-memory simulation (constant-offset cells) ---

    fn new_global(program: &mut Program) -> Inst {
        let init = program.new_value().zero_init(Type::get_i32());
        program.new_value().global_alloc(init)
    }

    #[test]
    fn folds_store_load_roundtrip_on_global_cell() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let (load, ret) = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(main),
            };
            let entry = data.add_entry_block();
            let zero = data.new_local_value().integer(0);
            let gep = data.new_local_value().get_elem_ptr(global, vec![zero]);
            data.layout_mut().insert_inst(entry, gep);
            let five = data.new_local_value().integer(5);
            let store = data.new_local_value().store(five, gep);
            data.layout_mut().insert_inst(entry, store);
            let load = data.new_local_value().load(gep);
            data.layout_mut().insert_inst(entry, load);
            let ret = data.new_local_value().ret(Some(load));
            data.layout_mut().insert_inst(entry, ret);
            (load, ret)
        };

        assert!(IPSCCP.run(&mut program));
        let data = program.func_data(main);
        assert!(matches!(
            data.inst_data(load).kind(),
            InstKind::Integer(integer) if integer.value() == 5
        ));
        let InstKind::Return(ret_data) = data.inst_data(ret).kind() else {
            panic!("expected return instruction")
        };
        assert_eq!(ret_data.value(), Some(load));
    }

    #[test]
    fn zero_initialized_global_load_folds_to_zero() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let (load, _ret) = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(main),
            };
            let entry = data.add_entry_block();
            let zero = data.new_local_value().integer(0);
            let gep = data.new_local_value().get_elem_ptr(global, vec![zero]);
            data.layout_mut().insert_inst(entry, gep);
            let load = data.new_local_value().load(gep);
            data.layout_mut().insert_inst(entry, load);
            let ret = data.new_local_value().ret(Some(load));
            data.layout_mut().insert_inst(entry, ret);
            (load, ret)
        };

        assert!(IPSCCP.run(&mut program));
        let data = program.func_data(main);
        assert!(matches!(
            data.inst_data(load).kind(),
            InstKind::Integer(integer) if integer.value() == 0
        ));
    }

    #[test]
    fn mem_zero_makes_local_array_loads_zero() {
        let mut program = Program::new();
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let (load, _ret) = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(main),
            };
            let entry = data.add_entry_block();
            let alloc = data
                .new_local_value()
                .alloc(Type::get_array(Type::get_i32(), 4));
            data.layout_mut().insert_inst(entry, alloc);
            let clear = data.new_local_value().mem_zero(alloc, 16);
            data.layout_mut().insert_inst(entry, clear);
            let zero = data.new_local_value().integer(0);
            let gep = data.new_local_value().get_elem_ptr(alloc, vec![zero]);
            data.layout_mut().insert_inst(entry, gep);
            let load = data.new_local_value().load(gep);
            data.layout_mut().insert_inst(entry, load);
            let ret = data.new_local_value().ret(Some(load));
            data.layout_mut().insert_inst(entry, ret);
            (load, ret)
        };

        assert!(IPSCCP.run(&mut program));
        let data = program.func_data(main);
        assert!(matches!(
            data.inst_data(load).kind(),
            InstKind::Integer(integer) if integer.value() == 0
        ));
    }

    #[test]
    fn call_to_writer_invalidates_global_cell() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        // The writer stores its (unknown) argument into the global.
        let writer = program.new_function(Type::get_unit(), "writer".into(), vec![Type::get_i32()]);
        {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(writer),
            };
            let entry = data.add_entry_block();
            let param = data.params()[0];
            let store = data.new_local_value().store(param, global);
            data.layout_mut().insert_inst(entry, store);
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(entry, ret);
        }
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        // An unknown value flows into the writer's parameter.
        let getint = program.new_function(Type::get_i32(), "getint".into(), vec![]);
        let (load, _ret) = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(main),
            };
            let entry = data.add_entry_block();
            let zero = data.new_local_value().integer(0);
            let gep = data.new_local_value().get_elem_ptr(global, vec![zero]);
            data.layout_mut().insert_inst(entry, gep);
            let five = data.new_local_value().integer(5);
            let store = data.new_local_value().store(five, gep);
            data.layout_mut().insert_inst(entry, store);
            let input = data.new_local_value().call(getint, vec![]);
            data.layout_mut().insert_inst(entry, input);
            let call = data.new_local_value().call(writer, vec![input]);
            data.layout_mut().insert_inst(entry, call);
            let load = data.new_local_value().load(gep);
            data.layout_mut().insert_inst(entry, load);
            let ret = data.new_local_value().ret(Some(load));
            data.layout_mut().insert_inst(entry, ret);
            (load, ret)
        };

        let _ = IPSCCP.run(&mut program);
        let data = program.func_data(main);
        // The writer may have overwritten the cell with an unknown value:
        // the load stays a load.
        assert!(matches!(data.inst_data(load).kind(), InstKind::Load(..)));
    }

    #[test]
    fn call_to_deterministic_writer_folds_load() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        // The writer stores a compile-time constant into the global.
        let writer = program.new_function(Type::get_unit(), "writer".into(), vec![]);
        {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(writer),
            };
            let entry = data.add_entry_block();
            let one = data.new_local_value().integer(1);
            let store = data.new_local_value().store(one, global);
            data.layout_mut().insert_inst(entry, store);
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(entry, ret);
        }
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let (load, _ret) = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(main),
            };
            let entry = data.add_entry_block();
            let zero = data.new_local_value().integer(0);
            let gep = data.new_local_value().get_elem_ptr(global, vec![zero]);
            data.layout_mut().insert_inst(entry, gep);
            let call = data.new_local_value().call(writer, vec![]);
            data.layout_mut().insert_inst(entry, call);
            let load = data.new_local_value().load(gep);
            data.layout_mut().insert_inst(entry, load);
            let ret = data.new_local_value().ret(Some(load));
            data.layout_mut().insert_inst(entry, ret);
            (load, ret)
        };

        assert!(IPSCCP.run(&mut program));
        let data = program.func_data(main);
        // The callee's constant store is modeled: the load folds to 1.
        assert!(matches!(
            data.inst_data(load).kind(),
            InstKind::Integer(integer) if integer.value() == 1
        ));
    }

    #[test]
    fn distinct_offsets_do_not_share_cells() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let (load, _ret) = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(main),
            };
            let entry = data.add_entry_block();
            let zero = data.new_local_value().integer(0);
            let one = data.new_local_value().integer(1);
            let gep0 = data.new_local_value().get_elem_ptr(global, vec![zero]);
            let gep1 = data.new_local_value().get_elem_ptr(global, vec![one]);
            data.layout_mut().insert_inst(entry, gep0);
            data.layout_mut().insert_inst(entry, gep1);
            let five = data.new_local_value().integer(5);
            let store = data.new_local_value().store(five, gep0);
            data.layout_mut().insert_inst(entry, store);
            let load = data.new_local_value().load(gep1);
            data.layout_mut().insert_inst(entry, load);
            let ret = data.new_local_value().ret(Some(load));
            data.layout_mut().insert_inst(entry, ret);
            (load, ret)
        };

        let _ = IPSCCP.run(&mut program);
        let data = program.func_data(main);
        // g[1] was never written: still a load.
        assert!(matches!(data.inst_data(load).kind(), InstKind::Load(..)));
    }

    #[test]
    fn divergent_stores_merge_to_bottom() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let (load, _ret) = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(main),
            };
            let entry = data.add_entry_block();
            let then_block = data.new_basic_block().basic_block("then".into(), vec![]);
            let else_block = data.new_basic_block().basic_block("else".into(), vec![]);
            let merge = data.new_basic_block().basic_block("merge".into(), vec![]);
            for block in [then_block, else_block, merge] {
                data.layout_mut().push_bb_back(block);
            }

            let zero = data.new_local_value().integer(0);
            let gep = data.new_local_value().get_elem_ptr(global, vec![zero]);
            data.layout_mut().insert_inst(entry, gep);
            let one = data.new_local_value().integer(1);
            let store_one = data.new_local_value().store(one, gep);
            data.layout_mut().insert_inst(entry, store_one);
            let cond = data.new_local_value().load(gep);
            data.layout_mut().insert_inst(entry, cond);
            let branch = data
                .new_local_value()
                .branch(cond, then_block, vec![], else_block, vec![]);
            data.layout_mut().insert_inst(entry, branch);

            let two = data.new_local_value().integer(2);
            let store_two = data.new_local_value().store(two, gep);
            data.layout_mut().insert_inst(else_block, store_two);
            let jump_t = data.new_local_value().jump(merge, vec![]);
            data.layout_mut().insert_inst(then_block, jump_t);
            let jump_e = data.new_local_value().jump(merge, vec![]);
            data.layout_mut().insert_inst(else_block, jump_e);

            let load = data.new_local_value().load(gep);
            data.layout_mut().insert_inst(merge, load);
            let ret = data.new_local_value().ret(Some(load));
            data.layout_mut().insert_inst(merge, ret);
            (load, ret)
        };

        let _ = IPSCCP.run(&mut program);
        let data = program.func_data(main);
        // Two different constant writers on different paths: Bottom, so the
        // load is not folded to either.
        assert!(matches!(data.inst_data(load).kind(), InstKind::Load(..)));
    }
}
