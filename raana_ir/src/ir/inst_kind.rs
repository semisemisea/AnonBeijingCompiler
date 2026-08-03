pub mod aggregate;
pub mod arg_ref;
pub mod binary;
pub mod branch;
pub mod call;
pub mod cast;
pub mod fma;
pub mod global_alloc;
pub mod jump;
pub mod mem_zero;
pub mod ptr;
pub mod r3turn;
pub mod scalar;
pub mod select;
pub mod stack_mem;
pub mod tail_call;
pub mod vector_lane;
pub mod vector_reduce;
pub mod vector_splat;

pub use aggregate::Aggregate;
pub use arg_ref::BlockArgRef;
pub use binary::Binary;
pub use binary::BinaryOp;
pub use branch::Branch;
pub use call::Call;
pub use cast::Cast;
pub use fma::Fma;
pub use global_alloc::GlobalAlloc;
pub use jump::Jump;
pub use mem_zero::MemZero;
pub use mem_zero::MemZeroLen;
pub use ptr::GetElemPtr;
pub use r3turn::Return;
pub use scalar::Float;
pub use scalar::Integer;
pub use select::Select;
pub use stack_mem::Load;
pub use stack_mem::Store;
pub use tail_call::TailCall;
pub use vector_lane::VectorExtractElement;
pub use vector_lane::VectorInsertElement;
pub use vector_reduce::VectorReduce;
pub use vector_reduce::VectorReduceOp;
pub use vector_splat::VectorSplat;

use crate::ir::basic_block::BasicBlock;
use crate::ir::instruction::Inst;

#[derive(Debug, Clone)]
pub enum InstKind {
    Undef,
    ZeroInit,
    Integer(Integer),
    Float(Float),
    Binary(Binary),
    Select(Select),
    Jump(Jump),
    Branch(Branch),
    Cast(Cast),
    Return(Return),
    GetElemPtr(GetElemPtr),
    Alloc,
    GlobalAlloc(GlobalAlloc),
    Store(Store),
    MemZero(MemZero),
    Load(Load),
    Call(Call),
    TailCall(TailCall),
    BlockArgRef(BlockArgRef),
    Aggregate(Aggregate),
    Fma(Fma),
    VectorSplat(VectorSplat),
    VectorExtractElement(VectorExtractElement),
    VectorInsertElement(VectorInsertElement),
    VectorReduce(VectorReduce),
}

impl InstKind {
    pub fn is_const(&self) -> bool {
        matches!(
            self,
            InstKind::ZeroInit | InstKind::Integer(..) | InstKind::Float(..)
        )
    }

    pub fn is_call(&self) -> bool {
        matches!(self, InstKind::Call(..))
    }

    pub fn is_load(&self) -> bool {
        matches!(self, InstKind::Load(..))
    }

    pub fn is_store(&self) -> bool {
        matches!(self, InstKind::Store(..))
    }

    pub fn is_mem_zero(&self) -> bool {
        matches!(self, InstKind::MemZero(..))
    }

    pub fn is_terminator(&self) -> bool {
        matches!(
            self,
            InstKind::Jump(..)
                | InstKind::Branch(..)
                | InstKind::Return(..)
                | InstKind::TailCall(..)
        )
    }

    pub fn is_branch(&self) -> bool {
        matches!(self, InstKind::Jump(..) | InstKind::Branch(..))
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
                if cur_index == 0 {
                    Some(get_elem_ptr.base())
                } else {
                    get_elem_ptr.offsets().get(cur_index - 1).copied()
                }
            }
            InstKind::GlobalAlloc(global_alloc) => field_use!(global_alloc.init()),
            InstKind::Store(store) => field_use!(store.src(), store.dest()),
            InstKind::MemZero(mem_zero) => match mem_zero.byte_len_len() {
                crate::ir::inst_kind::mem_zero::MemZeroLen::Const(_) => {
                    field_use!(mem_zero.dest())
                }
                crate::ir::inst_kind::mem_zero::MemZeroLen::Value(len) => {
                    field_use!(mem_zero.dest(), *len)
                }
            },
            InstKind::Load(load) => field_use!(load.src()),
            InstKind::Cast(cast) => field_use!(cast.src()),
            InstKind::Call(call) => call.args().get(cur_index).copied(),
            InstKind::TailCall(tail_call) => tail_call.args().get(cur_index).copied(),
            InstKind::Aggregate(aggregate) => aggregate.value().get(cur_index).copied(),
            InstKind::Binary(binary) => field_use!(binary.lhs(), binary.rhs()),
            InstKind::Select(select) => {
                field_use!(select.cond(), select.if_true(), select.if_false())
            }
            InstKind::Jump(jump) => jump.args().get(cur_index).copied(),
            InstKind::Fma(fma) => field_use!(fma.acc(), fma.lhs(), fma.rhs()),
            InstKind::VectorSplat(splat) => field_use!(splat.src()),
            InstKind::VectorExtractElement(extract) => {
                field_use!(extract.src(), extract.index())
            }
            InstKind::VectorInsertElement(insert) => {
                field_use!(insert.vector(), insert.element(), insert.index())
            }
            InstKind::VectorReduce(reduce) => field_use!(reduce.src()),
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
