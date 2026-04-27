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

use crate::ir::basic_block::BasicBlock;
use crate::ir::instruction::Inst;

#[derive(Debug, Clone)]
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
    Alloc,
    GlobalAlloc(GlobalAlloc),
    Store(Store),
    Load(Load),
    Call(Call),
    BlockArgRef(BlockArgRef),
    FuncArgRef(FuncArgRef),
    Aggregate(Aggregate),
}

impl InstKind {
    pub fn is_const(&self) -> bool {
        matches!(
            self,
            InstKind::ZeroInit
                | InstKind::Aggregate(..)
                | InstKind::Integer(..)
                | InstKind::Float(..)
        )
    }
}

pub struct InstUsage<'a> {
    pub(in crate::ir) data: &'a InstKind,
    pub(in crate::ir) index: usize,
}

impl Iterator for InstUsage<'_> {
    type Item = Inst;
    fn next(&mut self) -> Option<Self::Item> {
        let cur_index = self.index;
        self.index += 1;
        macro_rules! field_use {
            ($($field:expr),+) => {
                field_use!(@expand 0 $(,$field)+)
            };
            (@expand $index:expr) => {
                None
            };
            (@expand $index:expr, $head:expr $(,$tail:expr)*) => {
                if cur_index == $index {
                Some($head)
                } else {
                field_use!(@expand $index + 1 $(,$tail)*)
                }
            };
        }
        match self.data {
            InstKind::BlockArgRef(..)
            | InstKind::FuncArgRef(..)
            | InstKind::Float(..)
            | InstKind::Integer(..)
            | InstKind::Alloc
            | InstKind::ZeroInit
            | InstKind::Undef => None,
            InstKind::Branch(branch) => {
                let tlen = branch.t_args().len();
                let flen = branch.f_args().len();
                if cur_index == 0 {
                    Some(branch.cond())
                } else if cur_index < tlen + 1 {
                    Some(branch.t_args()[cur_index - 1])
                } else if cur_index < tlen + flen + 1 {
                    Some(branch.f_args()[cur_index - 1 - tlen])
                } else {
                    None
                }
            }
            InstKind::Return(ret) => {
                if cur_index == 0 {
                    ret.value()
                } else {
                    None
                }
            }
            InstKind::GetElemPtr(get_elem_ptr) => {
                field_use!(get_elem_ptr.base(), get_elem_ptr.offset())
            }
            InstKind::GetPtr(get_ptr) => field_use!(get_ptr.base(), get_ptr.offset()),
            InstKind::GlobalAlloc(global_alloc) => field_use!(global_alloc.init()),
            InstKind::Store(store) => field_use!(store.src(), store.dest()),
            InstKind::Load(load) => field_use!(load.src()),
            InstKind::Call(call) => call.args().get(cur_index).copied(),
            InstKind::Aggregate(aggregate) => aggregate.value().get(cur_index).copied(),
            InstKind::Binary(binary) => field_use!(binary.lhs(), binary.rhs()),
            InstKind::Jump(jump) => jump.args().get(cur_index).copied(),
        }
    }
}

pub struct BasicBlockUsage<'a> {
    pub(in crate::ir) data: &'a InstKind,
    pub(in crate::ir) index: usize,
}

impl Iterator for BasicBlockUsage<'_> {
    type Item = BasicBlock;
    fn next(&mut self) -> Option<Self::Item> {
        let cur_index = self.index;
        self.index += 1;
        match self.data {
            InstKind::Jump(jump) => match cur_index {
                0 => Some(jump.target()),
                _ => None,
            },
            InstKind::Branch(branch) => match cur_index {
                0 => Some(branch.t_target()),
                1 => Some(branch.f_target()),
                _ => None,
            },
            _ => None,
        }
    }
}
