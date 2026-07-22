//! Shared AArch64 integer constant planning.

use smallvec::{smallvec, SmallVec};
use taki_mir::register::{Reg, Writable};

use crate::{
    instructions::{AluOp, ImmLogic, MInst, MoveWideConst},
    regs::{OperandSize, RegOrZr},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstantStep {
    Zero,
    Logical(ImmLogic),
    MovZ(MoveWideConst),
    MovN(MoveWideConst),
    MovK(MoveWideConst),
}

pub fn plan_integer_constant(value: u64, size: OperandSize) -> SmallVec<[ConstantStep; 4]> {
    let value = truncate_to_size(value, size);
    if value == 0 {
        return smallvec![ConstantStep::Zero];
    }

    let chunks = chunks(value, size);
    let zero_cost = chunks.iter().filter(|&&chunk| chunk != 0).count();
    let ones_cost = chunks.iter().filter(|&&chunk| chunk != u16::MAX).count();
    if zero_cost == 1 {
        let index = chunks
            .iter()
            .position(|&chunk| chunk != 0)
            .expect("one nonzero move-wide chunk must have a seed");
        let imm = MoveWideConst::new(chunks[index], (index * 16) as u8, size)
            .expect("constant planner generated an invalid movz immediate");
        return smallvec![ConstantStep::MovZ(imm)];
    }
    if ones_cost == 0 {
        let imm = MoveWideConst::new(0, 0, size)
            .expect("constant planner generated an invalid movn immediate");
        return smallvec![ConstantStep::MovN(imm)];
    }
    if ones_cost == 1 {
        let index = chunks
            .iter()
            .position(|&chunk| chunk != u16::MAX)
            .expect("one non-ones move-wide chunk must have a seed");
        let imm = MoveWideConst::new(!chunks[index], (index * 16) as u8, size)
            .expect("constant planner generated an invalid movn immediate");
        return smallvec![ConstantStep::MovN(imm)];
    }
    if let Some(imm) = ImmLogic::new(value, size) {
        return smallvec![ConstantStep::Logical(imm)];
    }

    let use_movn = ones_cost < zero_cost;
    let seed_value = if use_movn { u16::MAX } else { 0 };
    let seed_index = chunks
        .iter()
        .position(|&chunk| chunk != seed_value)
        .expect("nonzero constant must differ from move-wide seed");
    let mut plan = SmallVec::new();
    let seed_bits = if use_movn {
        !chunks[seed_index]
    } else {
        chunks[seed_index]
    };
    let seed = MoveWideConst::new(seed_bits, (seed_index * 16) as u8, size)
        .expect("constant planner generated an invalid move-wide seed");
    plan.push(if use_movn {
        ConstantStep::MovN(seed)
    } else {
        ConstantStep::MovZ(seed)
    });

    for (index, &chunk) in chunks.iter().enumerate() {
        if index != seed_index && chunk != seed_value {
            let patch = MoveWideConst::new(chunk, (index * 16) as u8, size)
                .expect("constant planner generated an invalid move-wide patch");
            plan.push(ConstantStep::MovK(patch));
        }
    }
    plan
}

pub fn materialize_integer_constant(
    value: u64,
    size: OperandSize,
    dst: Writable<Reg>,
) -> SmallVec<[MInst; 4]> {
    plan_integer_constant(value, size)
        .into_iter()
        .map(|step| match step {
            ConstantStep::Zero => MInst::MovFromZero { size, dst },
            ConstantStep::Logical(imm) => MInst::AluRRImmLogic {
                op: AluOp::Orr,
                size,
                dst,
                src: RegOrZr::Zr,
                imm,
            },
            ConstantStep::MovZ(imm) => MInst::MovZ { size, dst, imm },
            ConstantStep::MovN(imm) => MInst::MovN { size, dst, imm },
            ConstantStep::MovK(imm) => MInst::MovK {
                size,
                dst,
                src: dst.to_reg(),
                imm,
            },
        })
        .collect()
}

fn truncate_to_size(value: u64, size: OperandSize) -> u64 {
    match size {
        OperandSize::Size32 => value & u64::from(u32::MAX),
        OperandSize::Size64 => value,
    }
}

fn chunks(value: u64, size: OperandSize) -> SmallVec<[u16; 4]> {
    let count = match size {
        OperandSize::Size32 => 2,
        OperandSize::Size64 => 4,
    };
    (0..count)
        .map(|index| ((value >> (index * 16)) & u64::from(u16::MAX)) as u16)
        .collect()
}
