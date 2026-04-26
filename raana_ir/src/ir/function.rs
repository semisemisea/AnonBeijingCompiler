use std::{
    collections::LinkedList,
    num::NonZeroU32,
    sync::atomic::{AtomicU32, Ordering},
};

use crate::ir::{basic_block::BasicBlock, instruction::Inst, layout::Layout, types::Type};

pub struct FunctionData {
    ret_ty: Type,
    name: String,
    params: Vec<Inst>,
    layout: Layout,
}

#[derive(Debug, Clone, Copy)]
pub struct Function(NonZeroU32);
// pub type Function = NonZeroU32;

static FUNCTION_ID: AtomicU32 = AtomicU32::new(1);

pub fn next_function_id() -> Function {
    Function(unsafe { NonZeroU32::new_unchecked(FUNCTION_ID.fetch_add(1, Ordering::Relaxed)) })
}
