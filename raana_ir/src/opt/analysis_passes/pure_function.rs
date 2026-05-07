use crate::opt::prelude::*;

pub fn is_pure_function(program: &Program, func: Function) -> bool {
    // All library function have side-effect.
    // And function should have unique name.
    let data = program.func_data(func);
    if matches!(
        data.name(),
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
    ) {
        return false;
    };
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
                InstKind::Call(_call) => {
                    // TODO: Use graph algorithm to get better result.
                    // if !is_pure_function(program, call.callee()) {
                    //     return false;
                    // }
                    return false;
                }
                InstKind::GetElemPtr(gep) => {
                    if gep.base().is_global() {
                        return false;
                    }
                }
                InstKind::GetPtr(gp) => {
                    if gp.base().is_global() {
                        return false;
                    }
                }
                _ => {}
            }
        }
    }
    true
}
