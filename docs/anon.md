# anon_armv8：AArch64 后端设计文档

本文档对应编译系统实现赛道的 ARM 后端，说明编译器的系统架构、模块划分、优化策略，以及对 Cranelift 的借鉴与大模型使用情况。共享 IR 与优化 pass 的具体设计见工程内对应文档，这里以后端部分为主，必要时引用共享管线。

## 一、定位与整体流水线

编译目标是把 SysY2026 程序编译为 ARMv8-A 64 位汇编，经 `gcc -march=armv8-a` 汇编链接后，在基于赛灵思 XCZU15EG（集成 Cortex-A53）的 Ubuntu 22.04 平台上运行。

整体流水线：

```
SysY2026 → RaanaIR（SSA HLIR，含 IR 优化）→ taki_mir VCode/MIR → 寄存器分配
         → AAPCS64 帧 → GNU AArch64 汇编
```

前端与后端共享的部分：`soyo_compiler` 用 lalrpop 生成 SysY2026 词法与语法分析器，构建 AST 后降到 RaanaIR；RaanaIR 是 SSA 形式的高层中间表示，持有全部 IR 优化 pass。`taki_mir` 提供通用机器中间表示（VCode/MIR）、寄存器分配、帧布局与发射基础设施。`anon_armv8` 只持有 AArch64 特有的部分，汇编输出使用 GNU AArch64 语法。

## 二、目标平台与运行环境

依据技术方案第 26 条，目标实验设备为 CG-FPGA15EG，主要参数：

- CPU：赛灵思 XCZU15EG ARM Cortex-A53 MPCore，4 核，其中 2 个隔离核（`isolcpus=2,3`）用于性能测评，L1 数据缓存 32 KB 4 路组相联，L1 指令缓存 32 KB 2 路组相联，L2 缓存 1 MB 16 路共享，支持 ARMv8-A 64 位指令、NEON 与单双精度浮点。
- 内存：4 GB DDR4，操作系统 Ubuntu 22.04（64 位）。
- 汇编与链接：gcc 11.2.0，命令 `gcc -march=armv8-a`。
- 编译器调用形式：功能测试 `compiler testcase.sysy -S -o testcase.s`，性能测试带 `-O1`。

平台特性对后端设计的影响集中在三点。调度模型针对 A53 的顺序双发射流水线建模；缓存大小决定循环结构优化的目标（见第九节的 IR 门控优化）；隔离核与固定主频使静态指令数与数据访问模式可以直接反映性能。

## 三、模块划分

`anon_armv8` 的各模块及其职责如下。

| 模块 | 职责 |
|------|------|
| `regs.rs` | AAPCS64 物理寄存器表与分配器策略，参数寄存器、被调用方保存寄存器、栈指针编码 |
| `instructions.rs` | 类型化机器指令（`MInst`）、操作数约束、指令合法性校验、GNU AArch64 汇编发射 |
| `constants.rs` | 宽度感知的整型常量规划，在 `movz/movn/movk`、逻辑立即数与零之间选择最短物化序列 |
| `abi.rs` | AAPCS64 参数位置规划、帧钩子、栈溢出存取、分配后地址合法化 |
| `lower.rs` | 从 RaanaIR 到 VCode 的指令与分支选择 |
| `labels.rs` | 类型化的块、函数、全局标签 |
| `passes/` | 目标相关的 MIR 优化 pass（见第七节） |
| `sched/` | Cortex-A53 调度模型：指令延迟表、依赖图构建、周期模拟器 |
| `runtime/` | 内嵌到汇编输出中的运行时片段（`memzero.S`、`calloc.S`） |

## 四、指令选择

`lower.rs` 把 RaanaIR 指令选择到带类型的 `MInst`。整数运算按 32/64 位宽度选择 W（32 位）或非 W 形式，并在物化操作数之前完成以下折叠，避免为可折叠的常量额外发射加载指令。

- 乘加融合：`a*b+c` 与 `a*b-c` 折叠为 `madd`/`msub`。
- 常量乘法：乘数分解为 `2^n`、`2^n±1` 时用移位加加减替代 `mul`。
- 常量除法：有符号除法与取余在除数非 `2^n` 时改写为 `signed_magic_i32` 的乘高序列（`smull; sxtw; sdiv; msub` 等），避免库调用。
- `select`：默认选择为 `csel`。条件本身若为单次使用的比较合取树（`band`/`bor` 两个纯比较），折叠为 `cmp; ccmp; csel`，`and` 用 `#nzcv` 使整体为假，`or` 使整体为真；两个分支分别为 `1` 与 `0` 常量时改为 `cset`，顺序相反时取反条件再 `cset`。向量 `select` 用 `bsl` 位选择。
- 分支：对整数比较的合取树（3 到 8 个比较）直接在标志位层面求值，先 `cmp`，再逐条 `ccmp`，最后 `b.cc`，不物化布尔值；`and`/`or` 形式同理折叠为 `ccmp` 链。单次使用的纯比较生产者会先被下沉到分支处再发射。
- 访存：常量 GEP 折叠进 load/store 寻址模式，经 `analyze_gep` 与 `fold_gep_constant_offset` 分析后按地址基址加立即数偏移发射。
- 浮点：`fadd/fmul/fdiv` 等选择为标量浮点指令，比较通过 `fcmp` 加 `cset`。
- 向量：对 `<4 x i32>`、`<2 x i64>` 等向量类型完整选择到 NEON，包括 `ld1/st1`、splat、按元素运算、插入/提取与 `faddp`/归约指令，向量参数按 ABI 落入 `v0`-`v7`。

常量物化由 `constants.rs` 统一规划。常量先按 16 位块切分，统计非零块与非全一块的数量：单个非零块选 `movz`，单个非全一块选 `movn`，能由逻辑立即数编码选 `orr zr, #imm`，否则以零或全一为种子，选择需要 `movk` 补块更少的种子，逐块补齐，保证序列长度最短。该规划同时服务于 `-O0` 与 `-O2`，并作为 MIR 层 ConstCse 的物化入口。

## 五、寄存器分配

后端运行在 `taki_mir` 的通用分配器上，其工作方式如下。

- 指令在下降时进入 VCode，每个指令的寄存器操作数通过操作数收集器（operand collector）登记到扁平的虚拟寄存器数组，由 `Function` trait 暴露给分配器。
- 块间值传递通过块参数边（block parameter edge）完成。SSA 的 `phi` 在下降时被改写为块入口参数，块间的并行拷贝由分配器生成的 move 序列在发射前解析。
- 分配器对每个函数先构建稠密的虚拟寄存器视图，计算活性区间，再执行回溯分配，支持溢出与活性区间分裂。

分配器策略与寄存器环境在 `regs.rs` 声明：分配器保留整型 scratch `x16` 与浮点 scratch `v31`，分配后 `x14`-`x17` 与 `v30`/`v31` 用于地址物化与栈到栈搬用。栈指针以 `PReg` 的专用硬件编码参与活性分析，发射时折叠为 `sp`。

## 六、帧与 ABI

ABI 策略（`abi.rs` 与 `regs.rs`）：

- 整型与指针参数用 `x0`-`x7`，`f32` 参数独立使用 `v0`-`v7`，向量参数使用 `v0`-`v7`。
- 溢出标量参数占据 8 字节栈槽，向量参数溢出占 16 字节栈槽。
- 调用在帧内预留最大的 outgoing 溢出参数区，不动态调整 `sp`。
- ABI 边界上 `sp` 保持 16 字节对齐。
- 整型被调用方保存寄存器为 `x19`-`x28`；浮点与向量视图下 `v8`-`v15` 在实现低 64 位保存之前不参与分配。

帧生成的细节：

- prologue：有 setup 区时先 `stp fp, lr, [sp, #-16]!` 保存帧指针与返回地址，再调整 `sp` 到帧底，最后 `add fp, sp, #total`，使 `fp` 指向调用方 `sp`，入栈参数可用 `[fp, #off]` 直接寻址。
- 被调用方保存寄存器按 8 字节槽从帧顶向下布局，相邻整型对被合并为单条 `stp`，向量寄存器按 16 字节槽对齐。
- epilogue：先 `add sp, sp, #(total - setup)`，再 `ldp fp, lr, [sp], #16` 恢复。
- 地址合法化：`StackAddr`、`Load`、`Store` 的帧相对地址在分配后解析。能直接编码的偏移保留，超出编码范围的先在 post-RA scratch 中物化地址，再以 `[base, #imm]` 发射。溢出区偏移在合法化时叠加 outgoing 参数区大小。

## 七、MIR 优化管线

`passes/mod.rs::build_pipeline` 构建 AArch64 的 MIR pass 管线，按 `-O` 级别开关（`-O1` 及以下关闭调度与配对，`-O0` 全关）。

分配前（pre-RA）：

- `dce`：机器指令级死代码消除。
- `peephole_combine`：局部指令模式合并。
- `chain_fusion`：把 `chain_to_switch` 产生的 `(eq, lt)` 比较节点对融合进检查块的一次比较。
- `const_cse`：把自然循环（唯一 preheader 且不超过 60 条指令）内的 `LoadImm`/`MovFromZero` 常量物化提升到 preheader 并去重，热循环每轮减少常量加载。循环形态由块级循环分析给出，属于 `taki_mir` 的通用基建。

分配后（post-RA）：

- `pair_combine`：把相邻的 `ldr`/`str` 配对为 `ldp`/`stp`。
- `list_scheduler`：基于 Cortex-A53 模型的列表调度。

调度器（`sched/`）的建模对象是 A53 的顺序双发射 8 级流水线。延迟表按指令类别给出吞吐与延迟（ALU 延迟 1、乘加延迟 3、L1 load 延迟 2 等），依赖图由 `dag.rs` 按真依赖与访存依赖构建，列表调度在基本块内重排指令以隐藏 load-use 延迟，效果由 `simulator.rs` 的周期模拟校验。L1 load 延迟 2 使紧随其后的依赖指令停顿 1 拍，调度器围绕用独立指令填充这个空档设计。

## 八、发射与分支优化

文本级发射通过 `taki_mir` 的 `EmitBuffer` 完成。每个槽对应一条定宽（4 字节）指令，指令以文本模板累积，分支作为目标符号化的结构化槽写入，标签在 `finish()` 时解析。这样分支优化可以在不修补字节的前提下对分支做截断、取反与重定向。

分支按编码范围分类（`tbz/tbnz` 的 BRANCH14、`b.cond/cbz/cbnz` 的 BRANCH19、`b/bl` 的 BRANCH26）。解析时做范围检查，越界分支插入 veneer 中转；标签别名与连续的 `goto next; next:` 链被合并，别名数量受阈值约束以避免平方级退化。分支优化可经配置关闭，用于与未优化输出做差分验证。

## 九、IR 层的 AArch64 门控优化

部分 IR 优化只在 AArch64 后端注册（`raana_ir/src/opt/pass.rs` 按 `TargetPolicy` 门控），RISC-V 不注册以避免回归。这些 pass 依赖 AArch64 后端能提供对应的指令形态：

- `mulmod_recognize`：识别倍增型模乘递归，改写为 `smull; sxtw; sdiv; msub` 内联序列。
- `recursive_memoize`：纯自递归函数的运行时缓存改写，配套后端内嵌 `soyo_calloc` 分配器。
- `zero_store_loop`：零初始化循环折叠为一次运行时长度 `MemZero`，后端发射内嵌清零。
- `chain_to_switch`：相等链改写为平衡判定树，配合后端的 `chain_fusion` 每次比较只发一条指令。
- `matmul_interchange`、`blocked_reduction`、`reduction_unroll`：矩阵乘内层循环交换与归约展开，破坏串行累加链、改善缓存访问。
- `licm` 的 load 数量上限按后端调整。

这些 pass 全部按程序结构触发，不做函数名或输入特征匹配，符合 `docs/Illegal_optimization.md`。

## 十、运行时支持

后端按需向输出汇编中内嵌带 `.L` 局部符号的运行时片段（`runtime.rs`），避免对测试环境的额外依赖。

- `.Lsoyo_memzero`：长度确定的清零分两级，小尺寸内联 store 展开，大尺寸调用内嵌清零例程；清零例程按尺寸分级使用标量 store、`stp` 与 `dc zva`。
- `.Lsoyo_calloc`：把两个 32 位参数零扩展到 64 位后尾调用 glibc `calloc`，供记忆化优化分配运行时缓存。

片段是否嵌入由程序扫描决定（是否存在不可内联的 `MemZero` 或对 `soyo_calloc` 的调用），未用到的片段不会出现在输出中。

## 十一、优化方法论与里程碑成果

性能工作的度量以两级进行。静态层面用 `scripts/perf_compare.sh` 统计生成汇编的指令数，对照 clang/gcc `-O2` 的汇编；动态层面用 QEMU 运行时间与 gem5（XCZU15EG Cortex-A53 建模，syscall 仿真）的 sim_insts 与缓存统计。静态计数用于迭代中的快速比较，动态数据用于最终确认。

已落地的代表性成果（记录于 `TODO.md`，QEMU 时间与静态指令数，双 target 152/152 全绿）：

- h-1 族：M68 纯递归记忆化，QEMU 22.79s 降到 6.5-8.4s（约 3 倍），clang `-O2` 的 17.10s 被反超约 2.6 倍。
- fft0：M60 模乘递归识别改写，QEMU 14.93s 降到约 0.5s，静态 373 降到 368，clang 为 359。
- h-5：M67 寄存器分块，QEMU 7.16s 降到 4.49s（-37%）。
- h-4：M65 循环不变量常量提升，QEMU 约 -11%。
- many_mat_cal：M57/M58 归约展开与矩阵乘循环交换 i-j-k 到 i-k-j 加行缓冲，QEMU 35.2s 降到 7.8s（约 4.5 倍）。
- huffman：M59 bit-test 前置，QEMU 约 60.9s 降到 31.7s。

## 十二、验证与质量门禁

后端每个改动都要过以下门禁。

- 单元测试：`cargo test -p raana_ir`、`-p taki_mir`、`-p anon_armv8`，指令发射、指令选择折叠与 ABI 布局均有随源文件内联的单元测试。
- 功能回归：`make test ARGS="-O 2"` 在 Docker harness 中跑 functional 与 h_functional 全部用例，比较 stdout 加换行加退出码与期望输出；每个用例同时跑 `-S` 与 `--emit ir` 两条路径。
- 双 target 回归：改动 AArch64 后端后跑 `make test-riscv`，改动 IR 层时两边都跑，确认另一 target 无回归。
- 静态性能回归：`scripts/perf_compare.sh` 确认语料静态指令数不劣于基线。
- 确定性：双 target 在各优化级别下输出逐字节一致，重跑 5 次不出现抖动。
- 动态验证：gem5 统计周期数与缓存缺失率，与静态代理对照。

## 十三、对 Cranelift 的借鉴情况

后端在结构与局部实现上借鉴了 Cranelift 生态的公开设计，具体分三类，全部在对应源码注释中标注。

1. 架构概念：`taki_mir` 的 VCode/MIR 管线参照 Cranelift 的 `machinst`（VCode）与 LLVM GlobalISel 的设计思想，包括 `MachInst` trait、lowering context、`ABIMachineSpec` 抽象、块参数边传输，以及在扁平操作数数组上运行的 `Function` trait。借鉴的是接口划分与数据流组织方式，代码为本工程自行实现。
2. 寄存器分配器：`taki_mir/src/reg_alloc/ion/` 是 regalloc2 0.15.1（BytecodeAlliance，Cranelift 生态）的 Ion 回溯分配器的改编移植，文件头注明来源与 Apache 2.0 + LLVM Exception 许可。移植过程中把分配器接口归一化到本工程的 `Function` trait 与稠密虚拟寄存器视图，并去掉了上游的注解与统计设施。
3. 编码细节：若干编码工具函数与 Cranelift 一致，包括 `Imm12::maybe_from_u64` 的立即数形式判定、栈指针在 `PReg` 中的硬件编码约定、栈指针调整辅助函数的形态，以及发射缓冲对 Cranelift `MachBuffer` 的建模。

按技术方案第 15 条，比赛禁止使用 GCC、LLVM 及其框架的源代码。本项目不使用 GCC 或 LLVM 的源代码，Cranelift 与 regalloc2 不属于 GCC/LLVM；全部代码从零构造，以上借鉴已在源码注释与本文档中标注与说明，满足第 14、17 条对第三方借鉴的可追溯要求。

## 十四、大模型使用情况

开发过程中使用了大模型辅助工具，说明如下。

- 工具名称：OpenCode（交互式编码代理），以 DeepSeek 系列大模型为推理后端。
- 生成内容范围：部分 Rust 源码（指令选择、优化 pass、单元测试）、代码重构、缺陷修复，以及本文档的撰写。
- 人工修改情况：模型生成的代码经人工逐段审查、改写与集成后再进入工程；正确性以单元测试、功能用例与性能回归门禁验证。所有优化均按程序结构触发，未针对任何测例生成或调整代码。

生成代码的可追溯性通过代码注释、单元测试与本文档保证，符合章程第 7.4 条与技术方案第 15 条的要求。

## 十五、已知限制与后续工作

- NEON 自动向量化（M69）：后端已有完整向量 lowering，但 IR 层尚无 SLP 或循环向量化 pass，目前全部标量。下一步在 IR 层识别 trip-count 已知、无回环依赖、访存连续的整数循环，改写为 `<4 x i32>` 向量运算加归约。
- `v8`-`v15` 未参与分配：它们的低 64 位保存尚未实现，被调用方保存的浮点寄存器范围因此受限。
- 记忆化 Phase D：命中路径仍是完整函数调用，可进一步内联进调用方循环或用 16 位打包缓存减半内存。
- 矩阵乘 M59：在 i-k-j 形状上做内层 j 循环寄存器阻塞与部分展开。
