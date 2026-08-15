# lalrpop 指南：语法文件怎么读、怎么写

> 离线工作手册 G7。lalrpop 是 **workspace 外的第三方库**（Rust 的 LALR(1)
> 解析器生成器，v0.22.1），所以本文单独成 md，不写 rustdoc。
> 对象：`soyo_compiler/src/sysy.lalrpop`（292 行，SysY2026 完整语法）。

## 1. 它怎么接进项目

- `soyo_compiler/Cargo.toml`：`lalrpop = "0.22.1"`（构建期生成解析器）、
  `lalrpop-util = { version = "0.22.1", features = ["lexer"] }`（运行时）。
- `soyo_compiler/build.rs`：`lalrpop::process_root()` —— 编译时自动扫描
  `src/*.lalrpop` 生成 `src/sysy.rs`（**改语法后重新 `cargo build` 即可，
  生成是自动的**）。
- `frontend.rs` 里 `pub mod items;` 定义 AST 类型（`CompUnit`/`Stmt`/`Exp`…），
  `.lalrpop` 的动作代码直接引用这些类型（文件头 `use crate::frontend::items::*;`）。
- 解析入口：生成的 `sysy::CompUnitsParser::new().parse(...)`，产出 `CompUnits`
  后由前端降成 RaanaIR。

## 2. 文件结构速览（对照 sysy.lalrpop 逐节）

```
use crate::frontend::items::*;   // AST 类型
use raana_ir::ir::BinaryOp;      // 直接复用 IR 的二元操作枚举

grammar;                          // ← 语法声明（必须有）

match {                           // ← 词法层：被忽略的 token
  r"\s*" => {},                   //    空白
  r"//[^\n\r]*[\n\r]*" => {},     //    行注释
  r"/\*[^*]*\*+(?:[^/*][^*]*\*+)*/" => {},  // 块注释
  _                               //    其余都交给规则里的正则/字面量
}

// ---- 泛型辅助规则 ----
Comma<T>: Vec<T> = {              // 逗号分隔列表（可空尾项）
    <mut v:(<T> ",")*> <e: T?> => match e { ... }
};

// ---- 顶层 ----
pub CompUnits: CompUnits = <comp_units: CompUnit*> => CompUnits {<>};
//   ↑ pub 规则 = 生成公开解析器（CompUnitsParser）

CompUnit: CompUnit = {            // 多分支：const 声明 / void 函数 / 标量开头
    <c_decl: ConstDecl> => CompUnit::Decl(Decl::ConstDecl(c_decl)),
    "void" <ident: Ident> "(" <params: Comma<FuncFParam>> ")" <block: Block> => {...},
    <btype: BType> <ident: Ident> <builder: ScalarSuffix> => builder(btype, ident),
    //                                    ↑ 歧义处理技巧，见 §4
};

// ---- 表达式优先级：用嵌套规则表达 ----
Exp → LOrExp → LAndExp → EqExp → RelExp → AddExp → MulExp → UnaryExp → PrimaryExp
// 每层：非终结符 = 上一层 或 (自身 + 该层运算符 + 上一层)
// 例：AddExp = MulExp | AddExp (+|-) MulExp   ← 左结合

// ---- 悬空 else：MatchedStmt / OpenStmt 拆分 ----
MatchedStmt: Stmt = { ... "if" (...) MatchedStmt "else" MatchedStmt ... };
OpenStmt:   Stmt = { "if" (...) Stmt | "if" (...) MatchedStmt "else" OpenStmt };
Stmt = MatchedStmt | OpenStmt      // 经典解法，见 §4
```

## 3. 语法速查（写新规则必备）

| 写法 | 含义 |
|------|------|
| `Name: Type = { alt1, alt2 };` | 定义规则 `Name`，产出类型 `Type`，多个备选 |
| `"keyword"` | 字面量 token |
| `r"regex"` | 正则 token（如 `Ident: Rc<str> = r"[_a-zA-Z][_a-zA-Z0-9]*"`） |
| `<sym>` | 捕获符号值，放在动作表达式里的 `<>` 位置 |
| `<name: sym>` | 命名捕获，动作里用 `name` |
| `<T>*` / `<T>+` / `<T>?` | 重复/可选（`*` 配 `mut` 可 push） |
| `<a: A> <b: B> => Expr {a, b}` | 动作代码：Rust 表达式，构造 AST |
| `pub Name: ...` | 生成公开解析器（顶层规则必须 pub） |
| `Comma<T>` | 规则可以泛型（本项目用它做逗号列表） |

**正则 token 注意**：`match` 块里的正则最先匹配；数字字面量 `IntConst` 分三
条：十进制 `[1-9][0-9]*`、八进制 `0[0-7]*`、十六进制 `0[xX]...`（顺序敏感，
`0` 前缀要小心）。`FloatConst` 支持小数/科学计数/十六进制浮点，经辅助函数
`parse_float_const` 处理。

## 4. 两个关键技巧（本项目特色，维护时别拆坏）

**技巧 A：ScalarSuffix 闭包解决 `BType Ident` 歧义**
`int f() {...}` 和 `int a = 1;` 都以 `BType Ident` 开头。解法：先解析
`BType Ident`，剩下的部分交给 `ScalarSuffix`——它返回
`Box<dyn FnOnce(Type, Rc<str>) -> CompUnit>` 闭包，等拿到完整后缀信息后
回调构造结果。**改 CompUnit/声明相关语法时，闭包签名和两个分支都要同步改。**

**技巧 B：MatchedStmt/OpenStmt 解决悬空 else**
`if (c) if (d) a; else b;` 的 `else` 归属有歧义。解法：`Stmt` 拆成
`MatchedStmt`（每个 if 都有 else）和 `OpenStmt`（含无 else 的 if）；
`while` 体、`if` 的 then/else 分支**只能用 MatchedStmt**，保证 else 就近
匹配。**新增带嵌套语句的语法（如 for/do-while）时，body 一律用
`MatchedStmt`，否则会产生 shift/reduce 冲突。**

## 5. 如何加一条语法规则（示例：加 `for` 循环）

1. **AST**：`frontend/items.rs` 的 `Stmt` 枚举加 `ForStmt` 变体（含 init/
   cond/step/body 字段，注意 body 用 `Box<Stmt>`）。
2. **语法**：`sysy.lalrpop` 里加
   ```
   ForStmt: Stmt = "for" "(" <init: Exp?> ";" <cond: Exp?> ";" <step: Exp?> ")"
                    <body: MatchedStmt> => Stmt::ForStmt(ForStmt{...});
   ```
   并把它加进 `MatchedStmt` 的分支（body 用 `MatchedStmt`！）。
   > 注：SysY2022 无 for，此例仅为演示；按比赛规则新增语法须谨慎。
3. **下降**：`frontend.rs` 的 AST→RaanaIR 转换里处理 `Stmt::ForStmt`，
   展开成 while + 块（init/step 放进块首尾）。
4. 验证：`cargo build`（自动重新生成解析器）→ 写一个 `.sy` 用例编译。

## 6. 常见坑

- **改 `.lalrpop` 后没生效**：`cargo build` 会自动跑 build.rs 重新生成；
  若用了增量缓存仍不生效，`touch` 一下 `.lalrpop` 或 `cargo clean -p soyo_compiler`。
- **shift/reduce 冲突**：lalrpop 编译报错会列出冲突位置。先看是否涉及
  `if` 嵌套（→ 用 MatchedStmt/OpenStmt）或 `BType Ident`（→ 用后缀闭包）。
- **动作代码编译错**：`<>` 占位符按捕获顺序填；命名捕获后 `<>` 数量减少，
  顺序别乱。
- **正则优先级**：`match` 块先匹配的赢；数字/标识符正则边界要写对
  （`IntConst` 三条顺序不能交换）。
- **生成文件**：`src/sysy.rs` 是生成的，**不要手改**；git 里确认它是
  提交的（本仓库生成文件入库，离线环境无网络也能编译）。
