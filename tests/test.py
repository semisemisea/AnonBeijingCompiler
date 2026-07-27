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


def copy_testcase_files(src, out_dir):
    src_rel = rel_test(src)
    dst_base = (out_dir / src_rel).with_suffix("")
    dst_base.parent.mkdir(parents=True, exist_ok=True)
    for path in (src, src.with_suffix(".in"), src.with_suffix(".out")):
        if path.exists():
            shutil.copy2(path, dst_base.with_suffix(path.suffix))


def run_test(src, out_dir, opt_level, compiler, backend, target, baseline):
    start = time.perf_counter()
    src_rel = rel_test(src)
    arch_config = TARGET_CONFIG[target]
    sysylib = ROOT / "sysylib" / arch_config["sysylib"]
    if str(src_rel) in SKIP_TESTS:
        return None, "SKIP", "skipped (missing input)"
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
            timeout=remaining_timeout(start),
        )
    except subprocess.TimeoutExpired as err:
        write_timeout_output(err, compile_stdout, compile_stderr, compile_returncode)
        return (
            None,
            " TLE",
            f"compile timeout after {TEST_TIMEOUT}s",
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
            None,
            " CE ",
            f"exit {compile_proc.returncode}\n{output or '(no output)'}",
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
                timeout=remaining_timeout(start),
            )
        except subprocess.TimeoutExpired as err:
            write_timeout_output(err, compile_stdout, compile_stderr, compile_returncode)
            return None, " TLE", f"ir timeout after {TEST_TIMEOUT}s"
        if ir_proc.returncode:
            output = (ir_proc.stdout + ir_proc.stderr).decode("utf-8", "replace").strip()
            return (
                None,
                " CE ",
                f"ir exit {ir_proc.returncode}\n{output or '(no output)'}",
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
                timeout=remaining_timeout(start),
            )
        except subprocess.TimeoutExpired as err:
            write_timeout_output(
                err, runtime_stdout, runtime_stderr, runtime_returncode
            )
            return (
                None,
                " TLE",
                f"llc timeout after {TEST_TIMEOUT}s",
            )
        if llc_proc.returncode:
            output = (
                (llc_proc.stdout + llc_proc.stderr).decode("utf-8", "replace").strip()
            )
            write_process_output(
                llc_proc, runtime_stdout, runtime_stderr, runtime_returncode
            )
            return (
                None,
                " CE ",
                f"llc exit {llc_proc.returncode}\n{output or '(no output)'}",
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
            timeout=remaining_timeout(start),
        )
    except subprocess.TimeoutExpired as err:
        write_timeout_output(err, runtime_stdout, runtime_stderr, runtime_returncode)
        return (
            None,
            " TLE",
            f"link timeout after {TEST_TIMEOUT}s",
        )
    if link_proc.returncode:
        write_process_output(
            link_proc, runtime_stdout, runtime_stderr, runtime_returncode
        )
        return (
            None,
            " RE ",
            f"link exit {link_proc.returncode}\n{link_proc.stderr.decode('utf-8', 'replace').strip() or '(no output)'}",
        )

    stdin = base.with_suffix(".in")
    stdin_file = stdin.open("rb") if stdin.exists() else None
    runtime_start = time.perf_counter()
    try:
        try:
            run_proc = subprocess.run(
                [arch_config["qemu"], str(elf)],
                stdin=stdin_file,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=remaining_timeout(start),
            )
        except subprocess.TimeoutExpired as err:
            write_timeout_output(
                err, runtime_stdout, runtime_stderr, runtime_returncode
            )
            return (
                time.perf_counter() - runtime_start,
                " TLE",
                f"runtime timeout after {TEST_TIMEOUT}s",
            )
    finally:
        if stdin_file is not None:
            stdin_file.close()

    write_process_output(run_proc, runtime_stdout, runtime_stderr, runtime_returncode)
    actual = combined_output(run_proc.stdout, run_proc.returncode)
    status, msg = compare_output(actual, base.with_suffix(".out"), run_proc.stderr)
    if status == "PASS":
        return time.perf_counter() - runtime_start, status, msg

    if run_proc.returncode != 0 and run_proc.stderr.strip():
        output = (run_proc.stdout + run_proc.stderr).decode("utf-8", "replace").strip()
        return (
            time.perf_counter() - runtime_start,
            " RE ",
            f"exit {run_proc.returncode}\n{output or '(no output)'}",
        )

    return time.perf_counter() - runtime_start, status, msg


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


def check_mounts(compiler, target, baseline):
    arch_config = TARGET_CONFIG[target]
    sysylib = ROOT / "sysylib" / arch_config["sysylib"]
    missing = []
    paths = (TESTS_ROOT, RESULTS_ROOT, sysylib)
    if not baseline:
        paths += (compiler,)
    for path in paths:
        if not path.exists():
            missing.append(str(path))
    if missing:
        print("Missing required mount/path:")
        for path in missing:
            print(f"  {path}")
        return False
    return True


def run_tests(args):
    compiler = args.compiler.resolve()
    if not check_mounts(compiler, args.target, args.baseline):
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
            ): src
            for src in files
        }
        set_status(0, rel_test(files[0]))
        for done, future in enumerate(as_completed(futures), 1):
            src = futures[future]
            elapsed, status, msg = future.result()
            path = rel_test(src)
            if elapsed is not None:
                timings.append((elapsed, path))
            counts[status] += 1

            running = next(
                (rel_test(futures[item]) for item in futures if not item.done()), None
            )
            log(
                f"{paint_status(status)} "
                f"{f'{elapsed * 1000:.2f}ms' if elapsed is not None else 'runtime n/a'} "
                f"{paint(str(path), 'dim')}",
                running is not None,
            )
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
    if timings:
        print("\nTop 5 slowest tests (runtime only):")
        for elapsed, path in sorted(timings, reverse=True)[:5]:
            print(f"{elapsed * 1000:>10.2f}ms {paint(path, 'dim')}")
        sorted_elapsed_times = sorted(elapsed for elapsed, _ in timings)
        p95 = sorted_elapsed_times[math.ceil(len(sorted_elapsed_times) * 0.95) - 1]
        elapsed_times = [elapsed for elapsed, _ in timings]
        print(
            "\nRuntime summary:"
            f"\n  Average: {statistics.mean(elapsed_times) * 1000:.2f}ms"
            f"\n  Median:  {statistics.median(elapsed_times) * 1000:.2f}ms"
            f"\n  P95:     {p95 * 1000:.2f}ms"
            f"\n  Fastest: {min(elapsed_times) * 1000:.2f}ms"
            f"\n  Slowest: {max(elapsed_times) * 1000:.2f}ms"
        )
    return 0 if failed == 0 else 1


def main():
    return run_tests(parse_args(sys.argv[1:]))


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print()
        raise SystemExit(130)
