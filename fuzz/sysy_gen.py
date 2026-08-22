#!/usr/bin/env python3
"""SysY 结构化程序生成器 —— 自差分 fuzz 的输入源。

用法: python3 sysy_gen.py --count 100 --seed 42 --outdir /path/to/dir
生成 N 个行为确定的 SysY 程序（.sy），供 -O0/-O2 自差分比对。

为什么"行为确定"很重要：
  自差分的判定是"两档优化输出必须一致"。如果程序本身含未定义/未指定
  行为（除零、越界、int 溢出、未定义求值顺序、读未初始化变量），
  差异可能是程序自己的问题（假阳性），而不是编译器 bug。
  因此生成器只产出语义安全子集内的程序：

方言约束（SysY 课程方言）：
  - 无 for / ?: / 位运算（& | ^ ~ << >>），逻辑仅 && ||，循环仅 while
  - 类型：int / float / 一维数组（int[n]、float[n]）
  - 库函数：putint / putfloat / putch（不用 getint，程序自包含、无输入）

语义安全约束：
  - int 字面量 [-50,50]；乘法操作数 [-20,20]；累计和 |g_sum| < 2^26（防溢出）
  - 无除法/取模（回避除零）
  - 数组索引 = 字面量（0..size-1）或循环变量（循环上界 ≤ 数组最小 size）→ 无越界
  - 函数调用只出现在语句级；表达式纯（无副作用）→ 无未定义求值顺序
  - 变量声明总是初始化 → 无未初始化读取
  - while 只生成计数循环（上界为字面量/参数，循环变量单调递增）→ 必然终止

程序骨架：
  int g_sum = 0;            // 全局累计（checksum）
  [int g_arr[N];]           // 可选全局数组（默认 0）
  helper*(...){...}         // 0~3 个辅助函数（int/float 标量 + int[] 数组参数）
  int main(){ ...; putint(g_sum); return g_sum; }
  main 末尾输出累计值并 return 它 —— 即使程序不打印任何中间值，
  退出码（= checksum）也是强差分信号。
"""

import argparse
import os
import random


class Env:
    """生成作用域：当前可见的变量/数组/函数/循环信息。"""

    def __init__(self, funcs=None):
        self.int_vars = []          # int 局部/参数变量名
        self.float_vars = []        # float 局部/参数变量名
        self.arrays = []            # (name, ty, size) 一维数组
        self.loops = []             # (loop_var_name, bound) 当前计数循环栈
        self.funcs = funcs or []    # [(name, [(param_ty, param_name), ...])]
        self.acc_name = "g_sum"     # 全局累计变量名

    def min_array_size(self):
        return min((s for _, _, s in self.arrays), default=None)


class Gen:
    def __init__(self, rng, max_funcs=3, max_depth=3, max_arr=16, max_loop=50):
        self.rng = rng
        self.max_funcs = max_funcs
        self.max_depth = max_depth
        self.max_arr = max_arr
        self.max_loop = max_loop
        self.lines = []
        self.tab = "    "
        # 全局唯一命名计数器：编译器不支持同名重声明（shadow）会 ICE，
        # 因此所有变量/循环变量/数组/函数名全局递增，绝不重名
        self.name_seq = 0

    def fresh(self, prefix):
        self.name_seq += 1
        return f"{prefix}{self.name_seq}"

    # ─────────────── 工具 ───────────────
    def pick(self, seq):
        return self.rng.choice(seq)

    def int_lit(self, lo=-50, hi=50):
        return self.rng.randint(lo, hi)

    def float_lit(self):
        return round(self.rng.uniform(-100.0, 100.0), self.rng.randint(1, 3))

    # ─────────────── 表达式（纯、无副作用） ───────────────
    def gen_int_atom(self, env):
        r = self.rng.random()
        if r < 0.35:
            return str(self.int_lit())
        if r < 0.6 and env.int_vars:
            return self.pick(env.int_vars)
        if r < 0.85:
            # 只选 int 数组（float 数组元素是 float 表达式，不能进 int 上下文）
            int_arrays = [(n, t, s) for n, t, s in env.arrays if t == "int"]
            if int_arrays:
                name, ty, size = self.pick(int_arrays)
                if env.loops and self.rng.random() < 0.5:
                    # 循环变量索引：上界已保证 ≤ 数组最小 size，无越界
                    return f"{name}[{env.loops[-1][0]}]"
                return f"{name}[{self.rng.randint(0, size - 1)}]"
        if r < 0.95 and env.loops:
            return env.loops[-1][0]
        return str(self.int_lit())

    def gen_int_small(self, env):
        """乘法操作数：收紧值域防溢出。"""
        if self.rng.random() < 0.7:
            return str(self.rng.randint(-20, 20))
        return self.gen_int_atom(env)

    def gen_int_expr(self, depth, env):
        if depth <= 0:
            return self.gen_int_atom(env)
        r = self.rng.random()
        if r < 0.3:
            return self.gen_int_atom(env)
        if r < 0.55:
            return f"({self.gen_int_atom(env)} + {self.gen_int_atom(env)})"
        if r < 0.75:
            return f"({self.gen_int_atom(env)} - {self.gen_int_atom(env)})"
        if r < 0.9:
            return f"({self.gen_int_atom(env)} * {self.gen_int_small(env)})"
        return f"({self.gen_int_expr(depth - 1, env)} + {self.gen_int_atom(env)})"

    def gen_float_atom(self, env):
        r = self.rng.random()
        if r < 0.45:
            return repr(self.float_lit())
        if r < 0.7 and env.float_vars:
            return self.pick(env.float_vars)
        if r < 0.85 and env.int_vars:
            return self.pick(env.int_vars)  # int → float 隐式转换
        if r < 0.95 and env.arrays:
            name, ty, size = self.pick(env.arrays)
            if ty == "float":
                if env.loops and self.rng.random() < 0.5:
                    return f"{name}[{env.loops[-1][0]}]"
                return f"{name}[{self.rng.randint(0, size - 1)}]"
        return repr(self.float_lit())

    def gen_float_expr(self, depth, env):
        if depth <= 0:
            return self.gen_float_atom(env)
        r = self.rng.random()
        if r < 0.35:
            return self.gen_float_atom(env)
        if r < 0.6:
            return self.gen_float_bin(env, "+")
        if r < 0.8:
            return self.gen_float_bin(env, "*")
        return self.gen_float_bin(env, "+", a=self.gen_float_expr(depth - 1, env))

    def gen_float_bin(self, env, op, a=None, b=None):
        """float 二元运算：至少一个操作数是变量/数组（非纯字面量）。
        纯字面量运算（如 `86.264 + -73.39`）会被两边编译期折叠成不同
        精度——我们按 IEEE f32 逐步舍入（规范正确），clang 折叠用
        double——1 ulp 差是 baseline 假阳性，不是编译器 bug。
        无变量可参与时退化为单值（不生成纯字面量二元运算）。"""
        a = a if a is not None else self.gen_float_atom(env)
        b = b if b is not None else self.gen_float_atom(env)
        if self.is_pure_literal(a) and self.is_pure_literal(b):
            vars_ = env.float_vars + env.int_vars
            if vars_:
                b = self.pick(vars_)
            else:
                return a
        return f"({a} {op} {b})"

    @staticmethod
    def is_pure_literal(expr):
        """表达式是否纯字面量（无变量/数组/调用）：只含数字/小数点/符号。"""
        return not any(c.isalpha() for c in expr)

    def gen_cmp(self, env):
        # int 比较用全部操作符；float 只用 < <= > >=（避免 ==/!= 的舍入歧义）
        if self.rng.random() < 0.6:
            op = self.pick(["<", "<=", ">", ">=", "==", "!="])
            a = self.gen_int_expr(1, env)
            b = self.gen_int_expr(1, env)
            return f"({a} {op} {b})"
        op = self.pick(["<", "<=", ">", ">="])
        a = self.gen_float_expr(1, env)
        b = self.gen_float_expr(1, env)
        return f"({a} {op} {b})"

    def gen_cond(self, depth, env):
        if depth <= 0:
            return self.gen_cmp(env)
        r = self.rng.random()
        if r < 0.45:
            return self.gen_cmp(env)
        if r < 0.75:
            return f"({self.gen_cmp(env)} && {self.gen_cmp(env)})"
        return f"({self.gen_cmp(env)} || {self.gen_cmp(env)})"

    # ─────────────── 语句 ───────────────
    def gen_block(self, depth, env, ind):
        # 作用域快照：块内新增的变量/数组在块结束时回收，防止跨作用域误用
        snap = (len(env.int_vars), len(env.float_vars), len(env.arrays))
        n = self.rng.randint(1, 3)
        for _ in range(n):
            self.gen_stmt(depth, env, ind)
        del env.int_vars[snap[0]:]
        del env.float_vars[snap[1]:]
        del env.arrays[snap[2]:]

    def gen_stmt(self, depth, env, ind):
        r = self.rng.random()
        if r < 0.18 and depth < self.max_depth:
            # if-else
            cond = self.gen_cond(1, env)
            self.lines.append(f"{ind}if ({cond}) {{")
            self.gen_block(depth + 1, env, ind + self.tab)
            if self.rng.random() < 0.5:
                self.lines.append(f"{ind}}} else {{")
                self.gen_block(depth + 1, env, ind + self.tab)
            self.lines.append(f"{ind}}}")
        elif r < 0.35 and depth < self.max_depth:
            # 计数 while：上界 ≤ 数组最小 size（保证 a[i] 不越界），否则取字面量
            ms = env.min_array_size()
            hi = min(self.max_loop, ms) if ms is not None else self.max_loop
            n = self.rng.randint(1, max(1, hi))
            name = self.fresh("i")
            self.lines.append(f"{ind}int {name} = 0;")
            self.lines.append(f"{ind}while ({name} < {n}) {{")
            env.loops.append((name, n))
            self.gen_block(depth + 1, env, ind + self.tab)
            env.loops.pop()
            self.lines.append(f"{ind}{name} = {name} + 1;")
            self.lines.append(f"{ind}}}")
        elif r < 0.48:
            # 数组元素写入
            if env.arrays:
                name, ty, size = self.pick(env.arrays)
                if env.loops and self.rng.random() < 0.5:
                    idx = env.loops[-1][0]
                else:
                    idx = self.rng.randint(0, size - 1)
                if ty == "int":
                    val = self.gen_int_expr(1, env)
                else:
                    val = self.gen_float_expr(1, env)
                self.lines.append(f"{ind}{name}[{idx}] = {val};")
        elif r < 0.62:
            # 变量赋值
            if env.int_vars and self.rng.random() < 0.6:
                v = self.pick(env.int_vars)
                self.lines.append(f"{ind}{v} = {self.gen_int_expr(1, env)};")
            elif env.float_vars:
                v = self.pick(env.float_vars)
                self.lines.append(f"{ind}{v} = {self.gen_float_expr(1, env)};")
        elif r < 0.72 and env.funcs:
            # 辅助函数调用（语句级，无副作用顺序问题）
            fn = self.pick(env.funcs)
            args = []
            for pty, pname in fn["params"]:
                if pty == "int[]":
                    arr = self.pick(env.arrays)
                    args.append(arr[0])
                elif pty == "int":
                    args.append(self.gen_int_expr(1, env))
                else:
                    args.append(self.gen_float_expr(1, env))
            self.lines.append(f"{ind}{fn['name']}({', '.join(args)});")
        elif r < 0.82:
            # 输出
            if self.rng.random() < 0.6:
                self.lines.append(f"{ind}putint({self.gen_int_expr(1, env)});")
            else:
                self.lines.append(f"{ind}putfloat({self.gen_float_expr(1, env)});")
        elif r < 0.9:
            # 累计到 checksum（差分信号）
            self.lines.append(
                f"{ind}{env.acc_name} = {env.acc_name} + {self.gen_int_expr(1, env)};"
            )
        else:
            # 局部变量声明 + 初始化（先生成表达式再加入 env，避免自引用）
            if self.rng.random() < 0.5:
                name = self.fresh("v")
                init = self.gen_int_expr(1, env)
                env.int_vars.append(name)
                self.lines.append(f"{ind}int {name} = {init};")
            else:
                name = self.fresh("f")
                init = self.gen_float_expr(1, env)
                env.float_vars.append(name)
                self.lines.append(f"{ind}float {name} = {init};")

    # ─────────────── 函数（第一版：只收 int/float 标量参数） ───────────────
    def gen_function(self, env):
        ret_ty = self.pick(["int", "float"])
        params = []
        n = self.rng.randint(0, 2)
        for _ in range(n):
            params.append((self.pick(["int", "float"]), f"p{len(params)}"))
        name = self.fresh("helper")

        # helper 内不再调用其他函数（避免声明顺序问题），funcs 置空
        fenv = Env(funcs=[])
        plist = []
        for pty, pname in params:
            if pty == "int":
                fenv.int_vars.append(pname)
                plist.append(f"int {pname}")
            else:
                fenv.float_vars.append(pname)
                plist.append(f"float {pname}")
        self.lines.append(f"{ret_ty} {name}({', '.join(plist)}) {{")
        self.gen_block(1, fenv, self.tab)
        if ret_ty == "int":
            self.lines.append(f"{self.tab}return {self.gen_int_expr(1, fenv)};")
        else:
            self.lines.append(f"{self.tab}return {self.gen_float_expr(1, fenv)};")
        self.lines.append("}")
        return {"name": name, "params": params, "ret_ty": ret_ty}

    # ─────────────── 程序 ───────────────
    def gen_program(self):
        self.lines = []
        self.lines.append("int g_sum = 0;")

        env = Env(funcs=[])
        if self.rng.random() < 0.4:
            gsize = self.rng.randint(1, 8)
            self.lines.append(f"int g_arr[{gsize}];")
            env.arrays.append(("g_arr", "int", gsize))

        nf = self.rng.randint(0, self.max_funcs)
        for _ in range(nf):
            fn = self.gen_function(env)
            env.funcs.append(fn)

        # main
        self.lines.append("int main() {")
        m = "    "
        n_locals = self.rng.randint(1, 4)
        for _ in range(n_locals):
            r = self.rng.random()
            if r < 0.5:
                name = self.fresh("v")
                init = self.gen_int_expr(1, env)
                env.int_vars.append(name)
                self.lines.append(f"{m}int {name} = {init};")
            elif r < 0.8:
                name = self.fresh("f")
                init = self.gen_float_expr(1, env)
                env.float_vars.append(name)
                self.lines.append(f"{m}float {name} = {init};")
            else:
                aname = self.fresh("a")
                asize = self.rng.randint(1, self.max_arr)
                aty = self.pick(["int", "float"])
                env.arrays.append((aname, aty, asize))
                # 必须显式初始化：未初始化局部数组元素是 UB（垃圾值），
                # 两档内存布局不同会导致输出不同（假阳性）
                self.lines.append(f"{m}{aty} {aname}[{asize}] = {{0}};")

        n_stmts = self.rng.randint(3, 8)
        for _ in range(n_stmts):
            self.gen_stmt(0, env, m)

        self.lines.append(f"{m}putint({env.acc_name});")
        self.lines.append(f"{m}putch(10);")
        self.lines.append(f"{m}return {env.acc_name};")
        self.lines.append("}")
        return "\n".join(self.lines) + "\n"


def main():
    ap = argparse.ArgumentParser(description="SysY 结构化程序生成器")
    ap.add_argument("--count", type=int, default=100)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--outdir", default=".")
    ap.add_argument("--max-funcs", type=int, default=3)
    ap.add_argument("--max-depth", type=int, default=3)
    ap.add_argument("--max-arr", type=int, default=16)
    ap.add_argument("--max-loop", type=int, default=50)
    args = ap.parse_args()

    os.makedirs(args.outdir, exist_ok=True)
    rng = random.Random(args.seed)
    gen = Gen(rng, args.max_funcs, args.max_depth, args.max_arr, args.max_loop)
    for i in range(args.count):
        src = gen.gen_program()
        with open(os.path.join(args.outdir, f"case_{i:04d}.sy"), "w") as f:
            f.write(src)
    print(f"generated {args.count} programs -> {args.outdir} (seed={args.seed})")


if __name__ == "__main__":
    main()
