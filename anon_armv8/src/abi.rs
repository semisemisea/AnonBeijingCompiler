use raana_ir::ir::TypeKind;
use taki_mir::{
    abi::{ABIMachineSpec, ArgPair, ArgSlot, StackAMode},
    reg_alloc::reg::{MachineEnv, Output as RegAllocOutput, PReg, RegClass},
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
    pub used_callee_saves: Vec<PReg>,
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
            used_callee_saves: Vec::new(),
        }
    }

    /// Finalize a frame after register allocation has selected spills and
    /// integer callee-save registers.
    pub fn from_regalloc(outgoing_args: u32, locals: u32, allocations: &RegAllocOutput) -> Self {
        let mut used_callee_saves = allocations
            .allocs
            .iter()
            .filter_map(|allocation| allocation.as_reg())
            .filter(|preg| preg.class() == RegClass::Int && (19..=28).contains(&preg.hw_enc()))
            .collect::<Vec<_>>();
        used_callee_saves.sort_by_key(|preg| preg.hw_enc());
        used_callee_saves.dedup();

        let mut layout = Self::new(
            outgoing_args,
            locals,
            u32::try_from(allocations.num_spillslots)
                .expect("AArch64 spill area exceeds the supported frame range"),
            (used_callee_saves.len() as u32) * 8,
        );
        layout.used_callee_saves = used_callee_saves;
        layout
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
    use taki_mir::reg_alloc::reg::{Allocation, Output as RegAllocOutput};

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

    #[test]
    fn finalizes_spills_and_integer_callee_saves_from_ra() {
        let allocations = RegAllocOutput {
            num_spillslots: 24,
            edits: vec![],
            allocs: vec![
                Allocation::reg(regs::int_preg(19)),
                Allocation::reg(regs::int_preg(19)),
                Allocation::reg(regs::int_preg(21)),
                Allocation::reg(regs::float_preg(8)),
            ],
            inst_alloc_offsets: vec![0],
        };
        let layout = FrameLayout::from_regalloc(8, 12, &allocations);

        assert_eq!(layout.spills, 24);
        assert_eq!(layout.callee_saves, 16);
        assert_eq!(
            layout.used_callee_saves,
            vec![regs::int_preg(19), regs::int_preg(21)]
        );
        assert_eq!(layout.frame_size, 64);
    }
}
