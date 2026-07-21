use raana_ir::ir::TypeKind;
use taki_mir::{
    abi::{ABIMachineSpec, ArgPair, ArgSlot, StackAMode},
    reg_alloc::reg::MachineEnv,
    register::Reg,
    types::Type,
};

use crate::{inst::Inst, regs};

pub struct AArch64Abi;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValueLocation {
    Reg(Reg),
    Stack { offset: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub args: Vec<ValueLocation>,
    pub stack_size: u32,
}

impl Signature {
    pub fn new(params: &[taki_mir::prelude::HirType]) -> Self {
        let mut int_args = 0u8;
        let mut float_args = 0u8;
        let mut stack_size = 0u32;
        let args = params
            .iter()
            .map(|ty| match ty.kind() {
                TypeKind::Float32 if float_args < 8 => {
                    let reg = regs::float_reg(float_args);
                    float_args += 1;
                    ValueLocation::Reg(reg)
                }
                TypeKind::Int32 | TypeKind::Pointer(_) | TypeKind::String if int_args < 8 => {
                    let reg = regs::int_reg(int_args);
                    int_args += 1;
                    ValueLocation::Reg(reg)
                }
                TypeKind::Float32 | TypeKind::Int32 | TypeKind::Pointer(_) | TypeKind::String => {
                    let offset = stack_size;
                    stack_size += 8;
                    ValueLocation::Stack { offset }
                }
                _ => panic!("unsupported AAPCS64 scalar parameter type: {ty}"),
            })
            .collect();
        Self { args, stack_size }
    }

    pub fn return_location(ty: &taki_mir::prelude::HirType) -> Option<Reg> {
        match ty.kind() {
            TypeKind::Unit => None,
            TypeKind::Float32 => Some(regs::float_reg(0)),
            TypeKind::Int32 | TypeKind::Pointer(_) | TypeKind::String => Some(regs::int_reg(0)),
            _ => panic!("unsupported AAPCS64 return type: {ty}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameLayout {
    pub outgoing_args: u32,
    pub locals: u32,
    pub spills: u32,
    pub callee_saves: u32,
    pub frame_size: u32,
}

impl FrameLayout {
    pub fn new(outgoing_args: u32, locals: u32, spills: u32, callee_saves: u32) -> Self {
        let body_size = outgoing_args + locals + spills + callee_saves;
        let frame_size = (body_size + 15) & !15;
        Self {
            outgoing_args,
            locals,
            spills,
            callee_saves,
            frame_size,
        }
    }
}

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

    fn compute_arg_loc(params: &[taki_mir::prelude::HirType]) -> (Vec<ArgSlot>, u32) {
        let signature = Signature::new(params);
        let args = signature
            .args
            .iter()
            .zip(params)
            .map(|(location, ty)| match location {
                ValueLocation::Reg(reg) => ArgSlot::Reg {
                    reg: reg.to_physical_reg().unwrap(),
                    ty: ty.clone(),
                },
                ValueLocation::Stack { offset } => ArgSlot::Stack {
                    offset: i64::from(*offset),
                    ty: ty.clone(),
                },
            })
            .collect();
        (args, signature.stack_size)
    }

    fn get_machine_env() -> &'static MachineEnv {
        regs::machine_env()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use raana_ir::ir::Type;

    #[test]
    fn separates_integer_and_float_argument_registers() {
        let signature = Signature::new(&[
            Type::get_i32(),
            Type::get_f32(),
            Type::get_i32(),
            Type::get_f32(),
        ]);
        assert_eq!(
            signature.args,
            vec![
                ValueLocation::Reg(regs::int_reg(0)),
                ValueLocation::Reg(regs::float_reg(0)),
                ValueLocation::Reg(regs::int_reg(1)),
                ValueLocation::Reg(regs::float_reg(1)),
            ]
        );
    }

    #[test]
    fn places_overflow_arguments_in_eight_byte_slots() {
        let signature = Signature::new(&vec![Type::get_i32(); 10]);
        assert_eq!(signature.args[8], ValueLocation::Stack { offset: 0 });
        assert_eq!(signature.args[9], ValueLocation::Stack { offset: 8 });
        assert_eq!(signature.stack_size, 16);
    }

    #[test]
    fn aligns_frame_to_sixteen_bytes() {
        assert_eq!(FrameLayout::new(8, 4, 8, 0).frame_size, 32);
    }
}
