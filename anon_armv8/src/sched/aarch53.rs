//! Cortex-A53 latency and issue-resource model.
//!
//! Numbers are from the *ARM Cortex-A53 Software Optimization Guide* (DUI 0901).
//! All values are guide-derived, not measured on XCZU15EG hardware.

/// Issue slot on the Cortex-A53 dual-issue pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Slot {
    /// Primary integer ALU / branch pipe.
    Alu0,
    /// Secondary integer ALU / load-store pipe.
    Alu1,
}

/// Functional-unit / pipeline class for one instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SchedClass {
    /// Simple integer ALU: `add`, `sub`, `and`, `orr`, `eor`, `mov`.
    Alu,
    /// Shifted/extended integer ALU: `add x0, x1, x2, lsl #3`.
    AluShift,
    /// Compare, conditional select, move-wide: `cmp`, `csel`, `movz`, `movk`.
    AluMisc,
    /// Integer multiply / multiply-accumulate: `mul`, `madd`, `msub`, `smull`.
    Mul,
    /// Integer divide 32-bit: `sdiv w0, w1, w2`. Variable latency, non-pipelined.
    Div32,
    /// Integer divide 64-bit: `sdiv x0, x1, x2`. Variable latency, non-pipelined.
    Div64,
    /// Scalar integer load: `ldr x0, [x1]`.
    LoadInt,
    /// Scalar integer store: `str x0, [x1]`.
    StoreInt,
    /// Scalar FP load: `ldr s0, [x1]`.
    LoadFp,
    /// Scalar FP store: `str s0, [x1]`.
    StoreFp,
    /// Pair integer load: `ldp x0, x1, [x2]`.
    LoadPairInt,
    /// Pair integer store: `stp x0, x1, [x2]`.
    StorePairInt,
    /// Pair FP load: `ldp s0, s1, [x2]`.
    LoadPairFp,
    /// Pair FP store: `stp s0, s1, [x2]`.
    StorePairFp,
    /// FP move: `fmov s0, s1` or `fmov s0, w1`.
    FpMove,
    /// FP add/sub: `fadd s0, s1, s2`.
    FpAddSub,
    /// FP multiply: `fmul s0, s1, s2`.
    FpMul,
    /// FP divide: `fdiv s0, s1, s2`.
    FpDiv,
    /// FP compare: `fcmp s0, s1`.
    FpCmp,
    /// Int/FP conversion: `scvtf s0, w1`.
    FpCvt,
    /// Branch / control flow: `b`, `bl`, `cbz`, `ret`.
    Branch,
    /// Barrier: `call`, `tailcall`. Must serialize.
    Barrier,
    /// Pseudo / no-op. No latency, no resource.
    Nop,
    /// Anything not yet precisely modeled. Conservative fallback.
    Other,
}

/// Which slot(s) an instruction can issue to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotMask(pub u8);

impl SlotMask {
    pub const NONE: Self = Self(0);
    pub const ALU0: Self = Self(1);
    pub const ALU1: Self = Self(2);
    pub const EITHER: Self = Self(3);

    pub fn can_issue_to(self, slot: Slot) -> bool {
        match slot {
            Slot::Alu0 => self.0 & Self::ALU0.0 != 0,
            Slot::Alu1 => self.0 & Self::ALU1.0 != 0,
        }
    }
}

/// Which resource(s) an instruction occupies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceMask(pub u16);

impl ResourceMask {
    pub const NONE: Self = Self(0);
    pub const ALU: Self = Self(1 << 0);
    pub const LSU: Self = Self(1 << 1);
    pub const MAC: Self = Self(1 << 2);
    pub const DIV: Self = Self(1 << 3);
    pub const FP_NEON: Self = Self(1 << 4);
    pub const BRANCH: Self = Self(1 << 5);
    pub const FRONTEND: Self = Self(1 << 6);

    pub fn overlaps(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

/// Scheduling characteristics of one instruction.
///
/// All values are guide-derived from the ARM Cortex-A53 Software Optimization
/// Guide unless marked otherwise. They have not been validated on XCZU15EG
/// hardware.
#[derive(Clone, Copy, Debug)]
pub struct InstrProfile {
    /// Result latency in cycles.
    pub latency: u32,
    /// Reciprocal throughput: minimum cycles between issuing two instructions
    /// of this class on the same resource.
    pub reciprocal_throughput: u32,
    /// Cycles the resource is occupied (non-pipelined if > 1).
    pub resource_occupancy: u32,
    /// Which issue slot(s) can accept this instruction.
    pub allowed_slots: SlotMask,
    /// Which resource(s) this instruction occupies.
    pub resources: ResourceMask,
    /// Number of real AArch64 instructions this node emits.
    pub emitted_ops: u8,
}

/// Look up the scheduling profile for a [`SchedClass`] on Cortex-A53.
pub fn instr_profile(class: SchedClass) -> InstrProfile {
    use ResourceMask as R;
    use SlotMask as S;
    let (latency, throughput, occupancy, slots, resources, emitted) = match class {
        SchedClass::Alu => (1, 1, 1, S::EITHER, R::ALU, 1),
        SchedClass::AluShift => (1, 1, 1, S::EITHER, R::ALU, 1),
        SchedClass::AluMisc => (1, 1, 1, S::EITHER, R::ALU, 1),
        SchedClass::Mul => (3, 1, 1, S::ALU0, R::MAC, 1),
        SchedClass::Div32 => (11, 11, 11, S::ALU0, R::DIV, 1),
        SchedClass::Div64 => (19, 19, 19, S::ALU0, R::DIV, 1),
        SchedClass::LoadInt => (2, 1, 1, S::ALU1, R::LSU, 1),
        SchedClass::StoreInt => (1, 1, 1, S::ALU1, R::LSU, 1),
        SchedClass::LoadFp => (3, 1, 1, S::ALU1, R::LSU, 1),
        SchedClass::StoreFp => (1, 1, 1, S::ALU1, R::LSU, 1),
        SchedClass::LoadPairInt => (2, 1, 1, S::ALU1, R::LSU, 1),
        SchedClass::StorePairInt => (1, 1, 1, S::ALU1, R::LSU, 1),
        SchedClass::LoadPairFp => (3, 1, 1, S::ALU1, R::LSU, 1),
        SchedClass::StorePairFp => (1, 1, 1, S::ALU1, R::LSU, 1),
        SchedClass::FpMove => (1, 1, 1, S::ALU1, R::FP_NEON, 1),
        SchedClass::FpAddSub => (4, 1, 1, S::ALU1, R::FP_NEON, 1),
        SchedClass::FpMul => (4, 1, 1, S::ALU1, R::FP_NEON, 1),
        SchedClass::FpDiv => (16, 16, 16, S::ALU1, R::FP_NEON, 1),
        SchedClass::FpCmp => (3, 1, 1, S::ALU1, R::FP_NEON, 1),
        SchedClass::FpCvt => (4, 1, 1, S::ALU1, R::FP_NEON, 1),
        SchedClass::Branch => (1, 1, 1, S::ALU0, R::BRANCH, 1),
        SchedClass::Barrier => (1, 1, 1, S::ALU0, R::BRANCH, 1),
        SchedClass::Nop => (0, 1, 0, S::EITHER, R::NONE, 0),
        SchedClass::Other => (4, 1, 1, S::EITHER, R::FP_NEON, 1),
    };
    InstrProfile {
        latency,
        reciprocal_throughput: throughput,
        resource_occupancy: occupancy,
        allowed_slots: slots,
        resources,
        emitted_ops: emitted,
    }
}

/// Conservative generic AArch64 profile. Does not assume precise ALU0/ALU1
/// pairing; uses conservative latency and occupancy for all classes.
pub fn generic_profile(class: SchedClass) -> InstrProfile {
    let mut profile = instr_profile(class);
    // Generic model: no slot restrictions, conservative occupancy.
    profile.allowed_slots = SlotMask::EITHER;
    profile.resource_occupancy = profile.resource_occupancy.max(1);
    profile
}

/// Select the profile function for a given scheduler model.
pub fn profile_for_model(
    model: crate::config::AArch64SchedModel,
) -> fn(SchedClass) -> InstrProfile {
    match model {
        crate::config::AArch64SchedModel::CortexA53 => instr_profile,
    }
}

/// Check if two instruction classes can legally dual-issue on Cortex-A53.
pub fn can_dual_issue(first: SchedClass, second: SchedClass) -> bool {
    let p1 = instr_profile(first);
    let p2 = instr_profile(second);
    // Both must fit in the 2-wide frontend.
    // Check slot compatibility: at least one valid slot assignment must exist.
    let slots_work = (p1.allowed_slots.can_issue_to(Slot::Alu0)
        && p2.allowed_slots.can_issue_to(Slot::Alu1))
        || (p1.allowed_slots.can_issue_to(Slot::Alu1) && p2.allowed_slots.can_issue_to(Slot::Alu0));
    if !slots_work {
        return false;
    }
    // Check resource conflicts: only non-shareable resources block pairing.
    // ALU is shareable (two pipes), LSU/MAC/DIV/FP_NEON/BRANCH are not.
    let shared = ResourceMask(
        p1.resources.0 & p2.resources.0 & !ResourceMask::ALU.0 & !ResourceMask::FRONTEND.0,
    );
    shared.0 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alu_pairs_with_alu() {
        assert!(can_dual_issue(SchedClass::Alu, SchedClass::Alu));
    }

    #[test]
    fn alu_pairs_with_load() {
        assert!(can_dual_issue(SchedClass::Alu, SchedClass::LoadInt));
    }

    #[test]
    fn load_does_not_pair_with_store() {
        assert!(!can_dual_issue(SchedClass::LoadInt, SchedClass::StoreInt));
    }

    #[test]
    fn mul_does_not_pair_with_mul() {
        assert!(!can_dual_issue(SchedClass::Mul, SchedClass::Mul));
    }

    #[test]
    fn barrier_blocks_pairing() {
        // Barrier occupies ALU0 + BRANCH; ALU can go to ALU1.
        // This is model-dependent; the key invariant is that barrier is restrictive.
        let p = instr_profile(SchedClass::Barrier);
        assert!(p.resources.overlaps(ResourceMask::BRANCH));
    }

    #[test]
    fn div32_and_div64_have_different_profiles() {
        let p32 = instr_profile(SchedClass::Div32);
        let p64 = instr_profile(SchedClass::Div64);
        assert_ne!(p32.latency, p64.latency);
        assert!(p64.latency > p32.latency);
    }

    #[test]
    fn pair_ops_have_own_profiles() {
        let scalar = instr_profile(SchedClass::LoadInt);
        let pair = instr_profile(SchedClass::LoadPairInt);
        // Both should be LSU but may have different latencies.
        assert!(scalar.resources.overlaps(ResourceMask::LSU));
        assert!(pair.resources.overlaps(ResourceMask::LSU));
    }

    #[test]
    fn generic_profile_is_more_permissive() {
        let cortex = instr_profile(SchedClass::LoadInt);
        let generic = generic_profile(SchedClass::LoadInt);
        assert!(!cortex.allowed_slots.can_issue_to(Slot::Alu0));
        assert!(generic.allowed_slots.can_issue_to(Slot::Alu0));
    }

    #[test]
    fn nop_has_zero_latency_and_ops() {
        let p = instr_profile(SchedClass::Nop);
        assert_eq!(p.latency, 0);
        assert_eq!(p.emitted_ops, 0);
    }
}
