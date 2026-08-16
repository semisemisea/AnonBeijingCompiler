//! Fold `br (x < 0), A, B` when `x` is provably `>= 0` (take `B`).
//!
//! The M60 `mulmod_recognize` rewrite emits a `b < 0` guard in front of every
//! `soyo_mulmod(a, b, p)` call to preserve the original recursion's "negative
//! `b` returns 0" semantics. Inlined into its callers, that guard is a
//! `cmp; b.lt` on the hot butterfly path. The `return_summary` analysis proves
//! which values (including function parameters that every call site passes
//! non-negative) are always `>= 0`, so guards whose second argument flows from
//! such a value fold away. Guards on input-derived values (array elements)
//! stay.
//!
//! The program-level summaries are computed once per run and shared across
//! functions.

use crate::{
    ir::{BasicBlock, BinaryOp, Inst, InstKind, Program},
    opt::{
        analysis_passes::return_summary::{
            always_nonneg_params, nonneg_in_function, nonneg_preserving_functions,
        },
        pass::Pass,
        prelude::*,
    },
};

pub struct GuardElimination;

impl Pass for GuardElimination {
    fn run(&mut self, program: &mut Program) -> bool {
        let nonneg = nonneg_preserving_functions(program);
        let params = always_nonneg_params(program, &nonneg);

        let mut changed = false;
        let funcs: Vec<Function> = program.function_layout().to_vec();
        for &f in &funcs {
            let data = program.func_data(f);
            if data.layout().is_decl() {
                continue;
            }
            let self_params = params.get(&f).cloned().unwrap_or_default();
            let facts = nonneg_in_function(program, f, &self_params, &nonneg);

            let mut folds: Vec<(Inst, BasicBlock, Vec<Inst>)> = Vec::new();
            for layout in data.layout().basicblocks() {
                let terminator = *layout.insts().get_last().unwrap();
                let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
                    continue;
                };
                let cond = branch.cond();
                let Some((BinaryOp::Lt, x, zero)) = binary_of(data, cond) else {
                    continue;
                };
                if !matches!(
                    data.inst_data(zero).kind(),
                    InstKind::Integer(int) if int.value() == 0
                ) {
                    continue;
                }
                if facts.contains(&x) {
                    // `x < 0` is false: always take the false edge.
                    folds.push((terminator, branch.f_target(), branch.f_args().to_vec()));
                }
            }

            let data = program.func_data_mut(f);
            for (inst, target, args) in folds {
                data.replace_inst_with(inst).jump(target, args);
                changed = true;
            }
        }
        changed
    }
}

fn binary_of(data: &crate::ir::FunctionData, inst: Inst) -> Option<(BinaryOp, Inst, Inst)> {
    match data.inst_data(inst).kind() {
        InstKind::Binary(binary) => Some((binary.op(), binary.lhs(), binary.rhs())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{Program, Type, arena::Arena, builder_trait::*},
        opt::pass::Pass as _,
    };

    /// `g(x)`: `br (x < 0), neg, nonneg; neg: ret 0; nonneg: ret x`.
    fn build_guard(program: &mut Program, name: &str, use_param: bool) -> (Function, Inst) {
        let f = program.new_function(Type::get_i32(), name.into(), vec![Type::get_i32()]);
        let (branch, _) = {
            let data = program.func_data_mut(f);
            let entry = data.add_entry_block();
            let neg = data.new_basic_block().basic_block("neg".into(), vec![]);
            let pos = data.new_basic_block().basic_block("pos".into(), vec![]);
            data.layout_mut().push_bb_back(neg);
            data.layout_mut().push_bb_back(pos);
            let x = data.params()[0];
            let guard = if use_param {
                x
            } else {
                let five = data.new_local_inst().integer(5);
                data.layout_mut().insert_inst(entry, five);
                five
            };
            let zero = data.new_local_inst().integer(0);
            let cond = data.new_local_inst().binary(BinaryOp::Lt, guard, zero);
            let branch = data.new_local_inst().branch(cond, neg, vec![], pos, vec![]);
            data.layout_mut().insert_inst(entry, cond);
            data.layout_mut().insert_inst(entry, branch);
            let ret_zero = data.new_local_inst().ret(Some(zero));
            data.layout_mut().insert_inst(neg, ret_zero);
            let ret_x = data.new_local_inst().ret(Some(x));
            data.layout_mut().insert_inst(pos, ret_x);
            (branch, x)
        };
        (f, branch)
    }

    #[test]
    fn folds_guard_on_constant_nonneg_and_keeps_param_guard() {
        let mut program = Program::new();
        let (constant_guard, const_branch) = build_guard(&mut program, "f_const", false);
        let (param_guard, param_branch) = build_guard(&mut program, "f_param", true);

        // `main` calls `f_param(load)` (input-derived), so the parameter guard
        // must stay; the constant guard is provably never taken.
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        {
            let data = program.func_data_mut(main);
            let entry = data.add_entry_block();
            let ptr = data.new_local_inst().alloc(Type::get_i32());
            let load = data.new_local_inst().load(ptr);
            let call =
                data.new_local_inst()
                    .call_with_type(param_guard, vec![load], Type::get_i32());
            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(entry, load);
            data.layout_mut().insert_inst(entry, call);
            data.layout_mut().insert_inst(entry, ret);
        }

        let mut pass = GuardElimination;
        pass.run(&mut program);

        let data = program.func_data(constant_guard);
        assert!(
            matches!(data.inst_data(const_branch).kind(), InstKind::Jump(_)),
            "constant guard must fold to a jump"
        );
        let data = program.func_data(param_guard);
        assert!(
            matches!(data.inst_data(param_branch).kind(), InstKind::Branch(_)),
            "input-derived guard must stay a branch"
        );
    }
}
