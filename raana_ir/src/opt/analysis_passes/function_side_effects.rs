//! Multi-dimensional function side-effect (purity) analysis.
//!
//! SysY has no general pointers (only array parameters degrade to pointers),
//! so a function's externally observable side effects are confined to:
//! reading/writing global variables, reading/writing array parameters,
//! performing I/O (getint/putint/...), and touching the timer
//! (`_sysy_starttime`/`_sysy_stoptime`).
//!
//! Instead of a coarse pure/impure flag, [`FunctionSideEffects`] records each
//! dimension separately; consumers (DCE, LICM, GVN, ...) can then ask
//! precisely scoped questions such as "does this call write global `g`?" or
//! "is this call free of I/O and memory writes?".
//!
//! The analysis is a bottom-up inter-procedural fixed-point:
//!   1. library functions get hardcoded attributes (leaf nodes);
//!   2. each user function is seeded with its direct (intra-procedural)
//!      effects by resolving every Load/Store/MemZero address to its memory
//!      base (global alloc / array parameter / local alloc / unknown);
//!   3. the call graph is condensed into SCCs (Tarjan) and processed
//!      bottom-up (callees before callers);
//!   4. effects are propagated across call sites with a precise actual-arg
//!      to formal-param mapping; recursive SCCs iterate to a fixed point.
//!
//! The mapping is what keeps the analysis precise: when callee touches its
//! `i`-th array parameter, the caller's actual argument is resolved. A local
//! allocation does not escape, so the effect is *not* propagated upward.

use crate::ir::arena::GlobalArena;
use crate::opt::prelude::*;

/// External side effects of one function.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FunctionSideEffects {
    /// Performs I/O directly or transitively (getint/getarray/putf/...).
    pub has_io_side_effects: bool,
    /// Calls `_sysy_starttime`/`_sysy_stoptime` directly or transitively.
    pub has_timer_effects: bool,
    /// Global variables (GlobalAlloc handles) this function may read.
    pub read_globals: HashSet<Inst>,
    /// Global variables (GlobalAlloc handles) this function may write.
    pub written_globals: HashSet<Inst>,
    /// Array parameters (by formal index) this function may read.
    pub read_array_params: HashSet<usize>,
    /// Array parameters (by formal index) this function may write.
    pub written_array_params: HashSet<usize>,
    /// A read through an address whose base could not be resolved to a global,
    /// a formal parameter, or a local allocation. Conservatively treated as a
    /// read of any global and any array parameter.
    pub reads_unknown_memory: bool,
    /// Same as [`Self::reads_unknown_memory`], for writes.
    pub writes_unknown_memory: bool,
}

impl FunctionSideEffects {
    /// Strictly pure: no I/O, no timer, no reads/writes of globals or array
    /// parameters. The return value is a pure function of scalar arguments
    /// (and local state).
    pub fn is_strictly_pure(&self) -> bool {
        !self.has_io_side_effects
            && !self.has_timer_effects
            && !self.reads_unknown_memory
            && !self.writes_unknown_memory
            && self.read_globals.is_empty()
            && self.written_globals.is_empty()
            && self.read_array_params.is_empty()
            && self.written_array_params.is_empty()
    }

    /// Read-only: no I/O, no timer, never writes any external memory; reads
    /// of globals and array parameters are allowed.
    pub fn is_read_only(&self) -> bool {
        !self.has_io_side_effects
            && !self.has_timer_effects
            && !self.writes_unknown_memory
            && self.written_globals.is_empty()
            && self.written_array_params.is_empty()
    }

    /// All memory effects (if any) are confined to array parameters: no
    /// global is touched, no unknown memory is touched, no I/O or timer.
    pub fn is_arg_mem_only(&self) -> bool {
        !self.has_io_side_effects
            && !self.has_timer_effects
            && !self.reads_unknown_memory
            && !self.writes_unknown_memory
            && self.read_globals.is_empty()
            && self.written_globals.is_empty()
    }

    /// May this function read the given global (via its GlobalAlloc handle)?
    pub fn may_read_global(&self, global: Inst) -> bool {
        self.reads_unknown_memory || self.read_globals.contains(&global)
    }

    /// May this function write the given global (via its GlobalAlloc handle)?
    pub fn may_write_global(&self, global: Inst) -> bool {
        self.writes_unknown_memory || self.written_globals.contains(&global)
    }

    /// May this function read the caller-visible array parameter `index`?
    pub fn may_read_array_param(&self, index: usize) -> bool {
        self.reads_unknown_memory || self.read_array_params.contains(&index)
    }

    /// May this function write the caller-visible array parameter `index`?
    pub fn may_write_array_param(&self, index: usize) -> bool {
        self.writes_unknown_memory || self.written_array_params.contains(&index)
    }

    /// Does this function write any external memory at all (a global, an
    /// array parameter, or an unresolved address)?
    pub fn has_external_writes(&self) -> bool {
        self.writes_unknown_memory
            || !self.written_globals.is_empty()
            || !self.written_array_params.is_empty()
    }

    /// Does this function read any external memory at all (a global, an array
    /// parameter, or an unresolved address)?
    pub fn has_any_read(&self) -> bool {
        self.reads_unknown_memory
            || !self.read_globals.is_empty()
            || !self.read_array_params.is_empty()
    }

    /// May the memory written by `self` (the writer) alias something `reader`
    /// reads? Used to decide whether a read-only callee's loads stay stable
    /// across a region containing `self` as a write (store/call).
    pub fn may_conflict_with_reads_of(&self, reader: &FunctionSideEffects) -> bool {
        if self.writes_unknown_memory && reader.has_any_read() {
            return true;
        }
        if reader.reads_unknown_memory
            && (!self.written_globals.is_empty() || !self.written_array_params.is_empty())
        {
            return true;
        }
        self.written_globals
            .iter()
            .any(|g| reader.read_globals.contains(g))
            || self
                .written_array_params
                .iter()
                .any(|i| reader.read_array_params.contains(i))
    }
}

/// Hardcoded side effects of the SysY runtime library functions. These are
/// frontend-declared leaves with no body; they are identified by name plus
/// the absence of an entry block, so a user function cannot be mistaken for
/// one even if it shares the name.
fn library_effects(name: &str) -> Option<FunctionSideEffects> {
    let mut effects = FunctionSideEffects::default();
    match name {
        "getint" | "getch" | "getfloat" => effects.has_io_side_effects = true,
        "getarray" | "getfarray" => {
            effects.has_io_side_effects = true;
            effects.written_array_params.insert(0);
        }
        "putint" | "putch" | "putfloat" => effects.has_io_side_effects = true,
        "putarray" | "putfarray" => {
            effects.has_io_side_effects = true;
            effects.read_array_params.insert(1);
        }
        "putf" => effects.has_io_side_effects = true,
        "_sysy_starttime" | "_sysy_stoptime" => effects.has_timer_effects = true,
        _ => return None,
    }
    Some(effects)
}

/// Where a pointer value ultimately points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoryBase {
    /// A global allocation (`InstKind::GlobalAlloc`).
    Global(Inst),
    /// The `index`-th formal parameter (an array degraded to a pointer).
    Param(usize),
    /// A local stack allocation (`InstKind::Alloc`); not externally visible.
    Local,
    /// Could not be resolved; conservatively treat as any memory.
    Unknown,
}

/// Resolve a pointer value to its memory base by following the GEP base
/// chain. SysY has no pointer casts or pointer loads, so the chain is a
/// single GEP hop at most in practice, but the loop keeps it robust.
pub(crate) fn resolve_base(data: &FunctionData, global: &GlobalArena, inst: Inst) -> MemoryBase {
    let mut cursor = inst;
    loop {
        if cursor.is_global() {
            return match global.inst_arena().data_of(cursor).kind() {
                InstKind::GlobalAlloc(..) => MemoryBase::Global(cursor),
                _ => MemoryBase::Unknown,
            };
        }
        // Function parameters are the entry block's parameters; comparing
        // handles identifies which formal index the pointer came from.
        if let Some(index) = data.params().iter().position(|&p| p == cursor) {
            return MemoryBase::Param(index);
        }
        match data.inst_data(cursor).kind() {
            InstKind::GetElemPtr(gep) => cursor = gep.base(),
            InstKind::Alloc => return MemoryBase::Local,
            _ => return MemoryBase::Unknown,
        }
    }
}

/// Record one resolved memory access into `effects`.
pub(crate) fn record_access(effects: &mut FunctionSideEffects, base: MemoryBase, is_write: bool) {
    let (globals, params, unknown) = if is_write {
        (
            &mut effects.written_globals,
            &mut effects.written_array_params,
            &mut effects.writes_unknown_memory,
        )
    } else {
        (
            &mut effects.read_globals,
            &mut effects.read_array_params,
            &mut effects.reads_unknown_memory,
        )
    };
    match base {
        MemoryBase::Global(g) => {
            globals.insert(g);
        }
        MemoryBase::Param(i) => {
            params.insert(i);
        }
        MemoryBase::Local => {}
        MemoryBase::Unknown => *unknown = true,
    }
}

/// All call sites in a function: `(callee, actual arguments)`.
fn calls_in(data: &FunctionData) -> Vec<(Function, Vec<Inst>)> {
    let mut calls = Vec::new();
    for layout in data.layout().basicblocks() {
        for &inst in layout.insts() {
            match data.inst_data(inst).kind() {
                InstKind::Call(call) => calls.push((call.callee(), call.args().to_vec())),
                InstKind::TailCall(tail_call) => {
                    calls.push((tail_call.callee(), tail_call.args().to_vec()))
                }
                _ => {}
            }
        }
    }
    calls
}

/// May a write through `base` hit something `reader` reads? Shared by LICM
/// (loop writes vs hoisted callee reads) and GVN (call CSE invalidation).
pub(crate) fn write_base_may_hit(base: MemoryBase, reader: &FunctionSideEffects) -> bool {
    match base {
        MemoryBase::Global(g) => reader.may_read_global(g),
        MemoryBase::Param(i) => reader.may_read_array_param(i),
        MemoryBase::Local => false,
        MemoryBase::Unknown => reader.has_any_read(),
    }
}

/// Intra-procedural collection: the effects a function produces on its own,
/// without considering calls it makes.
fn collect_direct(data: &FunctionData, global: &GlobalArena) -> FunctionSideEffects {
    let mut effects = FunctionSideEffects::default();
    for layout in data.layout().basicblocks() {
        for &inst in layout.insts() {
            match data.inst_data(inst).kind() {
                InstKind::Load(load) => {
                    let base = resolve_base(data, global, load.src());
                    record_access(&mut effects, base, false);
                }
                InstKind::Store(store) => {
                    let base = resolve_base(data, global, store.dest());
                    record_access(&mut effects, base, true);
                }
                InstKind::MemZero(mem_zero) => {
                    let base = resolve_base(data, global, mem_zero.dest());
                    record_access(&mut effects, base, true);
                }
                _ => {}
            }
        }
    }
    effects
}

/// Propagate `callee`'s effects into `caller` for one call site, mapping the
/// callee's array-parameter effects through the actual arguments.
fn propagate_call(
    caller: &mut FunctionSideEffects,
    callee: &FunctionSideEffects,
    args: &[Inst],
    data: &FunctionData,
    global: &GlobalArena,
) {
    caller.has_io_side_effects |= callee.has_io_side_effects;
    caller.has_timer_effects |= callee.has_timer_effects;
    caller.reads_unknown_memory |= callee.reads_unknown_memory;
    caller.writes_unknown_memory |= callee.writes_unknown_memory;
    caller
        .read_globals
        .extend(callee.read_globals.iter().copied());
    caller
        .written_globals
        .extend(callee.written_globals.iter().copied());

    for &i in &callee.read_array_params {
        if let Some(&arg) = args.get(i) {
            let base = resolve_base(data, global, arg);
            record_access(caller, base, false);
        }
    }
    for &i in &callee.written_array_params {
        if let Some(&arg) = args.get(i) {
            let base = resolve_base(data, global, arg);
            record_access(caller, base, true);
        }
    }
}

/// Tarjan's strongly connected components over the user-function call graph.
struct SccFinder<'a> {
    callees: &'a HashMap<Function, Vec<Function>>,
    index: usize,
    indices: HashMap<Function, usize>,
    lowlinks: HashMap<Function, usize>,
    stack: Vec<Function>,
    on_stack: HashSet<Function>,
    sccs: Vec<Vec<Function>>,
}

impl SccFinder<'_> {
    fn strongconnect(&mut self, func: Function) {
        self.indices.insert(func, self.index);
        self.lowlinks.insert(func, self.index);
        self.index += 1;
        self.stack.push(func);
        self.on_stack.insert(func);

        for &callee in self.callees.get(&func).into_iter().flatten() {
            if !self.indices.contains_key(&callee) {
                self.strongconnect(callee);
                let low = self.lowlinks[&func].min(self.lowlinks[&callee]);
                self.lowlinks.insert(func, low);
            } else if self.on_stack.contains(&callee) {
                let low = self.lowlinks[&func].min(self.indices[&callee]);
                self.lowlinks.insert(func, low);
            }
        }

        if self.lowlinks[&func] == self.indices[&func] {
            let mut component = Vec::new();
            loop {
                let member = self.stack.pop().unwrap();
                self.on_stack.remove(&member);
                component.push(member);
                if member == func {
                    break;
                }
            }
            self.sccs.push(component);
        }
    }
}

/// Post-order over the SCC condensation DAG: every SCC appears after all the
/// SCCs it calls, so processing in this order is bottom-up (callees first).
fn scc_post_order(
    scc_count: usize,
    scc_id: &HashMap<Function, usize>,
    scc_edges: &[Vec<usize>],
) -> Vec<usize> {
    let mut visited = vec![false; scc_count];
    let mut order = Vec::with_capacity(scc_count);
    fn dfs(node: usize, scc_edges: &[Vec<usize>], visited: &mut [bool], order: &mut Vec<usize>) {
        if visited[node] {
            return;
        }
        visited[node] = true;
        for &callee in &scc_edges[node] {
            dfs(callee, scc_edges, visited, order);
        }
        order.push(node);
    }
    for node in 0..scc_count {
        dfs(node, scc_edges, &mut visited, &mut order);
    }
    order
}

/// Run the inter-procedural side-effect analysis over every defined (user)
/// function and return the resulting per-function attributes. Library
/// functions are not included in the result; their effects are folded into
/// their callers (or consulted via [`library_effects`]).
pub fn analyze(program: &Program) -> HashMap<Function, FunctionSideEffects> {
    let global = program.global_arena();
    let is_library = |func: Function| -> bool {
        let data = program.func_data(func);
        data.layout().entry_bb().is_none() && library_effects(data.name()).is_some()
    };

    let user_functions = program
        .function_layout()
        .iter()
        .copied()
        .filter(|&func| !is_library(func))
        .collect::<Vec<_>>();

    // Call graph restricted to user functions; library calls are leaves.
    let mut callees = HashMap::default();
    for &func in &user_functions {
        let data = program.func_data(func);
        let list = calls_in(data)
            .into_iter()
            .map(|(callee, _)| callee)
            .filter(|&callee| !is_library(callee))
            .collect::<Vec<_>>();
        callees.insert(func, list);
    }

    // Seed every user function with its direct effects.
    let mut effects = HashMap::default();
    for &func in &user_functions {
        let data = program.func_data(func);
        effects.insert(func, collect_direct(data, global));
    }

    // SCC condensation + bottom-up processing order.
    let mut finder = SccFinder {
        callees: &callees,
        index: 0,
        indices: HashMap::default(),
        lowlinks: HashMap::default(),
        stack: Vec::new(),
        on_stack: HashSet::default(),
        sccs: Vec::new(),
    };
    for &func in &user_functions {
        if !finder.indices.contains_key(&func) {
            finder.strongconnect(func);
        }
    }
    let mut scc_id = HashMap::default();
    for (id, component) in finder.sccs.iter().enumerate() {
        for &func in component {
            scc_id.insert(func, id);
        }
    }
    let mut scc_edges = vec![HashSet::default(); finder.sccs.len()];
    for (id, component) in finder.sccs.iter().enumerate() {
        for &func in component {
            for &callee in &callees[&func] {
                let callee_id = scc_id[&callee];
                if callee_id != id {
                    scc_edges[id].insert(callee_id);
                }
            }
        }
    }
    let scc_edges = scc_edges
        .into_iter()
        .map(|set| set.into_iter().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let order = scc_post_order(finder.sccs.len(), &scc_id, &scc_edges);

    // Bottom-up propagation with fixed-point iteration inside each SCC.
    for component_id in order {
        let component = finder.sccs[component_id].clone();
        loop {
            // Snapshot the current attributes as this iteration's inputs so
            // intra-SCC calls read a consistent value.
            let snapshot = component
                .iter()
                .map(|&func| effects[&func].clone())
                .collect::<Vec<_>>();
            let mut changed = false;
            for (idx, &func) in component.iter().enumerate() {
                let data = program.func_data(func);
                let mut merged = snapshot[idx].clone();
                for (callee, args) in calls_in(data) {
                    let callee_effects = if is_library(callee) {
                        library_effects(program.func_data(callee).name()).unwrap_or_default()
                    } else if let Some(pos) = component.iter().position(|&f| f == callee) {
                        // Intra-SCC call: read this iteration's snapshot.
                        snapshot[pos].clone()
                    } else {
                        // External SCC (already converged, since we process
                        // callees before callers) or an unreachable function.
                        effects.get(&callee).cloned().unwrap_or_default()
                    };
                    propagate_call(&mut merged, &callee_effects, &args, data, global);
                }
                if merged != effects[&func] {
                    effects.insert(func, merged);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    effects
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::builder_trait::*;

    /// Create a unit-returning function with an entry block and no body.
    fn make_function(program: &mut Program, name: &str, params_ty: Vec<Type>) -> Function {
        let func = program.new_function(Type::get_unit(), name.into(), params_ty);
        let data = program.func_data_mut(func);
        data.add_entry_block();
        func
    }

    /// A fresh global i32 allocation (GlobalAlloc).
    fn make_global(program: &mut Program) -> Inst {
        let zero = program.new_value().integer(0);
        program.new_value().global_alloc(zero)
    }

    /// Run `f` over `func` through an arena context. FunctionData's global
    /// arena accessors are unimplemented, so any builder call that inspects a
    /// global value's type must go through the program arena.
    fn with_context<R>(
        program: &mut Program,
        func: Function,
        f: impl FnOnce(&mut ArenaContextMut<'_>) -> R,
    ) -> R {
        let mut context = ArenaContextMut {
            program,
            curr_func: Some(func),
        };
        f(&mut context)
    }

    fn entry_bb(ctx: &mut ArenaContextMut<'_>) -> crate::ir::BasicBlock {
        ctx.layout().entry_bb().unwrap().bb()
    }

    /// `gep(base, [offset])` -> `load`, inserted into `bb`.
    fn load_elem(
        ctx: &mut ArenaContextMut<'_>,
        bb: crate::ir::BasicBlock,
        base: Inst,
        offset: Inst,
    ) -> Inst {
        let gep = ctx.new_local_value().get_elem_ptr(base, vec![offset]);
        ctx.layout_mut().insert_inst(bb, gep);
        let load = ctx.new_local_value().load(gep);
        ctx.layout_mut().insert_inst(bb, load);
        load
    }

    /// `gep(base, [offset])` -> `store`, inserted into `bb`.
    fn store_elem(
        ctx: &mut ArenaContextMut<'_>,
        bb: crate::ir::BasicBlock,
        base: Inst,
        offset: Inst,
        value: Inst,
    ) {
        let gep = ctx.new_local_value().get_elem_ptr(base, vec![offset]);
        ctx.layout_mut().insert_inst(bb, gep);
        let store = ctx.new_local_value().store(value, gep);
        ctx.layout_mut().insert_inst(bb, store);
    }

    fn ret(ctx: &mut ArenaContextMut<'_>, bb: crate::ir::BasicBlock) {
        let r = ctx.new_local_value().ret(None);
        ctx.layout_mut().insert_inst(bb, r);
    }

    fn effects_of(program: &Program, func: Function) -> FunctionSideEffects {
        analyze(program).remove(&func).unwrap()
    }

    #[test]
    fn local_memory_only_function_is_strictly_pure() {
        let mut program = Program::new();
        let func = make_function(&mut program, "local_only", vec![]);
        with_context(&mut program, func, |ctx| {
            let entry = entry_bb(ctx);
            let slot = ctx.new_local_value().alloc(Type::get_i32());
            ctx.layout_mut().insert_inst(entry, slot);
            let one = ctx.new_local_value().integer(1);
            let store = ctx.new_local_value().store(one, slot);
            ctx.layout_mut().insert_inst(entry, store);
            let load = ctx.new_local_value().load(slot);
            ctx.layout_mut().insert_inst(entry, load);
            ret(ctx, entry);
        });

        let effects = effects_of(&program, func);
        assert!(effects.is_strictly_pure());
        assert!(effects.is_read_only());
        assert!(effects.is_arg_mem_only());
    }

    #[test]
    fn global_read_and_write_are_recorded() {
        let mut program = Program::new();
        let gv = make_global(&mut program);
        let func = make_function(&mut program, "global_user", vec![]);
        with_context(&mut program, func, |ctx| {
            let entry = entry_bb(ctx);
            let zero = ctx.new_local_value().integer(0);
            ctx.layout_mut().insert_inst(entry, zero);
            let one = ctx.new_local_value().integer(1);
            ctx.layout_mut().insert_inst(entry, one);
            load_elem(ctx, entry, gv, zero);
            store_elem(ctx, entry, gv, zero, one);
            ret(ctx, entry);
        });

        let effects = effects_of(&program, func);
        assert!(effects.read_globals.contains(&gv));
        assert!(effects.written_globals.contains(&gv));
        assert!(effects.may_read_global(gv));
        assert!(effects.may_write_global(gv));
        assert!(!effects.is_strictly_pure());
        assert!(!effects.is_read_only());
        assert!(!effects.is_arg_mem_only());
    }

    #[test]
    fn array_parameter_read_and_write_are_recorded_by_index() {
        let mut program = Program::new();
        let func = make_function(
            &mut program,
            "param_user",
            vec![Type::get_pointer(Type::get_i32())],
        );
        with_context(&mut program, func, |ctx| {
            let arr_param = ctx.curr_func_data().params()[0];
            let entry = entry_bb(ctx);
            let zero = ctx.new_local_value().integer(0);
            ctx.layout_mut().insert_inst(entry, zero);
            let one = ctx.new_local_value().integer(1);
            ctx.layout_mut().insert_inst(entry, one);
            load_elem(ctx, entry, arr_param, zero);
            store_elem(ctx, entry, arr_param, zero, one);
            ret(ctx, entry);
        });

        let effects = effects_of(&program, func);
        assert!(effects.read_array_params.contains(&0));
        assert!(effects.written_array_params.contains(&0));
        assert!(effects.may_read_array_param(0));
        assert!(effects.may_write_array_param(0));
        assert!(!effects.may_read_array_param(1));
        assert!(!effects.may_write_array_param(1));
        assert!(effects.is_arg_mem_only());
        assert!(!effects.is_strictly_pure());
    }

    #[test]
    fn library_functions_get_hardcoded_attributes() {
        let mut program = Program::new();
        let getint = program.new_function(Type::get_i32(), "getint".into(), vec![]);
        let getarray = program.new_function(
            Type::get_i32(),
            "getarray".into(),
            vec![Type::get_pointer(Type::get_i32())],
        );
        let putarray = program.new_function(
            Type::get_unit(),
            "putarray".into(),
            vec![Type::get_i32(), Type::get_pointer(Type::get_i32())],
        );
        let starttime = program.new_function(
            Type::get_unit(),
            "_sysy_starttime".into(),
            vec![Type::get_i32()],
        );

        let e = library_effects(program.func_data(getint).name()).unwrap();
        assert!(e.has_io_side_effects);
        let e = library_effects(program.func_data(getarray).name()).unwrap();
        assert!(e.has_io_side_effects && e.written_array_params.contains(&0));
        let e = library_effects(program.func_data(putarray).name()).unwrap();
        assert!(e.has_io_side_effects && e.read_array_params.contains(&1));
        let e = library_effects(program.func_data(starttime).name()).unwrap();
        assert!(e.has_timer_effects && !e.has_io_side_effects);
        assert!(library_effects("user_fn").is_none());
    }

    #[test]
    fn caller_inherits_global_effects_of_callee() {
        let mut program = Program::new();
        let gv = make_global(&mut program);
        let callee = make_function(&mut program, "reads_global", vec![]);
        with_context(&mut program, callee, |ctx| {
            let entry = entry_bb(ctx);
            let zero = ctx.new_local_value().integer(0);
            ctx.layout_mut().insert_inst(entry, zero);
            load_elem(ctx, entry, gv, zero);
            ret(ctx, entry);
        });
        let caller = make_function(&mut program, "calls_reads_global", vec![]);
        with_context(&mut program, caller, |ctx| {
            let entry = entry_bb(ctx);
            let call = ctx
                .new_local_value()
                .call_with_type(callee, vec![], Type::get_unit());
            ctx.layout_mut().insert_inst(entry, call);
            ret(ctx, entry);
        });

        let effects = effects_of(&program, caller);
        assert!(effects.read_globals.contains(&gv));
        assert!(effects.may_read_global(gv));
        assert!(!effects.may_write_global(gv));
        assert!(!effects.is_strictly_pure());
        assert!(effects.is_read_only());
    }

    #[test]
    fn passing_a_global_to_callee_maps_the_write_back_to_the_global() {
        let mut program = Program::new();
        let gv = make_global(&mut program);
        let callee = make_function(
            &mut program,
            "writes_param",
            vec![Type::get_pointer(Type::get_i32())],
        );
        with_context(&mut program, callee, |ctx| {
            let arr_param = ctx.curr_func_data().params()[0];
            let entry = entry_bb(ctx);
            let zero = ctx.new_local_value().integer(0);
            ctx.layout_mut().insert_inst(entry, zero);
            let one = ctx.new_local_value().integer(1);
            ctx.layout_mut().insert_inst(entry, one);
            store_elem(ctx, entry, arr_param, zero, one);
            ret(ctx, entry);
        });
        let caller = make_function(&mut program, "passes_global", vec![]);
        with_context(&mut program, caller, |ctx| {
            let entry = entry_bb(ctx);
            let call = ctx
                .new_local_value()
                .call_with_type(callee, vec![gv], Type::get_unit());
            ctx.layout_mut().insert_inst(entry, call);
            ret(ctx, entry);
        });

        let effects = effects_of(&program, caller);
        assert!(effects.written_globals.contains(&gv));
        assert!(effects.may_write_global(gv));
        assert!(!effects.written_array_params.contains(&0));
    }

    #[test]
    fn passing_a_parameter_to_callee_maps_the_write_back_to_that_parameter() {
        let mut program = Program::new();
        let callee = make_function(
            &mut program,
            "writes_param",
            vec![Type::get_pointer(Type::get_i32())],
        );
        with_context(&mut program, callee, |ctx| {
            let arr_param = ctx.curr_func_data().params()[0];
            let entry = entry_bb(ctx);
            let zero = ctx.new_local_value().integer(0);
            ctx.layout_mut().insert_inst(entry, zero);
            let one = ctx.new_local_value().integer(1);
            ctx.layout_mut().insert_inst(entry, one);
            store_elem(ctx, entry, arr_param, zero, one);
            ret(ctx, entry);
        });
        let caller = make_function(
            &mut program,
            "forwards_param",
            vec![
                Type::get_pointer(Type::get_i32()),
                Type::get_pointer(Type::get_i32()),
            ],
        );
        with_context(&mut program, caller, |ctx| {
            let first = ctx.curr_func_data().params()[0];
            let entry = entry_bb(ctx);
            let call = ctx
                .new_local_value()
                .call_with_type(callee, vec![first], Type::get_unit());
            ctx.layout_mut().insert_inst(entry, call);
            ret(ctx, entry);
        });

        let effects = effects_of(&program, caller);
        assert!(effects.written_array_params.contains(&0));
        assert!(!effects.written_array_params.contains(&1));
        assert!(effects.written_globals.is_empty());
    }

    #[test]
    fn local_array_passed_to_callee_does_not_escape() {
        let mut program = Program::new();
        let callee = make_function(
            &mut program,
            "writes_param",
            vec![Type::get_pointer(Type::get_i32())],
        );
        with_context(&mut program, callee, |ctx| {
            let arr_param = ctx.curr_func_data().params()[0];
            let entry = entry_bb(ctx);
            let zero = ctx.new_local_value().integer(0);
            ctx.layout_mut().insert_inst(entry, zero);
            let one = ctx.new_local_value().integer(1);
            ctx.layout_mut().insert_inst(entry, one);
            store_elem(ctx, entry, arr_param, zero, one);
            ret(ctx, entry);
        });
        let caller = make_function(&mut program, "passes_local", vec![]);
        with_context(&mut program, caller, |ctx| {
            let entry = entry_bb(ctx);
            let local = ctx
                .new_local_value()
                .alloc(Type::get_array(Type::get_i32(), 4));
            ctx.layout_mut().insert_inst(entry, local);
            let call = ctx
                .new_local_value()
                .call_with_type(callee, vec![local], Type::get_unit());
            ctx.layout_mut().insert_inst(entry, call);
            ret(ctx, entry);
        });

        let effects = effects_of(&program, caller);
        assert!(
            effects.is_strictly_pure(),
            "local array must not escape: {effects:?}"
        );
    }

    #[test]
    fn self_recursive_function_converges() {
        let mut program = Program::new();
        let gv = make_global(&mut program);
        let func = make_function(&mut program, "recursive", vec![Type::get_i32()]);
        with_context(&mut program, func, |ctx| {
            let entry = entry_bb(ctx);
            let n = ctx.curr_func_data().params()[0];
            let zero = ctx.new_local_value().integer(0);
            ctx.layout_mut().insert_inst(entry, zero);
            load_elem(ctx, entry, gv, zero);
            let one = ctx.new_local_value().integer(1);
            ctx.layout_mut().insert_inst(entry, one);
            let dec = ctx.new_local_value().binary(BinaryOp::Sub, n, one);
            ctx.layout_mut().insert_inst(entry, dec);
            let call = ctx
                .new_local_value()
                .call_with_type(func, vec![dec], Type::get_unit());
            ctx.layout_mut().insert_inst(entry, call);
            ret(ctx, entry);
        });

        let effects = effects_of(&program, func);
        assert!(effects.read_globals.contains(&gv));
        assert!(effects.may_read_global(gv));
    }

    #[test]
    fn mutually_recursive_functions_propagate_effects() {
        let mut program = Program::new();
        let gv = make_global(&mut program);
        let f = make_function(&mut program, "f", vec![]);
        let g = make_function(&mut program, "g", vec![]);
        with_context(&mut program, f, |ctx| {
            let entry = entry_bb(ctx);
            let zero = ctx.new_local_value().integer(0);
            ctx.layout_mut().insert_inst(entry, zero);
            load_elem(ctx, entry, gv, zero);
            let call = ctx
                .new_local_value()
                .call_with_type(g, vec![], Type::get_unit());
            ctx.layout_mut().insert_inst(entry, call);
            ret(ctx, entry);
        });
        with_context(&mut program, g, |ctx| {
            let entry = entry_bb(ctx);
            let call = ctx
                .new_local_value()
                .call_with_type(f, vec![], Type::get_unit());
            ctx.layout_mut().insert_inst(entry, call);
            ret(ctx, entry);
        });

        let effects = analyze(&program);
        assert!(effects[&f].read_globals.contains(&gv));
        assert!(
            effects[&g].read_globals.contains(&gv),
            "g must inherit f's read: {:?}",
            effects[&g]
        );
    }
}
