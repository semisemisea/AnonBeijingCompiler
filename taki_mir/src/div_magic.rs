//! Magic numbers for signed division by a constant.
//!
//! A signed 32-bit division by a compile-time constant can be replaced by a
//! widening multiply, an optional correction, and two shifts. The multiplier
//! and shift amount are computed by the algorithm of Granlund and Montgomery,
//! in the form given by *Hacker's Delight* (2nd ed., figure 10-1).
//!
//! Backends own the instruction sequence because the widening multiply is
//! machine specific; this module only owns the number theory, which is shared
//! and easy to get subtly wrong.

/// The extra term a backend must add before the final shifts.
///
/// The multiplier does not always fit in a signed 32-bit word, in which case
/// the algorithm uses `multiplier - 2^32` (or `multiplier + 2^32`) and repairs
/// the difference with the numerator itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MagicCorrection {
    /// The multiply-high result is already the value to shift.
    None,
    /// Add the numerator to the multiply-high result.
    AddNumerator,
    /// Subtract the numerator from the multiply-high result.
    SubNumerator,
}

/// A replacement sequence for `numerator / divisor`, evaluated as
/// `q = mulhs(numerator, multiplier) (+/- numerator) >> shift`, followed by
/// `q + (q as u32 >> 31)` to round the negative quotient toward zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedMagic {
    /// Multiplier of the signed multiply-high step.
    pub multiplier: i32,
    /// Arithmetic right shift applied after the correction, `0..=31`.
    pub shift: u8,
    /// Correction applied between the multiply-high and the shift.
    pub correction: MagicCorrection,
}

impl SignedMagic {
    /// Evaluates the sequence this magic number describes.
    ///
    /// Backends emit the steps as machine instructions; this mirrors them so
    /// that tests can check the emitted arithmetic against `numerator /
    /// divisor` without going through an assembler.
    pub fn apply(&self, numerator: i32) -> i32 {
        let product = i64::from(numerator) * i64::from(self.multiplier);
        let high = (product >> 32) as i32;
        let corrected = match self.correction {
            MagicCorrection::None => high,
            MagicCorrection::AddNumerator => high.wrapping_add(numerator),
            MagicCorrection::SubNumerator => high.wrapping_sub(numerator),
        };
        let shifted = corrected >> self.shift;
        shifted.wrapping_add(((shifted as u32) >> 31) as i32)
    }
}

/// Computes the magic number for a signed 32-bit division by `divisor`.
///
/// Returns `None` when the division has a cheaper shape that backends handle
/// before reaching for a multiply: `0`, `1`, `-1`, and powers of two.
pub fn signed_magic_i32(divisor: i32) -> Option<SignedMagic> {
    const TWO31: u32 = 0x8000_0000;

    let magnitude = divisor.unsigned_abs();
    if magnitude <= 1 || magnitude.is_power_of_two() {
        return None;
    }

    // Magnitude of the most negative (for `divisor > 0`, the largest positive)
    // numerator whose quotient the multiplier is not required to round.
    let threshold = TWO31 + ((divisor as u32) >> 31);
    let boundary = threshold - 1 - threshold % magnitude;

    // Quotients and remainders of `2^p` divided by `boundary` and `magnitude`,
    // grown one bit per iteration until the multiplier is accurate for every
    // numerator.
    let mut power = 31u32;
    let mut boundary_quotient = TWO31 / boundary;
    let mut boundary_remainder = TWO31 - boundary_quotient * boundary;
    let mut quotient = TWO31 / magnitude;
    let mut remainder = TWO31 - quotient * magnitude;
    loop {
        power += 1;
        boundary_quotient *= 2;
        boundary_remainder *= 2;
        if boundary_remainder >= boundary {
            boundary_quotient += 1;
            boundary_remainder -= boundary;
        }
        quotient *= 2;
        remainder *= 2;
        if remainder >= magnitude {
            quotient += 1;
            remainder -= magnitude;
        }
        let delta = magnitude - remainder;
        if boundary_quotient > delta || (boundary_quotient == delta && boundary_remainder != 0) {
            break;
        }
    }

    let mut multiplier = quotient.wrapping_add(1) as i32;
    if divisor < 0 {
        multiplier = multiplier.wrapping_neg();
    }
    let correction = match (divisor > 0, multiplier > 0) {
        (true, false) => MagicCorrection::AddNumerator,
        (false, true) => MagicCorrection::SubNumerator,
        _ => MagicCorrection::None,
    };

    Some(SignedMagic {
        multiplier,
        shift: (power - 32) as u8,
        correction,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numerators(divisor: i32) -> Vec<i32> {
        let mut values = vec![0, 1, -1, i32::MAX, i32::MIN, i32::MAX - 1, i32::MIN + 1];
        for step in 1..64i64 {
            for base in [
                i64::from(divisor) * step,
                i64::from(i32::MAX) - i64::from(divisor) * step,
                i64::from(i32::MIN) + i64::from(divisor) * step,
            ] {
                values.extend(
                    [base - 1, base, base + 1]
                        .into_iter()
                        .filter(|value| i32::try_from(*value).is_ok())
                        .map(|value| value as i32),
                );
            }
        }
        // A cheap deterministic spread over the whole numerator range.
        let mut state = 0x2545_f491u32;
        for _ in 0..512 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            values.push(state as i32);
        }
        values
    }

    #[test]
    fn magic_sequence_matches_division_for_every_small_divisor() {
        for divisor in -1024..=1024 {
            let Some(magic) = signed_magic_i32(divisor) else {
                assert!(divisor.unsigned_abs() <= 1 || divisor.unsigned_abs().is_power_of_two());
                continue;
            };
            assert!(magic.shift < 32);
            for numerator in numerators(divisor) {
                assert_eq!(
                    magic.apply(numerator),
                    numerator / divisor,
                    "{numerator} / {divisor}"
                );
            }
        }
    }

    #[test]
    fn magic_sequence_matches_division_for_large_divisors() {
        let mut divisors = vec![
            i32::MAX,
            i32::MAX - 1,
            i32::MIN + 1,
            i32::MIN + 2,
            (1 << 30) + 1,
            -((1 << 30) + 1),
            0x5555_5555,
            -0x5555_5555,
            1_000_000_007,
            -1_000_000_007,
        ];
        let mut state = 0x1234_5678u32;
        for _ in 0..256 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            divisors.push(state as i32);
        }
        for divisor in divisors {
            let Some(magic) = signed_magic_i32(divisor) else {
                continue;
            };
            for numerator in numerators(divisor) {
                assert_eq!(
                    magic.apply(numerator),
                    numerator / divisor,
                    "{numerator} / {divisor}"
                );
            }
        }
    }

    #[test]
    fn cheaper_divisors_are_left_to_the_shift_sequences() {
        for divisor in [0, 1, -1, 2, -2, 1024, -1024, 1 << 30, i32::MIN] {
            assert_eq!(signed_magic_i32(divisor), None, "{divisor}");
        }
    }

    #[test]
    fn known_multipliers_match_the_published_values() {
        let three = signed_magic_i32(3).unwrap();
        assert_eq!(three.multiplier, 0x5555_5556);
        assert_eq!(three.shift, 0);
        assert_eq!(three.correction, MagicCorrection::None);

        let seven = signed_magic_i32(7).unwrap();
        assert_eq!(seven.multiplier, 0x9249_2493u32 as i32);
        assert_eq!(seven.shift, 2);
        assert_eq!(seven.correction, MagicCorrection::AddNumerator);

        let minus_seven = signed_magic_i32(-7).unwrap();
        assert_eq!(minus_seven.multiplier, 0x6db6_db6du32 as i32);
        assert_eq!(minus_seven.shift, 2);
        assert_eq!(minus_seven.correction, MagicCorrection::SubNumerator);
    }
}
