use std::collections::HashMap;

use index_list::{Index, IndexList};

use crate::ir::{basic_block::BasicBlock, instruction::Inst};
pub struct Layout {
    bbs: IndexList<BasicBlockLayout>,
    back: HashMap<BasicBlock, Index>,
    parent: HashMap<Inst, BasicBlock>,
}

pub struct BasicBlockLayout {
    bb: BasicBlock,
    insts: IndexList<Inst>,
    back: HashMap<Inst, Index>,
}

impl BasicBlockLayout {
    fn new(bb: BasicBlock) -> BasicBlockLayout {
        BasicBlockLayout {
            bb,
            insts: IndexList::new(),
            back: HashMap::new(),
        }
    }

    pub fn insts(&self) -> &IndexList<Inst> {
        &self.insts
    }

    pub fn insts_mut(&mut self) -> &mut IndexList<Inst> {
        &mut self.insts
    }

    pub fn bb(&self) -> BasicBlock {
        self.bb
    }
}

impl Layout {
    pub fn new() -> Layout {
        Layout {
            bbs: IndexList::new(),
            back: HashMap::new(),
            parent: HashMap::new(),
        }
    }

    pub fn basicblocks(&self) -> &IndexList<BasicBlockLayout> {
        &self.bbs
    }

    pub fn basicblock(&self, bb: BasicBlock) -> &BasicBlockLayout {
        self.bbs.get(*self.back.get(&bb).unwrap()).unwrap()
    }

    fn basicblock_mut(&mut self, bb: BasicBlock) -> &mut BasicBlockLayout {
        self.bbs.get_mut(*self.back.get(&bb).unwrap()).unwrap()
    }

    fn basicblocks_mut(&mut self) -> &mut IndexList<BasicBlockLayout> {
        &mut self.bbs
    }

    pub fn push_bb_back(&mut self, bb: BasicBlock) -> index_list::ListIndex {
        let idx = self.bbs.insert_last(BasicBlockLayout::new(bb));
        self.back.insert(bb, idx);
        idx
    }

    pub fn entry_bb(&self) -> Option<&BasicBlockLayout> {
        self.bbs.get(self.bbs.first_index())
    }

    pub fn insert_inst(&mut self, bb: BasicBlock, inst: Inst) {
        self.parent.insert(inst, bb);
        let idx = self.basicblock_mut(bb).insts.insert_last(inst);
        self.basicblock_mut(bb).back.insert(inst, idx);
    }

    pub fn remove_inst(&mut self, bb: BasicBlock, inst: Inst) {
        self.parent.remove(&inst);
        let idx = self.basicblock_mut(bb).back.remove(&inst).unwrap();
        self.basicblock_mut(bb).insts.remove(idx);
    }

    pub fn remove_basicblock(&mut self, bb: BasicBlock) {
        let idx = self.back.remove(&bb).unwrap();
        let layout = self.bbs.remove(idx).unwrap();
        for inst in layout.insts() {
            self.parent.remove(inst);
        }
    }

    #[inline]
    pub fn is_decl(&self) -> bool {
        self.bbs.is_empty()
    }
}
