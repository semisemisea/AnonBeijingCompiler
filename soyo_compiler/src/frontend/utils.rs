//! # utils：AST 下降工具模块（AstGenContext 与 ToRaanaIR）
//!
//! 一句话定位：本模块是 SysY2026 源码降成 RaanaIR（SSA）的**上下文与辅助工具**——
//! 提供下降全程共享的 `AstGenContext` 状态机、统一的节点下降接口 `ToRaanaIR`，
//! 以及符号表、指令创建、常量折叠、类型转换等跨节点复用的机制；具体语法节点
//! 的下降逻辑在 `ast.rs`，AST 数据结构在 `items.rs`（分工详见下文）。
//!
//! ## 数据流
//!
//! ```text
//! SysY2026 源码 --lalrpop 解析--> items.rs 的 AST
//!     --ast.rs 的 ToRaanaIR 实现（由本模块的 AstGenContext 驱动）-->
//!     raana_ir 的 Program（SSA 形式 IR，每个值只定义一次，交给优化管线与后端）
//! ```
//!
//! 前端入口解析出 `items::CompUnits` 后调用 `convert(&mut AstGenContext::new())`，
//! 一次递归下降完成整棵 AST 的下降。
//!
//! ## 核心类型
//!
//! - `ToRaanaIR`：AST 节点的下降接口，含两个方法：
//!   - `convert`：函数体内的下降，可以创建指令、操作当前基本块；
//!   - `global_convert`：全局声明的下降，走编译期常量路径（只产生常量与全局
//!     分配）；没有对应语法的节点直接 `unreachable!`/`panic!`。
//! - `AstGenContext`：下降上下文（字段见下表），并实现了 `raana_ir` 的 `Arena`
//!   trait（局部 arena 取自当前函数、全局 arena 取自 `program`），因此
//!   builder_trait 里的整套构建接口（`new_local_value` / `new_global_value` /
//!   `new_basic_block` / `inst_data` / `bb_data` 等）都能以 `ctx.xxx()` 直接调用
//!   ——这是所有指令创建的基础。
//! - `Symbol`：符号表条目，三态枚举 `Constant(Inst)` / `Variable(Inst)` /
//!   `Callable(Function)`；`SymbolTable = HashMap<Ident, Symbol>`；
//!   `Ident = Rc<str>`（共享字符串，克隆与比较都是廉价操作）。
//!
//! `AstGenContext` 的字段：
//!
//! | 字段 | 含义 |
//! |------|------|
//! | `program` | 正在构建的 `Program`（全局 arena 与函数表所在） |
//! | `func_stack` | 当前函数栈，`push_func`/`pop_func` 维护，`curr_func_data(_mut)` 取栈顶 |
//! | `val_stack` | 操作数栈：表达式转换结果经 `push_val` 压栈，父节点 `pop_val` 弹栈 |
//! | `curr_bb` | 当前正在填充的基本块，`set_curr_bb`/`reset_curr_bb` 维护 |
//! | `symbol_table` | 作用域栈（`Vec<SymbolTable>`），第 0 层是全局作用域 |
//! | `def_type` | 当前声明语句的类型：`ConstDecl`/`VarDecl` 经 `set_def_type` 设置，`ConstDef`/`VarDef` 经 `curr_def_type` 读取 |
//! | `loop_stack` | 循环栈，元素为 `(entry_bb, end_bb)`，供 `break`/`continue` 定位跳转目标 |
//!
//! ## 与 ast.rs / items.rs 的分工
//!
//! 本模块**不含任何具体语法节点的下降逻辑**，只提供引擎与工具箱：
//!
//! | 文件 | 职责 |
//! |------|------|
//! | `items.rs` | AST 数据结构定义（`CompUnits`/`FuncDef`/`Stmt`/`Exp`/`Number` 等全部节点类型，lalrpop 语法动作的产物）；`FuncFParam` 的形参类型推导、`parse_float_const`。`FuncType`/`BType` 都是 `raana_ir::ir::Type` 的别名——类型体系直接复用 IR 侧 |
//! | `ast.rs` | 每个 AST 节点的 `ToRaanaIR` 实现：表达式优先级链、语句与控制流（`IfStmt`/`WhileStmt` 建块与跳转）、**声明处理**（`ConstDecl`/`ConstDef`/`VarDecl`/`VarDef` 的局部与全局转换）；节点级辅助 `eval_i32_binary`/`eval_f32_binary`/`local_array_element_ptr` |
//! | `utils.rs`（本文件） | `AstGenContext` 状态机、`ToRaanaIR` 接口、`Symbol` 符号体系、builder/arena 入口，以及常量折叠、类型转换、布尔化等跨节点复用的辅助 |
//!
//! 注意：声明节点（`Decl`/`ConstDecl`/…）的**类型定义**在 `items.rs`、**下降逻辑**
//! 在 `ast.rs`，本文件只通过 `insert_const`/`insert_var`/`set_def_type` 等接口参与。
//!
//! ## 关键机制
//!
//! ### 指令创建：builder 先建、push_inst 后插
//!
//! builder 方法（如 `new_local_value().binary(op, lhs, rhs)`）只创建指令并返回
//! 句柄，**不会**自动入块；`push_inst` 负责把指令插入 `curr_bb`。`is_complete_bb`
//! 判定当前块是否已以 `br`/`jump`/`ret` 终结：若已终结（后续代码不可达），
//! `push_inst` 静默丢弃该指令（`remove_orphan_inst` 从 arena 清掉），防止不可达
//! 指令残留在 IR 里干扰后续优化。
//!
//! ### 基本块生命周期与控制流
//!
//! - `add_entry_bb` 创建函数入口块；`register_bb`/`remove_bb` 把块挂入/移出当前
//!   函数布局；`bb_params` 读取块参数（本 IR 中 Phi 即块参数）。
//! - `set_curr_bb` 切换当前块：若旧块尚未终结，自动补一条默认 `ret`
//!   （int → 0、float → 0.0、unit → 无返回值），保证**任何控制流路径末端都有
//!   终结符**；`reset_curr_bb` 在函数结束时清空。
//! - 控制流块由 ast.rs 构造：`WhileStmt` 建 entry/body/end 三块并 `push_loop`，
//!   `break`/`continue` 经 `curr_loop` 取跳转目标；`IfStmt` 建 then/else/end 三块。
//!
//! ### 作用域与名字解析
//!
//! - 符号表是作用域栈：进块/进函数 `add_scope`、退出 `del_scope`；`get_symbol`
//!   从最内层向外逐层查找（内层可遮蔽全局），`get_global` 只查第 0 层。
//! - 函数只允许注册在全局层（`insert_func` 的 `debug_assert!(symbol_table.len() == 1)`）；
//!   `insert_const`/`insert_var` 在当前层重定义即 assert（SysY 同层不可重名）。
//! - 常量与变量分记（`Symbol::Constant`/`Variable`），`is_constant` 据此让「给
//!   常量赋值」在 ast.rs 侧直接 panic。
//!
//! ### 值栈：表达式结果的传递通道
//!
//! `ToRaanaIR::convert` 不返回值——子节点把结果 `push_val` 压入 `val_stack`，
//! 父节点 `pop_val` 取出；`pop_i32` 是「断言为整型常量并弹栈」的快捷方式
//! （数组维度求值使用）。二元运算、赋值、return、数组下标都走这个通道。
//!
//! ### 常量折叠与类型映射
//!
//! - `as_i32`/`as_f32`：探测指令是否为编译期常量（本地或全局 arena 中的
//!   `Integer`/`Float` 字面量）并取值，否则返回 `None`；`as_local_const_val`
//!   把全局常量复制成局部常量。`zero_local`/`zero_global` 按类型造 0 常量。
//! - `coerce_local`/`coerce_global`：类型相同直接返回；否则要求两侧都是标量，
//!   先尝试常量折叠（int↔float 数值重写），失败才发 `cast` 指令；全局侧要求
//!   编译期可求值（末尾 `unreachable!`）。
//! - `truthy_local`：把任意值布尔化——常量直接算 `v != 0`，比较类指令原样返回，
//!   其余补一条 `NotEq 0` 二元指令。
//! - 类型映射：SysY 的 `int`/`float` 对应 `Type::get_i32()`/`Type::get_f32()`；
//!   数组维度在编译期求值后从内向外 `rfold` 成 `Type::get_array`；数组形参退化
//!   为 `Type::get_pointer`（`FuncFParam::ty_global`）。`is_pointer_to_array`
//!   用于区分「指向数组的退化指针」与普通指针（下标寻址时 gep 是否前置 0 偏移）。
//!
//! ### 全局 vs 局部（`global_convert` 路径）
//!
//! - 函数体内：变量一律 `alloc` 分配后走内存（`store`/`load`），即符号表里变量
//!   存的是「地址」而非 SSA 值；数组 `alloc` 后 `mem_zero` 清零，再按扁平下标
//!   `get_elem_ptr` + `store` 逐元素写入显式初值。
//! - 全局层：用全局 builder 生成 `zero_init`/`aggregate`/`global_alloc`，初值必须
//!   全程编译期可求值。
//! - `decl_library_functions` 在下降开始时注册 SysY 运行时函数（`getint`/`getch`/
//!   `getfloat`/`getarray`/`getfarray`/`putint`/`putch`/`putfloat`/`putarray`/
//!   `putfarray`/`putf`/`_sysy_starttime`/`_sysy_stoptime`）。
//! - `set_value_name` 给指令命名（局部 `v_<ident>`、全局 `gv_<ident>`），便于阅读
//!   IR dump（`--emit ir`）。
//!
//! ## 正确性要点
//!
//! - **块完整性**：`set_curr_bb` 与 `FuncDef` 尾部都会为未终结块补默认 `ret`；
//!   已终结块上的 `push_inst` 被丢弃，不产生悬空指令。
//! - **符号安全**：重定义 assert/panic；未声明标识符、给常量/函数赋值在 ast.rs
//!   侧 panic；函数只允许出现在全局作用域。
//! - **类型一致**：`coerce_*` 只允许标量间转换（否则 assert）；整型专用运算
//!   （`Rem`/`And`/`Or`/`Xor`/`Shl`/`Shr`/`Sar`）遇浮点操作数、编译期除零/模零
//!   均 panic（ast.rs 侧）。
//! - **折叠只做常量**：拿不到常量值就返回 `None` 改发指令，绝不猜值；全局初值
//!   要求编译期可求值。
//! - **数组缺省补零**：局部数组先 `mem_zero`、全局数组 `zero_init`，再覆盖显式
//!   初值——与 SysY「未显式初始化的元素为 0」的语义一致。
//!
//! ## 验证
//!
//! - 本文件无独立单元测试（`items.rs` 尾部有解析层测试）；行为由 `tests/` 的
//!   functional / h_functional 语料经 Docker harness（`make test`）验证，也可本地
//!   单文件编译对照（`target/debug/compiler -S -O 2 --target aarch64` 或
//!   `--emit ir` 查看 IR dump）。
//! - 修改本文件后至少应通过 `cargo check -p soyo_compiler`（工具链锁定 Rust
//!   1.85.0，见 `rust-toolchain.toml`）。
//! - 背景：本文件在 commit 5921037（"\[Backend\] Migrate s2r code"）之后的前端
//!   重构中从 migrate 版的 909 行瘦身到当前 556 行——原 `items.rs` 的节点类型
//!   定义被拆出，本文件只保留下降上下文与工具，公开接口保持不变。
//!
/// Define how a AST node should convert to Koopa IR.
///
/// Required method: `fn convert(&self, ctx: &mut AstGenContext);`
///
/// @param `ctx`: Context that store everything needed to convert.
pub trait ToRaanaIR {
    fn convert(&self, ctx: &mut AstGenContext);

    fn global_convert(&self, ctx: &mut AstGenContext);
}

use super::items::*;
use raana_ir::ir::{arena::Arena, builder_trait::*, *};
use std::collections::{
    HashMap,
    hash_map::Entry::{Occupied, Vacant},
};

pub type Ident = std::rc::Rc<str>;

#[derive(Debug, Clone, Copy)]
pub enum Symbol {
    Constant(Inst),
    Variable(Inst),
    Callable(Function),
}

pub type SymbolTable = HashMap<Ident, Symbol>;

pub struct AstGenContext {
    pub program: Program,
    func_stack: Vec<Function>,
    val_stack: Vec<Inst>,
    curr_bb: Option<BasicBlock>,
    symbol_table: Vec<SymbolTable>,
    def_type: Option<Type>,
    loop_stack: Vec<(BasicBlock, BasicBlock)>,
}

impl Arena for AstGenContext {
    fn local(&self) -> &arena::LocalArena {
        self.curr_func_data().local_arena()
    }

    fn global(&self) -> &arena::GlobalArena {
        self.program.global_arena()
    }

    fn local_mut(&mut self) -> &mut arena::LocalArena {
        self.curr_func_data_mut().local_arena_mut()
    }

    fn global_mut(&mut self) -> &mut arena::GlobalArena {
        self.program.global_arena_mut()
    }
}

impl AstGenContext {
    pub fn new() -> AstGenContext {
        AstGenContext {
            program: Program::new(),
            func_stack: Vec::new(),
            val_stack: Vec::new(),
            curr_bb: None,
            symbol_table: vec![SymbolTable::new()],
            def_type: None,
            loop_stack: Vec::new(),
        }
    }

    pub fn push_loop(&mut self, entry_bb: BasicBlock, end_bb: BasicBlock) {
        self.loop_stack.push((entry_bb, end_bb));
    }

    pub fn pop_loop(&mut self) {
        self.loop_stack.pop();
    }

    pub fn curr_loop(&self) -> Option<(BasicBlock, BasicBlock)> {
        self.loop_stack.last().copied()
    }

    pub fn add_entry_bb(&mut self) -> BasicBlock {
        self.curr_func_data_mut().add_entry_block()
    }

    pub fn add_scope(&mut self) {
        self.symbol_table.push(HashMap::new());
    }

    pub fn del_scope(&mut self) {
        self.symbol_table.pop();
    }

    pub fn curr_scope(&self) -> &SymbolTable {
        self.symbol_table.last().unwrap()
    }

    pub fn curr_scope_mut(&mut self) -> &mut SymbolTable {
        self.symbol_table.last_mut().unwrap()
    }

    pub fn global_scope(&self) -> &SymbolTable {
        self.symbol_table.first().unwrap()
    }

    pub fn new_global_value(&mut self) -> GlobalBuilder<'_> {
        self.program.new_value()
    }

    #[inline]
    pub fn insert_const(&mut self, ident: Ident, val: Inst) {
        assert!(
            self.curr_scope().get(&ident).is_none(),
            "Redefine the constant {}",
            &*ident
        );
        self.curr_scope_mut().insert(ident, Symbol::Constant(val));
    }

    #[inline]
    pub fn insert_var(&mut self, ident: Ident, val: Inst) {
        assert!(
            // self.global_scope().get(&ident).is_none()
            self.curr_scope().get(&ident).is_none(),
            "Redefine the variable {}",
            &*ident
        );
        self.curr_scope_mut().insert(ident, Symbol::Variable(val));
    }

    #[inline]
    pub fn insert_func(&mut self, ident: Ident, func: Function) {
        debug_assert!(self.symbol_table.len() == 1);
        match self.curr_scope_mut().entry(ident.clone()) {
            Occupied(_) => panic!("Redefine the function {}", &*ident),
            Vacant(e) => {
                e.insert(Symbol::Callable(func));
            }
        }
    }

    #[inline]
    pub fn get_symbol(&self, ident: &Ident) -> Option<Symbol> {
        self.symbol_table
            .iter()
            .rev()
            .find_map(|symbol_table| symbol_table.get(ident).copied())
    }

    #[inline]
    /// cheap version of get_symbol when you want global
    pub fn get_global(&self, ident: &Ident) -> Option<Symbol> {
        self.symbol_table.first().unwrap().get(ident).copied()
    }

    #[inline]
    pub fn push_func(&mut self, func: Function) {
        self.func_stack.push(func);
    }

    #[inline]
    pub fn pop_func(&mut self) -> Option<Function> {
        self.func_stack.pop()
    }

    #[inline]
    pub fn curr_func_data_mut(&mut self) -> &mut FunctionData {
        self.program.func_data_mut(*self.func_stack.last().unwrap())
    }

    #[inline]
    pub fn curr_func_data(&self) -> &FunctionData {
        self.program.func_data(*self.func_stack.last().unwrap())
    }

    #[inline]
    /// A completed basic block means it has end with one of the instruction below.
    /// `br`, `jump`, `ret`
    pub fn is_complete_bb(&self) -> bool {
        let curr_bb = self.curr_bb.unwrap();
        self.curr_func_data()
            .layout()
            .basicblock(curr_bb)
            .insts()
            .get_last()
            .is_some_and(|&inst| {
                matches!(
                    self.inst_data(inst).kind(),
                    InstKind::Branch(_) | InstKind::Jump(_) | InstKind::Return(_)
                )
            })
    }

    #[inline]
    /// No effect when a basic block is completed (a.k.a have `br`, `jump` or `ret` at the end)
    pub fn push_inst(&mut self, inst: Inst) {
        let curr_bb = self.curr_bb.unwrap();
        if !self.is_complete_bb() {
            self.curr_func_data_mut()
                .layout_mut()
                .insert_inst(curr_bb, inst);
        } else {
            self.curr_func_data_mut().remove_orphan_inst(inst);
        }
    }

    pub fn remove_inst(&mut self, inst: Inst) {
        let curr_basic_blcok = self.curr_bb.unwrap();
        self.curr_func_data_mut()
            .remove_layout_inst(curr_basic_blcok, inst);
    }

    #[inline]
    pub fn push_val(&mut self, val: Inst) {
        self.val_stack.push(val);
    }

    #[inline]
    pub fn pop_val(&mut self) -> Option<Inst> {
        self.val_stack.pop()
    }

    pub fn register_bb(&mut self, bb: BasicBlock) {
        self.curr_func_data_mut().layout_mut().push_bb_back(bb);
    }

    pub fn remove_bb(&mut self, bb: BasicBlock) {
        self.curr_func_data_mut().remove_layout_basicblock(bb);
    }

    #[inline]
    /// Return the original basic_block handle
    pub fn set_curr_bb(&mut self, bb: BasicBlock) -> Option<BasicBlock> {
        if self.curr_bb.is_some() && !self.is_complete_bb() {
            let ret_val = match self.curr_func_data().ret_ty().kind() {
                TypeKind::Unit => None,
                TypeKind::Int32 => Some(self.new_local_value().integer(0)),
                TypeKind::Float32 => Some(self.new_local_value().float(0.0)),
                _ => unreachable!(),
            };
            let ret = self.new_local_value().ret(ret_val);
            self.push_inst(ret);
        }
        self.curr_bb.replace(bb)
    }

    #[inline]
    pub fn reset_curr_bb(&mut self) {
        self.curr_bb = None
    }

    #[inline]
    pub fn bb_params(&self, bb: BasicBlock) -> &[Inst] {
        self.bb_data(bb).params()
    }

    #[inline]
    pub fn set_def_type(&mut self, ty: Type) -> Option<Type> {
        self.def_type.replace(ty)
    }

    #[inline]
    pub fn curr_def_type(&self) -> Option<Type> {
        self.def_type.clone()
    }

    #[inline]
    pub fn is_constant(&self, l_val: &LVal) -> bool {
        matches!(
            self.curr_scope().get(&l_val.ident),
            Some(Symbol::Constant(_))
        )
    }

    #[inline]
    pub fn decl_library_functions(&mut self) {
        let getint = self
            .program
            .new_function(Type::get_i32(), "getint".into(), vec![]);
        self.insert_func(std::rc::Rc::from("getint"), getint);

        let getch = self
            .program
            .new_function(Type::get_i32(), "getch".into(), vec![]);
        self.insert_func(std::rc::Rc::from("getch"), getch);

        let getfloat = self
            .program
            .new_function(Type::get_f32(), "getfloat".into(), vec![]);
        self.insert_func(std::rc::Rc::from("getfloat"), getfloat);

        let getarray = self.program.new_function(
            Type::get_i32(),
            "getarray".into(),
            vec![Type::get_pointer(Type::get_i32())],
        );
        self.insert_func(std::rc::Rc::from("getarray"), getarray);

        let getfarray = self.program.new_function(
            Type::get_i32(),
            "getfarray".into(),
            vec![Type::get_pointer(Type::get_f32())],
        );
        self.insert_func(std::rc::Rc::from("getfarray"), getfarray);

        let putint =
            self.program
                .new_function(Type::get_unit(), "putint".into(), vec![Type::get_i32()]);
        self.insert_func(std::rc::Rc::from("putint"), putint);

        let putch =
            self.program
                .new_function(Type::get_unit(), "putch".into(), vec![Type::get_i32()]);
        self.insert_func(std::rc::Rc::from("putch"), putch);

        let putfloat =
            self.program
                .new_function(Type::get_unit(), "putfloat".into(), vec![Type::get_f32()]);
        self.insert_func(std::rc::Rc::from("putfloat"), putfloat);

        let putarray = self.program.new_function(
            Type::get_unit(),
            "putarray".into(),
            vec![Type::get_i32(), Type::get_pointer(Type::get_i32())],
        );
        self.insert_func(std::rc::Rc::from("putarray"), putarray);

        let putfarray = self.program.new_function(
            Type::get_unit(),
            "putfarray".into(),
            vec![Type::get_i32(), Type::get_pointer(Type::get_f32())],
        );
        self.insert_func(std::rc::Rc::from("putfarray"), putfarray);

        let putf = self.program.new_function(
            Type::get_unit(),
            "putf".into(),
            vec![Type::get_string(), Type::get_arg_list()],
        );
        self.insert_func(std::rc::Rc::from("putf"), putf);

        let starttime = self.program.new_function(
            Type::get_unit(),
            "_sysy_starttime".into(),
            vec![Type::get_i32()],
        );
        self.insert_func(std::rc::Rc::from("_sysy_starttime"), starttime);

        let stoptime = self.program.new_function(
            Type::get_unit(),
            "_sysy_stoptime".into(),
            vec![Type::get_i32()],
        );
        self.insert_func(std::rc::Rc::from("_sysy_stoptime"), stoptime);
    }

    #[inline]
    fn local_val_as_i32(&self, inst: Inst) -> Option<i32> {
        debug_assert!(!inst.is_global());
        match self.inst_data(inst).kind() {
            InstKind::Integer(int) => Some(int.value()),
            _ => None,
        }
    }

    #[inline]
    fn global_val_as_i32(&self, inst: Inst) -> Option<i32> {
        debug_assert!(inst.is_global());
        match self.inst_data(inst).kind() {
            InstKind::Integer(int) => Some(int.value()),
            _ => None,
        }
    }

    #[inline]
    fn local_val_as_f32(&self, inst: Inst) -> Option<f32> {
        debug_assert!(!inst.is_global());
        match self.inst_data(inst).kind() {
            InstKind::Float(float) => Some(float.value()),
            _ => None,
        }
    }

    #[inline]
    fn global_val_as_f32(&self, inst: Inst) -> Option<f32> {
        debug_assert!(inst.is_global());
        match self.inst_data(inst).kind() {
            InstKind::Float(float) => Some(float.value()),
            _ => None,
        }
    }

    pub fn as_i32(&self, val: Inst) -> Option<i32> {
        if val.is_global() {
            self.global_val_as_i32(val)
        } else {
            self.local_val_as_i32(val)
        }
    }

    pub fn as_f32(&self, val: Inst) -> Option<f32> {
        if val.is_global() {
            self.global_val_as_f32(val)
        } else {
            self.local_val_as_f32(val)
        }
    }

    #[inline]
    fn global_val_as_local_val(&mut self, inst: Inst) -> Inst {
        assert!(inst.is_global());
        match self.inst_data(inst).kind().clone() {
            InstKind::Integer(int) => self
                .curr_func_data_mut()
                .new_local_inst()
                .integer(int.value()),
            InstKind::Float(float) => self
                .curr_func_data_mut()
                .new_local_inst()
                .float(float.value()),
            _ => unreachable!(),
        }
    }

    pub fn as_local_const_val(&mut self, val: Inst) -> Inst {
        if val.is_global() {
            self.global_val_as_local_val(val)
        } else {
            val
        }
    }

    pub fn zero_local(&mut self, ty: &Type) -> Inst {
        if ty.is_f32() {
            self.new_local_value().float(0.0)
        } else {
            self.new_local_value().integer(0)
        }
    }

    pub fn zero_global(&mut self, ty: &Type) -> Inst {
        if ty.is_f32() {
            self.new_global_value().float(0.0)
        } else {
            self.new_global_value().integer(0)
        }
    }

    pub fn coerce_local(&mut self, val: Inst, ty: &Type) -> Inst {
        let from_ty = self.inst_data(val).ty().clone();
        if from_ty == *ty {
            return val;
        }
        assert!(
            from_ty.is_scalar() && ty.is_scalar(),
            "Cannot convert {from_ty} to {ty}"
        );
        if ty.is_i32() {
            if let Some(int) = self.as_i32(val) {
                return self.new_local_value().integer(int);
            }
            if let Some(float) = self.as_f32(val) {
                return self.new_local_value().integer(float as i32);
            }
        }
        if ty.is_f32() {
            if let Some(float) = self.as_f32(val) {
                return self.new_local_value().float(float);
            }
            if let Some(int) = self.as_i32(val) {
                return self.new_local_value().float(int as f32);
            }
        }
        let cast = self.new_local_value().cast(val, ty.clone());
        self.push_inst(cast);
        cast
    }

    pub fn coerce_global(&mut self, val: Inst, ty: &Type) -> Inst {
        let from_ty = self.inst_data(val).ty().clone();
        if from_ty == *ty {
            return val;
        }
        assert!(
            from_ty.is_scalar() && ty.is_scalar(),
            "Cannot convert {from_ty} to {ty}"
        );
        if ty.is_i32() {
            if let Some(int) = self.as_i32(val) {
                return self.new_global_value().integer(int);
            }
            if let Some(float) = self.as_f32(val) {
                return self.new_global_value().integer(float as i32);
            }
        }
        if ty.is_f32() {
            if let Some(float) = self.as_f32(val) {
                return self.new_global_value().float(float);
            }
            if let Some(int) = self.as_i32(val) {
                return self.new_global_value().float(int as f32);
            }
        }
        unreachable!("global values must be compile-time constants")
    }

    pub fn truthy_local(&mut self, val: Inst) -> Inst {
        let ty = self.inst_data(val).ty().clone();
        if let Some(int) = self.as_i32(val) {
            return self.new_local_value().integer((int != 0) as i32);
        }
        if let Some(float) = self.as_f32(val) {
            return self.new_local_value().integer((float != 0.0) as i32);
        }
        if matches!(self.inst_data(val).kind(), InstKind::Binary(binary) if binary.op().is_compare())
        {
            return val;
        }
        let zero = self.zero_local(&ty);
        let cond = self.new_local_value().binary(BinaryOp::NotEq, val, zero);
        self.push_inst(cond);
        cond
    }

    pub fn func_param_tys(&self, func: Function) -> Vec<Type> {
        self.program.func_data(func).params_ty().to_vec()
    }

    pub fn curr_func_ret_ty(&self) -> Type {
        self.curr_func_data().ret_ty().clone()
    }

    pub fn pop_i32(&mut self) -> i32 {
        let val = self.pop_val().expect("Value stack is empty");
        self.as_i32(val)
            .unwrap_or_else(|| panic!("Not an integer {:?}", val))
    }

    pub fn set_value_name(&mut self, inst: Inst, ident: Ident) {
        if inst.is_global() {
            self.inst_data_mut(inst)
                .set_name(format!("gv_{}", ident.clone()));
        } else {
            self.inst_data_mut(inst)
                .set_name(format!("v_{}", ident.clone()));
        }
    }

    pub fn is_pointer_to_array(&self, inst: Inst) -> bool {
        match self.inst_data(inst).ty().kind() {
            TypeKind::Pointer(point_to) => matches!(point_to.kind(), TypeKind::Array(..)),
            _ => false,
        }
    }
}
