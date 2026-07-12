use crate::reg_alloc::reg::{PReg, PRegSet, RegClass};
use core::ops::{Index, IndexMut};

pub struct Lru {
    pub data: Vec<LruNode>,
    pub head: u8,
    pub regclass: RegClass,
}

#[derive(Clone, Copy, Debug)]
pub struct LruNode {
    pub prev: u8,
    pub next: u8,
}

impl Lru {
    pub fn new(regclass: RegClass, regs: &PRegSet) -> Self {
        let regs: Vec<PReg> = regs.into_iter().collect();
        let mut data = vec![
            LruNode {
                prev: u8::MAX,
                next: u8::MAX
            };
            PReg::MAX + 1
        ];
        let len = regs.len();
        for i in 0..len {
            let (reg, prev_reg, next_reg) = (
                regs[i],
                regs[i.checked_sub(1).unwrap_or(len - 1)],
                regs[if i >= len - 1 { 0 } else { i + 1 }],
            );
            data[reg.hw_enc()].prev = prev_reg.hw_enc() as u8;
            data[reg.hw_enc()].next = next_reg.hw_enc() as u8;
        }
        Self {
            head: if regs.is_empty() {
                u8::MAX
            } else {
                regs[0].hw_enc() as u8
            },
            data,
            regclass,
        }
    }

    pub fn poke(&mut self, preg: PReg) {
        let prev_newest = self.head;
        let hw_enc = preg.hw_enc() as u8;
        if hw_enc == prev_newest {
            return;
        }
        if self.data[prev_newest as usize].prev != hw_enc {
            self.remove(hw_enc as usize);
            self.insert_before(hw_enc, self.head);
        }
        self.head = hw_enc;
    }

    pub fn pop(&mut self) -> PReg {
        if self.is_empty() {
            panic!("LRU is empty");
        }
        let oldest = self.data[self.head as usize].prev;
        self.remove(oldest as usize);
        PReg::new(oldest as usize, self.regclass)
    }

    pub fn last(&self, from: PRegSet) -> Option<PReg> {
        self.last_satisfying(|preg| from.contains(preg))
    }

    pub fn last_satisfying<F: Fn(PReg) -> bool>(&self, f: F) -> Option<PReg> {
        if self.is_empty() {
            return None;
        }
        let mut last = self.data[self.head as usize].prev;
        let init_last = last;
        loop {
            let preg = PReg::new(last as usize, self.regclass);
            if f(preg) {
                return Some(preg);
            }
            last = self.data[last as usize].prev;
            if last == init_last {
                return None;
            }
        }
    }

    fn remove(&mut self, hw_enc: usize) {
        let (iprev, inext) = (
            self.data[hw_enc].prev as usize,
            self.data[hw_enc].next as usize,
        );
        self.data[iprev].next = self.data[hw_enc].next;
        self.data[inext].prev = self.data[hw_enc].prev;
        self.data[hw_enc].prev = u8::MAX;
        self.data[hw_enc].next = u8::MAX;
        if hw_enc == self.head as usize {
            if hw_enc == inext {
                self.head = u8::MAX;
            } else {
                self.head = inext as u8;
            }
        }
    }

    fn insert_before(&mut self, i: u8, j: u8) {
        let prev = self.data[j as usize].prev;
        self.data[prev as usize].next = i;
        self.data[j as usize].prev = i;
        self.data[i as usize] = LruNode { next: j, prev };
    }

    pub fn is_empty(&self) -> bool {
        self.head == u8::MAX
    }
}

impl core::fmt::Debug for Lru {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let data_str = if self.head == u8::MAX {
            "<empty>".to_string()
        } else {
            let mut result = format!("p{}", self.head);
            let mut node = self.data[self.head as usize].next;
            let mut seen: Vec<u8> = Vec::new();
            while node != self.head {
                if seen.contains(&node) {
                    result += &format!(" -> p{} (CYCLE!)", node);
                    break;
                }
                seen.push(node);
                result += &format!(" -> p{}", node);
                node = self.data[node as usize].next;
            }
            result
        };
        f.debug_struct("Lru")
            .field("head", if self.is_empty() { &"none" } else { &self.head })
            .field("class", &self.regclass)
            .field("data", &data_str)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct PartedByRegClass<T> {
    pub items: [T; 3],
}

impl<T: Copy> Copy for PartedByRegClass<T> {}

impl<T> Index<RegClass> for PartedByRegClass<T> {
    type Output = T;

    fn index(&self, index: RegClass) -> &Self::Output {
        &self.items[index as usize]
    }
}

impl<T> IndexMut<RegClass> for PartedByRegClass<T> {
    fn index_mut(&mut self, index: RegClass) -> &mut Self::Output {
        &mut self.items[index as usize]
    }
}

impl<T: PartialEq> PartialEq for PartedByRegClass<T> {
    fn eq(&self, other: &Self) -> bool {
        self.items.eq(&other.items)
    }
}

impl<T: core::fmt::Display> core::fmt::Display for PartedByRegClass<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{{ int: {}, float: {}, vector: {} }}",
            self.items[0], self.items[1], self.items[2]
        )
    }
}

pub type Lrus = PartedByRegClass<Lru>;

impl Lrus {
    pub fn new(int_regs: &PRegSet, float_regs: &PRegSet, vec_regs: &PRegSet) -> Self {
        Self {
            items: [
                Lru::new(RegClass::Int, int_regs),
                Lru::new(RegClass::Float, float_regs),
                Lru::new(RegClass::Vector, vec_regs),
            ],
        }
    }
}
