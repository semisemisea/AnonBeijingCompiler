use core::fmt::Write;

use crate::abi::{ABIMachineSpec, FrameLayout};
use crate::block_order::MirBlockIndex;
use crate::lower::LowerBackend;
use crate::prelude::*;
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
    pub fn write_function(&mut self, vcode: &VCodeContainer<B::MInst>)
    where
        B::MInst: MachInstEmit,
    {
        type S<B> = <<B as LowerBackend>::MInst as MachInst>::ABISpec;

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

        for (bi, _lb) in block_order.lowered_order().iter().enumerate() {
            writeln!(self.buf, "{}:", self.block_labels[bi]).unwrap();

            for inst in vcode.block_insts(bi) {
                if matches!(inst.is_term(), MachTerminator::Return) {
                    for epi in &vcode.abi.gen_epilogue() {
                        self.write_inst(frame, epi);
                    }
                }
                self.print_inst(inst);
            }
        }
        writeln!(self.buf).unwrap();
    }

    /// Legalize and print an ABI-generated instruction (prologue, epilogue).
    /// VCode instructions are already finalized and use [`print_inst`] instead.
    fn write_inst<I: crate::vcode::VCodeInst + MachInstEmit>(
        &mut self,
        frame: &FrameLayout,
        inst: &I,
    ) {
        let legalized = I::ABISpec::legalize_inst(frame, inst.clone());
        for inst in legalized {
            write!(self.buf, "    ").unwrap();
            inst.emit(self).unwrap();
            writeln!(self.buf).unwrap();
        }
    }

    /// Print a finalized VCode instruction directly — no legalization needed.
    fn print_inst<I: MachInstEmit>(&mut self, inst: &I) {
        write!(self.buf, "    ").unwrap();
        inst.emit(self).unwrap();
        writeln!(self.buf).unwrap();
    }
}
