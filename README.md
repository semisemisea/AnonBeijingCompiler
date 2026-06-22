# AnonBeijingCompiler

Entry for CSC Compiler Implementation Competition. A SysY to Arm/RISC-V compiler, written in rust.

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

For more information please go to the crate `taki_mir`

#### Register Allocation

The register allocation is the simple linear one-pass scan allocation.
It could be upgrade to `Linear Greedy Scan` that `LLVM` adopted.

### Opt

At this time, we have 5 passes working.
See the Appendix(i)

## Current Progress

- Frontend float support
- Backend codegen
- IR text dump.
- IR optimization

## Usage

When most of work is done, you can:
Run the command

```bash
compiler -S -o testcase.s testcase.sy [-O1]
```

will generate assembly in file `testcase.s`.
Only this form of prompt is accepted by compiler.

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
