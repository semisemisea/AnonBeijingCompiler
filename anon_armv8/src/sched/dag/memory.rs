//! Memory access classification and provenance tracking for the scheduler DAG.

use super::inst_deps::InstDeps;
use super::*;

/// Memory access type for dependency tracking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemKind {
    Load,
    Store,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemRoot {
    StackSp,
    StackFp,
    Global(taki_mir::prelude::HirInst),
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemAccess {
    pub kind: MemKind,
    pub root: MemRoot,
    pub offset: Option<i64>,
    pub size: u8,
}

#[derive(Clone, Copy, Debug)]
struct Provenance {
    root: MemRoot,
    offset: i64,
}

pub(super) fn unknown_access(kind: MemKind, size: u8) -> MemAccess {
    MemAccess {
        kind,
        root: MemRoot::Unknown,
        offset: None,
        size,
    }
}

pub(super) fn annotate_memory_accesses(insts: &[MInst], deps: &mut [InstDeps]) {
    let mut provenance = base_provenance();

    for (inst, deps) in insts.iter().zip(deps) {
        deps.mem = memory_access(inst, &provenance);

        let propagated = propagated_provenance(inst, &provenance);
        for def in &deps.defs {
            provenance.remove(def);
        }
        if deps.is_barrier {
            provenance = base_provenance();
        }
        if let Some((dst, value)) = propagated {
            provenance.insert(dst, value);
        }
    }
}

fn base_provenance() -> FxHashMap<PReg, Provenance> {
    FxHashMap::from_iter([
        (
            stack_preg(),
            Provenance {
                root: MemRoot::StackSp,
                offset: 0,
            },
        ),
        (
            int_preg(FP),
            Provenance {
                root: MemRoot::StackFp,
                offset: 0,
            },
        ),
    ])
}

fn propagated_provenance(
    inst: &MInst,
    provenance: &FxHashMap<PReg, Provenance>,
) -> Option<(PReg, Provenance)> {
    match inst {
        MInst::Mov { size, dst, src } | MInst::MovPhys { size, dst, src }
            if *size == OperandSize::Size64 =>
        {
            Some((preg_one(dst.reg)?, reg_provenance(*src, provenance)?))
        }
        MInst::AluRRImm12 {
            op,
            size,
            dst,
            src,
            imm,
        } if *size == OperandSize::Size64 && matches!(op, AluOp::Add | AluOp::Sub) => {
            let mut value = reg_provenance(*src, provenance)?;
            let immediate = i64::from(imm.value()) << if imm.shift12() { 12 } else { 0 };
            value.offset = match op {
                AluOp::Add => value.offset.checked_add(immediate)?,
                AluOp::Sub => value.offset.checked_sub(immediate)?,
                _ => unreachable!(),
            };
            Some((preg_one(dst.reg)?, value))
        }
        MInst::LoadAddr {
            dst,
            label: Label::GlobalValue(global),
        } => Some((
            preg_one(dst.reg)?,
            Provenance {
                root: MemRoot::Global(*global),
                offset: 0,
            },
        )),
        MInst::StackAddr { dst, addr } => {
            let (root, offset) = pseudo_stack_address(addr)?;
            Some((preg_one(dst.reg)?, Provenance { root, offset }))
        }
        _ => None,
    }
}

fn memory_access(inst: &MInst, provenance: &FxHashMap<PReg, Provenance>) -> Option<MemAccess> {
    let (kind, ty, address, pair, base) = match inst {
        MInst::Load { ty, addr, .. } => (MemKind::Load, *ty, Some(addr), None, None),
        MInst::Store { ty, addr, .. } => (MemKind::Store, *ty, Some(addr), None, None),
        MInst::LoadPair { ty, addr, .. } => (MemKind::Load, *ty, None, Some(addr), None),
        MInst::StorePair { ty, addr, .. } => (MemKind::Store, *ty, None, Some(addr), None),
        // 128-bit vector load/store `ld1/st1 {v.16b}, [base]`: the base is a
        // plain register (not an AMode). The scheduler must see these as real
        // memory accesses or it may reorder them across alias-maybe scalar
        // stores, corrupting stack-initializer order (79_var_name).
        MInst::VecLd1 { base, .. } => (
            MemKind::Load,
            MemoryType::Vec128,
            None,
            None,
            Some(*base),
        ),
        MInst::VecSt1 { base, .. } => (
            MemKind::Store,
            MemoryType::Vec128,
            None,
            None,
            Some(*base),
        ),
        _ => return None,
    };

    let size = ty.byte_size() * if pair.is_some() { 2 } else { 1 };
    let location = address
        .and_then(|addr| amode_location(addr, provenance))
        .or_else(|| pair.and_then(|addr| pair_amode_location(addr, provenance)))
        .or_else(|| {
            base.and_then(|b| {
                reg_provenance(b, provenance).map(|mut p| {
                    p.offset = 0;
                    p
                })
            })
        });
    Some(match location {
        Some(value) => MemAccess {
            kind,
            root: value.root,
            offset: Some(value.offset),
            size,
        },
        None => unknown_access(kind, size),
    })
}

fn amode_location(addr: &AMode, provenance: &FxHashMap<PReg, Provenance>) -> Option<Provenance> {
    let (mut value, displacement) = match addr {
        AMode::Reg { base } => (reg_provenance(*base, provenance)?, 0),
        AMode::UnsignedOffset { base, offset } => (
            reg_provenance(*base, provenance)?,
            i64::try_from(offset.byte_offset()).ok()?,
        ),
        AMode::SignedOffset { base, offset } => (
            reg_provenance(*base, provenance)?,
            i64::from(offset.value()),
        ),
        AMode::FrameSlot(offset) | AMode::SpOffset(offset) | AMode::OutgoingArg(offset) => (
            Provenance {
                root: MemRoot::StackSp,
                offset: 0,
            },
            *offset,
        ),
        AMode::IncomingArg(offset) => (
            Provenance {
                root: MemRoot::StackFp,
                offset: 0,
            },
            *offset,
        ),
        AMode::RegOffset { .. }
        | AMode::ScaledRegOffset { .. }
        | AMode::ExtendedRegOffset { .. } => return None,
    };
    value.offset = value.offset.checked_add(displacement)?;
    Some(value)
}

fn pair_amode_location(
    addr: &PairAMode,
    provenance: &FxHashMap<PReg, Provenance>,
) -> Option<Provenance> {
    let PairAMode::SignedOffset { base, offset } = addr else {
        return None;
    };
    let mut value = reg_provenance(*base, provenance)?;
    value.offset = value.offset.checked_add(offset.byte_offset())?;
    Some(value)
}

fn pseudo_stack_address(addr: &AMode) -> Option<(MemRoot, i64)> {
    match addr {
        AMode::FrameSlot(offset) | AMode::SpOffset(offset) | AMode::OutgoingArg(offset) => {
            Some((MemRoot::StackSp, *offset))
        }
        AMode::IncomingArg(offset) => Some((MemRoot::StackFp, *offset)),
        _ => None,
    }
}

fn preg_one(reg: Reg) -> Option<PReg> {
    reg.to_physical_reg()
}

fn reg_provenance(reg: Reg, known: &FxHashMap<PReg, Provenance>) -> Option<Provenance> {
    known.get(&preg_one(reg)?).copied()
}

pub(super) fn may_alias(a: MemAccess, b: MemAccess) -> bool {
    match (a.root, b.root) {
        (MemRoot::Global(left), MemRoot::Global(right)) if left != right => false,
        (MemRoot::Global(_), MemRoot::StackSp | MemRoot::StackFp)
        | (MemRoot::StackSp | MemRoot::StackFp, MemRoot::Global(_)) => false,
        (left, right) if left == right => {
            let (Some(a_start), Some(b_start)) = (a.offset, b.offset) else {
                return true;
            };
            let Some(a_end) = a_start.checked_add(i64::from(a.size)) else {
                return true;
            };
            let Some(b_end) = b_start.checked_add(i64::from(b.size)) else {
                return true;
            };
            a_start < b_end && b_start < a_end
        }
        // SP- and FP-relative ranges may address the same frame, but their
        // relationship is unavailable after frame legalization.
        _ => true,
    }
}
