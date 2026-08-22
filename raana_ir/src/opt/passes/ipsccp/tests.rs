use super::*;
use crate::{
    ir::{
        BinaryOp,
        builder::{BasicBlockBuilder, GlobalInstBuilder, LocalInstBuilder, ScalarInstBuilder},
    },
    llvm::LlvmWriter,
    opt::pass::ArenaContextMut,
};

fn build_float_cast(value: f32) -> (Program, Function, Inst, Inst) {
    let mut program = Program::new();
    let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
    let data = program.func_data_mut(main);
    let entry = data.add_entry_block();
    let source = data.new_local_inst().float(value);
    let cast = data.new_local_inst().cast(source, Type::get_i32());
    let ret = data.new_local_inst().ret(Some(cast));
    data.layout_mut().insert_inst(entry, cast);
    data.layout_mut().insert_inst(entry, ret);
    (program, main, cast, ret)
}

fn assert_float_cast_folds(value: f32, expected: i32) {
    let (mut program, main, cast, ret) = build_float_cast(value);

    assert!(IPSCCP.run(&mut program));

    let data = program.func_data(main);
    assert!(matches!(
        data.inst_data(cast).kind(),
        InstKind::Integer(integer) if integer.value() == expected
    ));
    assert_eq!(data.layout().parent_bb(cast), None);
    let InstKind::Return(ret) = data.inst_data(ret).kind() else {
        panic!("expected return instruction")
    };
    assert_eq!(ret.value(), Some(cast));
}

#[test]
fn folds_in_range_float_literals_to_i32() {
    assert_float_cast_folds(3.75, 3);
    assert_float_cast_folds(-2.75, -2);
    assert_float_cast_folds(i32::MIN as f32, i32::MIN);
}

#[test]
fn keeps_non_finite_and_out_of_range_float_casts() {
    for value in [i32::MAX as f32, 1.0e10, f32::INFINITY, f32::NAN] {
        let (mut program, main, cast, _) = build_float_cast(value);

        assert!(!IPSCCP.run(&mut program));
        assert!(matches!(
            program.func_data(main).inst_data(cast).kind(),
            InstKind::Cast(..)
        ));
    }
}

/// Build a self-recursive tail-call function and verify that IPSCCP does
/// not mis-propagate a parameter that varies across recursive tail calls.
///
/// ```text
/// fun(n: i32, dep: i32) -> i32:
///     if n == 0: return dep      // base case — dep must stay variable
///     else: tail_call fun(n - 1, dep + 1)
/// main(): return fun(2, 0)
/// ```
///
/// With correct tail-call argument propagation, `dep` receives both
/// `Constant(0)` (from main) and `Constant(1)` (from the recursive
/// `dep+1`), converging to `Bottom`. Without the fix, IPSCCP would only
/// see the non-tail call `fun(2, 0)` and constant-propagate `dep` to `0`,
/// replacing the `ret dep` with `ret 0`.
#[test]
fn tail_call_args_prevent_constant_mispropagation() {
    let mut program = Program::new();

    // fun(n: i32, dep: i32) -> i32
    let fun = program.new_function(
        Type::get_i32(),
        "fun".into(),
        vec![Type::get_i32(), Type::get_i32()],
    );
    let dep_param = {
        let data = program.func_data_mut(fun);
        let entry = data.add_entry_block();
        let params = data.bb_data(entry).params();
        let n = params[0];
        let dep = params[1];

        let base = data.new_basic_block().basic_block("base".into(), vec![]);
        let rec = data.new_basic_block().basic_block("rec".into(), vec![]);
        data.layout_mut().push_bb_back(base);
        data.layout_mut().push_bb_back(rec);

        // entry: br (n == 0), base, rec
        // (Integer constants are not placed in the layout — IPSCCP
        // initialises their lattice in Stage 0.1.)
        let zero = data.new_local_inst().integer(0);
        let cond = data.new_local_inst().binary(BinaryOp::Eq, n, zero);
        let br = data
            .new_local_inst()
            .branch(cond, base, vec![], rec, vec![]);
        data.layout_mut().insert_inst(entry, cond);
        data.layout_mut().insert_inst(entry, br);

        // base: ret dep
        let ret_dep = data.new_local_inst().ret(Some(dep));
        data.layout_mut().insert_inst(base, ret_dep);

        // rec: tail_call fun(n - 1, dep + 1)
        let one = data.new_local_inst().integer(1);
        let nm1 = data.new_local_inst().binary(BinaryOp::Sub, n, one);
        let one2 = data.new_local_inst().integer(1);
        let depp1 = data.new_local_inst().binary(BinaryOp::Add, dep, one2);
        let tc = data.new_local_inst().tail_call(fun, vec![nm1, depp1]);
        data.layout_mut().insert_inst(rec, nm1);
        data.layout_mut().insert_inst(rec, depp1);
        data.layout_mut().insert_inst(rec, tc);

        dep
    };

    // main() -> i32: return fun(2, 0)
    let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
    {
        let data = program.func_data_mut(main);
        let entry = data.add_entry_block();
        let two = data.new_local_inst().integer(2);
        let zero = data.new_local_inst().integer(0);
        let call = data
            .new_local_inst()
            .call_with_type(fun, vec![two, zero], Type::get_i32());
        let ret = data.new_local_inst().ret(Some(call));
        data.layout_mut().insert_inst(entry, call);
        data.layout_mut().insert_inst(entry, ret);
    }

    IPSCCP.run(&mut program);

    // After IPSCCP, `dep` must still be a BlockArgRef (not folded to an
    // Integer constant). If the tail-call arm were missing, dep's lattice
    // would be Constant(0) and `replace_inst_with` would have mutated its
    // data to Integer(0).
    let data = program.func_data(fun);
    assert!(
        matches!(data.inst_data(dep_param).kind(), InstKind::BlockArgRef(..)),
        "dep parameter was constant-propagated — tail-call arg propagation is broken"
    );
}

#[test]
fn constant_block_param_replaces_uses_without_mutating_param() {
    let mut program = Program::new();
    let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
    let (param, ret) = {
        let data = program.func_data_mut(main);
        let entry = data.add_entry_block();
        let target = data
            .new_basic_block()
            .basic_block("target".into(), vec![Type::get_i32()]);
        data.layout_mut().push_bb_back(target);

        let seven = data.new_local_inst().integer(7);
        let jump = data.new_local_inst().jump(target, vec![seven]);
        data.layout_mut().insert_inst(entry, jump);

        let param = data.bb_data(target).params()[0];
        let ret = data.new_local_inst().ret(Some(param));
        data.layout_mut().insert_inst(target, ret);
        (param, ret)
    };

    assert!(IPSCCP.run(&mut program));

    let data = program.func_data(main);
    assert!(matches!(
        data.inst_data(param).kind(),
        InstKind::BlockArgRef(..)
    ));
    let InstKind::Return(ret_data) = data.inst_data(ret).kind() else {
        panic!("expected return instruction")
    };
    let value = ret_data.value().expect("return should have a value");
    assert!(matches!(data.inst_data(value).kind(), InstKind::Integer(int) if int.value() == 7));

    let mut writer = LlvmWriter::new(&program);
    writer.write().unwrap();
    let llvm = writer.finish();
    assert!(llvm.contains("phi i32 [ 7,"), "{llvm}");
    assert!(!llvm.contains("  7 = phi"), "{llvm}");
}

#[test]
fn constant_function_param_replaces_uses_without_mutating_abi_param() {
    let mut program = Program::new();
    let callee = program.new_function(Type::get_i32(), "callee".into(), vec![Type::get_i32()]);
    let (param, ret) = {
        let data = program.func_data_mut(callee);
        let entry = data.add_entry_block();
        let param = data.params()[0];
        let ret = data.new_local_inst().ret(Some(param));
        data.layout_mut().insert_inst(entry, ret);
        (param, ret)
    };

    let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
    {
        let data = program.func_data_mut(main);
        let entry = data.add_entry_block();
        let seven = data.new_local_inst().integer(7);
        let call = data
            .new_local_inst()
            .call_with_type(callee, vec![seven], Type::get_i32());
        let ret = data.new_local_inst().ret(Some(call));
        data.layout_mut().insert_inst(entry, call);
        data.layout_mut().insert_inst(entry, ret);
    }

    assert!(IPSCCP.run(&mut program));

    let data = program.func_data(callee);
    assert!(matches!(
        data.inst_data(param).kind(),
        InstKind::BlockArgRef(..)
    ));
    assert!(matches!(
        data.inst_data(data.params()[0]).kind(),
        InstKind::BlockArgRef(..)
    ));
    let InstKind::Return(ret_data) = data.inst_data(ret).kind() else {
        panic!("expected return instruction")
    };
    let value = ret_data.value().expect("return should have a value");
    assert!(matches!(data.inst_data(value).kind(), InstKind::Integer(int) if int.value() == 7));

    let mut writer = LlvmWriter::new(&program);
    writer.write().unwrap();
    let llvm = writer.finish();
    assert!(llvm.contains("define i32 @callee(i32 %"), "{llvm}");
    assert!(!llvm.contains("define i32 @callee(i32 7)"), "{llvm}");
}

// --- main-memory simulation (constant-offset cells) ---

fn new_global(program: &mut Program) -> Inst {
    let init = program.new_value().zero_init(Type::get_i32());
    program.new_value().global_alloc(init)
}

#[test]
fn folds_store_load_roundtrip_on_global_cell() {
    let mut program = Program::new();
    let global = new_global(&mut program);
    let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
    let (load, ret) = {
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(main),
        };
        let entry = data.add_entry_block();
        let zero = data.new_local_value().integer(0);
        let gep = data.new_local_value().get_elem_ptr(global, vec![zero]);
        data.layout_mut().insert_inst(entry, gep);
        let five = data.new_local_value().integer(5);
        let store = data.new_local_value().store(five, gep);
        data.layout_mut().insert_inst(entry, store);
        let load = data.new_local_value().load(gep);
        data.layout_mut().insert_inst(entry, load);
        let ret = data.new_local_value().ret(Some(load));
        data.layout_mut().insert_inst(entry, ret);
        (load, ret)
    };

    assert!(IPSCCP.run(&mut program));
    let data = program.func_data(main);
    assert!(matches!(
        data.inst_data(load).kind(),
        InstKind::Integer(integer) if integer.value() == 5
    ));
    let InstKind::Return(ret_data) = data.inst_data(ret).kind() else {
        panic!("expected return instruction")
    };
    assert_eq!(ret_data.value(), Some(load));
}

#[test]
fn zero_initialized_global_load_folds_to_zero() {
    let mut program = Program::new();
    let global = new_global(&mut program);
    let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
    let (load, _ret) = {
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(main),
        };
        let entry = data.add_entry_block();
        let zero = data.new_local_value().integer(0);
        let gep = data.new_local_value().get_elem_ptr(global, vec![zero]);
        data.layout_mut().insert_inst(entry, gep);
        let load = data.new_local_value().load(gep);
        data.layout_mut().insert_inst(entry, load);
        let ret = data.new_local_value().ret(Some(load));
        data.layout_mut().insert_inst(entry, ret);
        (load, ret)
    };

    assert!(IPSCCP.run(&mut program));
    let data = program.func_data(main);
    assert!(matches!(
        data.inst_data(load).kind(),
        InstKind::Integer(integer) if integer.value() == 0
    ));
}

#[test]
fn mem_zero_makes_local_array_loads_zero() {
    let mut program = Program::new();
    let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
    let (load, _ret) = {
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(main),
        };
        let entry = data.add_entry_block();
        let alloc = data
            .new_local_value()
            .alloc(Type::get_array(Type::get_i32(), 4));
        data.layout_mut().insert_inst(entry, alloc);
        let clear = data.new_local_value().mem_zero(alloc, 16);
        data.layout_mut().insert_inst(entry, clear);
        let zero = data.new_local_value().integer(0);
        // (0, 0) 双索引：对 *[i32;4] 数组取标量元素指针，load 才是 i32 标量。
        // 单索引 (0) 得到聚合指针（*[i32;4]），load 出数组类型——聚合值
        // 不该折叠成 int 0（IPSCCP 的零区间折叠只对 i32 标量 load 生效，
        // 见 MemState::read 的类型 guard）。
        let gep = data.new_local_value().get_elem_ptr(alloc, vec![zero, zero]);
        data.layout_mut().insert_inst(entry, gep);
        let load = data.new_local_value().load(gep);
        data.layout_mut().insert_inst(entry, load);
        let ret = data.new_local_value().ret(Some(load));
        data.layout_mut().insert_inst(entry, ret);
        (load, ret)
    };

    assert!(IPSCCP.run(&mut program));
    let data = program.func_data(main);
    assert!(matches!(
        data.inst_data(load).kind(),
        InstKind::Integer(integer) if integer.value() == 0
    ));
}

#[test]
fn call_to_writer_invalidates_global_cell() {
    let mut program = Program::new();
    let global = new_global(&mut program);
    // The writer stores its (unknown) argument into the global.
    let writer = program.new_function(Type::get_unit(), "writer".into(), vec![Type::get_i32()]);
    {
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(writer),
        };
        let entry = data.add_entry_block();
        let param = data.params()[0];
        let store = data.new_local_value().store(param, global);
        data.layout_mut().insert_inst(entry, store);
        let ret = data.new_local_value().ret(None);
        data.layout_mut().insert_inst(entry, ret);
    }
    let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
    // An unknown value flows into the writer's parameter.
    let getint = program.new_function(Type::get_i32(), "getint".into(), vec![]);
    let (load, _ret) = {
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(main),
        };
        let entry = data.add_entry_block();
        let zero = data.new_local_value().integer(0);
        let gep = data.new_local_value().get_elem_ptr(global, vec![zero]);
        data.layout_mut().insert_inst(entry, gep);
        let five = data.new_local_value().integer(5);
        let store = data.new_local_value().store(five, gep);
        data.layout_mut().insert_inst(entry, store);
        let input = data.new_local_value().call(getint, vec![]);
        data.layout_mut().insert_inst(entry, input);
        let call = data.new_local_value().call(writer, vec![input]);
        data.layout_mut().insert_inst(entry, call);
        let load = data.new_local_value().load(gep);
        data.layout_mut().insert_inst(entry, load);
        let ret = data.new_local_value().ret(Some(load));
        data.layout_mut().insert_inst(entry, ret);
        (load, ret)
    };

    let _ = IPSCCP.run(&mut program);
    let data = program.func_data(main);
    // The writer may have overwritten the cell with an unknown value:
    // the load stays a load.
    assert!(matches!(data.inst_data(load).kind(), InstKind::Load(..)));
}

#[test]
fn caller_store_blocks_zero_merge_in_callee() {
    // main 存 getint 结果到全局、helper 读：caller 侧 store 在跨函数时序
    // 上无法排除先于本 load——折叠初始 0 会把 getint() 结果固化
    // （fuzz: functional/70_dijkstra 的 gv_n 折叠成 0，Dijkstra 循环全死，
    // 输出全零）。有跨函数 writer 的 cell 禁止 zero-merge。
    let mut program = Program::new();
    let global = new_global(&mut program);
    let getint = program.new_function(Type::get_i32(), "getint".into(), vec![]);
    let helper = program.new_function(Type::get_i32(), "helper".into(), vec![]);
    let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
    let load = {
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(helper),
        };
        let entry = data.add_entry_block();
        let zero = data.new_local_value().integer(0);
        let gep = data.new_local_value().get_elem_ptr(global, vec![zero]);
        data.layout_mut().insert_inst(entry, gep);
        let load = data.new_local_value().load(gep);
        data.layout_mut().insert_inst(entry, load);
        let ret = data.new_local_value().ret(Some(load));
        data.layout_mut().insert_inst(entry, ret);
        load
    };
    {
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(main),
        };
        let entry = data.add_entry_block();
        let zero = data.new_local_value().integer(0);
        let gep = data.new_local_value().get_elem_ptr(global, vec![zero]);
        data.layout_mut().insert_inst(entry, gep);
        let input = data.new_local_value().call(getint, vec![]);
        data.layout_mut().insert_inst(entry, input);
        let store = data.new_local_value().store(input, gep);
        data.layout_mut().insert_inst(entry, store);
        let call = data.new_local_value().call(helper, vec![]);
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_value().ret(Some(call));
        data.layout_mut().insert_inst(entry, ret);
    }

    let _ = IPSCCP.run(&mut program);
    let data = program.func_data(helper);
    assert!(matches!(data.inst_data(load).kind(), InstKind::Load(..)));
}

#[test]
fn call_to_deterministic_writer_folds_load() {
    let mut program = Program::new();
    let global = new_global(&mut program);
    // The writer stores a compile-time constant into the global.
    let writer = program.new_function(Type::get_unit(), "writer".into(), vec![]);
    {
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
    }
    let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
    let (load, _ret) = {
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(main),
        };
        let entry = data.add_entry_block();
        let zero = data.new_local_value().integer(0);
        let gep = data.new_local_value().get_elem_ptr(global, vec![zero]);
        data.layout_mut().insert_inst(entry, gep);
        let call = data.new_local_value().call(writer, vec![]);
        data.layout_mut().insert_inst(entry, call);
        let load = data.new_local_value().load(gep);
        data.layout_mut().insert_inst(entry, load);
        let ret = data.new_local_value().ret(Some(load));
        data.layout_mut().insert_inst(entry, ret);
        (load, ret)
    };

    assert!(IPSCCP.run(&mut program));
    let data = program.func_data(main);
    // The callee's constant store is modeled: the load folds to 1.
    assert!(matches!(
        data.inst_data(load).kind(),
        InstKind::Integer(integer) if integer.value() == 1
    ));
}

#[test]
fn distinct_offsets_do_not_share_cells() {
    let mut program = Program::new();
    let global = new_global(&mut program);
    let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
    let (load, _ret) = {
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(main),
        };
        let entry = data.add_entry_block();
        let zero = data.new_local_value().integer(0);
        let one = data.new_local_value().integer(1);
        let gep0 = data.new_local_value().get_elem_ptr(global, vec![zero]);
        let gep1 = data.new_local_value().get_elem_ptr(global, vec![one]);
        data.layout_mut().insert_inst(entry, gep0);
        data.layout_mut().insert_inst(entry, gep1);
        let five = data.new_local_value().integer(5);
        let store = data.new_local_value().store(five, gep0);
        data.layout_mut().insert_inst(entry, store);
        let load = data.new_local_value().load(gep1);
        data.layout_mut().insert_inst(entry, load);
        let ret = data.new_local_value().ret(Some(load));
        data.layout_mut().insert_inst(entry, ret);
        (load, ret)
    };

    let _ = IPSCCP.run(&mut program);
    let data = program.func_data(main);
    // g[1] was never written: still a load.
    assert!(matches!(data.inst_data(load).kind(), InstKind::Load(..)));
}

#[test]
fn divergent_stores_merge_to_bottom() {
    let mut program = Program::new();
    let global = new_global(&mut program);
    let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
    let (load, _ret) = {
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(main),
        };
        let entry = data.add_entry_block();
        let then_block = data.new_basic_block().basic_block("then".into(), vec![]);
        let else_block = data.new_basic_block().basic_block("else".into(), vec![]);
        let merge = data.new_basic_block().basic_block("merge".into(), vec![]);
        for block in [then_block, else_block, merge] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_value().integer(0);
        let gep = data.new_local_value().get_elem_ptr(global, vec![zero]);
        data.layout_mut().insert_inst(entry, gep);
        let one = data.new_local_value().integer(1);
        let store_one = data.new_local_value().store(one, gep);
        data.layout_mut().insert_inst(entry, store_one);
        let cond = data.new_local_value().load(gep);
        data.layout_mut().insert_inst(entry, cond);
        let branch = data
            .new_local_value()
            .branch(cond, then_block, vec![], else_block, vec![]);
        data.layout_mut().insert_inst(entry, branch);

        let two = data.new_local_value().integer(2);
        let store_two = data.new_local_value().store(two, gep);
        data.layout_mut().insert_inst(else_block, store_two);
        let jump_t = data.new_local_value().jump(merge, vec![]);
        data.layout_mut().insert_inst(then_block, jump_t);
        let jump_e = data.new_local_value().jump(merge, vec![]);
        data.layout_mut().insert_inst(else_block, jump_e);

        let load = data.new_local_value().load(gep);
        data.layout_mut().insert_inst(merge, load);
        let ret = data.new_local_value().ret(Some(load));
        data.layout_mut().insert_inst(merge, ret);
        (load, ret)
    };

    let _ = IPSCCP.run(&mut program);
    let data = program.func_data(main);
    // Two different constant writers on different paths: Bottom, so the
    // load is not folded to either.
    assert!(matches!(data.inst_data(load).kind(), InstKind::Load(..)));
}

// --- 位置感知 read（docs/ipsccp_memory_order_fix.md §4）：load 只折叠
// 程序序上先于它的 writer，绝不允许读到更晚写入的值 ---

/// 洞 1（case_0019 最小形态）：同块内 load 先于 store。store 在程序序上
/// 位于 load 之后，对 load 不可见——load 折叠为初始 0，而不是 5。
#[test]
fn load_before_store_in_same_block_stays_zero() {
    let mut program = Program::new();
    let global = new_global(&mut program);
    let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
    let (load, _ret) = {
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(main),
        };
        let entry = data.add_entry_block();
        let zero = data.new_local_value().integer(0);
        let gep = data.new_local_value().get_elem_ptr(global, vec![zero]);
        data.layout_mut().insert_inst(entry, gep);
        let load = data.new_local_value().load(gep);
        data.layout_mut().insert_inst(entry, load);
        let five = data.new_local_value().integer(5);
        let store = data.new_local_value().store(five, gep);
        data.layout_mut().insert_inst(entry, store);
        let ret = data.new_local_value().ret(Some(load));
        data.layout_mut().insert_inst(entry, ret);
        (load, ret)
    };

    assert!(IPSCCP.run(&mut program));
    let data = program.func_data(main);
    // load 先执行：必须读到初始 0（store 的重调度不得把 5 灌给它）。
    assert!(matches!(
        data.inst_data(load).kind(),
        InstKind::Integer(integer) if integer.value() == 0
    ));
}

/// 循环携带 cell（fuzz: case_0030 形态）：同块内 load 先于 store，但块在
/// 循环内（回边）——下一轮迭代 store 先于 load 执行，cell 的值是固定点
/// （`g = 1 - g` 跨轮次 0/1 交替），不是初始 0。load 不得折叠成 0。
#[test]
fn loop_carried_cell_load_not_folded_to_init() {
    let mut program = Program::new();
    let global = new_global(&mut program);
    let main = program.new_function(Type::get_i32(), "main".into(), vec![Type::get_i32()]);
    let (load, _ret) = {
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(main),
        };
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        data.layout_mut().push_bb_back(header);
        data.layout_mut().push_bb_back(body);
        data.layout_mut().push_bb_back(exit);
        let bound = data.params()[0];
        let zero = data.new_local_value().integer(0);
        let gep = data.new_local_value().get_elem_ptr(global, vec![zero]);
        data.layout_mut().insert_inst(entry, gep);
        let entry_jump = data.new_local_value().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);

        let r = data.bb_data(header).params()[0];
        let test = data.new_local_value().binary(BinaryOp::Lt, r, bound);
        let branch = data
            .new_local_value()
            .branch(test, body, vec![], exit, vec![]);
        data.layout_mut().insert_inst(header, test);
        data.layout_mut().insert_inst(header, branch);

        // body：load g；store (1 - g)；r += 1；回边。
        let load = data.new_local_value().load(gep);
        data.layout_mut().insert_inst(body, load);
        let one = data.new_local_value().integer(1);
        let sub = data.new_local_value().binary(BinaryOp::Sub, one, load);
        data.layout_mut().insert_inst(body, sub);
        let store = data.new_local_value().store(sub, gep);
        data.layout_mut().insert_inst(body, store);
        let r2 = data.new_local_value().binary(BinaryOp::Add, r, one);
        data.layout_mut().insert_inst(body, r2);
        let back = data.new_local_value().jump(header, vec![r2]);
        data.layout_mut().insert_inst(body, back);

        let ret = data.new_local_value().ret(Some(load));
        data.layout_mut().insert_inst(exit, ret);
        (load, ret)
    };

    let _ = IPSCCP.run(&mut program);
    let data = program.func_data(main);
    // 循环内 load：回边使 store 先于下一轮的 load——不得折叠为初始 0。
    assert!(matches!(data.inst_data(load).kind(), InstKind::Load(..)));
}

/// 洞 2：分支合并点的 load 只被一条臂的 store 写（`if (c) { g = 7; }`，
/// else 路径直通）。store 块不支配 merge 块且初始 0 可达：meet(7, 0) =
/// Bottom，load 保持为 load，不得折叠成 7。
#[test]
fn single_writer_on_one_branch_keeps_load() {
    let mut program = Program::new();
    let global = new_global(&mut program);
    let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
    let (load, _ret) = {
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(main),
        };
        let entry = data.add_entry_block();
        let then_block = data.new_basic_block().basic_block("then".into(), vec![]);
        let merge = data.new_basic_block().basic_block("merge".into(), vec![]);
        data.layout_mut().push_bb_back(then_block);
        data.layout_mut().push_bb_back(merge);

        let zero = data.new_local_value().integer(0);
        let gep = data.new_local_value().get_elem_ptr(global, vec![zero]);
        data.layout_mut().insert_inst(entry, gep);
        // 条件用未初始化局部对象的 load（Bottom，不折叠）：两臂都必须可
        // 达，且不触发 unknown-write 失效（否则 cleared_roots 会掩盖洞 2）。
        let alloc = data.new_local_value().alloc(Type::get_i32());
        data.layout_mut().insert_inst(entry, alloc);
        let cond = data.new_local_value().load(alloc);
        data.layout_mut().insert_inst(entry, cond);
        let branch = data
            .new_local_value()
            .branch(cond, then_block, vec![], merge, vec![]);
        data.layout_mut().insert_inst(entry, branch);

        let seven = data.new_local_value().integer(7);
        let store = data.new_local_value().store(seven, gep);
        data.layout_mut().insert_inst(then_block, store);
        let jump = data.new_local_value().jump(merge, vec![]);
        data.layout_mut().insert_inst(then_block, jump);

        let load = data.new_local_value().load(gep);
        data.layout_mut().insert_inst(merge, load);
        let ret = data.new_local_value().ret(Some(load));
        data.layout_mut().insert_inst(merge, ret);
        (load, ret)
    };

    let _ = IPSCCP.run(&mut program);
    let data = program.func_data(main);
    // else 路径读到 0、then 路径读到 7：值分歧，保持 load。
    assert!(matches!(data.inst_data(load).kind(), InstKind::Load(..)));
}

/// 洞 2 的补集：双分支都写同一常量（`if (c) { g = 5; } else { g = 5; }`），
/// 必经其一且初始 0 不可达——meet(5, 5) = 5，load 正常折叠（不误伤）。
#[test]
fn identical_writers_on_both_branches_fold() {
    let mut program = Program::new();
    let global = new_global(&mut program);
    let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
    let (load, _ret) = {
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(main),
        };
        let entry = data.add_entry_block();
        let then_block = data.new_basic_block().basic_block("then".into(), vec![]);
        let else_block = data.new_basic_block().basic_block("else".into(), vec![]);
        let merge = data.new_basic_block().basic_block("merge".into(), vec![]);
        for block in [then_block, else_block, merge] {
            data.layout_mut().push_bb_back(block);
        }

        let zero = data.new_local_value().integer(0);
        let gep = data.new_local_value().get_elem_ptr(global, vec![zero]);
        data.layout_mut().insert_inst(entry, gep);
        let alloc = data.new_local_value().alloc(Type::get_i32());
        data.layout_mut().insert_inst(entry, alloc);
        let cond = data.new_local_value().load(alloc);
        data.layout_mut().insert_inst(entry, cond);
        let branch = data
            .new_local_value()
            .branch(cond, then_block, vec![], else_block, vec![]);
        data.layout_mut().insert_inst(entry, branch);

        let five = data.new_local_value().integer(5);
        let store_t = data.new_local_value().store(five, gep);
        data.layout_mut().insert_inst(then_block, store_t);
        let jump_t = data.new_local_value().jump(merge, vec![]);
        data.layout_mut().insert_inst(then_block, jump_t);
        let five2 = data.new_local_value().integer(5);
        let store_e = data.new_local_value().store(five2, gep);
        data.layout_mut().insert_inst(else_block, store_e);
        let jump_e = data.new_local_value().jump(merge, vec![]);
        data.layout_mut().insert_inst(else_block, jump_e);

        let load = data.new_local_value().load(gep);
        data.layout_mut().insert_inst(merge, load);
        let ret = data.new_local_value().ret(Some(load));
        data.layout_mut().insert_inst(merge, ret);
        (load, ret)
    };

    assert!(IPSCCP.run(&mut program));
    let data = program.func_data(main);
    // 两臂同写 5：无条件读 5，折叠（0 被"必经 store"排除）。
    assert!(matches!(
        data.inst_data(load).kind(),
        InstKind::Integer(integer) if integer.value() == 5
    ));
}

/// 洞 3：caller 内 load 先于对 writer(callee) 的 call。callee 的 store 在
/// 程序序上位于 load 之后（call 块与 load 同块且 call 在 load 之后）——
/// reschedule 链不得把 callee 的值灌给 call 之前的 load。
///
/// 断言放宽为"不得折叠成 callee 的 store 值（1）"：invalidate_call 会把
/// 该 root 的 zero 区间一并清掉（call 可能写 root 任意位置），load 重读
/// 时初始值已不可知——保持 Load 或折叠初始 0 都正确；"零区间带位置"让
/// call 前 load 精确折叠 0 是 `docs/ipsccp_memory_order_fix.md` §7 的后续
/// 工作。
#[test]
fn load_before_call_to_writer_stays_zero() {
    let mut program = Program::new();
    let global = new_global(&mut program);
    // The writer stores a compile-time constant into the global.
    let writer = program.new_function(Type::get_unit(), "writer".into(), vec![]);
    {
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
    }
    let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
    let (load, _ret) = {
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(main),
        };
        let entry = data.add_entry_block();
        let zero = data.new_local_value().integer(0);
        let gep = data.new_local_value().get_elem_ptr(global, vec![zero]);
        data.layout_mut().insert_inst(entry, gep);
        let load = data.new_local_value().load(gep);
        data.layout_mut().insert_inst(entry, load);
        let call = data.new_local_value().call(writer, vec![]);
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_value().ret(Some(load));
        data.layout_mut().insert_inst(entry, ret);
        (load, ret)
    };

    let _ = IPSCCP.run(&mut program);
    let data = program.func_data(main);
    let kind = data.inst_data(load).kind();
    // 洞 3 的病根是 load 读到 callee 的 store 值（1）——位置感知 read 后
    // 只可能是"保持 load"或"折叠初始 0"。
    assert!(
        matches!(kind, InstKind::Load(..))
            || matches!(kind, InstKind::Integer(integer) if integer.value() == 0),
        "load after a later call must not fold to the callee's value, got {kind:?}"
    );
}

/// f32 零初始化 load 不折叠：`Lattice::Constant` 只承载 i32 常量，f32 load
/// 折叠成 int 0 会让 f32 运算拿到整数寄存器操作数——后端编码出
/// `fadd s16, s4, x6` 这类非法指令（fuzzer 差分 case_0001，O2 汇编失败）。
/// float load 保持运行时读取（保守）。
#[test]
fn zero_initialized_f32_global_load_stays_load() {
    let mut program = Program::new();
    let init = program.new_value().zero_init(Type::get_f32());
    let global = program.new_value().global_alloc(init);
    let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
    let (load, _ret) = {
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(main),
        };
        let entry = data.add_entry_block();
        let zero = data.new_local_value().integer(0);
        let gep = data.new_local_value().get_elem_ptr(global, vec![zero]);
        data.layout_mut().insert_inst(entry, gep);
        let load = data.new_local_value().load(gep);
        data.layout_mut().insert_inst(entry, load);
        let ret = data.new_local_value().ret(Some(load));
        data.layout_mut().insert_inst(entry, ret);
        (load, ret)
    };

    let _ = IPSCCP.run(&mut program);
    let data = program.func_data(main);
    assert!(matches!(data.inst_data(load).kind(), InstKind::Load(..)));
}

/// 动态索引 store 必须失效整个 root（fuzzer baseline case_0150）：局部
/// 数组 memzero 后，循环内动态索引 store `a[i] = ...` 可能写 a[3]——
/// 静态 load a[3] 不得折叠零区间 0（否则第二轮循环读到错误的 0 而非
/// 上一轮写的 -12）。
#[test]
fn dynamic_index_store_invalidates_static_load() {
    let mut program = Program::new();
    // getint 是声明函数（无定义）：call 返回 Bottom，索引保持动态。
    let getint = program.new_function(Type::get_i32(), "getint".into(), vec![]);
    let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
    let (load, _ret) = {
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(main),
        };
        let entry = data.add_entry_block();
        let alloc = data
            .new_local_value()
            .alloc(Type::get_array(Type::get_i32(), 4));
        data.layout_mut().insert_inst(entry, alloc);
        let clear = data.new_local_value().mem_zero(alloc, 16);
        data.layout_mut().insert_inst(entry, clear);
        let idx = data.new_local_value().call(getint, vec![]);
        data.layout_mut().insert_inst(entry, idx);
        let gep_dyn = data.new_local_value().get_elem_ptr(alloc, vec![idx]);
        data.layout_mut().insert_inst(entry, gep_dyn);
        let five = data.new_local_value().integer(5);
        let store = data.new_local_value().store(five, gep_dyn);
        data.layout_mut().insert_inst(entry, store);
        let zero = data.new_local_value().integer(0);
        let gep3 = data.new_local_value().get_elem_ptr(alloc, vec![zero, zero]);
        data.layout_mut().insert_inst(entry, gep3);
        let load = data.new_local_value().load(gep3);
        data.layout_mut().insert_inst(entry, load);
        let ret = data.new_local_value().ret(Some(load));
        data.layout_mut().insert_inst(entry, ret);
        (load, ret)
    };

    let _ = IPSCCP.run(&mut program);
    let data = program.func_data(main);
    // 动态 store a[getint()] 可能写 a[0]：load 保持运行时读取。
    assert!(matches!(data.inst_data(load).kind(), InstKind::Load(..)));
}
