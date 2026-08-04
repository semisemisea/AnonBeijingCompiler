//! Convert zero-initialization loops into a single `MemZero`.
//!
//! After loop rotation a zeroing loop takes countdown form:
//!
//! ```text
//! pre:   row = gep(base, ...); jump header(j0, t0)
//! header(j, t): jump body
//! body:  store 0, gep(row, (.., j)); j' = j + 1; t' = t - 1;
//!        br t', header(j', t'), exit
//! ```
//!
//! The guard `t0 > 0` already lives in the pre-header (added by rotation), so
//! the whole loop collapses to `MemZero(row, t0 * elem_size)` executed on the
//! guard's true path. One `bl memset` replaces one store (and loop control)
//! per element.

use crate::opt::prelude::*;

pub struct ZeroStoreLoop;

impl Pass for ZeroStoreLoop {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().entry_bb().is_none() {
            return false;
        }
        let headers: Vec<BasicBlock> = data
            .layout()
            .basicblocks()
            .iter()
            .map(|layout| layout.bb())
            .collect();
        let entry = data.layout().entry_bb().unwrap().bb();
        let mut changed = false;
        for header in headers {
            if header == entry {
                continue;
            }
            if Self::convert_zero_loop(data, header) {
                changed = true;
            }
        }
        changed
    }
}

impl ZeroStoreLoop {
    fn convert_zero_loop(data: &mut ArenaContextMut<'_>, header: BasicBlock) -> bool {
        // Rotated countdown header: `jump body`.
        let terminator = data.layout().basicblock(header).terminator();
        let InstKind::Jump(jump) = data.inst_data(terminator).kind() else {
            return false;
        };
        if !jump.args().is_empty() {
            return false;
        }
        let body = jump.target();

        // Latch: `br (t - 1), header(args), exit`, false edge to a param-less
        // exit that reads no header parameter.
        let body_terminator = data.layout().basicblock(body).terminator();
        let InstKind::Branch(latch) = data.inst_data(body_terminator).kind() else {
            return false;
        };
        if latch.t_target() != header || !latch.f_args().is_empty() {
            return false;
        }
        let exit = latch.f_target();
        if !data.bb_data(exit).params().is_empty() {
            return false;
        }
        let InstKind::Binary(countdown) = data.inst_data(latch.cond()).kind() else {
            return false;
        };
        if countdown.op() != BinaryOp::Sub
            || !matches!(
                data.inst_data(countdown.rhs()).kind(),
                InstKind::Integer(one) if one.value() == 1
            )
        {
            return false;
        }
        let params = data.bb_data(header).params().to_vec();
        let Some(t_pos) = params.iter().position(|&p| p == countdown.lhs()) else {
            return false;
        };
        if data.layout().basicblock(exit).insts().iter().any(|&inst| {
            data.inst_data(inst)
                .inst_usage()
                .any(|operand| params.contains(&operand))
        }) {
            return false;
        }

        // The body must hold exactly one store of a constant zero into a
        // contiguous GEP range indexed by a header parameter as its last
        // offset, plus the IV/countdown updates.
        let body_insts: Vec<Inst> = data
            .layout()
            .basicblock(body)
            .insts()
            .iter()
            .copied()
            .collect();
        let mut zero_store: Option<(Inst, Inst, usize)> = None; // (gep, base, j offset position)
        for &inst in &body_insts {
            match data.inst_data(inst).kind() {
                InstKind::Store(store) => {
                    let is_zero =
                        matches!(
                            data.inst_data(store.src()).kind(),
                            InstKind::Integer(value) if value.value() == 0
                        ) || matches!(data.inst_data(store.src()).kind(), InstKind::ZeroInit);
                    if !is_zero || zero_store.is_some() {
                        return false;
                    }
                    let InstKind::GetElemPtr(gep) = data.inst_data(store.dest()).kind() else {
                        return false;
                    };
                    let Some((last_pos, &last_offset)) = gep.offsets().iter().enumerate().last()
                    else {
                        return false;
                    };
                    if !params.contains(&last_offset) {
                        return false;
                    }
                    zero_store = Some((store.dest(), gep.base(), last_pos));
                }
                InstKind::Call(..)
                | InstKind::TailCall(..)
                | InstKind::Load(..)
                | InstKind::MemZero(..) => return false,
                InstKind::Jump(..) | InstKind::Branch(..) => {}
                _ => {}
            }
        }
        let Some((store_gep, base, j_offset_pos)) = zero_store else {
            return false;
        };
        let InstKind::GetElemPtr(store_gep_data) = data.inst_data(store_gep).kind() else {
            unreachable!()
        };
        let j = store_gep_data.offsets()[j_offset_pos];
        let Some(j_pos) = params.iter().position(|&p| p == j) else {
            return false;
        };
        if j_pos == t_pos {
            return false;
        }
        let row_offsets = store_gep_data
            .offsets()
            .iter()
            .take(j_offset_pos)
            .copied()
            .collect::<Vec<_>>();
        if !Self::available_before_loop(data, header, body, &params, base)
            || row_offsets
                .iter()
                .any(|&offset| !Self::available_before_loop(data, header, body, &params, offset))
        {
            return false;
        }

        // The single entry edge: a jump into the header from a pre-loop block.
        let preds: Vec<Inst> = data.bb_data(header).used_by().iter().copied().collect();
        let mut entry_edge = None;
        for &pred in &preds {
            if pred == body_terminator {
                continue;
            }
            let InstKind::Jump(entry_jump) = data.inst_data(pred).kind() else {
                return false;
            };
            let args = entry_jump.args().to_vec();
            if args.len() != params.len() {
                return false;
            }
            if entry_edge.replace((pred, args)).is_some() {
                return false;
            }
        }
        let Some((entry_edge_inst, entry_args)) = entry_edge else {
            return false;
        };

        let elem_ty = data.inst_data(store_gep).ty().derefernce();
        let elem_size = elem_ty.size();
        if elem_size == 0 || elem_size > i32::MAX as usize {
            return false;
        }
        let elem_size = elem_size as i32;

        // Replace the loop with a runtime-length MemZero in the entry block.
        let entry_block = data.layout().parent_bb(entry_edge_inst).unwrap();
        let size = data.new_local_value().integer(elem_size);
        let byte_len = data
            .new_local_value()
            .binary(BinaryOp::Mul, entry_args[t_pos], size);
        // When the store GEP's first offset is the loop induction variable
        // (e.g. `buf[i] = 0`), the row prefix is empty: the row starts at
        // `base` itself. Reuse `base` directly — materializing a no-offset
        // GEP would insert a block-arg/constant reference into the layout,
        // which later passes (e.g. DCE's critical-inst scan) assert never
        // happens.
        let clear = if row_offsets.is_empty() {
            data.new_local_value().mem_zero_dynamic(base, byte_len)
        } else {
            let row_start = data.new_local_value().get_elem_ptr(base, row_offsets);
            data.layout_mut()
                .insert_before_terminator(entry_block, row_start);
            data.new_local_value().mem_zero_dynamic(row_start, byte_len)
        };
        data.layout_mut()
            .insert_before_terminator(entry_block, byte_len);
        data.layout_mut()
            .insert_before_terminator(entry_block, clear);
        data.replace_inst_with(entry_edge_inst).jump(exit, vec![]);

        true
    }

    /// A value is available before the loop when it is a constant/global, a
    /// function parameter, or is defined in a block outside the loop body.
    fn available_before_loop(
        data: &FunctionData,
        header: BasicBlock,
        body: BasicBlock,
        params: &[Inst],
        value: Inst,
    ) -> bool {
        if value.is_global() || data.inst_data(value).kind().is_const() {
            return true;
        }
        if params.contains(&value) {
            return false;
        }
        match data.layout().parent_bb(value) {
            Some(block) => block != header && block != body,
            None => matches!(data.inst_data(value).kind(), InstKind::BlockArgRef(..)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        Program, Type,
        builder_trait::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
        inst_kind::MemZeroLen,
    };

    fn build_zero_loop(
        program: &mut Program,
    ) -> (Function, BasicBlock, BasicBlock, BasicBlock, Inst) {
        let func = program.new_function(
            Type::get_unit(),
            "zero_loop".into(),
            vec![
                Type::get_pointer(Type::get_array(Type::get_i32(), 1024)),
                Type::get_i32(),
            ],
        );
        let data = program.func_data_mut(func);
        let entry = data.add_entry_block();
        let row = data.params()[0];
        let trip = data.params()[1];
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32(), Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero, trip]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let j = data.bb_data(header).params()[0];
        let t = data.bb_data(header).params()[1];
        let header_jump = data.new_local_inst().jump(body, vec![]);
        data.layout_mut().insert_inst(header, header_jump);

        let gep = data.new_local_inst().get_elem_ptr(row, vec![zero, j]);
        let store = data.new_local_inst().store(zero, gep);
        let one = data.new_local_inst().integer(1);
        let next_j = data.new_local_inst().binary(BinaryOp::Add, j, one);
        let next_t = data.new_local_inst().binary(BinaryOp::Sub, t, one);
        let latch =
            data.new_local_inst()
                .branch(next_t, header, vec![next_j, next_t], exit, vec![]);
        for inst in [gep, store, next_j, next_t, latch] {
            data.layout_mut().insert_inst(body, inst);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        (func, entry, header, exit, entry_jump)
    }

    fn run(data: &mut ArenaContextMut<'_>) -> bool {
        ZeroStoreLoop.run_on(data)
    }

    #[test]
    fn converts_a_zeroing_countdown_loop_to_memzero() {
        let mut program = Program::new();
        let (func, entry, header, exit, entry_jump) = build_zero_loop(&mut program);
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(func),
        };

        assert!(run(&mut data));
        let data = data.curr_func_data();
        // The entry block now holds a runtime-length MemZero over the row.
        let mem_zero = data
            .layout()
            .basicblock(entry)
            .insts()
            .iter()
            .copied()
            .find(|&inst| matches!(data.inst_data(inst).kind(), InstKind::MemZero(..)))
            .expect("entry block must contain the MemZero");
        let InstKind::MemZero(mem_zero) = data.inst_data(mem_zero).kind() else {
            unreachable!()
        };
        assert!(matches!(mem_zero.byte_len_len(), MemZeroLen::Value(_)));
        let MemZeroLen::Value(byte_len) = mem_zero.byte_len_len() else {
            unreachable!()
        };
        // The length is `trip * 4`.
        let InstKind::Binary(mul) = data.inst_data(*byte_len).kind() else {
            panic!("length must be a multiplication");
        };
        assert_eq!(mul.op(), BinaryOp::Mul);
        let InstKind::Integer(four) = data.inst_data(mul.rhs()).kind() else {
            panic!("element size must be the constant 4");
        };
        assert_eq!(four.value(), 4);

        // The entry terminator now falls into the exit instead of the loop.
        let InstKind::Jump(jump) = data.inst_data(entry_jump).kind() else {
            panic!("entry must end in a jump to the exit");
        };
        assert_eq!(jump.target(), exit);
        assert!(jump.args().is_empty());

        // The loop header is no longer reachable from the entry.
        assert!(
            !data
                .bb_data(header)
                .used_by()
                .iter()
                .any(|&pred| data.layout().parent_bb(pred) == Some(entry))
        );
    }

    #[test]
    fn refuses_loops_that_store_a_nonzero_value() {
        let mut program = Program::new();
        let (func, _entry, _header, _exit, _entry_jump) = build_zero_loop(&mut program);
        let (store_inst, dest) = {
            let data = program.func_data(func);
            let body = data
                .layout()
                .basicblocks()
                .iter()
                .find(|layout| {
                    layout
                        .insts()
                        .iter()
                        .any(|&inst| matches!(data.inst_data(inst).kind(), InstKind::Store(..)))
                })
                .unwrap()
                .bb();
            let store_inst = data
                .layout()
                .basicblock(body)
                .insts()
                .iter()
                .copied()
                .find(|&inst| matches!(data.inst_data(inst).kind(), InstKind::Store(..)))
                .unwrap();
            let InstKind::Store(store) = data.inst_data(store_inst).kind() else {
                unreachable!()
            };
            (store_inst, store.dest())
        };
        {
            let data = program.func_data_mut(func);
            let seven = data.new_local_inst().integer(7);
            data.replace_inst_with(store_inst).store(seven, dest);
        }

        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(func),
        };
        assert!(!run(&mut data));
    }
}
