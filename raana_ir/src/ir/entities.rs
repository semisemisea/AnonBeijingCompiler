//! Define all the entity in the RaanaIR.
use rustc_hash::FxHashSet;
use tomori_utils::{entity_impl, list::EntityList, packed_option::PackedOption};

use crate::ir::{BinaryOp, InstKind, Type};

#[derive(PartialOrd, Ord, PartialEq, Eq, Clone, Copy)]
/// Represent a slot on the stack.
/// Can read from a stackslot, or store data into it.
/// Produced by `alloc` and used by `load` or `store`.
pub struct StackSlot(u32);
entity_impl!(StackSlot, "ss");

#[derive(PartialOrd, Ord, PartialEq, Eq, Clone, Copy)]
/// Represent a SSA value.
/// Produced by an instruction.
/// Used by other instructions.
pub struct Value(u32);
entity_impl!(Value, "v");

pub enum ValueType {
    Inst { inst: Inst },
    Param { block: Block, index: u16 },
    Alias { orig: Value },
    Union { x: Value, y: Value },
}

pub struct ValueData {
    name: Option<String>,
    ty: Type,
    val_ty: ValueType,
    used_by: FxHashSet<Inst>,
}

#[derive(PartialOrd, Ord, PartialEq, Eq, Clone, Copy)]
/// Represent a Global value.
/// Used by instruction to load a global value.
pub struct GlobalValue(u32);
entity_impl!(GlobalValue, "gv");

pub struct GlobalValueData {
    name: Option<String>,
    ty: Type,
    used_by: FxHashSet<Inst>,
}

#[derive(PartialOrd, Ord, PartialEq, Eq, Clone, Copy)]
/// Represent a instruction.
/// Use value, stackslot, global value to produce a new value.
pub struct Inst(u32);
entity_impl!(Inst, "inst");

#[derive(PartialOrd, Ord, PartialEq, Eq, Clone, Copy)]
/// Represent a basic block.
pub struct Block(u32);
entity_impl!(Block, "block");

#[derive(PartialOrd, Ord, PartialEq, Eq, Clone, Copy)]
/// Represent a function.
pub struct Function(u32);
entity_impl!(Function, "f");

#[derive(PartialOrd, Ord, PartialEq, Eq, Clone, Copy)]
pub struct Constant(u32);
entity_impl!(Constant, "c");

pub enum InstData {
    Binary {
        op: BinaryOp,
        args: [Value; 2],
    },
    Jump {
        target: Block,
    },
    Brif {
        cond: Value,
        target: [Block; 2],
    },
    StackLoad {
        source: StackSlot,
    },
    StackStore {
        value: Value,
        dest: StackSlot,
    },
    StackAlloc,
    GetElemPtr {
        source: Value,
        indices: EntityList<Value>,
    },
    Call {
        func: Function,
        args: EntityList<Value>,
    },
    Return {
        val: PackedOption<Value>,
    },
    Integer {
        number: i32,
    },
    Float {
        number: f32,
    },
    Aggregate {
        agg: EntityList<Value>,
    },

    Cast {
        src: Value,
    },
}
