# Compiler TODO

Only pending work belongs in this file. Completed implementation notes and
historical measurements belong in commits, tests, or dedicated documentation.

## Parallel Priority: Cranelift 风格的整数强度削弱

本阶段参考本地 `../wasmtime/cranelift` 中的整数算术优化规则，但只移植
RaanaIR 当前指令集能够可靠表达、且不需要新增 `mulhi` 的部分。主要参考：

- `cranelift/codegen/src/opts/arithmetic.isle` 中整数乘法、常量除法和余数规则；
- `cranelift/codegen/src/opts/shifts.isle` 中零位移、连续位移和掩码化简规则；
- `cranelift/codegen/src/prelude_opt.isle` 中有符号二次幂除法/余数公式；
- `cranelift/filetests/filetests/runtests/sdiv.clif` 和 `srem_opts.clif` 中的
  正数、负数、边界值和负除数测试方法。

本阶段必须保持 SysY/C 风格的 `i32` 语义：整数算术按 32 位 wrapping 行为执行，
有符号除法向零截断，余数与被除数同号。不得把有符号 `% 2^k` 无条件改写成
`x & (2^k - 1)`，因为例如 `-3 % 2 == -1`，而 `-3 & 1 == 1`。

### 1. 范围、约束与规则优先级

- [ ] 本阶段只优化 `i32`，不得改写 `f32` 的 `Mul`、`Div` 或其他浮点运算。
  RaanaIR 当前只有 `i32` 和 `f32` 标量类型，因此所有整数规则都必须先检查
  结果与操作数类型，不能仅根据 `BinaryOp` 判断。
- [ ] 本阶段不实现任意非二次幂常量除法/余数的 magic-number 展开。Cranelift
  的通用算法依赖 signed/unsigned `mulhi`；RaanaIR 和两个目标后端当前没有
  对应 IR 能力。`x / 3`、`x % 3` 等表达式必须保持原样。
- [ ] 不新增兼容性分支或目标特定 IR。所有新规则必须保持目标无关，并能够由
  AArch64 和 RISC-V 现有的 `Add`、`Sub`、`And`、`Shl`、`Shr`、`Sar`
  lowering 正确处理。
- [ ] 明确并实现 rewrite 优先级：先处理 `% 2` 的比较消费端特化，再处理
  `Mul` 局部规则，再处理 `Div`/`Rem` 的通用二次幂展开，最后执行相关移位
  化简。该顺序必须避免先把 `Rem` 展开成多条算术指令，从而破坏
  `% 2 == 0/1/-1` 的高收益模式。
- [ ] 所有规则必须在现有 fixed-point pass 管线中收敛。第一次运行发生改写时
  返回 `true`；优化结果再次运行 `StrengthReduction` 必须返回 `false`，不得
  在等价形态之间来回改写。

### 2. 支持多指令 Rewrite 的 Layout 基础设施

- [ ] 在 `raana_ir/src/ir/layout.rs` 增加最小化的“在指定指令之前插入指令”
  接口，用于在原 `Div`/`Rem` 前按依赖顺序构造中间值。接口预计采用
  `insert_inst_before(before: Inst, inst: Inst)`，不要引入与当前任务无关的
  通用 layout 重构。
- [ ] 插入接口必须同时维护基本块内的 `IndexList<Inst>`、指令到基本块的
  `parent` 映射，以及 `BasicBlockLayout::back` 中的指令索引；插入后现有
  `parent_bb`、删除和遍历逻辑必须继续正确工作。
- [ ] 为插入接口增加 layout 单元测试：在首条、中间和 terminator 前插入；
  检查迭代顺序、`parent_bb`、后续删除、原指令索引和 terminator 均正确。
- [ ] `StrengthReduction` 先收集候选指令，再执行改写，禁止在借用 layout
  迭代器时直接修改指令链表。新建的中间指令必须按照定义支配使用的顺序插入，
  且全部位于原指令之前。
- [ ] 一条指令可以表达的 rewrite 应优先通过 `replace_inst_with` 保留原
  `Inst` 身份和 user 集合。结果需要替换成既有值时，使用
  `utils::visit_and_replace` 更新 use-def，再安全移除旧指令。多指令 rewrite
  必须验证每个新操作数的 `used_by` 集合都被 builder 正确登记。

### 3. 乘法强度削弱

- [ ] 实现整数乘法恒等式，并同时识别常量位于左侧和右侧的形式：
  `x * 0 -> 0`、`0 * x -> 0`、`x * 1 -> x`、`1 * x -> x`。
- [ ] 实现 `x * -1 -> 0 - x` 和 `-1 * x -> 0 - x`。必须使用 wrapping
  `i32` 语义，使 `i32::MIN * -1` 与当前前端/SCCP 的 `wrapping_mul` 行为
  一致，不得引入会在边界值上 panic 的宿主语言运算。
- [ ] 实现正二次幂常量乘法：`x * 2^k -> x << k` 和
  `2^k * x -> x << k`。只接受可表示为正 `i32` 的二次幂，因此常量范围为
  `2` 到 `1 << 30`，位移量通过 `trailing_zeros` 计算。
- [ ] 对 `x * 2` 统一生成 `Shl x, 1`，不照搬 Cranelift 中更高优先级的
  `x + x` 特例。当前目标是让 AArch64 和 RISC-V 都稳定选择立即数左移，
  并使 `tests/debug/01_and.sy` 的 `power * 2` 生成 `lsl #1`/`slli`。
- [ ] 实现 Cranelift 的变量二次幂模式：`x * (1 << y) -> x << y` 和
  `(1 << y) * x -> x << y`。仅当内层 `Shl` 的左操作数是整数常量 `1`，
  且所有相关值均为 `i32` 时匹配。
- [ ] 暂不把普通负二次幂乘法如 `x * -8` 展开成移位再取负。该变换会增加
  IR 指令数量，应等待目标成本模型或基准证明收益；`-1` 是单独的恒等式例外。
- [ ] 不改写 `x * 3`、`x * 5` 等非二次幂常量，也不在本阶段实现基于
  shift/add/sub 的任意常量乘法综合。

### 4. 有符号二次幂除法

- [ ] 实现 `x / 1 -> x`。
- [ ] 实现 `x / -1 -> 0 - x`，保持当前 `wrapping_div` 对
  `i32::MIN / -1` 的 wrapping 结果，不引入 trap 或 Rust debug overflow。
- [ ] 对正二次幂 `d = 2^k`、`1 <= k <= 30`，按 Cranelift 的向零截断公式
  展开：

  ```text
  sign    = x >>s (k - 1)
  bias    = sign >>u (32 - k)
  biased  = x + bias
  result  = biased >>s k
  ```

- [ ] 对负二次幂 `d = -2^k`，先使用相同公式计算 `x / 2^k`，再用
  `0 - quotient` 取负。需要覆盖 `-2`、`-4` 以及 `i32::MIN` 作为除数的
  情况；绝对值计算必须使用 `unsigned_abs` 或等价的无溢出方式。
- [ ] 证明并测试公式对正负被除数均为向零截断，而不是算术右移默认的向负无穷
  舍入。例如必须满足 `3 / 2 == 1`、`-3 / 2 == -1`、
  `3 / -2 == -1`、`-3 / -2 == 1`。
- [ ] 让展开后的公共子表达式能够被后续 fixed-point GVN 合并。不得在
  strength-reduction pass 内引入第二套局部值编号系统；依赖现有 GVN 处理
  相同 `x` 和相同除数产生的相同移位、偏置与商。

### 5. 有符号二次幂余数

- [ ] 实现 `x % 1 -> 0` 和 `x % -1 -> 0`。结果应直接替换为 `i32` 常量
  `0`，并允许 DCE 删除不再使用的操作数构造。
- [ ] 对正负二次幂 `d = +/-2^k`、`k >= 1`，按 Cranelift 的通用有符号
  余数公式展开：

  ```text
  t1      = x >>s (k - 1)
  t2      = t1 >>u (32 - k)
  biased  = x + t2
  masked  = biased & -(2^k)
  result  = x - masked
  ```

- [ ] 使用同一序列处理正、负二次幂除数，因为有符号余数的符号由被除数决定。
  必须验证 `3 % 2 == 1`、`-3 % 2 == -1`、`3 % -2 == 1`、
  `-3 % -2 == -1`。
- [ ] 掩码 `-(2^k)` 的构造必须使用 wrapping/位模式安全的方式，覆盖
  `k == 31`，不得在宿主 Rust 中计算会溢出的有符号 `1 << 31`。
- [ ] 若同一函数同时计算 `x / 2^k` 和 `x % 2^k`，确保两种展开产生的
  `sign`、`bias`、`biased` 和 quotient 等公共表达式形状一致，使 GVN 能够
  复用，而不是像当前后端 lowering 一样分别重复整套计算。

### 6. `% 2` 比较消费端特化

- [ ] 在通用 `Rem` 展开之前匹配以下形式，并同时支持比较操作数交换：

  ```text
  (x % 2)  == C    C == (x % 2)
  (x % -2) == C    C == (x % -2)
  (x % 2)  != C    C != (x % 2)
  (x % -2) != C    C != (x % -2)
  ```

- [ ] 对余数为零的比较使用奇偶位测试：

  ```text
  x % +/-2 == 0  -> (x & 1) == 0
  x % +/-2 != 0  -> (x & 1) != 0
  ```

- [ ] 对余数为正一的比较同时保留符号位和最低位：

  ```text
  x % +/-2 == 1  -> (x & 0x80000001) == 1
  x % +/-2 != 1  -> (x & 0x80000001) != 1
  ```

  `0x80000001` 在 RaanaIR 的 `i32` 常量中应表示为 `-2147483647`。AArch64
  后端应将其识别为逻辑立即数并生成 `and ..., #0x80000001`。
- [ ] 对余数为负一的比较使用同一掩码：

  ```text
  x % +/-2 == -1 -> (x & 0x80000001) == 0x80000001
  x % +/-2 != -1 -> (x & 0x80000001) != 0x80000001
  ```

- [ ] 只特化合法余数值 `-1`、`0`、`1`。其他常量比较可以交给 SCCP/后续
  范围优化；本阶段不要增加未经证明的值域推理。
- [ ] 按“比较 user”逐个改写，不要求某个 `Rem` 的所有 user 都是同一种比较。
  一个 `Rem` 同时用于比较和普通算术时，比较路径应获得掩码优化，普通数值路径
  仍由通用有符号余数规则正确展开。所有原 user 消失后由 DCE 删除旧 `Rem`。
- [ ] 多个比较产生相同 `And` 时保持规范化的操作数顺序，使后续 GVN 能合并
  掩码计算。不要在该 pass 中手工维护跨基本块公共表达式表。

### 7. 相关移位化简

- [ ] 参考 Cranelift `shifts.isle` 实现零位移恒等式：
  `x << 0 -> x`、`x >>u 0 -> x`、`x >>s 0 -> x`。
- [ ] 合并同方向常量移位，前提是两个位移量经过 `i32` 位移掩码语义处理后之和
  小于 32：

  ```text
  (x << a) << b    -> x << (a + b)
  (x >>u a) >>u b -> x >>u (a + b)
  (x >>s a) >>s b -> x >>s (a + b)
  ```

- [ ] 对逻辑左移和逻辑右移，如果规范化后的连续位移总量达到或超过 32，则结果
  可化为零。算术右移不得套用此规则，因为负数连续右移最终应为 `-1`。
- [ ] 不在本阶段移植需要多种整数位宽、extend/reduce、rotate 或 SIMD 类型的
  Cranelift shift 规则。RaanaIR 只有 `i32`，应避免添加当前类型系统无法表达的
  泛化代码。
- [ ] 确认移位化简能够清理二次幂除法/余数展开产生的 `Sar x, 0`，以及乘法
  转换后可能出现的连续 `Shl`，且不会与 SCCP 的 wrapping shift 语义冲突。

### 8. `StrengthReduction` 单元测试

- [ ] 为乘法规则覆盖 `x * 0`、`0 * x`、`x * 1`、`1 * x`、`x * -1`、
  `-1 * x`、`x * 2`、`8 * x`、`x * (1 << 30)`、`x * 3` 不转换、
  `f32 * 2.0` 不转换，以及 `x * (1 << y)` 两种操作数顺序。
- [ ] 为除法规则覆盖除数 `1`、`-1`、`2`、`8`、`-2`、`-4`、
  `i32::MIN`，并验证 `/ 3` 保持 `Div`。
- [ ] 为余数规则覆盖除数 `1`、`-1`、`2`、`8`、`-2`、`-4`、
  `i32::MIN`，并验证 `% 3` 保持 `Rem`。
- [ ] 为 `% 2` 比较特化覆盖常量 `-1`、`0`、`1`，`Eq`、`NotEq`，比较
  操作数交换，除数 `2` 和 `-2`，以及同一 `Rem` 同时存在比较 user 和普通
  算术 user 的情况。
- [ ] 为移位规则覆盖三种 shift-by-zero、三种同方向连续位移、逻辑移位总量
  达到 32 后置零，以及算术右移达到 32 时不得错误置零。
- [ ] 每种多指令 rewrite 都检查最终指令顺序、操作码、常量值、use-def 集合、
  `parent_bb` 和 return/branch user；不能只统计某种 opcode 的数量。
- [ ] 对完整 pass 增加收敛测试：第一次运行返回 `true`，第二次运行返回
  `false`，且第二次运行前后 IR 结构一致。

### 9. 语义与边界值验证

- [ ] 为二次幂乘法、除法和余数公式建立表驱动测试，至少覆盖：`0`、`1`、
  `-1`、`2`、`-2`、`3`、`-3`、`i32::MAX`、`i32::MIN`。
- [ ] 对每个支持的正负二次幂除数，将展开公式的结果分别与
  `wrapping_mul`、`wrapping_div`、`wrapping_rem` 比较。测试必须包含不整除的
  正负被除数，避免只验证恰好整除而漏掉舍入方向错误。
- [ ] 明确验证 `i32::MIN / -1`、`i32::MIN % -1`、
  `i32::MIN / i32::MIN` 和 `i32::MIN % i32::MIN`，确保优化前后不存在
  panic、trap 或值差异。
- [ ] 为 `% 2` 比较掩码在边界值上逐项验证，至少包括所有奇偶组合的正数、
  负数、`i32::MAX` 和 `i32::MIN`。

### 10. 集成验收与生成代码检查

- [ ] 运行 `cargo fmt --check`、`git diff --check`、
  `cargo test -p raana_ir` 和 `cargo test --workspace`。任何 use-def、layout、
  AArch64 或 RISC-V 回归均为阻塞问题。
- [ ] 使用当前编译器重新以 `-O1` 编译 `tests/debug/01_and.sy`，不要复用
  `results/debug` 中 revision、SHA256 或优化级别不一致的旧产物。
- [ ] 检查 `01_and` 的优化后 RaanaIR：`power * 2` 必须变成
  `shl ..., 1`；用于 `bit_a == 1`/`bit_b == 1` 的 `% 2` 必须变成
  `and ..., -2147483647` 加比较；不得再为这些比较保留完整有符号余数序列。
- [ ] 检查 AArch64 汇编包含 `and ..., #0x80000001` 和 `lsl ..., #1`，且
  对应位置不再出现用于 `% 2 == 1` 的 `asr/add/asr/sub` 四指令序列或
  `mul ..., 2`。
- [ ] 检查 RISC-V 汇编中乘二变为立即数左移，并确认 `% 2` 比较特化没有引入
  非法立即数或破坏有符号语义。AArch64 的逻辑立即数优势不应成为只在 AArch64
  正确的理由。
- [ ] 重新运行 `tests/functional/pow2_div_rem.sy`，验证 `/ 1`、`% 1`、
  `/ -1`、`% -1`、`/ 2`、`% 2`、`/ -4`、`% -4` 以及未优化的
  `/ 3`、`% 3` 输出完全一致。
- [ ] 运行包含高频二次幂算术的功能/性能用例，至少包括
  `tests/perf/huffman-01.sy`、`tests/perf/crc1.sy`、
  `tests/perf/crypto-1.sy` 和 `tests/h_functional/35_math.sy`，比较优化前后
  stdout、返回码和超时状态。
- [ ] 记录 `01_and`、Huffman/CRC 和 crypto 用例中的静态 `mul`、`div`、
  `rem` 展开、shift、and 与总指令数量。只有在语义测试通过后才报告代码质量
  改善，不以单个汇编片段替代完整回归验证。

### 11. 明确延期事项

- [ ] 任意常量有符号/无符号除法和余数的 magic-number 优化延期，直到 RaanaIR
  定义 `mulhi` 语义，并且 AArch64/RISC-V lowering、常量传播、GVN、LLVM
  writer 与边界测试全部具备对应支持。
- [ ] 基于成本模型的负二次幂乘法、任意常量 shift/add/sub 综合、目标相关
  peephole 和循环级 induction-variable 优化延期；这些工作必须单独测量，不能
  混入本次目标无关 strength-reduction 实现。
- [ ] `&&` 到 AArch64 `ccmp`、条件更新到 `csel`、循环旋转和栈帧消除不属于
  本阶段。它们分别属于 boolean/if-conversion、目标指令选择、循环优化和
  frame/liveness 清理，应继续作为独立任务追踪。
