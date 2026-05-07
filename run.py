#!/usr/bin/env python3

import os
import sys
import tempfile
from pathlib import Path
import subprocess
import time


DOCKER_COMPOSE = os.environ.get("DOCKER_COMPOSE", "docker compose")


def get_root() -> Path:
    env_root = os.environ.get("CARGO_MANIFEST_DIR")
    if env_root:
        return (Path(env_root) / ".." / "tests").resolve()
    return (Path(__file__).resolve().parent / "tests").resolve()


def get_project_root() -> Path:
    return Path(__file__).resolve().parent


def iter_tests(root: Path):
    return sorted(root.rglob("*.sy"))


def to_container(host_path: Path, project_root: Path) -> str:
    """Convert absolute host path to /app/... path inside Docker container."""
    return f"/app/{host_path.relative_to(project_root)}"


def run_one(file_path: Path, output_dir: Path, project_root: Path):
    """Compile .sy -> .s in compiler container, then assemble/link/run in runtime container."""
    stem = file_path.stem
    tests_root = file_path.parent
    base = tests_root / stem

    # All intermediate files go into output_dir (temp dir inside project root)
    out_container = to_container(output_dir, project_root)
    asm_container = f"{out_container}/{stem}.s"
    obj_container = f"{out_container}/{stem}.o"
    elf_container = f"{out_container}/{stem}.elf"
    sy_container = to_container(file_path, project_root)

    # --- Step 1: Compile .sy -> .s in compiler container ---
    start = time.perf_counter()
    subprocess.run(
        [*DOCKER_COMPOSE.split(), "run", "--rm", "compiler",
         "cargo", "run", "-p", "soyo_compiler", "--release", "--quiet", "--",
         "-O1", "-S", "-o", asm_container, sy_container],
        check=True, stderr=subprocess.STDOUT, stdout=subprocess.DEVNULL,
    )

    # --- Step 2: Assemble, link, run in runtime container ---
    in_path = base.with_suffix(".in")
    stdin_part = f"< {to_container(in_path, project_root)}" if in_path.exists() else ""

    runtime_cmd = (
        f"as {asm_container} -o {obj_container} && "
        f"gcc -static {obj_container} /app/sysylib/libsysy_arm.a -o {elf_container} && "
        f"{elf_container} {stdin_part}"
    )

    result = subprocess.run(
        [*DOCKER_COMPOSE.split(), "run", "--rm", "runtime",
         "bash", "-c", runtime_cmd],
        capture_output=True,
    )
    end = time.perf_counter()
    elapsed = end - start

    actual_stdout = result.stdout
    actual_exitcode = result.returncode

    # --- Step 3: Compare byte-by-byte ---
    out_path = base.with_suffix(".out")
    if out_path.exists():
        expected = out_path.read_bytes()
        # Accept match on either stdout or exit code (as string + newline)
        if actual_stdout == expected:
            return elapsed, True, ""
        expected_exit = (f"{actual_exitcode}\n").encode()
        if expected_exit == expected:
            return elapsed, True, ""
        # Neither matches — show diff
        diff_lines = []
        actual_lines = actual_stdout.decode("utf-8", errors="replace").splitlines()
        expected_lines = expected.decode("utf-8", errors="replace").splitlines()
        max_show = min(len(actual_lines), len(expected_lines), 8)
        for i in range(max_show):
            a = actual_lines[i] if i < len(actual_lines) else ""
            e = expected_lines[i] if i < len(expected_lines) else ""
            if a != e:
                diff_lines.append(f"  line {i+1}: got {a!r}  want {e!r}")
        if len(actual_lines) != len(expected_lines):
            diff_lines.append(f"  line count: got {len(actual_lines)}  want {len(expected_lines)}")
        diff_lines.append(f"  exit code: {actual_exitcode}")
        msg = "MISMATCH\n" + "\n".join(diff_lines)
        return elapsed, False, msg
    else:
        return elapsed, True, f"(no .out, stdout={actual_stdout!r}, exit={actual_exitcode})"


def main() -> int:
    tests_root = get_root()
    project_root = get_project_root()

    files = iter_tests(tests_root)

    total = len(files)
    if total == 0:
        print("No test files found. Check tests/functional/ path.", file=sys.stderr)
        return 1

    passed = 0
    failed = 0
    skipped = 0

    with tempfile.TemporaryDirectory(prefix=".soyo_test_", dir=project_root) as tmpdir:
        for index, file_path in enumerate(files, start=1):
            rel = file_path.relative_to(tests_root)
            try:
                elapsed, ok, msg = run_one(file_path, Path(tmpdir), project_root)
                if ok:
                    passed += 1
                    status = "passed"
                else:
                    failed += 1
                    status = "FAILED"
                print(
                    f"[{index:03}/{total} {status}] ({elapsed:.5f}s) {rel}",
                    file=sys.stderr,
                )
                if msg:
                    for line in msg.split("\n"):
                        print(f"  {line}", file=sys.stderr)
            except subprocess.CalledProcessError as e:
                skipped += 1
                print(
                    f"[{index:03}/{total} SKIPPED] {rel}",
                    file=sys.stderr,
                )
                print(f"  {e}", file=sys.stderr)

    print(
        f"\nResults: {passed} passed, {failed} failed, {skipped} skipped out of {total}",
        file=sys.stderr,
    )
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
