use std::collections::HashMap;
use std::fmt::Write;

use crate::ir::{
    InstKind, Program, Type,
    arena::Arena,
    function::FunctionData,
    inst_kind::{Binary, Branch, Call, GetElemPtr, GetPtr, Jump, Load, Return, Store},
    instruction::{Inst, InstData},
    layout::BasicBlockLayout,
};

pub struct Writer<'a> {
    buffer: String,
    program: &'a Program,
    symbol: HashMap<Inst, String>,
    counter: u32,
    arena: Arena<'a>,
}
macro_rules! get_name {
    ($self: expr, $inst: expr) => {{
        let data = $self.arena.inst_data($inst);
        match data.kind() {
            InstKind::Integer(i) => i.value().to_string(),
            InstKind::Float(i) => i.value().to_string(),
            _ => $self
                .arena
                .inst_data($inst)
                .name()
                .unwrap_or_else(|| $self.symbol.get(&$inst).unwrap())
                // just work for now
                .to_string(),
        }
    }};
}

impl Writer<'_> {
    pub fn new(program: &Program) -> Writer {
        Writer {
            buffer: String::new(),
            program,
            symbol: HashMap::new(),
            counter: 0,
            arena: Arena::new(None, Some(program.global_arena())),
        }
    }

    fn put_name(&mut self, inst: Inst) {
        if self.arena.inst_data(inst).name().is_none() {
            self.symbol.insert(inst, self.counter.to_string());
            self.counter += 1;
        }
    }

    pub fn write(&mut self) -> std::fmt::Result {
        for &global_inst in self.program.global_inst_layout() {
            let data = self.program.arena().inst_data(global_inst);
            self.visit_global_inst(global_inst, data)?;
        }

        for &func in self.program.function_layout() {
            // let func_data = self.program.func_data(func);
            let func_data = self.arena.func_data(func);
            self.arena.set_local(Some(func_data.local_arena()));
            self.visit_func(func_data)?;
        }
        Ok(())
    }

    fn visit_global_inst(&mut self, inst: Inst, data: &InstData) -> std::fmt::Result {
        self.put_name(inst);
        match data.kind() {
            InstKind::GlobalAlloc(global_alloc) => write!(
                self.buffer,
                "global %{} = alloc <init = {}, type = {}, size = {}>",
                get_name!(self, inst),
                get_name!(self, global_alloc.init()),
                data.ty(),
                data.ty().size()
            )?,
            _ => panic!(),
        };
        writeln!(self.buffer)
    }

    fn visit_func(&mut self, data: &FunctionData) -> std::fmt::Result {
        write!(
            self.buffer,
            "define func <name = {}, ret_ty = {}",
            data.name(),
            data.ret_ty()
        )?;
        if let Some((&last, rest)) = data.params().split_last() {
            write!(self.buffer, ", params = (")?;
            for &inst in rest {
                write!(
                    self.buffer,
                    "{}: {}, ",
                    get_name!(self, inst),
                    self.arena.inst_data(inst).ty()
                )?;
            }
            write!(
                self.buffer,
                "{}: {})",
                get_name!(self, last),
                self.arena.inst_data(last).ty()
            )?;
        }
        writeln!(self.buffer, ">: {{")?;
        for bb_layout in data.layout().bbs() {
            self.visit_bb(bb_layout)?;
        }
        writeln!(self.buffer, "}}")
    }

    fn visit_bb(&mut self, layout: &BasicBlockLayout) -> std::fmt::Result {
        let data = self.arena.bb_data(layout.bb());
        write!(self.buffer, "{}", data.name())?;
        if let Some((&last, rest)) = data.params().split_last() {
            write!(self.buffer, "(")?;
            for &inst in rest {
                write!(
                    self.buffer,
                    "{}: {}, ",
                    get_name!(self, inst),
                    self.arena.inst_data(inst).ty()
                )?;
            }
            write!(
                self.buffer,
                "{}: {}",
                get_name!(self, last),
                self.arena.inst_data(last).ty()
            )?;
            write!(self.buffer, ")")?;
        }
        writeln!(self.buffer, ":")?;
        for &inst in layout.insts() {
            write!(self.buffer, "    ")?;
            self.visit_local_inst(inst, self.arena.inst_data(inst))?
        }
        Ok(())
    }

    fn visit_local_inst(&mut self, inst: Inst, data: &InstData) -> std::fmt::Result {
        self.put_name(inst);
        if !data.ty().is_unit() {
            write!(self.buffer, "%{} = ", get_name!(self, inst))?;
        }

        match data.kind() {
            InstKind::Alloc => self.visit_alloc(data.ty()),
            InstKind::Binary(binary) => self.visit_binary(binary, data.ty()),
            InstKind::Branch(branch) => self.visit_branch(branch),
            InstKind::Call(call) => self.visit_call(call),
            InstKind::GetElemPtr(get_elem_ptr) => self.visit_get_elem_ptr(get_elem_ptr),
            InstKind::GetPtr(get_ptr) => self.visit_get_ptr(get_ptr),
            InstKind::Jump(jump) => self.visit_jump(jump),
            InstKind::Load(load) => self.visit_load(load),
            InstKind::Return(ret) => self.visit_return(ret),
            InstKind::Store(store) => self.visit_store(store),
            _ => panic!("invalid local instruction"),
        }?;
        writeln!(self.buffer)
    }

    pub fn finish(self) -> String {
        self.buffer
    }

    fn visit_alloc(&mut self, ty: &Type) -> std::fmt::Result {
        let base = ty.derefernce();
        write!(
            self.buffer,
            "alloc <type = {}, size = {}>",
            base,
            base.size()
        )
    }

    fn visit_binary(&mut self, binary: &Binary, ty: &Type) -> std::fmt::Result {
        write!(
            self.buffer,
            "{} {}, {} <type = {}, size = {}>",
            binary.op(),
            get_name!(self, binary.lhs()),
            get_name!(self, binary.rhs()),
            ty,
            ty.size()
        )
    }

    fn visit_branch(&mut self, branch: &Branch) -> std::fmt::Result {
        write!(self.buffer, "br {}, ", get_name!(self, branch.cond()))?;
        write!(
            self.buffer,
            "@{}",
            self.arena.bb_data(branch.t_target()).name()
        )?;
        if let Some((&last, rest)) = branch.t_args().split_last() {
            write!(self.buffer, "(")?;
            for &inst in rest {
                write!(self.buffer, "{}, ", get_name!(self, inst))?;
            }
            write!(self.buffer, "{}", get_name!(self, last))?;
            write!(self.buffer, ")")?;
        }
        write!(self.buffer, ", ")?;
        write!(
            self.buffer,
            "{}",
            self.arena.bb_data(branch.f_target()).name()
        )?;
        if let Some((&last, rest)) = branch.f_args().split_last() {
            write!(self.buffer, "(")?;
            for &inst in rest {
                write!(self.buffer, "{}, ", get_name!(self, inst))?;
            }
            write!(self.buffer, "{}", get_name!(self, last))?;
            write!(self.buffer, ")")?;
        }
        write!(
            self.buffer,
            " <type = {}, size = {}>",
            Type::get_unit(),
            Type::get_unit().size()
        )
    }

    fn visit_call(&mut self, call: &Call) -> std::fmt::Result {
        let callee_data = self.arena.func_data(call.callee());
        write!(
            self.buffer,
            "call <name = {}, type = {}, size = {}>",
            callee_data.name(),
            callee_data.ret_ty(),
            callee_data.ret_ty().size()
        )
    }

    fn visit_get_elem_ptr(&mut self, get_elem_ptr: &GetElemPtr) -> std::fmt::Result {
        let base_ty = self
            .arena
            .inst_data(get_elem_ptr.base())
            .ty()
            .get_array_elem_ty();
        write!(
            self.buffer,
            "getelemptr {}, {} <type = {}, size = {}>",
            get_name!(self, get_elem_ptr.base()),
            get_name!(self, get_elem_ptr.offset()),
            base_ty,
            base_ty.size()
        )
    }

    fn visit_get_ptr(&mut self, get_ptr: &GetPtr) -> std::fmt::Result {
        let base_ty = self.arena.inst_data(get_ptr.base()).ty().derefernce();
        write!(
            self.buffer,
            "getptr {}, {} <type = {}, size = {}>",
            get_name!(self, get_ptr.base()),
            get_name!(self, get_ptr.offset()),
            base_ty,
            base_ty.size()
        )
    }

    fn visit_jump(&mut self, jump: &Jump) -> std::fmt::Result {
        write!(
            self.buffer,
            "jump {}",
            self.arena.bb_data(jump.target()).name()
        )?;
        if let Some((&last, rest)) = jump.args().split_last() {
            write!(self.buffer, "(")?;
            for &inst in rest {
                write!(self.buffer, "{}, ", get_name!(self, inst))?;
            }
            write!(self.buffer, "{}", get_name!(self, last))?;
            write!(self.buffer, ")")?;
        }
        write!(
            self.buffer,
            " <type = {}, size = {}>",
            Type::get_unit(),
            Type::get_unit().size()
        )
    }

    fn visit_load(&mut self, load: &Load) -> std::fmt::Result {
        let ty = self.arena.inst_data(load.src()).ty();
        write!(
            self.buffer,
            "load {} <type = {}, size = {}>",
            get_name!(self, load.src()),
            ty,
            ty.size()
        )
    }

    fn visit_return(&mut self, ret: &Return) -> std::fmt::Result {
        write!(self.buffer, "ret")?;
        if let Some(inst) = ret.value() {
            write!(self.buffer, " {}", get_name!(self, inst))?;
        }
        write!(
            self.buffer,
            " <type = {}, size = {}>",
            Type::get_unit(),
            Type::get_unit().size()
        )
    }

    fn visit_store(&mut self, store: &Store) -> std::fmt::Result {
        write!(
            self.buffer,
            "store {}, {} <type = {}, size = {}>",
            get_name!(self, store.src()),
            get_name!(self, store.dest()),
            Type::get_unit(),
            Type::get_unit().size()
        )
    }
}

#[cfg(test)]
mod test {
    use crate::{
        fmt::writer::Writer,
        ir::{
            BinaryOp, Program, Type,
            builder::{BasicBlockBuilder, GlobalInstBuilder, LocalInstBuilder, ScalarInstBuilder},
        },
    };

    fn is_same(p: &Program, s: &str) {
        let mut writer = Writer::new(p);
        writer.write().unwrap();
        assert_eq!(std::str::from_utf8(writer.finish().as_ref()).unwrap(), s)
    }

    #[test]
    fn global() {
        let mut p = Program::new();
        let t = p.new_value().integer(3);
        p.new_value().global_alloc(t);
        let f = p.new_value().integer(5);
        p.new_value().global_alloc(f);
        is_same(
            &p,
            "\
global %0 = alloc <init = 3, type = i32, size = 4>
global %1 = alloc <init = 5, type = i32, size = 4>
",
        );
    }

    #[test]
    fn func() {
        let mut p = Program::new();
        let f = p.new_function(Type::get_unit(), "main".to_string(), vec![]);
        let fd = p.func_data_mut(f);
        let b = fd
            .new_basic_block()
            .basic_block("entry".to_string(), vec![]);
        fd.layout_mut().push_bb_back(b);
        let o = fd.new_local_inst().integer(1);
        let t = fd.new_local_inst().integer(2);
        let add = fd.new_local_inst().binary(o, t, BinaryOp::Add);
        fd.layout_mut()
            .bbs_mut()
            .get_mut_last()
            .unwrap()
            .insts_mut()
            .insert_last(add);
        let ret = fd.new_local_inst().ret(None);
        fd.layout_mut()
            .bbs_mut()
            .get_mut_last()
            .unwrap()
            .insts_mut()
            .insert_last(ret);
        is_same(
            &p,
            "\
define func <name = main, ret_ty = ()>: {
entry:
    %0 = add 1, 2 <type = i32, size = 4>
    ret <type = (), size = 0>
}
",
        );
    }
}
