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

    pub fn bb(&self) -> BasicBlock {
        self.bb
    }

    #[inline(always)]
    pub fn terminator(&self) -> Inst {
        *self.insts.get_last().unwrap()
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
        assert!(
            !self.back.contains_key(&bb),
            "block is already in the layout"
        );
        let idx = self.bbs.insert_last(BasicBlockLayout::new(bb));
        self.back.insert(bb, idx);
        idx
    }

    pub(crate) fn insert_bb_after(
        &mut self,
        after: BasicBlock,
        bb: BasicBlock,
    ) -> index_list::ListIndex {
        assert!(
            !self.back.contains_key(&bb),
            "block is already in the layout"
        );
        let after_index = *self
            .back
            .get(&after)
            .expect("anchor block must be in the layout");
        let index = self
            .bbs
            .insert_after(after_index, BasicBlockLayout::new(bb));
        self.back.insert(bb, index);
        index
    }

    pub fn entry_bb(&self) -> Option<&BasicBlockLayout> {
        self.bbs.get(self.bbs.first_index())
    }

    pub fn insert_inst(&mut self, bb: BasicBlock, inst: Inst) {
        assert!(self.back.contains_key(&bb), "block must be in the layout");
        assert!(
            !self.parent.contains_key(&inst),
            "instruction is already in the layout"
        );
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
        assert_eq!(
            self.parent.remove(&inst),
            Some(bb),
            "instruction must belong to the specified block"
        );
        let idx = self.basicblock_mut(bb).back.remove(&inst).unwrap();
        self.basicblock_mut(bb).insts.remove(idx);
    }

    /// Move every instruction strictly after `anchor` into an empty block.
    /// This changes layout ownership only; instruction use-def data is untouched.
    pub(crate) fn move_suffix_after(&mut self, anchor: Inst, destination: BasicBlock) -> Vec<Inst> {
        let source = self
            .parent_bb(anchor)
            .expect("anchor instruction must be in the layout");
        assert_ne!(source, destination, "source and destination must differ");
        assert!(
            self.basicblock(destination).insts().is_empty(),
            "destination block must be empty"
        );

        let moved = self
            .basicblock(source)
            .insts()
            .iter()
            .copied()
            .skip_while(|&inst| inst != anchor)
            .skip(1)
            .collect::<Vec<_>>();
        assert!(
            self.basicblock(source).back.contains_key(&anchor),
            "anchor instruction must be indexed in its basic block"
        );

        for &inst in &moved {
            self.remove_inst(source, inst);
        }
        for &inst in &moved {
            self.insert_inst(destination, inst);
        }
        moved
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
    use crate::ir::{Program, Type, arena::Arena, builder::*};

    #[test]
    fn inserts_instructions_before_layout_anchors() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_i32(), "insert".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();

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

    #[test]
    fn splits_a_block_after_an_instruction_without_changing_uses() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "split".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let value = data.params()[0];
        let one = data.new_local_inst().integer(1);
        let add = data
            .new_local_inst()
            .binary(crate::ir::BinaryOp::Add, value, one);
        let mul = data
            .new_local_inst()
            .binary(crate::ir::BinaryOp::Mul, add, one);
        let ret = data.new_local_inst().ret(Some(mul));
        data.layout_mut().insert_inst(entry, add);
        data.layout_mut().insert_inst(entry, mul);
        data.layout_mut().insert_inst(entry, ret);

        let add_users = data.inst_data(add).used_by().clone();
        let mul_users = data.inst_data(mul).used_by().clone();
        let tail = data.split_block_after(add, "split_tail".into(), vec![]);

        assert_eq!(
            data.layout()
                .basicblocks()
                .iter()
                .map(|layout| layout.bb())
                .collect::<Vec<_>>(),
            vec![entry, tail]
        );
        assert_eq!(
            data.layout()
                .basicblock(entry)
                .insts()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![add]
        );
        assert_eq!(
            data.layout()
                .basicblock(tail)
                .insts()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![mul, ret]
        );
        assert_eq!(data.layout().parent_bb(add), Some(entry));
        assert_eq!(data.layout().parent_bb(mul), Some(tail));
        assert_eq!(data.layout().parent_bb(ret), Some(tail));
        assert_eq!(data.inst_data(add).used_by(), &add_users);
        assert_eq!(data.inst_data(mul).used_by(), &mul_users);

        let before_ret = data
            .new_local_inst()
            .binary(crate::ir::BinaryOp::Sub, mul, one);
        data.layout_mut().insert_inst_before(ret, before_ret);
        assert_eq!(data.layout().parent_bb(before_ret), Some(tail));
    }
}
