#!/usr/bin/env python3

import os
import sys
import tempfile
from pathlib import Path
import subprocess
import time


def get_root() -> Path:
    env_root = os.environ.get("CARGO_MANIFEST_DIR")
    if env_root:
        return (Path(env_root) / ".." / "tests").resolve()
    return (Path(__file__).resolve().parent / "tests").resolve()


def iter_sy_files(root: Path):
    return sorted(root.rglob("*.sy"))


def run_one(file_path: Path, output_dir: Path):
    output_path = output_dir / (file_path.stem + ".s")
    cmd = [
        "cargo",
        "run",
        "-p",
        "soyo_compiler",
        "--release",
        "--quiet",
        "--",
        "-S",
        "-o",
        str(output_path),
        str(file_path),
    ]
    start = time.perf_counter()
    subprocess.run(cmd, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
    end = time.perf_counter()
    return end - start


def main() -> int:
    tests_root = get_root()
    files = iter_sy_files(tests_root)
    total = len(files)

    with tempfile.TemporaryDirectory() as tmpdir:
        out_dir = Path(tmpdir)
        for index, file_path in enumerate(files, start=1):
            elapsed = run_one(file_path, out_dir)
            rel = file_path.relative_to(tests_root)
            print(
                f"[{index:03}/{total} passed] ({elapsed:.5f}s) {rel}",
                file=sys.stderr,
            )

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
