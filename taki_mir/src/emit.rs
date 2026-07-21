use core::fmt::Write;

use crate::abi::ABIMachineSpec;
use crate::block_order::MirBlockIndex;
use crate::lower::LowerBackend;
use crate::prelude::*;
use crate::reg_alloc::function::Function;
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

        for inst in &vcode.abi.gen_prologue() {
            self.write_inst(inst);
        }

        let block_order = vcode.block_order();
        self.block_labels = block_order
            .lowered_order()
            .iter()
            .map(|lb| B::format_block_label(lb, self.func_data))
            .collect();

        let frame = vcode.abi.frame_layout();
        let spill_base = (frame.outgoing_args_size + frame.stackslots_size) as i64;
        let slot_size =
            S::<B>::spillslot_size(crate::reg_alloc::reg::RegClass::Int) as i64;

        for (bi, _lb) in block_order.lowered_order().iter().enumerate() {
            writeln!(self.buf, "{}:", self.block_labels[bi]).unwrap();

            let block_idx = MirBlockIndex::new(bi);
            for item in output.block_insts_and_edits(vcode, block_idx) {
                match item {
                    InstOrEdit::Edit(edit) => {
                        let crate::reg_alloc::reg::Edit::Move { from, to } = edit;
                        match (from.as_reg(), to.as_reg()) {
                            (Some(from_reg), Some(to_reg)) => {
                                let mv = S::<B>::gen_move(
                                    Reg::from_physical_reg(from_reg),
                                    Reg::from_physical_reg(to_reg),
                                    crate::types::I64,
                                );
                                self.write_inst(&mv);
                            }
                            (Some(from_reg), None) => {
                                let slot = to.as_stack().unwrap();
                                let offset = spill_base
                                    + slot.raw_bits() as i64 * slot_size;
                                let ty = match from_reg.class() {
                                    RegClass::Float => crate::types::F32,
                                    _ => crate::types::I64,
                                };
                                for inst in S::<B>::gen_spill_store(
                                    Reg::from_physical_reg(from_reg),
                                    offset,
                                    ty,
                                ) {
                                    self.write_inst(&inst);
                                }
                            }
                            (None, Some(to_reg)) => {
                                let slot = from.as_stack().unwrap();
                                let offset = spill_base
                                    + slot.raw_bits() as i64 * slot_size;
                                let ty = match to_reg.class() {
                                    RegClass::Float => crate::types::F32,
                                    _ => crate::types::I64,
                                };
                                for inst in S::<B>::gen_spill_load(
                                    offset,
                                    crate::register::Writable::from_reg(
                                        Reg::from_physical_reg(to_reg),
                                    ),
                                    ty,
                                ) {
                                    self.write_inst(&inst);
                                }
                            }
                            (None, None) => {
                                panic!("stack-to-stack edit should not exist")
                            }
                        }
                    }
                    InstOrEdit::Inst(inst_idx) => {
                        let inst = vcode.inst(inst_idx.index());

                        if matches!(inst.is_term(), MachTerminator::Return) {
                            for epi in &vcode.abi.gen_epilogue() {
                                self.write_inst(epi);
                            }
                        }
                        self.write_inst(inst);
                    }
                }
            }
        }
        writeln!(self.buf).unwrap();
    }

    fn write_inst<I: crate::vcode::VCodeInst + MachInstEmit>(&mut self, inst: &I) {
        write!(self.buf, "    ").unwrap();
        inst.emit(self).unwrap();
        writeln!(self.buf).unwrap();
    }
}
