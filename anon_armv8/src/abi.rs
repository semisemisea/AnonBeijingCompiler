use taki_mir::{
    abi::{ABIMachineSpec, ArgPair, ArgSlot, StackAMode},
    reg_alloc::reg::MachineEnv,
    register::Reg,
    types::Type,
};

use crate::{inst::Inst, regs};

pub struct AArch64Abi;

impl ABIMachineSpec for AArch64Abi {
    type I = Inst;

    fn stack_align() -> u32 {
        16
    }

    fn gen_load_stack(_: StackAMode, _: Reg, _: Type) -> Inst {
        panic!("stack argument lowering is implemented with AAPCS64 frame lowering")
    }

    fn gen_args(_: Vec<ArgPair>) -> Inst {
        panic!("argument copies are emitted by AArch64 ABI lowering")
    }

    fn gen_ret() -> Inst {
        Inst::Ret
    }

    fn gen_store_stack(_: Reg, _: StackAMode, _: Type) -> Inst {
        panic!("stack argument lowering is implemented with AAPCS64 frame lowering")
    }

    fn gen_jump(_: taki_mir::prelude::HirBasicBlock) -> Inst {
        panic!("HIR block IDs must be converted to MIR labels before emission")
    }

    fn gen_branch() -> Inst {
        panic!("branches are selected directly by the AArch64 lowering backend")
    }

    fn gen_nop() -> Inst {
        Inst::Nop
    }

    fn gen_move(src: Reg, dst: Reg, ty: Type) -> Inst {
        Inst::Mov { dst, src, ty }
    }

    fn compute_arg_loc(_: &[taki_mir::prelude::HirType]) -> (Vec<ArgSlot>, u32) {
        // M1 only defines the target instruction and register contract. AAPCS64
        // argument placement is completed in M2.
        (Vec::new(), 0)
    }

    fn get_machine_env() -> &'static MachineEnv {
        regs::machine_env()
    }
}
