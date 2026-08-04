use crate::{
    ir::{Function, FunctionData, Program, arena::Arena},
    opt::passes::*,
};

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
}

impl PassesManager {
    pub fn new() -> PassesManager {
        PassesManager {
            passes: Vec::new(),
            fixed_point_start: 0,
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

    pub fn run_passes(&mut self, program: &mut Program) {
        const MAX_PIPELINE_ITERATIONS: usize = 100;

        for pass in &mut self.passes[..self.fixed_point_start] {
            pass.run(program);
        }

        for iteration in 0..MAX_PIPELINE_ITERATIONS {
            let changed = self.passes[self.fixed_point_start..]
                .iter_mut()
                .fold(false, |changed, pass| pass.run(program) || changed);
            if !changed {
                return;
            }
            if iteration + 1 == MAX_PIPELINE_ITERATIONS {
                panic!(
                    "optimization pipeline did not converge after {MAX_PIPELINE_ITERATIONS} iterations"
                );
            }
        }
    }

    /// The AArch64 pipeline: the common pipeline plus `chain_to_switch`,
    /// which shapes equality chains for the AArch64 `chain_fusion` backend
    /// pass. RISC-V keeps the common pipeline (its branches materialize
    /// conditions into registers, so the tree would not amortize).
    pub fn aarch64() -> PassesManager {
        Self::build_pass_list(true)
    }

    fn build_pass_list(with_chain_to_switch: bool) -> PassesManager {
        let mut p = PassesManager::new();
        let ssa = Box::new(ssa::SSATransform);
        p.register_initial(ssa);

        let specialize = Box::new(specialize::Specialize::default());
        p.register_initial(specialize);

        let inline = Box::new(inline::Inline);
        p.register_initial(inline);

        let tco_initial = Box::new(tco::TailCallElim);
        p.register_initial(tco_initial);

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

        // Rotate test-at-top countdown loops to test-at-bottom so the
        // backend can fuse the decrement with the loop test.
        let rotate_loops = Box::new(rotate_loops::RotateLoops);
        p.register(rotate_loops);

        // Balanced decision tree for equality chains; the AArch64 backend
        // fuses each (eq, lt) node pair into a single compare.
        if with_chain_to_switch {
            let chain_to_switch = Box::new(chain_to_switch::ChainToSwitch);
            p.register(chain_to_switch);
        }

        // Hoist loop-invariant pure expressions to the preheader.
        let licm = Box::new(licm::LICM::new());
        p.register(licm);

        let gvn = Box::new(gvn::GlobalInstNumbering);
        p.register(gvn);

        // Dead store elimination: drops GSP redundant write-backs and
        // covered stores so later passes see a cleaner memory image.
        let dse = Box::new(dse::DSE);
        p.register(dse);

        let pointer_sr = Box::new(pointer_strength_reduction::PointerStrengthReduction);
        p.register(pointer_sr);

        let sr = Box::new(sr::StrengthReduction);
        p.register(sr);

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

        let if_conversion = Box::new(if_conversion::IfConversion);
        p.register(if_conversion);

        // A second TCO pass catches tail calls exposed by the
        // simplification passes above.
        let tco = Box::new(tco::TailCallElim);
        p.register(tco);

        let boolean_simplification = Box::new(boolean_simplify::BooleanSimplification);
        p.register(boolean_simplification);

        let gvn_pre = Box::new(gvn_pre::GVNPRE);
        p.register(gvn_pre);

        let dpe = dce::DeadPhiElimination;
        p.register(Box::new(dpe));

        let dce = dce::DeadCodeElimination;
        p.register(Box::new(dce));

        p
    }
}

impl Default for PassesManager {
    fn default() -> Self {
        Self::build_pass_list(false)
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
