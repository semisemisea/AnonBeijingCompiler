# AnonBeijingCompiler Agent Guide

## Workspace

- Rust toolchain is pinned to `1.85.0` (`rust-toolchain.toml`); the workspace uses edition 2024.
- `soyo_compiler` is the CLI entrypoint. The pipeline is SysY source -> RaanaIR (`raana_ir`) -> VCode/MIR (`taki_mir`) -> AArch64 assembly (`anon_armv8`); `tomori_utils` provides shared data structures.
- `soyo_compiler/build.rs` runs LALRPOP. Update `soyo_compiler/src/sysy.lalrpop`, not generated files under `target/`.
- The CLI accepts one input and requires `-o`. `-S` is an alias for `--emit asm`; the default target is RISC-V, so use `--target aarch64` for GNU AArch64 assembly. `--emit ir`, `--emit llvm`, and `--emit asm` select outputs. Multiple comma-separated `--emit` values treat `-o` as an output directory and create files named after the input stem.
- Optimizations run only when `-O` is greater than zero; `-O1` is the intended optimized path.

## Verification

- Run focused Rust unit tests with `cargo test -p <crate> <test-filter>`; run all workspace unit tests with `cargo test --workspace`.
- The end-to-end suite is `make test`, not `cargo test`. It builds a musl-hosted release compiler, then the Docker harness cross-links generated AArch64 code and executes it with `qemu-aarch64-static`.
- Run an individual functional case with `make test functional/00_main.sy`; paths are relative to `tests/` (a `tests/` prefix also works). Pass runner flags through `ARGS`, for example `make test ARGS="-O 1 --verbose" functional/00_main.sy`.
- Use `make test-llvm functional/00_main.sy` to validate the LLVM IR emitter; the harness lowers `.ll` with `llc` before linking.
- End-to-end tests require Docker. The Makefile rebuilds `soyo-test-tools` when `Dockerfile` or `tests/test.py` changes, builds `sysylib/libsysy_arm.a` in the container, and writes all per-case artifacts to ignored `results/`.
- `make run-elf path/to/program.elf` executes an AArch64 ELF in the test container; `make debug-elf path/to/program.elf` starts QEMU's gdb stub and `gdb-multiarch`.
