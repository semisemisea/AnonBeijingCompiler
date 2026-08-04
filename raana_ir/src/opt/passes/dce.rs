use crate::opt::{
    analysis_passes::effects::EffectAnalysis,
    prelude::*,
};

pub struct DeadPhiElimination;
pub struct DeadCodeElimination;
pub struct UnreachableBasicBlock;
pub struct JumpOnlyElimination;

/// Mark and sweep algorithm
/// To start the process, we mark all the useful instructions, including:
/// I/O
/// Function (call to function)
/// Branches and Return
impl Pass for DeadCodeElimination {
    fn run(&mut self, program: &mut Program) -> bool {
        // Whole-program purity analysis lets the mark phase drop calls to
        // effect-free callees whose result is unused (getint and friends
        // are preserved through the I/O flags).
        let analysis = EffectAnalysis::new(program);
        let mut changed = false;
        for func in program.function_layout().to_vec() {
            let mut removable_calls = HashSet::default();
            let data = program.func_data(func);
            for bb_layout in data.layout().basicblocks() {
                for &inst in bb_layout.insts() {
                    if let InstKind::Call(call) = data.inst_data(inst).kind() {
                        if analysis.is_removable(call.callee()) {
                            removable_calls.insert(inst);
                        }
                    }
                }
            }
            let mut arena_context = ArenaContextMut {
                program,
                curr_func: Some(func),
            };
            changed |= DeadCodeElimination::run_on_func(
                self,
                &mut arena_context,
                &removable_calls,
            );
        }
        changed
    }

    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        // Direct per-function invocation without the purity analysis: no
        // call is removable (the historical conservative behavior).
        self.run_on_func(data, &HashSet::default())
    }
}

// TODO: side-effet function rules.
#[allow(dead_code)]
fn has_side_effect(_func: Function) -> bool {
    true
}

#[inline]
fn is_critical(value: Inst, data: &FunctionData, removable_calls: &HashSet<Inst>) -> bool {
    match data.inst_data(value).kind() {
        InstKind::Branch(..)
        | InstKind::Jump(..)
        | InstKind::Store(..)
        | InstKind::MemZero(..)
        | InstKind::Return(..)
        | InstKind::TailCall(..) => true,
        InstKind::GlobalAlloc(..)
        | InstKind::BlockArgRef(..)
        | InstKind::Aggregate(..)
        | InstKind::Undef
        | InstKind::ZeroInit
        | InstKind::Integer(..) => unreachable!(),
        InstKind::Cast(..)
        | InstKind::Float(..)
        | InstKind::Alloc
        | InstKind::Load(..)
        | InstKind::GetElemPtr(..)
        | InstKind::Binary(..)
        | InstKind::Select(..)
        | InstKind::Fma(..)
        | InstKind::VectorSplat(..)
        | InstKind::VectorExtractElement(..)
        | InstKind::VectorInsertElement(..)
        | InstKind::VectorReduce(..) => false,
        // rdf is not ready
        InstKind::Call(..) => !removable_calls.contains(&value),
    }
}

impl DeadCodeElimination {
    pub(crate) fn run_on_func(
        &self,
        data: &mut ArenaContextMut<'_>,
        removable_calls: &HashSet<Inst>,
    ) -> bool {
        let mut worklist = VecDeque::new();
        let mut live_inst = HashSet::default();

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
                if is_critical(inst, data, removable_calls) {
                    mark_live!(inst);
                }
            }
        }

        // 1.2 Mark: Grow
        while let Some(inst) = worklist.pop_front() {
            // An earlier pass (e.g. interprocedural SCCP) may have replaced
            // an instruction and left a dangling reference in another inst's
            // operand list. Skip insts that no longer exist.
            if !data.has_inst_data(inst) && !inst.is_global() {
                continue;
            }
            match data.inst_data(inst).kind() {
                InstKind::GlobalAlloc(..)
                | InstKind::Alloc
                | InstKind::BlockArgRef(..)
                | InstKind::Undef
                | InstKind::ZeroInit
                | InstKind::Float(..)
                | InstKind::Integer(..) => continue,
                InstKind::Aggregate(agg) => {
                    for &elem in agg.value() {
                        mark_live!(elem);
                    }
                }
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
                InstKind::MemZero(mem_zero) => {
                    mark_live!(mem_zero.dest());
                    if let crate::ir::inst_kind::mem_zero::MemZeroLen::Value(byte_len) =
                        mem_zero.byte_len_len()
                    {
                        mark_live!(*byte_len);
                    }
                }
                InstKind::Load(load) => {
                    mark_live!(load.src());
                }
                InstKind::GetElemPtr(get_elem_ptr) => {
                    mark_live!(get_elem_ptr.base());
                    get_elem_ptr
                        .offsets()
                        .iter()
                        .for_each(|&inst| mark_live!(inst));
                }
                InstKind::Binary(binary) => {
                    mark_live!(binary.lhs());
                    mark_live!(binary.rhs());
                }
                InstKind::Select(select) => {
                    mark_live!(select.cond());
                    mark_live!(select.if_true());
                    mark_live!(select.if_false());
                }
                InstKind::Fma(fma) => {
                    mark_live!(fma.acc());
                    mark_live!(fma.lhs());
                    mark_live!(fma.rhs());
                }
                InstKind::VectorSplat(splat) => mark_live!(splat.src()),
                InstKind::VectorExtractElement(extract) => {
                    mark_live!(extract.src());
                    mark_live!(extract.index());
                }
                InstKind::VectorInsertElement(insert) => {
                    mark_live!(insert.vector());
                    mark_live!(insert.element());
                    mark_live!(insert.index());
                }
                InstKind::VectorReduce(reduce) => mark_live!(reduce.src()),
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
                InstKind::TailCall(tail_call) => {
                    for &arg in tail_call.args() {
                        mark_live!(arg)
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
        let changed = !rename_list.is_empty();
        for (inst, bb) in rename_list {
            data.remove_layout_inst(bb, inst);
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::{DeadCodeElimination, Pass};
    use crate::{
        ir::{Program, Type, arena::Arena, builder_trait::*},
        opt::pass::ArenaContextMut,
    };

    /// A function with an entry block and a bare `ret`.
    fn empty_function(program: &mut Program, name: &str) -> crate::ir::Function {
        let function = program.new_function(Type::get_unit(), name.into(), vec![]);
        let mut data = ArenaContextMut {
            program,
            curr_func: Some(function),
        };
        let entry = data.add_entry_block();
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);
        function
    }

    #[test]
    fn preserves_mem_zero_and_its_allocation() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "clear".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let alloc = data
            .new_local_inst()
            .alloc(Type::get_array(Type::get_i32(), 4));
        let clear = data.new_local_inst().mem_zero(alloc, 16);
        data.layout_mut().insert_inst(entry, clear);
        let one = data.new_local_inst().integer(1);
        let dead = data
            .new_local_inst()
            .binary(crate::ir::BinaryOp::Add, one, one);
        data.layout_mut().insert_inst(entry, dead);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        assert!(DeadCodeElimination.run(&mut program));
        let data = program.func_data(function);
        let insts = data.layout().basicblock(entry).insts();
        assert!(insts.iter().any(|&inst| inst == clear));
        assert!(!insts.iter().any(|&inst| inst == dead));
        assert!(data.inst_data(alloc).used_by().contains(&clear));
    }

    #[test]
    fn removes_call_to_pure_function_with_unused_result() {
        let mut program = Program::new();
        let callee = empty_function(&mut program, "callee");
        let caller = program.new_function(Type::get_unit(), "caller".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(caller),
        };
        let entry = data.add_entry_block();
        let call = data.new_local_value().call(callee, vec![]);
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        assert!(DeadCodeElimination.run(&mut program));
        let data = program.func_data(caller);
        let insts = data.layout().basicblock(entry).insts();
        assert!(!insts.iter().any(|&inst| inst == call));
        assert_eq!(insts.len(), 1); // only the ret remains
    }

    #[test]
    fn keeps_call_to_io_function() {
        let mut program = Program::new();
        // A declaration is enough: the analysis keys on the callee name.
        let getint = program.new_function(Type::get_i32(), "getint".into(), vec![]);
        let caller = program.new_function(Type::get_unit(), "caller".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(caller),
        };
        let entry = data.add_entry_block();
        let call = data.new_local_value().call(getint, vec![]);
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        assert!(!DeadCodeElimination.run(&mut program));
        let data = program.func_data(caller);
        assert!(data
            .layout()
            .basicblock(entry)
            .insts()
            .iter()
            .any(|&inst| inst == call));
    }

    #[test]
    fn keeps_call_to_function_writing_a_global() {
        let mut program = Program::new();
        let init = program.new_value().zero_init(Type::get_i32());
        let global = program.new_value().global_alloc(init);
        let writer = program.new_function(Type::get_unit(), "writer".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(writer),
        };
        let entry = data.add_entry_block();
        let one = data.new_local_value().integer(1);
        let store = data.new_local_value().store(one, global);
        data.layout_mut().insert_inst(entry, store);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let caller = program.new_function(Type::get_unit(), "caller".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(caller),
        };
        let entry = data.add_entry_block();
        let call = data.new_local_value().call(writer, vec![]);
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        assert!(!DeadCodeElimination.run(&mut program));
        let data = program.func_data(caller);
        assert!(data
            .layout()
            .basicblock(entry)
            .insts()
            .iter()
            .any(|&inst| inst == call));
    }

    #[test]
    fn keeps_call_whose_result_is_used() {
        let mut program = Program::new();
        let callee = program.new_function(Type::get_i32(), "callee".into(), vec![]);
        {
            let mut data = ArenaContextMut {
                program: &mut program,
                curr_func: Some(callee),
            };
            let entry = data.add_entry_block();
            let one = data.new_local_value().integer(1);
            let ret = data.new_local_value().ret(Some(one));
            data.layout_mut().insert_inst(entry, ret);
        }
        let caller = program.new_function(Type::get_i32(), "caller".into(), vec![]);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(caller),
        };
        let entry = data.add_entry_block();
        let call = data.new_local_value().call(callee, vec![]);
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_value().ret(Some(call));
        data.layout_mut().insert_inst(entry, ret);

        assert!(!DeadCodeElimination.run(&mut program));
        let data = program.func_data(caller);
        assert!(data
            .layout()
            .basicblock(entry)
            .insts()
            .iter()
            .any(|&inst| inst == call));
    }

    #[test]
    fn removes_pure_unused_calls_but_keeps_io_calls() {
        let mut program = Program::new();
        // A strictly pure callee with no body at all.
        let pure_callee = program.new_function(Type::get_i32(), "pure".into(), vec![]);
        let pure_data = program.func_data_mut(pure_callee);
        let pure_entry = pure_data.add_entry_block();
        // Constants stay outside the block layout; only the return is laid out.
        let one = pure_data.new_local_inst().integer(1);
        let pure_ret = pure_data.new_local_inst().ret(Some(one));
        pure_data.layout_mut().insert_inst(pure_entry, pure_ret);

        // A library I/O function (no body; identified by name).
        let io_callee = program.new_function(Type::get_i32(), "getint".into(), vec![]);

        let function = program.new_function(Type::get_unit(), "caller".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let pure_call = data
            .new_local_inst()
            .call_with_type(pure_callee, vec![], Type::get_i32());
        data.layout_mut().insert_inst(entry, pure_call);
        let io_call = data
            .new_local_inst()
            .call_with_type(io_callee, vec![], Type::get_i32());
        data.layout_mut().insert_inst(entry, io_call);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        assert!(DeadCodeElimination.run(&mut program));
        let data = program.func_data(function);
        let insts = data.layout().basicblock(entry).insts();
        assert!(
            !insts.iter().any(|&inst| inst == pure_call),
            "unused pure call must be removed"
        );
        assert!(
            insts.iter().any(|&inst| inst == io_call),
            "I/O call must be kept"
        );
    }

    #[test]
    fn keeps_pure_calls_whose_result_is_used() {
        let mut program = Program::new();
        let pure_callee = program.new_function(Type::get_i32(), "pure".into(), vec![]);
        let pure_data = program.func_data_mut(pure_callee);
        let pure_entry = pure_data.add_entry_block();
        let one = pure_data.new_local_inst().integer(1);
        let pure_ret = pure_data.new_local_inst().ret(Some(one));
        pure_data.layout_mut().insert_inst(pure_entry, pure_ret);

        let function = program.new_function(Type::get_i32(), "caller".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let pure_call = data
            .new_local_inst()
            .call_with_type(pure_callee, vec![], Type::get_i32());
        data.layout_mut().insert_inst(entry, pure_call);
        let ret = data.new_local_inst().ret(Some(pure_call));
        data.layout_mut().insert_inst(entry, ret);

        assert!(!DeadCodeElimination.run(&mut program));
        let data = program.func_data(function);
        let insts = data.layout().basicblock(entry).insts();
        assert!(insts.iter().any(|&inst| inst == pure_call));
    }
}

impl Pass for DeadPhiElimination {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        let mut bb_allocator: IDAllocator<BasicBlock, BId> = IDAllocator::new(1);
        let mut unused_params_indices = Vec::with_capacity(data.layout().basicblocks().len());

        let entry_bb = data.layout().entry_bb().map(|l| l.bb());
        for (assert_id, layout) in data.layout().basicblocks().iter().enumerate() {
            assert_eq!(bb_allocator.check_or_alloc_id_same(layout.bb()), assert_id);
            // Entry block parameters are the function's ABI parameters and are
            // tracked by FunctionData::params independently of the block's
            // params list. Removing one here would desynchronize the two,
            // leaving a dangling reference. Keep them all.
            if Some(layout.bb()) == entry_bb {
                unused_params_indices.push(Vec::new());
                continue;
            }
            let params = data.bb_data(layout.bb()).params();
            let unused_params_index = (0..params.len())
                .filter(|&index| data.inst_data(params[index]).used_by().is_empty())
                .rev()
                .collect::<Vec<_>>();
            unused_params_indices.push(unused_params_index);
        }
        let mut changed = false;
        for (i, unused_params_index) in unused_params_indices.into_iter().enumerate() {
            if unused_params_index.is_empty() {
                continue;
            }
            changed = true;
            let bb = bb_allocator.search_id(i);

            // `unused_params_index` is descending, so positional `remove`
            // keeps the remaining parameters in their original relative
            // order. `swap_remove` would also work positionally, but it
            // reorders the tail and every predecessor's argument vector
            // must be permuted identically for the block's phi values to
            // stay aligned across repeated runs.
            for &index in unused_params_index.iter() {
                let _val = data.bb_data_mut(bb).params_mut().remove(index);
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
                            a.remove(index);
                        }
                        data.replace_inst_with(inst).jump(t, a);
                    }
                    InstKind::Branch(branch) => {
                        let c = branch.cond();
                        let tt = branch.t_target();
                        let ft = branch.f_target();
                        let mut ta = branch.t_args().to_vec();
                        let mut fa = branch.f_args().to_vec();
                        // The same block may be both the true and the false
                        // target of a branch; trim each side independently so
                        // the rebuilt branch keeps args aligned with params.
                        if bb == branch.t_target() {
                            for &index in unused_params_index.iter() {
                                ta.remove(index);
                            }
                        }
                        if bb == branch.f_target() {
                            for &index in unused_params_index.iter() {
                                fa.remove(index);
                            }
                        }
                        data.replace_inst_with(inst).branch(c, tt, ta, ft, fa);
                    }
                    _ => unreachable!(),
                }
            }
        }
        changed
    }
}

#[cfg(test)]
mod dead_phi_tests {
    use super::{DeadPhiElimination, Pass};
    use crate::ir::{BinaryOp, InstKind, Program, Type, arena::Arena, builder_trait::*};

    #[test]
    fn removes_dead_param_from_both_same_target_branch_arms() {
        let mut program = Program::new();
        let function =
            program.new_function(Type::get_unit(), "dead_phi".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let merge = data
            .new_basic_block()
            .basic_block("merge".into(), vec![Type::get_i32()]);
        data.layout_mut().push_bb_back(merge);

        let cond = data.params()[0];
        let zero = data.new_local_inst().integer(0);
        let one = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(cond, merge, vec![zero], merge, vec![one]);
        data.layout_mut().insert_inst(entry, branch);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(merge, ret);

        assert!(DeadPhiElimination.run(&mut program));
        let data = program.func_data(function);
        assert!(data.bb_data(merge).params().is_empty());
        let InstKind::Branch(branch_data) = data.inst_data(branch).kind() else {
            panic!("entry terminator must remain a branch");
        };
        assert!(branch_data.t_args().is_empty());
        assert!(branch_data.f_args().is_empty());
    }

    #[test]
    fn jump_args_stay_aligned_when_trailing_params_are_dead() {
        // A block whose *trailing* parameters are dead: the jump arguments
        // must drop the same positions, keeping earlier args aligned.
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "dead_tail".into(),
            vec![Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let merge = data
            .new_basic_block()
            .basic_block(
                "merge".into(),
                vec![
                    Type::get_i32(),
                    Type::get_i32(),
                    Type::get_i32(),
                    Type::get_i32(),
                    Type::get_i32(),
                    Type::get_i32(),
                    Type::get_i32(),
                    Type::get_i32(),
                    Type::get_i32(),
                ],
                );
        data.layout_mut().push_bb_back(merge);
        let cond = data.params()[0];
        let one = data.new_local_inst().integer(1);
        let jump = data.new_local_inst().jump(merge, vec![one; 9]);
        data.layout_mut().insert_inst(entry, jump);
        // Only params 7 and 8 are unused; use the others so only the tail
        // two get removed.
        for i in 0..7 {
            let p = data.bb_data(merge).params()[i];
            let _use = data.new_local_inst().binary(BinaryOp::Add, p, one);
            data.layout_mut().insert_inst(entry, _use);
        }
        let _ = cond;
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        assert!(DeadPhiElimination.run(&mut program));
        let data = program.func_data(function);
        assert_eq!(data.bb_data(merge).params().len(), 7);
        // The jump into merge must still carry exactly 7 args.
        let mut found = false;
        for block in data.layout().basicblocks() {
            for &inst in block.insts() {
                if let InstKind::Jump(jump) = data.inst_data(inst).kind() {
                    if jump.target() == merge {
                        assert_eq!(jump.args().len(), 7);
                        found = true;
                    }
                }
            }
        }
        assert!(found);
    }
}

impl Pass for UnreachableBasicBlock {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().entry_bb().is_none() {
            return false;
        }
        let mut changed = false;
        loop {
            let mut id_allocator = IDAllocator::new(1);
            let (g, prece) = cfg::build_cfg_both(data, &mut id_allocator);

            let unreachable_bb = (1..id_allocator.cnt())
                // in-degree is zero.
                .filter(|id| prece[id].is_empty())
                .collect::<Vec<_>>();

            debug!("g:{:?}", g);
            debug!("prece:{:?}", prece);
            debug!("unreachable_bb:{:?}", unreachable_bb);

            if unreachable_bb.is_empty() && id_allocator.cnt() == data.layout().basicblocks().len()
            {
                return changed;
            }

            let mut island = Vec::new();
            for layout in data.layout().basicblocks() {
                if id_allocator.get_id_safe(&layout.bb()).is_none() {
                    // Only remove blocks whose instructions are all unused:
                    // an unreachable block may still feed values into
                    // reachable blocks (e.g. LICM-hoisted GEPs used by a
                    // surviving loop body), and removing it would leave
                    // dangling operands.
                    let all_unused = layout
                        .insts()
                        .iter()
                        .all(|&inst| data.inst_data(inst).used_by().is_empty());
                    if all_unused {
                        island.push(layout.bb());
                    }
                }
            }

            let mut removed_any = false;
            for bb in island {
                data.remove_layout_basicblock(bb);
                removed_any = true;
            }

            for id in unreachable_bb {
                let bb = id_allocator.search_id(id);
                let all_unused = data
                    .layout()
                    .basicblock(bb)
                    .insts()
                    .iter()
                    .all(|&inst| data.inst_data(inst).used_by().is_empty());
                if all_unused {
                    data.remove_layout_basicblock(bb);
                    removed_any = true;
                }
            }
            if removed_any {
                changed = true;
            } else {
                // Unreachable blocks remain but none are removable (their
                // values are still live). Further iterations cannot make
                // progress, so stop.
                return changed;
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

// TODO: This is SimplifyCFG. Please move to a single pass file.
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
