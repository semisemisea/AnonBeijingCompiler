use std::collections::HashSet;

use crate::backend::armv8::codegen::asm_gen_context::AsmGenContext;
use crate::backend::armv8::inst::Inst;
use crate::backend::armv8::register;

#[derive(Clone)]
pub struct Epilogue {
    pub(crate) offset: i32,
    pub(crate) call_ra: bool,
    pub(crate) callee_usage: HashSet<register::Register>,
    pub(crate) finished_once: bool,
}

impl Epilogue {
    pub fn mark(&mut self) -> &Epilogue {
        self.finished_once = true;
        &*self
    }

    pub fn finish(&self, ctx: &mut AsmGenContext) {
        use Inst::*;
        use register::*;
        let sp = Register::I(IReg(Bit::b64, IntRegister::sp));
        if self.offset != 0 {
            let mut callee_start = if self.call_ra {
                ctx.load_word(ra, self.offset - 4, sp);
                8
            } else {
                4
            };
            for &reg in self.callee_usage.iter().sorted() {
                ctx.load_word(reg, self.offset - callee_start, sp);
                callee_start += 4;
            }
            ctx.add_imm(sp, self.offset, sp);
        }
        ctx.write_inst(ret);
    }
}

impl Drop for Epilogue {
    fn drop(&mut self) {
        if !self.finished_once {
            eprintln!("Epilogue must be done before droped.");
        }
    }
}
