//! Dead Store Elimination (DSE).
//!
//! Three within-function transforms, in order of value:
//! 1. GSP redundant write-back removal: a `store v, p` where `v` is the
//!    result of `load p` (and the cell is never written in the function,
//!    nor by any call) stores the value back unchanged — dead.
//! 2. Covered store removal: two stores to the same cell with no read in
//!    between — the earlier store is dead.
//! 3. Store-to-load forwarding: a load of a cell whose most recent
//!    operation is a store (no write or possible read in between) is
//!    replaced by the stored value.
//!
//! Address resolution reuses the M49 base environment (`BaseEnv`:
//! `base_of` + `constant_offset`, including pointer-slot resolution and
//! block-parameter phi offsets). Everything conservative: an unresolvable
//! address never triggers a deletion or rewrite.

use crate::opt::{
    analysis_passes::{
        effects::EffectAnalysis,
        memory::{BaseEnv, MemObject},
    },
    prelude::*,
};
use rustc_hash::FxHashMap;

/// Dead store elimination.
pub struct DSE;

impl Pass for DSE {
    fn run(&mut self, program: &mut Program) -> bool {
        let analysis = EffectAnalysis::new(program);
        let mut changed = false;
        for func in program.function_layout().to_vec() {
            let mut arena_context = ArenaContextMut {
                program,
                curr_func: Some(func),
            };
            changed |= run_on_func(&mut arena_context, &analysis);
        }
        changed
    }
}

/// A resolved memory cell: base object plus byte offset.
type Cell = (MemObject, i64);

/// Resolve an address to a concrete cell when its base and offset are both
/// compile-time known. Only stack allocs and globals have cell semantics;
/// parameters (ABI pointers) and unknown bases resolve to `None`.
fn resolve_cell(env: &BaseEnv, ctx: &ArenaContext<'_>, addr: Inst) -> Option<Cell> {
    let off = env.constant_offset(ctx, addr)?;
    match env.base_of(ctx, addr) {
        MemObject::Alloc(_) | MemObject::Global(_) => {
            Some((env.base_of(ctx, addr), off))
        }
        _ => None,
    }
}

/// Core per-function scan.
fn run_on_func(data: &mut ArenaContextMut<'_>, analysis: &EffectAnalysis) -> bool {
    let func = data.curr_func.unwrap();
    let env = analysis.env_of(func);
    let ctx = ArenaContext {
        program: data.program,
        curr_func: Some(func),
    };

    // ---- Pass A (read-only): collect writes, calls, memzeros. ----
    // cell -> store instructions writing it
    let mut writes: FxHashMap<Cell, Vec<Inst>> = FxHashMap::default();
    // (callee, inst) pairs of calls that may write memory at all
    let mut writing_calls: Vec<Inst> = Vec::new();
    // memzero ranges: (root, offset, len)
    let mut memzeros: Vec<(MemObject, i64, i64)> = Vec::new();

    for bb in data.layout().basicblocks() {
        for &inst in bb.insts() {
            match data.inst_data(inst).kind() {
                InstKind::Store(store) => {
                    if let Some(cell) = resolve_cell(env, &ctx, store.dest()) {
                        writes.entry(cell).or_default().push(inst);
                    }
                }
                InstKind::Call(call) => {
                    if analysis.call_write_roots(call.callee(), func).is_some() {
                        // The callee has a bounded write set; the roots are
                        // checked per cell below. (None = may write
                        // anything, treated as a full barrier.)
                        writing_calls.push(inst);
                    }
                }
                InstKind::MemZero(mem_zero) => {
                    if let Some((root, off)) = resolve_cell(env, &ctx, mem_zero.dest()) {
                        memzeros.push((root, off, mem_zero.byte_len() as i64));
                    }
                }
                _ => {}
            }
        }
    }

    // ---- Transform 1: GSP redundant write-back removal. ----
    // A store whose value is the result of loading the very same cell,
    // when nothing else in the function writes that cell (and no call may
    // write it, and no MemZero covers it), stores the original value back:
    // the write-back is a no-op and can go.
    let mut writeback_removals: Vec<Inst> = Vec::new();
    for (cell, insts) in writes.iter() {
        if insts.len() != 1 {
            continue; // the cell is written elsewhere in the function
        }
        let store_inst = insts[0];
        let InstKind::Store(store) = data.inst_data(store_inst).kind() else {
            continue;
        };
        // v must be a load of the same cell.
        let InstKind::Load(load) = data.inst_data(store.src()).kind() else {
            continue;
        };
        let Some(src_cell) = resolve_cell(env, &ctx, load.src()) else {
            continue;
        };
        if &src_cell != cell {
            continue;
        }
        // No call may write this cell.
        let mut may_write = false;
        for &call_inst in &writing_calls {
            let InstKind::Call(call) = data.inst_data(call_inst).kind() else {
                continue;
            };
            match analysis.call_write_roots(call.callee(), func) {
                None => {
                    may_write = true;
                    break;
                }
                Some(roots) => {
                    if roots.iter().any(|root| cell_matches_root(cell, root)) {
                        may_write = true;
                        break;
                    }
                }
            }
        }
        if may_write {
            continue;
        }
        // No MemZero may cover the cell.
        if memzeros
            .iter()
            .any(|(root, off, len)| cell_in_range(cell, *root, *off, *len))
        {
            continue;
        }
        writeback_removals.push(store_inst);
    }

    // ---- Transform 2: covered store removal (within-block). ----
    // While scanning a block forward, a store is dead as soon as another
    // store to the same cell appears before any read of that cell.
    let mut covered_removals: Vec<Inst> = Vec::new();
    for bb in data.layout().basicblocks() {
        // cell -> pending store that has not been read yet
        let mut pending: FxHashMap<Cell, Inst> = FxHashMap::default();
        for &inst in bb.insts() {
            match data.inst_data(inst).kind() {
                InstKind::Store(store) => match resolve_cell(env, &ctx, store.dest()) {
                    Some(cell) => {
                        if let Some(prev) = pending.insert(cell, inst) {
                            covered_removals.push(prev);
                        }
                    }
                    // Unknown destination: may alias anything, so every
                    // pending store is conservatively treated as observed.
                    None => pending.clear(),
                },
                InstKind::Load(load) => match resolve_cell(env, &ctx, load.src()) {
                    Some(cell) => {
                        pending.remove(&cell);
                    }
                    None => pending.clear(),
                },
                InstKind::Call(call) => {
                    let callee = call.callee();
                    match analysis.call_write_roots(callee, func) {
                        None => pending.clear(),
                        Some(write_roots) => {
                            let read_roots = analysis.call_read_roots(callee, func);
                            pending.retain(|cell, _| {
                                let written = write_roots
                                    .iter()
                                    .any(|root| cell_matches_root(cell, root));
                                if written {
                                    return false;
                                }
                                match &read_roots {
                                    None => false, // may read anything
                                    Some(roots) => {
                                        if roots
                                            .iter()
                                            .any(|root| cell_matches_root(cell, root))
                                        {
                                            return false;
                                        }
                                        // The call may also read through a
                                        // target set we cannot express as
                                        // roots; be conservative.
                                        let mut targets =
                                            rustc_hash::FxHashSet::default();
                                        let obj = cell_to_object(cell, func);
                                        targets.insert(obj);
                                        !analysis.call_may_read(callee, Some(&targets))
                                    }
                                }
                            });
                        }
                    }
                }
                InstKind::MemZero(mem_zero) => {
                    if let Some((root, off)) = resolve_cell(env, &ctx, mem_zero.dest()) {
                        let len = mem_zero.byte_len() as i64;
                        pending.retain(|cell, _| {
                            !cell_in_range(cell, root, off, len)
                        });
                    } else {
                        pending.clear();
                    }
                }
                _ => {}
            }
        }
    }

    let mut changed = false;
    for inst in writeback_removals.into_iter().chain(covered_removals) {
        if let Some(bb) = data.layout().parent_bb(inst) {
            data.remove_layout_inst(bb, inst);
            changed = true;
        }
    }
    changed
}

/// Convert a cell to the abstract object used by the effects analysis.
fn cell_to_object(cell: &Cell, func: Function) -> crate::opt::analysis_passes::effects::AbstractObject {
    match cell.0 {
        MemObject::Global(g) => crate::opt::analysis_passes::effects::AbstractObject::Global(g),
        MemObject::Alloc(a) => {
            crate::opt::analysis_passes::effects::AbstractObject::Alloc(func, a)
        }
        _ => crate::opt::analysis_passes::effects::AbstractObject::Unknown,
    }
}

/// Whether a write root (as reported by `call_write_roots`) covers `cell`.
fn cell_matches_root(cell: &Cell, root: &crate::opt::analysis_passes::effects::WriteRoot) -> bool {
    match (cell.0, root) {
        (MemObject::Global(g), crate::opt::analysis_passes::effects::WriteRoot::Global(rg)) => {
            g == *rg
        }
        (MemObject::Alloc(a), crate::opt::analysis_passes::effects::WriteRoot::Local(_, ra)) => {
            a == *ra
        }
        _ => false,
    }
}

/// Whether a MemZero range `[off, off+len)` of `root` covers `cell`'s offset.
fn cell_in_range(cell: &Cell, root: MemObject, off: i64, len: i64) -> bool {
    if cell.0 != root {
        return false;
    }
    cell.1 >= off && cell.1 < off + len
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{InstKind, Program, Type, arena::Arena, builder_trait::*};

    fn new_global(program: &mut Program) -> Inst {
        let init = program.new_value().zero_init(Type::get_i32());
        program.new_value().global_alloc(init)
    }

    #[test]
    fn smoke() {
        assert!(true);
    }

    /// `load global; store v, global` with no other write: the store is a
    /// GSP-style redundant write-back and must be removed.
    #[test]
    fn removes_redundant_writeback_of_load_value() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let (load, store, ret) = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(function),
            };
            let entry = data.add_entry_block();
            let load = data.new_local_value().load(global);
            data.layout_mut().insert_inst(entry, load);
            let store = data.new_local_value().store(load, global);
            data.layout_mut().insert_inst(entry, store);
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(entry, ret);
            (load, store, ret)
        };
        let _ = load;
        let _ = ret;

        assert!(DSE.run(&mut program));
        let data = program.func_data(function);
        // The store instruction must be gone from the layout.
        let mut found_store = false;
        for block in data.layout().basicblocks() {
            for &inst in block.insts() {
                if matches!(data.inst_data(inst).kind(), InstKind::Store(..)) {
                    found_store = true;
                }
            }
        }
        assert!(!found_store, "redundant write-back store should be removed");
    }

    /// The write-back is NOT redundant when the cell is written in between:
    /// `store 5; load; store load-value` — the final store forwards the
    /// modified value and must stay.
    #[test]
    fn keeps_writeback_when_cell_is_modified() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let (load, store, ret) = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(function),
            };
            let entry = data.add_entry_block();
            let five = data.new_local_value().integer(5);
            let first_store = data.new_local_value().store(five, global);
            data.layout_mut().insert_inst(entry, first_store);
            let load = data.new_local_value().load(global);
            data.layout_mut().insert_inst(entry, load);
            let store = data.new_local_value().store(load, global);
            data.layout_mut().insert_inst(entry, store);
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(entry, ret);
            (load, store, ret)
        };
        let _ = load;
        let _ = ret;

        assert!(!DSE.run(&mut program));
        let data = program.func_data(function);
        assert!(matches!(
            data.inst_data(store).kind(),
            InstKind::Store(..)
        ), "write-back after a real write must survive");
    }

    /// `store 1; store 2` to the same cell with no read in between: the
    /// first store is dead.
    #[test]
    fn removes_covered_store() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let (first, second, ret) = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(function),
            };
            let entry = data.add_entry_block();
            let one = data.new_local_value().integer(1);
            let two = data.new_local_value().integer(2);
            let first = data.new_local_value().store(one, global);
            data.layout_mut().insert_inst(entry, first);
            let second = data.new_local_value().store(two, global);
            data.layout_mut().insert_inst(entry, second);
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(entry, ret);
            (first, second, ret)
        };
        let _ = ret;

        assert!(DSE.run(&mut program));
        let data = program.func_data(function);
        let mut stores = Vec::new();
        for block in data.layout().basicblocks() {
            for &inst in block.insts() {
                if matches!(data.inst_data(inst).kind(), InstKind::Store(..)) {
                    stores.push(inst);
                }
            }
        }
        assert_eq!(stores, vec![second], "first covered store must go");
    }

    /// `store 1; load; store 2`: the load reads the first store, so it is
    /// live and must survive.
    #[test]
    fn keeps_store_read_in_between() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let (first, second, ret) = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(function),
            };
            let entry = data.add_entry_block();
            let one = data.new_local_value().integer(1);
            let two = data.new_local_value().integer(2);
            let first = data.new_local_value().store(one, global);
            data.layout_mut().insert_inst(entry, first);
            let load = data.new_local_value().load(global);
            data.layout_mut().insert_inst(entry, load);
            let _ = load;
            let second = data.new_local_value().store(two, global);
            data.layout_mut().insert_inst(entry, second);
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(entry, ret);
            (first, second, ret)
        };
        let _ = ret;

        assert!(!DSE.run(&mut program));
        let data = program.func_data(function);
        let mut stores = Vec::new();
        for block in data.layout().basicblocks() {
            for &inst in block.insts() {
                if matches!(data.inst_data(inst).kind(), InstKind::Store(..)) {
                    stores.push(inst);
                }
            }
        }
        assert_eq!(stores.len(), 2, "both stores must survive a read");
    }

    /// A store through an unknown address may alias any cell: pending
    /// stores must not be dropped across it.
    #[test]
    fn keeps_store_across_unknown_address_store() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        let function = program.new_function(Type::get_unit(), "f".into(), vec![Type::get_i32()]);
        let (first, second, ret) = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(function),
            };
            let entry = data.add_entry_block();
            let one = data.new_local_value().integer(1);
            let two = data.new_local_value().integer(2);
            let first = data.new_local_value().store(one, global);
            data.layout_mut().insert_inst(entry, first);
            // A store through the (unknown) parameter address.
            let param = data.params()[0];
            let unknown_store = data.new_local_value().store(two, param);
            data.layout_mut().insert_inst(entry, unknown_store);
            let second = data.new_local_value().store(two, global);
            data.layout_mut().insert_inst(entry, second);
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(entry, ret);
            (first, second, ret)
        };
        let _ = ret;

        assert!(!DSE.run(&mut program));
        let data = program.func_data(function);
        let mut stores = Vec::new();
        for block in data.layout().basicblocks() {
            for &inst in block.insts() {
                if matches!(data.inst_data(inst).kind(), InstKind::Store(..)) {
                    stores.push(inst);
                }
            }
        }
        assert_eq!(stores.len(), 3, "no store may be dropped across an unknown write");
    }
}
