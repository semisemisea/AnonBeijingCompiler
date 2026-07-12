use log::trace;
use rustc_hash::FxHashMap;
use smallvec::{SmallVec, smallvec};

use crate::abi::CalleeABI;
use crate::block_order::{BlockLoweringOrder, LoweredBlock, MirBlockIndex};
use crate::prelude::*;
use crate::register::{Reg, VRegAllocator};
use crate::vcode::{VCodeBuilder, VCodeContainer, VCodeInst};

pub enum ValueUseCount {
    Unused = 0,
    Once = 1,
    Multiple = 2,
}

/// A lowering context for a single function
pub struct LowerContext<'prog, I: VCodeInst> {
    /// Arena to get everything you need about HirFunction
    arena: ArenaContext<'prog>,

    /// The VCode Container we currently building.
    vcode: VCodeBuilder<I>,

    /// A virtural register allocator.
    /// Allocate each instruction a register.
    vregs_alloc: VRegAllocator<I>,

    /// A map for instruction and register.
    /// INFO: Currently should be fixed map,
    /// since we allocate all the register in `new` function
    reg_map: FxHashMap<HirInst, Reg>,

    /// Current HirInstruction about to lower
    cur_inst: Option<HirInst>,

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

    fn lower(&self, ctx: &mut LowerContext<Self::MInst>, inst: HirInst);

    fn lower_branch(
        &self,
        ctx: &mut LowerContext<Self::MInst>,
        inst: HirInst,
        target: &[MirBlockIndex],
    );
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
                    let used_inst_data = data.inst_data(used);
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

        LowerContext {
            arena,
            vcode,
            vregs_alloc,
            reg_map,
            cur_inst: None,
            inst_sunk: FxHashSet::default(),
            inst_color,
            cur_color: None,
            bb_end_color,
            value_lowered_use: FxHashMap::default(),
            ir_inst: Vec::new(),
        }
    }

    /// Lower the function.
    fn lower<B: LowerBackend<MInst = I>>(mut self, backend: &B) -> VCodeContainer<I> {
        let mut targets_buffer = smallvec![];
        let lowered_order: SmallVec<[LoweredBlock; 64]> = self
            .vcode
            .block_order()
            .lowered_order()
            .iter()
            .copied()
            .collect();

        for (block_index, lb) in lowered_order.iter().enumerate().rev() {
            let block_index = MirBlockIndex::new(block_index);

            if let Some(_bb) = lb.orig_block() {
                if let Some(branch_inst) =
                    self.collect_branch_and_targets(block_index, &mut targets_buffer)
                {
                    self.lower_branch(backend, branch_inst, block_index, &targets_buffer);
                    self.finish_ir_inst();
                } else {
                    let succ = self.vcode.block_order().succ_indices(block_index).1[0];
                    self.emit(I::gen_jump(succ));
                    self.finish_ir_inst();
                    self.lower_branch_blockparam_args_move(block_index);
                }
            }

            if let Some(bb) = lb.orig_block() {
                self.lower_block(backend, bb);
            }

            // Entry block
            if block_index.index() == 0 {
                self.gen_arg_setup();
                self.finish_ir_inst();
            }

            self.finish_bb();
        }

        let vcode = self.vcode.build(self.vregs_alloc);

        vcode
    }

    fn gen_arg_setup(&mut self) {
        let Some(_entry_bb) = self.arena.f().layout().entry_bb() else {
            // INFO: Since we treat function without entry block as declaration
            panic!("function declaration should not be lowered");
        };
        for (i, &param) in self
            .arena
            .program
            .func_data(self.arena.curr_func.unwrap())
            .params()
            .iter()
            .enumerate()
        {
            if self.arena.inst_data(param).used_by().is_empty() {
                continue;
            }
            for inst in self
                .vcode
                .vcode
                .abi
                .gen_copy_arg_to_reg(i, self.reg_map[&param])
                .into_iter()
            {
                self.emit(inst);
            }

            self.finish_ir_inst();

            for inst in self.vcode.vcode.abi.take_args() {
                self.emit(inst);
            }
        }
    }

    /// Lower the branch instruction at the end of each basic block.
    /// - Set current instruction
    /// - Use backend to lower it
    /// - Copy the block parameter
    fn lower_branch<B: LowerBackend<MInst = I>>(
        &mut self,
        backend: &B,
        branch: HirInst,
        block: MirBlockIndex,
        target: &[MirBlockIndex],
    ) {
        trace!("to lower the branch: {:?}, block: {:?}", branch, block);
        self.cur_inst = Some(branch);

        backend.lower_branch(self, branch, target);

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
        let succ = succs[succ_idx];
        let this_lb = block_order.lowered_order()[block.index()];
        let succ_lb = block_order.lowered_order()[succ.index()];
        let (branch_inst, succ_idx) = match (this_lb, succ_lb) {
            (_, LoweredBlock::Edge { .. }) => {
                return (succ, &[]);
            }
            (LoweredBlock::Edge { pred, succ_idx, .. }, _) => {
                let branch = *self
                    .arena
                    .f()
                    .layout()
                    .basicblock(pred)
                    .insts()
                    .get_last()
                    .unwrap();
                (branch, succ_idx as usize)
            }
            (this, _) => {
                let block = this.orig_block().unwrap();
                let branch = *self
                    .arena
                    .f()
                    .layout()
                    .basicblock(block)
                    .insts()
                    .get_last()
                    .unwrap();
                (branch, succ_idx)
            }
        };

        let block_param = self.arena.inst_data(branch_inst);
        // Magic match
        let args = match block_param.kind() {
            InstKind::Branch(branch) => match succ_idx {
                0 => branch.t_args(),
                1 => branch.f_args(),
                _ => unreachable!(),
            },
            InstKind::Jump(jump) => jump.args(),
            _ => unreachable!(),
        };

        for &arg in args {
            // INFO: inline method of [put_value_in_reg]
            // I don't know what the fuck the borrow checker is doing.
            // Partial borrow when?
            let reg = {
                let inst = arg;
                *self.value_lowered_use.get_mut(&inst).unwrap() += 1;
                assert!(!self.inst_sunk.contains(&inst));
                let reg = self.reg_map[&inst];
                reg
            };
            buffer.push(reg);
        }
        (succ, &buffer[..])
    }

    fn lower_block<B: LowerBackend<MInst = I>>(&mut self, backend: &B, block: HirBasicBlock) {
        self.cur_color = Some(self.bb_end_color[&block]);
        for &inst in self
            .arena
            .program
            .func_data(self.arena.curr_func.unwrap())
            .layout()
            .basicblock(block)
            .insts()
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

            if self.arena.is_terminator(inst) {
                continue;
            }

            self.cur_inst = Some(inst);

            let value_needed = self.is_value_needed(inst);

            trace!(
                "to lower the instruction: {:?}, side-effect: {}, value_needed: {}",
                inst, side_effect, value_needed
            );

            if side_effect || value_needed {
                backend.lower(self, inst);
            }

            self.finish_ir_inst();

            self.cur_color = None;
        }
    }

    fn is_inst_sunk(&self, inst: HirInst) -> bool {
        self.inst_sunk.contains(&inst)
    }

    fn is_value_needed(&self, inst: HirInst) -> bool {
        self.value_lowered_use[&inst] > 0
    }

    fn finish_ir_inst(&mut self) {
        for inst in self.ir_inst.drain(..).rev() {
            self.vcode.push(inst);
        }
    }

    fn process_block_param(&mut self, block: HirBasicBlock) {
        for param in self.arena.bb_data(block).params() {
            let vreg = self.reg_map[param].to_virtual_reg().unwrap();
            self.vcode.add_block_param(vreg);
        }
    }

    fn put_value_in_reg(&mut self, inst: HirInst) -> Reg {
        *self.value_lowered_use.get_mut(&inst).unwrap() += 1;
        assert!(!self.inst_sunk.contains(&inst));
        let reg = self.reg_map[&inst];
        // TODO: trace!("put_value_in_regs: inst {:?} -> reg {:?}",inst, reg);
        reg
    }

    fn emit(&mut self, mach_inst: I) {
        trace!("emit mach inst {:?}", mach_inst);
        self.ir_inst.push(mach_inst);
    }

    fn finish_bb(&mut self) {
        self.vcode.end_bb()
    }
}
