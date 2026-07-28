//! Cortex-A53 latency and issue-resource model.
//!
//! Numbers are from the *ARM Cortex-A53 Software Optimization Guide* (DUI 0901).
//! Each `SchedClass` maps to an `InstrProfile` giving the result latency and
//! issue constraints used by the list scheduler.

/// Functional-unit / pipeline class for one instruction.
///
/// The scheduler uses this to estimate when an instruction's result becomes
/// available (latency) and which issue slot it prefers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SchedClass {
    /// Integer ALU: `add`, `sub`, `and`, `orr`, `eor`, `lsl`, `lsr`, `asr`,
    /// `cmp`, `mov`, `movz`, `movk`, etc. Latency 1, dual-issueable.
    Alu,

    /// Integer multiply / multiply-accumulate: `mul`, `madd`, `msub`,
    /// `smull`. Latency 3, single MAC pipeline.
    Mul,

    /// Integer divide: `sdiv`. Latency 11 (32-bit), non-pipelined.
    Div,

    /// L1 load: `ldr`, `ldp`. Latency 2 — the primary optimization target.
    Load,

    /// Store: `str`, `stp`. Latency 1.
    Store,

    /// Branch / control flow: `b`, `bl`, `cbz`, `ret`. Latency 1.
    Branch,

    /// Barrier: `call`, `tailcall`. Must serialize — depends on all prior
    /// memory and register writes.
    Barrier,

    /// Pseudo / no-op. No latency, no resource.
    Nop,

    /// Floating-point or anything we don't model precisely yet.
    /// Treated conservatively as latency 4.
    Other,
}

/// Scheduling characteristics of one instruction.
#[derive(Clone, Copy, Debug)]
pub struct InstrProfile {
    /// Result latency in cycles. The scheduler won't issue a dependent
    /// instruction until `issue_cycle + latency` cycles have passed.
    pub latency: u32,
    /// Issue class — drives dual-issue and resource modelling.
    pub class: SchedClass,
}

/// Look up the scheduling profile for a [`SchedClass`] on Cortex-A53.
pub fn instr_profile(class: SchedClass) -> InstrProfile {
    let latency = match class {
        SchedClass::Alu => 1,
        SchedClass::Mul => 3,
        SchedClass::Div => 11,
        SchedClass::Load => 2,
        SchedClass::Store => 1,
        SchedClass::Branch => 1,
        SchedClass::Barrier => 1,
        SchedClass::Nop => 0,
        SchedClass::Other => 4,
    };
    InstrProfile { latency, class }
}
