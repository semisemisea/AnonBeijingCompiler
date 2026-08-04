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
    analysis_passes::{effects::EffectAnalysis, memory::{BaseEnv, MemObject}},
    prelude::*,
};

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
/// compile-time known.
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
    let mut changed = false;
    // (implemented in later commits: write-back removal, covered stores,
    // store-to-load forwarding)
    let _ = (func, env, changed, data);
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        InstKind, Program, Type, arena::Arena, builder_trait::*,
    };

    #[test]
    fn smoke() {
        // Placeholder so the module compiles with tests wired in; real
        // tests arrive with the transforms.
        assert!(true);
    }
}
