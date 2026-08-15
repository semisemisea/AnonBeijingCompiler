# 测试 harness：test.py 与 make test 全解

> 离线工作手册 G8。对象：`tests/test.py`（855 行）+ Makefile + Docker 流程。
> 这是本项目的**真正质量门禁**（比 `cargo test` 更重要）。

## 1. 整体架构

```
make test                      # 入口（Makefile）
  ├─ 交叉编译 compiler → aarch64-unknown-linux-musl（产物 target/host-musl/）
  ├─ 构建 sysylib/libsysy_arm.a（sysylib/）
  ├─ 起 soyo-test-tools Docker 镜像
  └─ 容器内运行 tests/test.py（挂载进去的，改它无需重建镜像）
        └─ 每个用例：
            编译（-S 汇编 / --emit llvm）→ 再编译一次（--emit ir dump）
            → clang+lld 链接（+ sysylib）→ qemu 运行 → 输出与 .out 比对
```

**关键**：只有 `Dockerfile` 改动才需重建镜像；`test.py`/用例是挂载的，改了
直接生效。`docker rm -f soyo-test` 清残留容器（残留会导致跑成上次的用例集）。

## 2. 一个用例的生命周期（run_test，test.py:280）

1. **编译**（`-O{opt} --target {target} -S -o case.s`）：失败 → **CE**。
   - baseline 模式（`make test-baseline`）：改用 clang 编译（-fcommon
     -ffp-contract=off -fsingle-precision-constant，对照用）。
   - llvm 后端（`make test-llvm`）：`--emit llvm` 产出 `.ll`。
2. **IR dump**（非 baseline 必跑：`--emit ir -o case.raana`）：**IR dump
   崩溃也算 CE**——即使汇编成功。
3. **链接**：clang + lld（-static -mcmodel=medany）+ `sysylib/libsysy_arm.a`
   → `case.elf`。链接失败 → **RE**（"link exit"）。
4. **运行**：qemu 执行 elf（stdin 来自 `case.in`，存在才喂）；`--runner gem5`
   则用 gem5 模拟（顺带产出 simInsts/CPI/缓存 miss 统计）。
5. **判定**：`combined_output(stdout, returncode)` = stdout（补尾换行）+ 退出码
   行，与 `case.out` **字节精确**比对（compare_output）。无 `.out` 文件 →
   自动 PASS。不一致 → **FAIL**（打印前 9 行差异 + 行数差 + stderr 前 300 字）。
   退出码非 0 且有 stderr → **RE**。超时 → **TLE**。

## 3. 判定速查表

| 状态 | 含义 | 触发点 |
|------|------|--------|
| PASS | 输出完全一致 | — |
| FAIL | 输出不匹配（stdout 或退出码） | compare_output 不一致 |
| CE | 编译错误 | compiler 或 IR dump 退出非 0 |
| RE | 运行错误 | 链接失败 / 运行退出非 0 且有 stderr |
| TLE | 超时 | 编译/链接/运行任一步超 TEST_TIMEOUT |
| SKIP | 跳过 | 用例在 SKIP_TESTS（缺输入） |

**注意**：`.out` 文件里**包含退出码行**（combined_output 语义）——手写
`.out` 时最后一行必须是程序退出码，否则必然 FAIL。

## 4. results/ 产物布局

每个用例在 `results/{functional,h_functional,perf}/` 下生成：
```
case.s               汇编输出（编译产物）
case.raana           RaanaIR dump（第二次编译）
case.elf             链接产物
case.o               （llvm 后端）llc 产物
case.compile.{stdout,stderr,return}
case.runtime.{stdout,stderr,return}
case.gem5-stats/     （gem5 模式）stats.txt + exitcode
```
`make clean-results` 重置全部结果。

## 5. make 目标（Makefile）

| 目标 | 作用 |
|------|------|
| `make test [TESTS=...] [ARGS="..."]` | 主测试（默认 functional+h_functional） |
| `make test TESTS="functional h_functional" ARGS="-O 2"` | 指定用例集 + 优化级别 |
| `make test functional/75_max_flow.sy ARGS="-O 2"` | 单用例 |
| `make test-riscv ARGS="-O 2"` | RISC-V 后端测试（**改 pass 必跑**） |
| `make test-llvm` | LLVM IR 导出对照 |
| `make test-baseline` | 容器内 clang 基线 |
| `make run-elf path.elf` | 容器内 qemu 运行单个 elf |
| `make debug-elf` | qemu-gdb 调试 |
| `make mca path.s` | llvm-mca 静态分析（cortex-a53） |
| `make gem5 <case>` | gem5 全系统模拟（慢，只用小输入） |

**坑**：harness 默认优化级别是 **-O0**！CI 只跑 -O0。性能里程碑必须显式
`ARGS="-O 2"`。另外 `make test ARGS="-O 0"` 实测会跑 O2（参数解析陷阱），
权威对照 = 容器编译 + qemu。

## 6. 如何加一个测试用例

1. 放对目录：`tests/functional/`（功能）、`tests/h_functional/`（隐藏/特殊）、
   `tests/perf/`（性能）。
2. 写 `case.sy`（SysY 源码）+ `case.out`（期望输出，**最后一行 = 退出码**）；
   需要 stdin 时加 `case.in`。
3. 本地验证：`make test functional/case.sy ARGS="-O 2"`（先
   `docker rm -f soyo-test` 清残留容器）。
4. 性能用例另跑 `scripts/perf_compare.sh` 看静态指令数对照 clang。

## 7. 常见坑

- **改了 test.py 不用重建镜像**（挂载）；改 Dockerfile 才需要。
- **残留容器**：`docker rm -f soyo-test`，否则跑的是上次的用例集。
- **退出码行**：.out 末行是 returncode，别漏。
- **IR dump 崩溃 = CE**：汇编 OK 但 raana dump 崩，同样算编译失败。
- **QEMU 慢**：性能用例（如 huffman）-O2 可能 30s+；迭代优先用静态计数
  （perf_compare.sh），gem5 更慢（只用小输入）。
- **Docker 时间戳坑**：`make test` 前 touch 源文件重编 musl，否则可能用上
  过期二进制（本地 compiler 过期 → M44_TRACE 之类静默无输出）。
