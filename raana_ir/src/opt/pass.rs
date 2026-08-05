use crate::{
    ir::{Function, FunctionData, Program, arena::Arena},
    opt::{
        config::{LoopUnrollMode, OptimizationLevel, PassesConfig, TargetPolicy},
        passes::*,
        stats::PassesRunStats,
    },
};
use std::sync::{Arc, Mutex};

pub struct ArenaContext<'a> {
    pub program: &'a Program,
    pub curr_func: Option<Function>,
}

impl ArenaContext<'_> {
    pub fn curr_func_data(&self) -> &FunctionData {
        self.func_data(self.curr_func.unwrap())
    }
}

impl Arena for ArenaContext<'_> {
    fn local(&self) -> &crate::ir::arena::LocalArena {
        self.program
            .func_data(self.curr_func.unwrap())
            .local_arena()
    }

    fn local_mut(&mut self) -> &mut crate::ir::arena::LocalArena {
        unimplemented!()
    }

    fn global(&self) -> &crate::ir::arena::GlobalArena {
        self.program.global_arena()
    }

    fn global_mut(&mut self) -> &mut crate::ir::arena::GlobalArena {
        unimplemented!()
    }
}

pub struct ArenaContextMut<'a> {
    pub program: &'a mut Program,
    pub curr_func: Option<Function>,
}
impl ArenaContextMut<'_> {
    pub fn curr_func_data(&self) -> &FunctionData {
        self.func_data(self.curr_func.unwrap())
    }

    pub fn curr_func_data_mut(&mut self) -> &mut FunctionData {
        self.func_data_mut(self.curr_func.unwrap())
    }
}
impl std::ops::Deref for ArenaContextMut<'_> {
    type Target = FunctionData;
    fn deref(&self) -> &Self::Target {
        self.program.func_data(self.curr_func.unwrap())
    }
}

impl std::ops::DerefMut for ArenaContextMut<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.program.func_data_mut(self.curr_func.unwrap())
    }
}

impl Arena for ArenaContextMut<'_> {
    fn local(&self) -> &crate::ir::arena::LocalArena {
        self.program
            .func_data(self.curr_func.unwrap())
            .local_arena()
    }

    fn local_mut(&mut self) -> &mut crate::ir::arena::LocalArena {
        self.program
            .func_data_mut(self.curr_func.unwrap())
            .local_arena_mut()
    }

    fn global(&self) -> &crate::ir::arena::GlobalArena {
        self.program.global_arena()
    }

    fn global_mut(&mut self) -> &mut crate::ir::arena::GlobalArena {
        self.program.global_arena_mut()
    }
}

pub trait Pass: Send + Sync {
    /// Runs this pass over every function once and reports whether it changed IR.
    fn run(&mut self, program: &mut Program) -> bool {
        let func_layout = program.function_layout().to_vec();
        let mut arena_context = ArenaContextMut {
            program,
            curr_func: None,
        };
        let mut changed = false;
        for func in func_layout {
            arena_context.curr_func = Some(func);
            changed |= self.run_on(&mut arena_context);
        }
        changed
    }

    /// Compatibility for old code.
    /// Normally you should not !only! implement this function
    /// But you can implement both function at same time.
    /// Runs this pass on one function and reports whether it changed IR.
    fn run_on(&mut self, _data: &mut ArenaContextMut<'_>) -> bool {
        false
    }
}

pub struct PassesManager {
    passes: Vec<Box<dyn Pass>>,
    fixed_point_start: usize,
    stats: Arc<Mutex<PassesRunStats>>,
}

impl PassesManager {
    pub fn new() -> PassesManager {
        PassesManager {
            passes: Vec::new(),
            fixed_point_start: 0,
            stats: Arc::new(Mutex::new(PassesRunStats::default())),
        }
    }

    pub fn register(&mut self, pass: Box<dyn Pass>) {
        self.passes.push(pass);
    }

    /// Registers a normalization pass that runs before, but not inside, the
    /// optimization fixed point.
    pub fn register_initial(&mut self, pass: Box<dyn Pass>) {
        assert_eq!(self.fixed_point_start, self.passes.len());
        self.passes.push(pass);
        self.fixed_point_start += 1;
    }

    pub fn run_passes(&mut self, program: &mut Program) -> PassesRunStats {
        const MAX_PIPELINE_ITERATIONS: usize = 100;

        for pass in &mut self.passes[..self.fixed_point_start] {
            pass.run(program);
        }

        for iteration in 0..MAX_PIPELINE_ITERATIONS {
            let changed = self.passes[self.fixed_point_start..]
                .iter_mut()
                .fold(false, |changed, pass| pass.run(program) || changed);
            if !changed {
                self.stats.lock().unwrap().fixed_point_iterations = iteration + 1;
                return self.stats.lock().unwrap().clone();
            }
            if iteration + 1 == MAX_PIPELINE_ITERATIONS {
                panic!(
                    "optimization pipeline did not converge after {MAX_PIPELINE_ITERATIONS} iterations"
                );
            }
        }
        unreachable!()
    }

    /// The AArch64 pipeline: the common pipeline plus `chain_to_switch`,
    /// which shapes equality chains for the AArch64 `chain_fusion` backend
    /// pass. RISC-V keeps the common pipeline (its branches materialize
    /// conditions into registers, so the tree would not amortize).
    pub fn aarch64() -> PassesManager {
        Self::from_config(PassesConfig::new(
            OptimizationLevel::O2,
            TargetPolicy::aarch64(),
        ))
    }

    pub fn from_config(config: PassesConfig) -> PassesManager {
        let mut p = PassesManager::new();
        if config.opt_level == OptimizationLevel::O0 {
            return p;
        }
        let ssa = Box::new(ssa::SSATransform);
        p.register_initial(ssa);

        let specialize = Box::new(specialize::Specialize::default());
        p.register_initial(specialize);

        // Recognize the "multiply by doubling" modular-multiplication
        // recursion (fft0/fft1) and rewrite it to a `b < 0` guard plus a call
        // to the `soyo_mulmod` builtin, which the AArch64 backend expands
        // inline (`smull; sxtw; sdiv; msub`). Runs before `inline` so the
        // now-tiny callee is inlined into its callers, and after `specialize`
        // (which skips self-recursive candidates like `multiply`). AArch64-only;
        // RISC-V keeps the recursion.
        if config.target.enable_chain_to_switch {
            let mulmod = Box::new(mulmod_recognize::MulmodRecognize);
            p.register_initial(mulmod);
        }

        // Memoize a pure self-recursive function with an additive accumulator
        // whose single callsite drives a forward induction loop (h-1 family).
        // The rewrite allocates a runtime-sized cache via the AArch64-only
        // `soyo_calloc` builtin, so it must run before `inline` (which would
        // flatten the now-redundant body) and is gated to AArch64.
        if config.memoize && config.target.enable_chain_to_switch {
            let memoize = Box::new(recursive_memoize::RecursiveMemoize);
            p.register_initial(memoize);
        }

        let inline = Box::new(inline::Inline);
        p.register_initial(inline);

        let tco_initial = Box::new(tco::TailCallElim);
        p.register_initial(tco_initial);

        // Change eligible two-dimensional arrays to the layout favored by
        // their hottest innermost-loop accesses before GEPs are reshaped.
        let column_major = Box::new(column_major::ColumnMajor);
        p.register_initial(column_major);

        // Promote unobservable scalar globals to SSA values so the
        // backend keeps them in registers (load once, write back once).
        // One-shot (not a fixpoint rewrite): it must run after inlining
        // so the callee-touch analysis sees the final call graph.
        let gsp = Box::new(scalar_global_promotion::ScalarGlobalPromotion);
        p.register_initial(gsp);

        let ipsccp = Box::new(ipsccp::IPSCCP);
        p.register(ipsccp);

        let simplify_cfg = Box::new(simplify_cfg::SimplifyCFG);
        p.register(simplify_cfg);

        // Fully unroll small exact-trip loops while they are still in the
        // test-at-top form recognized by the induction analysis.
        if config.loop_unroll != LoopUnrollMode::Disabled {
            let loop_unroll = Box::new(loop_unroll::LoopUnroll::new(
                config.loop_unroll,
                Arc::clone(&p.stats),
                config.collect_stats,
            ));
            p.register(loop_unroll);
        }

        // Rotate test-at-top countdown loops to test-at-bottom so the
        // backend can fuse the decrement with the loop test.
        let rotate_loops = Box::new(rotate_loops::RotateLoops);
        p.register(rotate_loops);

        // Collapse zero-initialization loops into a single runtime-length
        // `MemZero` (`bl memset` on AArch64). AArch64-only for now; it runs
        // after rotation so it sees the countdown form.
        if config.target.enable_chain_to_switch {
            let zero_store_loop = Box::new(zero_store_loop::ZeroStoreLoop);
            p.register(zero_store_loop);
        }

        // Balanced decision tree for equality chains; the AArch64 backend
        // fuses each (eq, lt) node pair into a single compare.
        if config.target.enable_chain_to_switch {
            let chain_to_switch = Box::new(chain_to_switch::ChainToSwitch);
            p.register(chain_to_switch);
        }

        // Hoist loop-invariant pure expressions to the preheader.
        let licm = Box::new(licm::LICM::with_computed_load_limit(
            config.target.enable_chain_to_switch,
        ));
        p.register(licm);

        let gvn = Box::new(gvn::GlobalInstNumbering);
        p.register(gvn);

        // Dead store elimination: drops GSP redundant write-backs and
        // covered stores so later passes see a cleaner memory image.
        let dse = Box::new(dse::DSE);
        p.register(dse);

        let pointer_sr = Box::new(pointer_strength_reduction::PointerStrengthReduction);
        p.register(pointer_sr);

        // Fold `br (x < 0), A, B` when range analysis (with the M61 pure
        // non-negativity summaries) proves the modmul guard is never taken.
        let guard_elimination = Box::new(guard_elimination::GuardElimination);
        p.register(guard_elimination);

        // Fold `x % P` into a conditional subtraction when the dividend is
        // provably in `[0, 2P)` (one `sub; cmp; csel` instead of a 4-6
        // instruction magic-number sequence). Runs after guard elimination so
        // the range facts are stable.
        let mod_fold = Box::new(mod_fold::ModFold);
        p.register(mod_fold);

        let sr = Box::new(sr::StrengthReduction);
        p.register(sr);

        // Interchange the in-place GEMM nest i-j-k → i-k-j with a stack row
        // buffer so the innermost loop walks A row-contiguously (many_mat_cal
        // hotspot). Runs after strength reduction (which turns the direct
        // `A[k][j]` index into a pointer-carrying inner reduction) but before
        // invariant-reduction hoisting so it sees the pristine i-j-k nest.
        if config.target.enable_chain_to_switch {
            let matmul_interchange = Box::new(matmul_interchange::MatmulInterchange);
            p.register(matmul_interchange);
        }

        // Degrade an outer trip loop whose body is an invariant reduction nest
        // to a single `acc += D_total` per iteration (many_mat_cal hotspot).
        // Runs before reduction_unroll so it sees the pristine nested loops.
        let invariant_reduction_hoisting =
            Box::new(invariant_reduction_hoisting::InvariantReductionHoisting);
        p.register(invariant_reduction_hoisting);

        // Split single-accumulator reduction loops into four independent lanes
        // (breaks the serial accumulation dependency chain on AArch64).
        let reduction_unroll = Box::new(reduction_unroll::ReductionUnroll);
        p.register(reduction_unroll);

        // Register-block the strided matrix-reduction loop (`acc -= A[i][k] *
        // B[k][j]`): four accumulator lanes plus four column pointers overlap
        // the column-load cache misses (h-5 / matmul). Runs after
        // `reduction_unroll`, which handles the plain `acc += a[j]` shape.
        if config.blocked_reduction {
            let blocked_reduction = Box::new(blocked_reduction::BlockedReduction);
            p.register(blocked_reduction);
        }

        let if_conversion = Box::new(if_conversion::IfConversion);
        p.register(if_conversion);

        // A second TCO pass catches tail calls exposed by the
        // simplification passes above.
        let tco = Box::new(tco::TailCallElim);
        p.register(tco);

        // Turn `call F(args)` on a pure self-tail-recursive loop into an
        // inlined loop (clang-style): the callee's self `tail_call` becomes a
        // back-edge in the caller. Runs after TCO so the loop form is visible.
        let tail_recursive_inline = Box::new(tail_recursive_inline::TailRecursiveInline);
        p.register(tail_recursive_inline);

        let boolean_simplification = Box::new(boolean_simplify::BooleanSimplification);
        p.register(boolean_simplification);

        let gvn_pre = Box::new(gvn_pre::GVNPRE);
        p.register(gvn_pre);

        let dpe = dce::DeadPhiElimination;
        p.register(Box::new(dpe));

        let dce = dce::DeadCodeElimination;
        p.register(Box::new(dce));

        // Drop whole functions unreachable from `main` (dead functions):
        // instruction-level DCE keeps them, dragging their internal calls
        // into the emitted assembly. ABI observation tests compile through a
        // without-dead-function-elimination pipeline, since they assert on
        // the parameter binding of optimized-but-unreachable helpers.
        if config.dead_function_elimination {
            let dfe = dce::DeadFunctionElimination;
            p.register(Box::new(dfe));
        }

        p
    }
}

impl Default for PassesManager {
    fn default() -> Self {
        Self::from_config(PassesConfig::new(
            OptimizationLevel::O2,
            TargetPolicy::riscv64(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    struct Pass(AtomicUsize);

    impl super::Pass for Pass {
        fn run_on(&mut self, _data: &mut ArenaContextMut<'_>) -> bool {
            self.0.fetch_add(1, Ordering::Relaxed) == 0
        }
    }

    struct FirstPass(Arc<AtomicUsize>);

    impl super::Pass for FirstPass {
        fn run_on(&mut self, _data: &mut ArenaContextMut<'_>) -> bool {
            self.0
                .compare_exchange(1, 2, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        }
    }

    struct SecondPass(Arc<AtomicUsize>);

    impl super::Pass for SecondPass {
        fn run_on(&mut self, _data: &mut ArenaContextMut<'_>) -> bool {
            self.0
                .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        }
    }

    #[test]
    fn run_reports_per_function_changes() {
        let mut program = Program::new();
        program.new_function(crate::ir::Type::get_unit(), "test".into(), vec![]);
        let mut pass = Pass(AtomicUsize::new(0));

        assert!(super::Pass::run(&mut pass, &mut program));
        assert!(!super::Pass::run(&mut pass, &mut program));
        assert_eq!(pass.0.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn manager_repeats_the_pipeline_after_a_later_pass_changes_ir() {
        let state = Arc::new(AtomicUsize::new(0));
        let mut manager = PassesManager::new();
        manager.register(Box::new(FirstPass(Arc::clone(&state))));
        manager.register(Box::new(SecondPass(Arc::clone(&state))));
        let mut program = Program::new();
        program.new_function(crate::ir::Type::get_unit(), "test".into(), vec![]);

        manager.run_passes(&mut program);

        assert_eq!(state.load(Ordering::Relaxed), 2);
    }
}
