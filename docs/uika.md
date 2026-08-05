# uika_riscv：RISC-V 后端设计文档

本文档对应编译系统实现赛道的 RISC-V 后端，说明编译器的系统架构、模块划分、优化策略，以及对 Cranelift 的借鉴与大模型使用情况。共享 IR、共享 MIR 管线与寄存器分配的具体设计见工程内对应文档，这里以 RISC-V 后端特有部分为主。

## 一、定位与整体流水线

编译目标是把 SysY2026 程序编译为 64 位 RISC-V 汇编，经 `gcc -march=rv64gc` 汇编链接后，在 64 位 FPGA BOOM 软核（BOOM v3，乱序双发射）上运行。汇编代码按 `GCC -mcmodel=medany` 约定生成，支持在较大地址空间内运行。初赛阶段不使用 SIMD 指令。

整体流水线：

```
SysY2026 → RaanaIR（SSA HLIR，含 IR 优化）→ taki_mir VCode/MIR → 寄存器分配
         → RISC-V 帧 → GNU RISC-V 汇编
```

前端与后端共享的部分与 AArch64 后端一致：`soyo_compiler` 用 lalrpop 完成词法与语法分析并降到 RaanaIR；`taki_mir` 提供通用 VCode/MIR、寄存器分配、帧布局与发射。`uika_riscv` 只持有 RISC-V 特有的部分。与 AArch64 后端不同，RISC-V 使用 IR 层的公共优化管线，不注册 AArch64 门控的 pass（`mulmod_recognize`、`recursive_memoize`、`chain_to_switch`、`matmul_interchange` 等）。

## 二、目标平台与运行环境

依据技术方案第 27 条，目标实验设备为 CG-FPGA15EG，主要参数：

- CPU：BOOM v3（Berkeley Out-of-Order Machine Version 3），RISC-V 64GC 指令集，主频 50 MHz，乱序执行，双发射，超标量架构。
- 内存：2 GB，配备 FPU（IEEE754 格式）。初赛阶段不得使用 SIMD 指令，决赛阶段可根据开放的 SIMD 指令文档使用。
- 汇编与链接：gcc 13.3.0（Ubuntu 13.3.0-6ubuntu2~24.04），命令 `gcc -march=rv64gc`。
- 调试：目标程序直接运行于 BOOM 软核，可通过 GDB + OpenOCD 调试。
- 编译器调用形式：功能测试 `compiler testcase.sysy -S -o testcase.s`，性能测试带 `-O1`。

平台的指令约束影响后端设计：`-march=rv64gc` 不含向量扩展，指令选择只覆盖基础指令集与浮点扩展；主频 50 MHz 使动态指令数成为性能的主要度量，指令数少的序列直接对应更短的运行时间；medany 模型决定取址方式用 `auipc` 加 `addi` 的相对寻址。

## 三、模块划分

`uika_riscv` 的各模块及其职责如下。

| 模块 | 职责 |
|------|------|
| `regs.rs` | RISC-V 寄存器表与物理寄存器定义，ABI 参数寄存器、栈指针、帧指针、临时寄存器 |
| `instructions.rs` | 类型化机器指令（`MInst`）、操作数约束、GNU RISC-V 汇编发射 |
| `abi.rs` | RISC-V 64 参数位置规划、帧钩子、栈溢出存取、立即数越界地址合法化 |
| `lower.rs` | 从 RaanaIR 到 VCode 的指令与分支选择 |
| `labels.rs` | 类型化的块、函数、全局标签 |

该后端当前不注册目标相关的 MIR pass，优化集中在指令选择阶段完成（见第四节），与 AArch64 后端的 pre-RA/post-RA pass 管线形成差异。

## 四、指令选择

`lower.rs` 把 RaanaIR 指令选择到带类型的 `MInst`，并在物化操作数之前完成常量与模式折叠，使可编码的立即数直接进入指令。后端维持一条不变式：`i32` 值在 64 位寄存器中以符号扩展形式存放（`is_sign_extended_i32` 判定数据来源），因此 `W` 类指令与 32 位语义可以安全地在 64 位寄存器上运算。这一不变式由各数据来源共同维持：`lw` 符号扩展、`W` 类算术与移位写低 32 位并符号扩展、常量按 `abi.rs` 的 `gen_load_imm` 物化时符号扩展、`fcvt.w.s` 输出符号扩展。

- 算术：`add/sub/mul/div/rem` 与移位按 32/64 位选择 `W` 与非 `W` 形式。
- 常量折叠：常数加减选 `addiw/addi`，`and/or/xor` 选 `andi/ori/xori`，移位选 `slli/srli/srai` 立即数形式；`0 op x` 与 `x op 0` 的恒等与取负直接在操作数层面消去。减法常数取反后合并进 `addi` 的立即数。
- 比较：`x<k` 选 `slti`，`>=`、`<=`、`!=` 用 `xori 1` 翻转；`x==0`/`x!=0` 选 `seqz`/`snez`；`x==k` 折叠为 `addiw x, -k; seqz`；`x>k` 折叠为 `slti x, k+1`。浮点比较选 `flt.s`/`fle.s`/`feq.s`，不等号同样用 `xori 1` 翻转。
- 常量除法与取余：除数为 `2^n` 时用带符号偏置的移位序列（符号位经 `srai`/`srli` 得到偏置，再加后移位）；其余常数用 `signed_magic_i32` 的乘高序列替代 `divw`/`remw`，乘数无修正项时直接复用算术右移，减少指令数。取余由商乘回除数再减得到，避免第二段乘高序列。
- 常量乘法：乘数分解为 `2^n`、`2^n+1`、`2^n-1` 时折叠为移位加加减，替代 `mul`。
- `select`：无分支算术实现，`snez` 取非零值、取负得到全零或全一掩码，再用掩码做 `xor` 选择；浮点分支经整数寄存器位拷贝完成。
- 访存：常量 GEP 经 `analyze_gep` 与 `fold_gep_constant_offset` 分析后折叠进 load/store 寻址模式（`off(base)`）；索引按符号扩展不变式直接缩放（`2^n` 用移位，否则用 `mul`），动态项用 `add` 累加。
- 取址：全局与函数地址用 `la` 伪指令加载（汇编器展开为 `auipc` 加 `addi`，符合 medany 模型）；函数调用用 `call`，尾调用用 `tail` 伪指令（`auipc+jalr` 两条指令），复用调用方帧。
- 浮点转换：`fcvt.s.w` 与 `fcvt.w.s`（`rtz` 截断）覆盖 `int↔float` 转换。
- 清零：长度确定的 `MemZero` 在小尺寸内联 `sw` 展开，大尺寸调用 `memset`（参数经 `a0/a1/a2`）。
- 栈上数组：`alloc` 下降为栈槽分配，经 `alloc_stackslot_or_get` 复用同一数组的槽位，栈地址用 `StackAddr` 在分配后合法化。

## 五、寄存器分配与帧

与 AArch64 后端共用 `taki_mir` 的通用分配器，在扁平 VCode 操作数数组上运行，支持溢出、活性区间分裂与并行拷贝解析，块间值传递通过块参数边完成。

ABI 策略（`abi.rs` 与 `regs.rs`）：

- 整型参数用 `a0`-`a7`（`x10`-`x17`），`f32` 参数独立使用 `fa0`-`fa7`（`f10`-`f17`）。
- 溢出标量参数占据 8 字节栈槽。psABI 规定窄于 XLEN 的标量按 XLEN 位宽扩展后入栈。若按类型宽度紧凑排列，64 位参数会落在对 8 取模为 4 的偏移上，在 BOOM 上触发非对齐访问陷阱，QEMU 则静默放行，因此栈参数槽统一为 8 字节。
- 被调用方保存寄存器为整型与浮点各自的 `s0`、`s1` 与 `s2`-`s11`（硬件编码 8、9 与 18-27）。
- 调用在帧内预留 outgoing 溢出参数区；栈指针 `x2`，帧指针 `x8`，返回地址 `x1`。
- 帧布局：有 setup 区时在 `sp` 减量后于 `sp+base-8` 存 `ra`、`sp+base-16` 存 `fp`，再 `add fp, sp, base`；epilogue 反向恢复，栈槽与入参按帧指针相对寻址。

栈槽与溢出地址的立即数越界（超出 12 位有符号范围）在分配后合法化：`gen_spill_load/store` 与入参/出参存取先检查 `normalize_imm12`，越界时先用临时寄存器物化偏移再加基址，保证访存指令始终是合法的 `off(base)` 形式。

## 六、发射

指令发射同样通过 `taki_mir` 的 `EmitBuffer`。RISC-V 的分支按编码范围分类：B 型条件分支为 `RV_B`（13 位有符号偏移乘 2），`jal` 为 `RV_JAL`（21 位有符号偏移乘 2）。分支目标保持符号化，`finish()` 时解析并做范围检查，越界分支插入 veneer。`la`/`call`/`tail` 直接委托给 GNU 汇编器处理，取址与调用本身不受分支范围限制。

## 七、验证与质量门禁

RISC-V 后端与 AArch64 后端共享验证流程，另加目标相关门禁。

- 单元测试：`cargo test -p uika_riscv`，覆盖指令发射、常量折叠与 ABI 布局。
- 功能回归：`make test-riscv ARGS="-O 2"` 在 Docker harness 中跑 functional 与 h_functional 全部用例，比较 stdout 加换行加退出码与期望输出；每个用例同时跑 `-S` 与 `--emit ir` 两条路径。
- 双 target 回归：RISC-V 改动后跑 AArch64 全量，IR 层改动同理；新 IR pass 一律按 target 门控，确认 RISC-V 零回归。
- 动态运行：`make run-elf-riscv path/to/program.elf` 用 QEMU 运行目标程序，`make debug-elf-riscv` 走 QEMU/GDB 调试。

RISC-V 后端与 AArch64 后端共用同一功能用例集，当前双 target 152/152 全绿。

## 八、与 AArch64 后端的对照

两个后端共享 `taki_mir` 的 VCode、寄存器分配、帧与发射基建，差异集中在目标特有部分。

| 方面 | AArch64（`anon_armv8`） | RISC-V（`uika_riscv`） |
|------|------------------------|------------------------|
| 指令选择 | 面向性能，含 madd 融合、ccmp 链、NEON 向量 | 面向正确性与常数折叠，无向量 |
| MIR pass | pre-RA DCE/Peephole/ChainFusion/ConstCse，post-RA PairCombine/ListScheduler | 未注册 |
| IR 门控 pass | 启用 `mulmod_recognize`、`recursive_memoize`、`chain_to_switch` 等 | 只启用公共管线 |
| 调度 | Cortex-A53 模型列表调度 | 无（乱序核不依赖软件调度） |
| 运行时片段 | 内嵌 `memzero`、`calloc` | 无（大尺寸清零走 libc `memset`） |
| 常量除法 | `signed_magic_i32` 乘高序列 | 同，另含 `2^n` 移位序列 |

差异的技术原因：RISC-V 的 BOOM 乱序执行器会自行隐藏 load-use 延迟，软件列表调度收益有限；RISC-V 分支把条件物化进寄存器，判定树不能像 AArch64 那样把两次比较融合成一条 `ccmp`，`chain_to_switch` 因此不启用；NEON 需要向量扩展，rv64gc 不含。

## 九、对 Cranelift 的借鉴情况

RISC-V 后端对 Cranelift 生态的借鉴发生在共享管线上，具体如下。

1. 架构概念：`taki_mir` 的 VCode/MIR 管线参照 Cranelift 的 `machinst`（VCode）与 LLVM GlobalISel 的设计思想，包括 `MachInst` trait、lowering context、`ABIMachineSpec` 抽象、块参数边传输，以及在扁平操作数数组上运行的 `Function` trait。借鉴的是接口划分与数据流组织方式，代码为本工程自行实现。
2. 寄存器分配器：`taki_mir/src/reg_alloc/ion/` 是 regalloc2 0.15.1（BytecodeAlliance，Cranelift 生态）的 Ion 回溯分配器的改编移植，文件头注明来源与 Apache 2.0 + LLVM Exception 许可，适配到本工程的 `Function` trait 与稠密虚拟寄存器视图。

RISC-V 特有的部分（指令选择、ABI、指令发射、寄存器表）为自行实现，未参照或移植 Cranelift 的 RISC-V 后端源码。

按技术方案第 15 条，比赛禁止使用 GCC、LLVM 及其框架的源代码。本项目不使用 GCC 或 LLVM 的源代码，Cranelift 与 regalloc2 不属于 GCC/LLVM；全部代码从零构造，以上借鉴已在源码注释与本文档中标注与说明，满足第 14、17 条对第三方借鉴的可追溯要求。

## 十、大模型使用情况

开发过程中使用了大模型辅助工具，说明如下。

- 工具名称：OpenCode（交互式编码代理），以 DeepSeek 系列大模型为推理后端。
- 生成内容范围：部分 Rust 源码（指令选择、单元测试）、代码重构、缺陷修复，以及本文档的撰写。
- 人工修改情况：模型生成的代码经人工逐段审查、改写与集成后再进入工程；正确性以单元测试、功能用例与性能回归门禁验证。所有优化均按程序结构触发，未针对任何测例生成或调整代码。

生成代码的可追溯性通过代码注释、单元测试与本文档保证，符合章程第 7.4 条与技术方案第 15 条的要求。

## 十一、已知限制与后续工作

- MIR 优化管线未启用：目前依赖 lowering 阶段的折叠，分配后无配对、合并类 pass。可在公共管线稳定后按 RISC-V 指令形态补充。
- 性能优化：性能里程碑集中投入 AArch64 后端，RISC-V 的性能来自共享 IR 管线与指令选择的常数折叠，尚未针对 BOOM 的乱序特性做专门优化。
- 向量扩展：初赛按规则不使用 SIMD；决赛阶段可按开放的 SIMD 指令文档扩展指令选择。
