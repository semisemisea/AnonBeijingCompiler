use std::{
    collections::LinkedList,
    num::NonZeroU32,
    sync::atomic::{AtomicU32, Ordering},
};

use crate::ir::instruction::Inst;

pub struct BasicBlockData {
    name: String,
    params: Vec<Inst>,
    insts: LinkedList<Inst>,
}

pub type BasicBlock = NonZeroU32;

static BBID: AtomicU32 = AtomicU32::new(1);

fn next_bbid() -> BasicBlock {
    unsafe { NonZeroU32::new_unchecked(BBID.fetch_add(1, Ordering::Relaxed)) }
}
