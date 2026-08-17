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
import re
import shutil
import struct
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


def docker_run(opt, gen_tag, baseline=False):
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
        IMAGE, "-j", "2",
    ]
    if baseline:
        # --baseline：容器内 clang 交叉编译（参考实现，抓绝对错误——
        # 自差分只抓 O0/O2 相对差异，两档同错时漏网）
        cmd.append("--baseline")
    cmd += ["-O", str(opt), f"gen_{gen_tag}"]
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


def run_opt(opt: int, gen_tag: str, expected: int, tmp: Path,
            baseline: bool = False) -> bool:
    """跑一档（容器编译运行 + 拷产物），产物数不足则重跑（Docker Desktop
    bind mount 并发写入偶发丢失）。返回产物是否齐全。"""
    for attempt in range(3):
        if RESULTS.exists():
            shutil.rmtree(RESULTS)
        RESULTS.mkdir(parents=True)
        kind = "clang baseline" if baseline else f"-O {opt}"
        print(f"      running {kind} in container (attempt {attempt + 1}) ...")
        docker_run(opt, gen_tag, baseline=baseline)
        save_runtime(tmp)
        got = sum(1 for _ in tmp.rglob("*.runtime.stdout"))
        if got >= expected:
            return True
        print(f"      WARN: only {got}/{expected} runtime outputs after "
              f"attempt {attempt + 1}, retrying ...")
        shutil.rmtree(tmp, ignore_errors=True)
    return False


FLOAT_RE = re.compile(rb"0x[0-9a-f.]+p[+-]?\d+")


# putfloat 的 %a 输出与紧随的 putint 数字无分隔（如 `0x1.c428f4p+5`+`0`
# → `0x1.c428f4p+50`）。f32 合法指数上限 127 太宽——粘连指数（p+5→p+50）
# 在 127 内不截断。生成器 float 值域：字面量 ±100 × 乘法链（深度 5）≈
# 1e10（指数 ≤ 34），嵌套循环累加不超 2^40 量级——真 float 指数 ≤ 45；
# 粘连必然 ≥ 50。阈值 45 两者安全分离。
MAX_F32_EXP = 45


def split_float_tokens(seq: bytes) -> list:
    """切分 hex float token（putfloat 的 %a 输出）。指数超过生成器值域
    上限（MAX_F32_EXP）时截断：说明末尾数字属于粘连的后续 int，归还给
    普通文本段。截断后残余以 `x` 开头说明还吞了下一个 float 的 `0x`
    前缀（float 紧邻 float，`p+3`+`0x1...` → `p+30x1...`），继续截断。"""
    out = []
    i = 0
    while i < len(seq):
        m = FLOAT_RE.match(seq, i)
        if not m:
            out.append(seq[i:i + 1])
            i += 1
            continue
        tok = m.group()
        while True:
            pm = re.search(rb"p([+-]?\d+)$", tok)
            if pm is None:
                break
            digits = pm.group(1)
            # 粘连的 putint 数字串会撑爆指数位（5546 位实证，Python 3.11
            # int() 默认上限 4300 位）——超 3 位必 > MAX_F32_EXP，直接截，
            # 不做 int() 转换（生成器真 float 指数 ≤ 45，位长 ≤ 3）
            if len(digits.lstrip(b"+-")) > 3:
                tok = tok[:-1]
                continue
            exp = int(digits)
            # rest 从 tok 截断后的结尾算（m.start()+len(tok)），不是原始
            # 匹配结尾——截断后残留的 `x...`（吞了下一个 float 的 0x）
            # 才会被正确识别
            rest = seq[m.start() + len(tok):]
            if abs(exp) <= MAX_F32_EXP and not rest.startswith(b"x"):
                break
            tok = tok[:-1]
        out.append(tok)
        i = m.start() + len(tok)
    return out


def tolerant_compare(a: bytes, b: bytes) -> bool:
    """baseline 比对：float token 容忍值差（相对 ≤ 1e-4），其余逐字节一致。

    clang 编译期折叠 float 表达式（含可常量推导的变量参与）用 double
    精度，而 SysY 规范要求 f32 逐运算舍入（我们正确）。FP 形态谱系：
    - 1 ulp（case_0005：86.264f + -73.39f 逐步 vs double 折叠）
    - ≤ 5 ulp 多步累积（case_0156：`-20.044f+19`）
    - 粘连解析膨胀 ≤ 4 位（putfloat 后紧跟 putint/putfloat）
    - 内联+常量参数折叠 19-38 位差（case_0105：`-90.938 * p0`）
    - **相消放大**（case_0016：`93.9 + -94`，clang f64 折叠 = -0.1，
      我们 f32 逐步 = -0.0999985，位差 205 但相对差 1.5e-5）
    固定 ulp 阈值盖不住相消变体（位差随结果量级反比膨胀），改用相对差：
    |va - vb| ≤ 1e-4 × max(|va|, |vb|, 1e-6) 视为一致。int 输出/结构/
    长度/退出码仍严格比对——真 bug（顺序错、缺输出、逻辑错、int 值错）
    差异远大于此，不会漏（0086/0025/0057/0114/0147 实证仍报）。"""
    parts_a = split_float_tokens(a)
    parts_b = split_float_tokens(b)
    if len(parts_a) != len(parts_b):
        return False
    for sa, sb in zip(parts_a, parts_b):
        if sa == sb:
            continue
        if (re.fullmatch(rb"0x[0-9a-f.]+p[+-]?\d+", sa)
                and re.fullmatch(rb"0x[0-9a-f.]+p[+-]?\d+", sb)):
            na = struct.unpack("I", struct.pack("f", float.fromhex(sa.decode())))[0]
            nb = struct.unpack("I", struct.pack("f", float.fromhex(sb.decode())))[0]
            va = struct.unpack("f", struct.pack("I", na))[0]
            vb = struct.unpack("f", struct.pack("I", nb))[0]
            # 相对差判别（相消放大变体位差可达 205，见 docstring）
            if abs(va - vb) <= 1e-4 * max(abs(va), abs(vb), 1e-6):
                continue
        return False
    return True


def main():
    ap = argparse.ArgumentParser(description="SysY 自差分驱动")
    ap.add_argument("--count", type=int, default=200)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--tag", default=None)
    ap.add_argument("--keep", action="store_true", help="保留生成用例与 findings")
    ap.add_argument("--max-loop", type=int, default=50)
    ap.add_argument("--max-depth", type=int, default=3, help="sysy_gen 最大嵌套深度")
    ap.add_argument("--max-funcs", type=int, default=3, help="sysy_gen 最大辅助函数数")
    ap.add_argument("--baseline", action="store_true",
                    help="clang baseline 差分：我们 -O2 vs clang -O2，"
                         "抓绝对错误（自差分只抓 O0/O2 相对差异，两档同错漏网）")
    args = ap.parse_args()

    tag = args.tag or f"sd{args.seed}_{int(time.time())}"
    gen_dir = TESTS / f"gen_{tag}"
    if args.baseline:
        tmp_ours = Path("/tmp") / f"sd_{tag}_OURS"
        tmp_base = Path("/tmp") / f"sd_{tag}_BASE"
        tmp_dirs = [tmp_ours, tmp_base]
    else:
        tmp_o0 = Path("/tmp") / f"sd_{tag}_O0"
        tmp_o2 = Path("/tmp") / f"sd_{tag}_O2"
        tmp_dirs = [tmp_o0, tmp_o2]
    findings_dir = FINDINGS / tag

    for d in [gen_dir, *tmp_dirs, findings_dir]:
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

    # 2/3. 跑档（带产物完整性重跑）
    if args.baseline:
        print("[2/5] running ours -O 2 ...")
        ok_ours = run_opt(2, tag, len(cases), tmp_ours)
        print("[3/5] running clang baseline -O 2 ...")
        ok_base = run_opt(2, tag, len(cases), tmp_base, baseline=True)
        if not (ok_ours and ok_base):
            print("FATAL: failed to collect full runtime outputs after "
                  "retries — rerun later")
            return 1
        # 4. 比对：我们 vs clang（clang 为权威——抓两档同错的绝对错误）
        print("[4/5] comparing (ours vs clang baseline) ...")
        ok, mismatch, our_ce, gen_bad = [], [], [], []
        for sy in cases:
            stem = sy.stem
            so = tmp_ours / f"gen_{tag}" / f"{stem}.runtime.stdout"
            ro = tmp_ours / f"gen_{tag}" / f"{stem}.runtime.return"
            sb = tmp_base / f"gen_{tag}" / f"{stem}.runtime.stdout"
            rb = tmp_base / f"gen_{tag}" / f"{stem}.runtime.return"
            o_ok, b_ok = so.exists(), sb.exists()
            if o_ok and b_ok:
                if (tolerant_compare(so.read_bytes(), sb.read_bytes())
                        and ro.read_bytes() == rb.read_bytes()):
                    ok.append(sy)
                else:
                    mismatch.append(sy)   # 绝对 bug 候选
            elif not o_ok and b_ok:
                our_ce.append(sy)          # 我们漏编译合法程序 → bug
            else:
                gen_bad.append(sy)         # clang 拒收（或两边都 CE）= 生成器违约
        # 5. 报告
        print("[5/5] report")
        print(f"  total      : {len(cases)}")
        print(f"  OK         : {len(ok)}")
        print(f"  MISMATCH   : {len(mismatch)}   <- 绝对 bug（输出/退出码不同）")
        print(f"  OUR_CE     : {len(our_ce)}   <- 我们漏编译合法程序（bug）")
        print(f"  GEN_BAD    : {len(gen_bad)}   <- 生成器违约（clang 拒收/两边都 CE）")
        if mismatch or our_ce:
            for sy in [*mismatch, *our_ce]:
                shutil.copy2(sy, findings_dir / sy.name)
            print(f"  candidates saved -> {findings_dir}")
            for sy in mismatch[:10]:
                print(f"    MISMATCH: {sy.name}")
            for sy in our_ce[:10]:
                print(f"    OUR_CE  : {sy.name}")
        if gen_bad:
            for sy in gen_bad[:10]:
                print(f"    GEN_BAD : {sy.name}")
        if not args.keep:
            shutil.rmtree(gen_dir, ignore_errors=True)
            for d in tmp_dirs:
                shutil.rmtree(d, ignore_errors=True)
            print("  cleaned temp dirs (use --keep to retain)")
        else:
            print(f"  kept: {gen_dir}, {findings_dir}, /tmp/sd_{tag}_OURS,BASE")
        return 1 if (mismatch or our_ce) else 0

    # 自差分路径：-O0 / -O2 两档对比（抓相对差异）
    print("[2/5] running -O 0 ...")
    ok0 = run_opt(0, tag, len(cases), tmp_o0)
    print("[3/5] running -O 2 ...")
    ok2 = run_opt(2, tag, len(cases), tmp_o2)
    if not (ok0 and ok2):
        print("FATAL: failed to collect full runtime outputs for both "
              "opt levels after retries — rerun later")
        return 1

    # 4. 比对：两档都有 .runtime.* 产物（=都编译成功）的用例，逐字节比 stdout+退出码。
    #    CE 用例不会产 runtime 文件，由下面的产物集合差集统计。
    print("[4/5] comparing ...")
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
        for d in tmp_dirs:
            shutil.rmtree(d, ignore_errors=True)
        print(f"  cleaned temp dirs (use --keep to retain)")
    else:
        print(f"  kept: {gen_dir}, {findings_dir}, /tmp/sd_{tag}_O0,O2")

    return 1 if (mismatch or ce_one) else 0


if __name__ == "__main__":
    sys.exit(main())
