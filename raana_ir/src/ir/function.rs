use std::{
    collections::LinkedList,
    num::NonZeroU32,
    sync::atomic::{AtomicU32, Ordering},
};

use crate::ir::{basic_block::BasicBlock, instruction::Inst};

pub struct FunctionData {
    name: String,
    params: Vec<Inst>,
    // questionable data structure
    bbs: LinkedList<BasicBlock>,
}

pub type Function = NonZeroU32;

static FUNCTION_ID: AtomicU32 = AtomicU32::new(1);

pub fn next_function_id() -> Function {
    unsafe { NonZeroU32::new_unchecked(FUNCTION_ID.fetch_add(1, Ordering::Relaxed)) }
}
