#!/usr/bin/env python3

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import os
from pathlib import Path
import shlex
import subprocess
import sys
import time

ROOT = Path("/work")
TESTS_ROOT = ROOT / "tests"
RESULTS_ROOT = ROOT / "results"
SYSYLIB = ROOT / "sysylib" / "libsysy_arm.a"
DEFAULT_COMPILER = Path(os.environ.get("SOYO_COMPILER", "/work/target/release/soyo_compiler"))

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
STATUSES = ("PASS", "FAIL", " CE ", " RE ")
STATUS_STYLES = {
    "PASS": ("green", "bold"),
    "FAIL": ("red",),
    " CE ": ("dim",),
    " RE ": ("magenta",),
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


def write_process_output(proc, stdout_path, stderr_path):
    stdout_path.write_bytes(proc.stdout or b"")
    stderr_path.write_bytes(proc.stderr or b"")


def run_test(src, out_dir, opt_level, compiler):
    start = time.perf_counter()
    src_rel = rel_test(src)
    base = src.with_suffix("")

    asm = out_dir / src_rel.with_suffix(".s")
    elf = out_dir / src_rel.with_suffix(".elf")
    compile_stdout = out_dir / src_rel.with_suffix(".compile.stdout")
    compile_stderr = out_dir / src_rel.with_suffix(".compile.stderr")
    runtime_stdout = out_dir / src_rel.with_suffix(".runtime.stdout")
    runtime_stderr = out_dir / src_rel.with_suffix(".runtime.stderr")
    asm.parent.mkdir(parents=True, exist_ok=True)

    compile_args = [str(compiler)]
    if opt_level:
        compile_args.append(f"-O{opt_level}")
    compile_args += ["-S", "-o", str(asm), str(src)]

    compile_proc = subprocess.run(compile_args, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    write_process_output(compile_proc, compile_stdout, compile_stderr)
    if compile_proc.returncode:
        output = (compile_proc.stdout + compile_proc.stderr).decode("utf-8", "replace").strip()
        return (
            time.perf_counter() - start,
            " CE ",
            f"exit {compile_proc.returncode}\n{output or '(no output)'}",
        )

    link_proc = subprocess.run(
        [
            "clang",
            "--target=aarch64-linux-gnu",
            "--gcc-toolchain=/usr",
            "--sysroot=/usr/aarch64-linux-gnu",
            "-fuse-ld=lld",
            "-static",
            str(asm),
            str(SYSYLIB),
            "-o",
            str(elf),
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if link_proc.returncode:
        write_process_output(link_proc, runtime_stdout, runtime_stderr)
        return (
            time.perf_counter() - start,
            " RE ",
            f"link exit {link_proc.returncode}\n{link_proc.stderr.decode('utf-8', 'replace').strip() or '(no output)'}",
        )

    stdin = base.with_suffix(".in")
    stdin_file = stdin.open("rb") if stdin.exists() else None
    try:
        run_proc = subprocess.run(
            ["qemu-aarch64-static", str(elf)],
            stdin=stdin_file,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
    finally:
        if stdin_file is not None:
            stdin_file.close()

    write_process_output(run_proc, runtime_stdout, runtime_stderr)
    actual = combined_output(run_proc.stdout, run_proc.returncode)
    status, msg = compare_output(actual, base.with_suffix(".out"), run_proc.stderr)
    if status == "PASS":
        return time.perf_counter() - start, status, msg

    if run_proc.returncode != 0 and run_proc.stderr.strip():
        output = (run_proc.stdout + run_proc.stderr).decode("utf-8", "replace").strip()
        return (
            time.perf_counter() - start,
            " RE ",
            f"exit {run_proc.returncode}\n{output or '(no output)'}",
        )

    return time.perf_counter() - start, status, msg


def parse_args(argv):
    default_jobs = max(1, (os.cpu_count() or 1) // 2)
    parser = argparse.ArgumentParser(description="container test runner for soyo_compiler")
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
        "--compiler",
        type=Path,
        default=DEFAULT_COMPILER,
        help=f"compiler path in container (default: {DEFAULT_COMPILER})",
    )
    parser.add_argument(
        "--verbose",
        action="store_true",
        help="show test case output details",
    )
    parser.add_argument(
        "paths", nargs="*", help="optional .sy files or directories (default: /work/tests)"
    )
    args = parser.parse_args(argv)
    if args.jobs < 1:
        parser.error("--jobs must be at least 1")
    if args.opt_level < 0:
        parser.error("--opt-level must be non-negative")
    return args


def check_mounts(compiler):
    missing = []
    for path in (TESTS_ROOT, RESULTS_ROOT, SYSYLIB, compiler):
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
    if not check_mounts(compiler):
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
        futures = {pool.submit(run_test, src, RESULTS_ROOT, args.opt_level, compiler): src for src in files}
        set_status(0, rel_test(files[0]))
        for done, future in enumerate(as_completed(futures), 1):
            src = futures[future]
            elapsed, status, msg = future.result()
            path = rel_test(src)
            timings.append((elapsed, path))
            counts[status] += 1

            running = next((rel_test(futures[item]) for item in futures if not item.done()), None)
            log(
                f"{paint_status(status)} {elapsed * 1000:.2f}ms {paint(str(path), 'dim')}",
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
    failed = total - passed
    print(
        f"\n{total:>5} Total",
        paint(f"\n{counts['PASS']:>5} passed", "green", "bold"),
        paint(f"\n{counts['FAIL']:>5} failed (wrong answer)", "red"),
        paint(f"\n{counts[' CE ']:>5} CE (compile error)", "dim"),
        paint(f"\n{counts[' RE ']:>5} RE (runtime error)", "magenta"),
    )
    print("\nTop 5 slowest tests:")
    for elapsed, path in sorted(timings, reverse=True)[:5]:
        print(f"{elapsed * 1000:>10.2f}ms {paint(path, 'dim')}")
    return 0 if failed == 0 else 1


def main():
    return run_tests(parse_args(sys.argv[1:]))


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print()
        raise SystemExit(130)
