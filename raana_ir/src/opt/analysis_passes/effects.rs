//! Complete purity / memory-effect analysis (interprocedural).
//!
//! Two cooperating fixpoints over the call graph:
//!
//! 1. **Points-to sets** (top-down): for every function parameter, the set
//!    of concrete objects it may point to — globals, stack objects (tagged
//!    with their defining function), or `Unknown`. Flattened to concrete
//!    objects only, so recursive/cyclic call graphs converge.
//! 2. **Effect summaries** (bottom-up): per function, the set of objects it
//!    may read / write (in its own terms: globals, its own allocs, `Param(i)`
//!    symbols), plus unknown-provenance and stdin/stdout flags.
//!
//! The summaries answer the classic purity questions:
//! - `is_pure`: no writes to memory or stdout → movable/duplicable;
//! - `is_removable`: additionally no stdin reads → deletable when unused;
//! - `may_write_memory`: a memory barrier for load hoisting / CSE.
//!
//! SysY's restricted memory model (no pointers, no heap) makes the base
//! object of every access decidable (`memory::BaseEnv`); the interprocedural
//! `alias` here refines the intra-procedural rules for argument pairs using
//! the points-to sets (see `docs/memory_alias_analysis.md` §3-§4).
//!
//! The sysylib boundary (`soyo_compiler/src/frontend/utils.rs`) is modeled
//! explicitly: scalar I/O reads stdin / writes stdout; `getarray`/`putarray`
//! read/write their array argument; timing functions touch neither program
//! memory nor program-visible state beyond I/O.

use rustc_hash::FxHashMap as HashMap;
use rustc_hash::FxHashSet as HashSet;

use crate::{
    ir::{Function, Inst, InstKind, Program, arena::Arena},
    opt::{
        analysis_passes::memory::{AliasResult, BaseEnv, MemObject},
        pass::ArenaContext,
    },
};

/// A concrete abstract memory object used in points-to sets and effect
/// summaries. `Alloc` objects carry their defining function because local
/// instruction ids repeat across functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AbstractObject {
    /// A global object.
    Global(Inst),
    /// A stack object of the given function.
    Alloc(Function, Inst),
    /// Unknown provenance: may be anything.
    Unknown,
}

/// An effect target in a function's own terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffectObject {
    /// A global object.
    Global(Inst),
    /// A stack object of the given function (its own frame when the
    /// function is the summary's owner).
    Alloc(Function, Inst),
    /// The function's parameter at this index.
    Param(usize),
}

/// Memory / I/O effects of one function.
#[derive(Debug, Clone, Default)]
pub struct FunctionEffects {
    /// Objects the function may read.
    pub reads: HashSet<EffectObject>,
    /// Objects the function may write.
    pub writes: HashSet<EffectObject>,
    /// Reads through an unknown-provenance address.
    pub reads_unknown: bool,
    /// Writes through an unknown-provenance address.
    pub writes_unknown: bool,
    /// Reads stdin (consumes input; observable).
    pub reads_io: bool,
    /// Writes stdout (observable).
    pub writes_io: bool,
}

impl FunctionEffects {
    /// No writes to memory or I/O: the function may be freely moved,
    /// duplicated, or deleted when its result is unused.
    pub fn is_pure(&self) -> bool {
        self.writes.is_empty() && !self.writes_unknown && !self.writes_io
    }

    /// No observable effects at all (may still read memory): removable when
    /// its result is unused.
    pub fn is_removable(&self) -> bool {
        self.is_pure() && !self.reads_io
    }

    /// The function may write program memory (through known or unknown
    /// addresses): a barrier for load hoisting / load CSE.
    pub fn may_write_memory(&self) -> bool {
        !self.writes.is_empty() || self.writes_unknown
    }
}

/// A call site: caller, callee, and the actual arguments (positionally
/// matching the callee's parameters).
struct CallSiteInfo {
    caller: Function,
    callee: Function,
    args: Vec<Inst>,
}

/// Whole-program effect analysis: per-function base environments, points-to
/// sets, and effect summaries. Built once per `Pass::run`; immutable
/// afterwards.
pub struct EffectAnalysis {
    envs: HashMap<Function, BaseEnv>,
    effects: HashMap<Function, FunctionEffects>,
    points_to: HashMap<(Function, usize), HashSet<AbstractObject>>,
}

/// Fixed summaries for the sysylib declarations (see
/// `soyo_compiler/src/frontend/utils.rs`). `None` for unknown declarations.
fn decl_effects(name: &str) -> Option<FunctionEffects> {
    let mut fx = FunctionEffects::default();
    match name {
        "getint" | "getch" | "getfloat" => {
            fx.reads_io = true;
        }
        "getarray" | "getfarray" => {
            fx.reads_io = true;
            fx.writes.insert(EffectObject::Param(0));
        }
        "putint" | "putch" | "putfloat" | "putf" => {
            fx.writes_io = true;
        }
        "putarray" | "putfarray" => {
            fx.writes_io = true;
            fx.reads.insert(EffectObject::Param(0));
        }
        "_sysy_starttime" | "_sysy_stoptime" => {
            fx.reads_io = true;
            fx.writes_io = true;
        }
        _ => return None,
    }
    Some(fx)
}

impl EffectAnalysis {
    pub fn new(program: &Program) -> EffectAnalysis {
        let mut envs = HashMap::default();
        let mut effects = HashMap::default();
        let mut call_sites: Vec<CallSiteInfo> = Vec::new();

        for &func in program.function_layout() {
            let data = program.func_data(func);
            if data.layout().is_decl() {
                let fx = decl_effects(data.name()).unwrap_or_else(|| {
                    // Unknown declaration: assume the worst.
                    FunctionEffects {
                        reads_unknown: true,
                        writes_unknown: true,
                        reads_io: true,
                        writes_io: true,
                        ..Default::default()
                    }
                });
                effects.insert(func, fx);
                continue;
            }
            envs.insert(func, BaseEnv::new(data));
            effects.insert(func, direct_effects(program, func));

            for bb_layout in data.layout().basicblocks() {
                for &inst in bb_layout.insts() {
                    match data.inst_data(inst).kind() {
                        InstKind::Call(call) => call_sites.push(CallSiteInfo {
                            caller: func,
                            callee: call.callee(),
                            args: call.args().to_vec(),
                        }),
                        InstKind::TailCall(tail_call) => call_sites.push(CallSiteInfo {
                            caller: func,
                            callee: tail_call.callee(),
                            args: tail_call.args().to_vec(),
                        }),
                        _ => {}
                    }
                }
            }
        }

        let mut analysis = EffectAnalysis {
            envs,
            effects,
            points_to: HashMap::default(),
        };
        analysis.propagate_points_to(program, &call_sites);
        analysis.propagate_effects(program, &call_sites);
        analysis
    }

    pub fn env_of(&self, func: Function) -> &BaseEnv {
        &self.envs[&func]
    }

    pub fn effects_of(&self, func: Function) -> &FunctionEffects {
        &self.effects[&func]
    }

    pub fn is_pure(&self, func: Function) -> bool {
        self.effects_of(func).is_pure()
    }

    /// Whether a call to `func` can be removed when its result is unused.
    pub fn is_removable(&self, func: Function) -> bool {
        self.effects_of(func).is_removable()
    }

    /// The points-to set of `func`'s parameter `index`, if any call site
    /// contributes objects to it.
    pub fn points_to_of(&self, func: Function, index: usize) -> Option<&HashSet<AbstractObject>> {
        self.points_to.get(&(func, index))
    }

    /// The set of concrete abstract objects `addr` (inside `func`) may point
    /// to. `None` means unknown provenance (may point to anything).
    pub fn targets_of<A: Arena + ?Sized>(
        &self,
        arena: &A,
        func: Function,
        addr: Inst,
    ) -> Option<HashSet<AbstractObject>> {
        let base = self.env_of(func).base_of(arena, addr);
        match base {
            MemObject::Alloc(a) => Some(HashSet::from_iter([AbstractObject::Alloc(func, a)])),
            MemObject::Global(g) => Some(HashSet::from_iter([AbstractObject::Global(g)])),
            MemObject::Param(i) => {
                let set = self.points_to.get(&(func, i));
                match set {
                    Some(set) if !set.contains(&AbstractObject::Unknown) => Some(set.clone()),
                    _ => None,
                }
            }
            MemObject::Unknown => None,
        }
    }

    /// Whether a call to `callee` may write to any object in `targets`.
    /// `targets == None` means the queried address may be anything.
    pub fn call_may_write(
        &self,
        callee: Function,
        targets: Option<&HashSet<AbstractObject>>,
    ) -> bool {
        let fx = self.effects_of(callee);
        if fx.writes_unknown {
            return true;
        }
        let Some(targets) = targets else {
            return fx.may_write_memory();
        };
        fx.writes.iter().any(|w| match w {
            EffectObject::Global(g) => targets.contains(&AbstractObject::Global(*g)),
            EffectObject::Alloc(cf, a) => targets
                .contains(&AbstractObject::Alloc(*cf, *a)),
            EffectObject::Param(j) => self
                .points_to
                .get(&(callee, *j))
                .into_iter()
                .flatten()
                .any(|o| match o {
                    AbstractObject::Global(g) => targets.contains(&AbstractObject::Global(*g)),
                    AbstractObject::Alloc(cf, a) => {
                        targets.contains(&AbstractObject::Alloc(*cf, *a))
                    }
                    AbstractObject::Unknown => true,
                }),
        })
    }

    /// Interprocedural alias result between two addresses inside `func`,
    /// refining the intra-procedural rules with the points-to sets.
    pub fn alias<A: Arena + ?Sized>(
        &self,
        arena: &A,
        func: Function,
        a: Inst,
        b: Inst,
    ) -> AliasResult {
        let env = self.env_of(func);
        let base_a = env.base_of(arena, a);
        let base_b = env.base_of(arena, b);
        use MemObject::*;
        match (base_a, base_b) {
            (Param(i), Param(j)) if i != j => {
                // Disjoint points-to sets (an absent set is empty) and no
                // unknown provenance prove the parameters never alias.
                let sa = self.points_to.get(&(func, i));
                let sb = self.points_to.get(&(func, j));
                let disjoint = match (sa, sb) {
                    (Some(sa), Some(sb)) => {
                        !sa.contains(&AbstractObject::Unknown)
                            && !sb.contains(&AbstractObject::Unknown)
                            && sa.intersection(sb).next().is_none()
                    }
                    _ => true,
                };
                if disjoint {
                    AliasResult::NoAlias
                } else {
                    AliasResult::MayAlias
                }
            }
            (Param(i), Global(g)) | (Global(g), Param(i)) => {
                let s = self.points_to.get(&(func, i));
                let no_alias = match s {
                    Some(s) => {
                        !s.contains(&AbstractObject::Unknown)
                            && !s.contains(&AbstractObject::Global(g))
                    }
                    None => true,
                };
                if no_alias {
                    AliasResult::NoAlias
                } else {
                    AliasResult::MayAlias
                }
            }
            (Param(i), Alloc(a)) | (Alloc(a), Param(i)) => {
                let s = self.points_to.get(&(func, i));
                let no_alias = match s {
                    Some(s) => {
                        !s.contains(&AbstractObject::Unknown)
                            && !s.contains(&AbstractObject::Alloc(func, a))
                    }
                    None => true,
                };
                if no_alias {
                    AliasResult::NoAlias
                } else {
                    AliasResult::MayAlias
                }
            }
            _ => env.alias_with_bases(arena, a, b, base_a, base_b),
        }
    }

    /// Top-down points-to propagation over the call graph, to fixpoint.
    fn propagate_points_to(&mut self, program: &Program, call_sites: &[CallSiteInfo]) {
        loop {
            let mut changed = false;
            // Collect the contributions first: classification reads the
            // current sets while the application writes them.
            let mut additions: Vec<((Function, usize), AbstractObject)> = Vec::new();
            for site in call_sites {
                let env = &self.envs[&site.caller];
                let ctx = ArenaContext {
                    program,
                    curr_func: Some(site.caller),
                };
                for (index, &arg) in site.args.iter().enumerate() {
                    for obj in classify_actual(env, &ctx, &self.points_to, site.caller, arg) {
                        additions.push(((site.callee, index), obj));
                    }
                }
            }
            for (key, obj) in additions {
                changed |= self.points_to.entry(key).or_default().insert(obj);
            }
            if !changed {
                return;
            }
        }
    }

    /// Bottom-up effect propagation over the call graph, to fixpoint.
    fn propagate_effects(&mut self, program: &Program, call_sites: &[CallSiteInfo]) {
        loop {
            let mut changed = false;
            let mut deltas: HashMap<Function, FunctionEffects> = HashMap::default();
            for site in call_sites {
                let callee_fx = self.effects_of(site.callee).clone();
                let mut delta = FunctionEffects::default();
                for &obj in &callee_fx.reads {
                    apply_effect_object(
                        obj,
                        site.callee,
                        &self.points_to,
                        &mut delta.reads,
                        &mut delta.reads_unknown,
                    );
                }
                for &obj in &callee_fx.writes {
                    apply_effect_object(
                        obj,
                        site.callee,
                        &self.points_to,
                        &mut delta.writes,
                        &mut delta.writes_unknown,
                    );
                }
                delta.reads_unknown |= callee_fx.reads_unknown;
                delta.writes_unknown |= callee_fx.writes_unknown;
                delta.reads_io |= callee_fx.reads_io;
                delta.writes_io |= callee_fx.writes_io;
                *deltas.entry(site.caller).or_default() |= delta;
            }
            for (func, delta) in deltas {
                changed |= merge_effects(self.effects.get_mut(&func).unwrap(), delta);
            }
            let _ = program;
            if !changed {
                return;
            }
        }
    }
}

/// Substitute one effect object of the callee into caller terms, using the
/// callee's points-to sets. The callee's own frame (`Alloc(callee, ·)`) is
/// invisible to the caller and dropped.
fn apply_effect_object(
    obj: EffectObject,
    callee: Function,
    points_to: &HashMap<(Function, usize), HashSet<AbstractObject>>,
    out: &mut HashSet<EffectObject>,
    out_unknown: &mut bool,
) {
    match obj {
        EffectObject::Global(g) => {
            out.insert(EffectObject::Global(g));
        }
        EffectObject::Alloc(cf, _a) if cf == callee => {}
        EffectObject::Alloc(cf, a) => {
            out.insert(EffectObject::Alloc(cf, a));
        }
        EffectObject::Param(j) => {
            let set = points_to.get(&(callee, j));
            let Some(set) = set else { return };
            for o in set {
                match o {
                    AbstractObject::Global(g) => {
                        out.insert(EffectObject::Global(*g));
                    }
                    AbstractObject::Alloc(cf, _a) if *cf == callee => {}
                    AbstractObject::Alloc(cf, a) => {
                        out.insert(EffectObject::Alloc(*cf, *a));
                    }
                    AbstractObject::Unknown => *out_unknown = true,
                }
            }
        }
    }
}

fn merge_effects(dst: &mut FunctionEffects, delta: FunctionEffects) -> bool {
    let mut changed = false;
    for &obj in &delta.reads {
        changed |= dst.reads.insert(obj);
    }
    for &obj in &delta.writes {
        changed |= dst.writes.insert(obj);
    }
    changed |= !dst.reads_unknown && delta.reads_unknown;
    dst.reads_unknown |= delta.reads_unknown;
    changed |= !dst.writes_unknown && delta.writes_unknown;
    dst.writes_unknown |= delta.writes_unknown;
    changed |= !dst.reads_io && delta.reads_io;
    dst.reads_io |= delta.reads_io;
    changed |= !dst.writes_io && delta.writes_io;
    dst.writes_io |= delta.writes_io;
    changed
}

impl std::ops::BitOrAssign for FunctionEffects {
    fn bitor_assign(&mut self, rhs: FunctionEffects) {
        self.reads.extend(rhs.reads);
        self.writes.extend(rhs.writes);
        self.reads_unknown |= rhs.reads_unknown;
        self.writes_unknown |= rhs.writes_unknown;
        self.reads_io |= rhs.reads_io;
        self.writes_io |= rhs.writes_io;
    }
}

/// Direct (call-free) effects of a function body.
fn direct_effects(program: &Program, func: Function) -> FunctionEffects {
    let mut fx = FunctionEffects::default();
    let data = program.func_data(func);
    let env = BaseEnv::new(data);
    let ctx = ArenaContext {
        program,
        curr_func: Some(func),
    };
    for bb_layout in data.layout().basicblocks() {
        for &inst in bb_layout.insts() {
            match data.inst_data(inst).kind() {
                InstKind::Load(load) => {
                    add_address_effect(&mut fx.reads, &mut fx.reads_unknown, &env, &ctx, func, load.src());
                }
                InstKind::Store(store) => {
                    add_address_effect(
                        &mut fx.writes,
                        &mut fx.writes_unknown,
                        &env,
                        &ctx,
                        func,
                        store.dest(),
                    );
                }
                InstKind::MemZero(mem_zero) => {
                    add_address_effect(
                        &mut fx.writes,
                        &mut fx.writes_unknown,
                        &env,
                        &ctx,
                        func,
                        mem_zero.dest(),
                    );
                }
                _ => {}
            }
        }
    }
    fx
}

fn add_address_effect(
    out: &mut HashSet<EffectObject>,
    out_unknown: &mut bool,
    env: &BaseEnv,
    ctx: &ArenaContext<'_>,
    func: Function,
    addr: Inst,
) {
    match env.base_of(ctx, addr) {
        MemObject::Alloc(a) => {
            out.insert(EffectObject::Alloc(func, a));
        }
        MemObject::Global(g) => {
            out.insert(EffectObject::Global(g));
        }
        MemObject::Param(i) => {
            out.insert(EffectObject::Param(i));
        }
        MemObject::Unknown => *out_unknown = true,
    }
}

/// Classify a call-site actual argument into the concrete objects it may
/// point to.
fn classify_actual(
    env: &BaseEnv,
    ctx: &ArenaContext<'_>,
    points_to: &HashMap<(Function, usize), HashSet<AbstractObject>>,
    func: Function,
    arg: Inst,
) -> HashSet<AbstractObject> {
    match env.base_of(ctx, arg) {
        MemObject::Alloc(a) => HashSet::from_iter([AbstractObject::Alloc(func, a)]),
        MemObject::Global(g) => HashSet::from_iter([AbstractObject::Global(g)]),
        MemObject::Param(i) => points_to.get(&(func, i)).cloned().unwrap_or_default(),
        MemObject::Unknown => HashSet::from_iter([AbstractObject::Unknown]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{Program, Type, builder_trait::{GlobalInstBuilder, LocalInstBuilder, ScalarInstBuilder}},
        opt::pass::{ArenaContext, ArenaContextMut},
    };

    fn new_global(program: &mut Program) -> Inst {
        let init = program.new_value().zero_init(Type::get_i32());
        program.new_value().global_alloc(init)
    }

    /// A function `name(params)` with an entry block (no terminator yet;
    /// callers append their own instructions and a `ret`).
    fn new_body(program: &mut Program, name: &str, params: Vec<Type>) -> Function {
        let function = program.new_function(Type::get_unit(), name.into(), params);
        let mut data = ArenaContextMut {
            program,
            curr_func: Some(function),
        };
        data.add_entry_block();
        function
    }

    #[test]
    fn pure_function_without_memory_ops() {
        let mut program = Program::new();
        let f = new_body(&mut program, "f", vec![]);
        let analysis = EffectAnalysis::new(&program);
        assert!(analysis.is_pure(f));
        assert!(analysis.is_removable(f));
    }

    #[test]
    fn store_to_global_is_impure_and_recorded() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        let f = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let mut data = ArenaContextMut { program: &mut program, curr_func: Some(f) };
        let entry = data.add_entry_block();
        let one = data.new_local_value().integer(1);
        let store = data.new_local_value().store(one, global);
        data.layout_mut().insert_inst(entry, store);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let analysis = EffectAnalysis::new(&program);
        let fx = analysis.effects_of(f);
        assert!(fx.writes.contains(&EffectObject::Global(global)));
        assert!(!analysis.is_pure(f));
        assert!(!analysis.is_removable(f));
    }

    #[test]
    fn load_of_global_is_read_only_but_pure() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        let f = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let mut data = ArenaContextMut { program: &mut program, curr_func: Some(f) };
        let entry = data.add_entry_block();
        let load = data.new_local_value().load(global);
        data.layout_mut().insert_inst(entry, load);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let analysis = EffectAnalysis::new(&program);
        let fx = analysis.effects_of(f);
        assert!(fx.reads.contains(&EffectObject::Global(global)));
        assert!(analysis.is_pure(f)); // reads are not observable
        assert!(analysis.is_removable(f));
    }

    #[test]
    fn callee_effects_propagate_to_caller() {
        let mut program = Program::new();
        let global = new_global(&mut program);
        let writer = program.new_function(Type::get_unit(), "writer".into(), vec![]);
        let mut data = ArenaContextMut { program: &mut program, curr_func: Some(writer) };
        let entry = data.add_entry_block();
        let one = data.new_local_value().integer(1);
        let store = data.new_local_value().store(one, global);
        data.layout_mut().insert_inst(entry, store);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let caller = new_body(&mut program, "caller", vec![]);
        let mut data = ArenaContextMut { program: &mut program, curr_func: Some(caller) };
        let entry = data.layout().entry_bb().unwrap().bb();
        let call = data.new_local_value().call(writer, vec![]);
        data.layout_mut().insert_inst(entry, call);

        let analysis = EffectAnalysis::new(&program);
        let fx = analysis.effects_of(caller);
        assert!(fx.writes.contains(&EffectObject::Global(global)));
        assert!(!analysis.is_pure(caller));
    }

    #[test]
    fn param_write_is_substituted_at_call_site() {
        // `g` writes its parameter; `f` calls `g(global)` — f's write set
        // must contain the global.
        let mut program = Program::new();
        let global = new_global(&mut program);
        let g = program.new_function(
            Type::get_unit(),
            "g".into(),
            vec![Type::get_i32().reference()],
        );
        let mut data = ArenaContextMut { program: &mut program, curr_func: Some(g) };
        let entry = data.add_entry_block();
        let param = data.params()[0];
        let one = data.new_local_value().integer(1);
        let store = data.new_local_value().store(one, param);
        data.layout_mut().insert_inst(entry, store);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let f = new_body(&mut program, "f", vec![]);
        let mut data = ArenaContextMut { program: &mut program, curr_func: Some(f) };
        let entry = data.layout().entry_bb().unwrap().bb();
        let call = data.new_local_value().call(g, vec![global]);
        data.layout_mut().insert_inst(entry, call);

        let analysis = EffectAnalysis::new(&program);
        let fx = analysis.effects_of(f);
        assert!(fx.writes.contains(&EffectObject::Global(global)));
        assert!(!analysis.is_pure(f));
    }

    #[test]
    fn getint_is_io_and_not_removable() {
        let mut program = Program::new();
        let getint = program.new_function(Type::get_i32(), "getint".into(), vec![]);
        let f = new_body(&mut program, "f", vec![]);
        let mut data = ArenaContextMut { program: &mut program, curr_func: Some(f) };
        let entry = data.layout().entry_bb().unwrap().bb();
        let call = data.new_local_value().call(getint, vec![]);
        data.layout_mut().insert_inst(entry, call);

        let analysis = EffectAnalysis::new(&program);
        let fx = analysis.effects_of(getint);
        assert!(fx.reads_io);
        assert!(analysis.effects_of(f).reads_io); // propagated
        assert!(!analysis.is_removable(getint));
        assert!(!analysis.is_removable(f));
    }

    #[test]
    fn recursion_converges_conservatively() {
        let mut program = Program::new();
        let f = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference()],
        );
        let mut data = ArenaContextMut { program: &mut program, curr_func: Some(f) };
        let entry = data.add_entry_block();
        let param = data.params()[0];
        let one = data.new_local_value().integer(1);
        let store = data.new_local_value().store(one, param);
        data.layout_mut().insert_inst(entry, store);
        let call = data.new_local_value().call(f, vec![param]);
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let analysis = EffectAnalysis::new(&program);
        // Recursive self-call converges; the param write is still visible.
        let fx = analysis.effects_of(f);
        assert!(fx.writes.contains(&EffectObject::Param(0)));
    }

    #[test]
    fn points_to_refines_argument_vs_global_alias() {
        let mut program = Program::new();
        let global_a = new_global(&mut program);
        let global_b = new_global(&mut program);
        let f = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference()],
        );
        let mut data = ArenaContextMut { program: &mut program, curr_func: Some(f) };
        let entry = data.add_entry_block();
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);
        // main passes global_a to f.
        let main = new_body(&mut program, "main", vec![]);
        let mut data = ArenaContextMut { program: &mut program, curr_func: Some(main) };
        let entry = data.layout().entry_bb().unwrap().bb();
        let call = data.new_local_value().call(f, vec![global_a]);
        data.layout_mut().insert_inst(entry, call);

        let analysis = EffectAnalysis::new(&program);
        let pts = analysis.points_to_of(f, 0).unwrap();
        assert!(pts.contains(&AbstractObject::Global(global_a)));

        let ctx = ArenaContext {
            program: &program,
            curr_func: Some(f),
        };
        let param = program.func_data(f).params()[0];
        // param vs global_a: MayAlias (a may be passed to it)
        assert!(analysis
            .alias(&ctx, f, param, global_a)
            .may_alias());
        // param vs global_b: provably NoAlias
        assert_eq!(analysis.alias(&ctx, f, param, global_b), AliasResult::NoAlias);
    }

    #[test]
    fn points_to_disjoint_params_do_not_alias() {
        let mut program = Program::new();
        let global_a = new_global(&mut program);
        let global_b = new_global(&mut program);
        let f = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference(), Type::get_i32().reference()],
        );
        let mut data = ArenaContextMut { program: &mut program, curr_func: Some(f) };
        let entry = data.add_entry_block();
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let main = new_body(&mut program, "main", vec![]);
        let mut data = ArenaContextMut { program: &mut program, curr_func: Some(main) };
        let entry = data.layout().entry_bb().unwrap().bb();
        // Distinct globals: params can never alias.
        let call = data.new_local_value().call(f, vec![global_a, global_b]);
        data.layout_mut().insert_inst(entry, call);

        let analysis = EffectAnalysis::new(&program);
        let ctx = ArenaContext {
            program: &program,
            curr_func: Some(f),
        };
        let (p0, p1) = {
            let params = program.func_data(f).params();
            (params[0], params[1])
        };
        assert_eq!(analysis.alias(&ctx, f, p0, p1), AliasResult::NoAlias);
    }

    #[test]
    fn same_object_passed_twice_keeps_params_aliasing() {
        let mut program = Program::new();
        let global_a = new_global(&mut program);
        let f = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference(), Type::get_i32().reference()],
        );
        let mut data = ArenaContextMut { program: &mut program, curr_func: Some(f) };
        let entry = data.add_entry_block();
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let main = new_body(&mut program, "main", vec![]);
        let mut data = ArenaContextMut { program: &mut program, curr_func: Some(main) };
        let entry = data.layout().entry_bb().unwrap().bb();
        let call = data.new_local_value().call(f, vec![global_a, global_a]);
        data.layout_mut().insert_inst(entry, call);

        let analysis = EffectAnalysis::new(&program);
        let ctx = ArenaContext {
            program: &program,
            curr_func: Some(f),
        };
        let (p0, p1) = {
            let params = program.func_data(f).params();
            (params[0], params[1])
        };
        assert!(analysis.alias(&ctx, f, p0, p1).may_alias());
    }

    #[test]
    fn getarray_writes_its_argument() {
        let mut program = Program::new();
        let getarray = program.new_function(
            Type::get_i32(),
            "getarray".into(),
            vec![Type::get_i32().reference()],
        );
        let f = program.new_function(Type::get_unit(), "f".into(), vec![]);
        let mut data = ArenaContextMut { program: &mut program, curr_func: Some(f) };
        let entry = data.add_entry_block();
        let alloc = data.new_local_value().alloc(Type::get_array(Type::get_i32(), 4));
        data.layout_mut().insert_inst(entry, alloc);
        let call = data.new_local_value().call(getarray, vec![alloc]);
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let analysis = EffectAnalysis::new(&program);
        let fx = analysis.effects_of(f);
        // getarray writes into the local array: visible in f's write set.
        assert!(fx.writes.contains(&EffectObject::Alloc(f, alloc)));
        assert!(!analysis.is_pure(f));
        assert!(fx.reads_io);
    }

    #[test]
    fn local_allocs_never_alias_arguments() {
        let mut program = Program::new();
        let f = program.new_function(
            Type::get_unit(),
            "f".into(),
            vec![Type::get_i32().reference()],
        );
        let mut data = ArenaContextMut { program: &mut program, curr_func: Some(f) };
        let entry = data.add_entry_block();
        let alloc = data.new_local_value().alloc(Type::get_i32());
        data.layout_mut().insert_inst(entry, alloc);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let analysis = EffectAnalysis::new(&program);
        let ctx = ArenaContext {
            program: &program,
            curr_func: Some(f),
        };
        let param = program.func_data(f).params()[0];
        // Even with unknown points-to, a local never aliases an argument
        // of the same invocation (the frontend pointer-slot pattern makes
        // this the load-bearing case).
        assert_eq!(analysis.alias(&ctx, f, alloc, param), AliasResult::NoAlias);
    }
}
