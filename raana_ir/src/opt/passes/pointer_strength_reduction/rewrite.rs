//! Preheader cloning and loop rewrites for pointer strength reduction.
//!
//! ---
//!
//! # rewrite：改写实施（preheader 克隆初值 + 指针递进）
//!
//! 本文件是 `PointerStrengthReduction`（PSR）的**改写实施**子模块：把
//! `analysis` 分类出的仿射索引、`candidate` 发现的 GEP 候选，真正改写为
//! "指针递进"形态——循环里每轮重算 `gep base, [i*c + off]` 变成"上一轮
//! 地址 + 常量字节步长"，把每轮的乘加链换成一条地址增量（后端可融合进访存
//! 寻址）。术语（header / latch / preheader / backedge / 归纳变量 IV /
//! 仿射 / GEP / block 参数）：见 `docs/offline-handbook/glossary.md`。
//!
//! ## 变换形态（IR 示例）
//!
//! 以仿射索引 `idx = i*2 + 1` 的 GEP 为例（`i` 是 IV，每轮 `i' = i + 1`）：
//!
//! ```text
//! 前（每轮重算地址）：
//!   pre:          jump header(i0)
//!   header(i):    idx = i*2 + 1
//!                 p   = gep base, [idx]
//!                 v   = load p
//!                 i'  = i + 1;  jump header(i')
//!
//! 后（本文件的三处改写）：
//!   pre:          t0  = i0*2 + 1            // ① clone_affine_initial：仿射链克隆进 preheader
//!                 p0  = gep base, [t0]      //    初值指针 = 第 0 轮的 GEP 地址
//!                 jump header(i0, p0)
//!   header(i, p): v   = load p              // ② header 新增指针参数 p
//!                 p'  = gep p, [step]       // ③ backedge：指针 + 常量步长（字节）
//!                 i'  = i + 1;  jump header(i', p')
//! ```
//!
//! 改写后原 header 里的 `idx` / `p` 计算成为死代码（留给 DCE），循环体只读
//! 携带的指针 `p`。
//!
//! ## 改写步骤（`apply_candidate`，按代码顺序）
//!
//! 1. `ensure_preheader`：取（或新建）循环唯一入口 preheader，失败则放弃
//!    （`ApplyResult::Unchanged`）；筛出 preheader → header 的 entry edges，
//!    任一入边参数个数与 header 参数个数不符即放弃；
//! 2. 逐 entry edge 求**常量初值**：取 `initial_iv`（entry edge 上 IV 参数
//!    的值），若它是常量整数，则按 `IndexEvolution` 求每个偏移的常量值
//!    （`Invariant` 要求偏移本身是常量、`Direct` 取 `initial_iv`、`Affine`
//!    走 `evaluate_affine_initial`），再用 `gep_constant_offsets_fit` 验证
//!    常量偏移落在 base 各维元素个数内（越界则放弃）；
//! 3. 逐 entry edge 在 preheader 构造**初始指针**：按演化替换 offsets
//!    （`Invariant` 且偏移是 header 参数 → 换成该边对应参数值；`Direct` →
//!    `initial_iv`；`Affine` → 常量字面量，或 `clone_affine_initial` 把仿射
//!    链克隆进 preheader），`get_elem_ptr(base, offsets)` 插到 preheader
//!    终结符前，登记进 `LogicalEdgeRewriter`；
//! 4. `add_param` 给 header 增加**指针参数**（类型 `candidate.pointer_ty`），
//!    并生成常量 `pointer_step`（每轮字节步长）；
//! 5. 按 `BackedgeGroup`（同一 latch 的多条 backedge 共享一个增量）：latch
//!    终结符前插 `next_pointer = gep pointer, [pointer_step]`，组内所有
//!    backedge 都传它；
//! 6. `rewrites.apply()` 原子提交全部边参数改写（断言成功）；
//! 7. `replace_inst_with(candidate.gep)`：把原 GEP 替换为 `gep pointer, [0]`
//!    ——循环体内对原 GEP 的使用改读携带的指针，返回 `ApplyResult::Changed`。
//!
//! ## 触发 / 放弃条件
//!
//! - 触发前提由兄弟模块保证：`analysis::classify_index_evolution` 把 GEP 的
//!   每个索引分类为 `IndexEvolution::Invariant` / `Direct` / `Affine`；
//!   `candidate::find_candidate` 产出 `Candidate`（GEP 只被循环内 load/store
//!   使用、仿射步长每轮恒定等）。
//! - 本文件的检查：`apply_candidate` 开头 `debug_assert_ne!` 要求仿射系数
//!   非零（索引真的随 IV 变化才值得携带指针）；`ensure_preheader` 失败、
//!   边参数个数不齐、`gep_constant_offsets_fit` 不通过都会直接放弃。
//! - 放弃：函数无循环或声明函数（`run_on` 里 `cfg.is_acyclic()` /
//!   `is_decl()` 直接返回，根本到不了本文件）、入口边参数不一致、初值
//!   常量偏移越界。
//!
//! ## 正确性
//!
//! - **归纳论证**：初始指针 = 第 0 轮 GEP 地址（preheader 里用 `initial_iv`
//!   对仿射闭式求值，或 `clone_affine_initial` 克隆链）；每轮 backedge 前进
//!   `pointer_step` 字节，与逐轮重算 GEP 的地址增量一致（`pointer_step` 由
//!   系数 × IV 步长 × 元素大小推出，`affine_range_fits_i32` 保证 i32 不
//!   回绕），故第 k 轮携带指针 == 第 k 轮重算的 GEP 地址；
//! - **不逃逸**：改写要求地址只在循环内使用，循环外看不到中间指针；
//! - **不变 header 参数替换**：`candidate` 侧验证每条 backedge 原样传该
//!   参数 ⇒ 首轮值 == preheader 边参数，初值指针里用它代入是安全的；
//! - **原子性**：边参数改写全部经 `LogicalEdgeRewriter` 延迟到 `apply`
//!   一次性提交，不会留下参数个数不一致的中间状态。
//!
//! ## 管线位置
//!
//! - 父 pass `PointerStrengthReduction` 注册于 `opt/pass.rs` 的
//!   `PassesManager::from_config`，fixpoint 段，`dse` 之后、
//!   `guard_elimination` 之前（内存画像稳定后改写地址）；
//! - `run_on` 主循环：`LoopAnalysis` / IV / range 分析 → 逐循环
//!   `find_candidate` → `apply_candidate`；本文件改写会破坏 CFG/支配/循环
//!   快照，故 `Changed` 后 `break` 重跑整轮，直到无可改写循环。
//!
//! ## 与兄弟子模块的协作
//!
//! - `analysis.rs`：索引演化分类（`classify_index_evolution`）产出
//!   `IndexEvolution` 与 `AffineI32Expr`（系数 / 偏移范围 / `chain` /
//!   `invariants`）；`evaluate_affine_initial` 求常量初值；
//! - `candidate.rs`：`find_candidate` 产出 `Candidate`（GEP、IV、
//!   `backedge_groups`、base、offsets、`pointer_ty`、`address_evolution`、
//!   `forwarded_params`）；本文件只消费不重新分析——`clone_affine_initial`
//!   用 `affine.chain`，`apply_candidate` 用 `address_evolution` 的
//!   indices / coefficient / `pointer_step` 与 `backedge_groups`。
//!
//! ## 验证
//!
//! - 本 pass 的 `mod tests` 委托 `tests.rs`，改写相关单测覆盖前向/反向/非
//!   单位步长、仿射索引、双 latch 共享增量、转发块、新建 preheader 等场景。

use super::*;

impl PointerStrengthReduction {
    pub(super) fn clone_affine_initial(
        data: &mut ArenaContextMut<'_>,
        preheader: BasicBlock,
        affine: &AffineI32Expr,
        iv: Inst,
        initial_iv: Inst,
        header_param_positions: &FxHashMap<Inst, usize>,
        edge_args: &[Inst],
        forwarded_params: &FxHashMap<Inst, Inst>,
    ) -> Inst {
        fn clone_value(
            data: &mut ArenaContextMut<'_>,
            preheader: BasicBlock,
            chain: &[Inst],
            iv: Inst,
            initial_iv: Inst,
            header_param_positions: &FxHashMap<Inst, usize>,
            edge_args: &[Inst],
            forwarded_params: &FxHashMap<Inst, Inst>,
            value: Inst,
        ) -> Inst {
            let value = forwarded_params.get(&value).copied().unwrap_or(value);
            if value == iv {
                return initial_iv;
            }
            if !chain.contains(&value) {
                if let Some(&parameter_position) = header_param_positions.get(&value) {
                    return edge_args[parameter_position];
                }
                return value;
            }
            let InstKind::Binary(binary) = data.inst_data(value).kind() else {
                unreachable!("affine chains contain only binary instructions")
            };
            let (op, lhs, rhs) = (binary.op(), binary.lhs(), binary.rhs());
            let lhs = clone_value(
                data,
                preheader,
                chain,
                iv,
                initial_iv,
                header_param_positions,
                edge_args,
                forwarded_params,
                lhs,
            );
            let rhs = clone_value(
                data,
                preheader,
                chain,
                iv,
                initial_iv,
                header_param_positions,
                edge_args,
                forwarded_params,
                rhs,
            );
            let cloned = data.new_local_value().binary(op, lhs, rhs);
            data.layout_mut()
                .insert_before_terminator(preheader, cloned);
            cloned
        }

        clone_value(
            data,
            preheader,
            &affine.chain,
            iv,
            initial_iv,
            header_param_positions,
            edge_args,
            forwarded_params,
            affine.value,
        )
    }

    pub(super) fn apply_candidate(
        data: &mut ArenaContextMut<'_>,
        cfg: &CFG,
        looop: &Loop,
        candidate: Candidate,
    ) -> ApplyResult {
        debug_assert_ne!(candidate.address_evolution.coefficient, 0);
        let preheader = match ensure_preheader(data, cfg, looop) {
            Some(EnsurePreheader::Existing(preheader) | EnsurePreheader::Created(preheader)) => {
                preheader
            }
            None => return ApplyResult::Unchanged,
        };

        let entry_edges = outgoing_edges(data, preheader)
            .into_iter()
            .filter(|edge| edge.target(data) == looop.header())
            .collect::<Vec<_>>();
        if entry_edges.is_empty()
            || entry_edges
                .iter()
                .any(|edge| edge.args(data).len() != data.bb_data(looop.header()).params().len())
        {
            return ApplyResult::Unchanged;
        }

        let mut initial_indices_by_edge = Vec::with_capacity(entry_edges.len());
        for edge in entry_edges {
            let initial_iv = edge.args(data)[candidate.header_iv_position];
            let constant_offsets = match data.inst_data(initial_iv).kind() {
                InstKind::Integer(initial) => {
                    let mut offsets = Vec::with_capacity(candidate.offsets.len());
                    for (offset, evolution) in candidate
                        .offsets
                        .iter()
                        .zip(&candidate.address_evolution.indices)
                    {
                        offsets.push(match evolution {
                            IndexEvolution::Invariant => match data.inst_data(*offset).kind() {
                                InstKind::Integer(integer) => Some(integer.value()),
                                _ => None,
                            },
                            IndexEvolution::Direct => Some(initial.value()),
                            IndexEvolution::Affine(affine) => Self::evaluate_affine_initial(
                                data,
                                affine,
                                candidate.iv,
                                initial.value(),
                                &candidate.forwarded_params,
                            ),
                        });
                    }
                    Some(offsets)
                }
                _ => None,
            };
            if let Some(offsets) = constant_offsets
                .as_ref()
                .and_then(|offsets| offsets.iter().copied().collect::<Option<Vec<_>>>())
            {
                if !crate::opt::utils::gep::gep_constant_offsets_fit(data, candidate.base, &offsets)
                {
                    return ApplyResult::Unchanged;
                }
            }
            initial_indices_by_edge.push((edge, constant_offsets, initial_iv));
        }

        let mut rewrites = LogicalEdgeRewriter::new();
        let header_param_positions = data
            .bb_data(looop.header())
            .params()
            .iter()
            .enumerate()
            .map(|(position, &parameter)| (parameter, position))
            .collect::<FxHashMap<_, _>>();
        for (edge, constant_offsets, initial_iv) in initial_indices_by_edge {
            // Non-IV offsets may be loop-invariant header parameters; their
            // preheader edge argument substitutes for them in the initial
            // pointer (the parameter value is unchanged on every backedge).
            let edge_args = edge.args(data).to_vec();
            let mut initial_offsets = candidate.offsets.clone();
            for (position, evolution) in candidate.address_evolution.indices.iter().enumerate() {
                initial_offsets[position] = match evolution {
                    IndexEvolution::Invariant => {
                        match header_param_positions.get(&candidate.offsets[position]) {
                            Some(&parameter_position) => edge_args[parameter_position],
                            None => initial_offsets[position],
                        }
                    }
                    IndexEvolution::Direct => initial_iv,
                    IndexEvolution::Affine(affine) => match constant_offsets
                        .as_ref()
                        .and_then(|offsets| offsets[position])
                    {
                        Some(offset) => data.new_local_value().integer(offset),
                        None => Self::clone_affine_initial(
                            data,
                            preheader,
                            affine,
                            candidate.iv,
                            initial_iv,
                            &header_param_positions,
                            &edge_args,
                            &candidate.forwarded_params,
                        ),
                    },
                };
            }
            let initial_pointer = data
                .new_local_value()
                .get_elem_ptr(candidate.base, initial_offsets);
            debug_assert_eq!(data.inst_data(initial_pointer).ty(), &candidate.pointer_ty);
            data.layout_mut()
                .insert_before_terminator(preheader, initial_pointer);
            rewrites.append_arg(data, edge, initial_pointer);
        }

        let pointer = data
            .new_basic_block()
            .add_param(looop.header(), candidate.pointer_ty.clone());
        let pointer_step = data
            .new_local_value()
            .integer(candidate.address_evolution.pointer_step);
        for group in candidate.backedge_groups {
            let next_pointer = data
                .new_local_value()
                .get_elem_ptr(pointer, vec![pointer_step]);
            debug_assert_eq!(data.inst_data(next_pointer).ty(), &candidate.pointer_ty);
            data.layout_mut()
                .insert_before_terminator(group.source, next_pointer);
            for edge in group.edges {
                rewrites.append_arg(data, edge, next_pointer);
            }
        }
        assert!(rewrites.apply(data));

        let zero = data.new_local_value().integer(0);
        data.replace_inst_with(candidate.gep)
            .get_elem_ptr(pointer, vec![zero]);
        debug_assert_eq!(data.inst_data(candidate.gep).ty(), &candidate.pointer_ty);
        ApplyResult::Changed
    }
}
