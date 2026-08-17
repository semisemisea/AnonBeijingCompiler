use log::{debug, trace};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::{SmallVec, smallvec};

use crate::abi::{ABIMachineSpec, ArgSlot, CalleeABI};
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
            HirTypeKind::Vector(element, lanes) => checked_size(element)?.checked_mul(*lanes),
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

/// Fold a constant-only GEP into a `(base, constant_offset)` pair consumed by a
/// load/store, when the GEP's sole user is `consumer` and the offset satisfies
/// `offset_ok`.
///
/// On success the GEP is sunk via `sink_pure_single_use_producer`, so it is no
/// longer materialized on its own: the consumer's addressing mode can name the
/// base register and the folded constant offset directly. `offset_ok` runs
/// before sinking, so a caller that cannot encode the offset in its addressing
/// mode can safely fall back to `put_value_in_reg(gep)` without tripping the
/// sunk-instruction assertion.
pub fn fold_gep_constant_offset<I: VCodeInst>(
    ctx: &mut LowerContext<'_, I>,
    arena: ArenaContext<'_>,
    gep_inst: HirInst,
    consumer: HirInst,
    offset_ok: impl FnOnce(i64) -> bool,
) -> Option<(Reg, i64)> {
    let gep = match arena.inst_data(gep_inst).kind() {
        InstKind::GetElemPtr(gep) => gep,
        _ => return None,
    };
    let analysis = analyze_gep(arena, gep_inst, gep).ok()?;
    if !analysis.dynamic_terms.is_empty() || !offset_ok(analysis.constant_offset) {
        return None;
    }
    if !ctx.sink_pure_single_use_producer(gep_inst, consumer) {
        return None;
    }
    let base = ctx.put_value_in_reg(analysis.base);
    Some((base, analysis.constant_offset))
}

/// Fold a constant-only continuation GEP into a load/store addressing mode
/// even when the GEP is shared by multiple memory ops.
///
/// The 2x-unrolled vectorizer emits one continuation GEP (`getelemptr %base,
/// 4`) consumed by both the unrolled vector load and its paired vector store.
/// Every user of the GEP is a load or store that folds it into its own
/// addressing mode, so the producer is never materialized. The sink is
/// idempotent: the first memory consumer marks the GEP sunk, later consumers
/// still fold it (the base register is materialized once via `put_value_in_reg`).
pub fn fold_gep_constant_offset_shared<I: VCodeInst>(
    ctx: &mut LowerContext<'_, I>,
    arena: ArenaContext<'_>,
    gep_inst: HirInst,
    consumer: HirInst,
    offset_ok: impl FnOnce(i64) -> bool,
) -> Option<(Reg, i64)> {
    let gep = match arena.inst_data(gep_inst).kind() {
        InstKind::GetElemPtr(gep) => gep,
        _ => return None,
    };
    let analysis = analyze_gep(arena, gep_inst, gep).ok()?;
    if !analysis.dynamic_terms.is_empty() || !offset_ok(analysis.constant_offset) {
        return None;
    }
    if !ctx.sink_pure_shared_memory_gep(gep_inst, consumer) {
        return None;
    }
    let base = ctx.put_value_in_reg(analysis.base);
    Some((base, analysis.constant_offset))
}

/// Sink a single-use GEP and return its full address decomposition.
///
/// Unlike `fold_gep_constant_offset`, the `foldable` predicate sees the whole
/// `GepAddress` (dynamic terms included), so the caller decides how to exploit
/// the decomposition (e.g. AArch64 extended-register addressing for one
/// dynamic term). On success the GEP is sunk and must not be materialized;
/// every failure path runs before sinking, so the caller can safely fall back
/// to `put_value_in_reg(gep)`.
pub fn sink_gep_into_address<I: VCodeInst>(
    ctx: &mut LowerContext<'_, I>,
    arena: ArenaContext<'_>,
    gep_inst: HirInst,
    consumer: HirInst,
    foldable: impl FnOnce(&GepAddress) -> bool,
) -> Option<GepAddress> {
    let gep = match arena.inst_data(gep_inst).kind() {
        InstKind::GetElemPtr(gep) => gep,
        _ => return None,
    };
    let analysis = analyze_gep(arena, gep_inst, gep).ok()?;
    if !foldable(&analysis) {
        return None;
    }
    if !ctx.sink_pure_single_use_producer(gep_inst, consumer) {
        return None;
    }
    Some(analysis)
}

/// A lowering context for a single function
pub struct LowerContext<'prog, I: VCodeInst> {
    /// Arena to get everything you need about HirFunction
    pub arena: ArenaContext<'prog>,

    /// The VCode Container we currently building.
    pub(crate) vcode: VCodeBuilder<I>,

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

    /// BB-scoped constant sharing: constant value -> shared vreg, built during
    /// the current block's lowering. The materialization is deferred to the
    /// end of the block (see `emit_block_const_shared`), so the def lands at
    /// the block start after the stream reversal; same-value constants in one
    /// block then share a single movz/movk chain instead of one per use.
    block_const_shared: FxHashMap<i64, Reg>,

    /// Constant values used by two or more operands in the current block.
    /// Only these participate in `block_const_shared`; single-use constants
    /// keep their per-use materialization so their live range is not
    /// extended to the block start (register-pressure guard).
    block_const_multi: FxHashSet<i64>,

    /// Loop-scoped constant sharing: (loop_index, value) -> shared vreg.
    /// The materialization is deferred to the loop preheader (see
    /// `emit_loop_const_shared`); every use inside the loop body shares the
    /// vreg, so per-iteration movz/movk chains disappear entirely.
    loop_const_shared: FxHashMap<(usize, i64), Reg>,

    /// (loop_index, value) pairs used by two or more operands across the
    /// loop body (loop-level sharing gate, mirrors `block_const_multi`).
    loop_const_multi: FxHashSet<(usize, i64)>,

    /// Preheader block -> constant chains to emit when that block is
    /// lowered. Each entry is (value, shared vreg).
    loop_emissions: FxHashMap<HirBasicBlock, Vec<(i64, Reg)>>,

    /// Innermost loop containing the block currently being lowered
    /// (`None` for edge blocks and blocks outside any loop).
    current_loop: Option<usize>,

    /// Loop structure of the function being lowered (snapshot: lowering
    /// does not mutate the IR, so the analysis stays valid).
    loop_analysis: raana_ir::opt::prelude::loop_analysis::LoopAnalysis,

    /// CFG of the function, needed by `Loop::get_preheader`.
    loop_cfg: raana_ir::opt::utils::cfg::CFG,

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
    type CodegenConfig: Default;

    fn lower(ctx: &mut LowerContext<Self::MInst>, inst: HirInst) -> LoweredOutput;

    fn lower_branch(ctx: &mut LowerContext<Self::MInst>, inst: HirInst, target: &[MirBlockIndex]);

    fn data_section_directive() -> &'static str;

    fn bss_section_directive() -> &'static str;

    fn text_section_directive() -> &'static str;

    fn global_directive() -> &'static str;

    fn word_directive() -> &'static str;

    fn zero_directive() -> &'static str;

    /// Alignment directive emitted before every global, or `""` for none.
    /// AArch64 returns `.balign 16` so vector loads/stores (`ldr/str q`) over
    /// globals never fault or split; the assembler pads each label to 16 bytes.
    fn balign_directive() -> &'static str {
        ""
    }

    fn preg_name(preg: PReg) -> &'static str;

    fn format_block_label(lb: &LoweredBlock, func_data: &HirFunctionData) -> String;

    fn emit_long_jump(ctx: &mut LowerContext<Self::MInst>, target: MirBlockIndex);

    /// Optional target-private assembly appended after all generated functions.
    fn runtime_assembly(_program: &HirProgram) -> Option<String> {
        None
    }

    /// Build the MIR pass pipeline for this backend.
    ///
    /// Pre-RA passes run after `LowerContext::lower` produces the VCode and
    /// before register allocation. Post-RA passes run after frame layout and
    /// `finalize_for_emission`, immediately before assembly emission. The
    /// default returns an empty pipeline; backends override this to register
    /// peephole, scheduling, and other machine-code passes.
    fn mir_pipeline(_config: &Self::CodegenConfig) -> crate::passes::MIRPassPipeline<Self::MInst> {
        crate::passes::MIRPassPipeline::new()
    }

    /// Whether emission-time branch optimization (EmitBuffer simplification
    /// rules) is enabled for this config. `-O0` keeps the two-instruction
    /// branch form as an on/off differential baseline.
    fn branch_opt_enabled(_config: &Self::CodegenConfig) -> bool {
        false
    }

    /// Instruction lines of a veneer that reaches `target` for a branch of
    /// `kind` that fell out of range. Each line is one fixed-width
    /// instruction slot, spliced immediately after the branch (a block
    /// terminator, so no fallthrough can enter the veneer).
    fn veneer_lines(_kind: crate::emit_buffer::LabelKind, _target: &str) -> Vec<String> {
        unreachable!("backends with branch slots must provide veneer lines")
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
        let (loop_cfg, _, loop_analysis) =
            raana_ir::opt::prelude::loop_analysis::LoopAnalysis::new(arena.f());
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
            block_const_shared: FxHashMap::default(),
            block_const_multi: FxHashSet::default(),
            loop_const_shared: FxHashMap::default(),
            loop_const_multi: FxHashSet::default(),
            loop_emissions: FxHashMap::default(),
            current_loop: None,
            loop_analysis,
            loop_cfg,
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

        self.loop_const_multi = self.scan_loop_const_uses();

        for (block_index, lb) in lowered_order.iter().enumerate().rev() {
            let block_index = MirBlockIndex::new(block_index);
            debug!(target: "taki_mir::lower", "function={} lowering block={} descriptor={lb:?}", self.arena.f().name(), block_index.index());

            if let Some(bb) = lb.orig_block() {
                self.cur_block = Some(bb);
                self.current_loop = self.loop_analysis.min_loop_contain_index(bb);
                self.block_const_shared.clear();
                self.block_const_multi = self.scan_block_const_uses(bb);
                if let Some(branch_inst) =
                    self.collect_branch_and_targets(block_index, &mut targets_buffer)
                {
                    self.lower_branch::<B>(branch_inst, block_index, &targets_buffer);
                    self.finish_ir_inst();
                }
            } else {
                self.current_loop = None;
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

            // Emit loop-shared constant materializations when this block is
            // some loop's preheader (chains land at this block's start after
            // the stream reversal and dominate the whole loop body), then the
            // block's own shared constants.
            if let Some(bb) = lb.orig_block() {
                self.emit_loop_const_shared(bb);
            }
            self.emit_block_const_shared();

            // Entry block: the backend lowers the once-per-call ABI argument
            // setup here. Entry block parameters are materialized as arg-copy
            // vregs (see `process_block_param`), never as live-in block params.
            // Pushed last so that after the stream reversal the `Args` pseudo
            // lands before any hoisted constant materialization: those chains
            // reuse ABI argument registers (e.g. `li a0, 1`) that still hold
            // live parameters, and the register allocator only sees the
            // conflict when the parameter's live range starts first.
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

        // Phase 1: Emit incoming loads for stack arguments. Register arguments
        // are collected as `ArgPair`s and bound by the entry `Args` pseudo
        // below. (Iterating in `.rev()` keeps the per-instruction groups in
        // the same final forward order as the original two-phase scheme.)
        for (i, param) in params.into_iter().rev() {
            // A function parameter may have no ordinary HIR users yet still
            // feed the entry block parameters via the prologue edge (tracked
            // by `value_lowered_use`); keep such parameters live.
            if !self.is_value_needed(param) {
                if matches!(self.arg_slot(i), ArgSlot::Reg { .. }) {
                    self.vcode.vcode.abi.note_unused_register_arg();
                }
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

        // Phase 2: Package the collected register-argument bindings into the
        // entry `Args` pseudo. Emitted last so that after the reverse ordering
        // it becomes the first instruction of the entry block, before any stack
        // argument loads or ordinary entry instructions.
        if let Some(args) = self.vcode.vcode.abi.take_args() {
            self.emit(args);
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
        // BB-scoped constant sharing: an integer constant is only materialized
        // once per block, at the block start (see `emit_block_const_shared`).
        // Same-value uses share the vreg, so `movz`/`movk` chains are not
        // repeated per use. Only constants that actually reach this path are
        // shared; immediates folded into instructions never arrive here.
        if let InstKind::Integer(integer) = self.arena.inst_data(inst).kind() {
            let value = i64::from(integer.value());
            if let Some(shared) = self.loop_const_to_reg(value, value) {
                return shared;
            }
            if let Some(shared) = self.const_to_reg(value, value) {
                return shared;
            }
        }
        let reg = *self.reg_map.entry(inst).or_insert_with(|| {
            self.vregs_alloc
                .alloc(self.arena.inst_data(inst).ty().into())
        });
        self.rematerialize_if_needed(inst, reg)
    }

    /// BB-scoped constant sharing: return a shared vreg for `value` when
    /// `gate_on` is used by two or more operands in the current block (for
    /// ordinary constants `gate_on == value`; the division-magic path gates
    /// the magic multiplier on the divisor's operand count, because the magic
    /// value itself is not an IR operand). The materialization is deferred to
    /// the end of the block (see `emit_block_const_shared`), so the def lands
    /// at the block start after the stream reversal and the constant
    /// materializes once per block instead of once per use. `None` means the
    /// caller should keep its per-use materialization (single-use constants
    /// keep short live ranges, guarding register pressure).
    pub fn const_to_reg(&mut self, value: i64, gate_on: i64) -> Option<Reg> {
        if !self.block_const_multi.contains(&gate_on) {
            return None;
        }
        if let Some(&shared) = self.block_const_shared.get(&value) {
            return Some(shared);
        }
        let shared = self.alloc_tmp(HirType::get_i32());
        self.block_const_shared.insert(value, shared);
        Some(shared)
    }

    /// Count Integer-constant operands per value in `block`; values used by
    /// two or more operands are the sharing candidates for this block.
    fn scan_block_const_uses(&mut self, block: HirBasicBlock) -> FxHashSet<i64> {
        let mut counts: FxHashMap<i64, usize> = FxHashMap::default();
        let func = self.arena.program.func_data(
            self.arena
                .curr_func
                .expect("function is set during lowering"),
        );
        let insts = func.layout().basicblock(block).insts().to_vec();
        for inst in insts {
            for operand in self.arena.inst_data(*inst).inst_usage() {
                if let InstKind::Integer(integer) = self.arena.inst_data(operand).kind() {
                    *counts.entry(i64::from(integer.value())).or_default() += 1;
                }
            }
        }
        counts
            .into_iter()
            .filter(|&(_, count)| count >= 2)
            .map(|(value, _)| value)
            .collect()
    }

    /// Emit the deferred constant materializations recorded for the current
    /// block. Called at the end of the block's lowering so the shared vregs'
    /// defs land at the block start after the stream reversal.
    fn emit_block_const_shared(&mut self) {
        let entries: Vec<(i64, Reg)> = self.block_const_shared.drain().collect();
        for (value, reg) in entries {
            self.emit(<I::ABISpec as ABIMachineSpec>::gen_load_imm(
                Writable::from_reg(reg),
                value as u32 as u64,
                I32,
            ));
        }
        // Flush immediately: the emitted chain is the last code pushed for
        // this block, so the stream reversal places it at the block start.
        self.finish_ir_inst();
    }

    /// Count Integer-constant operands per (loop, value) across each loop's
    /// body (header included); pairs used by two or more operands become
    /// loop-level sharing candidates.
    fn scan_loop_const_uses(&mut self) -> FxHashSet<(usize, i64)> {
        let mut counts: FxHashMap<(usize, i64), usize> = FxHashMap::default();
        let func = self.arena.program.func_data(
            self.arena
                .curr_func
                .expect("function is set during lowering"),
        );
        for (index, loop_info) in self.loop_analysis.loops().iter().enumerate() {
            let mut blocks: Vec<HirBasicBlock> = loop_info.body().iter().copied().collect();
            blocks.push(loop_info.header());
            for block in blocks {
                let insts = func.layout().basicblock(block).insts().to_vec();
                for inst in insts {
                    for operand in self.arena.inst_data(*inst).inst_usage() {
                        if let InstKind::Integer(integer) = self.arena.inst_data(operand).kind() {
                            *counts
                                .entry((index, i64::from(integer.value())))
                                .or_default() += 1;
                        }
                    }
                }
            }
        }
        counts
            .into_iter()
            .filter(|&(_, count)| count >= 2)
            .map(|(key, _)| key)
            .collect()
    }

    /// Loop-scoped constant sharing: return a shared vreg for `value` when
    /// `gate_on` is used by two or more operands across the innermost loop
    /// containing the block being lowered (for ordinary constants
    /// `gate_on == value`; the division-magic path gates the magic
    /// multiplier on the divisor's operand count). The materialization is
    /// deferred to the loop preheader (see `emit_loop_const_shared`), so the
    /// chain executes once per loop entry instead of once per iteration.
    /// `None` means the caller should keep its per-use/block materialization
    /// (no loop, no preheader, or a single-use constant).
    pub fn loop_const_to_reg(&mut self, value: i64, gate_on: i64) -> Option<Reg> {
        let loop_index = self.current_loop?;
        if !self.loop_const_multi.contains(&(loop_index, gate_on)) {
            return None;
        }
        if let Some(&shared) = self.loop_const_shared.get(&(loop_index, value)) {
            return Some(shared);
        }
        let preheader = self.loop_analysis.loops()[loop_index].get_preheader(&self.loop_cfg)?;
        let shared = self.alloc_tmp(HirType::get_i32());
        self.loop_const_shared.insert((loop_index, value), shared);
        self.loop_emissions
            .entry(preheader)
            .or_default()
            .push((value, shared));
        Some(shared)
    }

    /// Emit the deferred loop-shared constant materializations recorded for
    /// `bb` (a loop preheader). Called at the end of the block's lowering;
    /// the chains land at the preheader's start after the stream reversal
    /// and dominate the whole loop body. `bb` is passed explicitly because
    /// `lower_block` clears `cur_block` before this point.
    fn emit_loop_const_shared(&mut self, bb: HirBasicBlock) {
        let Some(chains) = self.loop_emissions.remove(&bb) else {
            return;
        };
        for (value, reg) in chains {
            self.emit(<I::ABISpec as ABIMachineSpec>::gen_load_imm(
                Writable::from_reg(reg),
                value as u32 as u64,
                I32,
            ));
        }
        self.finish_ir_inst();
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

    /// Sink a constant-only continuation GEP whose every user is a load or
    /// store into each user's addressing mode.
    ///
    /// Unlike `sink_pure_single_use_producer`, the GEP may be shared by
    /// several memory ops: the 2x-unrolled vectorizer emits one continuation
    /// GEP (`getelemptr %base, 4`) consumed by both the unrolled vector load
    /// and its paired vector store. Every user folds the GEP into its own
    /// addressing mode, so the producer is never materialized on its own.
    /// The sink is idempotent: the first memory consumer marks the GEP sunk,
    /// and later consumers still fold it.
    pub fn sink_pure_shared_memory_gep(&mut self, producer: HirInst, consumer: HirInst) -> bool {
        if producer == consumer || self.cur_inst != Some(consumer) {
            return false;
        }
        if !matches!(
            self.arena.inst_data(producer).kind(),
            InstKind::GetElemPtr(_)
        ) {
            return false;
        }
        let users = self.arena.inst_data(producer).used_by();
        if users.is_empty()
            || !users
                .iter()
                .all(|u| matches!(self.arena.inst_data(*u).kind(), InstKind::Load(_) | InstKind::Store(_)))
        {
            return false;
        }
        if !users.contains(&consumer) {
            return false;
        }
        if self.value_lowered_use.contains_key(&producer)
            || self.arena.is_terminator(producer)
            || self.arena.has_side_effect_when_lowering(producer)
        {
            return false;
        }
        self.inst_sunk.insert(producer);
        true
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

    /// Atomically mark two producers consumed by the same intermediate, which
    /// is itself consumed by `root`. Used when a `band`/`bor` of two single-use
    /// comparisons is folded into a flag chain (ccmp) while lowering `root`.
    pub fn sink_pure_single_use_pair(
        &mut self,
        producer_a: HirInst,
        producer_b: HirInst,
        intermediate: HirInst,
        root: HirInst,
    ) -> bool {
        if !self.can_sink_pure_single_use_producer(producer_a, intermediate, root)
            || !self.can_sink_pure_single_use_producer(producer_b, intermediate, root)
            || !self.can_sink_pure_single_use_producer(intermediate, root, root)
        {
            return false;
        }
        self.inst_sunk.insert(producer_a);
        self.inst_sunk.insert(producer_b);
        self.inst_sunk.insert(intermediate);
        true
    }

    /// Atomically mark every `(producer, consumer)` edge in a pure,
    /// single-use expression tree as consumed by `root`.
    pub fn sink_pure_single_use_tree(
        &mut self,
        edges: &[(HirInst, HirInst)],
        root: HirInst,
    ) -> bool {
        if edges.iter().any(|&(producer, consumer)| {
            !self.can_sink_pure_single_use_producer(producer, consumer, root)
        }) {
            return false;
        }
        for &(producer, _) in edges {
            self.inst_sunk.insert(producer);
        }
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

    /// Mark this function as containing (tail) calls so that the ABI preserves
    /// the caller-save area / LR in the prologue.
    pub fn set_has_calls(&mut self) {
        self.vcode.vcode.abi.set_has_calls();
    }

    /// Record the outgoing-argument-area size needed by the largest call site
    /// in this function. Takes the max so multiple calls keep the area stable.
    pub fn set_outgoing_arg_size(&mut self, size: usize) {
        self.vcode.vcode.abi.set_outgoing_arg_size(size);
    }

    /// Look up the calling-convention slot (register or stack offset) for the
    /// `idx`-th incoming function parameter.
    pub fn arg_slot(&self, idx: usize) -> ArgSlot {
        self.vcode.vcode.abi.arg_slot(idx)
    }

    /// Return the lowered block that receives control on function entry.
    pub fn entry_block(&self) -> MirBlockIndex {
        let entry = self
            .arena
            .f()
            .layout()
            .entry_bb()
            .expect("lowered function must have an entry block")
            .bb();
        self.vcode
            .block_order()
            .lowered_index_for_block(entry)
            .expect("entry block must be present in lowering order")
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
