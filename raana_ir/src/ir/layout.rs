use index_list::IndexList;

use crate::ir::{basic_block::BasicBlock, instruction::Inst};

pub struct Layout {
    bbs: IndexList<BasicBlockLayout>,
    // parent check.
}

pub struct BasicBlockLayout {
    bb: BasicBlock,
    insts: IndexList<Inst>,
}

impl BasicBlockLayout {
    fn new(bb: BasicBlock) -> BasicBlockLayout {
        BasicBlockLayout {
            bb,
            insts: IndexList::new(),
        }
    }

    pub fn insts(&self) -> &IndexList<Inst> {
        &self.insts
    }

    pub fn insts_mut(&mut self) -> &mut IndexList<Inst> {
        &mut self.insts
    }
}

impl Layout {
    pub fn new() -> Layout {
        Layout {
            bbs: IndexList::new(),
        }
    }

    pub fn bbs(&self) -> &IndexList<BasicBlockLayout> {
        &self.bbs
    }

    pub fn bbs_mut(&mut self) -> &mut IndexList<BasicBlockLayout> {
        &mut self.bbs
    }

    pub fn push_bb_back(&mut self, bb: BasicBlock) -> index_list::ListIndex {
        self.bbs.insert_last(BasicBlockLayout::new(bb))
    }

    pub fn entry_bb(&self) -> Option<&BasicBlockLayout> {
        self.bbs.get(self.bbs.first_index())
    }
}
