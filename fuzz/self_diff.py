#!/usr/bin/env python3
"""自差分驱动：生成器 → 容器 -O0/-O2 两档编译运行 → 比对 → 报告。

用法（仓库根目录下）:
    python3 fuzz/self_diff.py --count 200 --seed 1 [--keep] [--tag NAME]

流程:
  1. 调 fuzz/sysy_gen.py 生成 N 个 .sy 到 tests/gen_<TAG>/（容器 tests 挂载点下）
  2. 容器内 test.py -O 0 gen_<TAG>  → 保存 .runtime.* 产物到 /tmp/sd_<TAG>_O0
  3. 容器内 test.py -O 2 gen_<TAG>  → 保存到 /tmp/sd_<TAG>_O2
  4. 逐用例比对 stdout + 退出码（忽略 stderr：perf 类计时输出是物理时间，
     O0/O2 必然不同，属预期假阳性）
  5. 不一致的用例 → fuzz/findings/<TAG>/ 归档 + 汇总报告

判定口径:
  - 两档输出一致        → OK
  - 两档都编译失败(CE)  → 生成器违约（生成程序不合法），计入 gen_bad
  - 一档 CE 一档 PASS    → 编译器 bug（同一输入不同优化级别可编译性不同）
  - 两档都编译成功但输出不同 → 编译器 bug（优化引入语义差异）
"""

import argparse
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FUZZ = ROOT / "fuzz"
TESTS = ROOT / "tests"
RESULTS = ROOT / "results"
FINDINGS = FUZZ / "findings"

IMAGE = "soyo-test-tools"
COMPILER_IN = "/work/target/aarch64-unknown-linux-musl/release/compiler"


def docker_run(opt, gen_tag):
    # -j 2：容器内 test.py 默认半核并发，大用例（max-depth 5 等激进参数）
    # 并发编译内存峰值高，宿主内存紧张时编译器被 OOM kill → 假 CE
    # （Error 137 jetsam 同款）。限 2 并发换取稳定。
    cmd = [
        "docker", "run", "--rm", "-t", "--network", "none",
        "-e", f"SOYO_COMPILER={COMPILER_IN}",
        "-v", f"{ROOT}/target/host-musl:/work/target:ro",
        "-v", f"{ROOT}/tests:/work/tests:ro",
        "-v", f"{ROOT}/sysylib:/work/sysylib:ro",
        "-v", f"{RESULTS}:/work/results:rw",
        IMAGE, "-j", "2", "-O", str(opt), f"gen_{gen_tag}",
    ]
    # test.py 退出码 1 = 有 FAIL/CE/RE/TLE 用例，属正常现象（正是要找的），不抛异常
    subprocess.run(cmd, check=False)


def save_runtime(dst: Path):
    """拷容器产物到 tmp 目录。Docker Desktop（macOS）的 VirtioFS 对
    bind mount 写入偶发同步延迟/丢失（容器内已写完、宿主侧暂不可见），
    等待+重试 3 次；仍不足则该档重跑（由调用方判定）。"""
    dst.mkdir(parents=True, exist_ok=True)
    for _ in range(3):
        files = list(RESULTS.rglob("*.runtime.*"))
        if files:
            break
        print("      (waiting for container output sync ...)")
        time.sleep(3)
    for p in RESULTS.rglob("*.runtime.*"):
        rel = p.relative_to(RESULTS)
        target = dst / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(p, target)


def run_opt(opt: int, gen_tag: str, expected: int, tmp: Path) -> bool:
    """跑一档（容器编译运行 + 拷产物），产物数不足则重跑（Docker Desktop
    bind mount 并发写入偶发丢失）。返回产物是否齐全。"""
    for attempt in range(3):
        if RESULTS.exists():
            shutil.rmtree(RESULTS)
        RESULTS.mkdir(parents=True)
        print(f"[{'2' if opt == 0 else '3'}/5] running -O {opt} in container "
              f"(attempt {attempt + 1}) ...")
        docker_run(opt, gen_tag)
        save_runtime(tmp)
        got = sum(1 for _ in tmp.rglob("*.runtime.stdout"))
        if got >= expected:
            return True
        print(f"      WARN: only {got}/{expected} runtime outputs after "
              f"attempt {attempt + 1}, retrying ...")
        shutil.rmtree(tmp, ignore_errors=True)
    return False


def main():
    ap = argparse.ArgumentParser(description="SysY 自差分驱动")
    ap.add_argument("--count", type=int, default=200)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--tag", default=None)
    ap.add_argument("--keep", action="store_true", help="保留生成用例与 findings")
    ap.add_argument("--max-loop", type=int, default=50)
    ap.add_argument("--max-depth", type=int, default=3, help="sysy_gen 最大嵌套深度")
    ap.add_argument("--max-funcs", type=int, default=3, help="sysy_gen 最大辅助函数数")
    args = ap.parse_args()

    tag = args.tag or f"sd{args.seed}_{int(time.time())}"
    gen_dir = TESTS / f"gen_{tag}"
    tmp_o0 = Path("/tmp") / f"sd_{tag}_O0"
    tmp_o2 = Path("/tmp") / f"sd_{tag}_O2"
    findings_dir = FINDINGS / tag

    for d in (gen_dir, tmp_o0, tmp_o2, findings_dir):
        if d.exists():
            shutil.rmtree(d)
    findings_dir.mkdir(parents=True, exist_ok=True)

    # 1. 生成
    subprocess.run(
        [sys.executable, str(FUZZ / "sysy_gen.py"), "--count", str(args.count),
         "--seed", str(args.seed), "--outdir", str(gen_dir),
         "--max-loop", str(args.max_loop),
         "--max-depth", str(args.max_depth),
         "--max-funcs", str(args.max_funcs)],
        check=True,
    )
    cases = sorted(gen_dir.glob("*.sy"))
    print(f"[1/5] generated {len(cases)} cases -> {gen_dir}")

    # 2/3. -O0 / -O2（带产物完整性重跑）
    ok0 = run_opt(0, tag, len(cases), tmp_o0)
    ok2 = run_opt(2, tag, len(cases), tmp_o2)
    if not (ok0 and ok2):
        print("FATAL: failed to collect full runtime outputs for both "
              "opt levels after retries — rerun later")
        return 1

    # 4. 比对：两档都有 .runtime.* 产物（=都编译成功）的用例，逐字节比 stdout+退出码。
    #    CE 用例不会产 runtime 文件，由下面的产物集合差集统计。
    print(f"[4/5] comparing ...")
    ok, mismatch = [], []
    for sy in cases:
        stem = sy.stem
        so0 = tmp_o0 / f"gen_{tag}" / f"{stem}.runtime.stdout"
        ro0 = tmp_o0 / f"gen_{tag}" / f"{stem}.runtime.return"
        so2 = tmp_o2 / f"gen_{tag}" / f"{stem}.runtime.stdout"
        ro2 = tmp_o2 / f"gen_{tag}" / f"{stem}.runtime.return"
        if not (so0.exists() and so2.exists()):
            continue
        if so0.read_bytes() == so2.read_bytes() and ro0.read_bytes() == ro2.read_bytes():
            ok.append(sy)
        else:
            mismatch.append(sy)

    # CE 统计：两档产物集合的差集（一档 CE 一档编译成功 = 编译器可编译性 bug）
    o0_stems = {p.name[: -len(".runtime.stdout")] for p in tmp_o0.rglob("*.runtime.stdout")}
    o2_stems = {p.name[: -len(".runtime.stdout")] for p in tmp_o2.rglob("*.runtime.stdout")}
    ce_both = [sy for sy in cases if sy.stem not in o0_stems and sy.stem not in o2_stems]
    ce_one = []
    for sy in cases:
        stem = sy.stem
        in0, in2 = stem in o0_stems, stem in o2_stems
        if in0 != in2:
            ce_one.append((sy.name, "O0 CE" if not in0 else "O2 CE"))

    # 5. 报告 + 归档
    print(f"[5/5] report")
    print(f"  total      : {len(cases)}")
    print(f"  OK         : {len(ok)}")
    print(f"  MISMATCH   : {len(mismatch)}   <- 编译器 bug 候选")
    print(f"  CE both    : {len(ce_both)}   <- 生成器违约（程序不合法）")
    print(f"  CE one-side: {len(ce_one)}   <- 编译器 bug 候选（可编译性差异）")

    if mismatch:
        for sy in mismatch:
            shutil.copy2(sy, findings_dir / sy.name)
        print(f"  mismatched cases saved -> {findings_dir}")
        for sy in mismatch[:10]:
            print(f"    {sy.name}")
    if ce_one:
        for sy, side in ce_one[:10]:
            print(f"    CE only {side}: {sy}")

    if not args.keep:
        shutil.rmtree(gen_dir, ignore_errors=True)
        for d in (tmp_o0, tmp_o2):
            shutil.rmtree(d, ignore_errors=True)
        print(f"  cleaned temp dirs (use --keep to retain)")
    else:
        print(f"  kept: {gen_dir}, {findings_dir}, /tmp/sd_{tag}_O0,O2")

    return 1 if (mismatch or ce_one) else 0


if __name__ == "__main__":
    sys.exit(main())
