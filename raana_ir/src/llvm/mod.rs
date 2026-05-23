use crate::ir::Program;

mod writer;

pub use writer::LlvmWriter;

/// Convert a RaanaIR `Program` into LLVM IR text.
pub fn write_llvm_ir(program: &Program) -> String {
    let mut w = LlvmWriter::new(program);
    w.write().unwrap();
    w.finish()
}
