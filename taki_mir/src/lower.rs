use log::{debug, trace};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::{SmallVec, smallvec};

use crate::abi::{ABIMachineSpec, CalleeABI};
use crate::block_order::{BlockLoweringOrder, LoweredBlock, MirBlockIndex};
use crate::prelude::*;
use crate::reg_alloc::function::Function;
use crate::reg_alloc::reg::PReg;
use crate::register::{Reg, VRegAllocator, Writable};
use crate::types::{F32, I32};
use crate::vcode::{VCodeBuilder, VCodeContainer, VCodeInst};
use raana_ir::ir::TypeKind as HirTypeKind;
pub enum ValueUseCount {
    Unused = 0,
    Once = 1,
    Multiple = 2,
}

/// The register, if any, containing the result of a lowered HIR instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoweredOutput {
    None,
    Value(Reg),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GepDynamicTerm {
    pub index: HirInst,
    pub stride: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GepAddress {
    pub base: HirInst,
    pub constant_offset: i64,
    pub dynamic_terms: SmallVec<[GepDynamicTerm; 4]>,
}

pub fn analyze_gep(
    arena: ArenaContext<'_>,
    inst: HirInst,
    gep: &GetElemPtr,
) -> Result<GepAddress, String> {
    fn checked_size(ty: &HirType) -> Option<usize> {
        match ty.kind() {
            HirTypeKind::ArgList | HirTypeKind::Unit => Some(0),
            HirTypeKind::Int32 | HirTypeKind::Float32 => Some(4),
            HirTypeKind::Array(element, len) => checked_size(element)?.checked_mul(*len),
            HirTypeKind::String | HirTypeKind::Pointer(_) | HirTypeKind::Function(_, _) => {
                Some(core::mem::size_of::<*const ()>())
            }
        }
    }

    let mut current_ty = arena.inst_data(gep.base()).ty().clone();
    let mut constant_offset = 0_i64;
    let mut dynamic_terms = SmallVec::new();

    for (position, &index) in gep.offsets().iter().enumerate() {
        let index_ty = arena.inst_data(index).ty();
        if !index_ty.is_i32() {
            return Err(format!(
                "GEP index {position} must be i32, received {index_ty}"
            ));
        }

        current_ty = match current_ty.kind() {
            HirTypeKind::Pointer(element) | HirTypeKind::Array(element, _) => element.clone(),
            _ => {
                return Err(format!(
                    "GEP index {position} cannot traverse type {current_ty}"
                ));
            }
        };

        let stride = checked_size(&current_ty).ok_or_else(|| {
            format!("GEP stride overflows for index {position} type {current_ty}")
        })?;
        let stride = u64::try_from(stride)
            .map_err(|_| format!("GEP stride does not fit u64 for index {position}"))?;

        if let HirInstKind::Integer(value) = arena.inst_data(index).kind() {
            let stride = i64::try_from(stride)
                .map_err(|_| format!("GEP constant stride does not fit i64 at index {position}"))?;
            let contribution = i64::from(value.value())
                .checked_mul(stride)
                .ok_or_else(|| {
                    format!("GEP constant multiplication overflows at index {position}")
                })?;
            constant_offset = constant_offset
                .checked_add(contribution)
                .ok_or_else(|| format!("GEP constant offset overflows at index {position}"))?;
        } else {
            dynamic_terms.push(GepDynamicTerm { index, stride });
        }
    }

    let computed_ty = current_ty.reference();
    let declared_ty = arena.inst_data(inst).ty();
    if &computed_ty != declared_ty {
        return Err(format!(
            "GEP result type mismatch: computed {computed_ty}, declared {declared_ty}"
        ));
    }

    Ok(GepAddress {
        base: gep.base(),
        constant_offset,
        dynamic_terms,
    })
}

/// A lowering context for a single function
pub struct LowerContext<'prog, I: VCodeInst> {
    /// Arena to get everything you need about HirFunction
    pub arena: ArenaContext<'prog>,

    /// The VCode Container we currently building.
    pub vcode: VCodeBuilder<I>,

    /// A virtural register allocator.
    /// Allocate each instruction a register.
    vregs_alloc: VRegAllocator<I>,

    /// A map for instruction and register.
    /// INFO: Currently should be fixed map,
    /// since we allocate all the register in `new` function
    reg_map: FxHashMap<HirInst, Reg>,

    /// Current HirInstruction about to lower
    cur_inst: Option<HirInst>,

    /// Current source block, when lowering a source-backed VCode block.
    cur_block: Option<HirBasicBlock>,

    /// Record the sunk inst.
    /// A dynamically changing map
    inst_sunk: FxHashSet<HirInst>,

    /// When lowering a HLIR instruction, it will produce zero/one/multilple instruction(s).
    /// For example, `branch cond, target 1, target 2`;
    /// will at least produce a comparison instruction `cmp` and a branch instruction `br`.
    /// In `ir_inst` is will store like [cmp, br]. Direction ->
    /// Since vcode is building backward, a.k.a [br, cmp] with direction <-
    /// we have to store it in a small buffer and reverse when pushing to vcode.
    ir_inst: Vec<I>,

    // ----------------- Side effect and color----------------- //
    // INFO: We give each instruction a color. The colors are same between two instruction if:
    // - There is no instruction have side effect between them.
    // - They are in the same basicblock.
    // So we traverse all instruction before lowering. If we met:
    // - A instruction have side effect
    // - Start of a basicblock
    // we would change the color.
    // The color is represented as u32, start with 0.
    //
    // INFO: Since color could change few times, compared to the number of instructions,
    // we only record the place where color mutate.
    //
    /// This is a sparse map
    /// In other words, it only record the side-effect instruction.
    /// We can easily infer the normal instruction's color.
    /// A fixed map.
    inst_color: FxHashMap<HirInst, u32>,

    /// Record the color at the tail of the block.
    /// A fixed map.
    bb_end_color: FxHashMap<HirBasicBlock, u32>,

    /// The color at current place.
    cur_color: Option<u32>,

    /// While lowering, we might use a instruction many time.
    /// We keep track of usage info in real-time.
    value_lowered_use: FxHashMap<HirInst, u32>,
}

pub trait LowerBackend {
    type MInst: VCodeInst;

    fn lower(ctx: &mut LowerContext<Self::MInst>, inst: HirInst) -> LoweredOutput;

    fn lower_branch(ctx: &mut LowerContext<Self::MInst>, inst: HirInst, target: &[MirBlockIndex]);

    fn data_section_directive() -> &'static str;

    fn bss_section_directive() -> &'static str;

    fn text_section_directive() -> &'static str;

    fn global_directive() -> &'static str;

    fn word_directive() -> &'static str;

    fn zero_directive() -> &'static str;

    fn preg_name(preg: PReg) -> &'static str;

    fn format_block_label(lb: &LoweredBlock, func_data: &HirFunctionData) -> String;

    fn emit_long_jump(ctx: &mut LowerContext<Self::MInst>, target: MirBlockIndex);

    /// Optional target-private assembly appended after all generated functions.
    fn runtime_assembly(_program: &HirProgram) -> Option<String> {
        None
    }
}

/// impl block for all backend specified operation
impl<'prog, I: VCodeInst> LowerContext<'prog, I> {
    pub fn new(
        program: &'prog HirProgram,
        func: HirFunction,
        abi: CalleeABI<I::ABISpec>,
        block_order: BlockLoweringOrder,
    ) -> LowerContext<'prog, I> {
        let arena = ArenaContext {
            program,
            curr_func: Some(func),
        };
        let mut abi = abi;
        abi.set_outgoing_arg_size(Self::precompute_outgoing_arg_size(arena));
        let vcode = VCodeBuilder::new(abi, block_order);
        let mut vregs_alloc = VRegAllocator::with_capaticy(0);
        let mut reg_map = FxHashMap::default();
        let mut inst_color = FxHashMap::default();
        let mut bb_end_color = FxHashMap::default();

        // Setup the struct before actual lowering.
        // This include:
        // - Set the current function indicator
        // - Pre-allocate the virtual register
        // - Calculate the side-effect color of each instruction.
        let data = arena.f();
        debug!(target: "taki_mir::lower", "function={} HIR params={} blocks={}", data.name(), data.params().len(), data.layout().basicblocks().into_iter().count());
        let mut acc_color = 0;

        // Allocate each function parameter a virtual register.
        for &f_param in data.params() {
            let param_data = data.inst_data(f_param);
            let reg = vregs_alloc.alloc(param_data.ty().into());
            assert!(reg_map.insert(f_param, reg).is_none());
        }

        // Virtual register allocation and color mapping.
        for bb_layout in data.layout().basicblocks() {
            acc_color += 1;
            // For each non-unit typed instruction/block parameter, we allocate a virtual register based on its type.
            for &param in data.bb_data(bb_layout.bb()).params() {
                match reg_map.entry(param) {
                    // Already allocated
                    std::collections::hash_map::Entry::Occupied(_) => {}
                    std::collections::hash_map::Entry::Vacant(v) => {
                        let inst_data = data.inst_data(param);
                        let reg = vregs_alloc.alloc(inst_data.ty().into());
                        v.insert(reg);
                    }
                }
            }
            for &inst in bb_layout.insts() {
                let have_side_effect = arena.has_side_effect_when_lowering(inst);

                if have_side_effect {
                    inst_color.insert(inst, acc_color);
                    acc_color += 1;
                }

                let inst_data = data.inst_data(inst);
                match reg_map.entry(inst) {
                    // Already allocated
                    std::collections::hash_map::Entry::Occupied(_) => {}
                    std::collections::hash_map::Entry::Vacant(vacant_entry) => {
                        if !inst_data.ty().is_unit() {
                            let reg = vregs_alloc.alloc(inst_data.ty().into());
                            vacant_entry.insert(reg);
                        }
                    }
                }
                // Find out all the constants.
                // INFO: We cannont find any integer/float constants in layout.
                // They are born in the use chain of other instructions.
                // So we manually find it out.
                // TODO: 1. We can insert the integer/float instruction into the layout.
                // TODO: 2. If we have constant pool or something maybe we can remove it.
                // if it produce a value, we allocate a virtual register to it.
                for used in inst_data.inst_usage() {
                    let used_inst_data = arena.inst_data(used);
                    match used_inst_data.kind() {
                        InstKind::Integer(..) | InstKind::Float(..) => match reg_map.entry(used) {
                            std::collections::hash_map::Entry::Occupied(_) => {}
                            std::collections::hash_map::Entry::Vacant(vacant_entry) => {
                                let reg = vregs_alloc.alloc(used_inst_data.ty().into());
                                vacant_entry.insert(reg);
                            }
                        },
                        _ => {}
                    }
                }
            }
            bb_end_color.insert(bb_layout.bb(), acc_color);
        }

        let mut value_lowered_use = FxHashMap::default();
        // Branch arguments are VCode edge uses but are not necessarily present
        // in HIR's ordinary instruction use lists. Seed them before lowering so
        // a producer is retained regardless of block traversal order.
        for bb_layout in data.layout().basicblocks() {
            let terminator = *bb_layout
                .insts()
                .get_last()
                .expect("every lowered HIR block has a terminator");
            let args: &[HirInst] = match data.inst_data(terminator).kind() {
                InstKind::Branch(branch) => {
                    for &arg in branch.t_args().iter().chain(branch.f_args()) {
                        *value_lowered_use.entry(arg).or_insert(0) += 1;
                    }
                    continue;
                }
                InstKind::Jump(jump) => jump.args(),
                _ => &[],
            };
            for &arg in args {
                *value_lowered_use.entry(arg).or_insert(0) += 1;
            }
        }

        LowerContext {
            arena,
            vcode,
            vregs_alloc,
            reg_map,
            cur_inst: None,
            cur_block: None,
            inst_sunk: FxHashSet::default(),
            inst_color,
            cur_color: None,
            bb_end_color,
            value_lowered_use,
            ir_inst: Vec::new(),
        }
    }

    fn precompute_outgoing_arg_size(arena: ArenaContext<'_>) -> usize {
        let mut max_size = 0usize;
        for bb_layout in arena.f().layout().basicblocks() {
            for &inst in bb_layout.insts() {
                let InstKind::Call(call) = arena.inst_data(inst).kind() else {
                    continue;
                };
                let types: Vec<_> = call
                    .args()
                    .iter()
                    .map(|&arg| arena.inst_data(arg).ty().clone())
                    .collect();
                let (_, outgoing_size) = I::ABISpec::compute_call_arg_loc(&types);
                max_size = max_size.max(outgoing_size as usize);
            }
        }
        max_size
    }

    /// Lower the function.
    pub fn lower<B: LowerBackend<MInst = I>>(mut self) -> VCodeContainer<I> {
        let mut targets_buffer = smallvec![];
        let lowered_order: SmallVec<[LoweredBlock; 64]> = self
            .vcode
            .block_order()
            .lowered_order()
            .iter()
            .copied()
            .collect();
        debug!(target: "taki_mir::lower", "function={} lowered block order={lowered_order:?}", self.arena.f().name());

        for (block_index, lb) in lowered_order.iter().enumerate().rev() {
            let block_index = MirBlockIndex::new(block_index);
            debug!(target: "taki_mir::lower", "function={} lowering block={} descriptor={lb:?}", self.arena.f().name(), block_index.index());

            if let Some(bb) = lb.orig_block() {
                self.cur_block = Some(bb);
                if let Some(branch_inst) =
                    self.collect_branch_and_targets(block_index, &mut targets_buffer)
                {
                    self.lower_branch::<B>(branch_inst, block_index, &targets_buffer);
                    self.finish_ir_inst();
                }
            } else {
                let &[succ] = self.vcode.block_order().succ_indices(block_index).1 else {
                    unreachable!("edge blocks must have exactly one successor")
                };
                B::emit_long_jump(&mut self, succ);
                self.finish_ir_inst();
                self.lower_branch_blockparam_args_move(block_index);
                self.finish_ir_inst();
            }

            if let Some(bb) = lb.orig_block() {
                self.lower_block::<B>(bb);
                self.process_block_param(bb);
            }

            // Entry block: the backend lowers the once-per-call ABI argument
            // setup here. Entry block parameters are materialized as arg-copy
            // vregs (see `process_block_param`), never as live-in block params.
            if block_index.index() == 0 {
                self.gen_arg_setup();
                self.finish_ir_inst();
            }

            self.finish_bb();
            self.cur_block = None;
        }

        let vcode = self.vcode.build(self.vregs_alloc);
        debug!(target: "taki_mir::lower", "function={} finalized VCode: blocks={}, instructions={}", self.arena.f().name(), vcode.num_blocks(), vcode.num_insts());

        vcode
    }

    fn gen_arg_setup(&mut self) {
        let Some(_entry_bb) = self.arena.f().layout().entry_bb() else {
            panic!("function declaration should not be lowered");
        };

        let params: Vec<_> = self
            .arena
            .program
            .func_data(self.arena.curr_func.unwrap())
            .params()
            .iter()
            .copied()
            .enumerate()
            .collect();

        // Pre-allocate spill slots so gen_copy_arg_to_reg emits only loads.
        self.vcode.vcode.abi.prealloc_reg_arg_spills();

        // Phase 1: Emit loads for all parameters (in .rev() order so that
        // register loads come last in VCode, first after reverse_and_finalize).
        for (i, param) in params.into_iter().rev() {
            // A function parameter may have no ordinary HIR users yet still
            // feed the entry block parameters via the prologue edge (tracked
            // by `value_lowered_use`); keep such parameters live.
            if !self.is_value_needed(param) {
                continue;
            }
            for inst in self
                .vcode
                .vcode
                .abi
                .gen_copy_arg_to_reg(i, self.reg_map[&param])
            {
                self.emit(inst);
            }
            self.finish_ir_inst();
        }

        // Phase 2: Emit register-arg stores at the END of the VCode block.
        // After reverse_and_finalize they become the FIRST instructions,
        // saving arg registers before any loads can clobber them.
        for inst in self.vcode.vcode.abi.gen_store_reg_args_to_stack() {
            self.emit(inst);
        }
        self.finish_ir_inst();
    }

    /// Lower the branch instruction at the end of each basic block.
    /// - Set current instruction
    /// - Use backend to lower it
    /// - Copy the block parameter
    fn lower_branch<B: LowerBackend<MInst = I>>(
        &mut self,
        branch: HirInst,
        block: MirBlockIndex,
        target: &[MirBlockIndex],
    ) {
        trace!("to lower the branch: {:?}, block: {:?}", branch, block);
        self.cur_inst = Some(branch);

        B::lower_branch(self, branch, target);

        self.finish_ir_inst();

        self.lower_branch_blockparam_args_move(block);
    }

    /// Lower the move of basicblock parameter.
    /// Explicitly named the function for readability.
    /// Typically it will insert a copy for all the parameters,
    /// and the regalloc algorithm will find out how to achieve parallel copy.
    fn lower_branch_blockparam_args_move(&mut self, block: MirBlockIndex) {
        let mut buffer = smallvec![];

        for succ_idx in 0..self.vcode.block_order().succ_indices(block).1.len() {
            buffer.clear();
            let (block, regs) = self.collect_outgoing_block_args(block, succ_idx, &mut buffer);
            self.vcode.add_succ(block, regs);
        }
    }

    fn collect_branch_and_targets(
        &self,
        block: MirBlockIndex,
        target: &mut SmallVec<[MirBlockIndex; 2]>,
    ) -> Option<HirInst> {
        target.clear();
        let (opt_inst, succs) = self.vcode.block_order().succ_indices(block);
        target.extend(succs.iter().copied());
        opt_inst
    }

    fn collect_outgoing_block_args<'a>(
        &mut self,
        block: MirBlockIndex,
        succ_idx: usize,
        buffer: &'a mut SmallVec<[Reg; 16]>,
    ) -> (MirBlockIndex, &'a [Reg]) {
        let block_order = self.vcode.block_order();
        let (_, succs) = block_order.succ_indices(block);
        assert!(succ_idx < succs.len(), "successor index must be valid");
        let succ = succs[succ_idx];
        let this_lb = block_order.lowered_order()[block.index()];
        let succ_lb = block_order.lowered_order()[succ.index()];
        let (branch_inst, succ_idx) = match (this_lb, succ_lb) {
            (
                LoweredBlock::Orig { block: pred },
                LoweredBlock::Edge {
                    pred: edge_pred,
                    succ_idx: edge_succ_idx,
                    ..
                },
            ) => {
                assert_eq!(pred, edge_pred, "edge must be owned by its predecessor");
                assert_eq!(
                    succ_idx, edge_succ_idx as usize,
                    "edge must retain its source successor index"
                );
                return (succ, &[]);
            }
            (LoweredBlock::Edge { .. }, LoweredBlock::Edge { .. }) => {
                unreachable!("edge blocks must not target edge blocks")
            }
            (
                LoweredBlock::Edge {
                    pred,
                    succ: edge_succ,
                    succ_idx: source_succ_idx,
                },
                LoweredBlock::Orig { block: target },
            ) => {
                assert_eq!(edge_succ, target, "edge must target its recorded successor");
                assert_eq!(succ_idx, 0, "edge block must have one successor");
                let branch = *self
                    .arena
                    .f()
                    .layout()
                    .basicblock(pred)
                    .insts()
                    .get_last()
                    .unwrap();
                (branch, source_succ_idx as usize)
            }
            (LoweredBlock::Orig { block: orig }, _) => {
                let branch = *self
                    .arena
                    .f()
                    .layout()
                    .basicblock(orig)
                    .insts()
                    .get_last()
                    .unwrap();
                (branch, succ_idx)
            }
        };

        let block_param = self.arena.inst_data(branch_inst);
        // Magic match
        let args: SmallVec<[HirInst; 16]> = match block_param.kind() {
            InstKind::Branch(branch) => match succ_idx {
                0 => {
                    assert_eq!(branch.t_target(), lowered_hir_block(succ_lb));
                    branch.t_args().iter().copied().collect()
                }
                1 => {
                    assert_eq!(branch.f_target(), lowered_hir_block(succ_lb));
                    branch.f_args().iter().copied().collect()
                }
                _ => unreachable!(),
            },
            InstKind::Jump(jump) => {
                assert_eq!(succ_idx, 0, "jump has exactly one successor");
                assert_eq!(jump.target(), lowered_hir_block(succ_lb));
                jump.args().iter().copied().collect()
            }
            _ => unreachable!(),
        };

        let params: SmallVec<[HirInst; 16]> = self
            .arena
            .bb_data(lowered_hir_block(succ_lb))
            .params()
            .iter()
            .copied()
            .collect();
        assert_eq!(
            args.len(),
            params.len(),
            "edge arguments must match block parameters"
        );
        for (&arg, &param) in args.iter().zip(params.iter()) {
            assert_eq!(
                self.arena.inst_data(arg).ty(),
                self.arena.inst_data(param).ty(),
                "edge argument type must match block parameter type"
            );
        }

        for (arg_idx, arg) in args.into_iter().enumerate() {
            // INFO: inline method of [put_value_in_reg]
            // I don't know what the fuck the borrow checker is doing.
            // Partial borrow when?
            let reg = {
                let inst = arg;
                let uses = self.value_lowered_use.entry(inst).or_insert(0);
                *uses += 1;
                assert!(!self.inst_sunk.contains(&inst));
                let reg = *self.reg_map.entry(inst).or_insert_with(|| {
                    self.vregs_alloc
                        .alloc(self.arena.inst_data(inst).ty().into())
                });
                self.rematerialize_if_needed(inst, reg)
            };
            let param_reg = self.reg_map[&params[arg_idx]];
            assert_eq!(
                reg.class(),
                param_reg.class(),
                "edge argument and block parameter must use compatible register classes"
            );
            buffer.push(reg);
        }
        (succ, &buffer[..])
    }

    fn lower_block<B: LowerBackend<MInst = I>>(&mut self, block: HirBasicBlock) {
        self.cur_block = Some(block);
        self.cur_color = Some(self.bb_end_color[&block]);
        for &inst in self
            .arena
            .program
            .func_data(self.arena.curr_func.unwrap())
            .layout()
            .basicblock(block)
            .insts()
            .iter()
            .rev()
        {
            if self.is_inst_sunk(inst) {
                continue;
            }
            let side_effect = self.arena.has_side_effect_when_lowering(inst);

            if side_effect {
                let Some(&color) = self.inst_color.get(&inst) else {
                    unreachable!("side effect instruction is recorded in color map.")
                };
                self.cur_color = Some(color);
            }

            // Branches are selected before the reverse block walk. Returns
            // have no CFG successors and are lowered as ordinary root instructions.
            if self.arena.is_branch(inst) {
                continue;
            }

            self.cur_inst = Some(inst);

            let value_needed = self.is_value_needed(inst);

            trace!(target: "taki_mir::lower",
                "function={} block={block:?} HIR inst={inst:?} side_effect={} value_needed={}",
                self.arena.f().name(),
                side_effect,
                value_needed
            );

            if side_effect || value_needed {
                let output = B::lower(self, inst);
                self.bind_lowered_output(inst, output);
            }

            self.finish_ir_inst();

            self.cur_color = None;
        }
        self.cur_block = None;
    }

    fn is_inst_sunk(&self, inst: HirInst) -> bool {
        self.inst_sunk.contains(&inst)
    }

    fn is_value_needed(&self, inst: HirInst) -> bool {
        self.value_lowered_use.get(&inst).copied().unwrap_or(0) > 0
            || !self.arena.inst_data(inst).used_by().is_empty()
    }

    fn finish_ir_inst(&mut self) {
        for inst in self.ir_inst.drain(..).rev() {
            self.vcode.push(inst);
        }
    }

    fn process_block_param(&mut self, block: HirBasicBlock) {
        // The entry block's parameters are the function arguments. They are
        // materialized once per call by `gen_arg_setup` (as arg-copy vregs),
        // not by the blockparam/phi machinery, so they must NOT be registered
        // as live-in block parameters (the entry has no CFG predecessor and
        // the register allocator forbids entry block parameters).
        let is_entry = self
            .arena
            .f()
            .layout()
            .entry_bb()
            .is_some_and(|e| e.bb() == block);
        if is_entry {
            return;
        }
        for param in self.arena.bb_data(block).params() {
            let vreg = self.reg_map[param].to_virtual_reg().unwrap();
            self.vcode.add_block_param(vreg);
        }
    }

    pub fn put_value_in_reg(&mut self, inst: HirInst) -> Reg {
        *self.value_lowered_use.entry(inst).or_insert(0) += 1;
        assert!(!self.inst_sunk.contains(&inst));
        let reg = *self.reg_map.entry(inst).or_insert_with(|| {
            self.vregs_alloc
                .alloc(self.arena.inst_data(inst).ty().into())
        });
        self.rematerialize_if_needed(inst, reg)
    }

    /// Return the virtual register preallocated for an HIR instruction result.
    pub fn result_reg(&self, inst: HirInst) -> Reg {
        let ty = self.arena.inst_data(inst).ty();
        assert!(
            !ty.is_unit(),
            "unit instructions do not have result registers"
        );
        let reg = *self
            .reg_map
            .get(&inst)
            .expect("value-producing instruction must have a preallocated result register");
        assert!(reg.is_virtual(), "HIR result must use a virtual register");
        reg
    }

    fn bind_lowered_output(&mut self, inst: HirInst, output: LoweredOutput) {
        let ty = self.arena.inst_data(inst).ty();
        match (ty.is_unit(), output) {
            (true, LoweredOutput::None) => {}
            (false, LoweredOutput::Value(selected)) => {
                let result = self.result_reg(inst);
                if result != selected {
                    self.vregs_alloc.set_reg_alias(result, selected);
                }
            }
            (true, LoweredOutput::Value(_)) => self.lowering_panic(
                "generic instruction lowering",
                "unit instruction returned a result register",
                None,
                Some(ty),
            ),
            (false, LoweredOutput::None) => self.lowering_panic(
                "generic instruction lowering",
                "value-producing instruction returned no result register",
                None,
                Some(ty),
            ),
        }
    }

    /// Mark a pure producer as consumed directly by `consumer`.
    ///
    /// Lowering walks HIR in reverse, so a selected consumer can emit an
    /// encoding which names its producer's inputs directly. This is only
    /// sound when the HIR use graph says that `consumer` is the producer's
    /// sole user and the producer has not otherwise been lowered.
    pub fn sink_pure_single_use_producer(&mut self, producer: HirInst, consumer: HirInst) -> bool {
        if !self.can_sink_pure_single_use_producer(producer, consumer, consumer) {
            return false;
        }
        self.inst_sunk.insert(producer)
    }

    /// Atomically mark a two-producer chain consumed while lowering `root`.
    pub fn sink_pure_single_use_chain(
        &mut self,
        producer: HirInst,
        intermediate: HirInst,
        root: HirInst,
    ) -> bool {
        if !self.can_sink_pure_single_use_producer(producer, intermediate, root)
            || !self.can_sink_pure_single_use_producer(intermediate, root, root)
        {
            return false;
        }
        self.inst_sunk.insert(producer);
        self.inst_sunk.insert(intermediate);
        true
    }

    fn can_sink_pure_single_use_producer(
        &self,
        producer: HirInst,
        consumer: HirInst,
        root: HirInst,
    ) -> bool {
        if producer == consumer
            || self.cur_inst != Some(root)
            || self.inst_sunk.contains(&producer)
            || self.value_lowered_use.contains_key(&producer)
            || self.arena.is_terminator(producer)
            || self.arena.has_side_effect_when_lowering(producer)
            || !matches!(
                self.arena.inst_data(producer).kind(),
                InstKind::Binary(_) | InstKind::Cast(_) | InstKind::GetElemPtr(_) | InstKind::Alloc
            )
        {
            return false;
        }

        let users = self.arena.inst_data(producer).used_by();
        if users.len() != 1 || !users.contains(&consumer) {
            return false;
        }
        true
    }

    fn rematerialize_if_needed(&mut self, inst: HirInst, reg: Reg) -> Reg {
        enum Remat {
            Int(i32),
            Float(u32),
            Global,
            Undef,
        }

        let remat = match self.arena.inst_data(inst).kind() {
            InstKind::Integer(i) => Some(Remat::Int(i.value())),
            InstKind::Float(f) => Some(Remat::Float(f.value().to_bits())),
            InstKind::GlobalAlloc(..) => Some(Remat::Global),
            InstKind::Undef => Some(Remat::Undef),
            _ => None,
        };
        match remat {
            Some(Remat::Int(value)) => {
                let reg = self.alloc_tmp(HirType::get_i32());
                self.emit(<I::ABISpec as ABIMachineSpec>::gen_load_imm(
                    Writable::from_reg(reg),
                    value as u32 as u64,
                    I32,
                ));
                reg
            }
            Some(Remat::Float(bits)) => {
                let tmp = self.alloc_tmp(HirType::get_i32());
                let reg = self.alloc_tmp(HirType::get_f32());
                self.emit(<I::ABISpec as ABIMachineSpec>::gen_load_imm(
                    Writable::from_reg(tmp),
                    bits as u64,
                    I32,
                ));
                self.emit(<I::ABISpec as ABIMachineSpec>::gen_move(tmp, reg, F32));
                reg
            }
            Some(Remat::Global) => {
                let reg = self.alloc_tmp(self.arena.inst_data(inst).ty().clone());
                self.emit(<I::ABISpec as ABIMachineSpec>::gen_load_addr(
                    Writable::from_reg(reg),
                    inst,
                ));
                reg
            }
            Some(Remat::Undef) => {
                let ty = self.arena.inst_data(inst).ty().clone();
                let reg = self.alloc_tmp(ty.clone());
                match ty.kind() {
                    HirTypeKind::Float32 => {
                        let bits = self.alloc_tmp(HirType::get_i32());
                        self.emit(<I::ABISpec as ABIMachineSpec>::gen_load_imm(
                            Writable::from_reg(bits),
                            0,
                            I32,
                        ));
                        self.emit(<I::ABISpec as ABIMachineSpec>::gen_move(bits, reg, F32));
                    }
                    HirTypeKind::Int32 | HirTypeKind::Pointer(_) => {
                        self.emit(<I::ABISpec as ABIMachineSpec>::gen_load_imm(
                            Writable::from_reg(reg),
                            0,
                            ty.into(),
                        ));
                    }
                    kind => unreachable!("unsupported undefined value type {kind:?}"),
                }
                reg
            }
            None => reg,
        }
    }

    pub fn alloc_tmp(&mut self, ty: HirType) -> Reg {
        self.vregs_alloc.alloc(ty.into())
    }

    /// Allocate (or retrieve) a stack slot owned by an HIR `Alloc`.
    ///
    /// Backends must use this rather than reaching through the VCode builder so
    /// that stack-frame ownership remains part of the generic lowering API.
    pub fn alloc_stackslot_or_get(&mut self, alloc: HirInst, ty: HirType) -> u32 {
        self.vcode.vcode.abi.alloc_stackslot_or_get(alloc, ty)
    }

    pub fn emit(&mut self, mach_inst: I) {
        trace!(target: "taki_mir::lower", "function={} HIR inst={:?} selected MInst={mach_inst:?}", self.arena.f().name(), self.cur_inst);
        self.ir_inst.push(mach_inst);
    }

    pub fn lowering_panic(
        &self,
        phase: &'static str,
        reason: impl core::fmt::Display,
        source_type: Option<&HirType>,
        target_type: Option<&HirType>,
    ) -> ! {
        let inst = self
            .cur_inst
            .expect("lowering failure requires a current HIR instruction");
        let block = self
            .cur_block
            .map(|block| self.arena.f().bb_data(block).name())
            .unwrap_or("<none>");
        let source_type = source_type
            .map(|ty| format!("{ty:?}"))
            .unwrap_or_else(|| "<none>".to_owned());
        let target_type = target_type
            .map(|ty| format!("{ty:?}"))
            .unwrap_or_else(|| "<none>".to_owned());

        panic!(
            "code generation invariant failed in function `{}`, block `{block}`, phase `{phase}`, HIR instruction {inst:?}: {reason} (source type {source_type}, target type {target_type})",
            self.arena.f().name()
        )
    }

    fn finish_bb(&mut self) {
        self.vcode.end_bb()
    }
}

fn lowered_hir_block(lb: LoweredBlock) -> HirBasicBlock {
    lb.succ_block()
        .or_else(|| lb.orig_block())
        .expect("lowered successor has an HIR block")
}

#[cfg(test)]
mod tests {
    use super::{GepDynamicTerm, analyze_gep};
    use crate::prelude::ArenaContext;
    use raana_ir::ir::{InstKind, Program, Type, arena::Arena, builder_trait::*};

    #[test]
    fn analyzes_mixed_multidimensional_gep() {
        let mut program = Program::new();
        let function = program.new_function(
            Type::get_unit(),
            "gep_analysis".to_owned(),
            vec![Type::get_i32()],
        );
        let data = program.func_data_mut(function);
        data.add_entry_block();
        let dynamic = data.params()[0];
        let inner = Type::get_array(Type::get_i32(), 7);
        let outer = Type::get_array(inner, 5);
        let base = data.new_local_inst().alloc(outer);
        let zero = data.new_local_inst().integer(0);
        let three = data.new_local_inst().integer(3);
        let gep = data
            .new_local_inst()
            .get_elem_ptr(base, vec![zero, dynamic, three]);

        let arena = ArenaContext {
            program: &program,
            curr_func: Some(function),
        };
        let InstKind::GetElemPtr(gep_data) = arena.inst_data(gep).kind() else {
            panic!("expected GEP instruction");
        };
        let analysis = analyze_gep(arena, gep, gep_data).unwrap();

        assert_eq!(analysis.base, base);
        assert_eq!(analysis.constant_offset, 12);
        assert_eq!(
            analysis.dynamic_terms.as_slice(),
            &[GepDynamicTerm {
                index: dynamic,
                stride: 28,
            }]
        );
    }
}
