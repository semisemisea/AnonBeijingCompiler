use std::fmt::Write;

use anon_armv8::{emit::emit_post_ra_inst, Inst};
use taki_mir::{
    reg_alloc::reg::{Allocation, SpillSlot, VReg},
    register::Reg,
    types::Type,
};

fn main() {
    let vreg =
        |index| Reg::from_virtual_reg(VReg::new(index, taki_mir::reg_alloc::reg::RegClass::Int));
    let inst = Inst::MSub {
        dst: vreg(3),
        mul_lhs: vreg(0),
        mul_rhs: vreg(1),
        sub: vreg(2),
        ty: Type::new_i32(),
    };
    let allocations = [
        Allocation::stack(SpillSlot::new(0)),
        Allocation::stack(SpillSlot::new(8)),
        Allocation::stack(SpillSlot::new(16)),
        Allocation::stack(SpillSlot::new(24)),
    ];
    let mut output = String::from(
        "    .text\n    .globl main\n    .type main, %function\nmain:\n    sub sp, sp, #32\n    mov w0, #6\n    str w0, [sp]\n    mov w0, #5\n    str w0, [sp, #8]\n    mov w0, #31\n    str w0, [sp, #16]\n",
    );
    emit_post_ra_inst(&mut output, "main", &inst, &allocations, 0)
        .expect("three-spill MSub must emit");
    writeln!(output, "    ldr w0, [sp, #24]").unwrap();
    writeln!(output, "    add sp, sp, #32").unwrap();
    writeln!(output, "    ret").unwrap();
    writeln!(output, "    .size main, .-main").unwrap();
    print!("{output}");
}
