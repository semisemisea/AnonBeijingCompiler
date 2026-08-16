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
//! 4. Covered MemZero removal: a MemZero whose whole byte range is
//!    overwritten by later stores (same base, constant offsets, known
//!    store widths, no intervening read/call) is dead and removed.
//!
//! Address resolution reuses the M49 base environment (`BaseEnv`:
//! `base_of` + `constant_offset`, including pointer-slot resolution and
//! block-parameter phi offsets). Everything conservative: an unresolvable
//! address never triggers a deletion or rewrite.
//!
//! ---
//!
//! ## 补充说明（中文）
//!
//! 死存储消除（DSE）：删掉"写了也不会被读到"的 store，并把"刚写入的值"
//! 直接转发给紧接着的 load，减少内存往返。术语：SSA / block 参数（Phi）/
//! 死存储 / used_by / def-use 链 等见 `docs/offline-handbook/glossary.md`
//! 的"IR 与 SSA 基础"与"优化与 pass 概念"分组；GSP（标量全局提升 pass）
//! 见 `scalar_global_promotion.rs`。
//!
//! ### 一句话定位 + 动机
//!
//! 函数内（within-function）的存储优化：`run` 对每个非 decl 函数跑一遍
//! `run_on_func`，消除不可能被观察到的写入。动机：GSP 把可观察性受限的
//! 标量全局提升为 SSA 值、寄存器保存副本（"load once, write back once"），
//! 若函数内没人改过内存，写回的就是 load 出来的原值——纯开销；DSE 顺手
//! 清掉被覆盖的 store、做 store-to-load 转发，让后续 pass 看到更干净的内存图。
//!
//! ### 变换形态（英文文档所列四类的简要版）
//!
//! 1. **GSP 冗余写回删除**：`store v, p` 且 `v` 是 `load p` 的结果，该单元
//!    在函数内没有其它写、任何调用都不会写它、也无 MemZero 覆盖 → 写回
//!    原值原样，是死操作，删除。
//! 2. **覆盖存储删除**：同一单元两次 store、中间无读 → 前一个 store 的值
//!    必然被覆盖，删除。
//! 3. **store-to-load 转发**：load 的单元最近一次"未被读过的操作"是 store
//!    （中间无写、无可能读）→ load 用被存的值替换（`visit_and_replace`
//!    重写使用点）。
//! 4. **覆盖 MemZero 删除**：MemZero 的整个字节区间被后续 store 覆盖（同
//!    base、常量偏移、已知 store 宽度、中间无读/调用）→ 删除
//!    （`LiveMemZero` + `record_coverage` 用排序不相交区间并集跟踪覆盖）。
//!
//! ### 触发 / 放弃条件
//!
//! 地址解析是热路径：`resolve_cell`（`BaseEnv::base_of` + `constant_offset`，
//! 含指针槽解析与 block 参数 phi 偏移）把地址归约成 `Cell = (MemObject, i64)`；
//! **只有栈分配（`MemObject::Alloc`）和全局（`Global`）有单元语义**，参数
//! （ABI 指针）与未知 base 解析为 `None`。解析不出就什么都不做——全部保守。
//! 具体放弃点：
//! - 冗余写回（Transform 1）：单元被函数内其它写命中（`writes[cell].len() != 1`）、
//!   store 的源不是同单元的 load、任何调用可能写它（`call_write_roots` 为
//!   `None` 视为全写屏障）或 MemZero 可能覆盖它；
//! - 覆盖存储 / 转发（Transform 2+3，块内前向扫描）：遇到**不可解析地址**
//!   的 store/load/MemZero（可能别名任意单元）时清空 `pending`；调用按
//!   `call_write_roots` / `call_read_roots` / `call_may_read` 保守保留可能
//!   被写/被读的 pending store；MemZero 对其区间内的 pending store 同样是屏障；
//! - 覆盖 MemZero（Transform 4）：只有常量长度（`MemZeroLen::Const`）才可能
//!   被证明完全覆盖；运行时长度（M53 零存储循环，`MemZeroLen::Value`）视为
//!   无限区间，永不删除。
//!
//! ### 正确性要点
//!
//! - 全保守：任何解析不出 / 无法证明的地址、调用、MemZero 都按"可能读写
//!   一切"处理；
//! - 转发只发生在同一基本块内、按指令序前向扫描，且仅当单元的最新操作是
//!   尚未被观察的 pending store；被转发的 load 留在布局中（不消耗 pending
//!   store，后面同单元的 store 仍可覆盖删除它），失去使用者的死 load 交给
//!   DCE 收尾；
//! - MemZero 覆盖只累计"已被后续 store 覆盖"的字节，任何对区间内字节的
//!   load 都使 MemZero 必须保留；
//! - 删除用 `remove_layout_inst`、改写用 `visit_and_replace`，保持 arena 与
//!   used_by 一致。
//!
//! ### 管线位置
//!
//! - 注册：`opt/pass.rs` 的 `PassesManager::from_config`，fixpoint 段，
//!   **`gvn` 之后、`pointer_strength_reduction` 之前**（注册处注释：让
//!   后续 pass 看到更干净的内存图）；
//! - 前后配合：GVN 先做值编号 / 冗余消除，DSE 再清内存图，PSR 随后把
//!   仿射地址计算改写为指针递进；
//! - 无目标门控、无 config 开关（AArch64 / RISC-V 都跑）。
//!
//! ### 验证
//!
//! - 本文件 `mod tests`（449 行起）覆盖：冗余写回删除 / 单元被改后写回保留 /
//!   覆盖存储删除 / 中间读转发 / 不可解析地址屏障 / MemZero 屏障 /
//!   MemZero 完全覆盖删除 / 部分覆盖保留 / 中间读保留；
//! - 端到端：`make test` 差分比对。

use crate::ir::inst_kind::mem_zero::MemZeroLen;
use crate::opt::{
    analysis_passes::{
        effects::EffectAnalysis,
        memory::{BaseEnv, MemObject},
    },
    prelude::*,
};
use rustc_hash::{FxHashMap, FxHashSet};

/// Dead store elimination.
pub struct DSE;

impl Pass for DSE {
    fn run(&mut self, program: &mut Program) -> bool {
        let analysis = EffectAnalysis::new(program);
        let mut changed = false;
        for func in program.function_layout().to_vec() {
            if program.func_data(func).layout().is_decl() {
                continue; // declarations have no body and no base env
            }
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
        MemObject::Alloc(_) | MemObject::Global(_) => Some((env.base_of(ctx, addr), off)),
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

    // Address resolution is the hot path (every store/load/memzero);
    // memoize per instruction since GEPs are shared across accesses.
    let mut cell_cache: FxHashMap<Inst, Option<Cell>> = FxHashMap::default();
    let mut resolve = |addr: Inst| -> Option<Cell> {
        *cell_cache
            .entry(addr)
            .or_insert_with(|| resolve_cell(env, &ctx, addr))
    };

    for bb in data.layout().basicblocks() {
        for &inst in bb.insts() {
            match data.inst_data(inst).kind() {
                InstKind::Store(store) => {
                    if let Some(cell) = resolve(store.dest()) {
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
                    if let Some((root, off)) = resolve(mem_zero.dest()) {
                        // A runtime-length MemZero (M53 zero-store loops)
                        // may cover anything after `off`: treat it as an
                        // unbounded range.
                        let len = match mem_zero.byte_len_len() {
                            MemZeroLen::Const(n) => *n as i64,
                            MemZeroLen::Value(_) => i64::MAX,
                        };
                        memzeros.push((root, off, len));
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
    //
    // Precompute the set of bases that any call may write, so the per-cell
    // check stays O(1) instead of O(#calls) (large functions with many
    // stores and calls would otherwise blow up quadratically).
    let mut call_written_bases: FxHashSet<MemObject> = FxHashSet::default();
    let mut call_writes_unknown = false;
    for &call_inst in &writing_calls {
        let InstKind::Call(call) = data.inst_data(call_inst).kind() else {
            continue;
        };
        match analysis.call_write_roots(call.callee(), func) {
            None => {
                call_writes_unknown = true;
                break;
            }
            Some(roots) => {
                for root in roots {
                    call_written_bases.insert(root_to_base(&root));
                }
            }
        }
    }
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
        let Some(src_cell) = resolve(load.src()) else {
            continue;
        };
        if &src_cell != cell {
            continue;
        }
        // No call may write this cell (or anything).
        if call_writes_unknown || call_written_bases.contains(&cell.0) {
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

    // ---- Transform 2 + 3: covered store removal & store-to-load
    // forwarding (within-block). ----
    // While scanning a block forward, a store is dead as soon as another
    // store to the same cell appears before any read of that cell. A load
    // of a cell whose latest operation is a pending store is replaced by
    // the stored value (forwarding).
    let mut covered_removals: Vec<Inst> = Vec::new();
    // (load inst, replacement value)
    let mut forwardings: Vec<(Inst, Inst)> = Vec::new();
    // MemZeros whose whole range is overwritten by later stores.
    let mut memzero_removals: Vec<Inst> = Vec::new();
    for bb in data.layout().basicblocks() {
        // cell -> (pending store, stored value); the store has not been
        // read from memory yet.
        let mut pending: FxHashMap<Cell, (Inst, Inst)> = FxHashMap::default();
        // MemZeros in this block that later stores may still cover fully.
        let mut live_memzeros: Vec<LiveMemZero> = Vec::new();
        for &inst in bb.insts() {
            match data.inst_data(inst).kind() {
                InstKind::Store(store) => match resolve(store.dest()) {
                    Some(cell) => {
                        if let Some((prev, _)) = pending.insert(cell, (inst, store.src())) {
                            covered_removals.push(prev);
                        }
                        // Each byte of the stored value overwrites any live
                        // MemZero range it lands in. The store width is the
                        // byte size of the stored value's type (i32 -> 4).
                        let width = data.inst_data(store.src()).ty().size() as i64;
                        if width > 0 {
                            let (root_s, off_s) = cell;
                            let mut i = 0;
                            while i < live_memzeros.len() {
                                let covered = {
                                    let m = &mut live_memzeros[i];
                                    if m.root != root_s {
                                        i += 1;
                                        continue;
                                    }
                                    let s = off_s.max(m.off);
                                    let e = (off_s + width).min(m.off + m.len);
                                    if s >= e {
                                        i += 1;
                                        continue;
                                    }
                                    record_coverage(m, s, e)
                                };
                                if covered {
                                    // Fully covered: the MemZero is dead.
                                    memzero_removals.push(live_memzeros[i].inst);
                                    live_memzeros.swap_remove(i);
                                } else {
                                    i += 1;
                                }
                            }
                        }
                    }
                    // Unknown destination: may alias anything, so every
                    // pending store is conservatively treated as observed,
                    // and no MemZero coverage can be proven.
                    None => {
                        pending.clear();
                        live_memzeros.clear();
                    }
                },
                InstKind::Load(load) => match resolve(load.src()) {
                    Some(cell) => {
                        if let Some(&(_, value)) = pending.get(&cell) {
                            forwardings.push((inst, value));
                            // The store's memory value is still unread; it
                            // stays pending so a later covering store can
                            // still remove it.
                        } else {
                            pending.remove(&cell);
                        }
                        // A load of any byte in a live MemZero's range
                        // observes the zeroing: the MemZero must stay.
                        live_memzeros.retain(|m| !cell_in_range(&cell, m.root, m.off, m.len));
                    }
                    None => {
                        pending.clear();
                        live_memzeros.clear();
                    }
                },
                InstKind::Call(call) => {
                    // A call may read any memory (and may write it), so no
                    // MemZero coverage can be proven past it.
                    live_memzeros.clear();
                    let callee = call.callee();
                    match analysis.call_write_roots(callee, func) {
                        None => pending.clear(),
                        Some(write_roots) => {
                            let read_roots = analysis.call_read_roots(callee, func);
                            pending.retain(|cell, _| {
                                let written =
                                    write_roots.iter().any(|root| cell_matches_root(cell, root));
                                if written {
                                    return false;
                                }
                                match &read_roots {
                                    None => false, // may read anything
                                    Some(roots) => {
                                        if roots.iter().any(|root| cell_matches_root(cell, root)) {
                                            return false;
                                        }
                                        // The call may also read through a
                                        // target set we cannot express as
                                        // roots; be conservative.
                                        let mut targets = rustc_hash::FxHashSet::default();
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
                    if let Some((root, off)) = resolve(mem_zero.dest()) {
                        let len = match mem_zero.byte_len_len() {
                            MemZeroLen::Const(n) => *n as i64,
                            MemZeroLen::Value(_) => i64::MAX,
                        };
                        pending.retain(|cell, _| !cell_in_range(cell, root, off, len));
                        // A following MemZero zeroes overlapping bytes again:
                        // no coverage can be proven for those live candidates.
                        live_memzeros.retain(|m| {
                            m.root != root || m.off >= off + len || off >= m.off + m.len
                        });
                        // Register as a removal candidate. A runtime-length
                        // MemZero (unbounded range) can never be proven fully
                        // covered by stores, so it is not tracked.
                        if len != i64::MAX {
                            live_memzeros.push(LiveMemZero {
                                inst,
                                root,
                                off,
                                len,
                                covered: Vec::new(),
                            });
                        }
                    } else {
                        pending.clear();
                        live_memzeros.clear();
                    }
                }
                _ => {}
            }
        }
    }

    let mut changed = false;
    for (load, value) in forwardings {
        utils::visit_and_replace(data, load, value);
        changed = true;
    }
    for inst in writeback_removals
        .into_iter()
        .chain(covered_removals)
        .chain(memzero_removals)
    {
        if let Some(bb) = data.layout().parent_bb(inst) {
            data.remove_layout_inst(bb, inst);
            changed = true;
        }
    }
    changed
}

/// Convert a cell to the abstract object used by the effects analysis.
fn cell_to_object(
    cell: &Cell,
    func: Function,
) -> crate::opt::analysis_passes::effects::AbstractObject {
    match cell.0 {
        MemObject::Global(g) => crate::opt::analysis_passes::effects::AbstractObject::Global(g),
        MemObject::Alloc(a) => crate::opt::analysis_passes::effects::AbstractObject::Alloc(func, a),
        _ => crate::opt::analysis_passes::effects::AbstractObject::Unknown,
    }
}

/// Convert a write root to its base object.
fn root_to_base(root: &crate::opt::analysis_passes::effects::WriteRoot) -> MemObject {
    match root {
        crate::opt::analysis_passes::effects::WriteRoot::Global(g) => MemObject::Global(*g),
        crate::opt::analysis_passes::effects::WriteRoot::Local(_, a) => MemObject::Alloc(*a),
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

/// A MemZero candidate for coverage-based removal while scanning a block.
///
/// `covered` is the sorted, disjoint union of byte ranges of
/// `[off, off+len)` already overwritten by later stores. Once the union
/// covers the whole range the MemZero is dead.
struct LiveMemZero {
    inst: Inst,
    root: MemObject,
    off: i64,
    len: i64,
    covered: Vec<(i64, i64)>,
}

/// Record that byte range `[s, e)` of `m`'s range was overwritten by a
/// later store, merging into the sorted disjoint `covered` intervals.
/// Returns `true` iff `[off, off+len)` is now fully covered.
fn record_coverage(m: &mut LiveMemZero, s: i64, e: i64) -> bool {
    if s >= e {
        return false;
    }
    // Insert `[s, e)` into the sorted disjoint interval list, merging
    // overlaps and adjacencies.
    let mut new_s = s;
    let mut new_e = e;
    let mut i = 0;
    while i < m.covered.len() {
        let (a, b) = m.covered[i];
        if b < new_s {
            i += 1;
            continue;
        }
        if a > new_e {
            break;
        }
        new_s = new_s.min(a);
        new_e = new_e.max(b);
        m.covered.remove(i);
    }
    m.covered.insert(i, (new_s, new_e));
    // Check whether the merged union now spans `[off, off+len)` without holes.
    let end = m.off + m.len;
    let mut cur = m.off;
    for &(a, b) in m.covered.iter() {
        if a > cur {
            return false;
        }
        cur = cur.max(b);
        if cur >= end {
            return true;
        }
    }
    cur >= end
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
    /// modified value and must stay. The intermediate load is forwarded and
    /// the first store is covered, so only the write-back store survives.
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
        let _ = ret;

        assert!(DSE.run(&mut program));
        let data = program.func_data(function);
        // The load is forwarded: it no longer has any user.
        assert!(
            data.inst_data(load).used_by().is_empty(),
            "load must be forwarded (no remaining users)"
        );
        // The write-back store (of the modified value) must survive.
        assert!(
            matches!(data.inst_data(store).kind(), InstKind::Store(..)),
            "write-back after a real write must survive"
        );
        let mut stores = Vec::new();
        for block in data.layout().basicblocks() {
            for &inst in block.insts() {
                if matches!(data.inst_data(inst).kind(), InstKind::Store(..)) {
                    stores.push(inst);
                }
            }
        }
        assert_eq!(stores, vec![store], "only the write-back store remains");
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

    /// `store 1; load; store 2`: the load is forwarded to 1 and the first
    /// store is covered; only `store 2` remains. (Forwarding makes the
    /// intermediate load a non-reader of memory.)
    #[test]
    fn keeps_store_read_in_between() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let (first, second, load, ret) = {
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
            let second = data.new_local_value().store(two, global);
            data.layout_mut().insert_inst(entry, second);
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(entry, ret);
            (first, second, load, ret)
        };
        let _ = ret;

        assert!(DSE.run(&mut program));
        let data = program.func_data(function);
        // The load was forwarded to the value of the first store: no users.
        assert!(
            data.inst_data(load).used_by().is_empty(),
            "load must be forwarded (no remaining users)"
        );
        let mut stores = Vec::new();
        for block in data.layout().basicblocks() {
            for &inst in block.insts() {
                if matches!(data.inst_data(inst).kind(), InstKind::Store(..)) {
                    stores.push(inst);
                }
            }
        }
        assert_eq!(stores, vec![second], "only the covering store remains");
        let _ = first;
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
        assert_eq!(
            stores.len(),
            3,
            "no store may be dropped across an unknown write"
        );
    }

    /// A MemZero covering the cell is a read/write barrier: the pending
    /// store before it cannot be forwarded across or dropped.
    #[test]
    fn memzero_barrier_stops_forwarding_and_removal() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let (first, second, load, ret) = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(function),
            };
            let entry = data.add_entry_block();
            let alloc = data.new_local_value().alloc(Type::get_i32());
            data.layout_mut().insert_inst(entry, alloc);
            let five = data.new_local_value().integer(5);
            let first = data.new_local_value().store(five, alloc);
            data.layout_mut().insert_inst(entry, first);
            let zero = data.new_local_value().mem_zero(alloc, 4);
            data.layout_mut().insert_inst(entry, zero);
            let load = data.new_local_value().load(alloc);
            data.layout_mut().insert_inst(entry, load);
            let second = data.new_local_value().store(load, alloc);
            data.layout_mut().insert_inst(entry, second);
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(entry, ret);
            (first, second, load, ret)
        };
        let _ = ret;

        assert!(!DSE.run(&mut program));
        let data = program.func_data(function);
        // The load was NOT forwarded (memzero cleared the pending store).
        assert!(matches!(data.inst_data(load).kind(), InstKind::Load(..)));
        // Both stores survive.
        let mut stores = Vec::new();
        for block in data.layout().basicblocks() {
            for &inst in block.insts() {
                if matches!(data.inst_data(inst).kind(), InstKind::Store(..)) {
                    stores.push(inst);
                }
            }
        }
        assert_eq!(stores.len(), 2, "memzero barrier must keep both stores");
    }

    /// `memzero a[4], 16` followed by stores of all 4 i32 elements: every
    /// byte of the range is overwritten before any read — the MemZero is
    /// dead and must be removed.
    #[test]
    fn removes_memzero_fully_covered_by_stores() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let (memzero, ret) = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(function),
            };
            let entry = data.add_entry_block();
            let alloc = data
                .new_local_value()
                .alloc(Type::get_array(Type::get_i32(), 4));
            data.layout_mut().insert_inst(entry, alloc);
            let zero = data.new_local_value().integer(0);
            data.layout_mut().insert_inst(entry, zero);
            let memzero = data.new_local_value().mem_zero(alloc, 16);
            data.layout_mut().insert_inst(entry, memzero);
            for i in 0..4 {
                let idx = data.new_local_value().integer(i);
                data.layout_mut().insert_inst(entry, idx);
                let ptr = data.new_local_value().get_elem_ptr(alloc, vec![zero, idx]);
                data.layout_mut().insert_inst(entry, ptr);
                let val = data.new_local_value().integer(i + 1);
                data.layout_mut().insert_inst(entry, val);
                let store = data.new_local_value().store(val, ptr);
                data.layout_mut().insert_inst(entry, store);
            }
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(entry, ret);
            (memzero, ret)
        };
        let _ = ret;

        assert!(DSE.run(&mut program));
        let data = program.func_data(function);
        let mut memzeros = Vec::new();
        let mut stores = 0;
        for block in data.layout().basicblocks() {
            for &inst in block.insts() {
                match data.inst_data(inst).kind() {
                    InstKind::MemZero(..) => memzeros.push(inst),
                    InstKind::Store(..) => stores += 1,
                    _ => {}
                }
            }
        }
        assert!(memzeros.is_empty(), "fully covered memzero must be removed");
        assert_eq!(stores, 4, "covering stores must survive");
        let _ = memzero;
    }

    /// `memzero a[4], 16` with only the first two elements stored: bytes
    /// [8, 16) are never overwritten — the MemZero must survive.
    #[test]
    fn keeps_memzero_partially_covered() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let (memzero, ret) = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(function),
            };
            let entry = data.add_entry_block();
            let alloc = data
                .new_local_value()
                .alloc(Type::get_array(Type::get_i32(), 4));
            data.layout_mut().insert_inst(entry, alloc);
            let zero = data.new_local_value().integer(0);
            data.layout_mut().insert_inst(entry, zero);
            let memzero = data.new_local_value().mem_zero(alloc, 16);
            data.layout_mut().insert_inst(entry, memzero);
            for i in 0..2 {
                let idx = data.new_local_value().integer(i);
                data.layout_mut().insert_inst(entry, idx);
                let ptr = data.new_local_value().get_elem_ptr(alloc, vec![zero, idx]);
                data.layout_mut().insert_inst(entry, ptr);
                let val = data.new_local_value().integer(i + 1);
                data.layout_mut().insert_inst(entry, val);
                let store = data.new_local_value().store(val, ptr);
                data.layout_mut().insert_inst(entry, store);
            }
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(entry, ret);
            (memzero, ret)
        };
        let _ = ret;

        assert!(!DSE.run(&mut program), "nothing may change");
        let data = program.func_data(function);
        let mut memzeros = Vec::new();
        for block in data.layout().basicblocks() {
            for &inst in block.insts() {
                if matches!(data.inst_data(inst).kind(), InstKind::MemZero(..)) {
                    memzeros.push(inst);
                }
            }
        }
        assert_eq!(memzeros.len(), 1, "partially covered memzero must survive");
        let _ = memzero;
    }

    /// A load of any byte in the MemZero's range observes the zeroing:
    /// the MemZero must survive even if later stores cover it fully.
    #[test]
    fn keeps_memzero_read_in_between() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let (memzero, ret) = {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(function),
            };
            let entry = data.add_entry_block();
            let alloc = data
                .new_local_value()
                .alloc(Type::get_array(Type::get_i32(), 4));
            data.layout_mut().insert_inst(entry, alloc);
            let zero = data.new_local_value().integer(0);
            data.layout_mut().insert_inst(entry, zero);
            let memzero = data.new_local_value().mem_zero(alloc, 16);
            data.layout_mut().insert_inst(entry, memzero);
            for i in 0..3 {
                let idx = data.new_local_value().integer(i);
                data.layout_mut().insert_inst(entry, idx);
                let ptr = data.new_local_value().get_elem_ptr(alloc, vec![zero, idx]);
                data.layout_mut().insert_inst(entry, ptr);
                let val = data.new_local_value().integer(i + 1);
                data.layout_mut().insert_inst(entry, val);
                let store = data.new_local_value().store(val, ptr);
                data.layout_mut().insert_inst(entry, store);
            }
            // Read a byte of the zeroed range before the final store.
            let first_ptr = data.new_local_value().get_elem_ptr(alloc, vec![zero, zero]);
            data.layout_mut().insert_inst(entry, first_ptr);
            let load = data.new_local_value().load(first_ptr);
            data.layout_mut().insert_inst(entry, load);
            let idx3 = data.new_local_value().integer(3);
            data.layout_mut().insert_inst(entry, idx3);
            let last_ptr = data.new_local_value().get_elem_ptr(alloc, vec![zero, idx3]);
            data.layout_mut().insert_inst(entry, last_ptr);
            let val = data.new_local_value().integer(9);
            data.layout_mut().insert_inst(entry, val);
            let store = data.new_local_value().store(val, last_ptr);
            data.layout_mut().insert_inst(entry, store);
            let ret = data.new_local_value().ret(None);
            data.layout_mut().insert_inst(entry, ret);
            (memzero, ret)
        };
        let _ = ret;

        DSE.run(&mut program);
        let data = program.func_data(function);
        let mut memzeros = Vec::new();
        for block in data.layout().basicblocks() {
            for &inst in block.insts() {
                if matches!(data.inst_data(inst).kind(), InstKind::MemZero(..)) {
                    memzeros.push(inst);
                }
            }
        }
        assert_eq!(
            memzeros.len(),
            1,
            "memzero observed by an intervening load must survive"
        );
        let _ = memzero;
    }
}
