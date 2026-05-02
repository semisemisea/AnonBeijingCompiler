use std::collections::HashMap;
use std::fmt::Write;

use crate::ir::{
    Aggregate, Function, FunctionData, InstKind, Program, Type,
    arena::Arena,
    inst_kind::{Binary, Branch, Call, Cast, GetElemPtr, GetPtr, Jump, Load, Return, Store},
    instruction::{Inst, InstData},
    layout::BasicBlockLayout,
};

pub struct Writer<'a> {
    buffer: String,
    program: ProgramWrapper<'a>,
    symbol: HashMap<Inst, String>,
    counter: u32,
}
struct ProgramWrapper<'a> {
    pub program: &'a Program,
    pub curr_func: Option<Function>,
}

impl<'a> std::ops::Deref for ProgramWrapper<'a> {
    type Target = &'a Program;
    fn deref(&self) -> &Self::Target {
        &self.program
    }
}

impl Arena for ProgramWrapper<'_> {
    fn local(&self) -> &crate::ir::arena::LocalArena {
        self.program
            .func_data(self.curr_func.unwrap())
            .local_arena()
    }
    fn global(&self) -> &crate::ir::arena::GlobalArena {
        self.program.global_arena()
    }

    fn local_mut(&mut self) -> &mut crate::ir::arena::LocalArena {
        unimplemented!()
    }
    fn global_mut(&mut self) -> &mut crate::ir::arena::GlobalArena {
        unimplemented!()
    }
}

macro_rules! put_name {
    ($self:expr, $inst:expr) => {
        'b: {
            let data = $self.program.inst_data($inst);
            match data.kind() {
                InstKind::Integer(..) | InstKind::Float(..) | InstKind::ZeroInit => break 'b,
                _ => {}
            };
            if let Some(name) = data.name() {
                $self.symbol.insert($inst, format!("%{}", name));
            } else {
                $self.symbol.insert($inst, format!("%{}", $self.counter));
                $self.counter += 1;
            }
        }
    };
}

macro_rules! get_name {
    ($self: expr, $inst: expr) => {{
        let data = $self.program.inst_data($inst);
        match data.kind() {
            InstKind::Integer(i) => i.value().to_string(),
            InstKind::Float(i) => i.value().to_string(),
            InstKind::ZeroInit => "zeroinit".to_string(),
            InstKind::Aggregate(agg) => $self.visit_aggregate(agg),
            _ => $self.symbol.get(&$inst).unwrap().clone(),
        }
    }};
}

impl Writer<'_> {
    pub fn new(program: &Program) -> Writer {
        Writer {
            buffer: String::new(),
            program: ProgramWrapper {
                program,
                curr_func: None,
            },
            symbol: HashMap::new(),
            counter: 0,
        }
    }

    pub fn write(&mut self) -> std::fmt::Result {
        for &global_inst in self.program.global_inst_layout() {
            self.visit_global_inst(global_inst)?;
        }

        for &func in self.program.function_layout() {
            self.program.curr_func.replace(func);
            let data = self.program.program.func_data(func);
            self.visit_func(data)?;
        }
        Ok(())
    }

    fn visit_global_inst(&mut self, inst: Inst) -> std::fmt::Result {
        put_name!(self, inst);
        let data = self.program.inst_data(inst);
        match data.kind() {
            InstKind::GlobalAlloc(global_alloc) => write!(
                self.buffer,
                "global {} = alloc <init = {}, type = {}, size = {}>",
                get_name!(self, inst),
                get_name!(self, global_alloc.init()),
                data.ty(),
                data.ty().size()
            )?,
            _ => panic!(),
        };
        writeln!(self.buffer)
    }

    fn visit_aggregate(&self, agg: &Aggregate) -> String {
        let mut s = String::new();
        if let Some((&last, rest)) = agg.value().split_last() {
            s.push('[');
            for &inst in rest {
                s.push_str(&get_name!(self, inst));
                s.push_str(", ");
            }
            s.push_str(&get_name!(self, last));
            s.push(']');
        }
        s
    }

    fn visit_func(&mut self, data: &FunctionData) -> std::fmt::Result {
        data.params()
            .to_vec()
            .iter()
            .for_each(|&inst| put_name!(self, inst));
        let is_decl = data.layout().is_decl();
        if is_decl {
            write!(self.buffer, "declare")?;
        } else {
            write!(self.buffer, "define")?;
        }
        write!(
            self.buffer,
            " func <name = {}, ret_ty = {}",
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
                    self.program.inst_data(inst).ty()
                )?;
            }
            write!(
                self.buffer,
                "{}: {})",
                get_name!(self, last),
                self.program.inst_data(last).ty()
            )?;
        }
        if is_decl {
            writeln!(self.buffer, ">")?;
            return writeln!(self.buffer);
        }
        writeln!(self.buffer, ">: {{")?;
        for bb_layout in data.layout().basicblocks() {
            self.visit_bb(bb_layout)?;
        }
        writeln!(self.buffer, "}}")
    }

    fn visit_bb(&mut self, layout: &BasicBlockLayout) -> std::fmt::Result {
        let data = self.program.bb_data(layout.bb());
        for &inst in self.program.bb_data(layout.bb()).params().iter() {
            put_name!(self, inst)
        }
        write!(self.buffer, "{}", data.name())?;
        if let Some((&last, rest)) = data.params().split_last() {
            write!(self.buffer, "(")?;
            for &inst in rest {
                write!(
                    self.buffer,
                    "{}: {}, ",
                    get_name!(self, inst),
                    self.program.inst_data(inst).ty()
                )?;
            }
            write!(
                self.buffer,
                "{}: {}",
                get_name!(self, last),
                self.program.inst_data(last).ty()
            )?;
            write!(self.buffer, ")")?;
        }
        writeln!(self.buffer, ":")?;
        for &inst in layout.insts() {
            write!(self.buffer, "    ")?;
            let data = self
                .program
                .program
                .func_data(self.program.curr_func.unwrap())
                .inst_data(inst);
            self.visit_local_inst(inst, data)?
        }
        Ok(())
    }

    fn visit_local_inst(&mut self, inst: Inst, data: &InstData) -> std::fmt::Result {
        put_name!(self, inst);
        if !data.ty().is_unit() {
            write!(self.buffer, "{} = ", get_name!(self, inst))?;
        }

        match data.kind() {
            InstKind::Alloc => self.visit_alloc(data.ty()),
            InstKind::Binary(binary) => self.visit_binary(binary, data.ty()),
            InstKind::Branch(branch) => self.visit_branch(branch),
            InstKind::Cast(cast) => self.visit_cast(cast, data.ty()),
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
        write!(self.buffer, "alloc <type = {}, size = {}>", ty, ty.size())
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

    fn visit_cast(&mut self, cast: &Cast, ty: &Type) -> std::fmt::Result {
        write!(
            self.buffer,
            "cast {} <type = {}, size = {}>",
            get_name!(self, cast.src()),
            ty,
            ty.size()
        )
    }

    fn visit_branch(&mut self, branch: &Branch) -> std::fmt::Result {
        write!(self.buffer, "br {}, ", get_name!(self, branch.cond()))?;
        write!(
            self.buffer,
            "{}",
            self.program.bb_data(branch.t_target()).name()
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
            self.program.bb_data(branch.f_target()).name()
        )?;
        if let Some((&last, rest)) = branch.f_args().split_last() {
            write!(self.buffer, "(")?;
            for &inst in rest {
                write!(self.buffer, "{}, ", get_name!(self, inst))?;
            }
            write!(self.buffer, "{}", get_name!(self, last))?;
            write!(self.buffer, ")")?;
        }
        Ok(())
    }

    fn visit_call(&mut self, call: &Call) -> std::fmt::Result {
        let callee_data = self.program.func_data(call.callee());
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
            .program
            .inst_data(get_elem_ptr.base())
            .ty()
            .derefernce()
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
        let base_ty = self.program.inst_data(get_ptr.base()).ty().derefernce();
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
            self.program.bb_data(jump.target()).name()
        )?;
        if let Some((&last, rest)) = jump.args().split_last() {
            write!(self.buffer, "(")?;
            for &inst in rest {
                write!(self.buffer, "{}, ", get_name!(self, inst))?;
            }
            write!(self.buffer, "{}", get_name!(self, last))?;
            write!(self.buffer, ")")?;
        }
        Ok(())
    }

    fn visit_load(&mut self, load: &Load) -> std::fmt::Result {
        let ty = self.program.inst_data(load.src()).ty().clone();
        write!(
            self.buffer,
            "load {} <type = {}, size = {}>",
            get_name!(self, load.src()),
            ty.derefernce(),
            ty.derefernce().size()
        )
    }

    fn visit_return(&mut self, ret: &Return) -> std::fmt::Result {
        write!(self.buffer, "ret")?;
        if let Some(inst) = ret.value() {
            write!(self.buffer, " {}", get_name!(self, inst))?;
        }
        Ok(())
    }

    fn visit_store(&mut self, store: &Store) -> std::fmt::Result {
        write!(
            self.buffer,
            "store {}, {}",
            get_name!(self, store.src()),
            get_name!(self, store.dest()),
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
global %0 = alloc <init = 3, type = *i32, size = 8>
global %1 = alloc <init = 5, type = *i32, size = 8>
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
        let add = fd.new_local_inst().binary(BinaryOp::Add, o, t);
        fd.layout_mut().insert_inst(b, add);
        let ret = fd.new_local_inst().ret(None);
        fd.layout_mut().insert_inst(b, ret);
        is_same(
            &p,
            "\
define func <name = main, ret_ty = ()>: {
entry:
    %0 = add 1, 2 <type = i32, size = 4>
    ret
}
",
        );
    }
}
