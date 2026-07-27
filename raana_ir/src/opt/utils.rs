use std::collections::{HashMap, hash_map::Entry};

use crate::{
    ir::{
        BasicBlock, FunctionData, Inst, InstKind, Type, TypeKind,
        arena::Arena,
        builder_trait::{LocalInstBuilder, ScalarInstBuilder},
    },
    opt::pass::ArenaContextMut,
};

use itertools::Itertools;
use log::debug;

pub mod type_alias {
    use std::collections::{HashMap, HashSet};

    use crate::{
        ir::{BasicBlock, Inst},
        opt::utils::IDAllocator,
    };

    // Inst ID
    pub type VId = usize;

    // Basic Block ID
    pub type BId = usize;

    pub type VIDAlloc = IDAllocator<Inst, VId>;
    pub type BIDAlloc = IDAllocator<BasicBlock, BId>;
    pub type IDomMap = Vec<BId>;
    pub type DomTree = Vec<Vec<BId>>;

    // Control Flow Graph
    // Each Vertex is a basic block
    pub type CFGGraph = HashMap<BId, Vec<BId>>;

    pub type GPath = Vec<BId>;
    // TODO: Could be replace to bit set.
    pub type Set = HashSet<BId>;
}

#[derive(Debug)]
pub struct IDAllocator<PKey, Id, NKey = PKey> {
    id_pos: HashMap<PKey, Id>,
    id_neg: HashMap<Id, NKey>,
    cnt: Id,
    increase_by: Id,
}

impl<PK, I, NK> Default for IDAllocator<PK, I, NK>
where
    I: num_traits::One + num_traits::Zero,
{
    fn default() -> Self {
        IDAllocator::new(I::one())
    }
}

impl<PK, I, NK> IDAllocator<PK, I, NK>
where
    I: num_traits::Zero,
{
    pub fn new(increase_by: I) -> IDAllocator<PK, I, NK> {
        Self {
            id_pos: HashMap::new(),
            id_neg: HashMap::new(),
            cnt: I::zero(),
            increase_by,
        }
    }
}

impl<PK, I, NK> IDAllocator<PK, I, NK>
where
    PK: Eq + std::hash::Hash + Copy + std::fmt::Debug,
    I: std::ops::AddAssign<I> + Default + Copy + Eq + std::hash::Hash,
    NK: Eq + std::hash::Hash + Copy + std::fmt::Debug,
{
    #[inline]
    pub fn check_or_alloc_id(&mut self, pkey: PK, nkey: NK) -> I {
        match self.id_pos.entry(pkey) {
            Entry::Occupied(e) => *e.get(),
            Entry::Vacant(..) => {
                debug!(
                    "Allocating new ID for key: {:?} with nkey: {:?}",
                    pkey, nkey
                );
                self.id_pos.insert(pkey, self.cnt);
                self.id_neg.insert(self.cnt, nkey);
                let ret = self.cnt;
                self.cnt += self.increase_by;
                ret
            }
        }
    }

    pub fn get_id_safe(&self, key: &PK) -> Option<&I> {
        self.id_pos.get(key)
    }

    pub fn get_id(&self, key: &PK) -> I {
        assert!(self.id_pos.contains_key(key), "Key not found: {:?}", key);
        *self.id_pos.get(key).unwrap()
    }

    pub fn search_id(&self, id: I) -> NK {
        *self.id_neg.get(&id).unwrap()
    }
}

impl<K, I> IDAllocator<K, I>
where
    K: Eq + std::hash::Hash + Copy + std::fmt::Debug,
    I: std::ops::AddAssign<I> + Default + Copy + Eq + std::hash::Hash,
{
    #[inline]
    pub fn check_or_alloc_id_same(&mut self, key: K) -> I {
        self.check_or_alloc_id(key, key)
    }

    pub fn cnt(&self) -> I {
        self.cnt
    }

    // pub fn ids(&self) -> impl Iterator<Item = &I> {
    //     self.id_neg.keys()
    // }

    // pub fn keys(&self) -> impl Iterator<Item = &K> {
    //     self.id_pos.keys()
    // }
}

#[inline]
pub fn get_terminator_inst(data: &FunctionData, bb: BasicBlock) -> Inst {
    *data.layout().basicblock(bb).insts().get_last().unwrap()
}

pub fn alloc_ty(val: Inst, data: &FunctionData) -> &Type {
    use TypeKind;
    let val_data = data.inst_data(val);
    assert!(matches!(val_data.kind(), InstKind::Alloc));
    // alloc should generate a pointer to its target type.
    let TypeKind::Pointer(pointee) = val_data.ty().kind() else {
        unreachable!()
    };
    pointee
}

pub fn visit_and_replace(data: &mut ArenaContextMut<'_>, rep: Inst, rep_with: Inst) {
    let list = data.inst_data(rep).used_by().iter().copied().collect_vec();
    for used_by in list {
        visit_and_replace_single(data, used_by, rep, rep_with);
    }
}

fn visit_and_replace_single(
    data: &mut ArenaContextMut<'_>,
    used_by: Inst,
    rep: Inst,
    rep_with: Inst,
) {
    let rep_val_data = data.inst_data(used_by);
    #[allow(unused_variables)]
    match rep_val_data.kind() {
        InstKind::ZeroInit
        | InstKind::Undef
        | InstKind::BlockArgRef(..)
        | InstKind::Alloc
        | InstKind::GlobalAlloc(..)
        | InstKind::TailCall(..) => unreachable!("Encountered kind: {:?}", rep_val_data.kind()),
        InstKind::Integer(..) | InstKind::Float(..) => {}
        InstKind::Aggregate(agg) => {
            let value = agg
                .value()
                .iter()
                .map(|&val| if val == rep { rep_with } else { val })
                .collect();
            data.replace_inst_with(used_by).aggregate(value);
        }
        InstKind::Cast(cast) => {
            let ty = rep_val_data.ty().clone();
            data.replace_inst_with(used_by).cast(rep_with, ty);
        }
        InstKind::Load(load) => {
            data.replace_inst_with(used_by).load(rep_with);
        }
        InstKind::Store(store) => {
            let src = if store.src() == rep {
                rep_with
            } else {
                store.src()
            };
            let dest = if store.dest() == rep {
                rep_with
            } else {
                store.dest()
            };
            data.replace_inst_with(used_by).store(src, dest);
        }
        InstKind::MemZero(mem_zero) => {
            let dest = if mem_zero.dest() == rep {
                rep_with
            } else {
                mem_zero.dest()
            };
            let byte_len = mem_zero.byte_len();
            data.replace_inst_with(used_by).mem_zero(dest, byte_len);
        }
        InstKind::GetElemPtr(get_elem_ptr) => {
            if get_elem_ptr.base() == rep || get_elem_ptr.offsets().contains(&rep) {
                let mut rep_with_vec = Vec::with_capacity(get_elem_ptr.offsets().len());
                for &offset in get_elem_ptr.offsets() {
                    if offset == rep {
                        rep_with_vec.push(rep_with);
                    } else {
                        rep_with_vec.push(offset);
                    }
                }
                let base = if get_elem_ptr.base() == rep {
                    rep_with
                } else {
                    get_elem_ptr.base()
                };
                data.replace_inst_with(used_by)
                    .get_elem_ptr(base, rep_with_vec);
            }
        }
        InstKind::Binary(binary) => {
            let lhs = if binary.lhs() == rep {
                rep_with
            } else {
                binary.lhs()
            };
            let rhs = if binary.rhs() == rep {
                rep_with
            } else {
                binary.rhs()
            };
            let op = binary.op();
            // info!("old data: {used_by} {:?}", data.inst_data(used_by));
            // info!("to replace: {rep} {:?}", data.inst_data(rep));
            // info!("replace with: {rep_with} {:?}", data.inst_data(rep_with));
            // info!("");
            data.replace_inst_with(used_by).binary(op, lhs, rhs);
        }
        InstKind::Select(select) => {
            let cond = if select.cond() == rep {
                rep_with
            } else {
                select.cond()
            };
            let if_true = if select.if_true() == rep {
                rep_with
            } else {
                select.if_true()
            };
            let if_false = if select.if_false() == rep {
                rep_with
            } else {
                select.if_false()
            };
            data.replace_inst_with(used_by)
                .select(cond, if_true, if_false);
        }
        InstKind::Branch(branch) => {
            let cond = if branch.cond() == rep {
                rep_with
            } else {
                branch.cond()
            };
            if let InstKind::Integer(int) = data.inst_data(cond).kind() {
                let (target, args) = if int.value() == 0 {
                    (branch.f_target(), branch.f_args().to_vec())
                } else {
                    (branch.t_target(), branch.t_args().to_vec())
                };
                data.replace_inst_with(used_by).jump(target, args);
            } else {
                let t_args = branch
                    .t_args()
                    .iter()
                    .map(|&val| if val == rep { rep_with } else { val })
                    .collect();
                let f_args = branch
                    .f_args()
                    .iter()
                    .map(|&val| if val == rep { rep_with } else { val })
                    .collect();
                let (t_target, f_target) = (branch.t_target(), branch.f_target());
                data.replace_inst_with(used_by)
                    .branch(cond, t_target, t_args, f_target, f_args);
            }
        }
        InstKind::Jump(jump) => {
            let args = jump
                .args()
                .iter()
                .map(|&val| if val == rep { rep_with } else { val })
                .collect();
            let target = jump.target();
            data.replace_inst_with(used_by).jump(target, args);
        }
        InstKind::Call(call) => {
            let args = call
                .args()
                .iter()
                .map(|&val| if val == rep { rep_with } else { val })
                .collect();
            let callee = call.callee();
            data.replace_inst_with(used_by).call(callee, args);
        }
        InstKind::TailCall(tail_call) => {
            let args = tail_call
                .args()
                .iter()
                .map(|&val| if val == rep { rep_with } else { val })
                .collect();
            let callee = tail_call.callee();
            data.replace_inst_with(used_by).tail_call(callee, args);
        }
        InstKind::Return(ret) => {
            if ret.value() == Some(rep) {
                data.replace_inst_with(used_by).ret(Some(rep_with));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::visit_and_replace;
    use crate::{
        ir::{Program, Type, arena::Arena, builder_trait::*},
        opt::pass::ArenaContextMut,
    };

    #[test]
    fn replacement_rebuilds_mem_zero_with_the_new_destination() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "clear".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let old_dest = data.new_local_inst().alloc(Type::get_i32());
        let new_dest = data.new_local_inst().alloc(Type::get_i32());
        let clear = data.new_local_inst().mem_zero(old_dest, 4);
        data.layout_mut().insert_inst(entry, clear);

        let mut context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        visit_and_replace(&mut context, old_dest, new_dest);

        let crate::ir::InstKind::MemZero(mem_zero) = context.inst_data(clear).kind() else {
            panic!("expected memzero")
        };
        assert_eq!(mem_zero.dest(), new_dest);
        assert_eq!(mem_zero.byte_len(), 4);
        assert!(context.inst_data(new_dest).used_by().contains(&clear));
    }
}
