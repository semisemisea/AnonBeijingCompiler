//! AArch64-first profitability estimates for pointer strength reduction.
//!
//! TODO: Replace the temporary Cortex-A53 defaults with target policy injected
//! by the optimization driver once target-aware optimizer configuration exists.

use crate::{
    ir::{InstKind, TypeKind, arena::Arena},
    opt::{
        analysis_passes::loop_analysis::Loop,
        pass::ArenaContextMut,
        utils::{cfg::CFG, logical_edge::outgoing_edges},
    },
};

const MAX_BREAK_EVEN_TRIPS: usize = 4;
const MAX_POINTER_RECURRENCES_AFTER_TRANSFORM: usize = 3;
const MAX_HEADER_GPR_PARAMS_AFTER_TRANSFORM: usize = 6;
const MAX_BACKEDGE_SOURCES: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerStrengthReductionCost {
    pub old_iteration_insts: usize,
    pub pointer_update_insts: usize,
    pub setup_insts: usize,
}

impl PointerStrengthReductionCost {
    pub fn iteration_saving(self) -> usize {
        self.old_iteration_insts
            .saturating_sub(self.pointer_update_insts)
    }

    pub fn break_even_trips(self) -> Option<usize> {
        let saving = self.iteration_saving();
        (saving != 0).then(|| self.setup_insts.div_ceil(saving))
    }

    pub fn is_profitable(self) -> bool {
        self.break_even_trips()
            .is_some_and(|trips| trips <= MAX_BREAK_EVEN_TRIPS)
    }

    pub fn is_better_than(self, other: Self) -> bool {
        self.iteration_saving() > other.iteration_saving()
            || (self.iteration_saving() == other.iteration_saving()
                && self.setup_insts < other.setup_insts)
    }
}

pub fn estimate_aarch64_pointer_strength_reduction(
    data: &ArenaContextMut<'_>,
    cfg: &CFG,
    looop: &Loop,
    gep: &crate::ir::GetElemPtr,
    pointer_byte_delta: i64,
    removable_derived_insts: usize,
    derived_setup_insts: usize,
) -> Option<PointerStrengthReductionCost> {
    if loop_has_call(data, looop)
        || looop.latches().len() > MAX_BACKEDGE_SOURCES
        || !pressure_allows_pointer(
            pointer_recurrence_count(data, looop),
            header_gpr_parameter_count(data, looop),
        )
    {
        return None;
    }

    let old_iteration_insts = aarch64_gep_cost(data, gep)?.checked_add(removable_derived_insts)?;
    let pointer_update_insts = aarch64_add_offset_cost(pointer_byte_delta);

    let (entry_count, preheader_penalty) = match looop.get_preheader(cfg) {
        Some(preheader) => {
            let entry_count = outgoing_edges(data, preheader)
                .into_iter()
                .filter(|edge| edge.target(data) == looop.header())
                .count();
            (entry_count, 0)
        }
        None => (1, 1),
    };
    if entry_count == 0 {
        return None;
    }

    Some(PointerStrengthReductionCost {
        old_iteration_insts,
        pointer_update_insts,
        setup_insts: old_iteration_insts
            .checked_mul(entry_count)?
            .checked_add(preheader_penalty)?
            .checked_add(derived_setup_insts.checked_mul(entry_count)?)?,
    })
}

fn pressure_allows_pointer(
    current_pointer_recurrences: usize,
    current_header_gpr_params: usize,
) -> bool {
    current_pointer_recurrences
        .checked_add(1)
        .is_some_and(|count| count <= MAX_POINTER_RECURRENCES_AFTER_TRANSFORM)
        && current_header_gpr_params
            .checked_add(1)
            .is_some_and(|count| count <= MAX_HEADER_GPR_PARAMS_AFTER_TRANSFORM)
}

fn loop_has_call(data: &ArenaContextMut<'_>, looop: &Loop) -> bool {
    looop.body().iter().any(|&block| {
        data.layout().basicblock(block).insts().iter().any(|&inst| {
            matches!(
                data.inst_data(inst).kind(),
                InstKind::Call(..) | InstKind::TailCall(..)
            )
        })
    })
}

fn pointer_recurrence_count(data: &ArenaContextMut<'_>, looop: &Loop) -> usize {
    data.bb_data(looop.header())
        .params()
        .iter()
        .filter(|&&parameter| data.inst_data(parameter).ty().is_pointer())
        .count()
}

fn header_gpr_parameter_count(data: &ArenaContextMut<'_>, looop: &Loop) -> usize {
    data.bb_data(looop.header())
        .params()
        .iter()
        .filter(|&&parameter| {
            let ty = data.inst_data(parameter).ty();
            ty.is_i32() || ty.is_pointer()
        })
        .count()
}

fn aarch64_gep_cost(data: &ArenaContextMut<'_>, gep: &crate::ir::GetElemPtr) -> Option<usize> {
    let mut current_ty = data.inst_data(gep.base()).ty().clone();
    let mut dynamic_cost = 0_usize;
    let mut constant_offset = 0_i64;

    for &index in gep.offsets() {
        current_ty = match current_ty.kind() {
            TypeKind::Pointer(element) | TypeKind::Array(element, _) => element.clone(),
            _ => return None,
        };
        let stride = u64::try_from(current_ty.size()).ok()?;

        match data.inst_data(index).kind() {
            InstKind::Integer(integer) => {
                let contribution =
                    i64::from(integer.value()).checked_mul(i64::try_from(stride).ok()?)?;
                constant_offset = constant_offset.checked_add(contribution)?;
            }
            _ => dynamic_cost = dynamic_cost.checked_add(aarch64_dynamic_term_cost(stride))?,
        }
    }

    if constant_offset != 0 {
        dynamic_cost = dynamic_cost.checked_add(aarch64_add_offset_cost(constant_offset))?;
    }
    Some(dynamic_cost)
}

fn aarch64_dynamic_term_cost(stride: u64) -> usize {
    if matches!(stride, 1 | 2 | 4 | 8 | 16) {
        1
    } else if stride.is_power_of_two() {
        3
    } else {
        4
    }
}

fn aarch64_add_offset_cost(offset: i64) -> usize {
    let magnitude = offset.unsigned_abs();
    if magnitude <= 0xfff || (magnitude & 0xfff == 0 && magnitude >> 12 <= 0xfff) {
        // A 12-bit (or 12-bit scaled by 12) immediate fits the add directly.
        1
    } else {
        // The backend materializes the constant with a move-wide sequence
        // (`movz`/`movn` seed plus `movk` patches) followed by one add.
        move_wide_cost(magnitude) + 1
    }
}

/// Number of instructions to materialize `value` into a 64-bit register using
/// the backend's `plan_integer_constant` move-wide scheme: one seed
/// (`movz`/`movn`) plus a `movk` per remaining chunk that differs from the
/// seed. Mirrors `anon_armv8::constants::plan_integer_constant`'s cost.
fn move_wide_cost(value: u64) -> usize {
    if value == 0 {
        return 1;
    }
    let chunks = [
        value as u16,
        (value >> 16) as u16,
        (value >> 32) as u16,
        (value >> 48) as u16,
    ];
    let zero_cost = chunks.iter().filter(|&&chunk| chunk != 0).count();
    let ones_cost = chunks.iter().filter(|&&chunk| chunk != u16::MAX).count();
    let use_movn = ones_cost < zero_cost;
    let seed = if use_movn { u16::MAX } else { 0 };
    // Seed + one `movk` per chunk that still differs from the seed.
    1 + chunks
        .iter()
        .filter(|&&chunk| chunk != seed)
        .count()
        .saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_a_real_per_iteration_saving() {
        let neutral = PointerStrengthReductionCost {
            old_iteration_insts: 1,
            pointer_update_insts: 1,
            setup_insts: 1,
        };
        assert_eq!(neutral.break_even_trips(), None);
        assert!(!neutral.is_profitable());

        let profitable = PointerStrengthReductionCost {
            old_iteration_insts: 4,
            pointer_update_insts: 1,
            setup_insts: 8,
        };
        assert_eq!(profitable.break_even_trips(), Some(3));
        assert!(profitable.is_profitable());
    }

    #[test]
    fn ranks_iteration_savings_before_setup_cost() {
        let lower_saving = PointerStrengthReductionCost {
            old_iteration_insts: 4,
            pointer_update_insts: 1,
            setup_insts: 1,
        };
        let higher_saving = PointerStrengthReductionCost {
            old_iteration_insts: 7,
            pointer_update_insts: 1,
            setup_insts: 7,
        };
        assert!(higher_saving.is_better_than(lower_saving));
        assert!(!lower_saving.is_better_than(higher_saving));

        let lower_setup = PointerStrengthReductionCost {
            old_iteration_insts: 4,
            pointer_update_insts: 1,
            setup_insts: 3,
        };
        let higher_setup = PointerStrengthReductionCost {
            setup_insts: 4,
            ..lower_setup
        };
        assert!(lower_setup.is_better_than(higher_setup));
        assert!(!higher_setup.is_better_than(lower_setup));
        assert!(!lower_setup.is_better_than(lower_setup));
    }

    #[test]
    fn projects_pointer_pressure_after_the_transformation() {
        assert!(pressure_allows_pointer(2, 5));
        assert!(!pressure_allows_pointer(3, 5));
        assert!(!pressure_allows_pointer(2, 6));
        assert!(!pressure_allows_pointer(usize::MAX, 0));
        assert!(!pressure_allows_pointer(0, usize::MAX));
    }

    #[test]
    fn models_cortex_a53_gep_recipes() {
        assert_eq!(aarch64_dynamic_term_cost(4), 1);
        assert_eq!(aarch64_dynamic_term_cost(128), 3);
        assert_eq!(aarch64_dynamic_term_cost(12), 4);
        assert_eq!(aarch64_add_offset_cost(4095), 1);
        assert_eq!(aarch64_add_offset_cost(-4095), 1);
        assert_eq!(aarch64_add_offset_cost(4096), 1);
        assert_eq!(aarch64_add_offset_cost(64), 1);
        assert_eq!(aarch64_add_offset_cost(-64), 1);
        // 4097 = 0x1001: a single movz seed (one nonzero chunk) plus one add.
        assert_eq!(aarch64_add_offset_cost(4097), 2);
        // 0x1234_5678 needs a movz seed plus one movk patch plus one add.
        assert_eq!(aarch64_add_offset_cost(0x12345678), 3);
    }
}
