//! Candidate discovery and reachability checks for pointer strength reduction.
//!
//! ---
//!
//! # Candidate 子模块：候选发现与可达性检查（PSR 阶段一）
//!
//! 在**最内层循环**里寻找"每轮都要重新计算、适合改写成指针递进"的 GEP
//! 地址，并完成可达性 / 安全性 / 收益性检查；只有这里产出的 `Candidate`
//! 才会被 `rewrite` 子模块实际改写。触发形状：循环 header 存在基本归纳变量
//! （IV）、循环内有以 IV 的仿射函数作索引的 GEP、且该 GEP 只被循环内的
//! load/store 当地址使用（不逃逸）。术语（header / latch / preheader /
//! backedge / 归纳变量 IV / 仿射 / GEP / block 参数）：见
//! `docs/offline-handbook/glossary.md`，不在此展开；父 pass 与三个子模块的
//! 整体分工见 `pointer_strength_reduction.rs` 的模块文档。
//!
//! ## 主流程：find_candidate 的检查步骤（按代码顺序）
//!
//! 1. **backedge 收集与分组**：`incoming_edges` 取 header 的全部入边，过滤出
//!    源块在循环内的边即 backedge；为空直接返回 `None`（不成循环）。按源块
//!    合并成 `BackedgeGroup`（同源多条边共享一个指针更新）。
//! 2. **循环不变量 header 参数**：`forwarded_block_params` +
//!    `resolve_forwarded_params` 先建 block 参数转发表；凡是所有 backedge 都
//!    原样传递的 header 参数（`invariant_header_params`）在循环内恒等于首轮
//!    值，计算初始指针时可用 preheader 边的参数作替身。
//! 3. **IV 步长必须为常量**：`iv.step()` 须为 `InductionStep::Add`/`Sub` 且
//!    `integer_constant` 能读出数值（`Sub` 取负）。循环里任一 IV 步长非常量
//!    会经 `?` 短路，**整个循环的候选搜索直接放弃**（保守处理）。
//! 4. **无回绕证明（iv_range）**：非单位步长必须同时满足
//!    `normalize_strict_exit`（严格 exit、常量边界）与
//!    `constant_induction_range`（常量归纳范围），任一失败即放弃——指针直接
//!    跟随 IV，步长可能跳过边界；单位步长只要求严格 exit，缺失时 header
//!    终结符须为 `Jump`（rotated countdown 形态：测试被移到 latch），否则
//!    跳过该 IV；header 分支是 `Le` 之类非严格比较（`normalize_strict_exit`
//!    拒绝）时 IV 可能绕过边界，同样拒绝。
//! 5. **IV 在 header 参数中的位置**：找不到 IV 参数位置则跳过；所有 backedge
//!    在该位置必须传 `iv.update_values()` 之一，否则跳过。
//! 6. **GEP 扫描与可达性**：只扫 `loops.min_loop_contain` 恰好是本循环的块
//!    （嵌套循环留给外层迭代）。候选 GEP 须同时满足：
//!    - base 在 header 处可得（`available_at_header`：全局 / 常量 /
//!      不变量 header 参数 / 循环外支配 header 的块中定义；`BlockArgRef`
//!      按 `parameter_blocks` 查其定义块，同样判定）；
//!    - 只被循环内内存操作使用（`has_only_loop_memory_users`：
//!      `Load`/`Store`/`MemZero` 以它为地址，或作为内层 GEP 的 base 递归
//!      满足；深度上限 `MAX_TRANSITIVE_GEP_DEPTH` = 8，visited 集防环）；
//!    - GEP 所在块支配所有 backedge 源（保证每条 backedge 携带同一指针值）。
//! 7. **索引演化分类与扁平化**：逐维 `classify_index_evolution` 得
//!    `IndexEvolution::Invariant`（须 `available_at_header`）/
//!    `Direct`（系数 1）/ `Affine`（系数 + 派生链 + 不变量，不变量逐个复核
//!    header 可得）；`gep_index_stride` 查该维元素步长，贡献 = 系数 × 步长，
//!    checked 累加进 `flat_coefficient`；任一失败即放弃该 GEP。
//! 8. **步长、溢出与成本闸门**：
//!    - `flat_coefficient != 0`（全抵消则没有可递进的东西）；
//!    - `signed_step × flat_coefficient` 必须装进 i32（`signed_pointer_step`）；
//!    - GEP 类型须为指针，`index_delta × element_size` 得每轮字节步长
//!      `signed_byte_delta`；
//!    - 运行期边界循环（`iv_range` 未知）跳过 i32 中间范围证明（增量方案只做
//!      64 位指针加法，被保护的 32 位计算已不存在），但每轮字节步长
//!      |Δ| > 2^20 拒绝（防地址跨度爆炸与立即数编码）；
//!    - 派生链中只有 `only_reaches_candidate` 的指令才计入可删收益（用作分支
//!      条件、非 `Jump`/`Branch` 角色、转发参数不一致都拒绝删除）；
//!    - `estimate_aarch64_pointer_strength_reduction` 估 AArch64 成本，
//!      `!cost.is_profitable()` 放弃。
//! 9. **选优**：同一循环多个候选按 `cost.is_better_than` 只留一个最佳
//!    （`run_on` 每轮 fixpoint 只改写一个候选，然后重建分析重跑）。
//!
//! ## 触发 / 放弃条件（小结）
//!
//! - 触发：backedge 存在、IV 步长为常量、GEP 索引对本循环 IV 呈仿射 / 直接 /
//!   不变量、GEP 不逃逸、改写有正收益；
//! - 放弃：无 backedge、IV 步长非常量、非严格或无常量界的 exit（单位步长 +
//!   jump-through header 除外）、backedge 更新值不一致、base / 不变量索引
//!   header 不可得、GEP 有循环外或非内存使用、GEP 块不支配 backedge 源、
//!   步长或字节增量溢出、运行期边界下字节步长过大、成本不盈利。
//!
//! ## 正确性要点
//!
//! - **逐轮相等**：改写后指针序列 = 原 GEP 地址序列（数学归纳：初值在
//!   preheader 用 IV 初值算出，每轮步长 = IV 步长 × 各维系数 × 元素大小，
//!   与每轮重算的地址一致，`rewrite` 子模块据此实施）；
//! - **header 可得性前置**：改写只在 preheader 计算初值、向 header 新增参数，
//!   因此 base 与一切不变量索引都必须在 header 处可得
//!   （`available_at_header`），不允许引用循环内才定义的值；
//! - **不逃逸**：GEP 只被循环内 `Load`/`Store`/`MemZero` 使用，地址不会进
//!   call / 返回 / 数据存储，循环外无人引用中间指针；
//! - **backedge 一致性**：IV 位置必须传更新值、GEP 块支配所有 backedge 源，
//!   保证无论从哪条边进入下一轮，指针步进一致；
//! - **无回绕**：非单位步长要求严格 exit 的常量界（`normalize_strict_exit`
//!   拒绝 `Le` 等非严格比较）；仿射 i32 中间范围检查
//!   （`affine_range_fits_i32`，`analysis` 子模块）保证折叠不溢出；运行期边界
//!   时改以每轮字节步长上界防护；
//! - **保守删除**：`only_reaches_candidate` 只放行"所有使用最终都汇到候选
//!   GEP"的派生指令，分支条件等旁路使用一律不删。
//!
//! ## 管线位置
//!
//! - 父 pass `PointerStrengthReduction` 注册于 `opt/pass.rs` 的 `from_config`，
//!   fixpoint 段，`dse` 之后、`guard_elimination` 之前（内存画像稳定后改写
//!   地址）；无目标门控、无 config 开关；
//! - `run_on` 每轮 fixpoint：`find_candidate` 选候选 → `apply_candidate`
//!   改写 → 重跑分析直到无变换；本文件只读 IR、不改写。
//!
//! ## 验证
//!
//! - 单测：父模块 `mod tests` 委托 `tests.rs`，正向覆盖单位 / 非单位步长、
//!   常量与运行期边界、仿射 / 负数系数 / 移位派生索引、多 latch、转发块、
//!   嵌套 GEP 链等；拒绝覆盖非严格边界、i32 中间回绕、非内存使用链、深链
//!   （超 `MAX_TRANSITIVE_GEP_DEPTH`）、超过两个 backedge 源等；
//! - 端到端：`make test` 差分比对。
//!
//! ## 与兄弟子模块的协作
//!
//! - `analysis`（索引演化分析）：`classify_index_evolution` 把每个 GEP 偏移
//!   分类为不变量 / 直接 IV / 仿射（系数、派生链、不变量与范围），并完成
//!   `affine_range_fits_i32` 的 i32 中间范围证明；本文件消费其结果，再独立
//!   复核各项的 header 可达性；
//! - `rewrite`（改写实施）：本文件产出的 `Candidate`（GEP、IV 参数位置、
//!   `backedge_groups`、`FlattenedAddressEvolution` 扁平化地址演化等）交给
//!   `apply_candidate`：preheader 克隆初值、header 新增携带指针的参数、
//!   backedge 传"指针 + 步长"。

use super::*;

impl PointerStrengthReduction {
    pub(super) fn find_candidate(
        data: &ArenaContextMut<'_>,
        cfg: &CFG,
        dom_tree: &DominanceTree,
        loops: &LoopAnalysis,
        ivs: &BasicInductionVariableAnalysis,
        ranges: &RangeAnalysis,
        looop: &Loop,
        parameter_blocks: &FxHashMap<Inst, BasicBlock>,
    ) -> Option<Candidate> {
        let backedges = incoming_edges(data, cfg, looop.header())
            .into_iter()
            .filter(|edge| looop.contains(edge.source()))
            .collect::<Vec<_>>();
        if backedges.is_empty() {
            return None;
        }
        let mut backedge_groups = SmallVec::<[BackedgeGroup; 2]>::new();
        for &edge in &backedges {
            if let Some(group) = backedge_groups
                .iter_mut()
                .find(|group| group.source == edge.source())
            {
                group.edges.push(edge);
            } else {
                backedge_groups.push(BackedgeGroup {
                    source: edge.source(),
                    edges: SmallVec::from_slice(&[edge]),
                });
            }
        }

        let mut best = None;
        let header_params = data.bb_data(looop.header()).params().to_vec();
        let forwarded_params = resolve_forwarded_params(&forwarded_block_params(data, cfg));
        // A header block parameter that every backedge passes through unchanged
        // is loop-invariant: its value on the first entry equals its value in
        // every iteration, so the preheader edge argument can substitute for it
        // when the initial pointer is computed.
        let invariant_header_params = header_params
            .iter()
            .enumerate()
            .filter_map(|(position, &parameter)| {
                backedges
                    .iter()
                    .all(|edge| edge.args(data).get(position) == Some(&parameter))
                    .then_some(parameter)
            })
            .collect::<FxHashSet<_>>();
        for iv in ivs.for_loop(looop) {
            // The pointer step only needs the IV's constant update value. The
            // strict-exit normalization additionally provides a constant
            // induction range used to prove that affine (non-direct) index
            // expressions cannot wrap `i32`. Rotated countdown loops whose
            // header no longer compares the IV (the test moved to the latch
            // comparing the countdown counter) still qualify for direct and
            // invariant indices.
            let signed_step = match iv.step() {
                InductionStep::Add(step) => Self::integer_constant(data, step)?,
                InductionStep::Sub(step) => Self::integer_constant(data, step)?.checked_neg()?,
            };
            // Non-unit steps must keep the no-wrap proof from a normalized
            // strict exit with a constant bound: the pointer follows `iv`
            // directly and could otherwise skip past the bound. Unit steps
            // only need a strict exit; the constant range is optional there
            // because a direct or invariant index is carried exactly by the
            // pointer (the affine wrap check still requires it). A loop whose
            // header branch `normalize_strict_exit` rejected (non-strict
            // compare such as `Le`) is refused: the IV can wrap past the
            // bound. Rotated countdown loops move the test to the latch and
            // leave the header as a jump-through block, which is the shape
            // h-5's inner loop has.
            let iv_range = if signed_step.unsigned_abs() != 1 {
                Some(constant_induction_range(
                    data,
                    iv,
                    normalize_strict_exit(data, looop, iv)?,
                )?)
            } else {
                match normalize_strict_exit(data, looop, iv) {
                    Some(exit) => constant_induction_range(data, iv, exit),
                    None => {
                        let header_terminator =
                            data.layout().basicblock(looop.header()).terminator();
                        if matches!(data.inst_data(header_terminator).kind(), InstKind::Jump(..)) {
                            None
                        } else {
                            continue;
                        }
                    }
                }
            };
            let Some(header_iv_position) = data
                .bb_data(looop.header())
                .params()
                .iter()
                .position(|&parameter| parameter == iv.parameter())
            else {
                continue;
            };
            if backedges.iter().any(|edge| {
                edge.args(data)
                    .get(header_iv_position)
                    .is_none_or(|value| !iv.update_values().contains(value))
            }) {
                continue;
            }

            for block_layout in data.layout().basicblocks() {
                let block = block_layout.bb();
                if !looop.contains(block)
                    || loops.min_loop_contain(block).map(Loop::header) != Some(looop.header())
                {
                    continue;
                }
                for &inst in block_layout.insts() {
                    let InstKind::GetElemPtr(gep) = data.inst_data(inst).kind() else {
                        continue;
                    };
                    if !Self::available_at_header(
                        data,
                        dom_tree,
                        looop,
                        parameter_blocks,
                        &invariant_header_params,
                        gep.base(),
                    ) || !Self::has_only_loop_memory_users(data, looop, inst)
                        || !backedge_groups
                            .iter()
                            .all(|group| dom_tree.dominates(block, group.source))
                    {
                        continue;
                    }
                    let mut index_evolutions = Vec::with_capacity(gep.offsets().len());
                    let mut flat_coefficient = 0_i64;
                    let mut removable_chain = FxHashSet::default();
                    let mut valid = true;
                    for (position, &offset) in gep.offsets().iter().enumerate() {
                        let evolution = Self::classify_index_evolution(
                            data,
                            ranges,
                            looop,
                            iv.parameter(),
                            iv_range,
                            &forwarded_params,
                            inst,
                            offset,
                        );
                        let Some(evolution) = evolution else {
                            valid = false;
                            break;
                        };
                        let (coefficient, chain, invariants) = match &evolution {
                            IndexEvolution::Invariant => {
                                if !Self::available_at_header(
                                    data,
                                    dom_tree,
                                    looop,
                                    parameter_blocks,
                                    &invariant_header_params,
                                    offset,
                                ) {
                                    valid = false;
                                    break;
                                }
                                index_evolutions.push(evolution);
                                continue;
                            }
                            IndexEvolution::Direct => (1, None, None),
                            IndexEvolution::Affine(affine) => (
                                affine.coefficient,
                                Some(&affine.chain),
                                Some(&affine.invariants),
                            ),
                        };
                        let Some(stride) = gep_index_stride(data, inst, position) else {
                            valid = false;
                            break;
                        };
                        if !invariants.into_iter().flatten().all(|&invariant| {
                            Self::available_at_header(
                                data,
                                dom_tree,
                                looop,
                                parameter_blocks,
                                &invariant_header_params,
                                invariant,
                            )
                        }) {
                            valid = false;
                            break;
                        }
                        let Some(contribution) =
                            coefficient.checked_mul(i64::from(stride.result_element_stride))
                        else {
                            valid = false;
                            break;
                        };
                        let Some(coefficient) = flat_coefficient.checked_add(contribution) else {
                            valid = false;
                            break;
                        };
                        flat_coefficient = coefficient;
                        if let Some(chain) = chain {
                            removable_chain.extend(chain.iter().copied());
                        }
                        index_evolutions.push(evolution);
                    }
                    if !valid || flat_coefficient == 0 {
                        continue;
                    }
                    let Some(index_delta) = i64::from(signed_step).checked_mul(flat_coefficient)
                    else {
                        continue;
                    };
                    let Some(signed_pointer_step) = i32::try_from(index_delta).ok() else {
                        continue;
                    };
                    let result_element_size = match data.inst_data(inst).ty().kind() {
                        crate::ir::TypeKind::Pointer(element) => i64::try_from(element.size()).ok(),
                        _ => None,
                    };
                    let Some(signed_byte_delta) =
                        result_element_size.and_then(|size| index_delta.checked_mul(size))
                    else {
                        continue;
                    };
                    // Runtime-bound loops (unknown iv_range) skip the i32
                    // intermediate-range proof: the incremental scheme only
                    // performs 64-bit pointer adds, so the protected 32-bit
                    // computation no longer exists. Guard the per-iteration
                    // byte step instead so a pathological stride cannot
                    // blow up the address span or the immediate encoding.
                    if iv_range.is_none() && signed_byte_delta.unsigned_abs() > (1 << 20) {
                        continue;
                    }
                    let removable_derived_insts = removable_chain
                        .iter()
                        .filter(|&&derived| {
                            Self::only_reaches_candidate(
                                data,
                                derived,
                                inst,
                                &removable_chain,
                                &forwarded_params,
                                &mut FxHashSet::default(),
                            )
                        })
                        .count();
                    let derived_setup_insts = index_evolutions
                        .iter()
                        .map(|evolution| match evolution {
                            IndexEvolution::Affine(affine) => affine.chain.len(),
                            IndexEvolution::Invariant | IndexEvolution::Direct => 0,
                        })
                        .sum();
                    let Some(cost) = estimate_aarch64_pointer_strength_reduction(
                        data,
                        cfg,
                        looop,
                        gep,
                        signed_byte_delta,
                        removable_derived_insts,
                        derived_setup_insts,
                    ) else {
                        continue;
                    };
                    if !cost.is_profitable() {
                        continue;
                    }
                    let candidate = Candidate {
                        gep: inst,
                        iv: iv.parameter(),
                        header_iv_position,
                        backedge_groups: backedge_groups.clone(),
                        base: gep.base(),
                        offsets: gep.offsets().to_vec(),
                        pointer_ty: data.inst_data(inst).ty().clone(),
                        address_evolution: FlattenedAddressEvolution {
                            indices: index_evolutions,
                            coefficient: flat_coefficient,
                            pointer_step: signed_pointer_step,
                        },
                        forwarded_params: forwarded_params.clone(),
                    };
                    if best
                        .as_ref()
                        .is_none_or(|(best_cost, _)| cost.is_better_than(*best_cost))
                    {
                        best = Some((cost, candidate));
                    }
                }
            }
        }
        best.map(|(_, candidate)| candidate)
    }

    pub(super) fn available_at_header(
        data: &ArenaContextMut<'_>,
        dom_tree: &DominanceTree,
        looop: &Loop,
        parameter_blocks: &FxHashMap<Inst, BasicBlock>,
        invariant_header_params: &FxHashSet<Inst>,
        value: Inst,
    ) -> bool {
        if value.is_global() || data.inst_data(value).kind().is_const() {
            return true;
        }
        if invariant_header_params.contains(&value) {
            return true;
        }
        match data.layout().parent_bb(value) {
            Some(block) => !looop.contains(block) && dom_tree.dominates(block, looop.header()),
            None if matches!(data.inst_data(value).kind(), InstKind::BlockArgRef(..)) => {
                parameter_blocks.get(&value).is_some_and(|&block| {
                    !looop.contains(block)
                        && dom_tree.contains(block)
                        && dom_tree.dominates(block, looop.header())
                })
            }
            None => false,
        }
    }

    /// Upper bound on how many nested `getelemptr` levels (an outer GEP used
    /// as the base of an inner GEP) `has_only_loop_memory_users` will follow
    /// before giving up conservatively. Realistic 2D/3D array access chains
    /// are 2-3 levels deep; anything deeper is rejected to bound compile time.
    pub(super) const MAX_TRANSITIVE_GEP_DEPTH: usize = 8;

    /// Returns whether every use of `gep` inside `looop` is a memory
    /// operation (`Load`/`Store`/`MemZero`) that uses `gep` as its address,
    /// possibly through nested `getelemptr` levels: a GEP that uses `gep` as
    /// its base is accepted iff its own users satisfy the same property. Any
    /// use in a non-address role (stored as data, passed to a call, returned,
    /// ...) anywhere along the chain rejects. This admits the 2D array shape
    /// `b[k][i]` where the outer GEP carries the induction-variable index and
    /// its only user is the inner GEP.
    pub(super) fn has_only_loop_memory_users(
        data: &ArenaContextMut<'_>,
        looop: &Loop,
        gep: Inst,
    ) -> bool {
        fn chain_has_only_loop_memory_users(
            data: &ArenaContextMut<'_>,
            looop: &Loop,
            gep: Inst,
            visited: &mut FxHashSet<Inst>,
            depth: usize,
        ) -> bool {
            // Cycle protection: each GEP is visited at most once. In a
            // well-formed SSA use graph a GEP has a single base, so a revisit
            // can only happen through a base-edge cycle that never reaches a
            // memory operation; reject it conservatively (this also bounds
            // the walk, alongside the depth cap).
            if depth >= PointerStrengthReduction::MAX_TRANSITIVE_GEP_DEPTH || !visited.insert(gep) {
                return false;
            }
            let users = data.inst_data(gep).used_by();
            !users.is_empty()
                && users.iter().all(|&user| {
                    data.layout()
                        .parent_bb(user)
                        .is_some_and(|block| looop.contains(block))
                        && match data.inst_data(user).kind() {
                            InstKind::Load(load) => load.src() == gep,
                            InstKind::Store(store) => store.dest() == gep,
                            InstKind::MemZero(mem_zero) => mem_zero.dest() == gep,
                            // Follow the chain only through the pointer
                            // (base) operand; a GEP used anywhere else is not
                            // an address-only use.
                            InstKind::GetElemPtr(inner) => {
                                inner.base() == gep
                                    && chain_has_only_loop_memory_users(
                                        data,
                                        looop,
                                        user,
                                        visited,
                                        depth + 1,
                                    )
                            }
                            _ => false,
                        }
                })
        }
        chain_has_only_loop_memory_users(data, looop, gep, &mut FxHashSet::default(), 0)
    }

    pub(super) fn only_reaches_candidate(
        data: &ArenaContextMut<'_>,
        value: Inst,
        gep: Inst,
        chain: &FxHashSet<Inst>,
        forwarded_params: &FxHashMap<Inst, Inst>,
        visiting: &mut FxHashSet<Inst>,
    ) -> bool {
        if !visiting.insert(value) {
            return false;
        }
        let result = data.inst_data(value).used_by().iter().all(|&user| {
            if user == gep {
                return true;
            }
            if chain.contains(&user) {
                return Self::only_reaches_candidate(
                    data,
                    user,
                    gep,
                    chain,
                    forwarded_params,
                    visiting,
                );
            }

            let Some(block) = data.layout().parent_bb(user) else {
                return false;
            };
            if matches!(data.inst_data(user).kind(), InstKind::Branch(branch) if branch.cond() == value)
            {
                return false;
            }
            if !matches!(data.inst_data(user).kind(), InstKind::Jump(..) | InstKind::Branch(..)) {
                return false;
            }

            let mut saw_forward = false;
            for edge in outgoing_edges(data, block) {
                for (position, &argument) in edge.args(data).iter().enumerate() {
                    if argument != value {
                        continue;
                    }
                    let Some(&parameter) = data.bb_data(edge.target(data)).params().get(position)
                    else {
                        return false;
                    };
                    let parameter_end = forwarded_params
                        .get(&parameter)
                        .copied()
                        .unwrap_or(parameter);
                    let value_end = forwarded_params.get(&value).copied().unwrap_or(value);
                    if parameter_end != value_end
                        || !Self::only_reaches_candidate(
                            data,
                            parameter,
                            gep,
                            chain,
                            forwarded_params,
                            visiting,
                        )
                    {
                        return false;
                    }
                    saw_forward = true;
                }
            }
            saw_forward
        });
        visiting.remove(&value);
        result
    }
}
