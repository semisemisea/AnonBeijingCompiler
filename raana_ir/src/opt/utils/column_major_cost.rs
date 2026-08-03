const CACHE_LINE_BYTES: usize = 64;
const PAGE_BYTES: usize = 4096;
const MIN_ABSOLUTE_SAVING: u128 = 8;
const MIN_RELATIVE_SAVING_PERCENT: u128 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessCost {
    pub current_stride: usize,
    pub transposed_stride: usize,
    pub weight: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profitability {
    pub current_cost: u128,
    pub transposed_cost: u128,
    pub saving: u128,
    pub creates_unit_stride: bool,
}

fn stride_penalty(stride: usize, element_size: usize) -> u128 {
    debug_assert!(element_size > 0);
    let stride = stride.max(element_size);
    let lines = stride.div_ceil(CACHE_LINE_BYTES).max(1) as u128;
    let page_penalty = u128::from(stride >= PAGE_BYTES) * 4;
    1 + lines.min(64) + page_penalty
}

pub fn estimate(accesses: &[AccessCost], element_size: usize) -> Profitability {
    let mut current = 0_u128;
    let mut transposed = 0_u128;
    let mut creates_unit_stride = false;

    for access in accesses {
        let weight = access.weight;
        current = current.saturating_add(
            weight.saturating_mul(stride_penalty(access.current_stride, element_size)),
        );
        transposed = transposed.saturating_add(
            weight.saturating_mul(stride_penalty(access.transposed_stride, element_size)),
        );
        creates_unit_stride |=
            access.current_stride > element_size && access.transposed_stride == element_size;
    }

    Profitability {
        current_cost: current,
        transposed_cost: transposed,
        saving: current.saturating_sub(transposed),
        creates_unit_stride,
    }
}

pub fn is_profitable(accesses: &[AccessCost], element_size: usize) -> bool {
    let estimate = estimate(accesses, element_size);
    estimate.creates_unit_stride
        && estimate.current_cost > estimate.transposed_cost
        && estimate.saving >= MIN_ABSOLUTE_SAVING
        && estimate.saving.saturating_mul(100)
            >= estimate
                .current_cost
                .saturating_mul(MIN_RELATIVE_SAVING_PERCENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_repeated_strided_access_becoming_unit_stride() {
        assert!(is_profitable(
            &[AccessCost {
                current_stride: 256,
                transposed_stride: 4,
                weight: 64,
            }],
            4,
        ));
    }

    #[test]
    fn rejects_unit_stride_regression() {
        assert!(!is_profitable(
            &[AccessCost {
                current_stride: 4,
                transposed_stride: 256,
                weight: 64,
            }],
            4,
        ));
    }

    #[test]
    fn weighs_hot_improvement_against_cold_regression() {
        assert!(is_profitable(
            &[
                AccessCost {
                    current_stride: 256,
                    transposed_stride: 4,
                    weight: 100,
                },
                AccessCost {
                    current_stride: 4,
                    transposed_stride: 128,
                    weight: 1,
                },
            ],
            4,
        ));
    }

    #[test]
    fn reports_the_cost_breakdown() {
        let result = estimate(
            &[AccessCost {
                current_stride: 256,
                transposed_stride: 4,
                weight: 64,
            }],
            4,
        );

        assert!(result.current_cost > result.transposed_cost);
        assert_eq!(result.saving, result.current_cost - result.transposed_cost);
        assert!(result.creates_unit_stride);
    }
}
