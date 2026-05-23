use std::collections::HashMap;
use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::fmt::Write;

use crate::ir::BasicBlock;
use crate::ir::arena::Arena;
use crate::ir::{
    Function, InstKind, Program, Type, TypeKind,
    inst_kind::{Binary, BinaryOp, Call, Cast, GetElemPtr},
    instruction::Inst,
};

pub struct LlvmWriter<'a> {
    buffer: String,
    arena: ProgramWrapper<'a>,
    local_names: HashMap<Inst, String>,
    global_names: HashMap<Inst, String>,
    bb_labels: HashMap<BasicBlock, String>,
    phi_incoming: HashMap<BasicBlock, Vec<(BasicBlock, Vec<Inst>)>>,
    name_counter: usize,
    bb_counter: usize,
}

struct ProgramWrapper<'a> {
    program: &'a Program,
    curr_func: Option<Function>,
}

impl std::ops::Deref for ProgramWrapper<'_> {
    type Target = Program;
    fn deref(&self) -> &Self::Target {
        self.program
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

/// Mutable: assign a name if the inst doesn't have one yet.
macro_rules! put_name {
    ($self:expr, $inst:expr) => {
        'b: {
            let data = $self.arena.inst_data($inst);
            match data.kind() {
                InstKind::Integer(..)
                | InstKind::Float(..)
                | InstKind::ZeroInit
                | InstKind::Aggregate(..)
                | InstKind::Undef => break 'b,
                _ => {}
            };
            if $inst.is_global() {
                match $self.global_names.entry($inst) {
                    Occupied(..) => {}
                    Vacant(e) => {
                        e.insert(format!("@g{}", $self.name_counter));
                        $self.name_counter += 1;
                    }
                };
            } else {
                match $self.local_names.entry($inst) {
                    Occupied(..) => {}
                    Vacant(e) => {
                        e.insert(format!("%{}", $self.name_counter));
                        $self.name_counter += 1;
                    }
                };
            }
        }
    };
}

/// Immutable: get the name. Constants render inline.
macro_rules! get_name {
    ($self:expr, $inst:expr) => {{
        let data = $self.arena.inst_data($inst);
        match data.kind() {
            InstKind::Integer(i) => i.value().to_string(),
            InstKind::Float(f) => {
                let v = f.value();
                if v == 0.0 {
                    "0.0".into()
                } else {
                    format!("{:.6}", v).parse::<f64>().unwrap().to_string()
                }
            }
            InstKind::ZeroInit => {
                let ty = data.ty();
                if ty.is_f32() {
                    "0.0".into()
                } else if ty.is_i32() {
                    "0".into()
                } else if ty.is_pointer() {
                    "null".into()
                } else {
                    "zeroinitializer".into()
                }
            }
            InstKind::Undef => "undef".into(),
            InstKind::Aggregate(agg) => $self.visit_aggregate(agg),
            _ => {
                if $inst.is_global() {
                    $self
                        .global_names
                        .get(&$inst)
                        .cloned()
                        .unwrap_or_else(|| panic!("name not assigned for global {:?}", $inst))
                } else {
                    $self
                        .local_names
                        .get(&$inst)
                        .cloned()
                        .unwrap_or_else(|| panic!("name not assigned for local {:?}", $inst))
                }
            }
        }
    }};
}

macro_rules! bb_label {
    ($self:expr, $bb:expr) => {{
        match $self.bb_labels.entry($bb) {
            Occupied(e) => e.get().clone(),
            Vacant(e) => {
                let label = format!("%L{}", $self.bb_counter);
                $self.bb_counter += 1;
                e.insert(label.clone());
                label
            }
        }
    }};
}

impl<'a> LlvmWriter<'a> {
    pub fn new(program: &'a Program) -> Self {
        LlvmWriter {
            buffer: String::new(),
            arena: ProgramWrapper {
                program,
                curr_func: None,
            },
            local_names: HashMap::new(),
            global_names: HashMap::new(),
            bb_labels: HashMap::new(),
            phi_incoming: HashMap::new(),
            name_counter: 0,
            bb_counter: 0,
        }
    }

    pub fn write(&mut self) -> std::fmt::Result {
        writeln!(self.buffer, "; Generated by AnonBeijingCompiler")?;
        writeln!(self.buffer)?;

        // Pre-collect to avoid borrow conflicts during iteration
        let global_insts: Vec<Inst> = self.arena.global_inst_layout().to_vec();
        for &global_inst in &global_insts {
            put_name!(self, global_inst);
            self.visit_global(global_inst)?;
        }
        if !global_insts.is_empty() {
            writeln!(self.buffer)?;
        }

        let funcs: Vec<Function> = self.arena.function_layout().to_vec();
        for &func in &funcs {
            self.arena.curr_func = Some(func);
            let is_decl = self.arena.func_data(func).layout().is_decl();
            if is_decl {
                self.visit_declare(func)?;
            } else {
                self.visit_define(func)?;
            }
            writeln!(self.buffer)?;
        }
        Ok(())
    }

    pub fn finish(self) -> String {
        self.buffer
    }

    // ─── Type ───

    fn type_to_llvm(&self, ty: &Type) -> String {
        match ty.kind() {
            TypeKind::Unit => "void".into(),
            TypeKind::Int32 => "i32".into(),
            TypeKind::Float32 => "float".into(),
            TypeKind::Pointer(_) => "ptr".into(),
            TypeKind::Array(base, len) => format!("[{} x {}]", len, self.type_to_llvm(base)),
            TypeKind::Function(params, ret) => {
                let ps: Vec<String> = params.iter().map(|p| self.type_to_llvm(p)).collect();
                format!("{} ({})", self.type_to_llvm(ret), ps.join(", "))
            }
            TypeKind::String => "ptr".into(),
            TypeKind::ArgList => "...".into(),
        }
    }

    // ─── Aggregate ───

    fn visit_aggregate(&self, agg: &crate::ir::Aggregate) -> String {
        let elem_ty = agg
            .value()
            .first()
            .map(|&v| self.arena.inst_data(v).ty().clone())
            .unwrap_or_else(|| Type::get_i32());
        let mut s = String::from("[");
        for (i, &v) in agg.value().iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(&self.type_to_llvm(&elem_ty));
            s.push(' ');
            s.push_str(&get_name!(self, v));
        }
        s.push(']');
        s
    }

    // ─── Global ───

    fn visit_global(&mut self, inst: Inst) -> std::fmt::Result {
        let data = self.arena.inst_data(inst);
        let ga = match data.kind() {
            InstKind::GlobalAlloc(ga) => ga,
            _ => panic!("unexpected global inst kind"),
        };
        // put_name already called in write() before this
        let name = get_name!(self, inst);
        let value_ty = data.ty().derefernce();
        let init_str = get_name!(self, ga.init());
        writeln!(
            self.buffer,
            "{} = global {} {}",
            name,
            self.type_to_llvm(&value_ty),
            init_str
        )
    }

    // ─── Declare ───

    fn visit_declare(&mut self, func: Function) -> std::fmt::Result {
        let data = self.arena.func_data(func);
        let params: Vec<String> = data
            .params()
            .iter()
            .map(|&p| self.type_to_llvm(self.arena.inst_data(p).ty()))
            .collect();
        write!(
            self.buffer,
            "declare {} @{}({})",
            self.type_to_llvm(data.ret_ty()),
            data.name(),
            params.join(", ")
        )
    }

    // ─── Define ───

    fn visit_define(&mut self, func: Function) -> std::fmt::Result {
        // Phase 1: grab params list (drop data ref before mutating)
        let params: Vec<Inst> = {
            let data = self.arena.func_data(func);
            data.params().to_vec()
        };
        for &param in &params {
            put_name!(self, param);
        }

        // Phase 2: collect phi incoming values and pre-assign instruction/param names
        self.collect_phi_incoming(func);

        // Phase 3: pre-assign block labels and inst names
        let all_bbs_and_insts: Vec<(BasicBlock, Vec<Inst>, Vec<Inst>)> = {
            let data = self.arena.func_data(func);
            data.layout()
                .basicblocks()
                .iter()
                .map(|layout| {
                    let bb = layout.bb();
                    let params = self.arena.bb_data(bb).params().to_vec();
                    for &param in &params {
                        put_name!(self, param);
                    }
                    let insts = layout.insts().iter().copied().collect::<Vec<_>>();
                    for &inst in &insts {
                        put_name!(self, inst);
                        for used in self.arena.inst_data(inst).inst_usage() {
                            put_name!(self, used);
                        }
                    }
                    bb_label!(self, bb);
                    (bb, params, insts)
                })
                .collect()
        };

        // Phase 4: emit function header
        let (ret_ty, name) = {
            let data = self.arena.func_data(func);
            (data.ret_ty().clone(), data.name().to_string())
        };
        let params_str: Vec<String> = params
            .iter()
            .map(|&p| {
                format!(
                    "{} {}",
                    self.type_to_llvm(self.arena.inst_data(p).ty()),
                    get_name!(self, p)
                )
            })
            .collect();
        writeln!(
            self.buffer,
            "define {} @{}({}) {{",
            self.type_to_llvm(&ret_ty),
            name,
            params_str.join(", ")
        )?;

        // Phase 5: emit blocks
        for (bb, _params, _insts) in &all_bbs_and_insts {
            self.visit_block(*bb)?;
        }

        writeln!(self.buffer, "}}")
    }

    fn collect_phi_incoming(&mut self, func: Function) {
        let data = self.arena.func_data(func);
        self.phi_incoming.clear();
        for layout in data.layout().basicblocks() {
            let bb = layout.bb();
            let Some(&last_inst) = layout.insts().get_last() else {
                continue;
            };
            let inst_data = self.arena.inst_data(last_inst);
            match inst_data.kind() {
                InstKind::Jump(jump) => {
                    self.phi_incoming
                        .entry(jump.target())
                        .or_default()
                        .push((bb, jump.args().to_vec()));
                }
                InstKind::Branch(branch) => {
                    self.phi_incoming
                        .entry(branch.t_target())
                        .or_default()
                        .push((bb, branch.t_args().to_vec()));
                    self.phi_incoming
                        .entry(branch.f_target())
                        .or_default()
                        .push((bb, branch.f_args().to_vec()));
                }
                _ => {}
            }
        }
    }

    // ─── Block ───

    fn visit_block(&mut self, bb: BasicBlock) -> std::fmt::Result {
        let label = bb_label!(self, bb);
        let label_def = &label[1..]; // strip '%'

        let params = self.arena.bb_data(bb).params().clone();
        let incomings = self.phi_incoming.get(&bb);

        writeln!(self.buffer, "{}:", label_def)?;

        if !params.is_empty() {
            let incoming_list = incomings.map(|v| v.as_slice()).unwrap_or(&[]);
            for (i, &param_inst) in params.iter().enumerate() {
                let param_ty = self.type_to_llvm(self.arena.inst_data(param_inst).ty());
                let phi_name = get_name!(self, param_inst);
                let mut parts = String::new();
                for (j, (pred_bb, args)) in incoming_list.iter().enumerate() {
                    if j > 0 {
                        parts.push_str(", ");
                    }
                    let pred_label = bb_label!(self, *pred_bb);
                    parts.push_str(&format!("[ {}, {} ]", get_name!(self, args[i]), pred_label));
                }
                writeln!(self.buffer, "  {} = phi {} {}", phi_name, param_ty, parts)?;
            }
        }

        let func = self.arena.curr_func.unwrap();
        let insts: Vec<Inst> = self
            .arena
            .func_data(func)
            .layout()
            .basicblock(bb)
            .insts()
            .iter()
            .copied()
            .collect();
        for inst in insts {
            self.visit_inst(inst)?;
        }
        Ok(())
    }

    // ─── Instruction ───

    fn visit_inst(&mut self, inst: Inst) -> std::fmt::Result {
        let (kind, ty) = {
            let data = self.arena.inst_data(inst);
            (data.kind().clone(), data.ty().clone())
        };

        // Comparisons produce i1 in LLVM but i32 in RaanaIR — handle specially
        let is_cmp = matches!(
            &kind,
            InstKind::Binary(b) if b.op().is_compare()
        );

        if !ty.is_unit() && !is_cmp {
            write!(self.buffer, "  {} = ", get_name!(self, inst))?;
        } else if !ty.is_unit() {
            write!(self.buffer, "  ")?;
        } else {
            write!(self.buffer, "  ")?;
        }

        match &kind {
            InstKind::Alloc => {
                let pointee_ty = ty.derefernce();
                writeln!(
                    self.buffer,
                    "alloca {}, align 4",
                    self.type_to_llvm(&pointee_ty)
                )
            }
            InstKind::Binary(binary) => self.visit_binary(binary, inst, &ty),
            InstKind::Branch(branch) => {
                // RaanaIR branch condition is i32 (0=false, non-zero=true).
                // LLVM br needs i1. Emit: %tmp = trunc i32 %cond to i1
                let cond_name = format!("%brcond{}", self.name_counter);
                self.name_counter += 1;
                writeln!(
                    self.buffer,
                    "{} = icmp ne i32 {}, 0",
                    cond_name,
                    get_name!(self, branch.cond())
                )?;
                write!(
                    self.buffer,
                    "  br i1 {}, label {}, label {}",
                    cond_name,
                    bb_label!(self, branch.t_target()),
                    bb_label!(self, branch.f_target())
                )?;
                writeln!(self.buffer)?;
                return Ok(());
            }
            InstKind::Cast(cast) => self.visit_cast(cast, &ty),
            InstKind::Call(call) => self.visit_call(call, &ty),
            InstKind::GetElemPtr(gep) => self.visit_get_elem_ptr(gep),
            InstKind::Jump(jump) => {
                writeln!(self.buffer, "br label {}", bb_label!(self, jump.target()))
            }
            InstKind::Load(load) => {
                writeln!(
                    self.buffer,
                    "load {}, ptr {}",
                    self.type_to_llvm(&ty),
                    get_name!(self, load.src())
                )
            }
            InstKind::Return(ret) => {
                if let Some(val) = ret.value() {
                    writeln!(
                        self.buffer,
                        "ret {} {}",
                        self.type_to_llvm(self.arena.inst_data(val).ty()),
                        get_name!(self, val)
                    )
                } else {
                    writeln!(self.buffer, "ret void")
                }
            }
            InstKind::Store(store) => {
                writeln!(
                    self.buffer,
                    "store {} {}, ptr {}",
                    self.type_to_llvm(self.arena.inst_data(store.src()).ty()),
                    get_name!(self, store.src()),
                    get_name!(self, store.dest())
                )
            }
            InstKind::Integer(_)
            | InstKind::Float(_)
            | InstKind::ZeroInit
            | InstKind::Undef
            | InstKind::Aggregate(_)
            | InstKind::FuncArgRef(_)
            | InstKind::BlockArgRef(_)
            | InstKind::GlobalAlloc(_) => {
                writeln!(self.buffer, "; value")?;
                Ok(())
            }
        }
    }

    // ─── Visitors ───

    fn visit_binary(&mut self, binary: &Binary, inst: Inst, ty: &Type) -> std::fmt::Result {
        let lhs = get_name!(self, binary.lhs());
        let rhs = get_name!(self, binary.rhs());
        let llvm_ty = self.type_to_llvm(ty);
        let is_float = ty.is_f32();

        if binary.op().is_compare() {
            // LLVM icmp/fcmp produce i1; RaanaIR comparisons produce i32.
            // Emit: %cmp = icmp ... ; %result = zext i1 %cmp to i32
            let cmp_name = format!("%cmp{}", self.name_counter);
            self.name_counter += 1;
            match (binary.op(), is_float) {
                (BinaryOp::Eq, true) => writeln!(
                    self.buffer,
                    "{} = fcmp oeq float {}, {}",
                    cmp_name, lhs, rhs
                )?,
                (BinaryOp::Eq, false) => {
                    writeln!(self.buffer, "{} = icmp eq i32 {}, {}", cmp_name, lhs, rhs)?
                }
                (BinaryOp::NotEq, true) => writeln!(
                    self.buffer,
                    "{} = fcmp one float {}, {}",
                    cmp_name, lhs, rhs
                )?,
                (BinaryOp::NotEq, false) => {
                    writeln!(self.buffer, "{} = icmp ne i32 {}, {}", cmp_name, lhs, rhs)?
                }
                (BinaryOp::Gt, true) => writeln!(
                    self.buffer,
                    "{} = fcmp ogt float {}, {}",
                    cmp_name, lhs, rhs
                )?,
                (BinaryOp::Gt, false) => {
                    writeln!(self.buffer, "{} = icmp sgt i32 {}, {}", cmp_name, lhs, rhs)?
                }
                (BinaryOp::Lt, true) => writeln!(
                    self.buffer,
                    "{} = fcmp olt float {}, {}",
                    cmp_name, lhs, rhs
                )?,
                (BinaryOp::Lt, false) => {
                    writeln!(self.buffer, "{} = icmp slt i32 {}, {}", cmp_name, lhs, rhs)?
                }
                (BinaryOp::Ge, true) => writeln!(
                    self.buffer,
                    "{} = fcmp oge float {}, {}",
                    cmp_name, lhs, rhs
                )?,
                (BinaryOp::Ge, false) => {
                    writeln!(self.buffer, "{} = icmp sge i32 {}, {}", cmp_name, lhs, rhs)?
                }
                (BinaryOp::Le, true) => writeln!(
                    self.buffer,
                    "{} = fcmp ole float {}, {}",
                    cmp_name, lhs, rhs
                )?,
                (BinaryOp::Le, false) => {
                    writeln!(self.buffer, "{} = icmp sle i32 {}, {}", cmp_name, lhs, rhs)?
                }
                _ => unreachable!(),
            }
            writeln!(
                self.buffer,
                "  {} = zext i1 {} to i32",
                get_name!(self, inst),
                cmp_name
            )
        } else {
            match binary.op() {
                BinaryOp::Add if is_float => {
                    writeln!(self.buffer, "fadd {} {}, {}", llvm_ty, lhs, rhs)
                }
                BinaryOp::Add => writeln!(self.buffer, "add {} {}, {}", llvm_ty, lhs, rhs),
                BinaryOp::Sub if is_float => {
                    writeln!(self.buffer, "fsub {} {}, {}", llvm_ty, lhs, rhs)
                }
                BinaryOp::Sub => writeln!(self.buffer, "sub {} {}, {}", llvm_ty, lhs, rhs),
                BinaryOp::Mul if is_float => {
                    writeln!(self.buffer, "fmul {} {}, {}", llvm_ty, lhs, rhs)
                }
                BinaryOp::Mul => writeln!(self.buffer, "mul {} {}, {}", llvm_ty, lhs, rhs),
                BinaryOp::Div if is_float => {
                    writeln!(self.buffer, "fdiv {} {}, {}", llvm_ty, lhs, rhs)
                }
                BinaryOp::Div => writeln!(self.buffer, "sdiv {} {}, {}", llvm_ty, lhs, rhs),
                BinaryOp::Rem if is_float => {
                    writeln!(self.buffer, "frem {} {}, {}", llvm_ty, lhs, rhs)
                }
                BinaryOp::Rem => writeln!(self.buffer, "srem {} {}, {}", llvm_ty, lhs, rhs),
                BinaryOp::And => writeln!(self.buffer, "and {} {}, {}", llvm_ty, lhs, rhs),
                BinaryOp::Or => writeln!(self.buffer, "or {} {}, {}", llvm_ty, lhs, rhs),
                BinaryOp::Xor => writeln!(self.buffer, "xor {} {}, {}", llvm_ty, lhs, rhs),
                BinaryOp::Shl => writeln!(self.buffer, "shl {} {}, {}", llvm_ty, lhs, rhs),
                BinaryOp::Shr => writeln!(self.buffer, "lshr {} {}, {}", llvm_ty, lhs, rhs),
                BinaryOp::Sar => writeln!(self.buffer, "ashr {} {}, {}", llvm_ty, lhs, rhs),
                _ => unreachable!(),
            }
        }
    }

    fn visit_cast(&mut self, cast: &Cast, dst_ty: &Type) -> std::fmt::Result {
        let src = get_name!(self, cast.src());
        let src_ty = self.arena.inst_data(cast.src()).ty();
        match (src_ty.kind(), dst_ty.kind()) {
            (TypeKind::Int32, TypeKind::Float32) => {
                writeln!(self.buffer, "sitofp i32 {} to float", src)
            }
            (TypeKind::Float32, TypeKind::Int32) => {
                writeln!(self.buffer, "fptosi float {} to i32", src)
            }
            _ => panic!("unsupported cast: {} -> {}", src_ty, dst_ty),
        }
    }

    fn visit_call(&mut self, call: &Call, ret_ty: &Type) -> std::fmt::Result {
        let callee_data = self.arena.func_data(call.callee());
        let args_str: Vec<String> = call
            .args()
            .iter()
            .map(|&a| {
                format!(
                    "{} {}",
                    self.type_to_llvm(self.arena.inst_data(a).ty()),
                    get_name!(self, a)
                )
            })
            .collect();
        writeln!(
            self.buffer,
            "call {} @{}({})",
            self.type_to_llvm(ret_ty),
            callee_data.name(),
            args_str.join(", ")
        )
    }

    fn visit_get_elem_ptr(&mut self, gep: &GetElemPtr) -> std::fmt::Result {
        let base = get_name!(self, gep.base());
        let base_ty = self.arena.inst_data(gep.base()).ty();
        let src_elem_ty = self.type_to_llvm(&base_ty.derefernce());
        let mut indices = String::new();
        for (i, &off) in gep.offsets().iter().enumerate() {
            if i > 0 {
                indices.push_str(", ");
            }
            indices.push_str(&format!("i32 {}", get_name!(self, off)));
        }
        writeln!(
            self.buffer,
            "getelementptr inbounds {}, ptr {}, {}",
            src_elem_ty, base, indices
        )
    }
}
