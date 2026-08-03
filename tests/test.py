#!/usr/bin/env python3

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

ROOT = Path("/work")
TESTS_ROOT = ROOT / "tests"
RESULTS_ROOT = ROOT / "results"

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
DEFAULT_COMPILER = Path(
    os.environ.get("SOYO_COMPILER", "/work/target/release/compiler")
)

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
TEST_TIMEOUT = 600

STATUSES = ("PASS", "FAIL", " CE ", " RE ", " TLE", "SKIP")

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
    if not COLOR:
        return text
    return f"{''.join(CODES[s] for s in styles)}{text}{CODES['reset']}"


def paint_status(status):
    return paint(status, *STATUS_STYLES[status])


def rel_test(path):
    return path.resolve().relative_to(TESTS_ROOT)


def resolve_test_path(raw):
    path = Path(raw)
    if path.is_absolute():
        return path.resolve()
    parts = path.parts
    if parts and parts[0] == "tests":
        return (TESTS_ROOT / Path(*parts[1:])).resolve()
    return (TESTS_ROOT / path).resolve()


def collect_tests(paths):
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
    if stdout and not stdout.endswith(b"\n"):
        stdout += b"\n"
    return stdout + f"{returncode}\n".encode()


GEM5_BIN = Path(os.environ.get("SOYO_GEM5", "/work/gem5/build/ARM/gem5.opt"))
GEM5_CONFIG = Path(
    os.environ.get("SOYO_GEM5_CONFIG", "/work/gem5-config/a53_se.py")
)


def read_stats_table(path):
    if not path.exists():
        return None
    stats = {}
    for line in path.read_text().splitlines():
        key, sep, rest = line.partition(" ")
        if sep and rest.strip():
            stats[key] = rest.split()[0]
    return stats


def fmt_count(value):
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
    hits = stats.get(path + ".overallHits::total")
    misses = stats.get(path + ".overallMisses::total")
    if hits is None or misses is None:
        return None
    total = int(hits) + int(misses)
    if total == 0:
        return 0.0
    return 100.0 * int(misses) / total


# With a single CPU the stat group is system.cpu.* rather than system.cpu0.*,
# so try both prefixes.
CPU_STAT_BASES = ("system.cpu0.", "system.cpu.")


def summarize_gem5(elf, stats_dir):
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
    stdout_path.write_bytes(proc.stdout or b"")
    stderr_path.write_bytes(proc.stderr or b"")
    returncode_path.write_text(f"{proc.returncode}\n")


def write_timeout_output(err, stdout_path, stderr_path, returncode_path):
    stdout_path.write_bytes(err.stdout or b"")
    stderr_path.write_bytes(err.stderr or b"")
    returncode_path.write_text("timeout\n")


def remaining_timeout(start):
    remaining = TEST_TIMEOUT - (time.perf_counter() - start)
    if remaining <= 0:
        raise subprocess.TimeoutExpired("test case", TEST_TIMEOUT)
    return remaining


def step_timeout(runner, start):
    """gem5 simulations legitimately run for far longer than the qemu limit,
    so disable the per-test timeout when running under gem5."""
    if runner == "gem5":
        return None
    return remaining_timeout(start)


def copy_testcase_files(src, out_dir):
    src_rel = rel_test(src)
    dst_base = (out_dir / src_rel).with_suffix("")
    dst_base.parent.mkdir(parents=True, exist_ok=True)
    for path in (src, src.with_suffix(".in"), src.with_suffix(".out")):
        if path.exists():
            shutil.copy2(path, dst_base.with_suffix(path.suffix))


def run_test(src, out_dir, opt_level, compiler, backend, target, baseline, runner):
    start = time.perf_counter()
    src_rel = rel_test(src)
    arch_config = TARGET_CONFIG[target]
    sysylib = ROOT / "sysylib" / arch_config["sysylib"]
    if str(src_rel) in SKIP_TESTS:
        return None, None, "SKIP", "skipped (missing input)", ""
    base = src.with_suffix("")
    copy_testcase_files(src, out_dir)

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
        compile_args = [str(compiler)]
        if opt_level:
            compile_args.append(f"-O{opt_level}")
        if backend == "asm":
            compile_args += [
                "-S",
                "--target",
                target,
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
        ir_args = [str(compiler)]
        if opt_level:
            ir_args.append(f"-O{opt_level}")
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

    # LLVM backend: lower .ll to .o via llc before linking
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
    if runner == "gem5":
        gem5_stdout = out_dir / (elf.stem + ".gem5.stdout")
        stdout_data = gem5_stdout.read_bytes() if gem5_stdout.exists() else b""
        gem5_exit = out_dir / (elf.stem + ".gem5-stats") / "exitcode"
        exit_code = int(gem5_exit.read_text().strip()) if gem5_exit.exists() else -1
        actual = combined_output(stdout_data, exit_code)
    else:
        actual = combined_output(run_proc.stdout, run_proc.returncode)
    status, msg = compare_output(actual, base.with_suffix(".out"), run_proc.stderr)
    if status == "PASS":
        return (
            compile_elapsed,
            time.perf_counter() - runtime_start,
            status,
            msg,
            gem5_summary,
        )

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
    print(
        f"{paint('Running', 'yellow')} tests "
        f"{paint(str(total), 'bold')} cases, {paint(str(args.jobs), 'bold')} jobs"
    )

    counts = {status: 0 for status in STATUSES}
    timings = []
    compile_timings = []
    statusline = ""

    def clear_status():
        nonlocal statusline
        if COLOR and statusline:
            print("\x1b[2K\r", end="", flush=True)
            statusline = ""

    def set_status(done, path):
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
        return 130
    finally:
        pool.shutdown(wait=True, cancel_futures=interrupted)

    clear_status()
    passed = counts["PASS"]
    skipped = counts.get("SKIP", 0)
    failed = total - passed - skipped
    skipped = counts.get("SKIP", 0)
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
    return 0 if failed == 0 else 1


def main():
    return run_tests(parse_args(sys.argv[1:]))


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print()
        raise SystemExit(130)
