#!/usr/bin/env python3
"""容器内测试 runner：SysY 用例的编译 + 执行 + 结果比对（由 Makefile 转发调用）。

对每个用例的流水线：
  1. 编译  : compiler -S foo.sy → foo.s 汇编（--baseline 时改用 clang 交叉编译）
  2. 出 IR  : compiler --emit ir → foo.raana（调试产物，baseline 模式跳过）
  3. 链接  : clang + sysylib 运行时库（libsysy_<arch>.a）→ foo.elf
  4. 执行  : qemu-<arch>-static 运行 ELF（--runner gem5 时用 gem5 全系统模拟）
  5. 比对  : stdout + 换行 + 退出码拼成 combined_output，与 golden 文件 .out 逐字节比对

调用入口（容器内 /work 下执行，见 Makefile 对应目标）：
  make test           AArch64 + 编译器自身后端
  make test-riscv     RISC-V 目标
  make test-baseline  clang 交叉编译作为参考实现（差分对照）
  make test-llvm      后端输出 LLVM IR，再用 llc 生成目标码
等价手动命令：python3 /work/tests/test.py [选项] [用例路径...]

产物布局：每个用例的中间产物（.s/.o/.elf/.stdout/.stderr/.return）镜像写到
results/<tests 下相对路径>/，文件名后缀区分阶段（.compile.* 编译期 / .runtime.* 运行期）。

本文件是纯测试基础设施，不参与编译器构建；改动后跑 make test 或
python3 -m py_compile tests/test.py 验证语法。
"""

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import math
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import sys
import time
from datetime import datetime

# 容器内固定路径：/work 是宿主机仓库根目录的挂载点，tests 与 results 都在其下
ROOT = Path("/work")
TESTS_ROOT = ROOT / "tests"     # SysY 用例目录（functional/ h_functional/ perf/）
RESULTS_ROOT = ROOT / "results"  # 产物输出目录（git 忽略）

# 双目标架构配置：aarch64（默认）与 riscv64
#   sysylib      : 静态运行时库名（sysylib/ 目录内预编译，提供 getint/putint 等 SysY 库函数）
#   clang_target : clang 交叉编译的目标三元组（clang 默认编宿主架构，必须显式指定）
#   sysroot      : 目标架构系统根目录（头文件/库；clang 交叉编译与链接都需要）
#   qemu         : 用户态模拟器，在宿主 CPU 上直接运行交叉编译出的 ELF
#   llvm_triple  : llc 把 LLVM IR 变成目标码时用的 triple
TARGET_CONFIG = {
    "aarch64": {
        "sysylib": "libsysy_arm.a",
        "clang_target": "aarch64-linux-gnu",
        "sysroot": "/usr/aarch64-linux-gnu",
        "qemu": "qemu-aarch64-static",
        "llvm_triple": "aarch64-linux-gnu",
    },
    "riscv64": {
        "sysylib": "libsysy_riscv.a",
        "clang_target": "riscv64-linux-gnu",
        "sysroot": "/usr/riscv64-linux-gnu",
        "qemu": "qemu-riscv64-static",
        "llvm_triple": "riscv64-linux-gnu",
    },
}
# 编译器二进制路径；容器内固定为 /work/target/release/compiler，可用环境变量 SOYO_COMPILER 覆盖
DEFAULT_COMPILER = Path(
    os.environ.get("SOYO_COMPILER", "/work/target/release/compiler")
)

# 终端 ANSI 颜色：stdout 不是终端（重定向/管道）时自动关闭，避免输出夹带转义序列
COLOR = sys.stdout.isatty()
CODES = {
    "reset": "\x1b[0m",
    "bold": "\x1b[1m",
    "dim": "\x1b[2m",
    "green": "\x1b[32m",
    "red": "\x1b[31m",
    "yellow": "\x1b[33m",
    "cyan": "\x1b[36m",
    "magenta": "\x1b[35m",
}
# 单个用例的总超时（秒）：编译/出 IR/链接/运行共享同一计时器（见 remaining_timeout）
TEST_TIMEOUT = 600

# 六种判定：PASS 输出正确 / FAIL 输出与 .out 不一致(WA) / CE 编译期错误
#            RE 运行期错误（链接失败或程序崩溃）/ TLE 超时 / SKIP 跳过
STATUSES = ("PASS", "FAIL", " CE ", " RE ", " TLE", "SKIP")

# 按用例相对路径跳过的表；SKIP_TESTS 恒为空集，此机制当前是死代码
SKIP_TESTS = {}

STATUS_STYLES = {
    "PASS": ("green", "bold"),
    "FAIL": ("red",),
    " CE ": ("dim",),
    " RE ": ("magenta",),
    " TLE": ("yellow", "bold"),
    "SKIP": ("dim",),
}


def paint(text, *styles):
    """按样式列表给文本上色（CODES 里的键名）；COLOR=False 时原样返回。"""
    if not COLOR:
        return text
    return f"{''.join(CODES[s] for s in styles)}{text}{CODES['reset']}"


def paint_status(status):
    """把状态字符串（如 "PASS"）染成它在 STATUS_STYLES 里定义的颜色。"""
    return paint(status, *STATUS_STYLES[status])


def rel_test(path):
    """把用例绝对路径转成相对 TESTS_ROOT 的路径（展示与 results 镜像目录都用它）。"""
    return path.resolve().relative_to(TESTS_ROOT)


def resolve_test_path(raw):
    """把命令行传入的用例路径解析成绝对路径：
    绝对路径直接用；'tests/xx' 开头剥掉前缀；其余按相对 TESTS_ROOT 解析。"""
    path = Path(raw)
    if path.is_absolute():
        return path.resolve()
    parts = path.parts
    if parts and parts[0] == "tests":
        return (TESTS_ROOT / Path(*parts[1:])).resolve()
    return (TESTS_ROOT / path).resolve()


def collect_tests(paths):
    """收集待跑用例：无参数时取 tests/ 下全部 *.sy；有参数时逐项解析
    （目录递归收集、.sy 文件直接用），并强制要求路径位于 tests/ 下。
    返回排序后的 .sy 绝对路径列表。"""
    if not paths:
        return sorted(TESTS_ROOT.rglob("*.sy"))

    files = []
    for raw in paths:
        path = resolve_test_path(raw)
        if not path.is_relative_to(TESTS_ROOT):
            raise ValueError(f"test path must be under /work/tests: {raw}")
        if path.is_dir():
            files += sorted(path.rglob("*.sy"))
        elif path.suffix == ".sy" and path.exists():
            files.append(path)
        else:
            raise ValueError(f"test path is not a .sy file or directory: {raw}")
    return sorted(files)


def compare_output(actual, expected_path, stderr):
    """比对实际输出与 golden 文件 .out（字节级）。
    无 .out 视为 PASS（仅记录实际输出，供临时用例容错）；字节一致 PASS；
    否则逐行找差异（最多列 8 行）+ 行数差异 + stderr 摘要，返回 (status, msg)。"""
    if not expected_path.exists():
        return "PASS", f"(no .out, output={actual!r})"

    expected = expected_path.read_bytes()
    if actual == expected:
        return "PASS", ""

    got_lines = actual.decode("utf-8", "replace").splitlines()
    want_lines = expected.decode("utf-8", "replace").splitlines()
    lines = ["MISMATCH"]
    for idx, (got, want) in enumerate(zip(got_lines, want_lines), 1):
        if got != want:
            lines.append(f"  line {idx}: got {got!r}  want {want!r}")
            if len(lines) >= 9:
                break
    if len(got_lines) != len(want_lines):
        lines.append(f"  line count: got {len(got_lines)}  want {len(want_lines)}")
    if stderr:
        lines.append(f"  stderr: {stderr.decode('utf-8', 'replace').strip()[:300]}")
    return "FAIL", "\n".join(lines)


def combined_output(stdout, returncode):
    """SysY 判定约定：程序输出 = stdout（末尾补一个换行）+ 退出码一行。
    .out golden 文件就是这个格式，例如 main 里 return 3 且无输出 → b"3\\n"。"""
    if stdout and not stdout.endswith(b"\n"):
        stdout += b"\n"
    return stdout + f"{returncode}\n".encode()


# gem5 全系统模拟器（性能测量用，比 qemu 慢几个数量级）；路径可用环境变量覆盖
GEM5_BIN = Path(os.environ.get("SOYO_GEM5", "/work/gem5/build/ARM/gem5.opt"))
GEM5_CONFIG = Path(
    os.environ.get("SOYO_GEM5_CONFIG", "/work/gem5-config/a53_se.py")
)


def read_stats_table(path):
    """解析 gem5 的 stats.txt（每行 '名称 值'），返回 {名称: 原始值字符串}。"""
    if not path.exists():
        return None
    stats = {}
    for line in path.read_text().splitlines():
        key, sep, rest = line.partition(" ")
        if sep and rest.strip():
            stats[key] = rest.split()[0]
    return stats


def fmt_count(value):
    """把计数器大数格式化成 1.23K/M/G 的人读形式（gem5 摘要用）。"""
    try:
        v = int(float(value))
    except ValueError:
        return value
    if v >= 1e9:
        return f"{v / 1e9:.2f}G"
    if v >= 1e6:
        return f"{v / 1e6:.2f}M"
    if v >= 1e3:
        return f"{v / 1e3:.2f}K"
    return str(v)


def miss_rate(stats, path):
    """算某缓存层级（如 system.cpu0.icache）的缺失率百分比；缺统计项返回 None。"""
    hits = stats.get(path + ".overallHits::total")
    misses = stats.get(path + ".overallMisses::total")
    if hits is None or misses is None:
        return None
    total = int(hits) + int(misses)
    if total == 0:
        return 0.0
    return 100.0 * int(misses) / total


# 单核配置下统计组前缀是 system.cpu.* 而非 system.cpu0.*，两个都试。
# （原英文注释：With a single CPU the stat group is system.cpu.* rather than
#  system.cpu0.*, so try both prefixes.）
CPU_STAT_BASES = ("system.cpu0.", "system.cpu.")


def summarize_gem5(elf, stats_dir):
    """把 gem5 stats.txt 汇总成一行摘要：模拟时间/指令数/CPI/主机耗时、
    L1I/L1D/L2 缺失率、stats 目录位置（性能对比时人工看）。"""
    stats = read_stats_table(stats_dir / "stats.txt")
    if stats is None:
        return "gem5: no stats.txt produced"
    lines = []

    cpi = next(
        (stats[k] for k in (base + "cpi" for base in CPU_STAT_BASES) if k in stats),
        None,
    )
    parts = []
    if stats.get("simSeconds"):
        parts.append(f"sim {stats['simSeconds']}s")
    if stats.get("simInsts"):
        parts.append(f"{fmt_count(stats['simInsts'])} inst")
    if cpi:
        parts.append(f"CPI {cpi}")
    if stats.get("hostSeconds"):
        parts.append(f"(host {stats['hostSeconds']}s)")
    lines.append("gem5: " + " ".join(parts))

    rates = []
    for label, cache in (("L1I", "icache"), ("L1D", "dcache"), ("L2", None)):
        if cache is None:
            path = "system.l2"
        else:
            path = next(
                (base + cache for base in CPU_STAT_BASES if base + cache + ".overallHits::total" in stats),
                None,
            )
        rate = miss_rate(stats, path) if path else None
        if rate is not None:
            rates.append(f"{label} {rate:.2f}% miss")
    if rates:
        lines.append("gem5: " + " | ".join(rates))
    lines.append(f"gem5: stats in {stats_dir.relative_to(RESULTS_ROOT)}")
    return "\n".join(lines)


def run_under_gem5(elf, stdin_file, out_dir, timeout):
    """用 gem5 全系统模拟跑 ELF（A53 模型，配置在 gem5-config/a53_se.py）。
    程序 stdout 经 --output 落盘、退出码经 --exitcode 落盘（gem5 不走 subprocess
    管道），返回 (proc, 摘要文本)。"""
    stats_dir = out_dir / (elf.stem + ".gem5-stats")
    stats_dir.mkdir(parents=True, exist_ok=True)
    program_stdout = out_dir / (elf.stem + ".gem5.stdout")
    program_exit = stats_dir / "exitcode"
    cmd = [
        str(GEM5_BIN),
        f"--outdir={stats_dir}",
        str(GEM5_CONFIG),
        str(elf),
        f"--output={program_stdout}",
        f"--exitcode={program_exit}",
    ]
    if stdin_file is not None:
        cmd.append(f"--input={stdin_file.name}")
    cmd.extend(os.environ.get("SOYO_GEM5_EXTRA", "").split())
    proc = subprocess.run(
        cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout
    )
    return proc, summarize_gem5(elf, stats_dir)


def write_process_output(proc, stdout_path, stderr_path, returncode_path):
    """把 subprocess 的 stdout/stderr/returncode 分别写盘（.compile.* / .runtime.*）。"""
    stdout_path.write_bytes(proc.stdout or b"")
    stderr_path.write_bytes(proc.stderr or b"")
    returncode_path.write_text(f"{proc.returncode}\n")


def write_timeout_output(err, stdout_path, stderr_path, returncode_path):
    """超时分支：把已产生的部分输出写盘，returncode 文件写 "timeout" 作标记。"""
    stdout_path.write_bytes(err.stdout or b"")
    stderr_path.write_bytes(err.stderr or b"")
    returncode_path.write_text("timeout\n")


def remaining_timeout(start):
    """算 TEST_TIMEOUT 相对用例开始时刻还剩余多少秒；已超时直接抛
    TimeoutExpired（由上层统一按 TLE 处理）。"""
    remaining = TEST_TIMEOUT - (time.perf_counter() - start)
    if remaining <= 0:
        raise subprocess.TimeoutExpired("test case", TEST_TIMEOUT)
    return remaining


def step_timeout(runner, start):
    """gem5 simulations legitimately run for far longer than the qemu limit,
    so disable the per-test timeout when running under gem5.
    中文：gem5 模拟本身极慢（比 qemu 慢几个数量级），不禁超时会必然误报
    TLE，因此 gem5 模式下返回 None（不设超时）。"""
    if runner == "gem5":
        return None
    return remaining_timeout(start)


def copy_testcase_files(src, out_dir):
    """把用例的 .sy/.in/.out 拷贝到 results 镜像目录（保持 tests 下相对路径），
    便于事后复现与归档。.in 是程序 stdin 输入，.out 是期望输出。"""
    src_rel = rel_test(src)
    dst_base = (out_dir / src_rel).with_suffix("")
    dst_base.parent.mkdir(parents=True, exist_ok=True)
    for path in (src, src.with_suffix(".in"), src.with_suffix(".out")):
        if path.exists():
            shutil.copy2(path, dst_base.with_suffix(path.suffix))


def run_test(
    src,
    out_dir,
    opt_level,
    compiler,
    backend,
    target,
    baseline,
    runner,
    loop_unroll,
    pass_stats,
    compile_only,
):
    """单个用例的完整流水线：编译 → 出 IR（可选）→ 链接 → 执行 → 比对。

    参数：
      src          : 用例 .sy 文件绝对路径
      out_dir      : results 根目录（产物按 tests 下相对路径镜像到其下）
      opt_level    : 优化级别（None 表示不传 -O 参数）
      compiler     : 编译器二进制路径（baseline 模式忽略）
      backend      : "asm"（汇编后端）或 "llvm"（LLVM IR 后端）
      target       : "aarch64" 或 "riscv64"
      baseline     : True 时改用 clang 交叉编译该用例作为参考实现（差分对照）
      runner       : "qemu" 或 "gem5"（执行 ELF 的方式）
      loop_unroll  : 循环展开模式覆盖（on/off/dry-run），None 不传
      pass_stats   : 让编译器输出 pass 统计到 .compile.stderr
      compile_only : 只编译不执行（快速语法/编译检查用）

    返回 5 元组 (compile_elapsed, run_elapsed, status, msg, gem5_summary)：
      compile_elapsed : 编译阶段耗时（秒），未进入该阶段为 None
      run_elapsed     : 执行阶段耗时（秒），未进入为 None
      status          : STATUSES 之一（PASS/FAIL/CE/RE/TLE/SKIP）
      msg             : 详情（CE 的编译输出、FAIL 的逐行 diff、TLE 提示等）
      gem5_summary    : gem5 性能摘要文本（qemu 模式为空串）
    """
    start = time.perf_counter()
    src_rel = rel_test(src)
    arch_config = TARGET_CONFIG[target]
    sysylib = ROOT / "sysylib" / arch_config["sysylib"]
    if str(src_rel) in SKIP_TESTS:
        return None, None, "SKIP", "skipped (missing input)", ""
    base = src.with_suffix("")
    copy_testcase_files(src, out_dir)

    # 产物路径全部镜像到 results/<tests 下相对路径>/，后缀区分内容：
    #   .s 汇编 / .raana 中间 IR / .o 目标码 / .elf 可执行文件
    #   .compile.* 编译期产物 / .runtime.* 运行期产物
    ext = {"asm": ".s", "llvm": ".ll"}[backend]
    compile_artifact = out_dir / src_rel.with_suffix(ext)
    ir = out_dir / src_rel.with_suffix(".raana")
    elf = out_dir / src_rel.with_suffix(".elf")
    obj = out_dir / src_rel.with_suffix(".o")
    compile_stdout = out_dir / src_rel.with_suffix(".compile.stdout")
    compile_stderr = out_dir / src_rel.with_suffix(".compile.stderr")
    compile_returncode = out_dir / src_rel.with_suffix(".compile.return")
    runtime_stdout = out_dir / src_rel.with_suffix(".runtime.stdout")
    runtime_stderr = out_dir / src_rel.with_suffix(".runtime.stderr")
    runtime_returncode = out_dir / src_rel.with_suffix(".runtime.return")
    compile_artifact.parent.mkdir(parents=True, exist_ok=True)

    if baseline:
        # --baseline：用容器内 clang 交叉编译该用例作为参考实现（差分对照）。
        # 下面几个 flag 都是为了让 clang 语义与 SysY 规范对齐：
        #   -fcommon       未初始化全局变量进 common 段（SysY 全局变量默认 0）
        #   -ffp-contract=off  禁止 FMA 融合（SysY 要求逐运算 IEEE 舍入）
        #   -fsingle-precision-constant  浮点常量按 float 而非 double 处理
        #   -Wno-incompatible-pointer-types  压制 sylib.h 的指针类型兼容警告
        compile_args = [
            "clang",
            "-x",
            "c",
            "-fcommon",
            "-ffp-contract=off",
            "-fsingle-precision-constant",
            "-Wno-incompatible-pointer-types",
        ]
        if opt_level:
            compile_args.append(f"-O{opt_level}")
        compile_args += [
            f"--target={arch_config['clang_target']}",
            f"--sysroot={arch_config['sysroot']}",
            "-include",
            str(ROOT / "sysylib" / "sylib.h"),
            "-S",
            "-o",
            str(compile_artifact),
            str(src),
        ]
    else:
        # 正常路径：调本项目编译器，参数与 CLI 一一对应
        compile_args = [str(compiler)]
        if opt_level:
            compile_args.append(f"-O{opt_level}")
        compile_args += ["--target", target]
        if loop_unroll:
            compile_args += ["--loop-unroll", loop_unroll]
        if pass_stats:
            compile_args.append("--pass-stats")
        if backend == "asm":
            compile_args += [
                "-S",
                "-o",
                str(compile_artifact),
                str(src),
            ]
        else:
            compile_args += ["--emit", "llvm", "-o", str(compile_artifact), str(src)]

    try:
        compile_proc = subprocess.run(
            compile_args,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=step_timeout(runner, start),
        )
    except subprocess.TimeoutExpired as err:
        # 编译超时 → TLE（可能陷入无限循环的 pass 或巨型展开）
        write_timeout_output(err, compile_stdout, compile_stderr, compile_returncode)
        return (
            time.perf_counter() - start,
            None,
            " TLE",
            f"compile timeout after {TEST_TIMEOUT}s",
            "",
        )
    write_process_output(
        compile_proc, compile_stdout, compile_stderr, compile_returncode
    )
    if compile_proc.returncode:
        # 编译进程非零退出 → CE（编译错误）：stdout+stderr 合并作为详情
        output = (
            (compile_proc.stdout + compile_proc.stderr)
            .decode("utf-8", "replace")
            .strip()
        )
        return (
            time.perf_counter() - start,
            None,
            " CE ",
            f"exit {compile_proc.returncode}\n{output or '(no output)'}",
            "",
        )

    if not baseline:
        # 非 baseline 时额外跑一次 --emit ir，产出 .raana 中间表示快照，
        # 供调试优化 pass 用；失败同样判 CE
        ir_args = [str(compiler)]
        if opt_level:
            ir_args.append(f"-O{opt_level}")
        ir_args += ["--target", target]
        if loop_unroll:
            ir_args += ["--loop-unroll", loop_unroll]
        ir_args += ["--emit", "ir", "-o", str(ir), str(src)]

        try:
            ir_proc = subprocess.run(
                ir_args,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=step_timeout(runner, start),
            )
        except subprocess.TimeoutExpired as err:
            write_timeout_output(err, compile_stdout, compile_stderr, compile_returncode)
            return (
                time.perf_counter() - start,
                None,
                " TLE",
                f"ir timeout after {TEST_TIMEOUT}s",
                "",
            )
        if ir_proc.returncode:
            output = (ir_proc.stdout + ir_proc.stderr).decode("utf-8", "replace").strip()
            return (
                time.perf_counter() - start,
                None,
                " CE ",
                f"ir exit {ir_proc.returncode}\n{output or '(no output)'}",
                "",
            )

    if compile_only:
        return time.perf_counter() - start, None, "PASS", "compile only", ""

    # LLVM backend: lower .ll to .o via llc before linking
    # 中文：后端输出 LLVM IR 时，先用 llc 把 .ll 变成目标机器码 .o，
    # 再走下面的公共链接流程（asm 后端则直接拿 .s 汇编去链接）
    if backend == "llvm":
        try:
            llc_proc = subprocess.run(
                [
                    "llc",
                    "-O2",
                    f"--mtriple={arch_config['llvm_triple']}",
                    "-filetype=obj",
                    str(compile_artifact),
                    "-o",
                    str(obj),
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=step_timeout(runner, start),
            )
        except subprocess.TimeoutExpired as err:
            write_timeout_output(
                err, runtime_stdout, runtime_stderr, runtime_returncode
            )
            return (
                time.perf_counter() - start,
                None,
                " TLE",
                f"llc timeout after {TEST_TIMEOUT}s",
                "",
            )
        if llc_proc.returncode:
            output = (
                (llc_proc.stdout + llc_proc.stderr).decode("utf-8", "replace").strip()
            )
            write_process_output(
                llc_proc, runtime_stdout, runtime_stderr, runtime_returncode
            )
            return (
                time.perf_counter() - start,
                None,
                " CE ",
                f"llc exit {llc_proc.returncode}\n{output or '(no output)'}",
                "",
            )
        link_input = str(obj)
    else:
        link_input = str(compile_artifact)

    # 链接：clang 交叉链接（lld）+ 静态运行时库 sysylib → 静态 ELF
    #   -static           qemu 用户态模拟只能执行静态链接的可执行文件（无动态加载器）
    #   -mcmodel=medany   放宽寻址范围的代码模型（RISC-V 静态链接常用）
    #   --sysroot / --gcc-toolchain=/usr  指向容器内目标架构工具链
    try:
        link_proc = subprocess.run(
            [
                "clang",
                f"--target={arch_config['clang_target']}",
                "--gcc-toolchain=/usr",
                f"--sysroot={arch_config['sysroot']}",
                "-fuse-ld=lld",
                "-mcmodel=medany",
                "-static",
                link_input,
                str(sysylib),
                "-o",
                str(elf),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=step_timeout(runner, start),
        )
    except subprocess.TimeoutExpired as err:
        write_timeout_output(err, runtime_stdout, runtime_stderr, runtime_returncode)
        return (
            time.perf_counter() - start,
            None,
            " TLE",
            f"link timeout after {TEST_TIMEOUT}s",
            "",
        )
    if link_proc.returncode:
        # 链接失败 → RE（归入运行期错误类，实际是构建环节失败）
        write_process_output(
            link_proc, runtime_stdout, runtime_stderr, runtime_returncode
        )
        return (
            time.perf_counter() - start,
            None,
            " RE ",
            f"link exit {link_proc.returncode}\n{link_proc.stderr.decode('utf-8', 'replace').strip() or '(no output)'}",
            "",
        )

    compile_elapsed = time.perf_counter() - start
    stdin = base.with_suffix(".in")
    stdin_file = stdin.open("rb") if stdin.exists() else None
    runtime_start = time.perf_counter()
    gem5_summary = ""
    try:
        try:
            # 执行：qemu-user 跑 ELF（.in 作为 stdin，供 getint 等库函数读入）；
            # gem5 模式走 run_under_gem5（输出/退出码落盘）
            if runner == "gem5":
                run_proc, gem5_summary = run_under_gem5(
                    elf, stdin_file, out_dir, step_timeout(runner, start)
                )
            else:
                run_proc = subprocess.run(
                    [arch_config["qemu"], str(elf)],
                    stdin=stdin_file,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    timeout=step_timeout(runner, start),
                )
        except subprocess.TimeoutExpired as err:
            write_timeout_output(
                err, runtime_stdout, runtime_stderr, runtime_returncode
            )
            return (
                compile_elapsed,
                time.perf_counter() - runtime_start,
                " TLE",
                f"runtime timeout after {TEST_TIMEOUT}s",
                gem5_summary,
            )
    finally:
        if stdin_file is not None:
            stdin_file.close()

    write_process_output(run_proc, runtime_stdout, runtime_stderr, runtime_returncode)
    # gem5 的输出与退出码从落盘文件读回（subprocess 管道拿不到），qemu 模式直接从结果拿
    if runner == "gem5":
        gem5_stdout = out_dir / (elf.stem + ".gem5.stdout")
        stdout_data = gem5_stdout.read_bytes() if gem5_stdout.exists() else b""
        gem5_exit = out_dir / (elf.stem + ".gem5-stats") / "exitcode"
        exit_code = int(gem5_exit.read_text().strip()) if gem5_exit.exists() else -1
        actual = combined_output(stdout_data, exit_code)
    else:
        actual = combined_output(run_proc.stdout, run_proc.returncode)
    # 比对：combined_output（stdout+换行+退出码）与 .out 逐字节对比
    status, msg = compare_output(actual, base.with_suffix(".out"), run_proc.stderr)
    if status == "PASS":
        return (
            compile_elapsed,
            time.perf_counter() - runtime_start,
            status,
            msg,
            gem5_summary,
        )

    # 输出不匹配 + 进程非零退出 + 有 stderr → 判 RE（运行期错误）而非普通 WA：
    # 说明程序在输出错误之外还崩溃/非法退出了
    if run_proc.returncode != 0 and run_proc.stderr.strip():
        output = (run_proc.stdout + run_proc.stderr).decode("utf-8", "replace").strip()
        return (
            compile_elapsed,
            time.perf_counter() - runtime_start,
            " RE ",
            f"exit {run_proc.returncode}\n{output or '(no output)'}",
            gem5_summary,
        )

    return (
        compile_elapsed,
        time.perf_counter() - runtime_start,
        status,
        msg,
        gem5_summary,
    )


def parse_args(argv):
    """命令行解析与参数校验。默认并行度 = CPU 核数一半（容器内与 CI 同款配置）。
    互斥约束：--baseline 只能用 asm 后端（clang 不产我们定义的 IR 形态）。"""
    default_jobs = max(1, (os.cpu_count() or 1) // 2)
    parser = argparse.ArgumentParser(
        description="container test runner for soyo_compiler"
    )
    parser.add_argument(
        "-j",
        "--jobs",
        type=int,
        default=default_jobs,
        help=f"parallel tests (default: half CPU cores, {default_jobs})",
    )
    parser.add_argument(
        "-O", "--opt-level", type=int, default=0, help="compiler optimization level"
    )
    parser.add_argument(
        "--backend",
        choices=["asm", "llvm"],
        default="asm",
        help="compiler backend: asm (assemnly) or llvm (LLVM IR)",
    )
    parser.add_argument(
        "--target",
        choices=["aarch64", "riscv64"],
        default="aarch64",
        help="target architecture (default: aarch64)",
    )
    parser.add_argument(
        "--compiler",
        type=Path,
        default=DEFAULT_COMPILER,
        help=f"compiler path in container (default: {DEFAULT_COMPILER})",
    )
    parser.add_argument(
        "--baseline",
        action="store_true",
        help="compile test cases with the container clang",
    )
    parser.add_argument(
        "--runner",
        choices=["qemu", "gem5"],
        default="qemu",
        help="runtime used to execute ELFs (default: qemu)",
    )
    parser.add_argument(
        "--loop-unroll",
        choices=["on", "off", "dry-run"],
        help="override the compiler loop-unroll mode",
    )
    parser.add_argument(
        "--pass-stats",
        action="store_true",
        help="save structured IR pass statistics in .compile.stderr",
    )
    parser.add_argument(
        "--compile-only",
        action="store_true",
        help="stop after producing the compiler artifact and final IR",
    )
    parser.add_argument(
        "--verbose",
        action="store_true",
        help="show test case output details",
    )
    parser.add_argument(
        "paths",
        nargs="*",
        help="optional .sy files or directories (default: /work/tests)",
    )
    args = parser.parse_args(argv)
    if args.jobs < 1:
        parser.error("--jobs must be at least 1")
    if args.opt_level < 0:
        parser.error("--opt-level must be non-negative")
    if args.baseline and args.backend != "asm":
        parser.error("--baseline only supports the asm backend")
    return args


def check_mounts(compiler, target, baseline, runner):
    """启动前检查容器挂载/路径是否齐全（tests、results、sysylib、编译器、
    gem5 二进制与配置等）；缺失则打印清单并返回 False（run_tests 据此退出）。"""
    arch_config = TARGET_CONFIG[target]
    sysylib = ROOT / "sysylib" / arch_config["sysylib"]
    missing = []
    paths = (TESTS_ROOT, RESULTS_ROOT, sysylib)
    if not baseline:
        paths += (compiler,)
    if runner == "gem5":
        paths += (GEM5_BIN, GEM5_CONFIG)
    for path in paths:
        if not path.exists():
            missing.append(str(path))
    if missing:
        print("Missing required mount/path:")
        for path in missing:
            print(f"  {path}")
        return False
    return True


def print_timing_summary(plural, singular, timings):
    """打印耗时的 Top5 与均值/中位数/P95/最快/最慢统计（编译与运行分开调）。"""
    if not timings:
        return
    print(f"\nTop 5 slowest {plural}:")
    for elapsed, path in sorted(timings, reverse=True)[:5]:
        print(f"{elapsed * 1000:>10.2f}ms {paint(path, 'dim')}")
    elapsed_times = [elapsed for elapsed, _ in timings]
    sorted_elapsed = sorted(elapsed_times)
    p95 = sorted_elapsed[math.ceil(len(sorted_elapsed) * 0.95) - 1]
    print(
        f"\n{singular.capitalize()} summary:"
        f"\n  Average: {statistics.mean(elapsed_times) * 1000:.2f}ms"
        f"\n  Median:  {statistics.median(elapsed_times) * 1000:.2f}ms"
        f"\n  P95:     {p95 * 1000:.2f}ms"
        f"\n  Fastest: {min(elapsed_times) * 1000:.2f}ms"
        f"\n  Slowest: {max(elapsed_times) * 1000:.2f}ms"
    )


def run_tests(args):
    """主流程：检查挂载 → 收集用例 → 线程池并行跑 run_test → 实时状态行 → 汇总。
    返回进程退出码：有任一 FAIL/CE/RE/TLE 时为 1；Ctrl-C 取消剩余任务返回 130。"""
    compiler = args.compiler.resolve()
    if not check_mounts(compiler, args.target, args.baseline, args.runner):
        return 1

    try:
        files = collect_tests(args.paths)
    except ValueError as err:
        print(err)
        return 1
    if not files:
        print("No test files found under /work/tests.")
        return 1

    total = len(files)

    logs_dir = RESULTS_ROOT / "logs"
    logs_dir.mkdir(parents=True, exist_ok=True)
    log_path = logs_dir / (
        datetime.now().isoformat(timespec="seconds").replace(":", "-") + ".log"
    )
    logf = log_path.open("w", encoding="utf-8")
    logf.write(
        f"soyo_compiler test run @ {datetime.now().isoformat(timespec='seconds')}\n"
    )
    logf.write("command: " + " ".join(sys.argv[1:]) + "\n")
    logf.write("options:\n")
    logf.write(
        f"  jobs={args.jobs} opt_level={args.opt_level} backend={args.backend} "
        f"target={args.target} baseline={args.baseline} runner={args.runner}\n"
    )
    logf.write(
        f"  compiler={compiler} loop_unroll={args.loop_unroll} "
        f"pass_stats={args.pass_stats} compile_only={args.compile_only} "
        f"verbose={args.verbose}\n"
    )
    logf.write("cases:\n")
    for f in files:
        logf.write(f"  {rel_test(f)}\n")
    logf.write("=" * 60 + "\n")

    print(
        f"{paint('Running', 'yellow')} tests "
        f"{paint(str(total), 'bold')} cases, {paint(str(args.jobs), 'bold')} jobs"
    )

    counts = {status: 0 for status in STATUSES}
    timings = []
    compile_timings = []
    statusline = ""

    # ── 单行进度条三件套（\x1b[2K 清行 + \r 回行首重写，非 tty 时降级为普通打印）──
    def clear_status():
        """清掉当前进度行。"""
        nonlocal statusline
        if COLOR and statusline:
            print("\x1b[2K\r", end="", flush=True)
            statusline = ""

    def set_status(done, path):
        """重写进度行：显示已完成数/总数 + 当前跑到的用例。"""
        nonlocal statusline
        if not COLOR:
            return
        if path is None:
            clear_status()
            return
        statusline = (
            f"{paint('Running', 'yellow')} "
            f"({done:03}/{total}) {paint(str(path), 'dim')}"
        )
        print(f"\x1b[2K\r{statusline}", end="", flush=True)

    def log(line, keep_status):
        """打印一行结果；keep_status 时打完再恢复进度行（并行环境下不丢进度）。"""
        nonlocal statusline
        if not COLOR:
            print(line)
            return
        old_status = statusline
        if old_status:
            print("\x1b[2K\r", end="", flush=True)
            statusline = ""
        print(line)
        if keep_status and old_status:
            statusline = old_status
            print(f"\x1b[2K\r{old_status}", end="", flush=True)

    # 线程池并行：每个用例一个 future，futures 映射 future → 用例路径（完成时反查）
    pool = ThreadPoolExecutor(max_workers=args.jobs)
    interrupted = False
    futures = {}
    try:
        futures = {
            pool.submit(
                run_test,
                src,
                RESULTS_ROOT,
                args.opt_level,
                compiler,
                args.backend,
                args.target,
                args.baseline,
                args.runner,
                args.loop_unroll,
                args.pass_stats,
                args.compile_only,
            ): src
            for src in files
        }
        set_status(0, rel_test(files[0]))
        for done, future in enumerate(as_completed(futures), 1):
            src = futures[future]
            compile_elapsed, run_elapsed, status, msg, gem5_summary = future.result()
            path = rel_test(src)
            if compile_elapsed is not None:
                compile_timings.append((compile_elapsed, path))
            if run_elapsed is not None:
                timings.append((run_elapsed, path))
            counts[status] += 1

            c_ms = (
                f"{compile_elapsed * 1000:8.2f}ms"
                if compile_elapsed is not None
                else "     n/a"
            )
            r_ms = (
                f"{run_elapsed * 1000:8.2f}ms"
                if run_elapsed is not None
                else "     n/a"
            )
            logf.write(f"[{status.strip():^4}] {path}  compile {c_ms}  run {r_ms}\n")
            if gem5_summary:
                for line in gem5_summary.splitlines():
                    logf.write(f"    {line}\n")
            if msg:
                for line in msg.splitlines():
                    logf.write(f"    {line}\n")
            logf.flush()

            running = next(
                (rel_test(futures[item]) for item in futures if not item.done()), None
            )
            if compile_elapsed is not None:
                compile_time = paint(f"{compile_elapsed * 1000:8.2f}ms", "cyan")
            else:
                compile_time = paint(f"{'n/a':>10}", "dim")
            if run_elapsed is not None:
                run_time = paint(f"{run_elapsed * 1000:8.2f}ms", "yellow")
            else:
                run_time = paint(f"{'n/a':>10}", "dim")
            log(
                f"{paint_status(status)} "
                f"{paint('c:', 'bold')} {compile_time} "
                f"{paint('r:', 'bold')} {run_time} "
                f"{paint(str(path), 'dim')}",
                running is not None,
            )
            if gem5_summary:
                for line in gem5_summary.splitlines():
                    log(f"  {line}", running is not None)
            if status != "PASS" and args.verbose:
                for line in msg.splitlines():
                    log(f"  {line}", running is not None)
            set_status(done, running)
    except KeyboardInterrupt:
        interrupted = True
        for future in futures:
            future.cancel()
        print()
        logf.write("=" * 60 + "\n")
        logf.write("interrupted by user\n")
        logf.close()
        return 130
    finally:
        pool.shutdown(wait=True, cancel_futures=interrupted)

    clear_status()
    passed = counts["PASS"]
    skipped = counts.get("SKIP", 0)
    failed = total - passed - skipped
    skipped = counts.get("SKIP", 0)
    # 汇总：六种状态计数 + 编译/运行耗时统计（异常退出的用例也计入 failed）
    print(
        f"\n{total:>5} Total",
        paint(f"\n{counts['PASS']:>5} passed", "green", "bold"),
        paint(f"\n{counts['FAIL']:>5} failed (wrong answer)", "red"),
        paint(f"\n{counts[' CE ']:>5} CE (compile error)", "dim"),
        paint(f"\n{counts[' RE ']:>5} RE (runtime error)", "magenta"),
        paint(f"\n{counts[' TLE']:>5} TLE (timeout error)", "yellow", "bold"),
        paint(f"\n{skipped:>5} Skipped", "dim"),
    )
    print_timing_summary("compiles", "compile", compile_timings)
    print_timing_summary("runs", "runtime", timings)

    logf.write("=" * 60 + "\n")
    logf.write("summary:\n")
    for status in STATUSES:
        logf.write(f"  {status.strip()}: {counts[status]}\n")
    logf.write(f"  TOTAL: {total}\n")
    logf.write(f"finished: {datetime.now().isoformat(timespec='seconds')}\n")
    logf.close()
    return 0 if failed == 0 else 1


def main():
    """入口：解析参数并跑全部测试，返回进程退出码（0 全过 / 1 有失败）。"""
    return run_tests(parse_args(sys.argv[1:]))


if __name__ == "__main__":
    # SystemExit(main()) 让退出码直接成为进程退出码；顶层再兜一层 Ctrl-C → 130
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print()
        raise SystemExit(130)
