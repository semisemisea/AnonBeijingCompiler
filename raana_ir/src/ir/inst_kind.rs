mod aggregate;
mod arg_ref;
mod binary;
mod branch;
mod call;
mod global_alloc;
mod jump;
mod ptr;
mod r3turn;
mod scalar;
mod stack_mem;

pub use aggregate::Aggregate;
pub use arg_ref::BlockArgRef;
pub use arg_ref::FuncArgRef;
pub use binary::Binary;
pub use binary::BinaryOp;
pub use branch::Branch;
pub use call::Call;
pub use global_alloc::GlobalAlloc;
pub use jump::Jump;
pub use ptr::GetElemPtr;
pub use ptr::GetPtr;
pub use r3turn::Return;
pub use scalar::Float;
pub use scalar::Integer;
pub use stack_mem::Load;
pub use stack_mem::Store;

use crate::ir::instruction::Inst;

pub trait InstUsage {
    fn usage(&self) -> impl Iterator<Item = Inst>;
}
