use crate::backend::armv8::register::{Bit, IReg, IntRegister, Register};

pub struct RegisterManager {
    temp_usage: usize,
    pool: Vec<Register>,
}

impl RegisterManager {
    pub(in crate::backend::armv8) fn new() -> RegisterManager {
        RegisterManager {
            // only using x14 and x15
            temp_usage: 5,
            pool: Vec::with_capacity(16),
        }
    }

    #[inline]
    pub(in crate::backend) fn push_register(&mut self, register: Register) {
        self.pool.push(register)
    }

    pub(in crate::backend) fn take_register(&mut self) -> Register {
        let ret = self.pool.pop().unwrap();
        if matches!(
            ret,
            Register::I(IReg(
                _,
                IntRegister::x14 | IntRegister::x15 | IntRegister::xzr
            ))
        ) {
            self.temp_usage -= 1;
        }
        ret
    }

    #[inline]
    pub(in crate::backend) fn alloc_temp(&mut self) -> Register {
        let ret = Register::temporary(self.temp_usage);
        self.temp_incr();
        self.pool.push(ret);
        ret
    }

    pub(in crate::backend) fn alloc_ret(&mut self) {
        self.pool.push(Register::I(IReg(Bit::b64, IntRegister::x0)));
    }

    #[inline]
    pub(in crate::backend) fn temp_incr(&mut self) {
        // debug_assert!(self.temp_usage < 7, "run out of temporary register");
        self.temp_usage += 1;
    }

    #[inline]
    pub(in crate::backend) fn temp_decr(&mut self) {
        self.temp_usage -= 1;
    }

    // fn take_register(&mut self) -> Register {
    //     let ret = self.pool.pop().unwrap();
    //     if ret.is_temp() {
    //         self.temp_decr();
    //     }
    //     ret
    // }
}
