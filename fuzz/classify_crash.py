#!/usr/bin/env python3
"""libFuzzer crash 分类脚本：区分"前端语义噪音"与"真 ICE"。

libFuzzer 的输入是任意字节，mutation 极易产生**非法 SysY**（未声明
函数/变量、类型不匹配等）。前端对语义错误直接 panic（设计使然，
无错误收集机制）——这些 crash 对"找编译器 bug"没有价值，属噪音。
真 bug 是**合法输入**下的 pass/后端 ICE（assert、unwrap、越界等）。

用法:
    python3 fuzz/classify_crash.py [artifact 文件或目录]
    # 默认: fuzz/artifacts/compile/ 下全部 crash

输出:
    NOISE <file> <panic message 首行>    # 前端语义错误，可删
    REAL  <file> <panic message 首行>    # 真 ICE 候选，需诊断

特征库（前端语义错误 panic，按出现频度更新）:
    - "Can't find function"  未声明函数调用
    - "Not a function"       名字是变量/数组却当函数调用
    - "Variable ... not exists"  未声明变量
    - "not exists"           未声明变量（数组/变量访问）
    - "Can't find variable"  变体
"""

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
COMPILER = ROOT / "target" / "debug" / "compiler"
DEFAULT_ARTIFACTS = ROOT / "fuzz" / "artifacts" / "compile"

# 前端语义错误 panic 特征（首行匹配即噪音）
NOISE_PATTERNS = [
    r"Can't find function",
    r"Not a function",
    r"not exists",
    r"Can't find variable",
    r"Variable .* not",
    r"You might forget to call",
]


def classify(artifact: Path) -> tuple[str, str]:
    """宿主 debug compiler 复现，返回 (类别, panic 位置/消息)。"""
    try:
        proc = subprocess.run(
            [str(COMPILER), "-O", "2", "-o", "/dev/null", str(artifact)],
            capture_output=True,
            text=True,
            timeout=60,
        )
    except subprocess.TimeoutExpired:
        return "REAL", "compile timeout (>60s)"
    if proc.returncode == 0:
        return "REAL", "compiles fine (?)"  # 复现不了，可能是 fuzz 构建差异
    err = proc.stderr or ""
    # panic 位置：`thread 'main' panicked at <path>:<line>:<col>:`
    loc = next(
        (l.strip() for l in err.splitlines() if "panicked at" in l),
        "?",
    )
    # panic 消息（panicked 后第一行非空、非 note/箭头行）
    msg = next(
        (l.strip() for l in err.splitlines()
         if l.strip() and "panicked" not in l and "note:" not in l
         and "--> " not in l and "thread '" not in l),
        "?",
    )
    # 前端（soyo_compiler/src/frontend/）的 panic 全是语义错误 ICE：
    # 词法/语法已过、语义层对非法输入直接 panic（无错误收集机制，设计使然）。
    # 真 ICE 在 raana_ir（pass）/ taki_mir / anon_armv8（后端）等。
    if "frontend/" in loc:
        return "NOISE", f"{loc} | {msg}"
    for pat in NOISE_PATTERNS:
        if re.search(pat, msg):
            return "NOISE", msg
    return "REAL", f"{loc} | {msg}"


def main() -> int:
    args = sys.argv[1:]
    target = Path(args[0]) if args else DEFAULT_ARTIFACTS
    files = sorted(target.iterdir()) if target.is_dir() else [target]
    crashes = [f for f in files if f.is_file()]
    if not crashes:
        print("no crash artifacts found")
        return 0
    noise, real = [], []
    for f in crashes:
        kind, msg = classify(f)
        (noise if kind == "NOISE" else real).append((f.name, msg))
        print(f"{kind:5s} {f.name}: {msg[:100]}")
    print(f"\n{len(noise)} noise / {len(real)} real")
    return 0 if not real else 1


if __name__ == "__main__":
    sys.exit(main())
