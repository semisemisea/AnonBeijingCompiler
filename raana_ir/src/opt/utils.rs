use std::collections::{HashMap, hash_map::Entry};

use crate::{
    ir::{
        BasicBlock, FunctionData, Inst, InstKind, Type, TypeKind,
        arena::Arena,
        builder_trait::{LocalInstBuilder, ScalarInstBuilder},
    },
    opt::pass::ArenaContext,
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

/// The `i32` an instruction denotes, when it is an integer literal.
///
/// Generic over the arena so that IR passes and machine-code backends ask the
/// question exactly once: `taki_mir` and the targets hold a read-only
/// `ArenaContext`, passes hold a mutable one, and both satisfy [`Arena`].
#[inline]
pub fn integer_constant<A: Arena + ?Sized>(arena: &A, inst: Inst) -> Option<i32> {
    let InstKind::Integer(integer) = arena.inst_data(inst).kind() else {
        return None;
    };
    Some(integer.value())
}

/// Classify `value` as `±(1 << shift)`, returning `(shift, is_negative)`.
///
/// Division and remainder by such a constant have shift-and-mask forms in both
/// the IR (strength reduction) and the backends, so the classification lives
/// here rather than being restated per consumer. `0` is rejected because it is
/// not a power of two; `i32::MIN` classifies as `-(1 << 31)`.
pub fn signed_power_of_two(value: i32) -> Option<(u8, bool)> {
    if value == 0 {
        return None;
    }
    let magnitude = value.unsigned_abs();
    magnitude
        .is_power_of_two()
        .then(|| (magnitude.trailing_zeros() as u8, value.is_negative()))
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

pub fn visit_and_replace(data: &mut ArenaContext<'_>, rep: Inst, rep_with: Inst) {
    let list = data.inst_data(rep).used_by().iter().copied().collect_vec();
    for used_by in list {
        visit_and_replace_single(data, used_by, rep, rep_with);
    }
}

fn visit_and_replace_single(data: &mut ArenaContext<'_>, used_by: Inst, rep: Inst, rep_with: Inst) {
    let rep_val_data = data.inst_data(used_by);
    #[allow(unused_variables)]
    match rep_val_data.kind() {
        InstKind::ZeroInit
        | InstKind::Undef
        | InstKind::FuncArgRef(..)
        | InstKind::BlockArgRef(..)
        | InstKind::Alloc
        | InstKind::GlobalAlloc(..) => unreachable!("Encountered kind: {:?}", rep_val_data.kind()),
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
        InstKind::Return(ret) => {
            if ret.value() == Some(rep) {
                data.replace_inst_with(used_by).ret(Some(rep_with));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{signed_power_of_two, visit_and_replace};
    use crate::{
        ir::{Program, Type, arena::Arena, builder_trait::*},
        opt::pass::ArenaContext,
    };

    #[test]
    fn classifies_signed_power_of_two_divisors() {
        assert_eq!(signed_power_of_two(0), None);
        assert_eq!(signed_power_of_two(1), Some((0, false)));
        assert_eq!(signed_power_of_two(-1), Some((0, true)));
        assert_eq!(signed_power_of_two(2), Some((1, false)));
        assert_eq!(signed_power_of_two(-2), Some((1, true)));
        assert_eq!(signed_power_of_two(1 << 30), Some((30, false)));
        assert_eq!(signed_power_of_two(-(1 << 30)), Some((30, true)));
        assert_eq!(signed_power_of_two(i32::MIN), Some((31, true)));
        assert_eq!(signed_power_of_two(3), None);
        assert_eq!(signed_power_of_two(-3), None);
    }

    #[test]
    fn signed_power_of_two_formula_truncates_toward_zero() {
        let dividends = [i32::MIN, -17, -9, -8, -7, -1, 0, 1, 7, 8, 9, 17, i32::MAX];
        let divisors = [1, -1, 2, -2, 4, -4, 8, -8, 1 << 30, -(1 << 30), i32::MIN];

        for dividend in dividends {
            for divisor in divisors {
                let (shift, negate) = signed_power_of_two(divisor).unwrap();
                let positive_quotient = if shift == 0 {
                    dividend
                } else {
                    let sign = dividend >> 31;
                    let bias = ((sign as u32) >> (32 - shift)) as i32;
                    dividend.wrapping_add(bias) >> shift
                };
                let quotient = if negate {
                    positive_quotient.wrapping_neg()
                } else {
                    positive_quotient
                };
                let remainder =
                    dividend.wrapping_sub(positive_quotient.wrapping_shl(u32::from(shift)));

                let expected_quotient = if dividend == i32::MIN && divisor == -1 {
                    i32::MIN
                } else {
                    dividend / divisor
                };
                let expected_remainder = if divisor == -1 { 0 } else { dividend % divisor };
                assert_eq!(quotient, expected_quotient, "{dividend} / {divisor}");
                assert_eq!(remainder, expected_remainder, "{dividend} % {divisor}");
            }
        }
    }

    #[test]
    fn replacement_rebuilds_mem_zero_with_the_new_destination() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "clear".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let old_dest = data.new_local_inst().alloc(Type::get_i32());
        let new_dest = data.new_local_inst().alloc(Type::get_i32());
        let clear = data.new_local_inst().mem_zero(old_dest, 4);
        data.layout_mut().insert_inst(entry, clear);

        let mut context = ArenaContext {
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
