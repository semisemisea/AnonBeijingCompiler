use crate::ir::{basic_block::BasicBlock, instruction::Inst};

pub struct Layout {
    bbs: LayoutList<BasicBlock, BasicBlockLayout>,
    // parent check.
}

pub struct BasicBlockLayout {
    bb: BasicBlock,
    insts: LayoutList<Inst, ()>,
}

pub struct LayoutList<K, V> {
    nodes: Vec<(K, V)>,
}

impl<K, V> LayoutList<K, V>
where
    K: Copy + Eq,
{
    pub fn new() -> LayoutList<K, V> {
        LayoutList { nodes: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.nodes.iter().map(|(key, value)| (key, value))
    }

    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.nodes.iter().map(|(key, _)| key)
    }

    pub fn front_key(&self) -> Option<&K> {
        self.nodes.first().map(|(key, _)| key)
    }

    pub fn back_key(&self) -> Option<&K> {
        self.nodes.last().map(|(key, _)| key)
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.nodes.iter().any(|(node_key, _)| node_key == key)
    }

    pub fn node(&self, key: &K) -> Option<&V> {
        self.nodes
            .iter()
            .find_map(|(node_key, value)| (node_key == key).then_some(value))
    }

    pub fn node_mut(&mut self, key: &K) -> Option<&mut V> {
        self.nodes
            .iter_mut()
            .find_map(|(node_key, value)| (node_key == key).then_some(value))
    }

    pub fn push_key_back(&mut self, key: K) -> Result<(), ()>
    where
        V: Default,
    {
        self.push_back(key, V::default())
    }

    pub fn push_back(&mut self, key: K, value: V) -> Result<(), ()> {
        if self.contains_key(&key) {
            return Err(());
        }
        self.nodes.push((key, value));
        Ok(())
    }

    pub fn remove(&mut self, key: &K) -> Option<(K, V)> {
        let index = self
            .nodes
            .iter()
            .position(|(node_key, _)| node_key == key)?;
        Some(self.nodes.remove(index))
    }
}

impl<'a, K, V> IntoIterator for &'a LayoutList<K, V>
where
    K: Copy + Eq,
{
    type Item = (&'a K, &'a V);
    type IntoIter = std::iter::Map<std::slice::Iter<'a, (K, V)>, fn(&(K, V)) -> (&K, &V)>;

    fn into_iter(self) -> Self::IntoIter {
        fn as_refs<K, V>((key, value): &(K, V)) -> (&K, &V) {
            (key, value)
        }

        self.nodes.iter().map(as_refs::<K, V>)
    }
}

impl<K, V> Default for LayoutList<K, V>
where
    K: Copy + Eq,
{
    fn default() -> Self {
        Self { nodes: Vec::new() }
    }
}

impl BasicBlockLayout {
    fn new(bb: BasicBlock) -> BasicBlockLayout {
        BasicBlockLayout {
            bb,
            insts: LayoutList::new(),
        }
    }

    pub fn bb(&self) -> BasicBlock {
        self.bb
    }

    pub fn insts(&self) -> &LayoutList<Inst, ()> {
        &self.insts
    }

    pub fn insts_mut(&mut self) -> &mut LayoutList<Inst, ()> {
        &mut self.insts
    }

    pub fn bb(&self) -> BasicBlock {
        self.bb
    }
}

impl Layout {
    pub fn new() -> Layout {
        Layout {
            bbs: LayoutList::new(),
        }
    }

    pub fn bbs(&self) -> &LayoutList<BasicBlock, BasicBlockLayout> {
        &self.bbs
    }

    pub fn bbs_mut(&mut self) -> &mut LayoutList<BasicBlock, BasicBlockLayout> {
        &mut self.bbs
    }

    pub fn push_bb_back(&mut self, bb: BasicBlock) -> Result<(), ()> {
        self.bbs.push_back(bb, BasicBlockLayout::new(bb))
    }

    pub fn entry_bb(&self) -> Option<BasicBlock> {
        self.bbs.front_key().copied()
    }

    pub fn bb_mut(&mut self, bb: BasicBlock) -> &mut BasicBlockLayout {
        self.bbs.node_mut(&bb).unwrap()
    }

    pub fn parent_bb(&self, inst: Inst) -> Option<BasicBlock> {
        self.bbs
            .iter()
            .find_map(|(&bb, node)| node.insts().contains_key(&inst).then_some(bb))
    }
}
