#![allow(unused)]
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Write,
    num::NonZeroU32,
};

use log::{debug, info};

use crate::{
    ir::{
        BasicBlock, BinaryOp, Function, FunctionData, Inst, InstData, InstKind, Program,
        arena::Arena,
        builder_trait::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
    },
    opt::pass::ArenaContext,
    // opt::{FunctionPass, ModulePass},
};

use crate::opt::{
    pass::Pass,
    utils::{BId, IDAllocator, VId, build_cfg_both},
};

pub struct DeadPhiElimination;
pub struct DeadCodeElimination;
pub struct UnreachableBasicBlock;
pub struct JumpOnlyElimination;

const REMOVE_FLAG: bool = true;

/// Mark and sweep algorithm
/// To start the process, we mark all the useful instructions, including:
/// I/O
/// Function (call to function)
/// Branches and Return
impl Pass for DeadCodeElimination {
    fn run(&self, program: &mut Program) {
        let mut val_id = IDAllocator::new(1);
        let funcs = program.global_arena().func_arena().funcs();
        let mut arena_context = ArenaContext {
            program,
            curr_func: None,
        };
        for func in funcs {
            arena_context.curr_func = Some(func);
            self.run_on_func(&mut arena_context, &mut val_id);
        }
    }
}

// TODO: side-effet function rules.
fn has_side_effect(func: Function) -> bool {
    true
}

#[inline]
fn is_critical(value: Inst, data: &FunctionData) -> bool {
    match data.inst_data(value).kind() {
        InstKind::Branch(..) | InstKind::Jump(..) | InstKind::Store(..) | InstKind::Return(..) => {
            true
        }
        InstKind::GlobalAlloc(..)
        | InstKind::BlockArgRef(..)
        | InstKind::FuncArgRef(..)
        | InstKind::Aggregate(..)
        | InstKind::Undef
        | InstKind::ZeroInit
        | InstKind::Integer(..) => unreachable!(),
        InstKind::Cast(..)
        | InstKind::Float(..)
        | InstKind::Alloc
        | InstKind::Load(..)
        | InstKind::GetPtr(..)
        | InstKind::GetElemPtr(..)
        | InstKind::Binary(..) => false,
        // rdf is not ready
        InstKind::Call(call) => has_side_effect(call.callee()),
    }
}

#[inline]
fn exists_in_layout(value: Inst, data: &FunctionData) -> bool {
    matches!(
        data.inst_data(value).kind(),
        InstKind::Binary(..)
            | InstKind::Load(..)
            | InstKind::GetPtr(..)
            | InstKind::GetElemPtr(..)
            | InstKind::Alloc
            | InstKind::Call(..)
    )
}

impl DeadCodeElimination {
    pub(crate) fn run_on_func(
        &self,
        data: &mut ArenaContext<'_>,
        val_id: &mut IDAllocator<Inst, VId>,
    ) {
        let mut worklist = VecDeque::new();
        let mut live_inst = HashSet::new();

        macro_rules! mark_live {
            ($inst: expr) => {
                if !live_inst.contains(&$inst) && !$inst.is_global() {
                    worklist.push_back($inst);
                    live_inst.insert($inst);
                }
            };
        }

        // 1.1 Mark: Initiate
        for layout in data.layout().basicblocks() {
            for &inst in layout.insts() {
                if is_critical(inst, data) {
                    mark_live!(inst);
                }
            }
        }

        // 1.2 Mark: Grow
        while let Some(inst) = worklist.pop_front() {
            match data.inst_data(inst).kind() {
                InstKind::GlobalAlloc(..)
                | InstKind::Alloc
                | InstKind::BlockArgRef(..)
                | InstKind::FuncArgRef(..)
                | InstKind::Aggregate(..)
                | InstKind::Undef
                | InstKind::ZeroInit
                | InstKind::Float(..)
                | InstKind::Integer(..) => continue,
                InstKind::Cast(cast) => mark_live!(cast.src()),
                InstKind::Return(ret) => {
                    if let Some(inst) = ret.value() {
                        mark_live!(inst);
                    }
                }
                InstKind::Store(store) => {
                    mark_live!(store.src());
                    mark_live!(store.dest());
                }
                InstKind::Load(load) => {
                    mark_live!(load.src());
                }
                InstKind::GetPtr(get_ptr) => {
                    mark_live!(get_ptr.base());
                    mark_live!(get_ptr.offset());
                }
                InstKind::GetElemPtr(get_elem_ptr) => {
                    mark_live!(get_elem_ptr.base());
                    mark_live!(get_elem_ptr.offset());
                }
                InstKind::Binary(binary) => {
                    mark_live!(binary.lhs());
                    mark_live!(binary.rhs());
                }
                InstKind::Branch(branch) => {
                    mark_live!(branch.cond());
                    for &ta in branch.t_args() {
                        mark_live!(ta);
                    }
                    for &fa in branch.f_args() {
                        mark_live!(fa);
                    }
                }
                InstKind::Jump(jump) => {
                    for &ja in jump.args() {
                        mark_live!(ja)
                    }
                }
                InstKind::Call(call) => {
                    for &ca in call.args() {
                        mark_live!(ca)
                    }
                }
            }
        }

        // 2 Sweep
        let mut rename_list = Vec::new();
        for layout in data.layout().basicblocks() {
            rename_list.extend(
                layout
                    .insts()
                    .iter()
                    .copied()
                    .filter(|inst| !live_inst.contains(inst))
                    .zip(std::iter::repeat(layout.bb())),
            );
        }
        for (inst, bb) in rename_list {
            data.layout_mut().remove_inst(bb, inst);
        }
    }
}

impl Pass for DeadPhiElimination {
    fn run_on(&self, data: &mut ArenaContext<'_>) {
        let mut bb_allocator: IDAllocator<BasicBlock, BId> = IDAllocator::new(1);
        let mut unused_params_indices = Vec::with_capacity(data.layout().basicblocks().len());

        for (assert_id, layout) in data.layout().basicblocks().iter().enumerate() {
            assert_eq!(bb_allocator.check_or_alloc_id_same(layout.bb()), assert_id);
            let params = data.bb_data(layout.bb()).params();
            let unused_params_index = (0..params.len())
                .filter(|&index| data.inst_data(params[index]).used_by().is_empty())
                .rev()
                .collect::<Vec<_>>();
            unused_params_indices.push(unused_params_index);
        }
        for (i, unused_params_index) in unused_params_indices.into_iter().enumerate() {
            let bb = bb_allocator.search_id(i);

            for &index in unused_params_index.iter() {
                let val = data.bb_data_mut(bb).params_mut().swap_remove(index);
            }

            let jump_inst = data
                .bb_data(bb)
                .used_by()
                .iter()
                .copied()
                // TODO: You can remove this if you correctly implement inst data remove.
                .filter(|&inst| data.layout().parent_bb(inst).is_some())
                .collect::<Vec<_>>();

            for inst in jump_inst {
                match data.inst_data(inst).kind() {
                    InstKind::Jump(jump) => {
                        let t = jump.target();
                        let mut a = jump.args().to_vec();
                        for &index in unused_params_index.iter() {
                            a.swap_remove(index);
                        }
                        data.replace_inst_with(inst).jump(t, a);
                    }
                    InstKind::Branch(branch) => {
                        let c = branch.cond();
                        let tt = branch.t_target();
                        let ft = branch.f_target();
                        let mut ta = branch.t_args().to_vec();
                        let mut fa = branch.f_args().to_vec();
                        if bb == branch.t_target() {
                            for &index in unused_params_index.iter() {
                                ta.swap_remove(index);
                            }
                        } else {
                            for &index in unused_params_index.iter() {
                                fa.swap_remove(index);
                            }
                        }
                        data.replace_inst_with(inst).branch(c, tt, ta, ft, fa);
                    }
                    _ => unreachable!(),
                }
            }
        }
    }
}

impl Pass for UnreachableBasicBlock {
    fn run_on(&self, data: &mut ArenaContext<'_>) {
        if data.layout().entry_bb().is_none() {
            return;
        }
        loop {
            let mut id_allocator = IDAllocator::new(1);
            let (g, prece) = build_cfg_both(data, &mut id_allocator);

            let unreachable_bb = (1..id_allocator.cnt())
                // in-degree is zero.
                .filter(|id| prece[id].is_empty())
                .collect::<Vec<_>>();

            debug!("g:{:?}", g);
            debug!("prece:{:?}", prece);
            debug!("unreachable_bb:{:?}", unreachable_bb);

            if unreachable_bb.is_empty() && id_allocator.cnt() == data.layout().basicblocks().len()
            {
                break;
            }

            let mut island = Vec::new();
            for layout in data.layout().basicblocks() {
                if id_allocator.get_id_safe(&layout.bb()).is_none() {
                    island.push(layout.bb());
                }
            }

            for bb in island {
                data.layout_mut().remove_basicblock(bb);
            }

            for id in unreachable_bb {
                let bb = id_allocator.search_id(id);
                data.layout_mut().remove_basicblock(bb);
            }
        }
    }
}

// fn dfs_remove(val: Inst, data: &mut FunctionData, bb: BasicBlock) {
//     let mut remove_list = Vec::new();
//     _dfs_remove(val, data, &mut remove_list);
//     for val in remove_list.into_iter().rev() {
//         eprintln!("remove:{val:?}");
//         data.layout_mut().bb_mut(bb).insts_mut().remove(&val);
//         data.dfg_mut().remove_value(val);
//     }
// }
//
// fn _dfs_remove(val: Inst, data: &FunctionData, remove_list: &mut Vec<Inst>) {
//     let vd = data.dfg().value(val);
//     remove_list.push(val);
//     for &child in vd.used_by().iter() {
//         _dfs_remove(child, data, remove_list);
//     }
// }
//
// #[inline]
// fn is_jump_inst(val: Inst, data: &FunctionData) -> bool {
//     matches!(data.dfg().value(val).kind(), InstKind::Jump(..))
// }
//
// impl Pass for JumpOnlyElimination {
//     fn run_on(&mut self, func: Function, data: &mut FunctionData) {
//         let Some(entry_bb) = data.layout().entry_bb() else {
//             return;
//         };
//         // let virtual_entry_bb = data
//         //     .dfg_mut()
//         //     .new_bb()
//         //     .basic_block(Some("%v_entry".to_string()));
//         // data.layout_mut().bbs_mut().push_key_front(virtual_entry_bb);
//         // let jump = data.dfg_mut().new_value().jump(entry_bb);
//         // data.layout_mut()
//         //     .bb_mut(virtual_entry_bb)
//         //     .insts_mut()
//         //     .push_key_back(jump)
//         //     .unwrap();
//         let worklist = data
//             .layout()
//             .bbs()
//             .iter()
//             .filter(|&(&bb, node)| {
//                 eprintln!("{:?}", data.dfg().bb(bb).name());
//                 let val = *node.insts().front_key().unwrap();
//                 if let InstKind::Jump(jump) = data.dfg().value(val).kind() {
//                     eprintln!("1");
//                     eprintln!("{:?} {:?}", jump.args(), data.dfg().bb(bb).params());
//                     jump.args().is_empty() && data.dfg().bb(bb).params().is_empty()
//                     // jump.args().iter().eq(data.dfg().bb(bb).params())
//                 } else {
//                     false
//                 }
//             })
//             .map(|(&bb, node)| bb)
//             .collect::<Vec<_>>();
//
//         for bb in worklist.into_iter().rev() {
//             eprintln!("{:?}", data.dfg().bb(bb).name());
//             let node = data.layout().bbs().node(&bb).unwrap();
//             let prev_jump_insts = data
//                 .dfg()
//                 .bb(bb)
//                 .used_by()
//                 .iter()
//                 .copied()
//                 .collect::<Vec<_>>();
//
//             let target_bb = if let InstKind::Jump(jump) =
//                 data.dfg().value(*node.insts().front_key().unwrap()).kind()
//             {
//                 jump.target()
//             } else {
//                 unreachable!()
//             };
//
//             for prev_jump_inst in prev_jump_insts {
//                 match data.dfg_mut().value_mut(prev_jump_inst).kind_mut() {
//                     InstKind::Jump(jump) => *jump.target_mut() = target_bb,
//                     InstKind::Branch(branch) => {
//                         if branch.true_bb() == bb {
//                             *branch.true_bb_mut() = target_bb;
//                         } else {
//                             *branch.false_bb_mut() = target_bb;
//                         }
//                     }
//                     _ => unreachable!(),
//                 }
//             }
//
//             // if data.layout().entry_bb().unwrap() != bb {
//             data.layout_mut().bbs_mut().remove(bb);
//             // } else {
//             //     let (key, node) = data.layout_mut().bbs_mut().remove(&target_bb).unwrap();
//             //     data.layout_mut().bbs_mut().remove(&bb);
//             //     data.layout_mut().bbs_mut().push_front(key, node);
//             // }
//         }
//         // data.layout_mut().bbs_mut().pop_front();
//         // for (&bb, data) in data.layout().
//         // data.layout_mut().bbs_mut().pusf
//     }
// }
