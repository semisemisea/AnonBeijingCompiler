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
    pub b_type: Type,
    pub ident: Ident,
    pub arr_ty: Option<Vec<ConstExp>>,
}

impl FuncFParam {
    pub fn ty_global(&self, ctx: &mut AstGenContext) -> Type {
        self.arr_ty
            .as_ref()
            .map(|arr_ty| {
                Type::get_pointer(arr_ty.iter().rfold(self.b_type.clone(), |ty, off| {
                    off.global_convert(ctx);
                    let idx = ctx.pop_i32() as usize;
                    Type::get_array(ty, idx)
                }))
            })
            // BUG: what the fuck is this line.
            .unwrap_or(self.b_type.clone())
    }

    pub fn ty(&self, ctx: &mut AstGenContext) -> Type {
        self.arr_ty
            .as_ref()
            .map(|arr_ty| {
                Type::get_pointer(arr_ty.iter().rfold(self.b_type.clone(), |ty, off| {
                    off.convert(ctx);
                    let idx = ctx.pop_i32() as usize;
                    Type::get_array(ty, idx)
                }))
            })
            // BUG: what the fuck is this line.
            .unwrap_or(self.b_type.clone())
    }
}

/// FuncType ::= "int" | "float";
///
/// The return type of a function.
pub type FuncType = Type;

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
pub type BType = Type;

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
    use super::parse_float_const;

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
}
