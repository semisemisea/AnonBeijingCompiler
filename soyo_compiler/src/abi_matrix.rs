//! Cross-target ABI regression matrix for incoming function parameters.
//!
//! Covers the AAPCS64 / RISC-V argument conventions exercised end to end:
//! zero/one/eight/nine arguments, integer/float mixes, unused parameters,
//! parameters flowing across ordinary calls and tail calls, recursion, local
//! allocations, and parameter reassignment. Every case must compile for both
//! targets at every optimization level and must be byte-for-byte deterministic
//! across repeated compilations.
//!
//! Differential correctness against expected runtime output is validated by
//! `tests/test.py` under QEMU (requires a cross toolchain); this module is the
//! portable compilation + determinism gate that runs in any environment.

use crate::cli::Target;
use crate::frontend::utils::{AstGenContext, ToRaanaIR};
use anon_armv8::AArch64Backend;
use taki_mir::stats::FunctionCodegenStats;
use uika_riscv::lower::Riscv64Backend;

struct MatrixCase {
    name: &'static str,
    source: &'static str,
}

const MATRIX: &[MatrixCase] = &[
    MatrixCase {
        name: "leaf_add",
        source: "int add(int a, int b) { return a + b; }\nint main() { return add(3, 4); }\n",
    },
    MatrixCase {
        name: "no_args",
        source: "int forty_two() { return 42; }\nint main() { return forty_two(); }\n",
    },
    MatrixCase {
        name: "one_i32",
        source: "int twice(int a) { return a + a; }\nint main() { return twice(7); }\n",
    },
    MatrixCase {
        name: "one_f32",
        source: "float identity(float x) { return x; }\nint main() { return 0; }\n",
    },
    MatrixCase {
        name: "eight_int_args",
        source: "int sum8(int a, int b, int c, int d, int e, int f, int g, int h) { return a + b + c + d + e + f + g + h; }\nint main() { return sum8(1, 2, 3, 4, 5, 6, 7, 8); }\n",
    },
    MatrixCase {
        name: "eight_float_args",
        source: "float pick(float a, float b, float c, float d, float e, float f, float g, float h) { return a; }\nint main() { return 0; }\n",
    },
    MatrixCase {
        name: "nine_int_args",
        source: "int last(int a, int b, int c, int d, int e, int f, int g, int h, int i) { return i; }\nint main() { return last(1, 2, 3, 4, 5, 6, 7, 8, 9); }\n",
    },
    MatrixCase {
        name: "mixed_int_float",
        source: "float mix(int a, float b) { return b; }\nint main() { return 0; }\n",
    },
    MatrixCase {
        name: "unused_params",
        source: "int use_first(int a, int b) { return a; }\nint main() { return use_first(3, 4); }\n",
    },
    MatrixCase {
        name: "params_across_call",
        source: "int helper(int x) { return x + 1; }\nint caller(int a, int b) { return helper(a) + b; }\nint main() { return caller(3, 4); }\n",
    },
    MatrixCase {
        name: "tail_recursion",
        source: "int fact(int n, int acc) { if (n == 0) { return acc; } return fact(n - 1, acc * n); }\nint main() { return fact(5, 1); }\n",
    },
    MatrixCase {
        name: "local_alloc",
        source: "int use_array() { int arr[4]; arr[0] = 3; return arr[0]; }\nint main() { return use_array(); }\n",
    },
    MatrixCase {
        name: "param_reassigned",
        source: "int reassign(int a) { a = a + 1; return a; }\nint main() { return reassign(3); }\n",
    },
    MatrixCase {
        name: "non_tail_recursion",
        source: "int fib(int n) { if (n < 2) { return n; } return fib(n - 1) + fib(n - 2); }\nint main() { return fib(10); }\n",
    },
];

fn compile_sy(source: &str, target: Target, opt_level: u8) -> String {
    compile_sy_with(source, target, opt_level, true)
}

/// Like `compile_sy`, but with a pipeline that keeps dead functions (no
/// dead-function elimination). ABI observation tests assert on the parameter
/// binding of optimized-but-unreachable helpers, which the real pipeline
/// correctly removes.
fn compile_sy_without_dfe(source: &str, target: Target, opt_level: u8) -> String {
    compile_sy_with(source, target, opt_level, false)
}

fn compile_sy_with(source: &str, target: Target, opt_level: u8, use_dfe: bool) -> String {
    let ast = crate::sysy::CompUnitsParser::new()
        .parse(source)
        .expect("SysY test case must parse");
    let mut ctx = AstGenContext::new();
    ast.convert(&mut ctx);
    let mut program = ctx.program;
    let ir_opt_level = raana_ir::opt::config::OptimizationLevel::try_from(opt_level).unwrap();
    let target_policy = match target {
        Target::Aarch64 => raana_ir::opt::config::TargetPolicy::aarch64(),
        Target::Riscv64 => raana_ir::opt::config::TargetPolicy::riscv64(),
    };
    let mut pass_manager = raana_ir::opt::pass::PassesManager::from_config(
        raana_ir::opt::config::PassesConfig {
            dead_function_elimination: use_dfe,
            ..raana_ir::opt::config::PassesConfig::new(ir_opt_level, target_policy)
        },
    );
    pass_manager.run_passes(&mut program);
    let aarch64_config = match opt_level {
        0 => anon_armv8::AArch64CodegenConfig {
            dce: false,
            peephole_combine: false,
            pair_combine: false,
            list_scheduler: false,
            sched_model: anon_armv8::AArch64SchedModel::CortexA53,
            branch_opt: false,
            chain_fusion: false,
            const_cse: false,
        },
        1 => anon_armv8::AArch64CodegenConfig {
            dce: true,
            peephole_combine: true,
            pair_combine: true,
            list_scheduler: false,
            sched_model: anon_armv8::AArch64SchedModel::CortexA53,
            branch_opt: true,
            chain_fusion: true,
            const_cse: true,
        },
        _ => anon_armv8::AArch64CodegenConfig {
            dce: true,
            peephole_combine: true,
            pair_combine: true,
            list_scheduler: true,
            sched_model: anon_armv8::AArch64SchedModel::CortexA53,
            branch_opt: true,
            chain_fusion: true,
            const_cse: true,
        },
    };
    match target {
        Target::Riscv64 => taki_mir::compile::<Riscv64Backend>(&program),
        Target::Aarch64 => {
            taki_mir::compile_with_config::<AArch64Backend>(&program, &aarch64_config).assembly
        }
    }
}

fn function_section(asm: &str, name: &str) -> String {
    let start = asm
        .lines()
        .position(|line| line.trim() == format!("{name}:"))
        .unwrap_or_else(|| panic!("missing function section `{name}` in assembly:\n{asm}"));
    let mut end = asm.lines().count();
    for (index, line) in asm.lines().enumerate().skip(start + 1) {
        let trimmed = line.trim();
        if (trimmed.ends_with(':') && !trimmed.starts_with('.'))
            || (trimmed.ends_with(':') && trimmed.starts_with("Lfunc"))
        {
            end = index;
            break;
        }
    }
    asm.lines()
        .skip(start)
        .take(end - start)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile_deterministically(case: &MatrixCase, target: Target, opt_level: u8) -> String {
        compile_deterministically_with(case, target, opt_level, true)
    }

    fn compile_deterministically_with(
        case: &MatrixCase,
        target: Target,
        opt_level: u8,
        use_dfe: bool,
    ) -> String {
        let compile = |case: &MatrixCase, target: Target, opt_level: u8| {
            compile_sy_with(case.source, target, opt_level, use_dfe)
        };
        let first = compile(case, target, opt_level);
        for _ in 0..4 {
            let again = compile(case, target, opt_level);
            assert_eq!(
                first, again,
                "compilation of `{}` ({target:?}, -O{opt_level}) is not deterministic",
                case.name
            );
        }
        first
    }

    #[test]
    fn abi_matrix_compiles_for_both_targets_at_all_optimization_levels() {
        for case in MATRIX {
            for target in [Target::Aarch64, Target::Riscv64] {
                for opt_level in [0u8, 1, 2] {
                    let asm = compile_deterministically(case, target, opt_level);
                    assert!(!asm.trim().is_empty(), "empty assembly for {}", case.name);
                }
            }
        }
    }

    #[test]
    fn leaf_register_function_has_no_frame_and_no_argument_memory_round_trip() {
        let case = MATRIX.iter().find(|c| c.name == "leaf_add").unwrap();
        for opt_level in [1u8, 2] {
            for target in [Target::Aarch64, Target::Riscv64] {
                let asm = compile_deterministically_with(case, target, opt_level, false);
                let add = function_section(&asm, "add");
                for banned in [
                    "str ", "ldr ", "sw ", "lw ", "stp", "ldp", "sub sp", "addi sp",
                ] {
                    assert!(
                        !add.contains(banned),
                        "`add` must have no frame or argument memory round trip \
                         (target {target:?}, -O{opt_level}); found `{banned}` in:\n{add}"
                    );
                }
                if target == Target::Aarch64 {
                    assert!(
                        add.contains("add w0, w0, w1"),
                        "expected `add w0, w0, w1; ret` for the leaf (target {target:?}, -O{opt_level}):\n{add}"
                    );
                }
            }
        }
    }

    #[test]
    fn unused_register_parameter_produces_no_argument_slots() {
        let case = MATRIX.iter().find(|c| c.name == "unused_params").unwrap();
        let asm = compile_deterministically_with(case, Target::Aarch64, 2, false);
        let use_first = function_section(&asm, "use_first");
        // The second parameter is dead; it must not be materialized onto the
        // stack or read back.
        assert!(
            !use_first.contains("str w1") && !use_first.contains("ldr w"),
            "dead parameter must not create memory traffic:\n{use_first}"
        );
    }

    #[test]
    fn aarch64_self_tail_call_loops_without_rebuilding_a_frame() {
        let case = MATRIX.iter().find(|c| c.name == "tail_recursion").unwrap();
        // -O1 inlines `fact` into `main`; disable dead-function elimination so
        // the standalone `fact` section is still available for ABI inspection.
        let asm = compile_deterministically_with(case, Target::Aarch64, 1, false);
        let fact = function_section(&asm, "fact");

        assert!(
            fact.lines().any(|line| {
                let line = line.trim();
                line.starts_with("b .L_fact_") && line.contains("entry")
            }),
            "self tail call must branch to the local entry block:\n{fact}"
        );
        assert!(
            !fact.contains("b fact"),
            "self tail call must not re-enter through the function symbol:\n{fact}"
        );
        assert_eq!(
            fact.matches("ret").count(),
            1,
            "only the base case should return:\n{fact}"
        );
        assert!(
            !fact.contains("x29") && !fact.contains("x30"),
            "pure self tail recursion should not create a call frame:\n{fact}"
        );
    }

    #[test]
    fn leaf_register_arguments_are_bound_without_spills_or_moves() {
        use anon_armv8::AArch64CodegenConfig;
        use taki_mir::stats::FunctionCodegenStats;

        let case = MATRIX.iter().find(|c| c.name == "leaf_add").unwrap();
        let ast = crate::sysy::CompUnitsParser::new()
            .parse(case.source)
            .expect("valid SysY");
        let mut ctx = AstGenContext::new();
        ast.convert(&mut ctx);
        let mut program = ctx.program;
        let mut pass_manager = raana_ir::opt::pass::PassesManager::from_config(
            raana_ir::opt::config::PassesConfig {
                dead_function_elimination: false,
                ..raana_ir::opt::config::PassesConfig::new(
                    raana_ir::opt::config::OptimizationLevel::O2,
                    raana_ir::opt::config::TargetPolicy::aarch64(),
                )
            },
        );
        pass_manager.run_passes(&mut program);
        let config = AArch64CodegenConfig {
            dce: true,
            peephole_combine: true,
            pair_combine: true,
            list_scheduler: true,
            sched_model: anon_armv8::AArch64SchedModel::CortexA53,
            branch_opt: true,
            chain_fusion: true,
            const_cse: true,
        };
        let output = taki_mir::compile_with_config::<AArch64Backend>(&program, &config);

        let add: &FunctionCodegenStats = output
            .stats
            .functions
            .iter()
            .find(|stats| stats.function == "add")
            .expect("stats must include the `add` function");
        assert_eq!(add.abi.register_args_bound, 2);
        assert_eq!(add.abi.unused_register_args_skipped, 0);
        assert_eq!(add.abi.incoming_stack_args_loaded, 0);
        assert_eq!(add.regalloc.spill_slots, 0);
        assert_eq!(add.regalloc.reg_to_reg_edits, 0);
        assert_eq!(add.regalloc.reg_to_stack_edits, 0);
        assert_eq!(add.regalloc.stack_to_reg_edits, 0);
    }

    #[test]
    fn dead_register_parameter_is_counted_as_skipped() {
        use anon_armv8::AArch64CodegenConfig;
        use taki_mir::stats::FunctionCodegenStats;

        let case = MATRIX.iter().find(|c| c.name == "unused_params").unwrap();
        let ast = crate::sysy::CompUnitsParser::new()
            .parse(case.source)
            .expect("valid SysY");
        let mut ctx = AstGenContext::new();
        ast.convert(&mut ctx);
        let mut program = ctx.program;
        let mut pass_manager = raana_ir::opt::pass::PassesManager::from_config(
            raana_ir::opt::config::PassesConfig {
                dead_function_elimination: false,
                ..raana_ir::opt::config::PassesConfig::new(
                    raana_ir::opt::config::OptimizationLevel::O2,
                    raana_ir::opt::config::TargetPolicy::aarch64(),
                )
            },
        );
        pass_manager.run_passes(&mut program);
        let config = AArch64CodegenConfig {
            dce: true,
            peephole_combine: true,
            pair_combine: true,
            list_scheduler: true,
            sched_model: anon_armv8::AArch64SchedModel::CortexA53,
            branch_opt: true,
            chain_fusion: true,
            const_cse: true,
        };
        let output = taki_mir::compile_with_config::<AArch64Backend>(&program, &config);

        let use_first: &FunctionCodegenStats = output
            .stats
            .functions
            .iter()
            .find(|stats| stats.function == "use_first")
            .expect("stats must include the `use_first` function");
        assert_eq!(use_first.abi.register_args_bound, 1);
        assert_eq!(use_first.abi.unused_register_args_skipped, 1);
        assert_eq!(use_first.abi.incoming_stack_args_loaded, 0);
        assert_eq!(use_first.regalloc.spill_slots, 0);
    }

    fn compile_with_branch_opt(source: &str, branch_opt: bool) -> taki_mir::CompileOutput {
        use anon_armv8::AArch64CodegenConfig;

        let ast = crate::sysy::CompUnitsParser::new()
            .parse(source)
            .expect("valid SysY");
        let mut ctx = AstGenContext::new();
        ast.convert(&mut ctx);
        let mut program = ctx.program;
        let mut pass_manager = raana_ir::opt::pass::PassesManager::from_config(
            raana_ir::opt::config::PassesConfig {
                dead_function_elimination: false,
                ..raana_ir::opt::config::PassesConfig::new(
                    raana_ir::opt::config::OptimizationLevel::O2,
                    raana_ir::opt::config::TargetPolicy::aarch64(),
                )
            },
        );
        pass_manager.run_passes(&mut program);
        let config = AArch64CodegenConfig {
            dce: true,
            peephole_combine: true,
            pair_combine: true,
            list_scheduler: true,
            sched_model: anon_armv8::AArch64SchedModel::CortexA53,
            branch_opt,
            chain_fusion: false,
            const_cse: true,
        };
        taki_mir::compile_with_config::<AArch64Backend>(&program, &config)
    }

    fn function_stats<'a>(
        output: &'a taki_mir::CompileOutput,
        name: &str,
    ) -> &'a FunctionCodegenStats {
        output
            .stats
            .functions
            .iter()
            .find(|stats| stats.function == name)
            .unwrap_or_else(|| panic!("stats must include the `{name}` function"))
    }

    #[test]
    fn branch_optimization_removes_fallthrough_and_inverts_jumps() {
        // `f` exercises the R1 fallthrough elimination and R2 threading;
        // `g` exercises the M32 `ccmp` chain and M34 GSP write-backs, whose
        // direct branch form needs no R4 inversion.
        let source = "int f(int x) { if (x > 3) { return 1; } return 0; }\n\
                      int h(int c) { return c; }\n\
                      int ga, gb;\n\
                      int g() {\n\
                          int r = 0;\n\
                          if (ga > 3 || gb < 2) { r = h(ga); }\n\
                          return r;\n\
                      }\n\
                      int main() { ga = 2; gb = 3; return f(getint()) + g(); }\n";
        let output = compile_with_branch_opt(source, true);
        let f = function_stats(&output, "f");
        assert!(f.branch_opt.ran);
        assert!(
            f.branch_opt.fallthrough_removed + f.branch_opt.dead_jumps_removed >= 1,
            "`f` must eliminate its jump to the fallthrough merge block"
        );
        let g = function_stats(&output, "g");
        assert!(
            g.branch_opt.fallthrough_removed >= 1,
            "`g` must eliminate its fallthrough branches"
        );
        let g_section = function_section(&output.assembly, "g");
        assert!(
            g_section.contains("ccmp"),
            "`g` must fold its `||` into a ccmp chain:\n{g_section}"
        );
        assert!(
            g_section.contains("gv_ga") && g_section.contains("gv_gb"),
            "`g` must write the promoted globals back at the exit:\n{g_section}"
        );

        let f_section = function_section(&output.assembly, "f");
        assert!(
            !f_section.lines().any(|line| line.trim() == "1:"),
            "no local trampoline labels may remain:\n{f_section}"
        );
    }

    #[test]
    fn branch_optimization_off_keeps_two_instruction_form() {
        let source =
            "int g(int x) { if (x > 3) { return x; } return 0; }\nint main() { return g(2); }\n";
        let off = compile_with_branch_opt(source, false);
        let on = compile_with_branch_opt(source, true);
        assert!(!function_stats(&off, "g").branch_opt.ran);
        let count = |asm: &str| asm.lines().filter(|l| l.starts_with("    ")).count();
        let off_g = function_section(&off.assembly, "g");
        let on_g = function_section(&on.assembly, "g");
        assert!(
            count(&off_g) > count(&on_g),
            "the -O0-style two-instruction form must be strictly larger:\n{off_g}\n{on_g}"
        );
    }

    #[test]
    fn product_parity_branch_avoids_materializing_the_product() {
        let source = "int main() { int a = getint(); int b = getint(); if (a * b % 2 == 0) { return 1; } return 0; }\n";
        let assembly = compile_sy(source, Target::Aarch64, 2);
        let main = function_section(&assembly, "main");
        assert!(main.contains("and w"), "{main}");
        assert!(main.contains("tbnz "), "{main}");
        assert!(!main.contains("mul "), "{main}");

        let values = [i32::MIN, -3, -2, -1, 0, 1, 2, 3, i32::MAX];
        for lhs in values {
            for rhs in values {
                assert_eq!(lhs.wrapping_mul(rhs) & 1, (lhs & rhs) & 1);
            }
        }
    }

    #[test]
    fn integer_truthiness_branch_uses_cbnz() {
        let source = "int main() { int value = getint(); if (value) { return 1; } return 0; }\n";
        let assembly = compile_sy(source, Target::Aarch64, 2);
        let main = function_section(&assembly, "main");
        assert!(main.contains("cbnz "), "{main}");
        assert!(!main.contains("cmp "), "{main}");
    }
}
