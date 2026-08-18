//! # items：SysY AST 节点定义与声明辅助逻辑
//!
//! 一句话定位：本模块是「声明（函数 / 全局变量 / 局部变量等）的 AST →
//! RaanaIR 生成」这条流水线的**数据侧**——定义前端（`soyo_compiler`）使用的
//! 全部 AST 节点类型，并提供降级（lowering）所需的声明辅助逻辑；真正的
//! AST → RaanaIR 下降实现（`ToRaanaIR` 的 impl）位于 `ast.rs`，生成上下文
//! `AstGenContext` 位于 `utils.rs`。
//!
//! > 现状说明：本文件在 git commit `5921037`（"\[Backend\] Migrate s2r code"）
//! > 之后由 AI 重构新增（migrate 时不存在），从原 `ast.rs` / `utils.rs` 中
//! > 拆出 AST 定义与声明辅助逻辑；`ast.rs` 现在只保留 `ToRaanaIR` 下降实现。
//!
//! ## 本模块包含的声明类型（AST 节点）
//!
//! - 编译单元：`CompUnits` → `Vec<CompUnit>`，`CompUnit` = `FuncDef` | `Decl`；
//! - 函数：`FuncDef`（返回类型 `FuncType`、参数 `FuncFParam`、函数体 `Block`）；
//! - 块与语句：`Block` / `BlockItem`（= `Decl` | `Stmt`）、`Stmt`（`AssignStmt`、
//!   `ReturnStmt`、`IfStmt`、`WhileStmt`、`Break`、`Continue`）；
//! - 声明：`Decl` = `ConstDecl` | `VarDecl`；`ConstDef` / `VarDef` 携带
//!   `arr_dim`（数组各维的 `ConstExp`）与初始化器 `ConstInitVal` / `InitVal`；
//! - 表达式层级：`Exp` → `LOrExp` → `LAndExp` → `EqExp` → `RelExp` → `AddExp`
//!   → `MulExp` → `UnaryExp` → `PrimaryExp`，终结于 `Number` / `LVal` /
//!   `FuncCall`；`ConstExp` 是编译期可求值的表达式（数组维度、常量初始化用）。
//!
//! 基础类型 `BType` / `FuncType` 直接复用 `raana_ir::ir::Type`，AST 从解析
//! 阶段起就与 IR 类型系统对齐。
//!
//! ## 声明辅助逻辑（本文件的"处理"部分）
//!
//! - `FuncFParam::ty_global` / `ty`：计算（数组）参数的类型——逐维把 `ConstExp`
//!   经 ctx 的值栈求值后 `rfold` 出数组类型，再包一层指针（数组参数按引用
//!   传递，降为指向数组的指针）；`ty_global` 用于函数签名与全局侧，`ty` 是
//!   未使用的遗留方法（`#[allow(unused)]`）；
//! - `ConstInitVal` / `InitVal` 的三个方法：`is_empty_init`（`{}`、`{{}}` 这类
//!   无叶子表达式的初始化器 → 全零）、`explicit_init_vals`（压平成
//!   `(展平下标, 表达式)` 稀疏列表，局部数组"先清零、后定点写"用）、
//!   `init_val_shape`（按完整 shape 展开成 `Vec<Option<&Exp>>` 槽位视图，
//!   全局聚合补零用）；
//! - `parse_float_const`：用 libc `strtof` 解析 C99 浮点字面量（`1.`、`.5`、
//!   `1e-6`、`0x1.921fb6p+1` 等），断言消费完整个 token 且结果有限。
//!
//! ## 与 `ast.rs` / `utils.rs` 的分工边界
//!
//! | 文件 | 职责 |
//! |------|------|
//! | `items.rs`（本文件） | AST 节点定义 + 声明辅助逻辑，本身不产生任何 IR |
//! | `ast.rs` | `ToRaanaIR` 的全部 impl：`convert`（函数内）与
//!   `global_convert`（全局作用域）两套下降；内部工具 `local_array_element_ptr`、
//!   常量折叠 `eval_i32_binary` / `eval_f32_binary` |
//! | `utils.rs` | `AstGenContext`（符号表栈、值栈、函数 / 基本块 / 循环栈、
//!   arena 访问、`coerce_*` / `zero_*` / `truthy_local` 等工具、
//!   `decl_library_functions` 库函数注册）、`ToRaanaIR` trait、`Symbol` /
//!   `SymbolTable`、`Ident` |
//!
//! 数据流：`sysy` 模块的 `CompUnitsParser` 产出本模块的 AST → `ast.rs` 下降
//! → `raana_ir::ir::Program` → 优化管线 → 后端。
//!
//! ## 关键流程
//!
//! ### 函数声明（`ast.rs` 中 `FuncDef::convert`）
//!
//! 1. 参数绑定：逐个 `FuncFParam::ty_global` 算出参数类型，
//!    `program.new_function` 注册函数拿到句柄；
//! 2. 登记与入口块：`insert_func` 写全局符号表（支持递归 / 前向调用）、
//!    `push_func` 入函数栈、`add_entry_bb` + `set_curr_bb` 建立入口块；
//! 3. 参数入栈：`add_scope` 开新作用域；入口块的 block 参数镜像函数签名，
//!    对每个参数 `alloc`（按参数类型）→ 命名 `v_<ident>` → `store` 参数值 →
//!    `insert_var` 绑定符号；
//! 4. 语句下降：逐条 `BlockItem::convert` 处理函数体（`is_complete_bb` 跳过
//!    终结指令之后的死代码）；
//! 5. 尾声：`del_scope` 收作用域；为最后一个基本块补 `ret`（int / float 补
//!    0 / 0.0）保证终结，`reset_curr_bb`、`pop_func` 恢复现场。
//!
//! ### 局部变量 / 常量声明（`VarDef::convert` / `ConstDef::convert`）
//!
//! - 标量变量：`alloc(ty)` → 命名 →（可选）初始化表达式 `convert` +
//!   `coerce_local` 强转 → `store` → `insert_var`；
//! - 数组变量：逐维 `ConstExp` 求值得 shape → `rfold(Type::get_array)` 构造
//!   数组类型 → `alloc(arr_ty)` → `mem_zero` 整块清零 → 用
//!   `explicit_init_vals` 得到 `(flat_index, exp)` 稀疏列表，逐项
//!   `get_elem_ptr` + `store` 定点写入 → `insert_var`；
//! - 局部常量：标量**不分配内存**，求值 + `coerce_local` 后直接
//!   `insert_const`（`Symbol::Constant` 直接绑定 SSA 值）；数组常量仍走
//!   alloc + 清零 + 显式元素 store，再 `insert_const` 绑定 alloc 句柄。
//!
//! ### 全局声明（`global_convert`）
//!
//! 标量：初始化器（或无初始化器时 `zero_init`）经 `coerce_global` 求编译期
//! 常量 → `global_alloc` → 绑定符号。数组：`is_empty_init` 直接
//! `zero_init(arr_ty)`；否则用 `init_val_shape` 填满槽位（缺省槽补
//! `zero_global`），按 shape 反向 `chunks` + `aggregate` 组装嵌套聚合值 →
//! `global_alloc`。
//!
//! ## 正确性要点
//!
//! - **常量与变量符号分离**：`insert_const` / `insert_var` 断言当前作用域内
//!   无重定义；`insert_func` 只能在全局作用域登记（`debug_assert!`）；
//! - **数组参数按指针传递**：`ty_global` 的 `unwrap_or` 分支实际不可达
//!   （代码里留了 `BUG: what the fuck is this line` 注释）；`ty` 是未使用的
//!   遗留方法；
//! - **稀疏数组初始化语义**：局部数组先 `mem_zero` 再定点写显式元素，全局
//!   数组缺省槽补零——保证 `int a[3][3] = {{1}, {}, {2, 3}}` 里 `{}` 覆盖的
//!   位置为 0；
//! - **类型不匹配即 panic**：数组维数非空时初始化器必须是 `Array` 变体
//!   （"Invalid assign" panic），`explicit_init_vals` / `init_val_shape` 内部
//!   对 `Normal` 变体 `unreachable!()`；`ConstDecl` / `VarDecl` 断言
//!   `btype.is_scalar()`；
//! - **死代码跳过**：`is_complete_bb()` 检查当前基本块是否已以 `br` / `jump`
//!   / `ret` 结尾，已终结则不继续生成指令（孤儿指令会被
//!   `remove_orphan_inst` 回收）；
//! - **浮点字面量**：`parse_float_const` 必须消费完整 token 且结果有限，
//!   否则编译期断言失败（拒绝编译而非静默截断）。
//!
//! ## 验证
//!
//! - 本文件 `#[cfg(test)] mod tests`：`parses_c99_float_constants_as_f32`
//!   （C99 浮点字面量）、`sparse_initializers_preserve_nested_subobject_positions`
//!   （`{{1}, {}, {2, 3}}` 压平后下标为 `[0, 6, 7]`）；
//! - 快速反馈：`cargo check -p soyo_compiler`；
//! - 全量门禁：`make test`（Docker harness，functional + h_functional，默认
//!   `-O0`；性能用例加 `ARGS="-O 2"`），RISC-V 侧 `make test-riscv`。
//!
use crate::frontend::utils::{AstGenContext, ToRaanaIR};

use super::utils::Ident;
use raana_ir::ir::{BinaryOp, Type};
use std::ffi::CString;
use std::os::raw::{c_char, c_float};

/// CompUnit ::= FuncDef;
///
/// The root of the AST, representing a complete compilation unit.
#[derive(Debug, Clone)]
pub struct CompUnits {
    pub comp_units: Vec<CompUnit>,
}

#[derive(Debug, Clone)]
pub enum CompUnit {
    FuncDef(FuncDef),
    Decl(Decl),
}

/// FuncDef ::= FuncType IDENT "(" ")" Block;
///
/// A function definition with return type, name, and body.
#[derive(Debug, Clone)]
pub struct FuncDef {
    pub func_type: FuncType,
    pub ident: Ident,
    pub params: Vec<FuncFParam>,
    pub block: Block,
}

#[derive(Debug, Clone)]
pub struct FuncFParam {
    pub b_type: BType,
    pub ident: Ident,
    pub arr_ty: Option<Vec<ConstExp>>,
}

impl FuncFParam {
    pub fn ty_global(&self, ctx: &mut AstGenContext) -> Type {
        self.arr_ty
            .as_ref()
            .map(|arr_ty| {
                Type::get_pointer(arr_ty.iter().rfold(self.b_type.btype.clone(), |ty, off| {
                    off.global_convert(ctx);
                    let idx = ctx.pop_i32() as usize;
                    Type::get_array(ty, idx)
                }))
            })
            // BUG: what the fuck is this line.
            .unwrap_or(self.b_type.btype.clone())
    }

    #[allow(unused)]
    pub fn ty(&self, ctx: &mut AstGenContext) -> Type {
        self.arr_ty
            .as_ref()
            .map(|arr_ty| {
                Type::get_pointer(arr_ty.iter().rfold(self.b_type.btype.clone(), |ty, off| {
                    off.convert(ctx);
                    let idx = ctx.pop_i32() as usize;
                    Type::get_array(ty, idx)
                }))
            })
            // BUG: what the fuck is this line.
            .unwrap_or(self.b_type.btype.clone())
    }
}

/// FuncType ::= "int" | "float";
///
/// The return type of a function.
#[derive(Debug, Clone)]
pub struct FuncType {
    pub is_tensor: bool,
    pub btype: Type,
}

/// Block ::= "{" {BlockItem} "}";
///
/// A block containing zero or more block items.
#[derive(Debug, Clone)]
pub struct Block {
    pub block_items: Vec<BlockItem>,
}

/// BlockItem ::= Decl | Stmt;
///
/// An item within a block, either a declaration or a statement.
#[derive(Debug, Clone)]
pub enum BlockItem {
    Decl(Decl),
    Stmt(Stmt),
}

/// Decl ::= ConstDecl | VarDecl;
///
/// A declaration, either constant or variable.
#[derive(Debug, Clone)]
pub enum Decl {
    ConstDecl(ConstDecl),
    VarDecl(VarDecl),
}

/// ConstDecl ::= "const" BType ConstDef {"," ConstDef} ";";
///
/// A constant declaration with base type and one or more constant definitions.
#[derive(Debug, Clone)]
pub struct ConstDecl {
    pub btype: BType,
    pub const_defs: Vec<ConstDef>,
}

/// BType ::= "int";
///
/// The base type for variables and constants.
#[derive(Debug, Clone)]
pub struct BType {
    pub is_tensor: bool,
    pub btype: Type,
}

pub struct TensorType {
    pub btype: BType,
}

/// ConstDef ::= IDENT "=" ConstInitVal;
///
/// A constant definition with identifier and initial value.
#[derive(Debug, Clone)]
pub struct ConstDef {
    pub ident: Ident,
    pub arr_dim: Vec<ConstExp>,
    pub const_init_val: ConstInitVal,
}

/// ConstInitVal ::= ConstExp;
///
/// The initial value of a constant.
#[derive(Debug, Clone)]
pub enum ConstInitVal {
    Normal(ConstExp),
    Array(Vec<ConstInitVal>),
}

impl ConstInitVal {
    /// An array initializer that contributes no leaf expression (e.g. `{}` or
    /// `{{}}`) initializes every element to zero. Callers may collapse such
    /// initializers into a single `ZeroInit` instead of materializing one
    /// element per array slot.
    pub fn is_empty_init(&self) -> bool {
        match self {
            ConstInitVal::Normal(_) => false,
            ConstInitVal::Array(init_vals) => init_vals.iter().all(Self::is_empty_init),
        }
    }

    pub fn explicit_init_vals(&self, array_shape: &[i32]) -> Vec<(usize, &ConstExp)> {
        let Self::Array(_) = self else { unreachable!() };
        let mut entries = Vec::new();
        self.collect_explicit_init_vals(array_shape, 0, &mut entries);
        entries
    }

    fn collect_explicit_init_vals<'a>(
        &'a self,
        array_shape: &[i32],
        base: usize,
        entries: &mut Vec<(usize, &'a ConstExp)>,
    ) {
        let Self::Array(init_vals) = self else {
            unreachable!()
        };
        let capacity = array_shape.iter().map(|&dim| dim as usize).product();
        let mut cursor = 0;
        for init_val in init_vals {
            if cursor >= capacity {
                break;
            }
            match init_val {
                Self::Normal(exp) => {
                    entries.push((base + cursor, exp));
                    cursor += 1;
                }
                Self::Array(..) => {
                    let mut stride = 1;
                    let mut sub_shape_idx = array_shape.len();
                    for (index, &dim) in array_shape.iter().enumerate().rev() {
                        stride *= dim as usize;
                        if cursor % stride == 0 {
                            sub_shape_idx = index;
                        } else {
                            break;
                        }
                    }
                    if sub_shape_idx == 0 && !array_shape.is_empty() {
                        sub_shape_idx = 1;
                    }
                    let sub_shape = &array_shape[sub_shape_idx..];
                    init_val.collect_explicit_init_vals(sub_shape, base + cursor, entries);
                    cursor += sub_shape.iter().map(|&dim| dim as usize).product::<usize>();
                }
            }
        }
    }

    pub fn init_val_shape(&self, array_shape: &[i32]) -> Vec<Option<&ConstExp>> {
        let Self::Array(c_init_vals) = self else {
            unreachable!()
        };
        let capacity = array_shape.iter().map(|x| *x as usize).product();
        let mut v = Vec::with_capacity(capacity);
        for init_val in c_init_vals {
            if v.len() >= capacity {
                break;
            }
            match init_val {
                Self::Normal(exp) => v.push(Some(exp)),
                Self::Array(..) => {
                    let mut stride = 1;
                    let mut sub_shape_idx = array_shape.len();

                    for (i, &dim) in array_shape.iter().enumerate().rev() {
                        stride *= dim as usize;
                        if v.len() % stride == 0 {
                            sub_shape_idx = i;
                        } else {
                            break;
                        }
                    }

                    // 如果是 {} 这种嵌套，强制至少下降一级
                    if sub_shape_idx == 0 && !array_shape.is_empty() {
                        sub_shape_idx = 1;
                    }

                    let sub_vals = init_val.init_val_shape(&array_shape[sub_shape_idx..]);
                    v.extend(sub_vals);
                }
            }
        }
        // if the initialization values are more than needed, simply truncate it.
        v.resize(capacity, None);
        v
    }
}

/// VarDecl ::= BType VarDef {"," VarDef} ";";
///
/// A variable declaration with base type and one or more variable definitions.
#[derive(Debug, Clone)]
pub struct VarDecl {
    pub btype: BType,
    pub var_defs: Vec<VarDef>,
}

/// VarDef ::= IDENT | IDENT "=" InitVal;
///
/// A variable definition with identifier and optional initial value.
#[derive(Debug, Clone)]
pub struct VarDef {
    pub ident: Ident,
    pub arr_dim: Vec<ConstExp>,
    pub init_val: Option<InitVal>,
}

/// InitVal ::= Exp;
///
/// The initial value of a variable.
#[derive(Debug, Clone)]
pub enum InitVal {
    Normal(Exp),
    Array(Vec<InitVal>),
}

impl InitVal {
    /// An array initializer that contributes no leaf expression (e.g. `{}` or
    /// `{{}}`) initializes every element to zero. Callers may collapse such
    /// initializers into a single `ZeroInit` instead of materializing one
    /// element per array slot.
    pub fn is_empty_init(&self) -> bool {
        match self {
            InitVal::Normal(_) => false,
            InitVal::Array(init_vals) => init_vals.iter().all(Self::is_empty_init),
        }
    }

    pub fn explicit_init_vals(&self, array_shape: &[i32]) -> Vec<(usize, &Exp)> {
        let Self::Array(_) = self else { unreachable!() };
        let mut entries = Vec::new();
        self.collect_explicit_init_vals(array_shape, 0, &mut entries);
        entries
    }

    fn collect_explicit_init_vals<'a>(
        &'a self,
        array_shape: &[i32],
        base: usize,
        entries: &mut Vec<(usize, &'a Exp)>,
    ) {
        let Self::Array(init_vals) = self else {
            unreachable!()
        };
        let capacity = array_shape.iter().map(|&dim| dim as usize).product();
        let mut cursor = 0;
        for init_val in init_vals {
            if cursor >= capacity {
                break;
            }
            match init_val {
                Self::Normal(exp) => {
                    entries.push((base + cursor, exp));
                    cursor += 1;
                }
                Self::Array(..) => {
                    let mut stride = 1;
                    let mut sub_shape_idx = array_shape.len();
                    for (index, &dim) in array_shape.iter().enumerate().rev() {
                        stride *= dim as usize;
                        if cursor % stride == 0 {
                            sub_shape_idx = index;
                        } else {
                            break;
                        }
                    }
                    if sub_shape_idx == 0 && !array_shape.is_empty() {
                        sub_shape_idx = 1;
                    }
                    let sub_shape = &array_shape[sub_shape_idx..];
                    init_val.collect_explicit_init_vals(sub_shape, base + cursor, entries);
                    cursor += sub_shape.iter().map(|&dim| dim as usize).product::<usize>();
                }
            }
        }
    }

    pub fn init_val_shape(&self, array_shape: &[i32]) -> Vec<Option<&Exp>> {
        let Self::Array(c_init_vals) = self else {
            unreachable!()
        };
        let capacity = array_shape.iter().map(|x| *x as usize).product();
        let mut v = Vec::with_capacity(capacity);
        for init_val in c_init_vals {
            if v.len() >= capacity {
                break;
            }
            match init_val {
                Self::Normal(exp) => v.push(Some(exp)),
                Self::Array(..) => {
                    let mut stride = 1;
                    let mut sub_shape_idx = array_shape.len();

                    for (i, &dim) in array_shape.iter().enumerate().rev() {
                        stride *= dim as usize;
                        if v.len() % stride == 0 {
                            sub_shape_idx = i;
                        } else {
                            break;
                        }
                    }

                    // 如果是 {} 这种嵌套，强制至少下降一级
                    if sub_shape_idx == 0 && !array_shape.is_empty() {
                        sub_shape_idx = 1;
                    }

                    let sub_vals = init_val.init_val_shape(&array_shape[sub_shape_idx..]);
                    v.extend(sub_vals);
                }
            }
        }
        // if the initialization values are more than needed, simply truncate it.
        v.resize(capacity, None);
        v
    }
}

/// Stmt ::= LVal "=" Exp ";" | "return" Exp ";";
///
/// A statement, either an assignment or a return statement.
#[derive(Debug, Clone)]
pub enum Stmt {
    Assign(AssignStmt),
    Block(Block),
    Single(Option<Exp>),
    Return(ReturnStmt),
    IfStmt(IfStmt),
    WhileStmt(WhileStmt),
    Break(Break),
    Continue(Continue),
}

#[derive(Debug, Clone)]
pub struct Break;

#[derive(Debug, Clone)]
pub struct Continue;

#[derive(Debug, Clone)]
pub struct ReturnStmt {
    pub exp: Option<Exp>,
}

#[derive(Debug, Clone)]
pub struct AssignStmt {
    pub l_val: LVal,
    pub exp: Exp,
}

#[derive(Debug, Clone)]
pub struct IfStmt {
    pub cond: Exp,
    pub then_branch: Box<Stmt>,
    pub else_branch: Option<Box<Stmt>>,
}

#[derive(Debug, Clone)]
pub struct WhileStmt {
    pub cond: Exp,
    pub body: Box<Stmt>,
}

/// Exp ::= LOrExp;
///
/// An expression, starting from logical OR expressions.
#[derive(Debug, Clone)]
pub struct Exp {
    pub lor_exp: LOrExp,
}

/// LVal ::= IDENT;
///
/// A left-value, representing a variable that can be assigned to.
#[derive(Debug, Clone)]
pub struct LVal {
    pub ident: Ident,
    pub index: Vec<Exp>,
}

/// ConstExp ::= Exp;
///
/// A constant expression, must be evaluable at compile time.
#[derive(Debug, Clone)]
pub struct ConstExp {
    pub exp: Exp,
}

/// UnaryExp ::= PrimaryExp | UnaryOp UnaryExp;
///
/// A unary expression, either a primary expression or a unary operation applied to another unary expression.
#[derive(Debug, Clone)]
pub enum UnaryExp {
    PrimaryExp(Box<PrimaryExp>),
    Unary(UnaryOp, Box<UnaryExp>),
    FuncCall(FuncCall),
}

#[derive(Debug, Clone)]
pub struct FuncCall {
    pub ident: Ident,
    pub args: Vec<Exp>,
}

/// UnaryOp ::= "+" | "-" | "!";
///
/// A unary operator: positive, negative, or logical negation.
#[derive(Debug, Clone)]
pub enum UnaryOp {
    Add,
    Minus,
    Negation,
}

/// PrimaryExp ::= "(" Exp ")" | LVal | Number;
///
/// A primary expression: parenthesized expression, left-value, or number literal.
#[derive(Debug, Clone)]
pub enum PrimaryExp {
    Exp(Exp),
    LVal(LVal),
    Number(Number),
}

/// AddExp ::= MulExp | AddExp ("+" | "-") MulExp;
///
/// An additive expression with addition or subtraction.
#[derive(Debug, Clone)]
pub enum AddExp {
    MulExp(MulExp),
    Comp(Box<AddExp>, BinaryOp, MulExp),
}

/// MulExp ::= UnaryExp | MulExp ("*" | "/" | "%") UnaryExp;
///
/// A multiplicative expression with multiplication, division, or modulo.
#[derive(Debug, Clone)]
pub enum MulExp {
    UnaryExp(UnaryExp),
    Comp(Box<MulExp>, BinaryOp, UnaryExp),
}

/// LOrExp ::= LAndExp | LOrExp "||" LAndExp;
///
/// A logical OR expression with short-circuit evaluation.
#[derive(Debug, Clone)]
pub enum LOrExp {
    LAndExp(LAndExp),
    Comp(Box<LOrExp>, LAndExp),
}

/// LAndExp ::= EqExp | LAndExp "&&" EqExp;
///
/// A logical AND expression with short-circuit evaluation.
#[derive(Debug, Clone)]
pub enum LAndExp {
    EqExp(EqExp),
    Comp(Box<LAndExp>, EqExp),
}

/// EqExp ::= RelExp | EqExp ("==" | "!=") RelExp;
///
/// An equality expression with equal or not-equal comparison.
#[derive(Debug, Clone)]
pub enum EqExp {
    RelExp(RelExp),
    Comp(Box<EqExp>, BinaryOp, RelExp),
}

/// RelExp ::= AddExp | RelExp ("<" | ">" | "<=" | ">=") AddExp;
///
/// A relational expression with comparison operators.
#[derive(Debug, Clone)]
pub enum RelExp {
    AddExp(AddExp),
    Comp(Box<RelExp>, BinaryOp, AddExp),
}

/// Number ::= INT_CONST | FLOAT_CONST;
///
/// A numeric constant literal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Number {
    Int(i32),
    Float(f32),
}

unsafe extern "C" {
    fn strtof(nptr: *const c_char, endptr: *mut *mut c_char) -> c_float;
}

pub fn parse_float_const(src: &str) -> f32 {
    let c_src = CString::new(src).expect("float constants cannot contain NUL bytes");
    let mut end = std::ptr::null_mut();
    let value = unsafe { strtof(c_src.as_ptr(), &mut end) };
    let expected_end = unsafe { c_src.as_ptr().add(src.len()) as *mut c_char };
    assert!(
        end == expected_end,
        "invalid float constant {src:?}: conversion stopped before the token ended"
    );
    assert!(
        value.is_finite(),
        "float constant {src:?} is outside the finite f32 range"
    );
    value
}

#[cfg(test)]
mod tests {
    use super::{CompUnit, ConstInitVal, Decl, InitVal, parse_float_const};

    #[test]
    fn parses_c99_float_constants_as_f32() {
        assert_eq!(parse_float_const("1.").to_bits(), 1.0f32.to_bits());
        assert_eq!(parse_float_const(".5").to_bits(), 0.5f32.to_bits());
        assert_eq!(parse_float_const("1e-6").to_bits(), 1e-6f32.to_bits());
        assert_eq!(
            parse_float_const("0x1.921fb6p+1").to_bits(),
            std::f32::consts::PI.to_bits()
        );
        assert_eq!(
            parse_float_const("0x.AP-3").to_bits(),
            0.078125f32.to_bits()
        );
        assert_eq!(
            parse_float_const("03.141592653589793").to_bits(),
            std::f32::consts::PI.to_bits()
        );
    }

    #[test]
    fn sparse_initializers_preserve_nested_subobject_positions() {
        let ast = crate::sysy::CompUnitsParser::new()
            .parse("int a[3][3] = {{1}, {}, {2, 3}}; const int b[3][3] = {{1}, {}, {2, 3}};")
            .unwrap();
        let CompUnit::Decl(Decl::VarDecl(var_decl)) = &ast.comp_units[0] else {
            panic!("expected variable declaration")
        };
        let Some(InitVal::Array(_)) = &var_decl.var_defs[0].init_val else {
            panic!("expected array initializer")
        };
        assert_eq!(
            var_decl.var_defs[0]
                .init_val
                .as_ref()
                .unwrap()
                .explicit_init_vals(&[3, 3])
                .iter()
                .map(|(index, _)| *index)
                .collect::<Vec<_>>(),
            vec![0, 6, 7]
        );

        let CompUnit::Decl(Decl::ConstDecl(const_decl)) = &ast.comp_units[1] else {
            panic!("expected constant declaration")
        };
        let ConstInitVal::Array(_) = &const_decl.const_defs[0].const_init_val else {
            panic!("expected array initializer")
        };
        assert_eq!(
            const_decl.const_defs[0]
                .const_init_val
                .explicit_init_vals(&[3, 3])
                .iter()
                .map(|(index, _)| *index)
                .collect::<Vec<_>>(),
            vec![0, 6, 7]
        );
    }
}
