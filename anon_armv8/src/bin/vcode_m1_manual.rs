use anon_armv8::compile_program_vcode;
use raana_ir::ir::{BinaryOp, Program, Type};
use taki_mir::prelude::{Arena, BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder};

fn main() {
    let mut program = Program::new();
    let addf = program.new_function(
        Type::get_f32(),
        "addf".into(),
        vec![Type::get_f32(), Type::get_f32()],
    );
    let addi = program.new_function(
        Type::get_i32(),
        "addi".into(),
        vec![Type::get_i32(), Type::get_i32()],
    );
    let float_main = program.new_function(
        Type::get_f32(),
        "float_main".into(),
        vec![Type::get_f32(), Type::get_f32()],
    );
    let block_main = program.new_function(Type::get_i32(), "block_main".into(), vec![]);
    let int_main = program.new_function(Type::get_i32(), "int_main".into(), vec![]);

    {
        let data = program.func_data_mut(addf);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let params = data.params().to_vec();
        let sum = data
            .new_local_inst()
            .binary(BinaryOp::Add, params[0], params[1]);
        let ret = data.new_local_inst().ret(Some(sum));
        for inst in [sum, ret] {
            data.layout_mut().insert_inst(entry, inst);
        }
    }

    {
        let data = program.func_data_mut(addi);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let params = data.params().to_vec();
        let sum = data
            .new_local_inst()
            .binary(BinaryOp::Add, params[0], params[1]);
        let ret = data.new_local_inst().ret(Some(sum));
        for inst in [sum, ret] {
            data.layout_mut().insert_inst(entry, inst);
        }
    }

    {
        let data = program.func_data_mut(float_main);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let params = data.params().to_vec();
        let values = (0..32)
            .map(|_| {
                data.new_local_inst()
                    .binary(BinaryOp::Add, params[0], params[1])
            })
            .collect::<Vec<_>>();
        let call =
            data.new_local_inst()
                .call_with_type(addf, vec![values[0], values[1]], Type::get_f32());
        let mut sum = call;
        let mut insts = values.clone();
        insts.push(call);
        for value in values.iter().skip(2) {
            sum = data.new_local_inst().binary(BinaryOp::Add, sum, *value);
            insts.push(sum);
        }
        let ret = data.new_local_inst().ret(Some(sum));
        insts.push(ret);
        for inst in insts {
            data.layout_mut().insert_inst(entry, inst);
        }
    }

    {
        let data = program.func_data_mut(int_main);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let values = (1..=32)
            .map(|value| data.new_local_inst().integer(value))
            .collect::<Vec<_>>();
        let call =
            data.new_local_inst()
                .call_with_type(addi, vec![values[0], values[1]], Type::get_i32());
        let mut sum = call;
        let mut insts = vec![call];
        for value in values.iter().skip(2) {
            sum = data.new_local_inst().binary(BinaryOp::Add, sum, *value);
            insts.push(sum);
        }
        let ret = data.new_local_inst().ret(Some(sum));
        insts.push(ret);
        for inst in insts {
            data.layout_mut().insert_inst(entry, inst);
        }
    }

    {
        let data = program.func_data_mut(block_main);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        let join = data
            .new_basic_block()
            .basic_block("join".into(), vec![Type::get_i32()]);
        let param = data.bb_data(join).params()[0];
        data.layout_mut().push_bb_back(entry);
        data.layout_mut().push_bb_back(join);
        let value = data.new_local_inst().integer(42);
        let jump = data.new_local_inst().jump(join, vec![value]);
        data.layout_mut().insert_inst(entry, jump);
        let ret = data.new_local_inst().ret(Some(param));
        data.layout_mut().insert_inst(join, ret);
    }

    print!(
        "{}",
        compile_program_vcode(&program).expect("M1 manual VCode program must lower")
    );
}
