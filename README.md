# AnonBeijingCompiler

Entry for CSC Compiler Implementation Competition. A SysY to AArch64/RISC-V compiler, written in Rust.

## Introduction

### Overall

The AnonBeijingCompiler project now consists of multiple parts: `RaanaIR` and `SoyoCompiler`.
Both projects are heavily influenced by the `pku-minic` course.
Specifically, although we built `RaanaIR` from scratch, it was designed while we were reading the source code of `KoopaIR`.
`SoyoCompiler` was migrated from the `s2r` repository (see Reference for more information).

### Lexing/Parsing

The lexing/parsing part is based on rust crate `lalrpop`. Which is adopted in `pku-minic` course.
It does the same thing YACC/Bison does, with a simple `sysy.lalrpop` file.
See more at `lalrpop` crate.
The parsing part, instead of writing a recursive descent parser, is handled by special AST design.
With so many nested structs, AST itself store the information of expression order.

### IR

RaanaIR is a statically strong typed high-level intermediate representation(HLIR).
It's also SSA-based to enable more aggressive/precise optimization.

For more information please go to the crate `raana_ir`

### Backend

Backend is built around machine-specific intermediate representation(MIR).
This part is influenced by the **VCode** from *Cranelift* and **GlobalISel** from **LLVM**.

The production AArch64 path is:

```text
SysY -> RaanaIR -> generic VCode/MIR -> register allocation -> AAPCS64 frame -> GNU AArch64 assembly
```

`anon_armv8` supplies typed instruction selection, AAPCS64 ABI hooks, and GNU
assembly emission. Allocation, frame layout, and final emission are driven by
the generic `taki_mir` pipeline; there is no alternate AArch64 backend path.

For more information please go to the crate `taki_mir`

#### Register Allocation

The register allocation is a port of regalloc2's **Ion backtracking allocator**.
It operates on the flat `VCode` operand array through an abstract `Function`
trait, supports spilling, live-range splitting, and parallel-copy resolution.

### Opt

At this time, we have 5 IR-level passes working.
See the Appendix(i)

The backend (`taki_mir`) also exposes a MIR-level pass pipeline
(`MIRPass` / `MIRPassPipeline`) split into pre-RA and post-RA phases,
mirroring the IR-level `Pass` / `PassesManager`. This framework will host
peephole combining, instruction scheduling, and other machine-code
optimizations.

## Current Progress

- Frontend float support
- Backend codegen
- IR text dump.
- IR optimization

## Usage

Build the CLI with `cargo build -p soyo_compiler`, then run:

```bash
soyo_compiler -S --target aarch64 -o testcase.s testcase.sy [-O 1]
```

`-S` is an alias for `--emit asm`. The default target is `riscv64`; pass
`--target aarch64` for GNU AArch64 assembly. `--emit ir`, `--emit llvm`, and
`--emit asm` select outputs. Multiple comma-separated `--emit` values treat
`-o` as an output directory and name files from the input stem.

### Optimization levels

| Level | IR passes | MIR DCE | MIR peephole | Pair combine | Scheduler |
|-------|-----------|---------|--------------|--------------|-----------|
| `-O0` | off       | off     | off          | off          | off       |
| `-O1` | on        | on      | on           | on           | off       |
| `-O2` | on        | on      | on           | on           | on        |

Explicit flags override the level defaults:

```bash
soyo_compiler -O2 --disable-sched -S --target aarch64 -o out.s test.sy
```

Available AArch64 MIR pass controls: `--enable/--disable-mir-dce`,
`--enable/--disable-mir-peephole`,
`--enable/--disable-pair-combine`, `--enable/--disable-sched`,
`--sched-model cortex-a53`.

Unsupported backend HIR is reported as a concise code-generation error with
function, block where available, instruction, type, phase, and legality
context. Internal compiler invariants remain fail-fast errors.

### gem5 performance testing

The container builds an ARM gem5 (syscall-emulation) model of the Xilinx
XCZU15EG Cortex-A53 subsystem: quad-core in-order A53, 32 KiB 2-way L1I /
32 KiB 4-way L1D, shared 1 MiB 16-way L2, ARMv8-A 64-bit with NEON and
single/double-precision FP. Run compiler-produced ELFs under it to collect
cycle counts and cache miss rates:

```bash
make gem5 perf/conv2d-1.sy        # build gem5 once, then run the case under gem5
make gem5-run path/to/program.elf   # run any AArch64 ELF under the A53 model
```

`make gem5-build` clones gem5 v25.1.0.1 into `.gem5/` and builds it (one-time,
30-60 min). gem5 SE simulates on the order of 100k instructions/s, so use the
small-input test cases rather than the MB-sized ones; each result lands in
`results/<case>/gem5-stats/stats.txt` with a compact summary printed by the
harness. `GEM5_ARGS` passes extra options (e.g. `--cpu-clock=1.5GHz`,
`--num-cpus=4`, `--maxinsts=100000000`).

## Build from source

To be announced

## Reference

lalrpop:
    - [crates.io](https://crates.io/crates/lalrpop)
    - [GitHub](github.com/lalrpop/lalrpop)
An convenient LR(1) 'parser' generator

[Koopa IR](https://github.com/pku-minic/koopa):
IR that used in `pku-minic` course. Influence heavily by `LLVM` and `Cranelift`
We learned a lot from Koopa IR, and also use some same technique when design our own IR.

[LLVM Passes Docs](https://llvm.org/docs/Passes.html):
We referred to the docs and learn how each pass work, and then chose some of them to implement in our `RaanaIR`.

[s2r](https://github.com/semisemisea/s2r)
Code of one member of our team attending `pku-minic` course.
`SoyoCompiler` is migrated and polish directly based on `s2r` repository.

## Appendix(i): Pass

Each pass will be introduced with a simple description. For more information, please help yourself on wikipedia/internet.

### SSA/mem2reg

Transform the original IR to static single assignment(SSA) form.

### ADCE

Aggressive dead code elimination that based on SSA.
It will assert every instruction, except instruction that have side-effect, is dead at beginning.

### SCCP

Sparse condition constant propagation that based on SSA.
Can propagate more constant than regular algorithm due to well property introduced by SSA.

### GVN

Global value numbering.
Find and replace the value/pattern that has been calculated.

### SR

Strength reduction.
Replace complex instruction to simple one.
E.g.: `%1 = mul %0, 2` is equivalent to `%1 = shl %0, 1`
