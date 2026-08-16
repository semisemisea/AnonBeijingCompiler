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
//!
//! ---
//!
//! ## 补充说明（中文）
//!
//! 术语：header / latch / preheader / 回边 / test-at-top / test-at-bottom /
//! trip count 等见 `docs/offline-handbook/glossary.md` 的"循环"分组；`MemZero`
//! 指令与 `TargetPolicy` 目标门控见同一文件的"本项目特有"分组。零初始化循环、
//! countdown 形态、旋转等概念按字面理解，不再展开。
//!
//! ### 一句话定位
//!
//! `ZeroStoreLoop` 把"逐元素写 0"的零初始化循环折叠成**一条**运行时长度的
//! `MemZero`（AArch64 后端展开为 `bl memset`），用一次内存清零调用替换每个元素
//! 的一次 store 加整套循环控制（递减、测试、回边）。典型收益对象是 SysY 的
//! `int a[1024] = {0}` 这类大数组零初始化。
//!
//! ### 变换形态
//!
//! 英文文档上方的示意图就是折叠前形态（旋转后的 countdown 循环）；折叠后：
//!
//! ```text
//! pre:  byte_len = t0 * elem_size;
//!       row_start = gep(base, row_offsets);   // row_offsets 为空时直接用 base
//!       MemZero(row_start, byte_len); jump exit
//! ```
//!
//! 旋转已把 guard `t0 > 0` 放进 pre-header，guard 失败直跳 exit；折叠只在 guard
//! 的真路径上执行，因此 trip = 0 的"零次执行"语义不变。`byte_len` 是运行时值
//! （`MemZeroLen::Value`），因为 `t0` 来自 entry 边传入的参数。
//!
//! ### 触发 / 放弃条件
//!
//! `run_on` 对除 entry 外的每个块调用 `convert_zero_loop` 做候选检查（`header ==
//! entry` 直接跳过），任一条件不满足即放弃：
//!
//! - **countdown 形态**：header 终结符必须是**无参数** `Jump`（旋转后的
//!   `jump body`）；body 终结符必须是 latch 形态 `br (t - 1), header, exit`——
//!   条件为 `BinaryOp::Sub` 且 rhs 是 `Integer(1)`，`t` 是 header 参数；
//! - **exit 干净**：`exit` 无参数，且不读任何 header 参数（否则旋转后绕过
//!   header 的值流会失效）；
//! - **体内容**：body 里**恰好一条**零 store——源是 `Integer(0)` 或 `ZeroInit`，
//!   目标是 `GetElemPtr` 且其**最后一个偏移**是 header 参数（IV `j`，且
//!   `j_pos != t_pos`）；body 不得含 `Call` / `TailCall` / `Load` / `MemZero`；
//! - **地址在循环前可用**：`base` 与所有前导偏移（`row_offsets`）必须通过
//!   `available_before_loop`——常量 / 全局 / 函数参数，或定义在 header / body
//!   之外的块里（无 parent 的 `BlockArgRef` 也可用）；header 参数与 body 内定义
//!   的值会随循环一起消失，不可用；
//! - **恰好一条 entry 边**：header 除回边外唯一的前驱是参数个数与 header 参数
//!   一致的 `Jump`；
//! - **长度可行**：元素大小 `elem_size`（store GEP 指针类型 deref 的大小）
//!   非零且 ≤ `i32::MAX`（`MemZero` 长度是 i32 量）。
//!
//! ### 正确性要点
//!
//! - 折叠的安全依据与旋转相同：guard 只 gate 第一次迭代，guard 通过则循环体
//!   至少执行一次，而 `MemZero` 清零的地址范围 `[row_start, row_start +
//!   t0 * elem_size)` 恰好等于逐元素 store 0 覆盖的范围，效果一致；
//! - 长度 `byte_len = entry_args[t_pos] * elem_size` 与（需要时的）`row_start`
//!   都插在 entry 块终结符之前；`row_offsets` 为空时直接用 `base`，不新建 GEP
//!   （`uses_base_pointer_when_the_row_has_no_leading_offsets` 专门验证这一点）；
//! - entry 的 jump 改写为 `jump(exit)`（`replace_inst_with`），header / body 变成
//!   不可达死代码，由后续 fixpoint 轮里的 `simplify_cfg` / `dce` 清理；
//! - `elem_size == 0` 拒绝：`t0 * 0` 长度恒为 0，`memset` 无效。
//!
//! ### 管线位置
//!
//! - 注册：`opt/pass.rs` 的 `PassesManager::from_config`，fixpoint 段，
//!   `rotate_loops` **之后**、`chain_to_switch` 之前；
//! - 依赖旋转：本 pass 识别的是旋转后的 countdown 形态（body 终结符
//!   `br (t-1), header, exit`），所以必须排在 `rotate_loops` 后面；
//! - 目标门控：`config.target.enable_chain_to_switch`——AArch64 专属，RISC-V
//!   不注册（`MemZero` 展开为 `bl memset` 是 AArch64 后端的做法）。
//!
//! ### 验证
//!
//! - 本文件 `mod tests`（240 行起）：
//!   - `converts_a_zeroing_countdown_loop_to_memzero`：entry 出现运行时长度
//!     `MemZero`（`byte_len` 是 `BinaryOp::Mul`：`t0 * 4`），entry jump 直跳
//!     exit，header 不再被 entry 引用；
//!   - `uses_base_pointer_when_the_row_has_no_leading_offsets`：flat 数组
//!     （store GEP 只有 IV 一个偏移）时 `MemZero` 目标直接是数组指针参数，
//!     不新建 GEP，块里也没有 `BlockArgRef`；
//!   - `refuses_loops_that_store_a_nonzero_value`：store 非零常量（`Integer(7)`）
//!     时放弃，IR 不变；
//! - 端到端：`cargo test -p raana_ir` 与 `make test`（差分比对）。

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
        let (row_start, row_start_is_new) = if row_offsets.is_empty() {
            (base, false)
        } else {
            let gep = data.new_local_value().get_elem_ptr(base, row_offsets);
            (gep, true)
        };
        let clear = data.new_local_value().mem_zero_dynamic(row_start, byte_len);
        data.layout_mut()
            .insert_before_terminator(entry_block, byte_len);
        if row_start_is_new {
            data.layout_mut()
                .insert_before_terminator(entry_block, row_start);
        }
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

    /// A flat array loop `arr[i] = 0` where `arr` is already an element
    /// pointer: the store GEP has a single (IV) offset, so `row_offsets` is
    /// empty and the MemZero must address `arr` itself, not a GEP of `arr`.
    #[test]
    fn uses_base_pointer_when_the_row_has_no_leading_offsets() {
        let mut program = Program::new();
        let func = program.new_function(
            Type::get_unit(),
            "clear_flat".into(),
            vec![Type::get_pointer(Type::get_i32()), Type::get_i32()],
        );
        {
            let data = program.func_data_mut(func);
            let entry = data.add_entry_block();
            let arr = data.params()[0];
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

            let gep = data.new_local_inst().get_elem_ptr(arr, vec![j]);
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
        }

        let mut context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(func),
        };
        assert!(run(&mut context));
        let data = context.curr_func_data();
        let mem_zero = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|layout| layout.insts().iter().copied())
            .find(|&inst| matches!(data.inst_data(inst).kind(), InstKind::MemZero(..)))
            .expect("a MemZero must be created");
        let InstKind::MemZero(mem_zero) = data.inst_data(mem_zero).kind() else {
            unreachable!()
        };
        // The MemZero targets the flat array pointer directly; its dest must
        // not be a fresh GEP (and the block's instruction list must not
        // contain a block-arg reference).
        assert_eq!(mem_zero.dest(), data.params()[0]);
        assert!(data.layout().basicblocks().iter().all(|layout| {
            layout
                .insts()
                .iter()
                .all(|&inst| !matches!(data.inst_data(inst).kind(), InstKind::BlockArgRef(..)))
        }));
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
