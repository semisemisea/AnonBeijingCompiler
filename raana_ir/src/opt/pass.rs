use std::sync::OnceLock;

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
    fn run(&self, program: &mut Program) -> bool {
        let funcs = program.global_arena().func_arena().funcs();
        let mut arena_context = ArenaContextMut {
            program,
            curr_func: None,
        };
        let mut changed = false;
        for func in funcs {
            arena_context.curr_func = Some(func);
            changed |= self.run_on(&mut arena_context);
        }
        changed
    }

    /// Compatibility for old code.
    /// Normally you should not !only! implement this function
    /// But you can implement both function at same time.
    /// Runs this pass on one function and reports whether it changed IR.
    fn run_on(&self, _data: &mut ArenaContextMut<'_>) -> bool {
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

    pub fn run_passes(&self, program: &mut Program) {
        const MAX_PIPELINE_ITERATIONS: usize = 100;

        for pass in &self.passes[..self.fixed_point_start] {
            pass.run(program);
        }

        for iteration in 0..MAX_PIPELINE_ITERATIONS {
            let changed = self.passes[self.fixed_point_start..]
                .iter()
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

    pub fn default_ref() -> &'static PassesManager {
        DEFAULT_PASSES_LIST.get_or_init(|| {
            let mut p = PassesManager::new();
            let ssa = Box::new(ssa::SSATransform);
            p.register_initial(ssa);

            // let sccp = Box::new(const_prop::SparseConditionConstantPropagation);
            // p.register(sccp);
            let ipsccp = Box::new(ipsccp::IPSCCP);
            p.register(ipsccp);

            let simplify_cfg = Box::new(simplify_cfg::SimplifyCFG);
            p.register(simplify_cfg);

            let gvn = Box::new(gvn::GlobalInstNumbering);
            p.register(gvn);

            let sr = Box::new(sr::StrengthReduction);
            p.register(sr);

            let if_conversion = Box::new(if_conversion::IfConversion);
            p.register(if_conversion);

            let boolean_simplification = Box::new(boolean_simplify::BooleanSimplification);
            p.register(boolean_simplification);

            let dpe = dce::DeadPhiElimination;
            p.register(Box::new(dpe));
            let dce = dce::DeadCodeElimination;
            p.register(Box::new(dce));

            p
        })
    }
}

static DEFAULT_PASSES_LIST: OnceLock<PassesManager> = OnceLock::new();

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    struct Pass(AtomicUsize);

    impl super::Pass for Pass {
        fn run_on(&self, _data: &mut ArenaContextMut<'_>) -> bool {
            self.0.fetch_add(1, Ordering::Relaxed) == 0
        }
    }

    struct FirstPass(Arc<AtomicUsize>);

    impl super::Pass for FirstPass {
        fn run_on(&self, _data: &mut ArenaContextMut<'_>) -> bool {
            self.0
                .compare_exchange(1, 2, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        }
    }

    struct SecondPass(Arc<AtomicUsize>);

    impl super::Pass for SecondPass {
        fn run_on(&self, _data: &mut ArenaContextMut<'_>) -> bool {
            self.0
                .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        }
    }

    #[test]
    fn run_reports_per_function_changes() {
        let mut program = Program::new();
        program.new_function(crate::ir::Type::get_unit(), "test".into(), vec![]);
        let pass = Pass(AtomicUsize::new(0));

        assert!(super::Pass::run(&pass, &mut program));
        assert!(!super::Pass::run(&pass, &mut program));
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
