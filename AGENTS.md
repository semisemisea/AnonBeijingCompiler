# AGENTS.md

SysY → AArch64/RISC-V 编译器（Rust 编写，CSC 编译系统设计赛参赛作品）。真正的质量
门禁是 Docker 测试 harness 和与 clang -O2 的静态代码量对比，而不是 `cargo test`。

## 构建与单元测试

- 工具链锁定 **Rust 1.85.0**（`rust-toolchain.toml`）。工作区 crate：
  `soyo_compiler`（CLI + lalrpop 前端，二进制名 `compiler`）、`raana_ir`（SSA HLIR +
  优化 pass）、`taki_mir`（通用 MIR/寄存器分配）、`anon_armv8`（AArch64 后端）、
  `uika_riscv`（RISC-V 后端）、`tomori_utils`（共享工具）、`sysylib`（运行时库）。
- 快速本地反馈：`cargo test -p raana_ir`（236 个单元测试）。每个 pass 的测试是
  inline 的 `#[cfg(test)] mod tests`。
- 本地单文件编译（无需 Docker，macOS 可用）：
  `target/debug/compiler -S -O 2 --target aarch64 -o /tmp/out.s tests/perf/xxx.sy`
  或 `--emit ir` 输出 RaanaIR dump。
- `.cargo/config.toml` 把全部依赖 **vendor** 到 `dependencies/`。新增 crate 必须先
  将其 vendor 进该目录。

## 测试 harness（真正的门禁）——通过 Docker 运行

`make test` 把编译器交叉编译为 `aarch64-unknown-linux-musl`（产物在
`target/host-musl/`），构建 `sysylib/libsysy_arm.a`，并在 `soyo-test-tools` 镜像里
运行 `tests/test.py`。`tests/test.py` 是挂载进容器的，改它无需重建镜像（只有
`Dockerfile` 改动才需要；`.docker-image` 是校验和戳）。

关键坑：

- **harness 默认优化级别是 `-O0`**。CI 用 `make test TESTS="functional h_functional"
  ARGS="-j 2"`（无 `-O`），所以 CI 并不验证 `-O2`。性能里程碑全是 `-O2` 工作——
  本地务必用 `make test ... ARGS="-O 2"`。
- **给 `make test`/`make test-riscv` 传用例有两种方式**（等效）：
  `make test functional h_functional ARGS="-O 2"`（把用例当 make 目标）或
  `make test TESTS="functional h_functional" ARGS="-O 2"`。单用例：
  `make test functional/75_max_flow.sy ARGS="-O 2"`。运行前先
  `docker rm -f soyo-test` 清掉可能残留的旧容器，否则会跑成上一次的用例集。
- 每个测试会把编译器运行**两次**：一次 `-S`（汇编），一次 `--emit ir`
  （RaanaIR dump）。IR dump 崩溃会以 `CE` 出现，即使汇编没问题。
- 通过/失败比较的是程序 `stdout` + 换行 + `returncode` 与 `.out` 文件
  （test.py 的 `combined_output`）——退出码行是期望输出的一部分。
- 每个用例的产物布局：`results/{functional,h_functional,perf}/<case>.{s,elf,raana,
  compile.*,runtime.*}`。`make clean-results` 重置。
- 其他目标：`make test-baseline`（容器内 clang 基线）、`make test-riscv ARGS="-O 2"`、
  `make test-llvm`、`make run-elf path.elf`、`make debug-elf`（qemu-gdb）、
  `make mca path.s`（llvm-mca，cortex-a53）、`make gem5 <case>`（慢；只用小输入）。
- QEMU 性能运行很慢（huffman-01 现在 ~30s，早先 ~60-100s @-O2）。迭代时优先用静态
  计数（`scripts/perf_compare.sh`）。

## 性能方法论（里程碑跟踪的目标）

- 参照：`results/perf/*_clang.s`、`results/perf/*_gcc.s`（clang/gcc -O2 汇编）。
- `scripts/perf_compare.sh [--gem5] [case...]` 以 `-O2` aarch64 编译并统计生成的
  `.s` 里真实指令数（awk 方法：跳过伪指令/标签/空行），写入
  `results/perf_compare/<git-short-sha>/table.tsv`，对照 `orig/sched/clang` 列。
  这是**静态模型级**代理，不是周期计数。
- 动态数据来自 `make gem5`（sim_insts）或 QEMU 墙钟时间。
- 每个用例的收敛状态在 `TODO.md` §1.2/§2 跟踪。主计划 F 已收敛：huffman
  `_and`/`_xor`/`_or` 循环（M59）已到 15 条/轮；SIMD Phase 2（M42-M46）仍搁置。

## IR / 优化架构要点

- 流水线顺序在 `raana_ir/src/opt/pass.rs`（`PassesManager::from_config`）。初始：
  SSA → Specialize → Inline → TCO → ColumnMajor → GSP；固定点内：IPSCCP,
  SimplifyCFG, LoopUnroll, RotateLoops, ZeroStoreLoop, ChainToSwitch, LICM, GVN,
  PSR, SR, InvariantReductionHoisting, ReductionUnroll, IfConversion, TCO,
  TailRecursiveInline, BooleanSimplification, GVNPRE, DeadPhiElim, DCE。
- **仅 AArch64 的 pass 必须用 `TargetPolicy.enable_chain_to_switch` 做门控**
  （见 `config.rs`、`pass.rs`），否则 RISC-V 会回归。务必跑 `make test-riscv`。
- **改 CFG / block 参数 / 循环前先读 `docs/Convention.md`**。Phi 是 block 参数
  （`BlockArgRef` 的位置必须匹配目标 `params()` 切片，而不是
  `BlockArgRef::index()`）；结构边与逻辑边不同；CFG/支配/循环/IV 分析是快照，
  任何修改都会使其失效。
- 纯度分析（`opt/analysis_passes/pure_function.rs`）：通过指针参数改写内存的函数
  **不是**纯函数（对调用方可见）。经指针参数的 load/store 会使函数不纯。
- MIR pass 在 `anon_armv8/src/passes/`（dce、peephole、pair_combine、chain_fusion、
  list_scheduler）；调度模型是 cortex-a53。

## 约定

- 提交风格：`[Opt(IR)]:`、`[Fix(Opt)]:`、`[Feat(IR)]:`、`[Docs]:`、`[Chore]:` 前缀
  （见 git log）；相关处引用里程碑号（如 `M56`）。
- `TODO.md` 只保留**未完成**的工作；完成的里程碑压缩成一行摘要。里程碑落地时更新它。
- `docs/Illegal_optimization.md`：不允许针对测试用例的优化（函数名/输入模式匹配、
  硬编码结果）。优化必须通用。
- 性能改动门禁：`cargo test -p raana_ir`、`make test ARGS="-O 2"`（functional +
  h_functional）、`make test-riscv ARGS="-O 2"`，以及 perf 语料无静态计数回归
  （`scripts/perf_compare.sh`）。
