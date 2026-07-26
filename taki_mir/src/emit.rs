use core::fmt::Write;

use crate::abi::{ABIMachineSpec, FrameLayout};
use crate::block_order::MirBlockIndex;
use crate::lower::LowerBackend;
use crate::prelude::*;
use crate::reg_alloc::reg::{InstOrEdit, Output, RegClass};
use crate::register::Reg;
use crate::vcode::{EmitContext, MachInst, MachInstEmit, MachTerminator, VCodeContainer};

pub(crate) struct AsmWriter<'a, B: LowerBackend> {
    pub buf: &'a mut String,
    pub func_data: &'a HirFunctionData,
    pub program: &'a HirProgram,
    pub(crate) block_labels: Vec<String>,
    pub(crate) _phantom: std::marker::PhantomData<B>,
}

impl<'a, B: LowerBackend> AsmWriter<'a, B> {
    pub fn new(
        buf: &'a mut String,
        func_data: &'a HirFunctionData,
        program: &'a HirProgram,
    ) -> AsmWriter<'a, B> {
        Self {
            buf,
            func_data,
            program,
            block_labels: vec![],
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<B: LowerBackend> core::fmt::Write for AsmWriter<'_, B> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.buf.write_str(s)
    }
}

impl<B: LowerBackend> EmitContext for AsmWriter<'_, B> {
    fn write_reg(&mut self, reg: &Reg) -> core::fmt::Result {
        if let Some(preg) = reg.to_real_reg() {
            write!(self, "{}", B::preg_name(preg))
        } else {
            panic!(
                "write_reg called on non-physical register {reg:?}; \
                 spill slots must be handled by regalloc Edit system \
                 (output.edits) before emission"
            )
        }
    }

    fn write_label_ref(&mut self, idx: MirBlockIndex) -> core::fmt::Result {
        let label = self.block_labels[idx.index()].clone();
        write!(self, "{label}")
    }

    fn write_function_label(&mut self, func: HirFunction) -> core::fmt::Result {
        let name = self.program.func_data(func).name();
        write!(self, "{name}")
    }

    fn write_global_label(&mut self, gv: HirInst) -> core::fmt::Result {
        if let Some(name) = self.program.inst_data(gv).name() {
            write!(self, "{name}")
        } else {
            write!(self, "<gv>")
        }
    }

    fn write_external_symbol(&mut self, symbol: &str) -> core::fmt::Result {
        write!(self, "{symbol}")
    }
}

impl<B: LowerBackend> AsmWriter<'_, B> {
    pub fn write_function(&mut self, vcode: &VCodeContainer<B::MInst>, output: &Output)
    where
        B::MInst: MachInstEmit,
    {
        type S<B: LowerBackend> = <<B as LowerBackend>::MInst as MachInst>::ABISpec;

        let name = self.func_data.name();
        writeln!(self.buf, "{} {name}", B::global_directive()).unwrap();
        writeln!(self.buf, "{name}:").unwrap();

        let frame = vcode.abi.frame_layout();
        for inst in &vcode.abi.gen_prologue() {
            self.write_inst(frame, inst);
        }

        let block_order = vcode.block_order();
        self.block_labels = block_order
            .lowered_order()
            .iter()
            .map(|lb| B::format_block_label(lb, self.func_data))
            .collect();

        let spill_unit_bytes = vcode.abi.spill_unit_bytes();

        for (bi, _lb) in block_order.lowered_order().iter().enumerate() {
            writeln!(self.buf, "{}:", self.block_labels[bi]).unwrap();

            let block_idx = MirBlockIndex::new(bi);
            for item in output.block_insts_and_edits(vcode, block_idx) {
                match item {
                    InstOrEdit::Edit(edit) => {
                        let crate::reg_alloc::reg::Edit::Move { from, to, class } = edit;
                        let ty = S::<B>::ty_for_regclass(*class);
                        match (from.as_reg(), to.as_reg()) {
                            (Some(from_reg), Some(to_reg)) => {
                                assert_eq!(
                                    from_reg.class(),
                                    *class,
                                    "register-to-register allocation moves cannot cross register classes"
                                );
                                assert_eq!(to_reg.class(), *class);
                                assert_ne!(
                                    *class,
                                    RegClass::Vector,
                                    "vector register moves are unsupported"
                                );
                                let mv = S::<B>::gen_move(
                                    Reg::from_physical_reg(from_reg),
                                    Reg::from_physical_reg(to_reg),
                                    ty,
                                );
                                self.write_inst(frame, &mv);
                            }
                            (Some(from_reg), None) => {
                                let slot = to.as_stack().unwrap();
                                let offset = frame.spill_slot_offset(slot, spill_unit_bytes);
                                for inst in S::<B>::gen_spill_store_at_sp(
                                    Reg::from_physical_reg(from_reg),
                                    offset,
                                    ty,
                                ) {
                                    self.write_inst(frame, &inst);
                                }
                            }
                            (None, Some(to_reg)) => {
                                let slot = from.as_stack().unwrap();
                                let offset = frame.spill_slot_offset(slot, spill_unit_bytes);
                                for inst in S::<B>::gen_spill_load_at_sp(
                                    offset,
                                    crate::register::Writable::from_reg(Reg::from_physical_reg(
                                        to_reg,
                                    )),
                                    ty,
                                ) {
                                    self.write_inst(frame, &inst);
                                }
                            }
                            (None, None) => {
                                let from_slot = from.as_stack().unwrap();
                                let to_slot = to.as_stack().unwrap();
                                let from_offset =
                                    frame.spill_slot_offset(from_slot, spill_unit_bytes);
                                let to_offset = frame.spill_slot_offset(to_slot, spill_unit_bytes);
                                for inst in S::<B>::gen_stack_to_stack_move(from_offset, to_offset)
                                {
                                    self.write_inst(frame, &inst);
                                }
                            }
                        }
                    }
                    InstOrEdit::Inst(inst_idx) => {
                        let inst = vcode.inst(inst_idx.index());

                        if matches!(inst.is_term(), MachTerminator::Return) {
                            for epi in &vcode.abi.gen_epilogue() {
                                self.write_inst(frame, epi);
                            }
                        }
                        self.write_inst(frame, inst);
                    }
                }
            }
        }
        writeln!(self.buf).unwrap();
    }

    fn write_inst<I: crate::vcode::VCodeInst + MachInstEmit>(
        &mut self,
        frame: &FrameLayout,
        inst: &I,
    ) {
        let legalized = I::ABISpec::legalize_inst(frame, inst.clone());
        log::trace!(target: "taki_mir::emit", "legalize original={inst:?} legalized={legalized:?}");
        for inst in legalized {
            write!(self.buf, "    ").unwrap();
            inst.emit(self).unwrap();
            writeln!(self.buf).unwrap();
        }
    }
}
