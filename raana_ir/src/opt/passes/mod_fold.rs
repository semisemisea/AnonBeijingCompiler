//! Fold `x % P` into a conditional subtraction when the dividend is provably
//! in `[0, 2P)`.
//!
//! A truncating remainder `x % P` with `P > 0` and `0 <= x < 2P` equals
//! `x >= P ? x - P : x` — one `sub; cmp; csel` instead of the 4-6 instruction
//! magic-number multiply-high sequence the backend would otherwise emit for a
//! constant divisor. The dividend bound is proven by the range analysis
//! (`RangeAnalysis::range_before`); when it cannot be proven (negative or
//! unbounded dividends, e.g. `f(x)` wrapping intermediates in h-4) the
//! remainder is left untouched (宁漏勿错).
//!
//! Powers of two are skipped so the backend's cheaper `and` lowering handles
//! them. The divisor must be a positive compile-time constant; negative and
//! `0`/`±1` divisors keep their current lowering.

use crate::{
    ir::{BinaryOp, Inst, InstKind},
    opt::{
        analysis_passes::{
            induction_variable::BasicInductionVariableAnalysis,
            loop_analysis::LoopAnalysis,
            range::{IntRange, RangeAnalysis},
            return_summary,
        },
        pass::{ArenaContext, Pass},
        prelude::*,
        utils::cfg::CFG,
    },
};

pub struct ModFold;

impl Pass for ModFold {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().is_decl() {
            return false;
        }
        // Cheap pre-scan: only build the (comparatively expensive) range
        // analysis when the function actually has a `Rem` by a positive
        // non-power-of-two constant divisor — the only shape this pass folds.
        let candidates: Vec<Inst> = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|layout| layout.insts().iter().copied())
            .filter(|&inst| {
                rem_of(data, inst).is_some_and(|(_, divisor)| {
                    positive_constant(data, divisor).is_some_and(|p| p > 0 && !is_power_of_two(p))
                })
            })
            .collect();
        if candidates.is_empty() {
            return false;
        }

        let folds: Vec<(Inst, Inst)> = {
            let Some(cfg) = CFG::new(data) else {
                return false;
            };
            let (cfg, _dom_tree, loops) = LoopAnalysis::from_cfg(cfg);
            let ivs = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
            let range_arena = ArenaContext {
                program: &*data.program,
                curr_func: data.curr_func,
            };
            let nonneg = return_summary::nonneg_preserving_functions(data.program);
            let all_params = return_summary::always_nonneg_params(data.program, &nonneg);
            let self_params = all_params
                .get(&data.curr_func.expect("run_on always sets curr_func"))
                .cloned()
                .unwrap_or_default();
            let ranges =
                RangeAnalysis::new(&range_arena, &cfg, &loops, &ivs, &nonneg, &self_params);

            let mut folds = Vec::new();
            for inst in candidates {
                let Some((dividend, divisor)) = rem_of(data, inst) else {
                    continue;
                };
                let Some(p) = positive_constant(data, divisor) else {
                    continue;
                };
                if foldable(ranges.range_before(inst, dividend), p) {
                    folds.push((inst, dividend));
                }
            }
            folds
        };
        if folds.is_empty() {
            return false;
        }
        for (inst, dividend) in folds {
            let InstKind::Binary(binary) = data.inst_data(inst).kind() else {
                continue;
            };
            let divisor = binary.rhs();
            let sub = data
                .new_local_inst()
                .binary(BinaryOp::Sub, dividend, divisor);
            let cond = data
                .new_local_inst()
                .binary(BinaryOp::Ge, dividend, divisor);
            data.layout_mut().insert_inst_before(inst, sub);
            data.layout_mut().insert_inst_before(inst, cond);
            data.replace_inst_with(inst).select(cond, sub, dividend);
        }
        true
    }
}

fn rem_of(data: &FunctionData, inst: Inst) -> Option<(Inst, Inst)> {
    match data.inst_data(inst).kind() {
        InstKind::Binary(binary) if binary.op() == BinaryOp::Rem => {
            Some((binary.lhs(), binary.rhs()))
        }
        _ => None,
    }
}

fn positive_constant(data: &FunctionData, inst: Inst) -> Option<i32> {
    match data.inst_data(inst).kind() {
        InstKind::Integer(integer) if integer.value() > 0 => Some(integer.value()),
        _ => None,
    }
}

fn is_power_of_two(p: i32) -> bool {
    p > 0 && (p & (p - 1)) == 0
}

fn foldable(range: IntRange, p: i32) -> bool {
    let Some(min) = range.min() else {
        return false;
    };
    let Some(max) = range.max() else {
        return false;
    };
    min >= 0 && (max as i64) < 2 * (p as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{Program, Type, builder_trait::*},
        opt::pass::Pass as _,
    };

    /// Builds `target(x)`: `t = x & mask; rem = t % divisor; ret rem`. The
    /// `and` transfer gives `t` the range `[0, mask]`, so `rem` folds whenever
    /// `mask < 2 * divisor` and `divisor` is not a power of two.
    fn build_and_rem(program: &mut Program, mask: i32, divisor: i32) -> (Function, Inst, Inst) {
        let f = program.new_function(Type::get_i32(), "target".into(), vec![Type::get_i32()]);
        let rem = {
            let data = program.func_data_mut(f);
            let entry = data.add_entry_block();
            let x = data.params()[0];
            let mask_inst = data.new_local_inst().integer(mask);
            let t = data.new_local_inst().binary(BinaryOp::And, x, mask_inst);
            let p = data.new_local_inst().integer(divisor);
            let rem = data.new_local_inst().binary(BinaryOp::Rem, t, p);
            let ret = data.new_local_inst().ret(Some(rem));
            data.layout_mut().insert_inst(entry, t);
            data.layout_mut().insert_inst(entry, rem);
            data.layout_mut().insert_inst(entry, ret);
            (rem, t)
        };
        (f, rem.0, rem.1)
    }

    #[test]
    fn folds_bounded_nonnegative_dividend() {
        let mut program = Program::new();
        // `t = x & 7` ∈ [0,7] ⊂ [0,10), so `t % 5` folds.
        let (f, rem, _) = build_and_rem(&mut program, 7, 5);

        ModFold.run(&mut program);
        let data = program.func_data(f);
        assert!(
            matches!(data.inst_data(rem).kind(), InstKind::Select(_)),
            "dividend [0,7] with divisor 5 must fold `% 5` to a select"
        );
    }

    #[test]
    fn keeps_unbounded_or_too_wide_dividend() {
        let mut program = Program::new();
        // `t = x & 31` ∈ [0,31] ⊄ [0,10), so `t % 5` must stay a remainder.
        let (f, rem, _) = build_and_rem(&mut program, 31, 5);

        ModFold.run(&mut program);
        let data = program.func_data(f);
        assert!(
            matches!(data.inst_data(rem).kind(), InstKind::Binary(_)),
            "dividend [0,31] with divisor 5 must keep the remainder"
        );
    }

    #[test]
    fn skips_power_of_two_divisor() {
        let mut program = Program::new();
        // `t = x & 7` ∈ [0,7] ⊂ [0,32), but 16 is a power of two: keep the
        // remainder so the backend's `and` lowering wins.
        let (f, rem, _) = build_and_rem(&mut program, 7, 16);

        ModFold.run(&mut program);
        let data = program.func_data(f);
        assert!(
            matches!(data.inst_data(rem).kind(), InstKind::Binary(_)),
            "power-of-two divisor must keep the remainder"
        );
    }

    #[test]
    fn rewrite_equals_rem_for_bounded_values() {
        // Execute the rewritten select against the truncating remainder for
        // every dividend in [0, 2P) to prove equivalence.
        let p: i32 = 10;
        let mut last = 0i32;
        for x in 0..(2 * p) {
            last = if x >= p { x - p } else { x };
            assert_eq!(last, x % p, "select must equal truncating rem for x = {x}");
        }
        assert_eq!(last, 9);
    }
}
