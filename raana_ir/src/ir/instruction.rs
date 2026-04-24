use super::types::Type;
use std::collections::HashSet;

pub struct InstData {
    ty: Type,
    name: Option<String>,
    kind: InstKind,
    used_by: HashSet<Inst>,
}

pub enum InstKind {
    Undef,
    ZeroInit,
    Integer(Integer),
    Float(Float),
    Binary(Binary),
    Jump(Jump),
    Branch(Branch),
    Return(Return),
    GetElemPtr(GetElemPtr),
    GetPtr(GetPtr),
    Alloc(Alloc),
    GlobalAlloc(GlobalAlloc),
    Store(Store),
    Load(Load),
    Call(Call),
    BlockArgRef(BlockArgRef),
    FunkArgRef(FuncArgRef),
}

pub struct Integer {
    value: i32,
}

pub struct Float {
    value: f32,
}

pub struct Binary {
    op: BinaryOp,
    lhs: Inst,
    rhs: Inst,
}

pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

pub struct Jump {
    target: BasicBlock,
    args: Vec<Inst>,
}

pub struct Branch {
    t_target: BasicBlock,
    t_args: Vec<Inst>,
    f_target: BasicBlock,
    f_args: Vec<Inst>,
}

pub struct Return {
    value: Option<Inst>,
}

pub struct GetElemPtr {
    base: Inst,
    offset: Inst,
}

pub struct GetPtr {
    base: Inst,
    offset: Inst,
}

pub struct Alloc;

pub struct Store {
    src: Inst,
    dest: Inst,
}

pub struct Load {
    src: Inst,
    dest: Inst,
}

pub struct Call {
    callee: Function,
    args: Vec<Inst>,
}

pub struct BlockArgRef {
    index: usize,
}

pub struct FuncArgRef {
    index: usize,
}

pub struct GlobalAlloc {
    init: Inst,
}

// Temporary placeholder.
pub struct Inst;
pub struct BasicBlock;
pub struct Function;
