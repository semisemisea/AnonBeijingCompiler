use crate::reg_alloc::reg::VReg;

#[derive(Clone)]
struct VRegNode {
    next: usize,
    prev: usize,
    vreg: VReg,
}

pub struct VRegSet {
    items: Vec<VRegNode>,
    head: usize,
}

impl VRegSet {
    pub fn with_capacity(num_vregs: usize) -> Self {
        let sentinel = num_vregs;
        Self {
            items: vec![
                VRegNode {
                    prev: sentinel,
                    next: sentinel,
                    vreg: VReg::invalid(),
                };
                num_vregs + 1
            ],
            head: sentinel,
        }
    }

    pub fn insert(&mut self, vreg: VReg) {
        debug_assert_eq!(self.items[vreg.vreg()].vreg, VReg::invalid());
        let old_head_next = self.items[self.head].next;
        self.items[vreg.vreg()] = VRegNode {
            next: old_head_next,
            prev: self.head,
            vreg,
        };
        self.items[self.head].next = vreg.vreg();
        self.items[old_head_next].prev = vreg.vreg();
    }

    pub fn remove(&mut self, vreg_num: usize) {
        let prev = self.items[vreg_num].prev;
        let next = self.items[vreg_num].next;
        self.items[prev].next = next;
        self.items[next].prev = prev;
        self.items[vreg_num].vreg = VReg::invalid();
    }

    pub fn contains(&self, vreg: VReg) -> bool {
        self.items[vreg.vreg()].vreg == vreg
    }

    pub fn is_empty(&self) -> bool {
        self.items[self.head].next == self.head
    }

    pub fn iter(&self) -> VRegSetIter<'_> {
        VRegSetIter {
            curr_item: self.items[self.head].next,
            head: self.head,
            items: &self.items,
        }
    }
}

pub struct VRegSetIter<'a> {
    curr_item: usize,
    head: usize,
    items: &'a [VRegNode],
}

impl<'a> Iterator for VRegSetIter<'a> {
    type Item = VReg;

    fn next(&mut self) -> Option<Self::Item> {
        if self.curr_item != self.head {
            let item = self.items[self.curr_item].clone();
            self.curr_item = item.next;
            Some(item.vreg)
        } else {
            None
        }
    }
}

impl std::fmt::Debug for VRegSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{{ ")?;
        for vreg in self.iter() {
            write!(f, "{vreg} ")?;
        }
        write!(f, "}}")
    }
}
