//! soyo_compiler 库入口：把编译管线暴露为库 API。
//!
//! 二进制 `compiler`（src/main.rs，CLI）与 fuzz target（仓库根 fuzz/）共用本库。
//! fuzz target 通过 [`compile`] 直接驱动整条管线（词法/语法 → AST → RaanaIR →
//! 优化 pass → 寄存器分配 → 目标汇编），编译过程中的 panic/assert 即被
//! libFuzzer 记为 crash（内部错误 ICE）；语法/语义错误则走 `Err` 返回，不 panic。
//!
//! 注意：本库与 CLI（cli.rs）各自维护一份优化/后端配置构造（ir_config /
//! aarch64_config），改动 `-O` 档位语义时两处需同步。

mod frontend;

pub use frontend::utils::{AstGenContext, ToRaanaIR};

// lalrpop_mod! 展开生成 `mod sysy`（parser），不能额外声明
lalrpop_util::lalrpop_mod!(sysy);

/// 库 API 使用的目标架构（与 cli::Target 对应，避免库依赖 clap）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Aarch64,
    Riscv64,
}

/// 编译一段 SysY 源码，返回目标架构汇编文本。
///
/// - `source`：SysY 源码字符串（UTF-8）
/// - `opt_level`：0..=2，与 CLI 的 `-O` 语义一致
/// - `target`：目标架构（AArch64 / RISC-V 64）
///
/// 错误路径（语法错误等）返回 `Err`；编译器内部 bug 表现为 panic。
pub fn compile(source: &str, opt_level: u8, target: Target) -> Result<String, String> {
    let ast = sysy::CompUnitsParser::new()
        .parse(source)
        .map_err(|error| format!("parse error: {error}"))?;
    let mut ctx = AstGenContext::new();
    ast.convert(&mut ctx);
    let mut program = ctx.program;

    // 空编译单元（空源码/只有声明）没有入口：与 gcc 的
    // "undefined reference to main" 语义一致，报 Err 而不是让依赖入口的
    // pass（IPSCCP 的 ICFG worklist 等）在 get_main_function().unwrap()
    // 处 ICE。fuzz 的 libFuzzer 会把输入变异成空串，这个 guard 让空输入
    // 归为正常输入而非 crash。
    if !program.has_main() {
        return Err("no main function".into());
    }

    let mut pass_manager =
        raana_ir::opt::pass::PassesManager::from_config(ir_config(opt_level, target));
    pass_manager.run_passes(&mut program);

    match target {
        Target::Riscv64 => Ok(taki_mir::compile::<uika_riscv::lower::Riscv64Backend>(
            &program,
        )),
        Target::Aarch64 => {
            let config = aarch64_config(opt_level);
            Ok(taki_mir::compile_with_config::<anon_armv8::AArch64Backend>(
                &program,
                &config,
            )
            .assembly)
        }
    }
}

/// IR 优化管线配置（基准档位，与 cli.rs `Arg::ir_optimization_config` 一致；
/// 库 API 不暴露 loop-unroll、pass-stats 等 CLI 覆盖项）。
fn ir_config(opt_level: u8, target: Target) -> raana_ir::opt::config::PassesConfig {
    use raana_ir::opt::config::{OptimizationLevel, PassesConfig, TargetPolicy};

    let opt_level = OptimizationLevel::try_from(opt_level).unwrap_or(OptimizationLevel::O2);
    let policy = match target {
        Target::Aarch64 => TargetPolicy::aarch64(),
        Target::Riscv64 => TargetPolicy::riscv64(),
    };
    PassesConfig::new(opt_level, policy)
}

/// AArch64 后端配置（各 `-O` 档位，与 cli.rs `Arg::aarch64_codegen_config` 一致；
/// 库 API 不暴露 --enable/--disable-* 覆盖项）。
fn aarch64_config(opt_level: u8) -> anon_armv8::AArch64CodegenConfig {
    use anon_armv8::{AArch64CodegenConfig, AArch64SchedModel};
    match opt_level {
        0 => AArch64CodegenConfig {
            dce: false,
            peephole_combine: false,
            pair_combine: false,
            list_scheduler: false,
            sched_model: AArch64SchedModel::CortexA53,
            branch_opt: false,
            chain_fusion: false,
            const_cse: false,
        },
        1 => AArch64CodegenConfig {
            dce: true,
            peephole_combine: true,
            pair_combine: true,
            list_scheduler: false,
            sched_model: AArch64SchedModel::CortexA53,
            branch_opt: true,
            chain_fusion: true,
            const_cse: true,
        },
        _ => AArch64CodegenConfig {
            dce: true,
            peephole_combine: true,
            pair_combine: true,
            list_scheduler: true,
            sched_model: AArch64SchedModel::CortexA53,
            branch_opt: true,
            chain_fusion: true,
            const_cse: true,
        },
    }
}
