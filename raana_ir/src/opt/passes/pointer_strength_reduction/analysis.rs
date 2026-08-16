//! Affine index-evolution and range analysis for pointer strength reduction.
//!
//! ---
//!
//! ## 中文说明
//!
//! PSR 的**分析**子模块：把候选 GEP 的索引值分类成三种演化形态
//! （`IndexEvolution`，定义在父文件），并求出仿射表达式的系数与偏移范围，供
//! `candidate` 子模块判定"步长每轮恒定、可安全改写"：
//!
//! - `Invariant`：全局 / 常量 / 循环外定义的值——每轮不变；
//! - `Direct`：索引就是本循环的归纳变量本身（系数 1）；
//! - `Affine(AffineI32Expr)`：索引是 IV 的仿射函数
//!   `value = coefficient * iv + offset`（`offset` 是区间
//!   `I64Range { min, max }`，含符号；`chain` 记录参与组合的指令链，
//!   `invariants` 记录需要移到 preheader 的循环不变量）。
//!
//! `classify` 沿指令树递归（`classify_index_evolution` 先做参数转发
//! `forwarded_params` 再分类）：
//!
//! | 组合 | 系数 / 偏移 |
//! |------|-------------|
//! | `lhs + rhs` | 系数相加、区间相加 |
//! | `lhs - rhs` | 系数相减、区间相减 |
//! | `lhs * rhs`（两者均不变量） | 不变量偏移 = 区间相乘（`row * runtime_width` 扁平行形态，整条乘积进 `chain` 供 preheader 克隆） |
//! | `lhs * const` | 系数 × 常量因子、区间 × 常量 |
//!
//! 减法/乘法用 `checked_*` 全程 i64 运算，任何一步溢出即返回 `None`
//! （放弃该候选）。`affine_range_fits_i32` 在改写前再确认仿射值域能落回
//! i32（地址计算不允许回绕语义被破坏）。
//!
//! 与兄弟子模块的协作：`candidate`（候选发现）调用本模块分类索引并做
//! 溢出检查；`rewrite`（改写）用 `evaluate_affine_initial` 计算 preheader
//! 里克隆的指针初值。正确性根基：仿射闭式 = 初值 + 系数 × 迭代数，只要
//! 初值、系数、步长逐轮一致，指针递进与每轮重算 GEP 逐轮相等。
//!
//! 验证：`tests.rs` 覆盖索引分类与范围边界；端到端 `make test` 差分。

use super::*;

impl PointerStrengthReduction {
    pub(super) fn classify_index_evolution(
        data: &ArenaContextMut<'_>,
        ranges: &RangeAnalysis,
        looop: &Loop,
        iv: Inst,
        iv_range: Option<ConstantInductionRange>,
        forwarded_params: &FxHashMap<Inst, Inst>,
        gep: Inst,
        value: Inst,
    ) -> Option<IndexEvolution> {
        let value = forwarded_params.get(&value).copied().unwrap_or(value);
        if value == iv {
            return Some(IndexEvolution::Direct);
        }
        if value.is_global()
            || data.inst_data(value).kind().is_const()
            || data
                .layout()
                .parent_bb(value)
                .is_none_or(|block| !looop.contains(block))
        {
            return Some(IndexEvolution::Invariant);
        }

        fn classify(
            data: &ArenaContextMut<'_>,
            ranges: &RangeAnalysis,
            looop: &Loop,
            iv: Inst,
            iv_range: Option<ConstantInductionRange>,
            forwarded_params: &FxHashMap<Inst, Inst>,
            gep: Inst,
            value: Inst,
        ) -> Option<AffineI32Expr> {
            let value = forwarded_params.get(&value).copied().unwrap_or(value);
            if value == iv {
                return Some(AffineI32Expr {
                    value,
                    coefficient: 1,
                    offset_range: I64Range { min: 0, max: 0 },
                    chain: SmallVec::new(),
                    invariants: SmallVec::new(),
                });
            }
            if value.is_global()
                || data.inst_data(value).kind().is_const()
                || data
                    .layout()
                    .parent_bb(value)
                    .is_none_or(|block| !looop.contains(block))
            {
                return Some(AffineI32Expr {
                    value,
                    coefficient: 0,
                    offset_range: I64Range::from_i32(ranges.range_before(gep, value))?,
                    chain: SmallVec::new(),
                    invariants: if data.inst_data(value).kind().is_const() {
                        SmallVec::new()
                    } else {
                        SmallVec::from_slice(&[value])
                    },
                });
            }
            if !data.inst_data(value).ty().is_i32() {
                return None;
            }
            let InstKind::Binary(binary) = data.inst_data(value).kind() else {
                return None;
            };
            let lhs = classify(
                data,
                ranges,
                looop,
                iv,
                iv_range,
                forwarded_params,
                gep,
                binary.lhs(),
            )?;
            let rhs = classify(
                data,
                ranges,
                looop,
                iv,
                iv_range,
                forwarded_params,
                gep,
                binary.rhs(),
            )?;
            let (coefficient, offset_range) = match binary.op() {
                BinaryOp::Add => (
                    lhs.coefficient.checked_add(rhs.coefficient)?,
                    lhs.offset_range.add(rhs.offset_range)?,
                ),
                BinaryOp::Sub => (
                    lhs.coefficient.checked_sub(rhs.coefficient)?,
                    lhs.offset_range.sub(rhs.offset_range)?,
                ),
                // A product of two loop-invariant values is itself an
                // invariant offset. Keep the product in the affine chain so
                // `clone_affine_initial` moves it to the preheader. This is
                // the common flattened-row form `row * runtime_width + iv`.
                BinaryOp::Mul if lhs.coefficient == 0 && rhs.coefficient == 0 => {
                    (0, lhs.offset_range.mul(rhs.offset_range)?)
                }
                BinaryOp::Mul if lhs.coefficient == 0 => {
                    let factor = PointerStrengthReduction::integer_constant(data, binary.lhs())?;
                    (
                        rhs.coefficient.checked_mul(i64::from(factor))?,
                        rhs.offset_range.mul(I64Range {
                            min: i64::from(factor),
                            max: i64::from(factor),
                        })?,
                    )
                }
                BinaryOp::Mul if rhs.coefficient == 0 => {
                    let factor = PointerStrengthReduction::integer_constant(data, binary.rhs())?;
                    (
                        lhs.coefficient.checked_mul(i64::from(factor))?,
                        lhs.offset_range.mul(I64Range {
                            min: i64::from(factor),
                            max: i64::from(factor),
                        })?,
                    )
                }
                BinaryOp::Shl if rhs.coefficient == 0 => {
                    let shift = PointerStrengthReduction::integer_constant(data, binary.rhs())?;
                    let factor = 1_i64.checked_shl(u32::try_from(shift).ok()?)?;
                    (
                        lhs.coefficient.checked_mul(factor)?,
                        lhs.offset_range.mul(I64Range {
                            min: factor,
                            max: factor,
                        })?,
                    )
                }
                _ => return None,
            };
            let depends_on_iv = lhs.coefficient != 0 || rhs.coefficient != 0;
            // With a constant induction range, both the intermediate wrap
            // proof and the i32 range fit keep the 32-bit offset arithmetic
            // sound. With a runtime bound (iv_range unknown) both checks are
            // skipped: the incremental scheme replaces that arithmetic with
            // 64-bit pointer adds, and the valid-input domain excludes i32
            // wraparound, so the incremental address sequence matches the
            // original linear one.
            if depends_on_iv
                && iv_range.is_some_and(|range| {
                    !ranges.proves_binary_no_signed_wrap(
                        binary.op(),
                        binary.lhs(),
                        binary.rhs(),
                        RangeContext::Before(gep),
                    ) || !PointerStrengthReduction::affine_range_fits_i32(
                        coefficient,
                        offset_range,
                        range,
                    )
                })
            {
                return None;
            }
            let mut chain = lhs.chain;
            for inst in rhs.chain {
                if !chain.contains(&inst) {
                    chain.push(inst);
                }
            }
            if !chain.contains(&value) {
                chain.push(value);
            }
            let mut invariants = lhs.invariants;
            for invariant in rhs.invariants {
                if !invariants.contains(&invariant) {
                    invariants.push(invariant);
                }
            }
            Some(AffineI32Expr {
                value,
                coefficient,
                offset_range,
                chain,
                invariants,
            })
        }

        let affine = classify(
            data,
            ranges,
            looop,
            iv,
            iv_range,
            forwarded_params,
            gep,
            value,
        )?;
        (affine.coefficient != 0).then_some(IndexEvolution::Affine(affine))
    }

    pub(super) fn affine_range_fits_i32(
        coefficient: i64,
        offset: I64Range,
        iv: ConstantInductionRange,
    ) -> bool {
        let iv_min = i64::from(iv.min());
        let iv_max = i64::from(iv.max());
        let (scaled_min, scaled_max) = if coefficient >= 0 {
            (
                coefficient.checked_mul(iv_min),
                coefficient.checked_mul(iv_max),
            )
        } else {
            (
                coefficient.checked_mul(iv_max),
                coefficient.checked_mul(iv_min),
            )
        };
        let (Some(scaled_min), Some(scaled_max)) = (scaled_min, scaled_max) else {
            return false;
        };
        let Some(min) = scaled_min.checked_add(offset.min) else {
            return false;
        };
        let Some(max) = scaled_max.checked_add(offset.max) else {
            return false;
        };
        min >= i64::from(i32::MIN) && max <= i64::from(i32::MAX)
    }

    pub(super) fn integer_constant(data: &ArenaContextMut<'_>, value: Inst) -> Option<i32> {
        match data.inst_data(value).kind() {
            InstKind::Integer(integer) => Some(integer.value()),
            _ => None,
        }
    }

    pub(super) fn evaluate_affine_initial(
        data: &ArenaContextMut<'_>,
        affine: &AffineI32Expr,
        iv: Inst,
        initial_iv: i32,
        forwarded_params: &FxHashMap<Inst, Inst>,
    ) -> Option<i32> {
        fn evaluate(
            data: &ArenaContextMut<'_>,
            chain: &[Inst],
            iv: Inst,
            initial_iv: i32,
            forwarded_params: &FxHashMap<Inst, Inst>,
            value: Inst,
        ) -> Option<i32> {
            let value = forwarded_params.get(&value).copied().unwrap_or(value);
            if value == iv {
                return Some(initial_iv);
            }
            if !chain.contains(&value) {
                return PointerStrengthReduction::integer_constant(data, value);
            }
            let InstKind::Binary(binary) = data.inst_data(value).kind() else {
                return None;
            };
            let lhs = evaluate(data, chain, iv, initial_iv, forwarded_params, binary.lhs())?;
            let rhs = evaluate(data, chain, iv, initial_iv, forwarded_params, binary.rhs())?;
            Some(match binary.op() {
                BinaryOp::Add => lhs.wrapping_add(rhs),
                BinaryOp::Sub => lhs.wrapping_sub(rhs),
                BinaryOp::Mul => lhs.wrapping_mul(rhs),
                BinaryOp::Shl => lhs.wrapping_shl(rhs as u32),
                _ => return None,
            })
        }

        evaluate(
            data,
            &affine.chain,
            iv,
            initial_iv,
            forwarded_params,
            affine.value,
        )
    }
}
