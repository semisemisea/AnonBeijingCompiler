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

use crate::opt::prelude::*;

pub struct RotateLoops;

impl Pass for RotateLoops {
    fn run_on(&self, data: &mut ArenaContextMut<'_>) -> bool {
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
            if Self::rotate_loop(data, header) {
                changed = true;
            }
        }
        changed
    }
}

impl RotateLoops {
    /// Try to rotate the loop whose header is `header`. Returns true when the
    /// test moved to the bottom of the loop.
    fn rotate_loop(data: &mut ArenaContextMut<'_>, header: BasicBlock) -> bool {
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
            let branch = data.new_local_inst().branch(param, body, vec![], exit, vec![]);
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
}
