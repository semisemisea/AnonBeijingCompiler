use itertools::Itertools;
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
        let x29 = Register::I(IReg(Bit::b64, IntRegister::x29));
        let x30 = Register::I(IReg(Bit::b64, IntRegister::x30));
        let sp = Register::I(IReg(Bit::b64, IntRegister::sp));
        if self.offset != 0 {
            // 恢复其他 callee-saved 寄存器
            let mut stack_used = 0;
            for &reg in self.callee_usage.iter().sorted() {
                ctx.load_word(reg, self.offset - (stack_used + 8), sp);
                stack_used += 8;
            }
            // 恢复x29和x30
            if self.call_ra {
                ctx.load_word(x29, 0, sp);
                ctx.load_word(x30, 8, sp);
            }
            // 回收栈帧
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
