use rustc_hash::{FxHashMap, FxHashSet};

use crate::opt::{
    analysis_passes::{dom_tree::v2::DominanceTree, loop_analysis::Loop},
    prelude::*,
    utils::{
        cfg::CFG,
        preheader::{EnsurePreheader, ensure_preheader},
    },
};

/// Loop invariant code motion
pub struct LICM;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lattice {
    Variant,
    Invariant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopResult {
    Unchanged,
    Changed,
    CfgChanged,
}

impl LICM {
    fn solve(
        looop: &Loop,
        data: &mut ArenaContextMut<'_>,
        cfg: &CFG,
        dom_tree: &DominanceTree,
        parameter_blocks: &FxHashMap<Inst, BasicBlock>,
    ) -> LoopResult {
        fn insts(looop: &Loop, data: &ArenaContextMut<'_>) -> impl Iterator<Item = Inst> {
            looop
                .body()
                .iter()
                .flat_map(|&bb| data.layout().basicblock(bb).insts())
                .copied()
        }

        fn can_be_invariant(kind: &InstKind) -> bool {
            matches!(
                kind,
                InstKind::Integer(..)
                    | InstKind::Float(..)
                    | InstKind::Binary(..)
                    | InstKind::Cast(..)
                    | InstKind::GetElemPtr(..)
                    | InstKind::Select(..)
            )
        }

        fn is_integer_zero(data: &ArenaContextMut<'_>, inst: Inst) -> bool {
            matches!(data.inst_data(inst).kind(), InstKind::Integer(value) if value.value() == 0)
        }

        let loop_params = looop
            .body()
            .iter()
            .flat_map(|&block| data.bb_data(block).params().iter().copied())
            .collect::<FxHashSet<_>>();
        let loop_insts = insts(looop, data).collect::<Vec<_>>();
        let mut map = loop_insts
            .iter()
            .copied()
            .map(|inst| (inst, Lattice::Variant))
            .collect::<FxHashMap<_, _>>();

        let operand_is_invariant = |operand: Inst, states: &FxHashMap<Inst, Lattice>| {
            if operand.is_global() || data.inst_data(operand).kind().is_const() {
                return true;
            }
            if loop_params.contains(&operand) {
                return false;
            }
            match data.layout().parent_bb(operand) {
                Some(block) if looop.contains(block) => {
                    states.get(&operand) == Some(&Lattice::Invariant)
                }
                Some(block) => dom_tree.dominates(block, looop.header()),
                None if matches!(data.inst_data(operand).kind(), InstKind::BlockArgRef(..)) => {
                    parameter_blocks
                        .get(&operand)
                        .is_some_and(|&block| dom_tree.dominates(block, looop.header()))
                }
                None => false,
            }
        };

        let mut invariant_order = vec![];
        let mut worklist = VecDeque::from_iter(loop_insts.iter().copied());
        while let Some(inst) = worklist.pop_front() {
            let inst_data = data.inst_data(inst);
            let status = if can_be_invariant(inst_data.kind())
                && inst_data
                    .inst_usage()
                    .all(|operand| operand_is_invariant(operand, &map))
            {
                Lattice::Invariant
            } else {
                Lattice::Variant
            };
            let orig = map
                .insert(inst, status)
                .expect("loop instruction was initialized");
            if orig != status {
                invariant_order.push(inst);
                worklist.extend(data.inst_data(inst).used_by().iter().filter_map(|&user| {
                    data.layout()
                        .parent_bb(user)
                        .filter(|&block| looop.contains(block))
                        .map(|_| user)
                }));
            }
        }

        let partial_geps = loop_insts
            .into_iter()
            .filter_map(|inst| {
                if map.get(&inst) == Some(&Lattice::Invariant) {
                    return None;
                }
                let InstKind::GetElemPtr(gep) = data.inst_data(inst).kind() else {
                    return None;
                };
                if !operand_is_invariant(gep.base(), &map) {
                    return None;
                }
                let prefix_len = gep
                    .offsets()
                    .iter()
                    .take_while(|&&offset| operand_is_invariant(offset, &map))
                    .count();
                if prefix_len == 0 || prefix_len == gep.offsets().len() {
                    return None;
                }
                if prefix_len == 1 && is_integer_zero(data, gep.offsets()[0]) {
                    return None;
                }
                Some((
                    inst,
                    gep.base(),
                    gep.offsets()[..prefix_len].to_vec(),
                    gep.offsets()[prefix_len..].to_vec(),
                    data.inst_data(inst).ty().clone(),
                ))
            })
            .collect::<Vec<_>>();

        let has_invariant_insts = invariant_order
            .iter()
            .any(|&inst| !data.inst_data(inst).kind().is_const());
        if !has_invariant_insts && partial_geps.is_empty() {
            return LoopResult::Unchanged;
        }

        let Some(preheader) = ensure_preheader(data, cfg, looop) else {
            return LoopResult::Unchanged;
        };
        let preheader = match preheader {
            EnsurePreheader::Existing(preheader) => preheader,
            EnsurePreheader::Created(..) => return LoopResult::CfgChanged,
        };

        let mut changed = false;
        for inst in invariant_order {
            let inst_data = data.inst_data(inst);
            if inst_data.kind().is_const() {
                continue;
            }
            changed = true;
            let bb = data
                .layout()
                .parent_bb(inst)
                .expect("invariant inst must be in the layout, constants is excluded");
            data.layout_mut().remove_inst(bb, inst);
            data.layout_mut().insert_before_terminator(preheader, inst);
        }

        for (inst, base, prefix_offsets, remaining_offsets, original_ty) in partial_geps {
            let prefix = data.new_local_value().get_elem_ptr(base, prefix_offsets);
            data.layout_mut()
                .insert_before_terminator(preheader, prefix);

            let zero = data.new_local_value().integer(0);
            let mut suffix_offsets = Vec::with_capacity(remaining_offsets.len() + 1);
            suffix_offsets.push(zero);
            suffix_offsets.extend(remaining_offsets);
            data.replace_inst_with(inst)
                .get_elem_ptr(prefix, suffix_offsets);
            assert_eq!(
                data.inst_data(inst).ty(),
                &original_ty,
                "splitting GEP must preserve its result type"
            );
            changed = true;
        }

        if changed {
            LoopResult::Changed
        } else {
            LoopResult::Unchanged
        }
    }
}

impl Pass for LICM {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().is_decl() {
            return false;
        }
        let mut changed = false;
        loop {
            let Some(cfg) = CFG::new(data) else {
                return changed;
            };
            if cfg.is_acyclic() {
                return changed;
            }
            let parameter_blocks = cfg
                .blocks()
                .iter()
                .flat_map(|&block| {
                    data.bb_data(block)
                        .params()
                        .iter()
                        .copied()
                        .map(move |parameter| (parameter, block))
                })
                .collect::<FxHashMap<_, _>>();
            let (cfg, dom_tree, loop_analysis) = loop_analysis::LoopAnalysis::from_cfg(cfg);
            let mut rebuild = false;

            // Loops are ordered from small to big. This lets an instruction
            // hoisted from an inner loop be considered by its outer loop.
            for looop in loop_analysis.loops() {
                match Self::solve(looop, data, &cfg, &dom_tree, &parameter_blocks) {
                    LoopResult::Unchanged => {}
                    LoopResult::Changed => changed = true,
                    LoopResult::CfgChanged => {
                        changed = true;
                        rebuild = true;
                        break;
                    }
                }
            }

            if !rebuild {
                return changed;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        Program, Type,
        builder_trait::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
    };

    fn run(program: &mut Program, function: Function) -> bool {
        let mut context = ArenaContextMut {
            program,
            curr_func: Some(function),
        };
        LICM.run_on(&mut context)
    }

    #[test]
    fn skips_an_acyclic_function() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "acyclic".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        data.layout_mut().push_bb_back(exit);
        let jump = data.new_local_inst().jump(exit, vec![]);
        data.layout_mut().insert_inst(entry, jump);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(!run(&mut program, function));
    }

    #[test]
    fn hoists_transitive_pure_invariants_in_dependency_order() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "licm".into(), vec![Type::get_i32()]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let external = data.params()[0];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let induction = data.bb_data(header).params()[0];
        let one = data.new_local_inst().integer(1);
        let invariant_one = data.new_local_inst().binary(BinaryOp::Add, external, one);
        let invariant_two = data
            .new_local_inst()
            .binary(BinaryOp::Mul, invariant_one, one);
        let variant = data.new_local_inst().binary(BinaryOp::Add, induction, one);
        let slot = data.new_local_inst().alloc(Type::get_i32());
        let store = data.new_local_inst().store(invariant_two, slot);
        for inst in [invariant_one, invariant_two, variant, slot, store] {
            data.layout_mut().insert_inst(header, inst);
        }
        let condition = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);

        let backedge = data.new_local_inst().jump(header, vec![variant]);
        data.layout_mut().insert_inst(body, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.layout().parent_bb(invariant_one), Some(entry));
        assert_eq!(data.layout().parent_bb(invariant_two), Some(entry));
        assert_eq!(data.layout().parent_bb(variant), Some(header));
        assert_eq!(data.layout().parent_bb(slot), Some(header));
        assert_eq!(data.layout().parent_bb(store), Some(header));
        assert_eq!(data.layout().parent_bb(branch), Some(header));
        assert_eq!(data.layout().parent_bb(backedge), Some(body));

        let preheader_insts = data
            .layout()
            .basicblock(entry)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let first = preheader_insts
            .iter()
            .position(|&inst| inst == invariant_one)
            .unwrap();
        let second = preheader_insts
            .iter()
            .position(|&inst| inst == invariant_two)
            .unwrap();
        let terminator = preheader_insts
            .iter()
            .position(|&inst| inst == entry_jump)
            .unwrap();
        assert!(first < second && second < terminator);
    }

    #[test]
    fn hoists_pure_binary_ops_but_not_memory_side_effects() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "licm_effects".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data.new_basic_block().basic_block("header".into(), vec![]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let slot = data.new_local_inst().alloc(Type::get_i32());
        data.layout_mut().insert_inst(entry, slot);
        let entry_jump = data.new_local_inst().jump(header, vec![]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let one = data.new_local_inst().integer(1);
        let zero = data.new_local_inst().integer(0);
        let div = data.new_local_inst().binary(BinaryOp::Div, one, zero);
        let rem = data.new_local_inst().binary(BinaryOp::Rem, one, zero);
        let load = data.new_local_inst().load(slot);
        let store = data.new_local_inst().store(one, slot);
        for inst in [div, rem, load, store] {
            data.layout_mut().insert_inst(header, inst);
        }
        let condition = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);
        let backedge = data.new_local_inst().jump(header, vec![]);
        data.layout_mut().insert_inst(body, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.layout().parent_bb(div), Some(entry));
        assert_eq!(data.layout().parent_bb(rem), Some(entry));
        for inst in [load, store] {
            assert_eq!(data.layout().parent_bb(inst), Some(header));
        }
    }

    #[test]
    fn creates_a_preheader_before_hoisting() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "licm_no_preheader".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let side = data.new_basic_block().basic_block("side".into(), vec![]);
        let header = data.new_basic_block().basic_block("header".into(), vec![]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [side, header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let condition = data.new_local_inst().integer(1);
        let entry_branch = data
            .new_local_inst()
            .branch(condition, header, vec![], side, vec![]);
        data.layout_mut().insert_inst(entry, entry_branch);
        let side_ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(side, side_ret);

        let one = data.new_local_inst().integer(1);
        let invariant = data.new_local_inst().binary(BinaryOp::Add, one, one);
        data.layout_mut().insert_inst(header, invariant);
        let header_branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, header_branch);
        let backedge = data.new_local_inst().jump(header, vec![]);
        data.layout_mut().insert_inst(body, backedge);
        let exit_ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, exit_ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = loop_analysis::LoopAnalysis::new(data);
        let looop = loops
            .loops()
            .iter()
            .find(|looop| looop.header() == header)
            .unwrap();
        let preheader = looop.get_preheader(&cfg).unwrap();
        assert_ne!(preheader, entry);
        assert_eq!(data.layout().parent_bb(invariant), Some(preheader));
        let insts = data.layout().basicblock(preheader).insts();
        let invariant_position = insts.iter().position(|&inst| inst == invariant).unwrap();
        let terminator_position = insts.len() - 1;
        assert!(invariant_position < terminator_position);
        assert!(!run(&mut program, function));
    }

    #[test]
    fn does_not_create_a_preheader_without_hoist_candidates() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "licm_no_candidate".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let side = data.new_basic_block().basic_block("side".into(), vec![]);
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [side, header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let condition = data.new_local_inst().integer(1);
        let zero = data.new_local_inst().integer(0);
        let entry_branch =
            data.new_local_inst()
                .branch(condition, header, vec![zero], side, vec![]);
        data.layout_mut().insert_inst(entry, entry_branch);
        let side_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(side, side_jump);

        let induction = data.bb_data(header).params()[0];
        let one = data.new_local_inst().integer(1);
        let next = data.new_local_inst().binary(BinaryOp::Add, induction, one);
        data.layout_mut().insert_inst(header, next);
        let header_branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, header_branch);
        let backedge = data.new_local_inst().jump(header, vec![next]);
        data.layout_mut().insert_inst(body, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        let block_count = data.layout().basicblocks().len();
        assert!(!run(&mut program, function));
        assert_eq!(
            program.func_data(function).layout().basicblocks().len(),
            block_count
        );
    }

    #[test]
    fn does_not_rewrite_an_entry_header_loop() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "licm_entry_loop".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let one = data.new_local_inst().integer(1);
        let invariant = data.new_local_inst().binary(BinaryOp::Add, one, one);
        data.layout_mut().insert_inst(entry, invariant);
        let condition = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(entry, branch);
        let backedge = data.new_local_inst().jump(entry, vec![]);
        data.layout_mut().insert_inst(body, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(!run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.layout().entry_bb().unwrap().bb(), entry);
        assert_eq!(data.layout().parent_bb(invariant), Some(entry));
    }

    #[test]
    fn rebuilds_analyses_after_each_created_preheader() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "licm_rebuild".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let side_a = data.new_basic_block().basic_block("side_a".into(), vec![]);
        let header_a = data
            .new_basic_block()
            .basic_block("header_a".into(), vec![]);
        let latch_a = data.new_basic_block().basic_block("latch_a".into(), vec![]);
        let after_a = data.new_basic_block().basic_block("after_a".into(), vec![]);
        let side_b = data.new_basic_block().basic_block("side_b".into(), vec![]);
        let header_b = data
            .new_basic_block()
            .basic_block("header_b".into(), vec![]);
        let latch_b = data.new_basic_block().basic_block("latch_b".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [
            side_a, header_a, latch_a, after_a, side_b, header_b, latch_b, exit,
        ] {
            data.layout_mut().push_bb_back(block);
        }

        let condition = data.new_local_inst().integer(1);
        let entry_branch =
            data.new_local_inst()
                .branch(condition, header_a, vec![], side_a, vec![]);
        data.layout_mut().insert_inst(entry, entry_branch);
        let side_a_jump = data.new_local_inst().jump(header_a, vec![]);
        data.layout_mut().insert_inst(side_a, side_a_jump);

        let one = data.new_local_inst().integer(1);
        let invariant_a = data.new_local_inst().binary(BinaryOp::Add, one, one);
        data.layout_mut().insert_inst(header_a, invariant_a);
        let branch_a = data
            .new_local_inst()
            .branch(condition, latch_a, vec![], after_a, vec![]);
        data.layout_mut().insert_inst(header_a, branch_a);
        let backedge_a = data.new_local_inst().jump(header_a, vec![]);
        data.layout_mut().insert_inst(latch_a, backedge_a);

        let after_a_branch =
            data.new_local_inst()
                .branch(condition, header_b, vec![], side_b, vec![]);
        data.layout_mut().insert_inst(after_a, after_a_branch);
        let side_b_jump = data.new_local_inst().jump(header_b, vec![]);
        data.layout_mut().insert_inst(side_b, side_b_jump);

        let invariant_b = data.new_local_inst().binary(BinaryOp::Mul, one, one);
        data.layout_mut().insert_inst(header_b, invariant_b);
        let branch_b = data
            .new_local_inst()
            .branch(condition, latch_b, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header_b, branch_b);
        let backedge_b = data.new_local_inst().jump(header_b, vec![]);
        data.layout_mut().insert_inst(latch_b, backedge_b);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        let (cfg, _dom_tree, loops) = loop_analysis::LoopAnalysis::new(data);
        for (header, invariant) in [(header_a, invariant_a), (header_b, invariant_b)] {
            let looop = loops
                .loops()
                .iter()
                .find(|looop| looop.header() == header)
                .unwrap();
            let preheader = looop.get_preheader(&cfg).unwrap();
            assert_eq!(data.layout().parent_bb(invariant), Some(preheader));
        }
        assert!(!run(&mut program, function));
    }

    #[test]
    fn hoists_fully_invariant_gep() {
        let array_ty = Type::get_array(Type::get_array(Type::get_i32(), 8), 8);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "licm_gep".into(),
            vec![Type::get_pointer(array_ty), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let row = data.params()[1];
        let header = data.new_basic_block().basic_block("header".into(), vec![]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let entry_jump = data.new_local_inst().jump(header, vec![]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let zero = data.new_local_inst().integer(0);
        let gep = data.new_local_inst().get_elem_ptr(base, vec![zero, row]);
        data.layout_mut().insert_inst(header, gep);
        let condition = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);
        let backedge = data.new_local_inst().jump(header, vec![]);
        data.layout_mut().insert_inst(body, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.layout().parent_bb(gep), Some(entry));
        let entry_insts = data.layout().basicblock(entry).insts();
        let gep_position = entry_insts.iter().position(|&inst| inst == gep).unwrap();
        let terminator_position = entry_insts
            .iter()
            .position(|&inst| inst == entry_jump)
            .unwrap();
        assert!(gep_position < terminator_position);
    }

    #[test]
    fn hoists_invariant_gep_prefix_and_preserves_original_value() {
        let array_ty = Type::get_array(Type::get_array(Type::get_i32(), 8), 8);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "licm_partial_gep".into(),
            vec![Type::get_pointer(array_ty), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let row = data.params()[1];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let column = data.bb_data(header).params()[0];
        let gep = data
            .new_local_inst()
            .get_elem_ptr(base, vec![zero, row, column]);
        let original_ty = data.inst_data(gep).ty().clone();
        let load = data.new_local_inst().load(gep);
        for inst in [gep, load] {
            data.layout_mut().insert_inst(header, inst);
        }
        let one = data.new_local_inst().integer(1);
        let next = data.new_local_inst().binary(BinaryOp::Add, column, one);
        data.layout_mut().insert_inst(header, next);
        let condition = data.new_local_inst().integer(1);
        let branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, branch);
        let backedge = data.new_local_inst().jump(header, vec![next]);
        data.layout_mut().insert_inst(body, backedge);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(run(&mut program, function));
        let data = program.func_data(function);
        assert_eq!(data.layout().parent_bb(gep), Some(header));
        assert_eq!(data.inst_data(gep).ty(), &original_ty);
        assert_eq!(
            data.inst_data(load).inst_usage().collect::<Vec<_>>(),
            vec![gep]
        );

        let InstKind::GetElemPtr(suffix) = data.inst_data(gep).kind() else {
            panic!("original value must remain a GEP");
        };
        assert_eq!(suffix.offsets().len(), 2);
        assert!(matches!(
            data.inst_data(suffix.offsets()[0]).kind(),
            InstKind::Integer(value) if value.value() == 0
        ));
        assert_eq!(suffix.offsets()[1], column);

        let prefix = suffix.base();
        assert_eq!(data.layout().parent_bb(prefix), Some(entry));
        let InstKind::GetElemPtr(prefix_gep) = data.inst_data(prefix).kind() else {
            panic!("partial motion must create a prefix GEP");
        };
        assert_eq!(prefix_gep.base(), base);
        assert_eq!(prefix_gep.offsets(), &[zero, row]);
        assert!(data.inst_data(prefix).used_by().contains(&gep));
        assert!(!data.inst_data(base).used_by().contains(&gep));

        assert!(!run(&mut program, function));
    }
}
