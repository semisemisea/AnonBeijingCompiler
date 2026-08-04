use crate::opt::prelude::*;

/// Names of the runtime-library entry points. These always have observable
/// side effects (I/O), so they can never be treated as pure.
pub fn is_library_function(name: &str) -> bool {
    matches!(
        name,
        "getint"
            | "getch"
            | "getfloat"
            | "getarray"
            | "getfarray"
            | "putint"
            | "putch"
            | "putfloat"
            | "putarray"
            | "putfarray"
            | "putf"
    )
}

/// A function is *locally pure* when its body performs no observable memory
/// operation: it may not load or store through a global address, write memory
/// with `MemZero`, or take the address of a global. Scalar reads/writes through
/// pointers that the callee cannot reach are invisible to the rest of the
/// program, so they do not make a function impure.
///
/// Callers must still check that every callee is pure (see
/// [`pure_functions`]); this only inspects the function's own body.
fn locally_pure(program: &Program, func: Function) -> bool {
    let data = program.func_data(func);
    if is_library_function(data.name()) {
        return false;
    }
    // Declarations (library-style entries with no body) have unknown
    // behaviour; never treat them as pure.
    if data.layout().entry_bb().is_none() {
        return false;
    }
    for bb in data.layout().basicblocks() {
        for &inst in bb.insts() {
            match data.inst_data(inst).kind() {
                InstKind::Load(load) => {
                    if load.src().is_global() {
                        return false;
                    }
                }
                InstKind::Store(store) => {
                    if store.src().is_global() || store.dest().is_global() {
                        return false;
                    }
                }
                InstKind::MemZero(..) => return false,
                InstKind::GetElemPtr(gep) => {
                    if gep.base().is_global() {
                        return false;
                    }
                }
                _ => {}
            }
        }
    }
    true
}

/// The set of pure functions in the program: functions whose behaviour is
/// fully determined by their arguments, so that duplicate calls with identical
/// arguments may be CSE'd. Computed as a least fixpoint over the call graph so
/// that recursion and mutual recursion are handled correctly.
pub fn pure_functions(program: &Program) -> HashSet<Function> {
    let all: Vec<Function> = program.function_layout().to_vec();
    // Start from the locally-pure candidates, then repeatedly remove any
    // function that calls a (transitively) impure function until stable.
    let mut pure: HashSet<Function> = all.iter().copied().filter(|&f| locally_pure(program, f)).collect();
    loop {
        let mut removed = false;
        let snapshot: Vec<Function> = pure.iter().copied().collect();
        for func in snapshot {
            let data = program.func_data(func);
            let calls_impure = data.layout().basicblocks().iter().any(|bb| {
                bb.insts().iter().any(|&inst| match data.inst_data(inst).kind() {
                    InstKind::Call(call) => !pure.contains(&call.callee()),
                    InstKind::TailCall(tail_call) => !pure.contains(&tail_call.callee()),
                    _ => false,
                })
            });
            if calls_impure {
                pure.remove(&func);
                removed = true;
            }
        }
        if !removed {
            break;
        }
    }
    pure
}
