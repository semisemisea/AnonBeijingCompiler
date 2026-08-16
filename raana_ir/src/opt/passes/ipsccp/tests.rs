
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
        let gep = data.new_local_value().get_elem_ptr(alloc, vec![zero]);
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
