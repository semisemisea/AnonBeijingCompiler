//! # SysY 前端 AST 下降：AST → RaanaIR
//!
//! 本文件承载 SysY 前端的**下降（lowering）**：把解析器（`sysy.lalrpop` 生成的
//! `CompUnitsParser`）产出的 AST 一棵一棵翻译成 `raana_ir` 的 SSA 中间表示
//! （`Program` / `Function` / `BasicBlock` / `Inst`），供优化管线与后端使用。
//! 注意：**AST 节点的类型定义不在本文件**，而在 `items.rs`；这里只有下降逻辑——
//! 33 个 `ToRaanaIR` 的 `impl` 块，外加 4 个私有辅助函数，共 37 个顶层项，
//! 即"每个 AST 节点如何变成 RaanaIR"的全部答案。
//!
//! ## 文件分工（`frontend/` 三件套）
//!
//! | 文件 | 职责 |
//! |------|------|
//! | `items.rs` | AST 节点**类型定义**（`CompUnits` / `Stmt` / `Exp` 等，带 EBNF 文法注释）；初始化器形状计算（`explicit_init_vals` / `init_val_shape` / `is_empty_init`）、`FuncFParam::ty_global`（数组形参降指针）、`parse_float_const`（浮点字面量解析，用 `strtof`） |
//! | `ast.rs`（本文件） | `ToRaanaIR` 的全部实现：AST → RaanaIR 的下降 |
//! | `utils.rs` | 下降基础设施：`ToRaanaIR` trait 定义、`AstGenContext`（持有 `Program`、值栈、符号表栈、当前基本块、循环栈等）、`Symbol` 符号表条目、`coerce_local` / `truthy_local` / `zero_*` 等辅助、库函数声明（`decl_library_functions`） |
//! | `frontend.rs` | 模块装配：`mod ast;`（私有）、`pub mod items;`、`pub mod utils;` |
//!
//! ## AST 节点分类（定义在 `items.rs`）
//!
//! 按 SysY 文法自顶向下分四类：
//!
//! - **顶层**：`CompUnits`（根）→ `CompUnit` = `FuncDef` 函数定义 | `Decl` 全局声明；
//! - **声明**：`Decl` = `ConstDecl` | `VarDecl` → `ConstDef` / `VarDef`（含 `arr_dim`
//!   数组维度，元素是 `ConstExp`）→ `ConstInitVal` / `InitVal`（`Normal` 单个表达式
//!   | `Array` 嵌套花括号列表）、`ConstExp` 常量表达式（要求编译期可求值）；
//! - **语句**：`Stmt` 枚举——`Block` 复合块、`AssignStmt` 赋值、`ReturnStmt` 返回、
//!   `IfStmt` 条件、`WhileStmt` 循环、`Break` / `Continue`、`Single(Option<Exp>)`
//!   表达式语句；
//! - **表达式**：按优先级从低到高逐层包装的经典链（每个复合层都是
//!   `Comp(Box<Self>, 操作数, 右操作数)` 右递归形态，与 EBNF 一一对应）：
//!   `Exp` → `LOrExp`（`\|\|`）→ `LAndExp`（`&&`）→ `EqExp`（`==` / `!=`）→
//!   `RelExp`（`<` / `>` / `<=` / `>=`）→ `AddExp`（`+` / `-`）→ `MulExp`
//!   （`*` / `/` / `%`）→ `UnaryExp`（一元 `+` / `-` / `!`，以及 `FuncCall`
//!   函数调用）→ `PrimaryExp`（括号表达式 | `LVal` 左值 | `Number` 字面量）；
//! - **其他**：`FuncFParam` 形参（`b_type` + 可选数组维度）、`UnaryOp` 一元操作符、
//!   `Number` = `Int(i32)` | `Float(f32)`；`FuncType` / `BType` 是 `raana_ir` 的
//!   `Type` 的类型别名。
//!
//! ## 下降流程（AST → RaanaIR，5 步）
//!
//! 入口在 `main.rs::run`（`-S` / `--emit ir` 主流程）与 `abi_matrix.rs`（ABI 矩阵
//! 工具）：lalrpop 解析源码得到 `CompUnits` 后，`AstGenContext::new()` 建上下文，
//! 调用 `ast.convert(&mut ctx)`：
//!
//! 1. **根下降**（`CompUnits::convert`）：先把 13 个 SysY 运行时库函数
//!   （`getint` / `putint` / `getarray` / `_sysy_starttime` 等）声明进**全局**
//!   符号表（`decl_library_functions`），再逐个下降 `CompUnit`——函数定义走
//!   `convert`，全局声明走 `global_convert`；
//! 2. **函数下降**（`FuncDef::convert`）：`program.new_function` 注册函数 →
//!   `insert_func` / `push_func` 入栈 → 建 entry 基本块 → `add_scope` 开新作用域 →
//!   形参逐个 `alloc` + `store` 并 `insert_var` 绑定到符号表 → 递归下降函数体 →
//!   `del_scope`；末尾补一条默认 `ret`（`void` 无操作数，`int`/`float` 补 0/0.0），
//!   保证最后一个基本块有终结符；
//! 3. **节点递归**：语句把指令插进当前基本块（`push_inst`）；表达式把结果 `Inst`
//!   压进 `AstGenContext` 的**值栈**（`push_val` / `pop_val`）——二元/一元操作符
//!   从栈顶弹操作数（先弹右操作数，再弹左操作数），算完把结果压回；
//! 4. **控制流**：`IfStmt` / `WhileStmt` / 短路 `&&` `||` 用 builder 新建基本块、
//!   以 `branch` / `jump` 连接（`set_curr_bb` 切换当前块，块间用 SSA 块参数传值）；
//!   `break` / `continue` 从循环栈（`loop_stack`）取 entry / end 块直接 `jump`；
//! 5. **全局项**（`global_convert`）：全局变量/常量在 `Program` 的全局 arena 里
//!   `global_alloc`，初始值必须是编译期常量（`coerce_global`，否则 `unreachable`）；
//!   全局数组初始值用 `zero_init` / `aggregate` 按维度逐层拼成 `GlobalAlloc` 的
//!   初始值。
//!
//! 下降产物即 `ctx.program`（`raana_ir::ir::Program`），随后交给
//! `PassesManager::run_passes` 做 IR 层优化。
//!
//! ## 关键机制与正确性要点
//!
//! - **作用域**：符号表是 `Vec<SymbolTable>` 栈（第 0 层即全局作用域）；
//!   `add_scope` / `del_scope` 配对；`get_symbol` 从内向外查（内层同名遮蔽外层）；
//!   同一作用域重复定义变量/常量/函数直接 panic；函数只允许注册在全局作用域
//!   （`insert_func` 的 `debug_assert`）；
//! - **左值语义**：符号表里变量绑定到 `alloc` 的**指针**，读变量是 `load`、写变量
//!   是 `store`；数组下标逐维 `get_elem_ptr`（GEP）寻址；数组形参类型本身降为指针，
//!   实参传数组时先 `get_elem_ptr([0, 0])` 降一级再传；
//! - **类型检查**：赋值时编译期断言 rhs 类型与 lhs 解引用类型一致
//!   （`Type::get_pointer(rhs_ty) == lhs_ptr_ty`）；`%` / `&&` / `\|\|` / 位运算等
//!   int-only 操作符遇 float 操作数 panic（`binary_requires_int`）；声明要求标量基
//!   类型（`is_scalar` 断言）；`Symbol::Constant` 不可赋值、不可取地址修改；
//! - **常量折叠**：二元/一元运算在操作数是 `Integer` / `Float` 字面量时直接算出
//!   结果（`eval_i32_binary` / `eval_f32_binary`，除零/模零 panic；整数运算走
//!   wrapping 语义，溢出回绕）；短路逻辑在两侧都是常量时删掉临时基本块直接求值；
//! - **死代码跳过**：`is_complete_bb` 检测当前块已以 `br` / `jump` / `ret` 终结；
//!   终结后 `push_inst` 丢弃孤儿指令，各 `convert` 开头也据此短路——`return` 之后
//!   的语句、`break` / `continue` 之后的语句天然不会生成代码；
//! - **终结符不变量**：`set_curr_bb` 离开一个未终结的块时自动补默认 `ret`，与函数
//!   末尾的显式 `ret` 一起保证**每个基本块都以终结符结尾**（IR 层硬性不变量）；
//! - **循环约束**：`break` / `continue` 不在循环内（`curr_loop` 为 `None`）时
//!   panic；`while` 循环展开为 entry（条件测试）/ body / end 三块，条件经
//!   `truthy_local` 归一化成 i32 布尔后 `branch`；
//! - **宏特判**：`starttime()` / `stoptime()` 调用被改写为 `_sysy_starttime(0)` /
//!   `_sysy_stoptime(0)`（性能计时用，行号参数以 0 代替）。
//!
//! ## 触发与使用场景
//!
//! - `soyo_compiler/src/main.rs`（`run`）：`-S` 汇编 / `--emit ir` 的编译主流程；
//! - `soyo_compiler/src/abi_matrix.rs`：ABI 矩阵测试工具（同样 parse + convert）。
//!
//! ## 验证
//!
//! - 本文件无独立测试：AST/下降的单元测试在 `items.rs` 的 `mod tests`（解析 +
//!   下降冒烟用例）与 `abi_matrix.rs` 的 `mod tests`；
//! - IR 层回归：`cargo test -p raana_ir`；编译正确性靠 `make test`（Docker
//!   harness，functional / h_functional / perf 语料，默认 `-O0`，优化验证加
//!   `ARGS="-O 2"`）；改动本文件后至少跑 `cargo check -p soyo_compiler`。
//!
//! ## 相关背景
//!
//! `frontend/` 在 git commit 5921037（"\[Backend\] Migrate s2r code"）之后经 AI 大量
//! 重构：本文件由 1498 行（33 个 pub 项）增至 1586 行（37 个 pub 项）。重构新增了
//! 全局项下降（`global_convert` 体系）、短路逻辑的编译期折叠、`is_complete_bb`
//! 死代码跳过与自动补 `ret` 等机制——读代码时以本模块文档为索引，逐 impl 对照
//! `items.rs` 的节点定义与 `utils.rs` 的上下文辅助即可。

use super::items;
use crate::frontend::{
    items::{
        AddExp, AssignStmt, Block, BlockItem, ConstDef, Decl, EqExp, Exp, LAndExp, LOrExp, MulExp,
        PrimaryExp, RelExp, Stmt, UnaryExp, VarDef,
    },
    utils::{AstGenContext, Ident, Symbol, ToRaanaIR},
};
use inst_kind::binary;
use raana_ir::ir::{arena::Arena, builder_trait::*, *};

fn binary_requires_int(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Rem
            | BinaryOp::And
            | BinaryOp::Or
            | BinaryOp::Xor
            | BinaryOp::Shl
            | BinaryOp::Shr
            | BinaryOp::Sar
    )
}

fn eval_i32_binary(op: BinaryOp, lhs: i32, rhs: i32) -> i32 {
    match op {
        BinaryOp::NotEq => (lhs != rhs) as i32,
        BinaryOp::Eq => (lhs == rhs) as i32,
        BinaryOp::Gt => (lhs > rhs) as i32,
        BinaryOp::Lt => (lhs < rhs) as i32,
        BinaryOp::Ge => (lhs >= rhs) as i32,
        BinaryOp::Le => (lhs <= rhs) as i32,
        BinaryOp::Add => lhs.wrapping_add(rhs),
        BinaryOp::Sub => lhs.wrapping_sub(rhs),
        BinaryOp::Mul => lhs.wrapping_mul(rhs),
        BinaryOp::Div => {
            if rhs == 0 {
                panic!("Division by zero");
            }
            lhs.wrapping_div(rhs)
        }
        BinaryOp::Rem => {
            if rhs == 0 {
                panic!("Modulo by zero");
            }
            lhs.wrapping_rem(rhs)
        }
        BinaryOp::And => lhs & rhs,
        BinaryOp::Or => lhs | rhs,
        BinaryOp::Xor => lhs ^ rhs,
        BinaryOp::Shl => lhs.wrapping_shl(rhs as u32),
        BinaryOp::Shr => (lhs as u32).wrapping_shr(rhs as u32) as i32,
        BinaryOp::Sar => lhs.wrapping_shr(rhs as u32),
        BinaryOp::Min | BinaryOp::Max => {
            unreachable!("min/max is vector-only and never produced by the frontend")
        }
        BinaryOp::MatMul => todo!(),
    }
}

fn eval_f32_binary(op: BinaryOp, lhs: f32, rhs: f32) -> items::Number {
    match op {
        BinaryOp::NotEq => items::Number::Int((lhs != rhs) as i32),
        BinaryOp::Eq => items::Number::Int((lhs == rhs) as i32),
        BinaryOp::Gt => items::Number::Int((lhs > rhs) as i32),
        BinaryOp::Lt => items::Number::Int((lhs < rhs) as i32),
        BinaryOp::Ge => items::Number::Int((lhs >= rhs) as i32),
        BinaryOp::Le => items::Number::Int((lhs <= rhs) as i32),
        BinaryOp::Add => items::Number::Float(lhs + rhs),
        BinaryOp::Sub => items::Number::Float(lhs - rhs),
        BinaryOp::Mul => items::Number::Float(lhs * rhs),
        BinaryOp::Div => items::Number::Float(lhs / rhs),
        BinaryOp::Rem
        | BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::Xor
        | BinaryOp::Shl
        | BinaryOp::Shr
        | BinaryOp::Sar => panic!("Integer-only binary operator used with float operand"),
        BinaryOp::Min | BinaryOp::Max => {
            unreachable!("min/max is vector-only and never produced by the frontend")
        }
        BinaryOp::MatMul => todo!(),
    }
}

fn local_array_element_ptr(
    ctx: &mut AstGenContext,
    alloc: Inst,
    array_shape: &[i32],
    flat_index: usize,
) -> Inst {
    let mut offsets = Vec::with_capacity(array_shape.len() + 1);
    offsets.push(ctx.new_local_value().integer(0));
    let mut remaining = flat_index;
    let mut indices = Vec::with_capacity(array_shape.len());
    for &dim in array_shape.iter().rev() {
        indices.push(remaining % dim as usize);
        remaining /= dim as usize;
    }
    for index in indices.into_iter().rev() {
        offsets.push(ctx.new_local_value().integer(index as i32));
    }
    ctx.new_local_value().get_elem_ptr(alloc, offsets)
}

fn tensor_elem_ty(ctx: &AstGenContext, inst: Inst) -> Type {
    let ty = ctx.inst_data(inst).ty();
    assert!(ty.is_tensor(), "not a tensor");
    if ty.is_array() {
        ty.get_array_elem_ty()
    } else {
        // is pointer
        ty.derefernce().get_array_elem_ty()
    }
}

fn tensor_elem_ptr_raw(ctx: &mut AstGenContext, inst: Inst, idxs: &[Inst]) -> Inst {
    let ty = ctx.inst_data(inst).ty();

    let offsets = if ty.is_pointer() {
        std::iter::once(ctx.new_local_value().integer(0))
            .chain(idxs.iter().copied())
            .collect::<Vec<_>>()
    } else {
        idxs.to_vec()
    };

    let gep = ctx.new_local_value().get_elem_ptr(inst, offsets);
    ctx.push_inst(gep);
    gep
}

fn tensor_elem_ptr(ctx: &mut AstGenContext, inst: Inst, idxs: &[usize]) -> Inst {
    let ty = ctx.inst_data(inst).ty();

    let offsets = if ty.is_pointer() {
        std::iter::once(0usize)
            .chain(idxs.iter().copied())
            .map(|idx| ctx.new_local_value().integer(idx as i32))
            .collect::<Vec<_>>()
    } else {
        idxs.iter()
            .map(|&idx| ctx.new_local_value().integer(idx as i32))
            .collect::<Vec<_>>()
    };

    let gep = ctx.new_local_value().get_elem_ptr(inst, offsets);
    ctx.push_inst(gep);
    gep
}

fn tensor_get_elem_raw(ctx: &mut AstGenContext, inst: Inst, idxs: &[Inst]) -> Inst {
    let gep = tensor_elem_ptr_raw(ctx, inst, idxs);
    let load = ctx.new_local_value().load(gep);
    ctx.push_inst(load);
    load
}

fn tensor_get_elem(ctx: &mut AstGenContext, inst: Inst, idxs: &[usize]) -> Inst {
    let ptr = tensor_elem_ptr(ctx, inst, idxs);
    let load = ctx.new_local_value().load(ptr);
    ctx.push_inst(load);
    load
}

impl ToRaanaIR for items::CompUnits {
    fn convert(&self, ctx: &mut AstGenContext) {
        ctx.decl_library_functions();
        for comp_unit in &self.comp_units {
            comp_unit.convert(ctx);
        }
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::CompUnit {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::CompUnit::FuncDef(func_def) => func_def.convert(ctx),
            items::CompUnit::Decl(decl) => decl.global_convert(ctx),
        }
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

fn infer_exp_shape(ctx: &AstGenContext, exp: &Exp) -> Option<Type> {
    let lor = &exp.lor_exp;
    let LOrExp::LAndExp(land) = lor else {
        return None;
    };
    let LAndExp::EqExp(eq) = land else {
        return None;
    };
    let EqExp::RelExp(rel) = eq else { return None };
    let RelExp::AddExp(add) = rel else {
        return None;
    };
    let AddExp::MulExp(mul) = add else {
        return None;
    };
    let MulExp::UnaryExp(unary) = mul else {
        return None;
    };
    match unary {
        UnaryExp::FuncCall(call) => None,
        UnaryExp::PrimaryExp(primary) => {
            let PrimaryExp::LVal(ref lval) = **primary else {
                return None;
            };
            ctx.tensor_table().get(&lval.ident).cloned()
        }
        UnaryExp::Unary(op, expr) => None,
    }
}

fn collect_stmt_shape(ctx: &mut AstGenContext, stmt: &Stmt) -> Option<Type> {
    match stmt {
        Stmt::Assign(assign_stmt) => {
            // todo!();
            // let lval = assign_stmt.l_val;
            // let ident = lval.ident.clone();
            // ctx.tensor_table_mut().insert()
            None
        }
        Stmt::Block(block) => collect_ret_shape(ctx, block),
        Stmt::Single(_) => todo!(),
        Stmt::IfStmt(if_stmt) => {
            let shape = collect_stmt_shape(ctx, &if_stmt.then_branch);
            if let Some(ref else_stmt) = if_stmt.else_branch {
                // collect_stmt_shape(ctx, *else_stmt);
            }
            shape
        }
        Stmt::WhileStmt(while_stmt) => collect_stmt_shape(ctx, &while_stmt.body),
        Stmt::Break(_) => None,
        Stmt::Continue(_) => None,
        Stmt::Return(return_stmt) => {
            let Some(ref exp) = return_stmt.exp else {
                return None;
            };
            infer_exp_shape(ctx, exp)
        }
    }
}

fn collect_ret_shape(ctx: &mut AstGenContext, block: &Block) -> Option<Type> {
    let types = block
        .block_items
        .iter()
        .map(|item| match item {
            BlockItem::Decl(decl) => match decl {
                Decl::ConstDecl(decl) => {
                    if !decl.btype.is_tensor {
                        return None;
                    }
                    decl.const_defs.iter().for_each(|def: &ConstDef| {
                        let base_ty = decl.btype.btype.array_base_scalar_type();
                        let shape = def
                            .arr_dim
                            .iter()
                            .map(|exp| {
                                exp.global_convert(ctx);
                                ctx.pop_i32() as usize
                            })
                            .collect::<Vec<_>>();
                        let ty = get_type_from_shape(base_ty, &shape);
                        ctx.tensor_table_mut().insert(def.ident.clone(), ty);
                    });
                    None
                }
                Decl::VarDecl(decl) => {
                    if !decl.btype.is_tensor {
                        return None;
                    }
                    decl.var_defs.iter().for_each(|def: &VarDef| {
                        let base_ty = decl.btype.btype.array_base_scalar_type().clone();
                        let shape = def
                            .arr_dim
                            .iter()
                            .map(|exp| {
                                exp.global_convert(ctx);
                                ctx.pop_i32() as usize
                            })
                            .collect::<Vec<_>>();
                        let ty = get_type_from_shape(base_ty, &shape);
                        ctx.tensor_table_mut().insert(def.ident.clone(), ty);
                    });
                    None
                }
            },
            BlockItem::Stmt(stmt) => {
                let result = collect_stmt_shape(ctx, &stmt);
                result
            }
        })
        .collect::<Vec<_>>();
    let types = types
        .iter()
        .filter(|ty| ty.is_some())
        .map(|ty| ty.clone().unwrap())
        .collect::<Vec<Type>>();
    types.first().cloned()
}

impl ToRaanaIR for items::FuncDef {
    fn convert(&self, ctx: &mut AstGenContext) {
        // 检查返回tensor形状
        let ret_ty = if self.func_type.is_tensor {
            Type::get_pointer(collect_ret_shape(ctx, &self.block).unwrap())
        } else {
            self.func_type.btype.clone()
        };

        // Register the function to get handle
        let param_ty = self
            .params
            .iter()
            .map(|x| x.ty_global(ctx))
            .collect::<Vec<_>>();
        let func = ctx
            .program
            .new_function(ret_ty, self.ident.as_ref().to_string(), param_ty);
        // Prologue
        // - Add function to the stack
        // - Insert the "entry" basic block and save it.
        // - Increse the scope depth.
        ctx.insert_func(self.ident.clone(), func);
        ctx.push_func(func);
        let entry_bb = ctx.add_entry_bb();
        ctx.set_curr_bb(entry_bb);
        // let prev_bb = ctx.set_curr_bb(entry_bb);

        // Recursive conversion.
        ctx.add_scope();
        // The function body references the entry block's parameters (which
        // mirror the signature).
        let params = ctx.curr_func_data().bb_data(entry_bb).params().to_vec();
        let ty_name_and_val = self.params.iter().cloned().zip(params.iter());
        for (param_slot, &param_val) in ty_name_and_val {
            let ty = param_slot.ty_global(ctx);
            let alloc = ctx.new_local_value().alloc(ty);
            ctx.set_value_name(alloc, param_slot.ident.clone().clone());
            let store = ctx.new_local_value().store(param_val, alloc);
            ctx.insert_var(param_slot.ident.clone(), alloc);
            ctx.push_inst(alloc);
            ctx.push_inst(store);
        }
        for block_item in &self.block.block_items {
            block_item.convert(ctx);
        }
        ctx.del_scope();

        // Epilogue: ensure the final block has a terminator.
        // Unreachable join blocks (all branches return) still get a valid ret
        // for the function's declared type.
        let ret_val = match ctx.curr_func_data().ret_ty().kind() {
            TypeKind::Unit => None,
            TypeKind::Int32 => Some(ctx.new_local_value().integer(0)),
            TypeKind::Float32 => Some(ctx.new_local_value().float(0.0)),
            TypeKind::Pointer(ty) => {
                let ty = ty.clone();
                Some(ctx.new_local_value().undef(ty))
            }
            _ => unreachable!(),
        };
        let ret = ctx.new_local_value().ret(ret_val);
        ctx.push_inst(ret);

        ctx.reset_curr_bb();
        ctx.pop_func();
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::Block {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        ctx.add_scope();
        for block_item in &self.block_items {
            block_item.convert(ctx);
        }
        ctx.del_scope();
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::BlockItem {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::BlockItem::Decl(decl) => decl.convert(ctx),
            items::BlockItem::Stmt(stmt) => stmt.convert(ctx),
        }
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::Decl {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::Decl::ConstDecl(c_decl) => c_decl.convert(ctx),
            items::Decl::VarDecl(v_decl) => v_decl.convert(ctx),
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::Decl::ConstDecl(c_decl) => c_decl.global_convert(ctx),
            items::Decl::VarDecl(v_decl) => v_decl.global_convert(ctx),
        }
    }
}

impl ToRaanaIR for items::ConstDecl {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        assert!(
            self.btype.btype.is_scalar(),
            "Unknown type for constant declaration."
        );
        ctx.set_def_type(self.btype.clone());
        for const_def in &self.const_defs {
            const_def.convert(ctx);
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        assert!(
            self.btype.btype.is_scalar(),
            "Unknown type for constant declaration."
        );
        ctx.set_def_type(self.btype.clone());
        for const_def in &self.const_defs {
            const_def.global_convert(ctx);
        }
    }
}

impl ToRaanaIR for items::ConstDef {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        let ty = ctx.curr_def_type().unwrap();
        // not an array
        if self.arr_dim.is_empty() {
            let ty = ty.btype;
            // Get the init val
            let items::ConstInitVal::Normal(_) = self.const_init_val else {
                panic!("Invalid assign: array to a integer")
            };
            self.const_init_val.convert(ctx);
            let init_val = ctx.pop_val().unwrap();
            let init_val = ctx.coerce_local(init_val, &ty);
            // Not a constant val
            // if !ctx.curr_func_data().dfg().value(init_val).kind().is_const() {
            //     panic!("Inst can't be calculated at compile time.");
            // };
            ctx.insert_const(self.ident.clone(), init_val)
        }
        // is an array
        else {
            let ty = ty.btype;
            let array_shape = self
                .arr_dim
                .iter()
                // .rev()
                .map(|const_exp| {
                    const_exp.convert(ctx);
                    ctx.pop_i32()
                })
                .collect::<Vec<_>>();

            let arr_ty = array_shape
                .iter()
                .map(|x| *x as usize)
                .rfold(ty.clone(), Type::get_array);
            let byte_len = arr_ty.size();
            let alloc_var = ctx.new_local_value().alloc(arr_ty);
            ctx.set_value_name(alloc_var, self.ident.clone());
            ctx.push_inst(alloc_var);

            if !matches!(self.const_init_val, items::ConstInitVal::Array(_)) {
                panic!("Invalid assign: integer to an array")
            }
            let clear = ctx.new_local_value().mem_zero(alloc_var, byte_len);
            ctx.push_inst(clear);
            for (flat_index, exp) in self.const_init_val.explicit_init_vals(&array_shape) {
                exp.convert(ctx);
                let value = ctx.pop_val().unwrap();
                let value = ctx.coerce_local(value, &ty);
                let dest = local_array_element_ptr(ctx, alloc_var, &array_shape, flat_index);
                ctx.push_inst(dest);
                let store = ctx.new_local_value().store(value, dest);
                ctx.push_inst(store);
            }
            ctx.insert_const(self.ident.clone(), alloc_var)
        }
    }

    fn global_convert(&self, ctx: &mut AstGenContext) {
        let ty = ctx.curr_def_type().unwrap();
        // is array
        if self.arr_dim.is_empty() && !ty.is_tensor {
            let ty = ty.btype;
            self.const_init_val.global_convert(ctx);
            let init_val = ctx.pop_val().unwrap();
            let init_val = ctx.coerce_global(init_val, &ty);
            // No more check
            ctx.insert_const(self.ident.clone(), init_val)
        }
        // not an array
        else {
            let ty = ty.btype;
            let array_shape = self
                .arr_dim
                .iter()
                .map(|const_exp| {
                    const_exp.global_convert(ctx);
                    ctx.pop_i32()
                })
                .collect::<Vec<_>>();

            if !matches!(self.const_init_val, items::ConstInitVal::Array(_)) {
                panic!("Invalid assign: integer to an array")
            };
            if self.const_init_val.is_empty_init() {
                let arr_ty = array_shape
                    .iter()
                    .map(|x| *x as usize)
                    .rfold(ty.clone(), Type::get_array);
                let init = ctx.new_global_value().zero_init(arr_ty);
                let alloc_var = ctx.new_global_value().global_alloc(init);
                ctx.set_value_name(alloc_var, self.ident.clone());
                ctx.insert_const(self.ident.clone(), alloc_var);
                return;
            }
            let exps = self.const_init_val.init_val_shape(&array_shape);
            let zero = ctx.zero_global(&ty);
            let elems = exps
                .iter()
                .map(|exp| match exp {
                    Some(exp) => {
                        exp.global_convert(ctx);
                        let val = ctx.pop_val().unwrap();
                        ctx.coerce_global(val, &ty)
                    }
                    None => zero,
                })
                .collect::<Vec<_>>();
            let agg = array_shape.iter().rev().fold(elems, |elems, &dim_l| {
                elems
                    .chunks(dim_l as _)
                    .map(|chunk| ctx.new_global_value().aggregate(chunk.to_owned()))
                    .collect::<Vec<_>>()
            });
            let init = *agg.first().unwrap();
            let alloc_var = ctx.new_global_value().global_alloc(init);
            ctx.set_value_name(alloc_var, self.ident.clone());
            ctx.insert_const(self.ident.clone(), alloc_var)
        }
    }
}

impl ToRaanaIR for items::ConstInitVal {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::ConstInitVal::Normal(const_exp) => const_exp.convert(ctx),
            items::ConstInitVal::Array(const_exps) => {
                for const_exp in const_exps {
                    const_exp.convert(ctx);
                }
            }
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::ConstInitVal::Normal(const_exp) => const_exp.global_convert(ctx),
            items::ConstInitVal::Array(const_exps) => {
                for const_exp in const_exps {
                    const_exp.global_convert(ctx);
                }
            }
        }
    }
}

impl ToRaanaIR for items::ConstExp {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        self.exp.convert(ctx)
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        self.exp.global_convert(ctx)
    }
}

impl ToRaanaIR for items::VarDecl {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        assert!(
            self.btype.btype.is_scalar(),
            "Unknown type for variable declaration"
        );
        ctx.set_def_type(self.btype.clone());
        for var_def in &self.var_defs {
            var_def.convert(ctx);
        }
    }

    fn global_convert(&self, ctx: &mut AstGenContext) {
        assert!(
            self.btype.btype.is_scalar(),
            "Unknown type for variable declaration"
        );
        ctx.set_def_type(self.btype.clone());
        for var_def in &self.var_defs {
            var_def.global_convert(ctx);
        }
    }
}

impl ToRaanaIR for items::VarDef {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        let ty = ctx.curr_def_type().unwrap();
        // Not an array
        if self.arr_dim.is_empty() && !ty.is_tensor {
            let ty = ty.btype;
            // Allocate a target type of variable.
            let alloc_var = ctx.new_local_value().alloc(ty.clone());
            ctx.set_value_name(alloc_var, self.ident.clone());
            ctx.push_inst(alloc_var);
            if let Some(ref init_val) = self.init_val {
                let items::InitVal::Normal(exp) = init_val else {
                    panic!("Invalid assign: array to a integer")
                };
                exp.convert(ctx);
                // store the calculated value.
                let val = ctx.pop_val().unwrap();
                let val = ctx.coerce_local(val, &ty);
                let store = ctx.new_local_value().store(val, alloc_var);
                ctx.push_inst(store);
            }
            ctx.insert_var(self.ident.clone(), alloc_var)
        }
        // is an array
        else {
            let ty = ty.btype;
            // for given expression like `a[x][y][z]`, we first take out each const exp in the []
            // bracket and calculated it as i32(only type we accept)
            // we calculate from `z` to `x`, and pop it from `x` to `z`.
            // then we could get the array shape [x, y, z] as Vec<i32>
            let array_shape = self
                .arr_dim
                .iter()
                .map(|const_exp| {
                    const_exp.convert(ctx);
                    ctx.pop_i32()
                })
                .collect::<Vec<_>>();

            // But for array type, we built arr[z] first, then brr[y][z], finally crr[x][y][z]
            // so we need to do it in reverse order.
            // at the end we can allocate that type and give it a name.
            let arr_ty = array_shape
                .iter()
                .map(|x| *x as usize)
                .rfold(ty.clone(), Type::get_array);
            let byte_len = arr_ty.size();
            let alloc_var = ctx.new_local_value().alloc(arr_ty);
            ctx.set_value_name(alloc_var, self.ident.clone());
            ctx.push_inst(alloc_var);

            // We handle the possible initial value.
            if let Some(ref init_val) = self.init_val {
                match init_val {
                    items::InitVal::Normal(exp) => {
                        exp.convert(ctx);
                        let val = ctx.pop_val().unwrap();
                        copy_tensor(ctx, val, alloc_var);
                    }
                    items::InitVal::Array(..) => {
                        let clear = ctx.new_local_value().mem_zero(alloc_var, byte_len);
                        ctx.push_inst(clear);
                        for (flat_index, exp) in init_val.explicit_init_vals(&array_shape) {
                            exp.convert(ctx);
                            let value = ctx.pop_val().unwrap();
                            let value = ctx.coerce_local(value, &ty);
                            let dest =
                                local_array_element_ptr(ctx, alloc_var, &array_shape, flat_index);
                            ctx.push_inst(dest);
                            let store = ctx.new_local_value().store(value, dest);
                            ctx.push_inst(store);
                        }
                    }
                }
            }
            ctx.insert_var(self.ident.clone(), alloc_var)
        }
    }

    fn global_convert(&self, ctx: &mut AstGenContext) {
        let ty = ctx.curr_def_type().unwrap();
        if self.arr_dim.is_empty() && !ty.is_tensor {
            let ty = ty.btype;
            let init_val = if let Some(ref init_val) = self.init_val {
                init_val.global_convert(ctx);
                let val = ctx.pop_val().unwrap();
                ctx.coerce_global(val, &ty)
            } else {
                ctx.new_global_value().zero_init(ty.clone())
            };
            let val = ctx.new_global_value().global_alloc(init_val);
            ctx.set_value_name(val, self.ident.clone());
            ctx.insert_var(self.ident.clone(), val)
        } else {
            let ty = ty.btype;
            let array_shape = self
                .arr_dim
                .iter()
                .map(|const_exp| {
                    const_exp.global_convert(ctx);
                    ctx.pop_i32()
                })
                .collect::<Vec<_>>();

            let arr_ty = array_shape
                .iter()
                .map(|x| *x as usize)
                .rfold(ty.clone(), Type::get_array);

            let init = if let Some(ref init_val) = self.init_val {
                if !matches!(init_val, items::InitVal::Array(_)) {
                    panic!("Invalid assign: integer to an array")
                }
                if init_val.is_empty_init() {
                    ctx.new_global_value().zero_init(arr_ty.clone())
                } else {
                    let exps = init_val.init_val_shape(&array_shape);
                    let zero = ctx.zero_global(&ty);
                    let elems = exps
                        .iter()
                        .map(|exp| match exp {
                            Some(exp) => {
                                exp.global_convert(ctx);
                                let val = ctx.pop_val().unwrap();
                                ctx.coerce_global(val, &ty)
                            }
                            None => zero,
                        })
                        .collect::<Vec<_>>();
                    let agg = array_shape.iter().rev().fold(elems, |elems, &dim_l| {
                        elems
                            .chunks(dim_l as _)
                            .map(|chunk| ctx.new_global_value().aggregate(chunk.to_owned()))
                            .collect::<Vec<_>>()
                    });
                    agg[0]
                }
            } else {
                ctx.new_global_value().zero_init(arr_ty)
            };
            let alloc_var = ctx.new_global_value().global_alloc(init);
            ctx.set_value_name(alloc_var, self.ident.clone());
            ctx.insert_var(self.ident.clone(), alloc_var)
        }
    }
}

impl ToRaanaIR for items::InitVal {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::InitVal::Normal(exp) => exp.convert(ctx),
            items::InitVal::Array(exps) => {
                for exp in exps {
                    exp.convert(ctx);
                }
            }
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::InitVal::Normal(exp) => exp.global_convert(ctx),
            items::InitVal::Array(exps) => {
                for exp in exps {
                    exp.global_convert(ctx);
                }
            }
        }
    }
}

impl ToRaanaIR for items::Stmt {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::Stmt::Assign(assign_stmt) => assign_stmt.convert(ctx),
            items::Stmt::Return(return_stmt) => return_stmt.convert(ctx),
            items::Stmt::Block(block) => block.convert(ctx),
            items::Stmt::Single(exp) => {
                if let Some(exp) = exp {
                    exp.convert(ctx);
                }
            }
            items::Stmt::IfStmt(if_stmt) => if_stmt.convert(ctx),
            items::Stmt::WhileStmt(while_stmt) => while_stmt.convert(ctx),
            items::Stmt::Break(break_stmt) => break_stmt.convert(ctx),
            items::Stmt::Continue(continue_stmt) => continue_stmt.convert(ctx),
        }
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::Break {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        let loop_end = ctx
            .curr_loop()
            .unwrap_or_else(|| panic!("Use break outside of loop"))
            .1;
        let jump_to_loop_end = ctx.new_local_value().jump(loop_end, vec![]);
        ctx.push_inst(jump_to_loop_end);
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::Continue {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        let loop_start = ctx
            .curr_loop()
            .unwrap_or_else(|| panic!("Use continue outside of loop"))
            .0;
        let jump_to_loop_start = ctx.new_local_value().jump(loop_start, vec![]);
        ctx.push_inst(jump_to_loop_start);
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::WhileStmt {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        // create 3 basic blocks for while loop
        let entry = ctx
            .new_basic_block()
            .basic_block("while_entry".into(), vec![]);
        ctx.register_bb(entry);
        let body = ctx
            .new_basic_block()
            .basic_block("while_body".into(), vec![]);
        ctx.register_bb(body);
        let end = ctx
            .new_basic_block()
            .basic_block("while_end".into(), vec![]);
        ctx.register_bb(end);
        ctx.push_loop(entry, end);

        // jump into while entry block unconditionally
        let jump_to_while_entry = ctx.new_local_value().jump(entry, vec![]);
        ctx.push_inst(jump_to_while_entry);

        ctx.set_curr_bb(entry);
        self.cond.convert(ctx);
        let cond_val = ctx.pop_val().unwrap();
        let cond_val = ctx.truthy_local(cond_val);
        let branch = ctx
            .new_local_value()
            .branch(cond_val, body, vec![], end, vec![]);
        ctx.push_inst(branch);

        ctx.set_curr_bb(body);
        self.body.convert(ctx);
        let jump = ctx.new_local_value().jump(entry, vec![]);
        ctx.push_inst(jump);

        ctx.pop_loop();
        ctx.set_curr_bb(end);
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::ReturnStmt {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        let v_ret = match &self.exp {
            Some(ret_exp) => {
                ret_exp.convert(ctx);
                let ret = ctx.pop_val().unwrap();
                let ret_ty = ctx.curr_func_ret_ty();
                Some(ctx.coerce_local(ret, &ret_ty))
            }
            None => None,
        };
        let ret = ctx.new_local_value().ret(v_ret);
        ctx.push_inst(ret);
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::IfStmt {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        // Get condition exp value.
        self.cond.convert(ctx);
        let cond_val = ctx.pop_val().unwrap();
        let cond_val = ctx.truthy_local(cond_val);
        let then_bb = ctx.new_basic_block().basic_block("then".into(), vec![]);
        ctx.register_bb(then_bb);
        let else_bb = self.else_branch.as_ref().map(|_| {
            let bb = ctx.new_basic_block().basic_block("else".into(), vec![]);
            ctx.register_bb(bb);
            bb
        });
        let end_bb = ctx.new_basic_block().basic_block("end".into(), vec![]);
        ctx.register_bb(end_bb);
        let br = ctx.new_local_value().branch(
            cond_val,
            then_bb,
            vec![],
            else_bb.unwrap_or(end_bb),
            vec![],
        );
        ctx.push_inst(br);

        ctx.set_curr_bb(then_bb);
        self.then_branch.convert(ctx);
        let then_jump = ctx.new_local_value().jump(end_bb, vec![]);
        ctx.push_inst(then_jump);

        if let Some(else_bb) = else_bb {
            ctx.set_curr_bb(else_bb);
            self.else_branch.as_ref().unwrap().convert(ctx);
            let else_jump = ctx.new_local_value().jump(end_bb, vec![]);
            ctx.push_inst(else_jump);
        }

        ctx.set_curr_bb(end_bb);
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::AssignStmt {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        if ctx.is_constant(&self.l_val) {
            panic!("Can't modify a constant");
        }
        self.l_val.convert(ctx);
        let lhs_l_val = ctx.pop_val().unwrap();
        self.exp.convert(ctx);

        // Compile time type-check.
        let lhs_ptr_type = ctx.new_local_value().inst_type(lhs_l_val);
        let lhs_type = lhs_ptr_type.derefernce();
        if !lhs_type.is_scalar() {
            // is tensor
            let val = ctx.pop_val().unwrap();
            copy_tensor(ctx, val, lhs_l_val);
        } else {
            let rhs_exp = ctx.pop_val().unwrap();
            let rhs_exp = ctx.coerce_local(rhs_exp, &lhs_type);
            let rhs_exp_type = ctx.new_local_value().inst_type(rhs_exp);
            assert!(
                Type::get_pointer(rhs_exp_type.clone()) == lhs_ptr_type.clone(),
                "Type not match. {rhs_exp_type} can't store in {lhs_ptr_type}"
            );
            let store = ctx.new_local_value().store(rhs_exp, lhs_l_val);
            ctx.push_inst(store);
        }
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::Exp {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        self.lor_exp.convert(ctx)
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        self.lor_exp.global_convert(ctx)
    }
}

impl ToRaanaIR for items::LOrExp {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::LOrExp::LAndExp(land_exp) => land_exp.convert(ctx),
            items::LOrExp::Comp(lor_exp, land_exp) => {
                // handle lhs
                lor_exp.convert(ctx);
                let lhs = ctx.pop_val().unwrap();

                let lhs_ne_0 = ctx.truthy_local(lhs);

                // two basic block for short circuit logic
                let rhs_bb = ctx.new_basic_block().basic_block("lor_rhs".into(), vec![]);
                ctx.register_bb(rhs_bb);
                let merge_bb = ctx
                    .new_basic_block()
                    .basic_block("lor_merge".into(), vec![Type::get_i32()]);
                ctx.register_bb(merge_bb);

                // short circuit logic
                let br = ctx.new_local_value().branch(
                    lhs_ne_0,
                    merge_bb,
                    vec![lhs_ne_0],
                    rhs_bb,
                    vec![],
                );
                ctx.push_inst(br);

                // check rhs
                let original = ctx.set_curr_bb(rhs_bb).unwrap();
                land_exp.convert(ctx);
                let rhs = ctx.pop_val().unwrap();

                // Constant folding
                let lhs_const_truthy = ctx
                    .as_i32(lhs)
                    .map(|v| v != 0)
                    .or_else(|| ctx.as_f32(lhs).map(|v| v != 0.0));
                let rhs_const_truthy = ctx
                    .as_i32(rhs)
                    .map(|v| v != 0)
                    .or_else(|| ctx.as_f32(rhs).map(|v| v != 0.0));
                if let (Some(lhs_truthy), Some(rhs_truthy)) = (lhs_const_truthy, rhs_const_truthy) {
                    ctx.set_curr_bb(original);
                    ctx.remove_inst(br);
                    ctx.remove_bb(rhs_bb);
                    ctx.remove_bb(merge_bb);
                    let result = ctx
                        .new_local_value()
                        .integer((lhs_truthy || rhs_truthy) as _);
                    ctx.push_val(result);
                    return;
                }

                let rhs_ne_0 = ctx.truthy_local(rhs);

                // jump to the merge block and pass the information
                let jump = ctx.new_local_value().jump(merge_bb, vec![rhs_ne_0]);
                ctx.push_inst(jump);

                ctx.set_curr_bb(merge_bb);
                let result = ctx.bb_params(merge_bb)[0];
                ctx.push_val(result);
            }
        }
    }

    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::LOrExp::LAndExp(land_exp) => land_exp.global_convert(ctx),
            items::LOrExp::Comp(lor_exp, land_exp) => {
                lor_exp.global_convert(ctx);
                let lhs_val = ctx.pop_val().unwrap();
                let lhs_int = ctx
                    .as_i32(lhs_val)
                    .unwrap_or_else(|| (ctx.as_f32(lhs_val).unwrap() != 0.0) as i32);
                land_exp.global_convert(ctx);
                let rhs_val = ctx.pop_val().unwrap();
                let rhs_int = ctx
                    .as_i32(rhs_val)
                    .unwrap_or_else(|| (ctx.as_f32(rhs_val).unwrap() != 0.0) as i32);
                let or_result = ctx
                    .program
                    .new_value()
                    .integer((lhs_int != 0 || rhs_int != 0) as i32);
                ctx.push_val(or_result);
            }
        }
    }
}

impl ToRaanaIR for items::LAndExp {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::LAndExp::EqExp(eq_exp) => eq_exp.convert(ctx),
            items::LAndExp::Comp(land_exp, eq_exp) => {
                // handle lhs
                land_exp.convert(ctx);
                let lhs = ctx.pop_val().unwrap();

                let zero = ctx.new_local_value().integer(0);
                let lhs_ne_0 = ctx.truthy_local(lhs);

                // two basic block for short circuit logic
                let rhs_bb = ctx.new_basic_block().basic_block("land_rhs".into(), vec![]);
                ctx.register_bb(rhs_bb);
                let merge_bb = ctx
                    .new_basic_block()
                    .basic_block("land_merge".into(), vec![Type::get_i32()]);
                ctx.register_bb(merge_bb);

                //short circuit logic
                let br =
                    ctx.new_local_value()
                        .branch(lhs_ne_0, rhs_bb, vec![], merge_bb, vec![zero]);
                ctx.push_inst(br);

                // check rhs
                let original = ctx.set_curr_bb(rhs_bb).unwrap();
                eq_exp.convert(ctx);
                let rhs = ctx.pop_val().unwrap();

                // Constant folding
                let lhs_const_truthy = ctx
                    .as_i32(lhs)
                    .map(|v| v != 0)
                    .or_else(|| ctx.as_f32(lhs).map(|v| v != 0.0));
                let rhs_const_truthy = ctx
                    .as_i32(rhs)
                    .map(|v| v != 0)
                    .or_else(|| ctx.as_f32(rhs).map(|v| v != 0.0));
                if let (Some(lhs_truthy), Some(rhs_truthy)) = (lhs_const_truthy, rhs_const_truthy) {
                    ctx.set_curr_bb(original);
                    ctx.remove_inst(br);
                    ctx.remove_bb(rhs_bb);
                    ctx.remove_bb(merge_bb);
                    let result = ctx
                        .new_local_value()
                        .integer((lhs_truthy && rhs_truthy) as _);
                    ctx.push_val(result);
                    return;
                }

                let rhs_ne_0 = ctx.truthy_local(rhs);

                // jump to merge block and pass the information
                let jump = ctx.new_local_value().jump(merge_bb, vec![rhs_ne_0]);
                ctx.push_inst(jump);

                ctx.set_curr_bb(merge_bb);
                let result = ctx.bb_params(merge_bb)[0];
                ctx.push_val(result);
            }
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::LAndExp::EqExp(eq_exp) => eq_exp.global_convert(ctx),
            items::LAndExp::Comp(land_exp, eq_exp) => {
                land_exp.global_convert(ctx);
                let lhs_val = ctx.pop_val().unwrap();
                let lhs_int = ctx
                    .as_i32(lhs_val)
                    .unwrap_or_else(|| (ctx.as_f32(lhs_val).unwrap() != 0.0) as i32);
                eq_exp.global_convert(ctx);
                let rhs_val = ctx.pop_val().unwrap();
                let rhs_int = ctx
                    .as_i32(rhs_val)
                    .unwrap_or_else(|| (ctx.as_f32(rhs_val).unwrap() != 0.0) as i32);
                let and_result = ctx
                    .program
                    .new_value()
                    .integer((lhs_int != 0 && rhs_int != 0) as i32);
                ctx.push_val(and_result);
            }
        }
    }
}

impl ToRaanaIR for items::EqExp {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::EqExp::RelExp(rel_exp) => rel_exp.convert(ctx),
            items::EqExp::Comp(lhs_eq, op, rhs_rel) => {
                lhs_eq.convert(ctx);
                rhs_rel.convert(ctx);
                op.convert(ctx)
            }
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::EqExp::RelExp(rel_exp) => rel_exp.global_convert(ctx),
            items::EqExp::Comp(eq_exp, binary_op, rel_exp) => {
                eq_exp.global_convert(ctx);
                rel_exp.global_convert(ctx);
                binary_op.global_convert(ctx)
            }
        }
    }
}

impl ToRaanaIR for items::RelExp {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::RelExp::AddExp(add_exp) => add_exp.convert(ctx),
            items::RelExp::Comp(lhs_rel, op, rhs_add) => {
                lhs_rel.convert(ctx);
                rhs_add.convert(ctx);
                op.convert(ctx)
            }
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::RelExp::AddExp(add_exp) => add_exp.global_convert(ctx),
            items::RelExp::Comp(rel_exp, binary_op, add_exp) => {
                rel_exp.global_convert(ctx);
                add_exp.global_convert(ctx);
                binary_op.global_convert(ctx)
            }
        }
    }
}

impl ToRaanaIR for items::AddExp {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::AddExp::MulExp(mul_exp) => mul_exp.convert(ctx),
            items::AddExp::Comp(lhs_add, op, rhs_mul) => {
                lhs_add.convert(ctx);
                rhs_mul.convert(ctx);
                op.convert(ctx)
            }
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::AddExp::MulExp(mul_exp) => mul_exp.global_convert(ctx),
            items::AddExp::Comp(add_exp, binary_op, mul_exp) => {
                add_exp.global_convert(ctx);
                mul_exp.global_convert(ctx);
                binary_op.global_convert(ctx)
            }
        }
    }
}

impl ToRaanaIR for items::MulExp {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::MulExp::UnaryExp(unary_exp) => unary_exp.convert(ctx),
            items::MulExp::Comp(lhs_mul, op, rhs_unary) => {
                lhs_mul.convert(ctx);
                rhs_unary.convert(ctx);
                op.convert(ctx)
            }
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::MulExp::UnaryExp(unary_exp) => unary_exp.global_convert(ctx),
            items::MulExp::Comp(mul_exp, binary_op, unary_exp) => {
                mul_exp.global_convert(ctx);
                unary_exp.global_convert(ctx);
                binary_op.global_convert(ctx)
            }
        }
    }
}

impl ToRaanaIR for items::UnaryExp {
    #[inline]
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::UnaryExp::PrimaryExp(exp) => exp.convert(ctx),
            items::UnaryExp::Unary(unary_op, unary_exp) => {
                unary_exp.convert(ctx);
                unary_op.convert(ctx)
            }
            items::UnaryExp::FuncCall(func_call) => func_call.convert(ctx),
        }
    }

    #[inline]
    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::UnaryExp::PrimaryExp(primary_exp) => primary_exp.global_convert(ctx),
            items::UnaryExp::Unary(unary_op, unary_exp) => {
                unary_exp.global_convert(ctx);
                unary_op.global_convert(ctx)
            }
            items::UnaryExp::FuncCall(_) => panic!("Const function is not supported"),
        }
    }
}

impl ToRaanaIR for items::FuncCall {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }

        // handle the macro in the header
        if (self.ident == "starttime".into() || self.ident == "stoptime".into())
            && self.args.is_empty()
        {
            let func_name = format!("_sysy_{}", &*self.ident);
            let func = Ident::from(func_name);
            let Symbol::Callable(target_func) = ctx
                .get_global(&func)
                .unwrap_or_else(|| panic!("Can't find function {}", &*func))
            else {
                panic!("Not a function {}", &*self.ident)
            };
            // mock the line number for the macro function call, since we don't hold a line number.
            let args = vec![ctx.new_local_value().integer(0)];
            let call = ctx.new_local_value().call(target_func, args);
            ctx.push_inst(call);
            return;
        }

        let Symbol::Callable(target_func) = ctx
            .get_global(&self.ident)
            .unwrap_or_else(|| panic!("Can't find function {}", &*self.ident))
        else {
            panic!("Not a function {}", &*self.ident)
        };
        let param_tys = ctx.func_param_tys(target_func);
        let args = self
            .args
            .iter()
            .zip(param_tys.iter())
            .map(|(exp, param_ty)| {
                exp.convert(ctx);
                let arg = ctx.pop_val().unwrap();
                let arg = if ctx.is_pointer_to_array(arg) {
                    let zero = ctx.new_local_value().integer(0);
                    let get_elem_ptr = ctx.new_local_value().get_elem_ptr(arg, vec![zero, zero]);
                    ctx.push_inst(get_elem_ptr);
                    get_elem_ptr
                } else {
                    arg
                };
                let arg_ty = ctx.new_local_value().inst_type(arg);
                if arg_ty.is_scalar() && param_ty.is_scalar() {
                    ctx.coerce_local(arg, param_ty)
                } else {
                    arg
                }
            })
            .collect::<Vec<_>>();

        let ret_ty = ctx.func_data(target_func).ret_ty().clone();
        let call = ctx.new_local_value().call(target_func, args);
        ctx.push_inst(call);
        if ret_ty.is_tensor() {
            let temp = ctx.new_local_value().alloc(ret_ty.derefernce());
            ctx.push_inst(temp);
            copy_tensor(ctx, call, temp);
            ctx.push_val(temp);
        } else if !ctx.inst_data(call).ty().is_unit() {
            ctx.push_val(call);
        }
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        unreachable!("No corresponding syntax")
    }
}

impl ToRaanaIR for items::PrimaryExp {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        match self {
            items::PrimaryExp::Exp(exp) => exp.convert(ctx),
            items::PrimaryExp::Number(num) => {
                let val = match num {
                    items::Number::Int(num) => ctx.new_local_value().integer(*num),
                    items::Number::Float(num) => ctx.new_local_value().float(*num),
                };
                ctx.push_val(val);
            }
            // LVal on the right side.
            // Meaning it's not defining but using a variable.
            // We take the value and push to value stack to use.
            items::PrimaryExp::LVal(l_val) => {
                // not a array
                if l_val.index.is_empty() {
                    match ctx.get_symbol(&l_val.ident).unwrap() {
                        Symbol::Constant(const_val) => {
                            let val = ctx.as_local_const_val(const_val);
                            ctx.push_val(val);
                        }
                        Symbol::Variable(var_ptr) => {
                            if ctx.is_pointer_to_array(var_ptr) {
                                ctx.push_val(var_ptr);
                            } else {
                                let load = ctx.new_local_value().load(var_ptr);
                                ctx.push_inst(load);
                                ctx.push_val(load);
                            }
                        }
                        Symbol::Callable(..) => {
                            panic!("You might forget to call the function.")
                        }
                    }
                }
                // visiting an array
                else {
                    let offsets = l_val
                        .index
                        .iter()
                        .map(|x| {
                            x.convert(ctx);
                            ctx.pop_val().unwrap()
                        })
                        .collect::<Vec<_>>();
                    match ctx.get_symbol(&l_val.ident).unwrap() {
                        Symbol::Constant(array) | Symbol::Variable(array) => {
                            let array_ty = ctx.inst_data(array).ty().clone();
                            let (base, gep_offsets) = if ctx.is_pointer_to_array(array) {
                                let mut gep_offset = vec![ctx.new_local_value().integer(0)];
                                gep_offset.extend(offsets);
                                (array, gep_offset)
                            } else if array_ty.is_pointer() && array_ty.derefernce().is_pointer() {
                                let load = ctx.new_local_value().load(array);
                                ctx.push_inst(load);
                                (load, offsets)
                            } else {
                                (array, offsets)
                            };
                            let get_from = ctx.new_local_value().get_elem_ptr(base, gep_offsets);
                            ctx.push_inst(get_from);
                            if ctx.is_pointer_to_array(get_from) {
                                ctx.push_val(get_from);
                            } else {
                                let load = ctx.new_local_value().load(get_from);
                                ctx.push_inst(load);
                                ctx.push_val(load);
                            }
                        }
                        Symbol::Callable(_function) => panic!("Function can not be indexed."),
                    }
                }
            }
        }
    }

    fn global_convert(&self, ctx: &mut AstGenContext) {
        match self {
            items::PrimaryExp::Exp(exp) => exp.global_convert(ctx),
            items::PrimaryExp::LVal(lval) => {
                let sym = *ctx
                    .global_scope()
                    .get(&lval.ident)
                    .unwrap_or_else(|| panic!("{} not defined", &*lval.ident));
                let val = match sym {
                    Symbol::Constant(val) => val,
                    Symbol::Variable(var) => {
                        let borrow_value = ctx.inst_data(var);
                        let InstKind::GlobalAlloc(glob_alloc) = borrow_value.kind() else {
                            unreachable!();
                        };
                        match ctx.inst_data(glob_alloc.init()).kind().clone() {
                            InstKind::Integer(int) => ctx.new_global_value().integer(int.value()),
                            InstKind::Float(float) => ctx.new_global_value().float(float.value()),
                            InstKind::ZeroInit => {
                                let ty = ctx.inst_data(glob_alloc.init()).ty().clone();
                                ctx.zero_global(&ty)
                            }
                            _ => unreachable!(),
                        }
                    }
                    Symbol::Callable(_) => unreachable!(),
                };
                ctx.push_val(val);
            }
            items::PrimaryExp::Number(num) => {
                let num_lit = match num {
                    items::Number::Int(num) => ctx.new_global_value().integer(*num),
                    items::Number::Float(num) => ctx.new_global_value().float(*num),
                };
                ctx.push_val(num_lit);
            }
        }
    }
}

impl ToRaanaIR for items::LVal {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        let symbol = ctx
            .get_symbol(&self.ident)
            .unwrap_or_else(|| panic!("Variable {} not exists.", &*self.ident));
        let val = match symbol {
            Symbol::Constant(const_val) => panic!("Cannot modify a constant {const_val:?}"),
            Symbol::Variable(p_val) => {
                if self.index.is_empty() {
                    p_val
                } else {
                    let indices = self
                        .index
                        .iter()
                        .map(|exp| {
                            exp.convert(ctx);
                            ctx.pop_val().unwrap()
                        })
                        .collect::<Vec<_>>();
                    let var_ty = ctx.inst_data(p_val).ty().clone();
                    let (base, gep_indices) = if ctx.is_pointer_to_array(p_val) {
                        let mut gep_offsets = vec![ctx.new_local_value().integer(0)];
                        gep_offsets.extend(indices);
                        (p_val, gep_offsets)
                    } else if var_ty.is_pointer() && var_ty.derefernce().is_pointer() {
                        let load = ctx.new_local_value().load(p_val);
                        ctx.push_inst(load);
                        (load, indices)
                    } else {
                        (p_val, indices)
                    };
                    let get = ctx.new_local_value().get_elem_ptr(base, gep_indices);
                    ctx.push_inst(get);
                    get
                }
            }
            Symbol::Callable(func_handle) => {
                panic!("Cannot assign a value to a function {func_handle:?}")
            }
        };
        ctx.push_val(val);
    }

    fn global_convert(&self, _ctx: &mut AstGenContext) {
        panic!("No corresponding syntax")
    }
}

impl ToRaanaIR for BinaryOp {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        let rhs = ctx.pop_val().unwrap();
        let lhs = ctx.pop_val().unwrap();
        let lhs_ty = ctx.inst_data(lhs).ty().clone();
        let rhs_ty = ctx.inst_data(rhs).ty().clone();

        let lhs_tensor = lhs_ty.is_tensor();
        let rhs_tensor = rhs_ty.is_tensor();

        // tensor type arith
        if lhs_tensor || rhs_tensor {
            let result = match self {
                BinaryOp::MatMul => lower_matmul_native_loop(ctx, lhs, rhs),
                _ => lower_elementwise_native_loop(ctx, lhs, rhs, *self, lhs_tensor, rhs_tensor),
            };
            ctx.push_val(result);
            return;
        }

        let use_float = lhs_ty.is_f32() || rhs_ty.is_f32();

        if use_float {
            assert!(
                !binary_requires_int(*self),
                "Integer-only binary operator used with float operand"
            );
            let lhs = ctx.coerce_local(lhs, &Type::get_f32());
            let rhs = ctx.coerce_local(rhs, &Type::get_f32());
            if let (Some(lhs), Some(rhs)) = (ctx.as_f32(lhs), ctx.as_f32(rhs)) {
                let val = match eval_f32_binary(*self, lhs, rhs) {
                    items::Number::Int(value) => ctx.new_local_value().integer(value),
                    items::Number::Float(value) => ctx.new_local_value().float(value),
                };
                ctx.push_val(val);
                return;
            }
            let operation = ctx.new_local_value().binary(*self, lhs, rhs);
            ctx.push_val(operation);
            ctx.push_inst(operation);
            return;
        }

        if let (Some(lhs), Some(rhs)) = (ctx.as_i32(lhs), ctx.as_i32(rhs)) {
            let val = ctx
                .new_local_value()
                .integer(eval_i32_binary(*self, lhs, rhs));
            ctx.push_val(val);
            return;
        }
        let operation = ctx.new_local_value().binary(*self, lhs, rhs);
        ctx.push_val(operation);
        ctx.push_inst(operation);
    }

    fn global_convert(&self, ctx: &mut AstGenContext) {
        let rhs = ctx.pop_val().unwrap();
        let lhs = ctx.pop_val().unwrap();
        let lhs_ty = ctx.inst_data(lhs).ty().clone();
        let rhs_ty = ctx.inst_data(rhs).ty().clone();
        if lhs_ty.is_f32() || rhs_ty.is_f32() {
            assert!(
                !binary_requires_int(*self),
                "Integer-only binary operator used with float operand"
            );
            let lhs = ctx.coerce_global(lhs, &Type::get_f32());
            let rhs = ctx.coerce_global(rhs, &Type::get_f32());
            let lhs = ctx.as_f32(lhs).unwrap();
            let rhs = ctx.as_f32(rhs).unwrap();
            let val = match eval_f32_binary(*self, lhs, rhs) {
                items::Number::Int(value) => ctx.new_global_value().integer(value),
                items::Number::Float(value) => ctx.new_global_value().float(value),
            };
            ctx.push_val(val);
        } else {
            let lhs = ctx.as_i32(lhs).unwrap();
            let rhs = ctx.as_i32(rhs).unwrap();
            let val = ctx
                .new_global_value()
                .integer(eval_i32_binary(*self, lhs, rhs));
            ctx.push_val(val);
        }
    }
}

fn is_tensor(ctx: &mut AstGenContext, tensor: Inst) -> bool {
    ctx.inst_data(tensor).ty().is_tensor()
}

fn tensor_shape_type(ctx: &mut AstGenContext, tensor: Inst) -> Type {
    assert!(is_tensor(ctx, tensor));
    let ty = ctx.inst_data(tensor).ty().clone();
    if ty.is_pointer() { ty.derefernce() } else { ty }
}

fn get_type_from_shape(base_ty: Type, shape: &[usize]) -> Type {
    let mut ty = base_ty;
    for &len in shape.iter().rev() {
        ty = Type::get_array(ty, len);
    }
    ty
}

fn copy_tensor(ctx: &mut AstGenContext, src: Inst, dst: Inst) {
    let src_ty = tensor_shape_type(ctx, src);
    let dst_ty = tensor_shape_type(ctx, dst);
    let lhs_shape = src_ty.get_array_shape();
    let rhs_shape = dst_ty.get_array_shape();
    assert!(lhs_shape == rhs_shape);
    tensor_for_each_loopify(ctx, dst, |ctx, idxs| {
        let s = tensor_get_elem_raw(ctx, src, idxs);
        let d = tensor_elem_ptr_raw(ctx, dst, idxs);
        let store = ctx.new_local_value().store(s, d);
        ctx.push_inst(store);
    });
}

fn lower_matmul_native_loop(ctx: &mut AstGenContext, lhs: Inst, rhs: Inst) -> Inst {
    let lhs_ty = tensor_shape_type(ctx, lhs);
    let rhs_ty = tensor_shape_type(ctx, rhs);
    assert!(lhs_ty.array_base_scalar_type() == rhs_ty.array_base_scalar_type());

    let base_ty = lhs_ty.array_base_scalar_type();
    let lhs_shape = lhs_ty.get_array_shape();
    let rhs_shape = rhs_ty.get_array_shape();

    assert!(lhs_shape.len() == 2, "lhs must have rank 2");
    assert!(rhs_shape.len() == 2, "rhs must have rank 2");
    assert!(lhs_shape[1] == rhs_shape[0]);

    let &[a, _] = lhs_shape.as_slice() else {
        unreachable!()
    };
    let &[b, c] = rhs_shape.as_slice() else {
        unreachable!()
    };

    let result_ty = get_type_from_shape(base_ty.clone(), &[a, c]);
    let ans = ctx.new_local_value().alloc(result_ty);
    ctx.push_inst(ans);

    // i k j ->-> a c b

    let (entry, body, end) = create_loop_blocks(ctx);

    // jump into while entry block unconditionally, with zero-init induction variable.
    let zero_init_induction_variable = ctx.new_local_value().integer(0);
    let jump_to_while_entry = ctx
        .new_local_value()
        .jump(entry, vec![zero_init_induction_variable]);
    ctx.push_inst(jump_to_while_entry);

    // manual set the entry
    // It only contains a comparison with tensor length limit.
    ctx.set_curr_bb(entry);
    let entry_params = ctx.bb_params(entry);
    assert!(entry_params.len() == 1);
    let i = entry_params[0];

    let target = ctx.new_local_value().integer(a as i32);
    let cond_val = ctx.new_local_value().binary(BinaryOp::Lt, i, target);
    ctx.push_inst(cond_val);
    let branch = ctx
        .new_local_value()
        .branch(cond_val, body, vec![], end, vec![]);
    ctx.push_inst(branch);

    ctx.set_curr_bb(body);

    // should start recursion here

    {
        let (entry, body, end) = create_loop_blocks(ctx);

        // jump into while entry block unconditionally, with zero-init induction variable.
        let zero_init_induction_variable = ctx.new_local_value().integer(0);
        let jump_to_while_entry = ctx
            .new_local_value()
            .jump(entry, vec![zero_init_induction_variable]);
        ctx.push_inst(jump_to_while_entry);

        // manual set the entry
        // It only contains a comparison with tensor length limit.
        ctx.set_curr_bb(entry);
        let entry_params = ctx.bb_params(entry);
        assert!(entry_params.len() == 1);
        let k = entry_params[0];

        let target = ctx.new_local_value().integer(c as i32);
        let cond_val = ctx.new_local_value().binary(BinaryOp::Lt, k, target);
        ctx.push_inst(cond_val);
        let branch = ctx
            .new_local_value()
            .branch(cond_val, body, vec![], end, vec![]);
        ctx.push_inst(branch);

        ctx.set_curr_bb(body);

        // should start recursion here
        {
            let (entry, body, end) = {
                let entry = ctx.new_basic_block().basic_block(
                    "while_entry_tensor_elementwise".into(),
                    vec![Type::get_i32(), Type::get_i32()],
                );
                ctx.register_bb(entry);
                let body = ctx
                    .new_basic_block()
                    .basic_block("while_body_tensor_elementwise".into(), vec![]);
                ctx.register_bb(body);
                let end = ctx
                    .new_basic_block()
                    .basic_block("while_end_tensor_elementwise".into(), vec![]);
                ctx.register_bb(end);
                ctx.push_loop(entry, end);
                (entry, body, end)
            };

            // jump into while entry block unconditionally, with zero-init induction variable.
            let zero_init_induction_variable = ctx.new_local_value().integer(0);
            let acc = ctx.new_local_value().integer(0);
            let jump_to_while_entry = ctx
                .new_local_value()
                .jump(entry, vec![zero_init_induction_variable, acc]);
            ctx.push_inst(jump_to_while_entry);

            // manual set the entry
            // It only contains a comparison with tensor length limit.
            ctx.set_curr_bb(entry);
            let entry_params = ctx.bb_params(entry);
            assert!(entry_params.len() == 2);
            let j = entry_params[0];
            let acc = entry_params[1];

            let target = ctx.new_local_value().integer(b as i32);
            let cond_val = ctx.new_local_value().binary(BinaryOp::Lt, j, target);
            ctx.push_inst(cond_val);
            let branch = ctx
                .new_local_value()
                .branch(cond_val, body, vec![], end, vec![]);
            ctx.push_inst(branch);

            ctx.set_curr_bb(body);

            // main content of matmul
            let lhs = tensor_get_elem_raw(ctx, lhs, vec![i, j].as_slice());
            let rhs = tensor_get_elem_raw(ctx, rhs, vec![j, k].as_slice());
            let mul = ctx.new_local_value().binary(BinaryOp::Mul, lhs, rhs);
            ctx.push_inst(mul);

            // add the while a unconditional jump to entry to have a comparison.
            let one = ctx.new_local_value().integer(1);
            let add = ctx.new_local_value().binary(BinaryOp::Add, j, one);
            ctx.push_inst(add);
            let add_acc = ctx.new_local_value().binary(BinaryOp::Add, acc, mul);
            let jump = ctx.new_local_value().jump(entry, vec![add, add_acc]);
            ctx.push_inst(jump);

            ctx.pop_loop();
            ctx.set_curr_bb(end);

            let target_res = tensor_elem_ptr_raw(ctx, ans, vec![i, k].as_slice());
            let store = ctx.new_local_value().store(acc, target_res);
            ctx.push_inst(store);
        }

        // add the while a unconditional jump to entry to have a comparison.
        let one = ctx.new_local_value().integer(1);
        let add = ctx.new_local_value().binary(BinaryOp::Add, k, one);
        ctx.push_inst(add);
        let jump = ctx.new_local_value().jump(entry, vec![add]);
        ctx.push_inst(jump);

        ctx.pop_loop();
        ctx.set_curr_bb(end);
    }
    // add the while a unconditional jump to entry to have a comparison.
    let one = ctx.new_local_value().integer(1);
    let add = ctx.new_local_value().binary(BinaryOp::Add, i, one);
    ctx.push_inst(add);
    let jump = ctx.new_local_value().jump(entry, vec![add]);
    ctx.push_inst(jump);

    ctx.pop_loop();
    ctx.set_curr_bb(end);
    ans
}

fn lower_matmul(ctx: &mut AstGenContext, lhs: Inst, rhs: Inst) -> Inst {
    let lhs_ty = tensor_shape_type(ctx, lhs);
    let rhs_ty = tensor_shape_type(ctx, rhs);
    assert!(lhs_ty.array_base_scalar_type() == rhs_ty.array_base_scalar_type());

    let base_ty = lhs_ty.array_base_scalar_type();
    let lhs_shape = lhs_ty.get_array_shape();
    let rhs_shape = rhs_ty.get_array_shape();

    assert!(lhs_shape.len() == 2, "lhs must have rank 2");
    assert!(rhs_shape.len() == 2, "rhs must have rank 2");
    assert!(lhs_shape[1] == rhs_shape[0]);

    let &[a, _] = lhs_shape.as_slice() else {
        unreachable!()
    };
    let &[b, c] = rhs_shape.as_slice() else {
        unreachable!()
    };

    let result_ty = get_type_from_shape(base_ty.clone(), &[a, c]);
    let ans = ctx.new_local_value().alloc(result_ty);
    ctx.push_inst(ans);

    let mut i = 0;
    while i < a {
        let flag_i = i + 1 < a;

        let mut k = 0;
        while k < c {
            let flag_k = k + 1 < c;

            let (mut c00, mut c01, mut c10, mut c11) = (None, None, None, None);

            for j in 0..b {
                let l0 = tensor_get_elem(ctx, lhs, vec![i, j].as_slice());
                let r0 = tensor_get_elem(ctx, rhs, vec![j, k].as_slice());

                c00 = mul_then_acc(ctx, c00, l0, r0);
                if flag_i {
                    let l1 = tensor_get_elem(ctx, lhs, vec![i + 1, j].as_slice());
                    c10 = mul_then_acc(ctx, c10, l1, r0);
                    if flag_k {
                        let r1 = tensor_get_elem(ctx, rhs, vec![j, k + 1].as_slice());
                        c01 = mul_then_acc(ctx, c01, l0, r1);
                        c11 = mul_then_acc(ctx, c11, l1, r1);
                    }
                } else if flag_k {
                    let r1 = tensor_get_elem(ctx, rhs, vec![j, k + 1].as_slice());
                    c01 = mul_then_acc(ctx, c01, l0, r1);
                }
            }

            store(ctx, ans, i, k, c00.unwrap());
            if flag_i {
                store(ctx, ans, i + 1, k, c10.unwrap());
            }
            if flag_k {
                store(ctx, ans, i, k + 1, c01.unwrap());
            }
            if flag_i && flag_k {
                store(ctx, ans, i + 1, k + 1, c11.unwrap());
            }

            k += 2;
        }
        i += 2;
    }
    ans
}

/// dest += l * r
fn mul_then_acc(ctx: &mut AstGenContext, dest: Option<Inst>, l: Inst, r: Inst) -> Option<Inst> {
    let mul = ctx.new_local_value().binary(BinaryOp::Mul, l, r);
    ctx.push_inst(mul);
    match dest {
        None => Some(mul),
        Some(acc) => {
            let add = ctx.new_local_value().binary(BinaryOp::Add, acc, mul);
            ctx.push_inst(add);
            Some(add)
        }
    }
}

/// dest[i][k] += src
fn store(ctx: &mut AstGenContext, dest: Inst, i: usize, k: usize, src: Inst) {
    let res = tensor_elem_ptr(ctx, dest, vec![i, k].as_slice());
    let store = ctx.new_local_value().store(src, res);
    ctx.push_inst(store);
}

fn tensor_for_each_loopify(
    ctx: &mut AstGenContext,
    tensor: Inst,
    mut f: impl FnMut(&mut AstGenContext, &[Inst]),
) {
    fn rec(
        ctx: &mut AstGenContext,
        shape: &[usize],
        dep: usize,
        idxs: &mut Vec<Inst>,
        f: &mut impl FnMut(&mut AstGenContext, &[Inst]),
    ) {
        if dep == shape.len() {
            f(ctx, idxs);
            return;
        }
        let (entry, body, end) = create_loop_blocks(ctx);

        // jump into while entry block unconditionally, with zero-init induction variable.
        let zero_init_induction_variable = ctx.new_local_value().integer(0);
        let jump_to_while_entry = ctx
            .new_local_value()
            .jump(entry, vec![zero_init_induction_variable]);
        ctx.push_inst(jump_to_while_entry);

        // manual set the entry
        // It only contains a comparison with tensor length limit.
        ctx.set_curr_bb(entry);
        let entry_params = ctx.bb_params(entry);
        assert!(entry_params.len() == 1);
        let idx_var = entry_params[0];
        idxs.push(idx_var);

        let target = ctx.new_local_value().integer(shape[dep] as i32);
        let cond_val = ctx.new_local_value().binary(BinaryOp::Lt, idx_var, target);
        ctx.push_inst(cond_val);
        let branch = ctx
            .new_local_value()
            .branch(cond_val, body, vec![], end, vec![]);
        ctx.push_inst(branch);

        ctx.set_curr_bb(body);

        // should start recursion here
        rec(ctx, shape, dep + 1, idxs, f);

        // add the while a unconditional jump to entry to have a comparison.
        let one = ctx.new_local_value().integer(1);
        let add = ctx.new_local_value().binary(BinaryOp::Add, idx_var, one);
        ctx.push_inst(add);
        let jump = ctx.new_local_value().jump(entry, vec![add]);
        ctx.push_inst(jump);

        ctx.pop_loop();
        ctx.set_curr_bb(end);
    }
    let shape = tensor_shape_type(ctx, tensor).get_array_shape();
    let mut idxs = vec![];
    rec(ctx, &shape, 0, &mut idxs, &mut f);
}

fn tensor_for_each(
    ctx: &mut AstGenContext,
    tensor: Inst,
    mut f: impl FnMut(&mut AstGenContext, &[usize]),
) {
    let array_shape = tensor_shape_type(ctx, tensor).get_array_shape();
    fn rec(
        ctx: &mut AstGenContext,
        array_shape: &[usize],
        idxs: &mut [usize],
        dep: usize,
        f: &mut impl FnMut(&mut AstGenContext, &[usize]),
    ) {
        if dep == idxs.len() {
            f(ctx, idxs);
            return;
        }
        while idxs[dep] < array_shape[dep] {
            rec(ctx, array_shape, idxs, dep + 1, f);
            idxs[dep] += 1;
        }
        idxs[dep] = 0;
    }
    let mut idxs = vec![0; array_shape.len()];
    rec(ctx, &array_shape, &mut idxs, 0, &mut f);
}

fn create_loop_blocks(ctx: &mut AstGenContext) -> (BasicBlock, BasicBlock, BasicBlock) {
    let entry = ctx.new_basic_block().basic_block(
        "while_entry_tensor_elementwise".into(),
        vec![Type::get_i32()],
    );
    ctx.register_bb(entry);
    let body = ctx
        .new_basic_block()
        .basic_block("while_body_tensor_elementwise".into(), vec![]);
    ctx.register_bb(body);
    let end = ctx
        .new_basic_block()
        .basic_block("while_end_tensor_elementwise".into(), vec![]);
    ctx.register_bb(end);
    ctx.push_loop(entry, end);
    (entry, body, end)
}

fn lower_elementwise_native_loop(
    ctx: &mut AstGenContext,
    lhs: Inst,
    rhs: Inst,
    op: BinaryOp,
    lhs_tensor: bool,
    rhs_tensor: bool,
) -> Inst {
    #[allow(clippy::too_many_arguments)]
    fn rec(
        ctx: &mut AstGenContext,
        shape: &[usize],
        dep: usize,
        idxs: &mut Vec<Inst>,
        lhs: Inst,
        rhs: Inst,
        op: BinaryOp,
        lhs_tensor: bool,
        rhs_tensor: bool,
        target_tensor: Inst,
    ) {
        if dep == shape.len() {
            leaf(
                ctx,
                idxs,
                lhs,
                rhs,
                op,
                lhs_tensor,
                rhs_tensor,
                target_tensor,
            );
            return;
        }
        // create 3 basic blocks for while loop
        let (entry, body, end) = create_loop_blocks(ctx);

        // jump into while entry block unconditionally, with zero-init induction variable.
        let zero_init_induction_variable = ctx.new_local_value().integer(0);
        let jump_to_while_entry = ctx
            .new_local_value()
            .jump(entry, vec![zero_init_induction_variable]);
        ctx.push_inst(jump_to_while_entry);

        // manual set the entry
        // It only contains a comparison with tensor length limit.
        ctx.set_curr_bb(entry);
        let entry_params = ctx.bb_params(entry);
        assert!(entry_params.len() == 1);
        let idx_var = entry_params[0];
        idxs.push(idx_var);

        let target = ctx.new_local_value().integer(shape[dep] as i32);
        let cond_val = ctx.new_local_value().binary(BinaryOp::Lt, idx_var, target);
        ctx.push_inst(cond_val);
        let branch = ctx
            .new_local_value()
            .branch(cond_val, body, vec![], end, vec![]);
        ctx.push_inst(branch);

        ctx.set_curr_bb(body);

        // should start recursion here
        rec(
            ctx,
            shape,
            dep + 1,
            idxs,
            lhs,
            rhs,
            op,
            lhs_tensor,
            rhs_tensor,
            target_tensor,
        );

        // add the while a unconditional jump to entry to have a comparison.
        let one = ctx.new_local_value().integer(1);
        let add = ctx.new_local_value().binary(BinaryOp::Add, idx_var, one);
        ctx.push_inst(add);
        let jump = ctx.new_local_value().jump(entry, vec![add]);
        ctx.push_inst(jump);

        ctx.pop_loop();
        ctx.set_curr_bb(end);
    }

    #[allow(clippy::too_many_arguments)]
    fn leaf(
        ctx: &mut AstGenContext,
        idxs: &[Inst],
        lhs: Inst,
        rhs: Inst,
        op: BinaryOp,
        lhs_tensor: bool,
        rhs_tensor: bool,
        target_tensor: Inst,
    ) {
        let arr_ty = tensor_shape_type(ctx, if lhs_tensor { lhs } else { rhs });
        let base_ty = arr_ty.array_base_scalar_type();

        let lhs = if lhs_tensor {
            lhs
        } else {
            ctx.coerce_local(lhs, &base_ty)
        };
        let rhs = if rhs_tensor {
            rhs
        } else {
            ctx.coerce_local(rhs, &base_ty)
        };
        let lhs = if lhs_tensor {
            tensor_get_elem_raw(ctx, lhs, idxs)
        } else {
            lhs
        };
        let rhs = if rhs_tensor {
            tensor_get_elem_raw(ctx, rhs, idxs)
        } else {
            rhs
        };
        let binary = ctx.new_local_value().binary(op, lhs, rhs);
        ctx.push_inst(binary);
        let res = tensor_elem_ptr_raw(ctx, target_tensor, idxs);
        let store = ctx.new_local_value().store(binary, res);
        ctx.push_inst(store);
    }
    let arr_ty = tensor_shape_type(ctx, if lhs_tensor { lhs } else { rhs });
    let shape = arr_ty.get_array_shape();
    let temp = ctx.new_local_value().alloc(arr_ty);
    ctx.push_inst(temp);

    let mut idxs = vec![];
    rec(
        ctx, &shape, 0, &mut idxs, lhs, rhs, op, lhs_tensor, rhs_tensor, temp,
    );
    temp
}

fn lower_elementwise(
    ctx: &mut AstGenContext,
    lhs: Inst,
    rhs: Inst,
    op: BinaryOp,
    lhs_tensor: bool,
    rhs_tensor: bool,
) -> Inst {
    let arr_ty = tensor_shape_type(ctx, if lhs_tensor { lhs } else { rhs });
    let base_ty = arr_ty.array_base_scalar_type();
    // let shape = arr_ty.get_array_shape();

    let lhs = if lhs_tensor {
        lhs
    } else {
        ctx.coerce_local(lhs, &base_ty)
    };
    let rhs = if rhs_tensor {
        rhs
    } else {
        ctx.coerce_local(rhs, &base_ty)
    };

    let temp = ctx.new_local_value().alloc(arr_ty);
    ctx.push_inst(temp);
    tensor_for_each(ctx, temp, |ctx, idxs| {
        let lhs = if lhs_tensor {
            tensor_get_elem(ctx, lhs, idxs)
        } else {
            lhs
        };
        let rhs = if rhs_tensor {
            tensor_get_elem(ctx, rhs, idxs)
        } else {
            rhs
        };
        let binary = ctx.new_local_value().binary(op, lhs, rhs);
        ctx.push_inst(binary);
        let res = tensor_elem_ptr(ctx, temp, idxs);
        let store = ctx.new_local_value().store(binary, res);
        ctx.push_inst(store);
    });
    temp
}

impl ToRaanaIR for items::UnaryOp {
    fn convert(&self, ctx: &mut AstGenContext) {
        if ctx.is_complete_bb() {
            return;
        }
        // if `+` is unary then it will do nothing.
        if matches!(self, items::UnaryOp::Add) {
            return;
        }

        let rhs = ctx.pop_val().unwrap();

        let is_tensor = is_tensor(ctx, rhs);
        if is_tensor {
            let inst = match self {
                Self::Minus => {
                    let zero = ctx.new_local_value().integer(0);
                    lower_elementwise_native_loop(ctx, zero, rhs, BinaryOp::Sub, false, true)
                }
                _ => unreachable!(),
            };
            ctx.push_val(inst);
            return;
        }

        //Constant folding
        let rhs_val = ctx.inst_data(rhs);
        if let InstKind::Integer(integer) = rhs_val.kind().clone() {
            let operation = match self {
                items::UnaryOp::Add => unreachable!(),
                items::UnaryOp::Minus => ctx.new_local_value().integer(-integer.value()),
                items::UnaryOp::Negation => {
                    ctx.new_local_value().integer((integer.value() == 0) as _)
                }
            };
            ctx.push_val(operation);
            return;
        }
        if let InstKind::Float(float) = rhs_val.kind().clone() {
            let operation = match self {
                items::UnaryOp::Add => unreachable!(),
                items::UnaryOp::Minus => ctx.new_local_value().float(-float.value()),
                items::UnaryOp::Negation => {
                    ctx.new_local_value().integer((float.value() == 0.0) as _)
                }
            };
            ctx.push_val(operation);
            return;
        }

        let operation = match self {
            items::UnaryOp::Add => unreachable!(),
            items::UnaryOp::Minus => {
                let ty = ctx.inst_data(rhs).ty().clone();
                let zero = ctx.zero_local(&ty);
                ctx.new_local_value().binary(BinaryOp::Sub, zero, rhs)
            }
            items::UnaryOp::Negation => {
                let ty = ctx.inst_data(rhs).ty().clone();
                let zero = ctx.zero_local(&ty);
                ctx.new_local_value().binary(BinaryOp::Eq, rhs, zero)
            }
        };
        ctx.push_val(operation);
        ctx.push_inst(operation);
    }

    fn global_convert(&self, ctx: &mut AstGenContext) {
        if matches!(self, items::UnaryOp::Add) {
            return;
        }
        let rhs = ctx.pop_val().unwrap();
        let val = match ctx.inst_data(rhs).kind().clone() {
            InstKind::Integer(int) => match self {
                items::UnaryOp::Add => unreachable!(),
                items::UnaryOp::Minus => ctx.new_global_value().integer(-int.value()),
                items::UnaryOp::Negation => {
                    ctx.new_global_value().integer((int.value() == 0) as i32)
                }
            },
            InstKind::Float(float) => match self {
                items::UnaryOp::Add => unreachable!(),
                items::UnaryOp::Minus => ctx.new_global_value().float(-float.value()),
                items::UnaryOp::Negation => ctx
                    .new_global_value()
                    .integer((float.value() == 0.0) as i32),
            },
            _ => unreachable!(),
        };
        ctx.push_val(val);
    }
}
