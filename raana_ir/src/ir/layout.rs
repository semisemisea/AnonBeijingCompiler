use std::collections::HashMap;

use index_list::{Index, IndexList};

use crate::ir::{basic_block::BasicBlock, instruction::Inst};
pub struct Layout {
    bbs: IndexList<BasicBlockLayout>,
    back: HashMap<BasicBlock, Index>,
    parent: HashMap<Inst, BasicBlock>,
}

pub struct BasicBlockLayout {
    pub bb: BasicBlock,
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

    /// The block's terminator, i.e. its last instruction.
    ///
    /// This is the canonical way to reach a terminator: it keeps the intrusive
    /// instruction list an implementation detail instead of something every
    /// caller re-derives. Panics on a block with no instructions at all, which
    /// only happens on a malformed or still-under-construction function; use
    /// [`try_terminator`](Self::try_terminator) where that case is expected.
    #[inline(always)]
    pub fn terminator(&self) -> Inst {
        self.try_terminator()
            .expect("basic block has no terminator")
    }

    /// The block's terminator, or `None` when the block is empty.
    ///
    /// The fallible companion to [`terminator`](Self::terminator), for callers
    /// that legitimately see incomplete blocks: the frontend while it is still
    /// filling a block in, and analyses that walk blocks a pass has emptied.
    #[inline(always)]
    pub fn try_terminator(&self) -> Option<Inst> {
        self.insts.get_last().copied()
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

    pub fn insert_inst_before(&mut self, before: Inst, inst: Inst) {
        let bb = *self
            .parent
            .get(&before)
            .expect("anchor instruction must be in the layout");
        let before_index = *self
            .basicblock(bb)
            .back
            .get(&before)
            .expect("anchor instruction must be indexed in its basic block");
        self.parent.insert(inst, bb);
        let index = self
            .basicblock_mut(bb)
            .insts
            .insert_before(before_index, inst);
        self.basicblock_mut(bb).back.insert(inst, index);
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

    pub fn parent_bb(&self, inst: Inst) -> Option<BasicBlock> {
        self.parent.get(&inst).copied()
    }
}

#[cfg(test)]
mod tests {
    use crate::ir::{builder::*, Program, Type};

    #[test]
    fn inserts_instructions_before_layout_anchors() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "insert".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);

        let x = data.params()[0];
        let one = data.new_local_inst().integer(1);
        let add = data
            .new_local_inst()
            .binary(crate::ir::BinaryOp::Add, x, one);
        let ret = data.new_local_inst().ret(Some(add));
        data.layout_mut().insert_inst(entry, add);
        data.layout_mut().insert_inst(entry, ret);

        let first = data
            .new_local_inst()
            .binary(crate::ir::BinaryOp::Sub, x, one);
        let middle = data
            .new_local_inst()
            .binary(crate::ir::BinaryOp::Mul, x, one);
        let before_ret = data
            .new_local_inst()
            .binary(crate::ir::BinaryOp::And, x, one);
        data.layout_mut().insert_inst_before(add, first);
        data.layout_mut().insert_inst_before(ret, middle);
        data.layout_mut().insert_inst_before(ret, before_ret);

        assert_eq!(
            data.layout()
                .basicblock(entry)
                .insts()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![first, add, middle, before_ret, ret]
        );
        for inst in [first, add, middle, before_ret, ret] {
            assert_eq!(data.layout().parent_bb(inst), Some(entry));
        }
        assert_eq!(data.layout().basicblock(entry).terminator(), ret);

        data.remove_layout_inst(entry, middle);
        assert_eq!(data.layout().parent_bb(middle), None);
        assert_eq!(
            data.layout()
                .basicblock(entry)
                .insts()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![first, add, before_ret, ret]
        );
        assert_eq!(data.layout().basicblock(entry).terminator(), ret);
    }
}
