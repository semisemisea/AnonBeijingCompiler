//! Post-RA formation of adjacent AArch64 load/store pairs.

use taki_mir::{
    passes::MIRPass, prelude::ArenaContext, register::Reg, stats::FunctionCodegenStats,
    vcode::VCodeContainer,
};

use crate::instructions::{AMode, MInst, PairAMode, SImm7Scaled};

pub struct PairCombine;

impl MIRPass<MInst> for PairCombine {
    fn name(&self) -> &'static str {
        "PairCombine"
    }

    fn run(
        &self,
        vcode: &mut VCodeContainer<MInst>,
        _arena: ArenaContext,
        stats: &mut FunctionCodegenStats,
    ) -> bool {
        stats.pair.ran = true;
        let mut changed = false;

        for block_idx in 0..vcode.num_blocks() {
            let range = vcode.block_inst_range(block_idx);
            let mut i = range.start;
            while i + 1 < range.end {
                if let Some(pair) = form_pair(vcode.inst(i), vcode.inst(i + 1)) {
                    match pair {
                        MInst::LoadPair { .. } => stats.pair.load_pairs_formed += 1,
                        MInst::StorePair { .. } => stats.pair.store_pairs_formed += 1,
                        _ => unreachable!(),
                    }
                    stats.pair.tombstone_nops_created += 1;
                    *vcode.inst_mut(i) = pair;
                    *vcode.inst_mut(i + 1) = MInst::Nop;
                    changed = true;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }

        stats.pair.changed = changed;
        changed
    }
}

fn form_pair(first: &MInst, second: &MInst) -> Option<MInst> {
    match (first, second) {
        (
            MInst::Load {
                ty: first_ty,
                dst: dst1,
                addr: first_addr,
            },
            MInst::Load {
                ty: second_ty,
                dst: dst2,
                addr: second_addr,
            },
        ) if first_ty == second_ty => {
            let addr = pair_addr(first_addr, second_addr, first_ty.byte_size())?;
            let base = pair_base(&addr);
            let dst1_reg = dst1.to_reg();
            let dst2_reg = dst2.to_reg();

            if dst1_reg == dst2_reg || dst1_reg == base || dst2_reg == base {
                return None;
            }
            if [dst1_reg, dst2_reg, base]
                .iter()
                .any(|reg| reg.to_physical_reg().is_none())
            {
                return None;
            }

            Some(MInst::LoadPair {
                ty: *first_ty,
                dst1: *dst1,
                dst2: *dst2,
                addr,
            })
        }
        (
            MInst::Store {
                ty: first_ty,
                src: src1,
                addr: first_addr,
            },
            MInst::Store {
                ty: second_ty,
                src: src2,
                addr: second_addr,
            },
        ) if first_ty == second_ty => {
            let addr = pair_addr(first_addr, second_addr, first_ty.byte_size())?;
            if [*src1, *src2, pair_base(&addr)]
                .iter()
                .any(|reg| reg.to_physical_reg().is_none())
            {
                return None;
            }

            Some(MInst::StorePair {
                ty: *first_ty,
                src1: *src1,
                src2: *src2,
                addr,
            })
        }
        _ => None,
    }
}

fn pair_addr(first: &AMode, second: &AMode, access_size: u8) -> Option<PairAMode> {
    let (first_base, first_offset) = fixed_addr(first)?;
    let (second_base, second_offset) = fixed_addr(second)?;
    if first_base != second_base
        || first_offset.checked_add(i64::from(access_size)) != Some(second_offset)
    {
        return None;
    }

    Some(PairAMode::SignedOffset {
        base: first_base,
        offset: SImm7Scaled::new(first_offset, access_size)?,
    })
}

fn fixed_addr(addr: &AMode) -> Option<(Reg, i64)> {
    match addr {
        AMode::Reg { base } => Some((*base, 0)),
        AMode::UnsignedOffset { base, offset } => {
            Some((*base, i64::try_from(offset.byte_offset()).ok()?))
        }
        AMode::SignedOffset { base, offset } => Some((*base, i64::from(offset.value()))),
        _ => None,
    }
}

fn pair_base(addr: &PairAMode) -> Reg {
    match addr {
        PairAMode::SignedOffset { base, .. }
        | PairAMode::PreIndex { base, .. }
        | PairAMode::PostIndex { base, .. } => *base,
    }
}

#[cfg(test)]
mod tests {
    use taki_mir::register::Writable;

    use super::*;
    use crate::{
        instructions::{MemoryType, UImm12Scaled},
        regs::int_reg,
    };

    fn writable(index: u8) -> Writable<Reg> {
        Writable::from_reg(int_reg(index))
    }

    fn addr(base: u8, offset: u64, size: u8) -> AMode {
        if offset == 0 {
            AMode::Reg {
                base: int_reg(base),
            }
        } else {
            AMode::UnsignedOffset {
                base: int_reg(base),
                offset: UImm12Scaled::new(offset, size).unwrap(),
            }
        }
    }

    #[test]
    fn forms_contiguous_load_pair() {
        let pair = form_pair(
            &MInst::Load {
                ty: MemoryType::I64,
                dst: writable(1),
                addr: addr(10, 0, 8),
            },
            &MInst::Load {
                ty: MemoryType::I64,
                dst: writable(2),
                addr: addr(10, 8, 8),
            },
        );

        assert!(matches!(
            pair,
            Some(MInst::LoadPair {
                ty: MemoryType::I64,
                dst1,
                dst2,
                addr: PairAMode::SignedOffset { base, offset },
            }) if dst1.to_reg() == int_reg(1)
                && dst2.to_reg() == int_reg(2)
                && base == int_reg(10)
                && offset.byte_offset() == 0
        ));
    }

    #[test]
    fn forms_contiguous_store_pair() {
        let pair = form_pair(
            &MInst::Store {
                ty: MemoryType::I32,
                src: int_reg(1),
                addr: addr(10, 12, 4),
            },
            &MInst::Store {
                ty: MemoryType::I32,
                src: int_reg(2),
                addr: addr(10, 16, 4),
            },
        );

        assert!(matches!(
            pair,
            Some(MInst::StorePair {
                ty: MemoryType::I32,
                addr: PairAMode::SignedOffset { offset, .. },
                ..
            }) if offset.byte_offset() == 12
        ));
    }

    #[test]
    fn rejects_noncontiguous_or_reversed_addresses() {
        let store = |offset| MInst::Store {
            ty: MemoryType::I64,
            src: int_reg(1),
            addr: addr(10, offset, 8),
        };

        assert!(form_pair(&store(0), &store(16)).is_none());
        assert!(form_pair(&store(8), &store(0)).is_none());
    }

    #[test]
    fn rejects_load_register_hazards() {
        let load = |dst, base, offset| MInst::Load {
            ty: MemoryType::I64,
            dst: writable(dst),
            addr: addr(base, offset, 8),
        };

        assert!(form_pair(&load(1, 10, 0), &load(1, 10, 8)).is_none());
        assert!(form_pair(&load(10, 10, 0), &load(1, 10, 8)).is_none());
        assert!(form_pair(&load(1, 10, 0), &load(10, 10, 8)).is_none());
    }

    #[test]
    fn rejects_different_types_bases_and_indexed_addresses() {
        let first = MInst::Load {
            ty: MemoryType::I32,
            dst: writable(1),
            addr: addr(10, 0, 4),
        };
        let different_type = MInst::Load {
            ty: MemoryType::F32,
            dst: writable(2),
            addr: addr(10, 4, 4),
        };
        let different_base = MInst::Load {
            ty: MemoryType::I32,
            dst: writable(2),
            addr: addr(11, 4, 4),
        };
        let indexed = MInst::Load {
            ty: MemoryType::I32,
            dst: writable(2),
            addr: AMode::RegOffset {
                base: int_reg(10),
                index: int_reg(11),
            },
        };

        assert!(form_pair(&first, &different_type).is_none());
        assert!(form_pair(&first, &different_base).is_none());
        assert!(form_pair(&first, &indexed).is_none());
    }

    #[test]
    fn rejects_unencodable_pair_offset() {
        let first = MInst::Store {
            ty: MemoryType::I64,
            src: int_reg(1),
            addr: addr(10, 512, 8),
        };
        let second = MInst::Store {
            ty: MemoryType::I64,
            src: int_reg(2),
            addr: addr(10, 520, 8),
        };

        assert!(form_pair(&first, &second).is_none());
    }
}
