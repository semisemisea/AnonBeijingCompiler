use raana_ir::ir::Program;

use crate::backend::armv8::codegen::asm_gen_context::AsmGenContext;

pub trait GenerateAsm {
    fn generate(&self, program: &Program, ctx: &mut AsmGenContext);
}
