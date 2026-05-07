#!/usr/bin/env python3

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import os
from pathlib import Path
import shlex
import subprocess
import sys
import termios
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parent
TESTS_ROOT = ROOT / "tests"
COMPOSE = ("docker", "compose")
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
TERM_ATTRS = None
WORKER_STATE = threading.local()
OPEN_WORKERS = []
OPEN_WORKERS_LOCK = threading.Lock()


def paint(text, *styles):
    if not COLOR:
        return text
    return f"{''.join(CODES[s] for s in styles)}{text}{CODES['reset']}"


def rel(path):
    return path.resolve().relative_to(ROOT)


def container(path):
    return f"/app/{rel(path)}"


def paint_status(status):
    return paint(status, *STATUS_STYLES[status])


def mute_input():
    global TERM_ATTRS
    if not sys.stdin.isatty():
        return

    fd = sys.stdin.fileno()
    TERM_ATTRS = termios.tcgetattr(fd)
    muted = TERM_ATTRS[:]
    muted[3] &= ~termios.ECHO
    if hasattr(termios, "ECHOCTL"):
        muted[3] &= ~termios.ECHOCTL
    termios.tcsetattr(fd, termios.TCSANOW, muted)


def restore_input():
    global TERM_ATTRS
    if TERM_ATTRS is not None:
        fd = sys.stdin.fileno()
        termios.tcflush(fd, termios.TCIFLUSH)
        termios.tcsetattr(fd, termios.TCSANOW, TERM_ATTRS)
        TERM_ATTRS = None


def collect_tests(paths):
    if not paths:
        return sorted(TESTS_ROOT.rglob("*.sy"))

    files = []
    for raw in paths:
        path = (ROOT / raw).resolve()
        if path.is_dir():
            files += sorted(path.rglob("*.sy"))
        elif path.suffix == ".sy":
            files.append(path)
        else:
            raise ValueError(f"test path is not a .sy file or directory: {raw}")
    return sorted(files)


def checked(cmd, label, **kwargs):
    if subprocess.run(cmd, cwd=ROOT, **kwargs).returncode == 0:
        return True
    print(f"{label} failed. Aborting.")
    return False


def ensure_containers(rebuild):
    if rebuild and not checked([*COMPOSE, "build"], "Docker image build"):
        return False

    up = subprocess.run(
        [*COMPOSE, "up", "-d", "--no-build", "compiler", "runtime"],
        cwd=ROOT,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    if up.returncode == 0:
        return True

    print("Docker images are missing or stale; building once...")
    if up.stderr:
        print(up.stderr.decode("utf-8", "replace").strip())
    return checked([*COMPOSE, "build"], "Docker image build") and checked(
        [*COMPOSE, "up", "-d", "--no-build", "compiler", "runtime"], "Docker startup"
    )


def build_compiler():
    print("Building soyo_compiler release binary in compiler container...")
    return checked(
        [
            *COMPOSE,
            "exec",
            "-T",
            "compiler",
            "cargo",
            "build",
            "-p",
            "soyo_compiler",
            "--release",
            "--quiet",
        ],
        "Compiler build",
    )


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


def shell_join(args):
    return " ".join(shlex.quote(str(arg)) for arg in args)


class DockerShell:
    def __init__(self, service):
        self.service = service
        self.proc = subprocess.Popen(
            [
                *COMPOSE,
                "exec",
                "-T",
                service,
                "bash",
                "--noprofile",
                "--norc",
            ],
            cwd=ROOT,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1,
        )
        self.seq = 0

    def run(self, script, stdout_path, stderr_path):
        if self.proc.poll() is not None:
            return (
                self.proc.returncode or 1,
                b"",
                f"{self.service} docker shell exited unexpectedly".encode(),
            )

        self.seq += 1
        marker = f"__SOYO_RUN_DONE_{os.getpid()}_{threading.get_ident()}_{self.seq}__"
        stdout_path.parent.mkdir(parents=True, exist_ok=True)
        stderr_path.parent.mkdir(parents=True, exist_ok=True)
        wrapper = (
            f"{{\n{script}\n"
            f"}} > {shlex.quote(container(stdout_path))} "
            f"2> {shlex.quote(container(stderr_path))}\n"
            "__soyo_status=$?\n"
            f"printf '%s %s\\n' {shlex.quote(marker)} \"$__soyo_status\"\n"
        )

        try:
            self.proc.stdin.write(wrapper)
            self.proc.stdin.flush()
        except (BrokenPipeError, AttributeError):
            return (
                self.proc.poll() or 1,
                b"",
                f"{self.service} docker shell is not writable".encode(),
            )

        status = 1
        diagnostics = []
        while True:
            line = self.proc.stdout.readline()
            if line == "":
                return (
                    self.proc.poll() or 1,
                    b"",
                    (
                        f"{self.service} docker shell closed before command completed\n"
                        + "".join(diagnostics)
                    ).encode(),
                )
            if line.startswith(marker + " "):
                try:
                    status = int(line.split()[1])
                except (IndexError, ValueError):
                    status = 1
                break
            diagnostics.append(line)

        stdout = stdout_path.read_bytes() if stdout_path.exists() else b""
        stderr = stderr_path.read_bytes() if stderr_path.exists() else b""
        if diagnostics:
            stderr += "".join(diagnostics).encode()
        return status, stdout, stderr

    def close(self):
        if self.proc.poll() is None:
            try:
                self.proc.stdin.write("exit\n")
                self.proc.stdin.flush()
            except (BrokenPipeError, AttributeError):
                pass
            try:
                self.proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.proc.terminate()
                try:
                    self.proc.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    self.proc.kill()


class DockerTestWorker:
    def __init__(self):
        self.compiler = DockerShell("compiler")
        self.runtime = DockerShell("runtime")

    def close(self):
        self.compiler.close()
        self.runtime.close()


def get_worker():
    worker = getattr(WORKER_STATE, "worker", None)
    if worker is None:
        worker = DockerTestWorker()
        WORKER_STATE.worker = worker
        with OPEN_WORKERS_LOCK:
            OPEN_WORKERS.append(worker)
    return worker


def close_workers():
    with OPEN_WORKERS_LOCK:
        workers = OPEN_WORKERS[:]
        OPEN_WORKERS.clear()
    for worker in workers:
        worker.close()


def run_test(src, out_dir, opt_level):
    start = time.perf_counter()
    base, src_rel = src.with_suffix(""), rel(src)
    asm = out_dir / src_rel.with_suffix(".s")
    obj = out_dir / src_rel.with_suffix(".o")
    elf = out_dir / src_rel.with_suffix(".elf")
    compile_stdout = out_dir / src_rel.with_suffix(".compile.stdout")
    compile_stderr = out_dir / src_rel.with_suffix(".compile.stderr")
    runtime_stdout = out_dir / src_rel.with_suffix(".runtime.stdout")
    runtime_stderr = out_dir / src_rel.with_suffix(".runtime.stderr")
    asm.parent.mkdir(parents=True, exist_ok=True)

    compile_args = [
        "/app/target/release/soyo_compiler",
    ]
    if opt_level:
        compile_args.append(f"-O{opt_level}")
    compile_args += ["-S", "-o", container(asm), container(src)]

    worker = get_worker()
    compile_status, compile_out, compile_err = worker.compiler.run(
        shell_join(compile_args), compile_stdout, compile_stderr
    )
    if compile_status:
        output = (compile_out + compile_err).decode("utf-8", "replace").strip()
        return (
            time.perf_counter() - start,
            " CE ",
            f"exit {compile_status}\n{output or '(no output)'}",
        )

    stdin = base.with_suffix(".in")
    stdin_redirect = f" < {shlex.quote(container(stdin))}" if stdin.exists() else ""
    runtime_cmd = (
        f"as {shlex.quote(container(asm))} -o {shlex.quote(container(obj))} && "
        f"gcc -static {shlex.quote(container(obj))} /app/sysylib/libsysy_arm.a "
        f"-o {shlex.quote(container(elf))} && "
        f"{shlex.quote(container(elf))}{stdin_redirect}"
    )
    runtime_status, run_out, run_err = worker.runtime.run(
        runtime_cmd, runtime_stdout, runtime_stderr
    )
    actual = combined_output(run_out, runtime_status)
    status, msg = compare_output(actual, base.with_suffix(".out"), run_err)
    if status == "PASS":
        return time.perf_counter() - start, status, msg

    if runtime_status != 0 and run_err.strip():
        output = (run_out + run_err).decode("utf-8", "replace").strip()
        return (
            time.perf_counter() - start,
            " RE ",
            f"exit {runtime_status}\n{output or '(no output)'}",
        )

    return time.perf_counter() - start, status, msg


def parse_args(argv):
    default_jobs = max(1, (os.cpu_count() or 1) // 2)
    parser = argparse.ArgumentParser(description="Docker test runner for soyo_compiler")
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
        "--rebuild", action="store_true", help="force docker image rebuild"
    )
    parser.add_argument(
        "--silent",
        action="store_true",
        help="hide test case output details",
    )
    parser.add_argument(
        "paths", nargs="*", help="optional .sy files or directories (default: tests/)"
    )
    args = parser.parse_args(argv)
    if args.jobs < 1:
        parser.error("--jobs must be at least 1")
    if args.opt_level < 0:
        parser.error("--opt-level must be non-negative")
    return args


def run_tests(args, out_dir):
    try:
        files = collect_tests(args.paths)
    except ValueError as err:
        print(err)
        return 1
    if not files:
        print("No test files found under tests/.")
        return 1

    total = len(files)
    print(
        f"{paint('Running', 'yellow')} docker tests "
        f"{paint(str(total), 'bold')} cases, {paint(str(args.jobs), 'bold')} jobs"
    )

    counts = {status: 0 for status in STATUSES}
    timings = []
    statusline = ""

    def set_status(done, path):
        nonlocal statusline
        if path is None:
            clear_status()
            return
        statusline = (
            f"{paint('Running', 'yellow')} "
            f"({done:03}/{total}) {paint(str(path), 'dim')}"
        )
        print(f"\x1b[2K\r{statusline}", end="", flush=True)

    def clear_status():
        nonlocal statusline
        if statusline:
            print("\x1b[2K\r", end="", flush=True)
            statusline = ""

    def log(line, keep_status):
        nonlocal statusline
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
            pool.submit(run_test, src, out_dir, args.opt_level): src for src in files
        }
        set_status(0, rel(files[0]))
        for done, future in enumerate(as_completed(futures), 1):
            src = futures[future]
            elapsed, status, msg = future.result()
            path = rel(src)
            timings.append((elapsed, path))
            counts[status] += 1

            running = next(
                (rel(futures[item]) for item in futures if not item.done()), None
            )
            log(
                f"{paint_status(status)} {elapsed * 1000:.2f}ms {paint(str(path), 'dim')}",
                running is not None,
            )
            if status != "PASS" and not args.silent:
                for line in msg.splitlines():
                    log(f"  {line}", running is not None)
            set_status(done, running)
    except KeyboardInterrupt:
        interrupted = True
        for future in futures:
            future.cancel()
        restore_input()
        print()
        return 130
    finally:
        close_workers()
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
    args = parse_args(sys.argv[1:])
    mute_input()
    try:
        if not ensure_containers(args.rebuild) or not build_compiler():
            return 1
        with tempfile.TemporaryDirectory(prefix=".soyo_test_", dir=ROOT) as tmp:
            return run_tests(args, Path(tmp))
    finally:
        restore_input()


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        restore_input()
        print()
        raise SystemExit(130)
