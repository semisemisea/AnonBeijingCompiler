//! Loop rotation for test-at-bottom countdown loops.
//!
//! `while (len) { body; len = len - 1; }` lowers to a header test at the top
//! of the loop:
//!
//! ```text
//! entry:      jump header(v0)
//! header(v):  br v, body, exit
//! body:       ...; v' = f(v); jump header(v')
//! exit:       ret
//! ```
//!
//! When every non-back edge into `header` passes a provably nonzero value
//! for the tested parameter, the first-iteration test can never fail, so the
//! test moves to the bottom of the loop, merged into the latch (do-while
//! form):
//!
//! ```text
//! entry:      jump header(v0)
//! header(v):  jump body
//! body:       ...; v' = f(v); br v', header(v'), exit
//! exit:       ret
//! ```
//!
//! The backend can then fuse the count-down arithmetic with the loop test
//! (`subs w8, w8, #1; b.ne header` instead of `sub; b header; cmp; b.ne`),
//! eliminating the standalone compare from the loop.
//!
//! Correctness: the head test only gates the *first* iteration; every other
//! iteration is gated by the same test on the same value at the bottom.
//! Removing the head test is sound exactly when all non-back edges pass a
//! nonzero value, which the constant check proves.
//!
//! SysY `while (i < n) { body; i += 1 }` loops are count-up and test a
//! comparison (`lt i, bound`) rather than a counter directly. They are
//! rotated to countdown form by introducing a trip counter `t = bound - i0`
//! carried in a new header parameter, guarded by a pre-header test `t > 0`
//! that preserves the trip-zero semantics of the original head test.

use crate::opt::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

pub struct RotateLoops;

impl Pass for RotateLoops {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().entry_bb().is_none() {
            return false;
        }
        let mut changed = false;
        let headers: Vec<BasicBlock> = data
            .layout()
            .basicblocks()
            .iter()
            .map(|layout| layout.bb())
            .collect();
        let entry = data.layout().entry_bb().unwrap().bb();
        for header in headers {
            if header == entry {
                continue;
            }
            if Self::rotate_countdown(data, header) || Self::rotate_count_up(data, header) {
                changed = true;
            }
        }
        changed
    }
}

impl RotateLoops {
    /// Try to rotate the loop whose header is `header`. Returns true when the
    /// test moved to the bottom of the loop.
    fn rotate_countdown(data: &mut ArenaContextMut<'_>, header: BasicBlock) -> bool {
        let (cond, body, exit) = {
            let terminator = data.layout().basicblock(header).terminator();
            let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
                return false;
            };
            let body = branch.t_target();
            let exit = branch.f_target();
            if body == exit
                || !branch.t_args().is_empty()
                || !branch.f_args().is_empty()
                || !data.bb_data(body).params().is_empty()
                || !data.bb_data(exit).params().is_empty()
            {
                return false;
            }
            (branch.cond(), body, exit)
        };

        // The tested value must be one of the header's parameters directly.
        let params = data.bb_data(header).params().to_vec();
        let Some(tested_index) = params.iter().position(|&p| p == cond) else {
            return false;
        };

        // Partition the header's predecessors into constant (nonzero) edges,
        // which cannot fail the first-iteration test, and the single back
        // edge, whose arrival must be retested at the bottom of the loop.
        let pred_insts: Vec<Inst> = data.bb_data(header).used_by().iter().copied().collect();
        let mut const_preds = Vec::new();
        let mut back_edge: Option<Inst> = None;
        for &pred_inst in &pred_insts {
            let InstKind::Jump(jump) = data.inst_data(pred_inst).kind() else {
                return false;
            };
            let args = jump.args().to_vec();
            if args.len() != params.len() {
                return false;
            }
            let tested_arg = args[tested_index];
            let is_nonzero_const = matches!(data.inst_data(tested_arg).kind(),
                InstKind::Integer(i) if i.value() != 0);
            if is_nonzero_const {
                const_preds.push(pred_inst);
            } else if back_edge.is_none() {
                back_edge = Some(pred_inst);
            } else {
                return false;
            }
        }
        // A loop needs at least one guaranteed-executed entry and exactly one
        // back edge to retest.
        if const_preds.is_empty() || back_edge.is_none() {
            return false;
        }
        let back_edge = back_edge.unwrap();

        // After rotation the body's `br v', header(args), exit` skips the
        // header on the false edge, so any reference to a header parameter
        // inside `exit` would see the *previous* iteration's value instead of
        // the freshly computed one. The tested counter is the exception: its
        // value on exit is zero either way. Refuse to rotate when `exit`
        // mentions any other header parameter.
        let tested_param = params[tested_index];
        for &inst in data.layout().basicblock(exit).insts().iter() {
            for used in data.inst_data(inst).inst_usage() {
                if params.contains(&used) && used != tested_param {
                    return false;
                }
            }
        }

        // Move the header's test to the latch: br v', header(args), exit.
        let back_args = match data.inst_data(back_edge).kind() {
            InstKind::Jump(jump) => jump.args().to_vec(),
            _ => unreachable!(),
        };
        data.replace_inst_with(back_edge).branch(
            back_args[tested_index],
            header,
            back_args,
            exit,
            vec![],
        );

        // The header no longer tests; it passes straight through to the body.
        data.replace_inst_with(data.layout().basicblock(header).terminator())
            .jump(body, vec![]);

        true
    }

    /// Rotate a count-up `while (i < bound) { body; i += 1 }` loop to countdown
    /// form so the backend can fuse the decrement with the loop test. A trip
    /// counter `t = bound - i0` is carried in a new header parameter; a guard
    /// in the pre-header preserves the trip-zero semantics.
    fn rotate_count_up(data: &mut ArenaContextMut<'_>, header: BasicBlock) -> bool {
        let (body, exit, lt) = {
            let terminator = data.layout().basicblock(header).terminator();
            let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
                return false;
            };
            let body = branch.t_target();
            let exit = branch.f_target();
            if body == exit
                || !branch.t_args().is_empty()
                || !branch.f_args().is_empty()
                || !data.bb_data(exit).params().is_empty()
            {
                return false;
            }
            let InstKind::Binary(lt) = data.inst_data(branch.cond()).kind() else {
                return false;
            };
            if lt.op() != BinaryOp::Lt {
                return false;
            }
            (body, exit, (lt.lhs(), lt.rhs()))
        };
        let (iv, bound) = lt;

        let params = data.bb_data(header).params().to_vec();
        let Some(iv_pos) = params.iter().position(|&p| p == iv) else {
            return false;
        };
        // The bound must be defined before the loop so the trip counter and the
        // guard can be computed in the pre-header.
        if params.contains(&bound) || !Self::available_before_loop(data, &params, bound) {
            return false;
        }

        // Exactly one entry edge and one back edge, both jumps. The back edge
        // is the one carrying the `iv + 1` update; the entry edge carries the
        // initial value.
        let preds: Vec<Inst> = data.bb_data(header).used_by().iter().copied().collect();
        let mut back_edge: Option<(Inst, Vec<Inst>)> = None;
        let mut entry_edge: Option<(Inst, Vec<Inst>)> = None;
        for &pred in &preds {
            let InstKind::Jump(jump) = data.inst_data(pred).kind() else {
                return false;
            };
            let args = jump.args().to_vec();
            if args.len() != params.len() {
                return false;
            }
            let carries_update = matches!(
                data.inst_data(args[iv_pos]).kind(),
                InstKind::Binary(b)
                    if b.op() == BinaryOp::Add
                        && b.lhs() == iv
                        && matches!(data.inst_data(b.rhs()).kind(), InstKind::Integer(one) if one.value() == 1)
            );
            if carries_update {
                if back_edge.replace((pred, args)).is_some() {
                    return false;
                }
            } else if entry_edge.replace((pred, args)).is_some() {
                return false;
            }
        }
        let Some((back_edge_inst, back_args)) = back_edge else {
            return false;
        };
        let Some((entry_edge_inst, entry_args)) = entry_edge else {
            return false;
        };

        // Give `exit` block parameters mirroring the header parameters whenever
        // it reads any of them. The guard and the latch both flow into `exit`
        // (with the entry or the last-iteration values respectively), so the
        // reads are remapped from the header parameters to the new block
        // parameters to keep SSA dominance.
        let exit_reads_header = data.layout().basicblock(exit).insts().iter().any(|&inst| {
            data.inst_data(inst)
                .inst_usage()
                .any(|operand| params.contains(&operand))
        });
        // The guard's false edge flows into `exit` from the pre-header without
        // passing through the header. Every block reachable from `exit` then
        // has a path that bypasses the loop, so values produced inside the
        // loop (header parameters or body-defined values) would lose their
        // dominating definition. Refuse rotation whenever that region uses
        // such a value, unless the only user is `exit` itself, which is
        // handled by substituting its own block parameters below.
        let loop_blocks = Self::loop_blocks(data, header, body);
        let exit_region = Self::exit_region(data, exit, &loop_blocks);
        let region_uses_loop_value = exit_region.iter().any(|&block| {
            if block == exit {
                return false;
            }
            data.layout().basicblock(block).insts().iter().any(|&inst| {
                data.inst_data(inst)
                    .inst_usage()
                    .any(|operand| Self::is_loop_value(data, operand, &params, &loop_blocks))
            })
        });
        if region_uses_loop_value {
            return false;
        }
        // `exit` itself may read loop values only when they are header
        // parameters, which are remapped to its own parameters below; a
        // loop-defined non-parameter value cannot be substituted.
        let exit_uses_loop_value = data.layout().basicblock(exit).insts().iter().any(|&inst| {
            data.inst_data(inst).inst_usage().any(|operand| {
                if params.contains(&operand) {
                    return false;
                }
                Self::is_loop_value(data, operand, &params, &loop_blocks)
            })
        });
        if exit_uses_loop_value {
            return false;
        }
        if exit_reads_header {
            // Param-izing the exit block requires updating every edge into it.
            // The guard and latch are rewritten below, but any other
            // predecessor (e.g. a `break`/`continue` path) would keep passing
            // the old empty argument list, desynchronizing args from params.
            let header_terminator = data.layout().basicblock(header).terminator();
            if data
                .bb_data(exit)
                .used_by()
                .iter()
                .any(|&pred| pred != header_terminator)
            {
                return false;
            }
        }
        let _exit_params: Vec<Inst> = if exit_reads_header {
            let exit_params = params
                .iter()
                .map(|&parameter| {
                    let ty = data.inst_data(parameter).ty().clone();
                    data.new_basic_block().add_param(exit, ty)
                })
                .collect::<Vec<_>>();
            let substitution = params
                .iter()
                .zip(&exit_params)
                .map(|(&parameter, &replacement)| (parameter, replacement))
                .collect::<FxHashMap<_, _>>();
            let mut mapper = SubstMapper {
                substitution: &substitution,
            };
            let insts: Vec<Inst> = data
                .layout()
                .basicblock(exit)
                .insts()
                .iter()
                .copied()
                .collect();
            for inst in insts {
                if data
                    .inst_data(inst)
                    .inst_usage()
                    .any(|operand| substitution.contains_key(&operand))
                {
                    let remapped = data
                        .inst_data(inst)
                        .clone()
                        .remap_refs(&mut mapper)
                        .expect("mapping header parameters cannot fail");
                    data.replace_inst_with(inst).raw(remapped);
                }
            }
            exit_params
        } else {
            Vec::new()
        };

        // Append the trip counter to the header parameters.
        let t = data.new_basic_block().add_param(header, Type::get_i32());

        // Guard in the pre-header: `t0 = bound - i0`, enter the loop only when
        // the trip count is positive.
        let preheader = data.layout().parent_bb(entry_edge_inst).unwrap();
        let init_iv = entry_args[iv_pos];
        let zero = data.new_local_value().integer(0);
        let one = data.new_local_value().integer(1);
        let t0 = data.new_local_value().binary(BinaryOp::Sub, bound, init_iv);
        let positive = data.new_local_value().binary(BinaryOp::Gt, t0, zero);
        data.layout_mut().insert_before_terminator(preheader, t0);
        data.layout_mut()
            .insert_before_terminator(preheader, positive);
        let mut header_entry_args = entry_args.clone();
        header_entry_args.push(t0);
        let exit_entry_args = if exit_reads_header {
            entry_args.clone()
        } else {
            Vec::new()
        };
        data.replace_inst_with(entry_edge_inst).branch(
            positive,
            header,
            header_entry_args,
            exit,
            exit_entry_args,
        );

        // The header passes straight through to the body.
        data.replace_inst_with(data.layout().basicblock(header).terminator())
            .jump(body, vec![]);

        // Latch: `t_next = t - 1`; test it at the bottom.
        let latch = data.layout().parent_bb(back_edge_inst).unwrap();
        let t_next = data.new_local_value().binary(BinaryOp::Sub, t, one);
        data.layout_mut().insert_before_terminator(latch, t_next);
        let mut header_back_args = back_args.clone();
        header_back_args.push(t_next);
        let exit_back_args = if exit_reads_header {
            back_args.clone()
        } else {
            Vec::new()
        };
        data.replace_inst_with(back_edge_inst).branch(
            t_next,
            header,
            header_back_args,
            exit,
            exit_back_args,
        );

        true
    }

    fn available_before_loop(data: &FunctionData, params: &[Inst], bound: Inst) -> bool {
        if bound.is_global() || data.inst_data(bound).kind().is_const() {
            return true;
        }
        matches!(data.inst_data(bound).kind(), InstKind::BlockArgRef(..))
            && !params.contains(&bound)
    }

    /// Whether `value` is produced inside the loop being rotated: it is a
    /// header parameter, defined by an instruction in a loop block, or a
    /// block parameter of a loop block.
    fn is_loop_value(
        data: &FunctionData,
        value: Inst,
        params: &[Inst],
        loop_blocks: &FxHashSet<BasicBlock>,
    ) -> bool {
        if params.contains(&value) {
            return true;
        }
        match data.layout().parent_bb(value) {
            Some(block) => loop_blocks.contains(&block),
            None => loop_blocks
                .iter()
                .any(|&block| data.bb_data(block).params().contains(&value)),
        }
    }

    /// Blocks belonging to the loop being rotated: the header plus every
    /// block reachable from the body without passing back through the header.
    fn loop_blocks(
        data: &FunctionData,
        header: BasicBlock,
        body: BasicBlock,
    ) -> FxHashSet<BasicBlock> {
        let mut blocks = FxHashSet::default();
        blocks.insert(header);
        let mut stack = vec![body];
        while let Some(block) = stack.pop() {
            if blocks.insert(block) {
                for successor in Self::block_successors(data, block) {
                    if successor != header {
                        stack.push(successor);
                    }
                }
            }
        }
        blocks
    }

    /// The region reachable from the loop `exit`: every block that becomes
    /// reachable on the pre-header guard's false edge after rotation.
    fn exit_region(
        data: &FunctionData,
        exit: BasicBlock,
        loop_blocks: &FxHashSet<BasicBlock>,
    ) -> Vec<BasicBlock> {
        let mut region = Vec::new();
        let mut seen = FxHashSet::default();
        let mut stack = vec![exit];
        while let Some(block) = stack.pop() {
            if seen.insert(block) {
                region.push(block);
                for successor in Self::block_successors(data, block) {
                    if !loop_blocks.contains(&successor) {
                        stack.push(successor);
                    }
                }
            }
        }
        region
    }

    fn block_successors(data: &FunctionData, block: BasicBlock) -> Vec<BasicBlock> {
        let terminator = data.layout().basicblock(block).terminator();
        match data.inst_data(terminator).kind() {
            InstKind::Jump(jump) => vec![jump.target()],
            InstKind::Branch(branch) => vec![branch.t_target(), branch.f_target()],
            _ => Vec::new(),
        }
    }
}

/// Remaps header-parameter operands inside the exit block to its own
/// parameters while the loop rotates.
struct SubstMapper<'a> {
    substitution: &'a FxHashMap<Inst, Inst>,
}

impl crate::ir::remap::EntityMapper for SubstMapper<'_> {
    type Error = std::convert::Infallible;

    fn map_inst(&mut self, inst: Inst) -> Result<Inst, Self::Error> {
        Ok(self.substitution.get(&inst).copied().unwrap_or(inst))
    }

    fn map_block(&mut self, block: BasicBlock) -> Result<BasicBlock, Self::Error> {
        Ok(block)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_loop(
        program: &mut Program,
        entry_value: i32,
    ) -> (Function, BasicBlock, BasicBlock, BasicBlock, BasicBlock) {
        let func = program.new_function(Type::get_i32(), "rot".to_owned(), vec![]);
        let (entry, header, body, exit) = {
            let data = program.func_data_mut(func);
            let entry = data
                .new_basic_block()
                .basic_block("entry".to_owned(), vec![]);
            let header = data
                .new_basic_block()
                .basic_block("header".to_owned(), vec![Type::get_i32()]);
            let body = data
                .new_basic_block()
                .basic_block("body".to_owned(), vec![]);
            let exit = data
                .new_basic_block()
                .basic_block("exit".to_owned(), vec![]);
            data.layout_mut().push_bb_back(entry);
            data.layout_mut().push_bb_back(header);
            data.layout_mut().push_bb_back(body);
            data.layout_mut().push_bb_back(exit);

            let init = data.new_local_inst().integer(entry_value);
            let jump = data.new_local_inst().jump(header, vec![init]);
            data.layout_mut().insert_inst(entry, jump);

            let param = data.bb_data(header).params()[0];
            let branch = data
                .new_local_inst()
                .branch(param, body, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, branch);

            let one = data.new_local_inst().integer(1);
            let decrement = data.new_local_inst().binary(BinaryOp::Sub, param, one);
            let back = data.new_local_inst().jump(header, vec![decrement]);
            data.layout_mut().insert_inst(body, back);

            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(exit, ret);

            (entry, header, body, exit)
        };
        (func, entry, header, body, exit)
    }

    fn run(data: &mut ArenaContextMut<'_>) -> bool {
        RotateLoops.run_on(data)
    }

    #[test]
    fn rotates_nonzero_entry_loop() {
        let mut program = Program::new();
        let (func, _entry, header, body, exit) = build_loop(&mut program, 32);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(func),
        };

        assert!(run(&mut data));
        // The header no longer tests; it jumps straight into the body.
        let terminator = data.layout().basicblock(header).terminator();
        match data.inst_data(terminator).kind() {
            InstKind::Jump(jump) => assert_eq!(jump.target(), body),
            other => panic!("expected jump to body, got {other:?}"),
        }
        // The latch now carries the test: one block branches back to the
        // header (with args) and out to the exit.
        let mut tested = data
            .layout()
            .basicblocks()
            .iter()
            .filter(|layout| {
                let terminator = layout.terminator();
                matches!(data.inst_data(terminator).kind(), InstKind::Branch(b)
                    if b.t_target() == header && b.f_target() == exit)
            })
            .count();
        assert_eq!(tested, 1);
        tested = data
            .layout()
            .basicblocks()
            .iter()
            .filter(|layout| {
                let terminator = layout.terminator();
                matches!(data.inst_data(terminator).kind(), InstKind::Branch(b)
                    if b.t_target() == body)
            })
            .count();
        assert_eq!(tested, 0, "no block may still branch to the body directly");
    }

    #[test]
    fn skips_zero_or_missing_const_entry() {
        let mut program = Program::new();
        let (func, _entry, _header, _body, _exit) = build_loop(&mut program, 0);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(func),
        };
        assert!(!run(&mut data), "zero entry value must not rotate");
    }

    fn build_count_up(
        program: &mut Program,
        function_name: &str,
        init: i32,
    ) -> (
        Function,
        BasicBlock,
        BasicBlock,
        BasicBlock,
        BasicBlock,
        Inst,
    ) {
        let func = program.new_function(
            Type::get_i32(),
            function_name.to_owned(),
            vec![Type::get_i32()],
        );
        let (entry, header, body, exit, entry_jump) = {
            let data = program.func_data_mut(func);
            let entry = data.add_entry_block();
            let header = data
                .new_basic_block()
                .basic_block("header".to_owned(), vec![Type::get_i32()]);
            let body = data
                .new_basic_block()
                .basic_block("body".to_owned(), vec![]);
            let exit = data
                .new_basic_block()
                .basic_block("exit".to_owned(), vec![]);
            for block in [header, body, exit] {
                data.layout_mut().push_bb_back(block);
            }

            let init_inst = data.new_local_inst().integer(init);
            let entry_jump = data.new_local_inst().jump(header, vec![init_inst]);
            data.layout_mut().insert_inst(entry, entry_jump);

            let iv = data.bb_data(header).params()[0];
            let bound = data.params()[0];
            let lt = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
            let header_branch = data.new_local_inst().branch(lt, body, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, lt);
            data.layout_mut().insert_inst(header, header_branch);

            let one = data.new_local_inst().integer(1);
            let next_iv = data.new_local_inst().binary(BinaryOp::Add, iv, one);
            let backedge = data.new_local_inst().jump(header, vec![next_iv]);
            data.layout_mut().insert_inst(body, next_iv);
            data.layout_mut().insert_inst(body, backedge);

            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(exit, ret);

            (entry, header, body, exit, entry_jump)
        };
        (func, entry, header, body, exit, entry_jump)
    }

    #[test]
    fn rotates_count_up_loop_into_guarded_countdown() {
        let mut program = Program::new();
        let (func, entry, header, body, exit, entry_jump) =
            build_count_up(&mut program, "rot_up", 0);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(func),
        };

        assert!(run(&mut data));
        // The header no longer tests; it jumps straight into the body and now
        // carries the countdown trip counter as a second parameter.
        let header_terminator = data.layout().basicblock(header).terminator();
        assert!(
            matches!(data.inst_data(header_terminator).kind(), InstKind::Jump(j) if j.target() == body)
        );
        assert_eq!(data.bb_data(header).params().len(), 2);
        // The trip counter is a header parameter.
        let t = data.bb_data(header).params()[1];
        assert!(data.inst_data(t).ty().is_i32());

        // The pre-header now guards `trip > 0`: it branches to the header (with
        // the countdown initial value) or to the exit.
        let InstKind::Branch(guard) = data.inst_data(entry_jump).kind() else {
            panic!("pre-header terminator must become a guard branch");
        };
        assert_eq!(guard.t_target(), header);
        assert_eq!(guard.f_target(), exit);
        assert_eq!(guard.t_args().len(), 2);
        assert!(guard.f_args().is_empty());

        // The latch tests `t - 1` at the bottom and passes the updated counter.
        let backedge = data.layout().basicblock(body).terminator();
        let InstKind::Branch(latch) = data.inst_data(backedge).kind() else {
            panic!("latch must become the bottom test");
        };
        assert_eq!(latch.t_target(), header);
        assert_eq!(latch.f_target(), exit);
        assert_eq!(latch.t_args().len(), 2);
        let InstKind::Binary(step) = data.inst_data(latch.cond()).kind() else {
            panic!("latch condition must be the decremented counter");
        };
        assert_eq!(step.op(), BinaryOp::Sub);
        assert_eq!(step.lhs(), t);
    }

    #[test]
    fn refuses_when_the_region_after_exit_uses_the_induction_variable() {
        // Mirrors the sort-test regression: the exit block itself is clean,
        // but a block reached after the loop reads the induction variable
        // directly. After rotation the pre-header guard reaches that block
        // without passing through the header, so the header parameter would
        // lose its dominating definition; rotation must be refused.
        let mut program = Program::new();
        let func = program.new_function(
            Type::get_i32(),
            "rot_up_exit_region".to_owned(),
            vec![Type::get_i32()],
        );
        let (entry, header, body, exit, downstream, _entry_jump) = {
            let data = program.func_data_mut(func);
            let entry = data.add_entry_block();
            let header = data
                .new_basic_block()
                .basic_block("header".to_owned(), vec![Type::get_i32()]);
            let body = data
                .new_basic_block()
                .basic_block("body".to_owned(), vec![]);
            let exit = data
                .new_basic_block()
                .basic_block("exit".to_owned(), vec![]);
            let downstream = data
                .new_basic_block()
                .basic_block("downstream".to_owned(), vec![]);
            for block in [header, body, exit, downstream] {
                data.layout_mut().push_bb_back(block);
            }

            let init_inst = data.new_local_inst().integer(0);
            let entry_jump = data.new_local_inst().jump(header, vec![init_inst]);
            data.layout_mut().insert_inst(entry, entry_jump);

            let iv = data.bb_data(header).params()[0];
            let bound = data.params()[0];
            let lt = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
            let header_branch = data.new_local_inst().branch(lt, body, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, lt);
            data.layout_mut().insert_inst(header, header_branch);

            let one = data.new_local_inst().integer(1);
            let next_iv = data.new_local_inst().binary(BinaryOp::Add, iv, one);
            let backedge = data.new_local_inst().jump(header, vec![next_iv]);
            data.layout_mut().insert_inst(body, next_iv);
            data.layout_mut().insert_inst(body, backedge);

            let continue_jump = data.new_local_inst().jump(downstream, vec![]);
            data.layout_mut().insert_inst(exit, continue_jump);

            // The downstream block reads the induction variable directly, as
            // the sort-test swap block does.
            let five = data.new_local_inst().integer(5);
            let observed = data.new_local_inst().binary(BinaryOp::Add, iv, five);
            let ret = data.new_local_inst().ret(Some(observed));
            data.layout_mut().insert_inst(downstream, five);
            data.layout_mut().insert_inst(downstream, observed);
            data.layout_mut().insert_inst(downstream, ret);

            (entry, header, body, exit, downstream, entry_jump)
        };
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(func),
        };
        assert!(
            !run(&mut data),
            "rotation must be refused when the post-exit region reads a header parameter"
        );
        // The header still tests.
        let terminator = data.layout().basicblock(header).terminator();
        assert!(
            matches!(data.inst_data(terminator).kind(), InstKind::Branch(_)),
            "header must keep its test"
        );
    }

    #[test]
    fn gives_the_exit_block_parameters_when_it_reads_the_induction_variable() {
        let mut program = Program::new();
        let func = program.new_function(
            Type::get_i32(),
            "rot_up_exit_iv".to_owned(),
            vec![Type::get_i32()],
        );
        let (entry, header, body, exit, entry_jump) = {
            let data = program.func_data_mut(func);
            let entry = data.add_entry_block();
            let header = data
                .new_basic_block()
                .basic_block("header".to_owned(), vec![Type::get_i32()]);
            let body = data
                .new_basic_block()
                .basic_block("body".to_owned(), vec![]);
            let exit = data
                .new_basic_block()
                .basic_block("exit".to_owned(), vec![]);
            for block in [header, body, exit] {
                data.layout_mut().push_bb_back(block);
            }

            let init_inst = data.new_local_inst().integer(0);
            let entry_jump = data.new_local_inst().jump(header, vec![init_inst]);
            data.layout_mut().insert_inst(entry, entry_jump);

            let iv = data.bb_data(header).params()[0];
            let bound = data.params()[0];
            let lt = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
            let header_branch = data.new_local_inst().branch(lt, body, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, lt);
            data.layout_mut().insert_inst(header, header_branch);

            let one = data.new_local_inst().integer(1);
            let next_iv = data.new_local_inst().binary(BinaryOp::Add, iv, one);
            let backedge = data.new_local_inst().jump(header, vec![next_iv]);
            data.layout_mut().insert_inst(body, next_iv);
            data.layout_mut().insert_inst(body, backedge);

            // The exit reads the induction variable, forcing the rotation to
            // give it its own parameter.
            let five = data.new_local_inst().integer(5);
            let observed = data.new_local_inst().binary(BinaryOp::Add, iv, five);
            let ret = data.new_local_inst().ret(Some(observed));
            data.layout_mut().insert_inst(exit, five);
            data.layout_mut().insert_inst(exit, observed);
            data.layout_mut().insert_inst(exit, ret);

            (entry, header, body, exit, entry_jump)
        };
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(func),
        };
        assert!(run(&mut data));

        // Exit now carries one parameter mirroring the induction variable, and
        // its computation reads that parameter rather than the header one.
        assert_eq!(data.bb_data(exit).params().len(), 1);
        let exit_param = data.bb_data(exit).params()[0];
        let observed_uses_param = data
            .layout()
            .basicblock(exit)
            .insts()
            .iter()
            .any(|&inst| data.inst_data(inst).inst_usage().any(|op| op == exit_param));
        assert!(observed_uses_param, "exit must use its own parameter");

        // Both the guard (trip zero) and the latch (last iteration) pass the
        // induction variable to the exit.
        let InstKind::Branch(guard) = data.inst_data(entry_jump).kind() else {
            panic!("pre-header must guard");
        };
        assert_eq!(guard.f_target(), exit);
        assert_eq!(guard.f_args().len(), 1);
        let backedge = data.layout().basicblock(body).terminator();
        let InstKind::Branch(latch) = data.inst_data(backedge).kind() else {
            panic!("latch must be a branch");
        };
        assert_eq!(latch.f_target(), exit);
        assert_eq!(latch.f_args().len(), 1);
    }
}
