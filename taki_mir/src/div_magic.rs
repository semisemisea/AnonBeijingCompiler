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
//!
//! ---
//!
//! # 补充说明（中文）
//!
//! 一句话定位：本模块在**编译期**为「有符号 32 位整数除以常数」生成 magic
//! number（乘数 + 移位量 + 修正项），把除法改写为 **加宽乘法 → 修正 →
//! 算术右移 → 向零取整** 的指令序列；真正的除法指令（AArch64 的 `sdiv` /
//! RISC-V 的 `divw`）不再出现在生成的代码里。
//!
//! ## magic number 是怎么算出来的（`signed_magic_i32`）
//!
//! 思路：对除数 `d` 找乘数 `m` 与移位量 `s`，使任意被除数 `n` 满足
//! `n / d == mulhs(n, m)`（64 位乘积的高 32 位）修正后再算术右移 `s` 位。
//! 即把「除以 `d`」近似为「乘以 `2^(32+s) / |d|` 的整数近似再右移」；
//! `s` 越大近似越精确，代价是乘数与移位成本。
//!
//! `signed_magic_i32` 的实现（Granlund–Montgomery 算法，Hacker's Delight
//! 2nd ed. 图 10-1 的形式）：
//!
//! 1. **排除便宜形态**：`|d| <= 1` 或 `d` 是 2 的幂 → 返回 `None`（交给
//!    更便宜的序列，见「与 sr.rs 的分工」）；
//! 2. **定边界**：`threshold = 2^31 + (d < 0 ? 1 : 0)` 是精度要求最苛刻的
//!    被除数绝对值（正除数时为 `|i32::MIN|`，负除数时为 `|i32::MAX| + 1`，
//!    商的位数最多）；`boundary = threshold - 1 - threshold % |d|` 是小于
//!    `threshold` 的最大非 `|d|` 倍数；
//! 3. **逐位逼近**：以 `2^31` 为被除数同时做 `2^31 / boundary` 与
//!    `2^31 / |d|` 两组长除法（`boundary_quotient`/`boundary_remainder` 与
//!    `quotient`/`remainder`，每轮 ×2 再归一，`power` 从 31 递增），直到
//!    `boundary_quotient > delta || (boundary_quotient == delta &&
//!    boundary_remainder != 0)`（`delta = |d| - remainder`）——这是「乘数
//!    对阈值内所有被除数都足够精确」的判据；
//! 4. **组装**：`multiplier = quotient + 1`（wrapping），`d < 0` 时取负；
//!    `shift = power - 32`。若 `quotient + 1` 超出有符号 32 位范围，代码用
//!    `multiplier - 2^32`（或 `multiplier + 2^32`）的有符号表示，表现为
//!    乘数与除数**异号**，并在乘高之后用被除数把差值补回——这正是
//!    `MagicCorrection`：除数为正而乘数为负 → `AddNumerator`；除数为负而
//!    乘数为正 → `SubNumerator`；否则 `None`。
//!
//! 结果就是 `SignedMagic { multiplier, shift, correction }`，语义为
//! `q = mulhs(n, multiplier) (+/- n) >> shift`，末位 `q + (q as u32 >> 31)`
//! 把负商向零取整（Rust / C 的截断除法语义）。
//!
//! ## 谁在用
//!
//! 两个后端的 lower 阶段遇到**常数除数**、且该除数既非 2 的幂也非 `0/±1`
//! 时调用 `signed_magic_i32`，把结果发射成指令序列。加宽乘法是机器相关的，
//! 所以指令序列归后端所有，本模块只出数：
//!
//! - `anon_armv8/src/lower/arith.rs` 的 `lower_signed_div_rem_magic`：
//!   `smull` 保留完整 64 位乘积，无修正项时乘高与 magic 移位合并成一次
//!   算术右移；
//! - `uika_riscv/src/lower.rs` 的 `lower_signed_div_rem_magic`：替代
//!   `divw`/`remw`。RISC-V 后端把 i32 值符号扩展保存在 64 位寄存器里，
//!   普通 `mul` 即得完整乘积、`srai` 取高半；取余由「商乘回除数再减」
//!   得到，避免第二段乘高序列（见 `docs/uika.md`、`docs/anon.md`）。
//!
//! `SignedMagic::apply` 用 i64 乘高在软件里镜像这条序列，供测试核对发射
//! 算术，不必经过汇编器。
//!
//! ## 正确性
//!
//! - 数学上：第 3 步的迭代判据保证乘数在 `boundary` 与 `threshold - 1`
//!   两个端点精确，而商函数在端点之间是分段线性的，故对阈值区间内的
//!   **每个**被除数都给出正确商（Hacker's Delight 的标准论证）；修正项
//!   恢复「乘数偏移 `2^32`」丢掉的贡献，末位的 `q + (q as u32 >> 31)`
//!   处理负商向零取整。
//! - 测试上：`mod tests` 用 `magic.apply(n) == n / divisor` 做差分验证——
//!   小除数全扫 `[-1024, 1024]`，大除数（`i32::MAX`、`(1 << 30) + 1`、
//!   `0x5555_5555`、`1_000_000_007` 等）各配一批被除数采样（边界值、
//!   除数倍数 ±1、LCG 伪随机铺满 i32 值域）；`known_multipliers_match_
//!   the_published_values` 把 3 / 7 / -7 的乘数与 Hacker's Delight 的
//!   发表值逐一对照。
//!
//! ## 与 `raana_ir` sr.rs 的分工
//!
//! 除数是 2 的幂时根本不需要乘法：`raana_ir/src/opt/passes/sr.rs` 的
//! `StrengthReduction` 在 IR 层把 `x / ±2^k` 改写成「算术右移 + bias 修正」
//! （`sign = x >> 31; bias = sign >>> (32-k); q = (x + bias) >> k`，保持
//! 向零取整），`x % ±2^k` 改写成掩码 + 符号修正；其放弃条件明确写着
//! 「除数不是 ±2^k 留给后端 magic-number 序列」。相应地 `signed_magic_i32`
//! 对 `|d| <= 1 || d.is_power_of_two()` 一律返回 `None`——magic number
//! 只处理**非 2 幂**的常数除数，两者互不重叠；`0/±1` 由 SR 的 `x / 1 → x`、
//! `x / -1 → 0 - x` 等规则消化。
//!
//! ## 验证
//!
//! - 本文件 `mod tests`：4 个单元测试，覆盖小 / 大除数、伪随机被除数、
//!   便宜形态的 `None` 与发表值对照；
//! - 端到端：`make test ARGS="-O 2"` 差分比对（真实质量门禁，见
//!   `AGENTS.md`）。

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
