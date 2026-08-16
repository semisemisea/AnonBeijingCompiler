use rustc_hash::{FxHashMap as HashMap, FxHashSet};
use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::fmt::Write;

use crate::ir::BasicBlock;
use crate::ir::arena::Arena;
use crate::ir::{
    Function, InstKind, Program, Type, TypeKind,
    inst_kind::{
        Binary, BinaryOp, Call, Cast, Fma, GetElemPtr, VectorExtractElement, VectorInsertElement,
        VectorReduce, VectorSplat,
    },
    instruction::Inst,
};
use crate::opt::{CALLOO_NAME, MULMOD_HELPER};

pub struct LlvmWriter<'a> {
    buffer: String,
    arena: ProgramWrapper<'a>,
    local_names: HashMap<Inst, String>,
    global_names: HashMap<Inst, String>,
    bb_labels: HashMap<BasicBlock, String>,
    phi_incoming: HashMap<BasicBlock, Vec<(BasicBlock, Vec<Inst>)>>,
    /// Blocks reachable from the entry (dead blocks are not emitted, and
    /// their outgoing edges must not feed phis).
    reachable: FxHashSet<BasicBlock>,
    name_counter: usize,
    bb_counter: usize,
    used_memset: bool,
    reduce_decls: FxHashSet<String>,
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
                    "0.0".to_string()
                } else {
                    // LLVM 18 rejects plain decimal constants (interpreted as double) in
                    // float context.  Emit the exact f64 bit pattern of the f32→f64 promotion.
                    let bits = (v as f64).to_bits();
                    format!("0x{:016X}", bits)
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
            local_names: HashMap::default(),
            global_names: HashMap::default(),
            bb_labels: HashMap::default(),
            phi_incoming: HashMap::default(),
            reachable: FxHashSet::default(),
            name_counter: 0,
            bb_counter: 0,
            used_memset: false,
            reduce_decls: FxHashSet::default(),
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
        writeln!(
            self.buffer,
            "declare void @llvm.memset.p0.i64(ptr, i8, i64, i1)"
        )?;

        let funcs: Vec<Function> = self.arena.function_layout().to_vec();
        for &func in &funcs {
            self.arena.curr_func = Some(func);
            let (is_decl, name) = {
                let data = self.arena.func_data(func);
                (data.layout().is_decl(), data.name().to_string())
            };
            if is_decl && name == MULMOD_HELPER {
                // The `soyo_mulmod` builtin has no body and is never a real
                // symbol: every call is expanded inline by `visit_mulmod_call`,
                // so no declaration is emitted for it.
                continue;
            }
            if is_decl && name == CALLOO_NAME {
                // The M68 `soyo_calloc` builtin is provided by the AArch64
                // backend as embedded `.Lsoyo_calloc`, which the LLVM path
                // lacks. Emit a real definition that forwards to libc calloc
                // so the LLVM output links.
                self.visit_calloc_decl()?;
            } else if is_decl {
                self.visit_declare(func)?;
            } else {
                self.visit_define(func)?;
            }
            writeln!(self.buffer)?;
        }
        for decl in &self.reduce_decls {
            writeln!(self.buffer, "{}", decl)?;
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
            TypeKind::Vector(elem, lanes) => {
                format!("<{} x {}>", lanes, self.type_to_llvm(elem))
            }
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
            .unwrap_or_else(Type::get_i32);
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

    /// Emit a real definition for the M68 `soyo_calloc` builtin. The AArch64
    /// backend provides it as embedded `.Lsoyo_calloc`, but the LLVM path has
    /// no such runtime, so forward to libc `calloc` (available on both target
    /// triples the LLVM harness links for).
    fn visit_calloc_decl(&mut self) -> std::fmt::Result {
        self.reduce_decls
            .insert("declare ptr @calloc(i64, i64)".into());
        let c = format!("%calloc_c{}", self.name_counter);
        self.name_counter += 1;
        let s = format!("%calloc_s{}", self.name_counter);
        self.name_counter += 1;
        let p = format!("%calloc_p{}", self.name_counter);
        self.name_counter += 1;
        writeln!(
            self.buffer,
            "define ptr @soyo_calloc(i32 %count, i32 %size) {{"
        )?;
        writeln!(self.buffer, "entry:")?;
        writeln!(self.buffer, "  {} = zext i32 %count to i64", c)?;
        writeln!(self.buffer, "  {} = zext i32 %size to i64", s)?;
        writeln!(
            self.buffer,
            "  {} = call ptr @calloc(i64 {}, i64 {})",
            p, c, s
        )?;
        writeln!(self.buffer, "  ret ptr {}", p)?;
        writeln!(self.buffer, "}}")
    }

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
        self.compute_reachable(func);
        self.collect_phi_incoming(func);

        // Phase 3: pre-assign block labels and inst names, collect all Alloc insts
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

        // Collect all Alloc insts from all blocks (must be hoisted to entry).
        // Preserve block order; each inst appears at most once.
        let alloca_insts: Vec<Inst> = {
            let mut seen = FxHashSet::default();
            all_bbs_and_insts
                .iter()
                .flat_map(|(_, _, insts)| insts.iter().copied())
                .filter(|&inst| {
                    matches!(self.arena.inst_data(inst).kind(), InstKind::Alloc)
                        && seen.insert(inst)
                })
                .collect()
        };

        // Renumber: func params then allocas then everything else, so names stay
        // monotonic when allocas are hoisted to the entry block.
        self.local_names.clear();
        self.name_counter = 0;
        let alloca_set: FxHashSet<Inst> = alloca_insts.iter().copied().collect();
        // Pass 0: function parameters (appear before allocas in the signature)
        for &p in &params {
            self.local_names
                .insert(p, format!("%{}", self.name_counter));
            self.name_counter += 1;
        }
        // Pass 1: allocas (block order)
        for &inst in &alloca_insts {
            self.local_names
                .insert(inst, format!("%{}", self.name_counter));
            self.name_counter += 1;
        }
        // Pass 2: block params then non-alloca insts (block order)
        for (_, bb_params, insts) in &all_bbs_and_insts {
            for &param in bb_params {
                if let Vacant(entry) = self.local_names.entry(param) {
                    entry.insert(format!("%{}", self.name_counter));
                    self.name_counter += 1;
                }
            }
            for &inst in insts {
                if !alloca_set.contains(&inst) && !self.local_names.contains_key(&inst) {
                    self.local_names
                        .insert(inst, format!("%{}", self.name_counter));
                    self.name_counter += 1;
                }
            }
        }
        // Renumber bb labels too
        self.bb_labels.clear();
        self.bb_counter = 0;
        for (bb, _, _) in &all_bbs_and_insts {
            self.bb_labels.entry(*bb).or_insert_with(|| {
                let label = format!("%L{}", self.bb_counter);
                self.bb_counter += 1;
                label
            });
        }

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

        // Phase 5: emit blocks (skip unreachable ones).
        // Alloca insts are hoisted to the entry block.
        let entry_bb = all_bbs_and_insts.first().map(|(bb, ..)| *bb);
        for (bb, _params, _insts) in &all_bbs_and_insts {
            if self.reachable.contains(bb) {
                self.visit_block(*bb, Some(*bb) == entry_bb, &alloca_insts)?;
            }
        }

        writeln!(self.buffer, "}}")
    }

    /// BFS from the entry block over terminator targets: dead blocks (no
    /// path from entry) are never emitted, and their edges must not feed
    /// phis of live blocks.
    fn compute_reachable(&mut self, func: Function) {
        let data = self.arena.func_data(func);
        self.reachable.clear();
        let Some(entry) = data.layout().entry_bb() else {
            return;
        };
        let mut worklist = vec![entry.bb()];
        self.reachable.insert(entry.bb());
        while let Some(bb) = worklist.pop() {
            let layout = data.layout().basicblock(bb);
            let Some(&last_inst) = layout.insts().get_last() else {
                continue;
            };
            let targets: Vec<BasicBlock> = match self.arena.inst_data(last_inst).kind() {
                InstKind::Jump(jump) => vec![jump.target()],
                InstKind::Branch(branch) => vec![branch.t_target(), branch.f_target()],
                _ => Vec::new(),
            };
            for target in targets {
                if self.reachable.insert(target) {
                    worklist.push(target);
                }
            }
        }
    }

    fn collect_phi_incoming(&mut self, func: Function) {
        let data = self.arena.func_data(func);
        self.phi_incoming.clear();
        for layout in data.layout().basicblocks() {
            let bb = layout.bb();
            if !self.reachable.contains(&bb) {
                continue;
            }
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

    fn visit_block(
        &mut self,
        bb: BasicBlock,
        is_entry: bool,
        alloca_insts: &[Inst],
    ) -> std::fmt::Result {
        let label = bb_label!(self, bb);
        let label_def = &label[1..]; // strip '%'

        let params = self.arena.bb_data(bb).params().clone();
        let incomings = self.phi_incoming.get(&bb);

        writeln!(self.buffer, "{}:", label_def)?;

        if !params.is_empty() && !is_entry {
            let incoming_list = incomings.map(|v| v.as_slice()).unwrap_or(&[]);
            for (i, &param_inst) in params.iter().enumerate() {
                let param_ty = self.type_to_llvm(self.arena.inst_data(param_inst).ty());
                let phi_name = get_name!(self, param_inst);
                let mut parts = String::new();
                for (pred_bb, args) in incoming_list.iter() {
                    if !parts.is_empty() {
                        parts.push_str(", ");
                    }
                    let pred_label = bb_label!(self, *pred_bb);
                    parts.push_str(&format!("[ {}, {} ]", get_name!(self, args[i]), pred_label));
                }
                writeln!(self.buffer, "  {} = phi {} {}", phi_name, param_ty, parts)?;
            }
        }

        // Hoist all allocas into the entry block (LLVM requires this to avoid
        // stack growth on loops / non-entry allocas).
        if is_entry {
            for &inst in alloca_insts {
                self.visit_inst(inst)?;
            }
        }

        let alloca_set: FxHashSet<Inst> = alloca_insts.iter().copied().collect();
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
            if !alloca_set.contains(&inst) {
                self.visit_inst(inst)?;
            }
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
        let is_select = matches!(&kind, InstKind::Select(..));
        // `soyo_mulmod` calls are expanded inline (see `visit_mulmod_call`),
        // which emits its own intermediate SSA names, so no shared `%r = ...`
        // prefix is written for them.
        let is_mulmod = matches!(&kind, InstKind::Call(call) if self.is_mulmod_callee(call));

        if !is_mulmod && !ty.is_unit() && !is_cmp && !is_select {
            write!(self.buffer, "  {} = ", get_name!(self, inst))?;
        } else if !is_mulmod {
            write!(self.buffer, "  ")?;
        }

        match &kind {
            InstKind::Alloc => {
                let pointee_ty = ty.derefernce();
                writeln!(
                    self.buffer,
                    "alloca {}, align {}",
                    self.type_to_llvm(&pointee_ty),
                    pointee_ty.alignment()
                )
            }
            InstKind::Binary(binary) => self.visit_binary(binary, inst, &ty),
            InstKind::Select(select) => self.visit_select(select, inst, &ty),
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
                Ok(())
            }
            InstKind::Cast(cast) => self.visit_cast(cast, &ty),
            InstKind::Call(call) => self.visit_call(inst, call, &ty),
            InstKind::TailCall(tail_call) => {
                let callee_data = self.arena.func_data(tail_call.callee());
                let args_str: Vec<String> = tail_call
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
                let ret_ty = callee_data.ret_ty().clone();
                if ret_ty.is_unit() {
                    writeln!(
                        self.buffer,
                        "musttail call void @{}({})",
                        callee_data.name(),
                        args_str.join(", ")
                    )?;
                    writeln!(self.buffer, "  ret void")
                } else {
                    // Reuse the tail-call instruction's own (block-ordered)
                    // name for the musttail result so LLVM's unnamed-value
                    // numbering stays monotonic.
                    let result = get_name!(self, inst);
                    writeln!(
                        self.buffer,
                        "{} = musttail call {} @{}({})",
                        result,
                        self.type_to_llvm(&ret_ty),
                        callee_data.name(),
                        args_str.join(", ")
                    )?;
                    write!(
                        self.buffer,
                        "  ret {} {}",
                        self.type_to_llvm(&ret_ty),
                        result
                    )?;
                    writeln!(self.buffer)
                }
            }
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
            InstKind::MemZero(mem_zero) => {
                put_name!(self, mem_zero.dest());
                let len = match mem_zero.byte_len_len() {
                    crate::ir::inst_kind::mem_zero::MemZeroLen::Const(byte_len) => {
                        byte_len.to_string()
                    }
                    crate::ir::inst_kind::mem_zero::MemZeroLen::Value(byte_len) => {
                        let data = self.arena.inst_data(*byte_len);
                        if let InstKind::Integer(value) = data.kind() {
                            // i32 constant: use it directly as an i64
                            // literal. Modern llc rejects `zext i32 40 to
                            // i64` constexprs inside call arguments.
                            value.value().to_string()
                        } else {
                            let name = get_name!(self, *byte_len);
                            let zext = format!("%len_ext_{}", self.name_counter);
                            self.name_counter += 1;
                            writeln!(self.buffer, "{} = zext i32 {} to i64", zext, name)?;
                            zext
                        }
                    }
                };
                writeln!(
                    self.buffer,
                    "call void @llvm.memset.p0.i64(ptr {}, i8 0, i64 {}, i1 false)",
                    get_name!(self, mem_zero.dest()),
                    len
                )?;
                self.used_memset = true;
                Ok(())
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
                    let func_ret_ty = self
                        .arena
                        .func_data(self.arena.curr_func.unwrap())
                        .ret_ty()
                        .clone();
                    if func_ret_ty.is_unit() {
                        writeln!(self.buffer, "ret void")
                    } else {
                        writeln!(self.buffer, "ret {} undef", self.type_to_llvm(&func_ret_ty))
                    }
                }
            }
            InstKind::Store(store) => {
                let maybe_agg: Option<(crate::ir::Aggregate, Type)> = {
                    let src_data = self.arena.inst_data(store.src());
                    if let InstKind::Aggregate(agg) = src_data.kind() {
                        Some((agg.clone(), src_data.ty().clone()))
                    } else {
                        None
                    }
                };
                if let Some((agg, src_ty)) = maybe_agg {
                    if self.is_all_zeroinit(&agg) {
                        let size = self.type_size_bytes(&src_ty);
                        writeln!(
                            self.buffer,
                            "call void @llvm.memset.p0.i64(ptr {}, i8 0, i64 {}, i1 false)",
                            get_name!(self, store.dest()),
                            size
                        )?;
                        self.used_memset = true;
                        return Ok(());
                    }
                    self.emit_aggregate_store(&agg, store.dest(), &src_ty, &[])?;
                    Ok(())
                } else {
                    let src_data = self.arena.inst_data(store.src());
                    writeln!(
                        self.buffer,
                        "store {} {}, ptr {}",
                        self.type_to_llvm(src_data.ty()),
                        get_name!(self, store.src()),
                        get_name!(self, store.dest())
                    )
                }
            }
            InstKind::Integer(_)
            | InstKind::Float(_)
            | InstKind::ZeroInit
            | InstKind::Undef
            | InstKind::Aggregate(_)
            | InstKind::BlockArgRef(_)
            | InstKind::GlobalAlloc(_) => {
                writeln!(self.buffer, "; value")?;
                Ok(())
            }
            InstKind::Fma(fma) => self.visit_fma(fma, inst, &ty),
            InstKind::VectorSplat(splat) => self.visit_vector_splat(splat, inst, &ty),
            InstKind::VectorExtractElement(extract) => {
                self.visit_vector_extract_element(extract, inst, &ty)
            }
            InstKind::VectorInsertElement(insert) => {
                self.visit_vector_insert_element(insert, inst, &ty)
            }
            InstKind::VectorReduce(reduce) => self.visit_vector_reduce(reduce, inst, &ty),
        }
    }

    // ─── Visitors ───

    fn visit_binary(&mut self, binary: &Binary, inst: Inst, ty: &Type) -> std::fmt::Result {
        let lhs = get_name!(self, binary.lhs());
        let rhs = get_name!(self, binary.rhs());
        let llvm_ty = self.type_to_llvm(ty);
        // For comparisons, the result type is always i32, but operands may be float.
        // Use the lhs operand type to determine int vs float dispatch.
        let is_float = self.arena.inst_data(binary.lhs()).ty().is_f32();
        let is_vector = self.arena.inst_data(binary.lhs()).ty().is_vector();

        if is_vector {
            let TypeKind::Vector(_elem, lanes) = self.arena.inst_data(binary.lhs()).ty().kind()
            else {
                unreachable!("vector operand has a vector type");
            };
            return match binary.op() {
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
                BinaryOp::And => writeln!(self.buffer, "and {} {}, {}", llvm_ty, lhs, rhs),
                BinaryOp::Or => writeln!(self.buffer, "or {} {}, {}", llvm_ty, lhs, rhs),
                BinaryOp::Xor => writeln!(self.buffer, "xor {} {}, {}", llvm_ty, lhs, rhs),
                BinaryOp::Min if is_float => {
                    writeln!(self.buffer, "fmin {} {}, {}", llvm_ty, lhs, rhs)
                }
                BinaryOp::Max if is_float => {
                    writeln!(self.buffer, "fmax {} {}, {}", llvm_ty, lhs, rhs)
                }
                BinaryOp::Min => writeln!(self.buffer, "smin {} {}, {}", llvm_ty, lhs, rhs),
                BinaryOp::Max => writeln!(self.buffer, "smax {} {}, {}", llvm_ty, lhs, rhs),
                BinaryOp::Eq | BinaryOp::Gt => {
                    // LLVM vector compares produce `<N x i1>`; RaanaIR masks are
                    // all-ones lanes of the operand type. Sign-extend to match.
                    let cmp_name = format!("%vcmp{}", self.name_counter);
                    self.name_counter += 1;
                    let cmp_op = if is_float {
                        match binary.op() {
                            BinaryOp::Eq => "oeq",
                            BinaryOp::Gt => "ogt",
                            _ => unreachable!(),
                        }
                    } else {
                        match binary.op() {
                            BinaryOp::Eq => "eq",
                            BinaryOp::Gt => "sgt",
                            _ => unreachable!(),
                        }
                    };
                    writeln!(
                        self.buffer,
                        "{} = {} {} {}, {}",
                        cmp_name,
                        format_args!("icmp {}", cmp_op),
                        llvm_ty,
                        lhs,
                        rhs
                    )?;
                    writeln!(
                        self.buffer,
                        "  {} = sext <{} x i1> {} to {}",
                        get_name!(self, inst),
                        lanes,
                        cmp_name,
                        llvm_ty
                    )
                }
                op => panic!("unsupported vector binary operation: {op:?}"),
            };
        }

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
                // SysY/C `!=` is true for unordered float operands, matching
                // frontend folding and AArch64 `fcmp` plus `cset ne`.
                (BinaryOp::NotEq, true) => writeln!(
                    self.buffer,
                    "{} = fcmp une float {}, {}",
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
            (TypeKind::Vector(_, _), TypeKind::Vector(_, _)) => {
                let llvm_src = self.type_to_llvm(src_ty);
                let llvm_dst = self.type_to_llvm(dst_ty);
                let is_int_src =
                    matches!(src_ty.kind(), TypeKind::Vector(elem, _) if elem.is_i32());
                if is_int_src {
                    writeln!(self.buffer, "sitofp {} {} to {}", llvm_src, src, llvm_dst)
                } else {
                    writeln!(self.buffer, "fptosi {} {} to {}", llvm_src, src, llvm_dst)
                }
            }
            _ => panic!("unsupported cast: {} -> {}", src_ty, dst_ty),
        }
    }

    fn visit_select(
        &mut self,
        select: &crate::ir::Select,
        inst: Inst,
        ty: &Type,
    ) -> std::fmt::Result {
        let cond_ty = self.arena.inst_data(select.cond()).ty().clone();
        if cond_ty.is_vector() {
            // A RaanaIR vector condition is an all-ones/zero mask of the
            // alternative type. LLVM select needs `<N x i1>`: compare each lane
            // against zero.
            let TypeKind::Vector(_, lanes) = cond_ty.kind() else {
                unreachable!("vector condition has a vector type");
            };
            let cond_name = format!("%vselcond{}", self.name_counter);
            self.name_counter += 1;
            writeln!(
                self.buffer,
                "{} = icmp ne {} {}, zeroinitializer",
                cond_name,
                self.type_to_llvm(&cond_ty),
                get_name!(self, select.cond())
            )?;
            writeln!(
                self.buffer,
                "  {} = select <{} x i1> {}, {} {}, {} {}",
                get_name!(self, inst),
                lanes,
                cond_name,
                self.type_to_llvm(ty),
                get_name!(self, select.if_true()),
                self.type_to_llvm(ty),
                get_name!(self, select.if_false())
            )
        } else {
            let cond_name = format!("%selectcond{}", self.name_counter);
            self.name_counter += 1;
            writeln!(
                self.buffer,
                "{} = icmp ne i32 {}, 0",
                cond_name,
                get_name!(self, select.cond())
            )?;
            writeln!(
                self.buffer,
                "  {} = select i1 {}, {} {}, {} {}",
                get_name!(self, inst),
                cond_name,
                self.type_to_llvm(ty),
                get_name!(self, select.if_true()),
                self.type_to_llvm(ty),
                get_name!(self, select.if_false())
            )
        }
    }

    fn visit_fma(&mut self, fma: &Fma, inst: Inst, ty: &Type) -> std::fmt::Result {
        let llvm_ty = self.type_to_llvm(ty);
        let mul = format!("%fma_mul{}", self.name_counter);
        self.name_counter += 1;
        writeln!(
            self.buffer,
            "{} = fmul {} {}, {}",
            mul,
            llvm_ty,
            get_name!(self, fma.lhs()),
            get_name!(self, fma.rhs())
        )?;
        writeln!(
            self.buffer,
            "  {} = fadd {} {}, {}",
            get_name!(self, inst),
            llvm_ty,
            get_name!(self, fma.acc()),
            mul
        )
    }

    fn visit_vector_splat(
        &mut self,
        splat: &VectorSplat,
        inst: Inst,
        ty: &Type,
    ) -> std::fmt::Result {
        let TypeKind::Vector(elem, lanes) = ty.kind() else {
            unreachable!("splat produces a vector");
        };
        let elem_llvm = self.type_to_llvm(elem);
        let inserted = format!("%splat0{}", self.name_counter);
        self.name_counter += 1;
        writeln!(
            self.buffer,
            "{} = insertelement {} poison, {} {}, i32 0",
            inserted,
            self.type_to_llvm(ty),
            elem_llvm,
            get_name!(self, splat.src())
        )?;
        writeln!(
            self.buffer,
            "  {} = shufflevector {} {}, {} poison, <{} x i32> zeroinitializer",
            get_name!(self, inst),
            self.type_to_llvm(ty),
            inserted,
            self.type_to_llvm(ty),
            lanes
        )
    }

    fn visit_vector_extract_element(
        &mut self,
        extract: &VectorExtractElement,
        inst: Inst,
        ty: &Type,
    ) -> std::fmt::Result {
        writeln!(
            self.buffer,
            "  {} = extractelement {}, {} {}, i32 {}",
            get_name!(self, inst),
            self.type_to_llvm(self.arena.inst_data(extract.src()).ty()),
            self.type_to_llvm(ty),
            get_name!(self, extract.src()),
            get_name!(self, extract.index())
        )
    }

    fn visit_vector_insert_element(
        &mut self,
        insert: &VectorInsertElement,
        inst: Inst,
        ty: &Type,
    ) -> std::fmt::Result {
        writeln!(
            self.buffer,
            "  {} = insertelement {} {}, {} {}, i32 {}",
            get_name!(self, inst),
            self.type_to_llvm(ty),
            get_name!(self, insert.vector()),
            self.type_to_llvm(self.arena.inst_data(insert.element()).ty()),
            get_name!(self, insert.element()),
            get_name!(self, insert.index())
        )
    }

    fn visit_vector_reduce(
        &mut self,
        reduce: &VectorReduce,
        inst: Inst,
        ty: &Type,
    ) -> std::fmt::Result {
        let TypeKind::Vector(elem, lanes) = self.arena.inst_data(reduce.src()).ty().kind() else {
            unreachable!("reduce source is a vector");
        };
        let elem_llvm = self.type_to_llvm(elem);
        let src_llvm = self.type_to_llvm(self.arena.inst_data(reduce.src()).ty());
        let intrinsic = format!("@llvm.vector.reduce.add.v{lanes}e{}", elem_llvm);
        let decl = format!(
            "declare {} {}({})",
            self.type_to_llvm(ty),
            intrinsic,
            src_llvm
        );
        self.reduce_decls.insert(decl);
        writeln!(
            self.buffer,
            "  {} = call {} {}({} {})",
            get_name!(self, inst),
            self.type_to_llvm(ty),
            intrinsic,
            src_llvm,
            get_name!(self, reduce.src())
        )
    }

    fn is_mulmod_callee(&self, call: &Call) -> bool {
        let callee = call.callee();
        self.arena.func_data(callee).name() == MULMOD_HELPER
    }

    /// Expand a call to the compiler-provided `soyo_mulmod(a, b, p)` builtin
    /// (see the M60 `mulmod_recognize` pass) into the LLVM IR equivalent of the
    /// AArch64 backend's `smull; sxtw; sdiv; msub` sequence: the exact 64-bit
    /// product of the two 32-bit operands, sign-extended modulus, truncating
    /// division, and remainder. The builtin has no body and is never emitted as
    /// a real symbol, so every call must be expanded here.
    fn visit_mulmod_call(&mut self, inst: Inst, call: &Call) -> std::fmt::Result {
        let args = call.args();
        assert_eq!(args.len(), 3, "soyo_mulmod takes exactly three arguments");
        let a = get_name!(self, args[0]);
        let b = get_name!(self, args[1]);
        let p = get_name!(self, args[2]);

        let a64 = format!("%mulmod_a{}", self.name_counter);
        self.name_counter += 1;
        writeln!(self.buffer, "  {} = sext i32 {} to i64", a64, a)?;
        let b64 = format!("%mulmod_b{}", self.name_counter);
        self.name_counter += 1;
        writeln!(self.buffer, "  {} = sext i32 {} to i64", b64, b)?;
        let prod = format!("%mulmod_p{}", self.name_counter);
        self.name_counter += 1;
        writeln!(self.buffer, "  {} = mul i64 {}, {}", prod, a64, b64)?;
        let p64 = format!("%mulmod_m{}", self.name_counter);
        self.name_counter += 1;
        writeln!(self.buffer, "  {} = sext i32 {} to i64", p64, p)?;
        let quot = format!("%mulmod_q{}", self.name_counter);
        self.name_counter += 1;
        writeln!(self.buffer, "  {} = sdiv i64 {}, {}", quot, prod, p64)?;
        let rem = format!("%mulmod_r{}", self.name_counter);
        self.name_counter += 1;
        writeln!(self.buffer, "  {} = srem i64 {}, {}", rem, prod, p64)?;
        writeln!(
            self.buffer,
            "  {} = trunc i64 {} to i32",
            get_name!(self, inst),
            rem
        )
    }

    fn visit_call(&mut self, inst: Inst, call: &Call, ret_ty: &Type) -> std::fmt::Result {
        let callee_data = self.arena.func_data(call.callee());
        let callee_name = callee_data.name().to_string();
        if callee_name == MULMOD_HELPER {
            return self.visit_mulmod_call(inst, call);
        }
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
            callee_name,
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
            "getelementptr {}, ptr {}, {}",
            src_elem_ty, base, indices
        )
    }

    fn type_size_bytes(&self, ty: &Type) -> usize {
        match ty.kind() {
            TypeKind::Int32 | TypeKind::Float32 => 4,
            TypeKind::Pointer(_) | TypeKind::String => 8,
            TypeKind::Array(elem, len) => *len * self.type_size_bytes(elem),
            TypeKind::Vector(elem, lanes) => *lanes * self.type_size_bytes(elem),
            _ => 0,
        }
    }

    fn is_all_zeroinit(&self, agg: &crate::ir::Aggregate) -> bool {
        agg.value().iter().all(|&v| {
            let data = self.arena.inst_data(v);
            match data.kind() {
                InstKind::ZeroInit => true,
                InstKind::Integer(i) => i.value() == 0,
                InstKind::Float(f) => f.value() == 0.0,
                InstKind::Aggregate(inner) => self.is_all_zeroinit(inner),
                _ => false,
            }
        })
    }

    /// Recursively decompose an Aggregate store into individual GEP+store pairs.
    /// Needed because LLVM does not allow runtime values inside aggregate store constants.
    fn emit_aggregate_store(
        &mut self,
        agg: &crate::ir::Aggregate,
        dest: Inst,
        agg_ty: &Type,
        indices: &[u32],
    ) -> std::fmt::Result {
        match agg_ty.kind() {
            TypeKind::Array(elem_ty, _len) => {
                for (i, &elem_val) in agg.value().iter().enumerate() {
                    let mut new_indices = indices.to_vec();
                    new_indices.push(i as u32);
                    let inner_agg: Option<crate::ir::Aggregate> = {
                        let elem_data = self.arena.inst_data(elem_val);
                        if let InstKind::Aggregate(inner) = elem_data.kind() {
                            Some(inner.clone())
                        } else {
                            None
                        }
                    };
                    if let Some(inner) = inner_agg {
                        self.emit_aggregate_store(&inner, dest, elem_ty, &new_indices)?;
                    } else {
                        // Emit GEP to this element, then store
                        let gep_name = format!("%gep{}", self.name_counter);
                        self.name_counter += 1;
                        let elem_llvm_ty = self.type_to_llvm(elem_ty);
                        let dest_name = get_name!(self, dest);
                        let base_ty =
                            self.type_to_llvm(&self.arena.inst_data(dest).ty().derefernce());
                        let idx_strs: Vec<String> = std::iter::once("i32 0".to_string())
                            .chain(new_indices.iter().map(|idx| format!("i32 {idx}")))
                            .collect();
                        writeln!(
                            self.buffer,
                            "  {} = getelementptr {}, ptr {}, {}",
                            gep_name,
                            base_ty,
                            dest_name,
                            idx_strs.join(", ")
                        )?;
                        writeln!(
                            self.buffer,
                            "  store {} {}, ptr {}",
                            elem_llvm_ty,
                            get_name!(self, elem_val),
                            gep_name
                        )?;
                    }
                }
            }
            _ => {
                // Scalar type — single element store
                // GEP to the single element
                let gep_name = format!("%gep{}", self.name_counter);
                self.name_counter += 1;
                let dest_ty = self.arena.inst_data(dest).ty();
                let base_ty = self.type_to_llvm(&dest_ty.derefernce());
                let dest_name = get_name!(self, dest);
                let idx_strs: Vec<String> = std::iter::once("i32 0".to_string())
                    .chain(indices.iter().map(|idx| format!("i32 {idx}")))
                    .collect();
                writeln!(
                    self.buffer,
                    "  {} = getelementptr {}, ptr {}, {}",
                    gep_name,
                    base_ty,
                    dest_name,
                    idx_strs.join(", ")
                )?;
                let elem_val = agg.value()[0];
                let elem_ty = self.type_to_llvm(self.arena.inst_data(elem_val).ty());
                writeln!(
                    self.buffer,
                    "  store {} {}, ptr {}",
                    elem_ty,
                    get_name!(self, elem_val),
                    gep_name
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::ir::{
        Program, Type,
        builder_trait::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
    };

    use super::LlvmWriter;

    #[test]
    fn writes_select_with_i32_truthiness() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "choose".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();

        let cond = data.new_local_inst().integer(2);
        let if_true = data.new_local_inst().integer(10);
        let if_false = data.new_local_inst().integer(20);
        let select = data.new_local_inst().select(cond, if_true, if_false);
        data.layout_mut().insert_inst(entry, select);
        let ret = data.new_local_inst().ret(Some(select));
        data.layout_mut().insert_inst(entry, ret);

        let mut writer = LlvmWriter::new(&program);
        writer.write().unwrap();
        let llvm = writer.finish();
        assert!(llvm.contains("icmp ne i32 2, 0"), "{llvm}");
        assert!(llvm.contains("select i1 %selectcond"), "{llvm}");
        assert!(llvm.contains(", i32 10, i32 20"), "{llvm}");
    }

    #[test]
    fn writes_unordered_float_not_equal() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_i32(),
            "float_not_equal".into(),
            vec![Type::get_f32(), Type::get_f32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let lhs = data.params()[0];
        let rhs = data.params()[1];
        let not_equal = data
            .new_local_inst()
            .binary(crate::ir::BinaryOp::NotEq, lhs, rhs);
        let ret = data.new_local_inst().ret(Some(not_equal));
        data.layout_mut().insert_inst(entry, not_equal);
        data.layout_mut().insert_inst(entry, ret);

        let mut writer = LlvmWriter::new(&program);
        writer.write().unwrap();
        assert!(writer.finish().contains("fcmp une float"));
    }

    #[test]
    fn writes_mem_zero_as_i64_memset() {
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "clear".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let alloc = data
            .new_local_inst()
            .alloc(Type::get_array(Type::get_i32(), 4));
        let clear = data.new_local_inst().mem_zero(alloc, 16);
        data.layout_mut().insert_inst(entry, clear);
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let mut writer = LlvmWriter::new(&program);
        writer.write().unwrap();
        let llvm = writer.finish();
        assert!(
            llvm.contains("declare void @llvm.memset.p0.i64(ptr, i8, i64, i1)"),
            "{llvm}"
        );
        assert!(
            llvm.contains("call void @llvm.memset.p0.i64(ptr %"),
            "{llvm}"
        );
        assert!(llvm.contains("i8 0, i64 16, i1 false)"), "{llvm}");
    }

    #[test]
    fn does_not_claim_unproven_gep_inbounds() {
        let array_ty = Type::get_array(Type::get_i32(), 4);
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_pointer(Type::get_i32()),
            "address".into(),
            vec![Type::get_pointer(array_ty), Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let base = data.params()[0];
        let index = data.params()[1];
        let zero = data.new_local_inst().integer(0);
        let gep = data.new_local_inst().get_elem_ptr(base, vec![zero, index]);
        data.layout_mut().insert_inst(entry, gep);
        let ret = data.new_local_inst().ret(Some(gep));
        data.layout_mut().insert_inst(entry, ret);

        let mut writer = LlvmWriter::new(&program);
        writer.write().unwrap();
        let llvm = writer.finish();
        assert!(llvm.contains("getelementptr [4 x i32]"), "{llvm}");
        assert!(!llvm.contains("getelementptr inbounds"), "{llvm}");
    }

    #[test]
    fn expands_soyo_mulmod_inline_and_omits_declaration() {
        let mut program = Program::new();
        let helper = program.new_function(
            Type::get_i32(),
            super::MULMOD_HELPER.into(),
            vec![Type::get_i32(), Type::get_i32(), Type::get_i32()],
        );
        let function = program.new_function(Type::get_i32(), "caller".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let a = data.new_local_inst().integer(5);
        let b = data.new_local_inst().integer(7);
        let p = data.new_local_inst().integer(11);
        let call = data
            .new_local_inst()
            .call_with_type(helper, vec![a, b, p], Type::get_i32());
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_inst().ret(Some(call));
        data.layout_mut().insert_inst(entry, ret);

        let mut writer = LlvmWriter::new(&program);
        writer.write().unwrap();
        let llvm = writer.finish();
        assert!(llvm.contains("sext i32 5 to i64"), "{llvm}");
        assert!(llvm.contains("sext i32 7 to i64"), "{llvm}");
        assert!(llvm.contains("sext i32 11 to i64"), "{llvm}");
        assert!(llvm.contains("mul i64"), "{llvm}");
        assert!(llvm.contains("sdiv i64"), "{llvm}");
        assert!(llvm.contains("srem i64"), "{llvm}");
        assert!(llvm.contains("trunc i64"), "{llvm}");
        assert!(!llvm.contains("@soyo_mulmod"), "{llvm}");
    }

    #[test]
    fn defines_soyo_calloc_forwarding_to_libc_calloc() {
        let mut program = Program::new();
        let helper = program.new_function(
            Type::get_pointer(Type::get_i32()),
            super::CALLOO_NAME.into(),
            vec![],
        );
        let function =
            program.new_function(Type::get_pointer(Type::get_i32()), "caller".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let count = data.new_local_inst().integer(4);
        let size = data.new_local_inst().integer(4);
        let call = data.new_local_inst().call_with_type(
            helper,
            vec![count, size],
            Type::get_pointer(Type::get_i32()),
        );
        data.layout_mut().insert_inst(entry, call);
        let ret = data.new_local_inst().ret(Some(call));
        data.layout_mut().insert_inst(entry, ret);

        let mut writer = LlvmWriter::new(&program);
        writer.write().unwrap();
        let llvm = writer.finish();
        assert!(
            llvm.contains("define ptr @soyo_calloc(i32 %count, i32 %size)"),
            "{llvm}"
        );
        assert!(llvm.contains("call ptr @calloc(i64"), "{llvm}");
        assert!(llvm.contains("declare ptr @calloc(i64, i64)"), "{llvm}");
        assert!(!llvm.contains("declare ptr @soyo_calloc"), "{llvm}");
    }
}
