#!/usr/bin/env python3

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import tempfile
from pathlib import Path
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parent
TESTS_ROOT = ROOT / "tests"
COMPILER = ROOT / "target" / "release" / "soyo_compiler"


def build_compiler() -> bool:
    cmd = [
        "cargo",
        "build",
        "-p",
        "soyo_compiler",
        "--release",
        "--quiet",
    ]
    print("Building soyo_compiler release binary...", file=sys.stderr)
    result = subprocess.run(cmd, cwd=ROOT)
    if result.returncode != 0:
        print("Build failed. Aborting.", file=sys.stderr)
        return False
    return True


def run_one(compiler: Path, file_path: Path, tests_root: Path, output_dir: Path):
    rel = file_path.relative_to(tests_root)
    output_path = output_dir / rel.with_suffix(".ir")
    output_path.parent.mkdir(parents=True, exist_ok=True)
    cmd = [
        str(compiler),
        "-o",
        str(output_path),
        str(file_path),
    ]
    start = time.perf_counter()
    subprocess.run(cmd, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
    end = time.perf_counter()
    return end - start


def parse_args(argv):
    parser = argparse.ArgumentParser(description="Test runner for soyo_compiler")
    parser.add_argument(
        "-j",
        "--jobs",
        type=int,
        default=1,
        help="number of parallel jobs (default: 1)",
    )
    args = parser.parse_args(argv)
    if args.jobs < 1:
        parser.error("--jobs must be at least 1")
    return args


def main() -> int:
    args = parse_args(sys.argv[1:])
    files = sorted(TESTS_ROOT.rglob("*.sy"))
    total = len(files)
    timings = []

    if not build_compiler():
        return 1

    with tempfile.TemporaryDirectory() as tmpdir:
        out_dir = Path(tmpdir)
        print(
            f"Running {total} tests with {args.jobs} parallel jobs...",
            file=sys.stderr,
        )
        with ThreadPoolExecutor(max_workers=args.jobs) as executor:
            futures = {
                executor.submit(run_one, COMPILER, file_path, TESTS_ROOT, out_dir): file_path
                for file_path in files
            }
            for done, future in enumerate(as_completed(futures), start=1):
                file_path = futures[future]
                rel = file_path.relative_to(ROOT)
                prefix = f"[{done:03}/{total}]"
                try:
                    elapsed = future.result()
                except subprocess.CalledProcessError as err:
                    print(
                        f"\x1b[2K\r{prefix} failed {rel} (exit {err.returncode})",
                        file=sys.stderr,
                    )
                    return 1

                timings.append((elapsed, rel))
                elapsed_ms = elapsed * 1000
                print(
                    f"\x1b[2K\r{prefix} {elapsed_ms:.3f}ms {rel}",
                    file=sys.stderr,
                )

    print("\n  Top 5 slowest tests", file=sys.stderr)
    print("-" * 40, file=sys.stderr)
    for _, (elapsed, rel) in enumerate(sorted(timings, reverse=True)[:5], start=1):
        elapsed_ms = elapsed * 1000
        print(f"{elapsed_ms:>8.3f}ms - {rel}", file=sys.stderr)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
