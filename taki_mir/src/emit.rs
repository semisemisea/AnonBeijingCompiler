use core::fmt::Write;

use crate::abi::{ABIMachineSpec, FrameLayout};
use crate::block_order::MirBlockIndex;
use crate::emit_buffer::EmitBuffer;
use crate::lower::LowerBackend;
use crate::prelude::*;
use crate::vcode::{
    EmitContext, MachInst, MachInstEmit, MachTerminator, VCodeContainer, VCodeInst,
};

pub(crate) struct AsmWriter<'a, B: LowerBackend> {
    pub buf: &'a mut String,
    pub func_data: &'a HirFunctionData,
    pub program: &'a HirProgram,
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
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<B: LowerBackend> AsmWriter<'_, B> {
    /// Emit one function through the [`EmitBuffer`]: every instruction
    /// (prologue, blocks, epilogue) lands in the buffer as a text slot, then
    /// the finished buffer is rendered into the output string.
    pub fn write_function(&mut self, vcode: &VCodeContainer<B::MInst>)
    where
        B::MInst: MachInstEmit,
    {
        let name = self.func_data.name();
        writeln!(self.buf, "{} {name}", B::global_directive()).unwrap();
        writeln!(self.buf, "{name}:").unwrap();

        let frame = vcode.abi.frame_layout();
        let block_labels: Vec<String> = vcode
            .block_order()
            .lowered_order()
            .iter()
            .map(|lb| B::format_block_label(lb, self.func_data))
            .collect();
        let mut buffer = EmitBuffer::<B>::new(self.program, block_labels);

        for inst in &vcode.abi.gen_prologue() {
            emit_legalized::<B::MInst, B>(&frame, inst, &mut buffer);
        }

        for (bi, _lb) in vcode.block_order().lowered_order().iter().enumerate() {
            buffer.bind_label(MirBlockIndex::new(bi));
            for inst in vcode.block_insts(bi) {
                if matches!(inst.is_term(), MachTerminator::Return) {
                    for epi in &vcode.abi.gen_epilogue() {
                        emit_legalized::<B::MInst, B>(&frame, epi, &mut buffer);
                    }
                }
                inst.emit(&mut buffer).unwrap();
                buffer.end_inst().unwrap();
            }
        }

        buffer.resolve();
        self.buf.push_str(&buffer.finish());
        writeln!(self.buf).unwrap();
    }
}

/// Legalize and emit an ABI-generated instruction (prologue, epilogue).
/// VCode instructions are already finalized and go through the buffer's
/// plain `inst.emit` path instead.
fn emit_legalized<I, B>(frame: &FrameLayout, inst: &I, buffer: &mut EmitBuffer<'_, B>)
where
    I: VCodeInst + MachInstEmit,
    B: LowerBackend<MInst = I>,
{
    for inst in I::ABISpec::legalize_inst(frame, inst.clone()) {
        inst.emit(buffer).unwrap();
        buffer.end_inst().unwrap();
    }
}
