use std::marker::PhantomData;

use crate::{
    reg_alloc::reg::{PReg, RegClass, SpillSlot},
    types::LoweredType,
    vcode::VCodeInst,
};
use rustc_hash::FxHashMap;

use crate::reg_alloc::reg::VReg;

pub const fn pinned_vreg_to_preg(vreg: VReg) -> Option<PReg> {
    if vreg.vreg() < PINNED_PREG {
        Some(PReg::from_index(vreg.vreg()))
    } else {
        None
    }
}

pub const fn preg_to_pinned_vreg(preg: PReg) -> VReg {
    VReg::new(preg.index(), preg.class())
}

/// The first 192 vregs (64 int, 64 float, 64 vec) are "pinned" to
/// physical registers. These must not be passed into the regalloc,
/// but they are used to represent physical registers in the same
/// `Reg` type post-regalloc.
pub const PINNED_PREG: usize = 192;

/// A register named in an instruction. This register can be a virtual
/// register, a fixed physical register, or a named spillslot (after
/// regalloc). It does not have any constraints applied to it: those
/// can be added later in `MachInst::get_operands()` when the `Reg`s
/// are converted to `Operand`s.
/// It seems to be a glue type for connecting `PReg`, `VReg` or a
/// named `SpillSlot`
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Reg(pub(crate) u32);

#[doc(hidden)]
const SPILLSLOT_BIT: u32 = 0x8000_0000;

const SPILLSLOT_MASK: u32 = !SPILLSLOT_BIT;

impl Reg {
    pub const fn from_virtual_reg(vreg: VReg) -> Reg {
        let bits = vreg.repr();
        assert!(bits < SPILLSLOT_MASK);
        Reg(bits)
    }

    pub const fn from_physical_reg(preg: PReg) -> Reg {
        let vreg = preg_to_pinned_vreg(preg);
        let bits = vreg.repr();
        assert!(bits < SPILLSLOT_MASK);
        Reg(bits)
    }

    pub fn from_spillslot(slot: SpillSlot) -> Reg {
        Reg(slot.raw_bits() | SPILLSLOT_BIT)
    }

    pub fn to_spillslot(self) -> Option<SpillSlot> {
        if (self.0 & SPILLSLOT_BIT) != 0 {
            Some(SpillSlot::new((self.0 & SPILLSLOT_MASK) as usize))
        } else {
            None
        }
    }

    /// Glue method
    pub fn to_real_reg(self) -> Option<PReg> {
        self.to_physical_reg()
    }

    pub fn to_physical_reg(self) -> Option<PReg> {
        pinned_vreg_to_preg(VReg::from_bits(self.0))
    }

    pub fn to_virtual_reg(self) -> Option<VReg> {
        if self.to_spillslot().is_some() {
            None
        } else if pinned_vreg_to_preg(VReg::from_bits(self.0)).is_none() {
            Some(VReg::from_bits(self.0))
        } else {
            None
        }
    }

    /// Get the class of this register.
    pub fn class(self) -> RegClass {
        assert!(!self.to_spillslot().is_some());
        VReg::from(self.0).class()
    }

    /// Is this a real (physical) reg?
    pub fn is_real(self) -> bool {
        self.to_physical_reg().is_some()
    }

    /// Is this a virtual reg?
    pub fn is_virtual(self) -> bool {
        self.to_virtual_reg().is_some()
    }

    /// Is this a spillslot?
    pub fn is_spillslot(self) -> bool {
        self.to_spillslot().is_some()
    }
}

impl core::fmt::Debug for Reg {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        if VReg::from(self.0) == VReg::invalid() {
            write!(f, "<invalid>")
        } else if let Some(spillslot) = self.to_spillslot() {
            write!(f, "{spillslot}")
        } else if let Some(rreg) = self.to_real_reg() {
            let preg: PReg = rreg.into();
            write!(f, "{preg}")
        } else if let Some(vreg) = self.to_virtual_reg() {
            let vreg: VReg = vreg.into();
            write!(f, "{vreg}")
        } else {
            unreachable!()
        }
    }
}

impl AsMut<Reg> for Reg {
    fn as_mut(&mut self) -> &mut Reg {
        self
    }
}

#[derive(Debug, Default)]
pub struct VRegAllocator<I: VCodeInst> {
    pub vreg_types: Vec<LoweredType>,
    vreg_alias: FxHashMap<VReg, VReg>,
    _marker: PhantomData<I>,
}

impl<I: VCodeInst> VRegAllocator<I> {
    pub fn with_capaticy(cap: usize) -> VRegAllocator<I> {
        let capacity = PINNED_PREG + cap;
        let mut vreg_types = Vec::with_capacity(capacity);
        vreg_types.resize(PINNED_PREG, LoweredType::invalid());
        VRegAllocator {
            vreg_types,
            vreg_alias: FxHashMap::default(),
            _marker: PhantomData,
        }
    }

    pub fn alloc(&mut self, ty: LoweredType) -> Reg {
        let len = self.vreg_types.len();
        let (&[regclass], &[ty]) = I::rc_for_type(ty) else {
            // INFO: Since we only have to deal with i32, f32, u64 and self defined SIMD vector,
            // the method, compared with cranelift's original one, is determined to return a
            // one-element slice.
            // So the pattern match is irrefutable.
            unreachable!("rc_for_type method should only return one element for now.")
        };
        assert!(len < VReg::MAX);

        let reg = Reg::from_virtual_reg(VReg::new(len, regclass));

        let vreg = reg.to_virtual_reg().unwrap();
        debug_assert_eq!(vreg.vreg(), len);
        self.vreg_types.push(ty);
        reg
    }

    pub fn set_reg_alias(&mut self, from: Reg, to: Reg) {
        let from = from.into();
        let resolved_to = self.resolve_alias(to.into());

        assert_ne!(from, resolved_to);

        let old_alias = self.vreg_alias.insert(from, resolved_to);
        debug_assert_eq!(old_alias, None);
    }

    pub fn resolve_alias(&self, mut vreg: VReg) -> VReg {
        while let Some(alias) = self.vreg_alias.get(&vreg) {
            vreg = *alias;
        }
        vreg
    }

    #[inline]
    pub fn assert_no_vreg_aliases(&self, mut list: impl Iterator<Item = VReg>) {
        assert!(list.all(|vreg| !self.vreg_alias.contains_key(&vreg)));
    }
}

impl From<u32> for VReg {
    fn from(bits: u32) -> Self {
        Self::from_bits(bits)
    }
}

impl From<Reg> for VReg {
    fn from(reg: Reg) -> Self {
        reg.0.into()
    }
}

impl From<VReg> for Reg {
    fn from(value: VReg) -> Self {
        Reg::from_virtual_reg(value)
    }
}

impl From<PReg> for Reg {
    fn from(value: PReg) -> Self {
        Reg::from_physical_reg(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Writable<T> {
    pub(crate) reg: T,
}

impl<T> Writable<T> {
    pub const fn from_reg(reg: T) -> Writable<T> {
        Writable { reg }
    }

    pub fn to_reg(self) -> T {
        self.reg
    }

    pub fn reg_mut(&mut self) -> &mut T {
        &mut self.reg
    }

    pub fn map<U>(self, f: impl Fn(T) -> U) -> Writable<U> {
        Writable { reg: f(self.reg) }
    }
}
