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

    #[deprecated]
    pub fn bbs(&self) -> BasicBlocks<'_> {
        BasicBlocks { layout: self }
    }

    pub fn basicblock(&self, bb: BasicBlock) -> &BasicBlockLayout {
        self.bbs.get(*self.back.get(&bb).unwrap()).unwrap()
    }

    fn basicblock_mut(&mut self, bb: BasicBlock) -> &mut BasicBlockLayout {
        self.bbs.get_mut(*self.back.get(&bb).unwrap()).unwrap()
    }

    #[deprecated]
    pub fn bbs_mut(&mut self) -> BasicBlocksMut<'_> {
        BasicBlocksMut { layout: self }
    }

    pub fn basicblocks_mut(&mut self) -> &mut IndexList<BasicBlockLayout> {
        &mut self.bbs
    }

    pub fn push_bb_back(&mut self, bb: BasicBlock) -> index_list::ListIndex {
        self.bbs.insert_last(BasicBlockLayout::new(bb))
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
}

pub struct BasicBlocks<'a> {
    layout: &'a Layout,
}

// impl Iterator for BasicBlocks<'_> {
//     fn next(&mut self) -> Option<Self::Item> {
//
//     }
// }

impl BasicBlocks<'_> {
    #[deprecated]
    pub fn keys(&self) -> impl Iterator<Item = BasicBlock> {
        self.layout.bbs.iter().map(|bl| bl.bb)
    }

    #[deprecated]
    pub fn node(&self, bb: BasicBlock) -> &BasicBlockLayout {
        self.layout.basicblock(bb)
    }
}

pub struct BasicBlocksMut<'a> {
    layout: &'a mut Layout,
}

impl BasicBlocksMut<'_> {
    pub fn remove(&mut self, bb: BasicBlock) -> BasicBlockLayout {
        self.layout
            .bbs
            .remove(*self.layout.back.get(&bb).unwrap())
            .unwrap()
    }
}
