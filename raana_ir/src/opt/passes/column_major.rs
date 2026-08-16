//! # ColumnMajor：二维数组列主序布局转换（矩阵转置存储）
//!
//! 把**二维标量数组**（类型 `[rows][columns]` 的元素）从行主序改成列主序
//! 存储（`[columns][rows]`），使其**最热循环**内的访问变成单位步长
//! （unit stride）连续访存。典型受益：行主序数组上按列遍历的循环
//! （如 `A[k][j]` 列访问），转置后连续元素落在同一缓存行。
//!
//! ## 变换形态（IR 示例）
//!
//! ```text
//! 前：root = alloc [N][M] i32           // 行主序：a[i][j] 地址 = base + i*M + j
//!     ...p = gep root, [0, i, j]        // 行主序下标
//!     ...load p
//! 后：root = alloc [M][N] i32           // 列主序：a[i][j] 地址 = base + j*N + i
//!     ...p = gep root, [0, j, i]        // 下标交换（offset[1]↔offset[2]）
//!     ...load p
//! ```
//!
//! 所有被识别的访问 GEP 的第 1、2 个偏移（行、列）对调；全局变量的初始化器
//! 同步转置（`transpose_initializer`：`ZeroInit` 保持，`Aggregate` 按
//! `flatten` 后重排）；debug 构建下 `verify_rewrite` 断言根类型、GEP 偏移与
//! 使用者集合不变。
//!
//! ## 候选与收益判定
//!
//! **候选**（`collect_candidates` + `collect_uses`）：
//! - 根是 `Alloc`（局部）或 `GlobalAlloc`（全局），类型为二维标量数组
//!   `[rows][columns]`（元素必须标量，维度乘积不溢出）；
//! - 数组的**所有**使用者必须被识别：三维偏移 GEP（`[0, row, column]`）
//!   且 GEP 只被 `load`/`store` 使用，或 `MemZero`（全局的初始化器还必须是
//!   `ZeroInit` 或元素数匹配的 `Aggregate`）；任何其它用法（地址逃逸、传参
//!   等）→ 放弃。
//!
//! **收益**（`candidate_is_profitable`，按函数逐个访问评估）：
//! - 每个访问的行/列下标对最内层循环的 IV 求系数（`classify_access_delta`
//!   → `(row_delta, column_delta)`），算出当前步长与转置后步长（字节）；
//! - 访问所在循环巢的执行次数估计（`loop_nest_executions`）× 该 GEP 的
//!   内存使用者数 = 权重；
//! - 成本模型（`opt/utils/column_major_cost.rs`）：步长惩罚 = 覆盖的缓存行
//!   数（≥64B 按行计、≥4KB 额外页惩罚），加权求和；
//! - **转置收益条件**（`is_profitable`）：转置后产生单位步长
//!   （`current_stride > element_size && transposed == element_size`），且
//!   总代价下降的绝对节省 ≥ 8、相对节省 ≥ 10%。
//!
//! ## 正确性
//!
//! - 转置是纯**布局**变换：元素集合与初始化内容不变，下标 `[i][j]` 映射到
//!   新布局 `[j][i]`，所有访问点同步交换偏移，读写关系不变；
//! - 必须确认**所有**使用点都被重写（`used_by` 全量核对），漏掉任何一个
//!   访问都会造成语义错乱——这是 `collect_uses` 严格的原因。
//!
//! ## 管线位置
//!
//! - 注册：`opt/pass.rs` 的 `from_config`，initial 段，`tco` 之后、`gsp`
//!   之前（GEP 形状重塑之前调整布局）；
//! - **AArch64 门控**（`config.target.enable_chain_to_switch`）——成本模型
//!   按 Cortex-A53 缓存行/页参数标定，RISC-V 不注册；
//! - 无独立 config 开关。
//!
//! ## 验证
//!
//! - 本文件 `mod tests`（632 行起）覆盖候选识别/收益判定/重写与初始化器
//!   转置；
//! - 端到端：`make test` 差分比对（AArch64）。
//!
//! ## 已知边界
//!
//! - 只处理**二维**数组（`matrix_type` 恰好两层数组）；三维及以上不识别；
//! - 非单位步长但仍有收益（如两列步长交错）的访问不处理——收益模型要求
//!   转置后必须产生单位步长。

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    ir::{
        TypeKind,
        inst_kind::{Aggregate, GlobalAlloc},
    },
    opt::{
        analysis_passes::{
            induction_variable::{
                BasicInductionVariable, BasicInductionVariableAnalysis, ConstantInductionRange,
                InductionDirection, TripCountEstimate, constant_induction_range,
                induction_trip_count, normalize_strict_exit,
            },
            loop_analysis::LoopAnalysis,
        },
        prelude::*,
        utils::{
            cfg::CFG,
            column_major_cost::{AccessCost, is_profitable},
        },
    },
};

// 候选根的来源：局部 Alloc（属于某个函数）或全局 GlobalAlloc（附带初始化器）。
#[derive(Debug, Clone, Copy)]
enum RootKind {
    Local(Function),
    Global { init: Inst },
}

// 一个待转置的二维数组候选：root 是分配指令，rows/columns 是逻辑行列数，
// accesses 收集所有通过验证的访问点，供后续逐函数收益评估。
#[derive(Debug, Clone)]
struct Candidate {
    root: Inst,
    kind: RootKind,
    rows: usize,
    columns: usize,
    element_ty: Type,
    accesses: Vec<Access>,
}

// 单个访问点：三维 GEP（base, 0, row, column）。row/column 是下标 SSA 值，
// memory_user_count 是该 GEP 的 load/store 使用者数，用作收益加权。
#[derive(Debug, Clone, Copy)]
struct Access {
    function: Function,
    gep: Inst,
    row: Inst,
    column: Inst,
    memory_user_count: usize,
}

pub struct ColumnMajor;

impl Pass for ColumnMajor {
    fn run(&mut self, program: &mut Program) -> bool {
        // 三段式流水：收集候选 → 按成本模型过滤出值得转置的 → 逐个重写。
        // 返回 true 表示程序被修改（调用方据此标记 pass 生效）。
        let candidates = collect_candidates(program)
            .into_iter()
            .filter(|candidate| candidate_is_profitable(program, candidate))
            .collect::<Vec<_>>();

        for candidate in &candidates {
            rewrite_candidate(program, candidate);
        }
        !candidates.is_empty()
    }
}

// 形状识别：期望 *[rows][columns] 的两层数组指针，元素必须为标量。
// 返回 (元素类型, rows, columns)；checked_mul 防维度乘积与总字节数溢出。
fn matrix_type(ty: &Type) -> Option<(Type, usize, usize)> {
    let TypeKind::Pointer(outer) = ty.kind() else {
        return None;
    };
    let TypeKind::Array(inner, rows) = outer.kind() else {
        return None;
    };
    let TypeKind::Array(element, columns) = inner.kind() else {
        return None;
    };
    // 元素须为标量：转置只交换两层下标，元素若仍是数组则"二维"前提不成立。
    if !element.is_scalar() || matches!(element.kind(), TypeKind::Array(..)) {
        return None;
    }
    rows.checked_mul(*columns)?;
    rows.checked_mul(*columns)?.checked_mul(element.size())?;
    Some((element.clone(), *rows, *columns))
}

// 全程序扫描两类分配点：函数内 Alloc 与全局 GlobalAlloc，
// 形状匹配 matrix_type 的进入候选池，最后统一用 collect_uses 验证使用者。
fn collect_candidates(program: &Program) -> Vec<Candidate> {
    let mut roots = Vec::new();
    for &function in program.function_layout() {
        let data = program.func_data(function);
        for (&inst, inst_data) in data.local_arena().inst_arena().datas() {
            // 第一遍：函数内的局部 Alloc（栈上数组）。
            if matches!(inst_data.kind(), InstKind::Alloc) {
                if let Some((element_ty, rows, columns)) = matrix_type(inst_data.ty()) {
                    roots.push(Candidate {
                        root: inst,
                        kind: RootKind::Local(function),
                        rows,
                        columns,
                        element_ty,
                        accesses: Vec::new(),
                    });
                }
            }
        }
    }
    for &root in program.global_inst_layout() {
        // 第二遍：全局 GlobalAlloc（静态数组），需记录初始化器供后续转置。
        let InstKind::GlobalAlloc(global) = program.inst_data(root).kind() else {
            continue;
        };
        if let Some((element_ty, rows, columns)) = matrix_type(program.inst_data(root).ty()) {
            roots.push(Candidate {
                root,
                kind: RootKind::Global {
                    init: global.init(),
                },
                rows,
                columns,
                element_ty,
                accesses: Vec::new(),
            });
        }
    }

    roots
        .into_iter()
        .filter_map(|mut candidate| collect_uses(program, &mut candidate).then_some(candidate))
        .collect()
}

// 验证候选的全部使用者都是可重写的访问，并把它们收集进 candidate.accesses。
// 返回 false 表示存在无法处理的用法——整个候选放弃（宁可保守也不漏改）。
fn collect_uses(program: &Program, candidate: &mut Candidate) -> bool {
    if candidate.root.is_global()
        && program
            .inst_data(candidate.root)
            .used_by()
            .iter()
            .any(|user| user.is_global())
    {
        // 全局数组被其它全局指令（如另一个全局的初始化器）引用 → 放弃：
        // 我们只能重写本数组的初始化器，无法同步别的全局里对它的引用。
        return false;
    }
    let mut saw_access = false;
    let mut recognized_users = FxHashSet::default();
    for &function in program.function_layout() {
        if matches!(candidate.kind, RootKind::Local(owner) if owner != function) {
            continue;
        }
        let data = program.func_data(function);
        for (&inst, inst_data) in data.local_arena().inst_arena().datas() {
            let directly_uses_root = inst_data
                .inst_usage()
                .any(|operand| operand == candidate.root);
            if !directly_uses_root {
                continue;
            }
            match inst_data.kind() {
                InstKind::GetElemPtr(gep)
                    if gep.base() == candidate.root && gep.offsets().len() == 3 =>
                {
                    // 唯一允许的访问形态：三维偏移 GEP（[0, row, column]），
                    // 且它只被 load/store 使用。转置时只需交换 offsets[1]↔offsets[2]。
                    let offsets = gep.offsets();
                    let Some(memory_user_count) = collect_gep_memory_users(data, inst) else {
                        return false;
                    };
                    candidate.accesses.push(Access {
                        function,
                        gep: inst,
                        row: offsets[1],
                        column: offsets[2],
                        memory_user_count,
                    });
                    recognized_users.insert(inst);
                    saw_access = true;
                }
                InstKind::MemZero(mem_zero) if mem_zero.dest() == candidate.root => {
                    // MemZero 整块清零与布局无关，允许但不计入 Access（无步长收益可评）。
                    recognized_users.insert(inst);
                }
                // 其它任何用法（地址拷贝、作为函数实参等）都无法安全重写 → 放弃。
                _ => return false,
            }
        }
    }
    let all_local_uses_recognized = match candidate.kind {
        RootKind::Local(function) => {
            // 局部候选要求"识别集 == 全部使用者"：used_by() 是反向使用集合，
            // 漏掉任何一个访问点都会在转置后留下错误寻址——这是严格的原因。
            recognized_users
                == *program
                    .func_data(function)
                    .inst_data(candidate.root)
                    .used_by()
        }
        // Local Inst IDs are function-scoped, so a global's reverse-use set
        // cannot distinguish equal IDs from different functions. The scan
        // above checks every function's direct users individually instead.
        RootKind::Global { .. } => true,
    };
    saw_access && all_local_uses_recognized && valid_initializer(program, candidate)
}

// GEP 的使用者必须全部是 Load(gep) / Store(gep)（无其它用途），
// 返回使用者个数：一个 GEP 被多个 load/store 共享时访问更热，用于收益加权。
fn collect_gep_memory_users(data: &FunctionData, gep: Inst) -> Option<usize> {
    let users = data.inst_data(gep).used_by();
    (!users.is_empty()
        && users.iter().all(|&user| match data.inst_data(user).kind() {
            InstKind::Load(load) => load.src() == gep,
            InstKind::Store(store) => store.dest() == gep,
            _ => false,
        }))
    .then_some(users.len())
}

// 全局初始化器必须可转置：ZeroInit 天然对称；Aggregate 要求展平后的
// 元素数恰好等于 rows*columns，否则重排无法对齐新旧布局。
fn valid_initializer(program: &Program, candidate: &Candidate) -> bool {
    let RootKind::Global { init } = candidate.kind else {
        return true;
    };
    match program.inst_data(init).kind() {
        InstKind::ZeroInit => true,
        InstKind::Aggregate(aggregate) => candidate
            .rows
            .checked_mul(candidate.columns)
            .is_some_and(|elements| aggregate.flatten(program).len() == elements),
        _ => false,
    }
}

// 收益判定主流程：访问按函数分组，逐个函数构建 CFG/循环/IV 分析，
// 对每个访问估计"当前布局步长 vs 转置后步长"的代价，汇总成 AccessCost
// 列表后交给 column_major_cost::is_profitable 做最终裁决。
fn candidate_is_profitable(program: &mut Program, candidate: &Candidate) -> bool {
    let mut by_function: FxHashMap<Function, Vec<Access>> = FxHashMap::default();
    for &access in &candidate.accesses {
        by_function.entry(access.function).or_default().push(access);
    }

    let mut costs = Vec::new();
    for (function, accesses) in by_function {
        let context = ArenaContextMut {
            program,
            curr_func: Some(function),
        };
        let Some(cfg) = CFG::new(&context) else {
            return false;
        };
        // 三个快照分析：CFG、循环巢（LoopAnalysis）、基本归纳变量（IV）。
        // 均为只读快照——收益评估阶段绝不修改程序。
        let (cfg, dominance, loops) = LoopAnalysis::from_cfg(cfg);
        let induction = BasicInductionVariableAnalysis::new(&context, &cfg, &loops);

        for access in accesses {
            let Some(block) = context.layout().parent_bb(access.gep) else {
                return false;
            };
            if !index_in_bounds(
                &context,
                &loops,
                &induction,
                block,
                access.row,
                candidate.rows,
            ) || !index_in_bounds(
                &context,
                &loops,
                &induction,
                block,
                access.column,
                candidate.columns,
            ) {
                // 安全前提：行/列下标必须可证明在各自维度内。转置不改变
                // 语义的前提是访问范围合法，证明不了就放弃候选。
                return false;
            }
            let Some(looop) = loops.min_loop_contain(block) else {
                continue;
            };
            if !looop
                .latches()
                .iter()
                .all(|&latch| dominance.dominates(block, latch))
            {
                // 访问块须支配循环的所有 latch：保证每次迭代都执行到该 GEP，
                // 步长估计（每迭代增量 × 迭代次数）才成立。
                return false;
            }
            let Some((row_delta, column_delta)) =
                classify_access_delta(&context, looop, &induction, access.row, access.column)
            else {
                return false;
            };
            // (row_delta, column_delta) 是每迭代行/列下标的增量，步长的原材料。
            let element_size = candidate.element_ty.size();
            let Some(current_stride) =
                byte_stride(row_delta, column_delta, candidate.columns, element_size)
            else {
                return false;
            };
            let Some(transposed_stride) =
                byte_stride(column_delta, row_delta, candidate.rows, element_size)
            else {
                return false;
            };
            // 行主序步长 = |row_delta*columns + column_delta| * elem_size；
            // 转置后列主序步长 = |column_delta*rows + row_delta| * elem_size。
            if current_stride == 0 && transposed_stride == 0 {
                // 两个步长都为零：访问地址不随迭代变化（对标量），布局无关，跳过。
                continue;
            }
            let Some(executions) = loop_nest_executions(&context, &loops, &induction, block) else {
                return false;
            };
            let weight = executions.saturating_mul(access.memory_user_count as u128);
            // 权重 = 循环巢总执行次数 × 该 GEP 的 load/store 数：按真实访存
            // 频率加权，冷访问的劣化不会淹没热访问的收益。
            costs.push(AccessCost {
                current_stride,
                transposed_stride,
                weight,
            });
        }
    }

    is_profitable(&costs, candidate.element_ty.size())
}

// 求最内层循环中行/列下标的每迭代增量 (row_delta, column_delta)：
// 遍历循环的每个 IV，若下标随它线性变化（index_coefficient 求系数），
// 增量 = 系数 × IV 步长。恰好一个 IV 驱动下标 → 返回增量；
// 多个 IV 都驱动 → 步长不唯一无法建模，返回 None（放弃）。
fn classify_access_delta(
    data: &ArenaContextMut<'_>,
    looop: &loop_analysis::Loop,
    induction: &BasicInductionVariableAnalysis,
    row: Inst,
    column: Inst,
) -> Option<(i64, i64)> {
    let mut result = None;

    for iv in induction.for_loop(looop) {
        let Some(exit) = normalize_strict_exit(data, looop, iv) else {
            continue;
        };
        let range = constant_induction_range(data, iv, exit);
        let row_coefficient = index_coefficient(data, looop, iv, row, range)?;
        let column_coefficient = index_coefficient(data, looop, iv, column, range)?;
        if row_coefficient != 0 || column_coefficient != 0 {
            if result.is_some() {
                // 第二个 IV 也影响下标：增量不唯一，无法评估，放弃。
                return None;
            }
            let step = i64::from(exit.signed_step());
            result = Some((
                row_coefficient.checked_mul(step)?,
                column_coefficient.checked_mul(step)?,
            ));
        }
    }
    // 没有 IV 驱动下标：若行/列都是循环不变量则增量恒为 (0, 0)
    //（每次迭代访问同一元素）；否则下标来源不明 → None。
    result.or_else(|| {
        (is_loop_invariant(data, looop, row) && is_loop_invariant(data, looop, column))
            .then_some((0, 0))
    })
}

// 求 value 相对循环 IV 的线性系数：IV 自身 = 1，循环不变量 = 0，
// 其余交给 classify_derived_induction_variable 识别派生 IV（如 k*2 之类
// 的线性表达式）并取其系数。
fn index_coefficient(
    data: &ArenaContextMut<'_>,
    looop: &loop_analysis::Loop,
    iv: &BasicInductionVariable,
    value: Inst,
    range: Option<ConstantInductionRange>,
) -> Option<i64> {
    if value == iv.parameter() {
        return Some(1);
    }
    if is_loop_invariant(data, looop, value) {
        return Some(0);
    }
    induction_variable::classify_derived_induction_variable(
        data,
        looop,
        iv.parameter(),
        value,
        range?,
    )
    .map(|derived| derived.coefficient())
}

// 循环不变量判定：全局/常量/函数参数天然不变；BlockArgRef 必须来自循环外
// 的块（循环内块的参数每迭代都会换新值）；普通指令看定义块是否在循环外。
fn is_loop_invariant(data: &ArenaContextMut<'_>, looop: &loop_analysis::Loop, value: Inst) -> bool {
    if value.is_global()
        || integer_constant(data, value).is_some()
        || data.params().contains(&value)
    {
        return true;
    }
    if matches!(data.inst_data(value).kind(), InstKind::BlockArgRef(..)) {
        return data
            .layout()
            .basicblocks()
            .iter()
            .find(|block| data.bb_data(block.bb()).params().contains(&value))
            .is_some_and(|block| !looop.contains(block.bb()));
    }
    data.layout()
        .parent_bb(value)
        .is_some_and(|block| !looop.contains(block))
}

// 估计块所在循环巢的总执行次数：包含它的每个循环的 trip count 相乘
//（saturating_mul 防溢出；UpperBound 估算同样接受）。次数不可知 → None，
// 调用方将放弃该候选（无法加权）。
fn loop_nest_executions(
    data: &ArenaContextMut<'_>,
    loops: &LoopAnalysis,
    induction: &BasicInductionVariableAnalysis,
    block: BasicBlock,
) -> Option<u128> {
    let mut executions = 1_u128;
    for looop in loops.containing_loops(block) {
        let estimate = induction.for_loop(looop).iter().find_map(|iv| {
            let exit = normalize_strict_exit(data, looop, iv)?;
            induction_trip_count(data, iv, exit)
        })?;
        let trips = match estimate {
            TripCountEstimate::Exact(trips) | TripCountEstimate::UpperBound(trips) => trips,
        };
        executions = executions.saturating_mul(u128::from(trips));
    }
    Some(executions)
}

// 布局地址步长公式：|major_delta * minor_len + minor_delta| 个元素 × 元素大小。
// major/minor 对应行/列增量，minor_len 是内层维度长度；i128 中间运算防溢出，
// unsigned_abs 保证步长非负。
fn byte_stride(
    major_delta: i64,
    minor_delta: i64,
    minor_len: usize,
    element_size: usize,
) -> Option<usize> {
    let minor_len = i128::try_from(minor_len).ok()?;
    let elements = i128::from(major_delta)
        .checked_mul(minor_len)?
        .checked_add(i128::from(minor_delta))?
        .unsigned_abs();
    let bytes = elements.checked_mul(element_size as u128)?;
    usize::try_from(bytes).ok()
}

// 证明下标 value 的取值恒在 [0, dimension-1] 内：常量直接比对；
// IV 驱动的下标用迭代上下界（iteration_value_bounds）推出值域再比对。
// 证明不了 → false，调用方放弃候选（越界访问转置后语义会变）。
fn index_in_bounds(
    data: &ArenaContextMut<'_>,
    loops: &LoopAnalysis,
    induction: &BasicInductionVariableAnalysis,
    block: BasicBlock,
    value: Inst,
    dimension: usize,
) -> bool {
    let Some(maximum) = dimension
        .checked_sub(1)
        .and_then(|value| i64::try_from(value).ok())
    else {
        return false;
    };
    if let Some(value) = integer_constant(data, value) {
        // 常量下标：直接区间判断。
        return (0..=maximum).contains(&i64::from(value));
    }
    for looop in loops.containing_loops(block) {
        for iv in induction.for_loop(looop) {
            let Some(exit) = normalize_strict_exit(data, looop, iv) else {
                continue;
            };
            let Some(range) = constant_induction_range(data, iv, exit) else {
                continue;
            };
            let Some((iteration_min, iteration_max)) = iteration_value_bounds(data, iv, exit)
            else {
                continue;
            };
            // IV 驱动的下标：用首末迭代推出值域。若下标就是 IV 参数，
            // 值域即迭代范围；否则把 IV 的线性变换作用到迭代两端求 min/max。
            let bounds = if value == iv.parameter() {
                Some((iteration_min, iteration_max))
            } else {
                induction_variable::classify_derived_induction_variable(
                    data,
                    looop,
                    iv.parameter(),
                    value,
                    range,
                )
                .and_then(|derived| {
                    let at_min = derived
                        .coefficient()
                        .checked_mul(iteration_min)?
                        .checked_add(derived.offset())?;
                    let at_max = derived
                        .coefficient()
                        .checked_mul(iteration_max)?
                        .checked_add(derived.offset())?;
                    Some((at_min.min(at_max), at_min.max(at_max)))
                })
            };
            if let Some((minimum, maximum_value)) = bounds {
                return minimum >= 0 && maximum_value <= maximum;
            }
        }
    }
    false
}

// IV 在整个循环中的取值下界/上界：初始值集合取 min/max；正向循环上界含
// bound-1（退出条件为严格小于），反向循环下界含 bound+1。
fn iteration_value_bounds(
    data: &ArenaContextMut<'_>,
    iv: &BasicInductionVariable,
    exit: induction_variable::NormalizedInductionExit,
) -> Option<(i64, i64)> {
    let bound = i64::from(integer_constant(data, exit.bound())?);
    let initial_values = iv
        .initial_values()
        .iter()
        .map(|&value| integer_constant(data, value).map(i64::from))
        .collect::<Option<Vec<_>>>()?;
    let initial_min = initial_values.iter().copied().min()?;
    let initial_max = initial_values.iter().copied().max()?;
    match exit.direction() {
        InductionDirection::Forward => Some((initial_min, initial_max.max(bound.checked_sub(1)?))),
        InductionDirection::Backward => Some((initial_min.min(bound.checked_add(1)?), initial_max)),
    }
}

// 重写步骤：1) 根分配换成转置类型 [columns][rows]（全局连同初始化器一起
// 转置）；2) 每个访问 GEP 的 offsets[1]↔offsets[2] 对调；3) debug 构建下
// 用 verify_rewrite 断言重写结果。
fn rewrite_candidate(program: &mut Program, candidate: &Candidate) {
    let new_matrix_ty = Type::get_array(
        Type::get_array(candidate.element_ty.clone(), candidate.rows),
        candidate.columns,
    );
    // 新类型 [columns][rows]：外层长度变为 columns（列主序），
    // 下标 [i][j] 在新布局中的地址 = base + j*rows + i。
    let old_name = match candidate.kind {
        RootKind::Local(function) => program
            .func_data(function)
            .inst_data(candidate.root)
            .name()
            .cloned(),
        RootKind::Global { .. } => program.inst_data(candidate.root).name().cloned(),
    };

    match candidate.kind {
        RootKind::Local(function) => {
            // 局部数组：在同一指令槽重建 Alloc，只改类型。
            let mut context = ArenaContextMut {
                program,
                curr_func: Some(function),
            };
            context
                .replace_inst_with(candidate.root)
                .alloc(new_matrix_ty.clone());
            restore_name(&mut context, candidate.root, old_name);
        }
        RootKind::Global { init } => {
            let new_init = transpose_initializer(program, init, candidate, &new_matrix_ty);
            // 全局数组：先转置初始化器，再重建 GlobalAlloc 指向新初始化器。
            // 借用某个访问所在函数构造上下文——全局指令不属于任何函数的 arena。
            let function = candidate.accesses[0].function;
            let mut context = ArenaContextMut {
                program,
                curr_func: Some(function),
            };
            context
                .replace_inst_with(candidate.root)
                .raw(GlobalAlloc::new_data(
                    new_init,
                    Type::get_pointer(new_matrix_ty.clone()),
                ));
            restore_name(&mut context, candidate.root, old_name);
        }
    }

    for &access in &candidate.accesses {
        // 逐访问点重写 GEP：偏移由 [0, row, column] 变为 [0, column, row]，
        // 与新的 [columns][rows] 布局严格对应——这就是转置的语义映射。
        let mut context = ArenaContextMut {
            program,
            curr_func: Some(access.function),
        };
        let name = context.inst_data(access.gep).name().cloned();
        let zero = match context.inst_data(access.gep).kind() {
            InstKind::GetElemPtr(gep) => gep.offsets()[0],
            _ => unreachable!(),
        };
        context
            .replace_inst_with(access.gep)
            .get_elem_ptr(candidate.root, vec![zero, access.column, access.row]);
        restore_name(&mut context, access.gep, name);
    }

    #[cfg(debug_assertions)]
    verify_rewrite(program, candidate, &new_matrix_ty);
}

#[cfg(debug_assertions)]
fn verify_rewrite(program: &Program, candidate: &Candidate, new_matrix_ty: &Type) {
    // 仅 debug 构建生效的完整性检查：根类型、每个 GEP 的 base 与偏移、
    // 以及 GEP 使用者集合（数量与形态）都必须与预期一致——把"漏改/错改"
    // 变成显式崩溃而不是静默的寻址错误。
    let expected_root_ty = Type::get_pointer(new_matrix_ty.clone());
    match candidate.kind {
        RootKind::Local(function) => {
            assert_eq!(
                program.func_data(function).inst_data(candidate.root).ty(),
                &expected_root_ty
            );
        }
        RootKind::Global { .. } => {
            assert_eq!(program.inst_data(candidate.root).ty(), &expected_root_ty);
            let InstKind::GlobalAlloc(global) = program.inst_data(candidate.root).kind() else {
                unreachable!();
            };
            assert_eq!(program.inst_data(global.init()).ty(), new_matrix_ty);
        }
    }

    for access in &candidate.accesses {
        let data = program.func_data(access.function);
        let InstKind::GetElemPtr(gep) = data.inst_data(access.gep).kind() else {
            unreachable!();
        };
        assert_eq!(gep.base(), candidate.root);
        assert_eq!(gep.offsets()[1], access.column);
        assert_eq!(gep.offsets()[2], access.row);
        assert_eq!(
            data.inst_data(access.gep).used_by().len(),
            access.memory_user_count
        );
        assert!(data.inst_data(access.gep).used_by().iter().all(|&user| {
            matches!(data.inst_data(user).kind(), InstKind::Load(load) if load.src() == access.gep)
                || matches!(data.inst_data(user).kind(), InstKind::Store(store) if store.dest() == access.gep)
        }));
    }
}

fn transpose_initializer(
    program: &mut Program,
    init: Inst,
    candidate: &Candidate,
    new_matrix_ty: &Type,
) -> Inst {
    // 初始化器转置：ZeroInit 无需改动；Aggregate 先展平成一维元素序列，
    // 按转置公式重排，再按新内层长度 rows 重新分组回嵌套 Aggregate。
    match program.inst_data(init).kind() {
        // 全零初始化转置后仍是全零：换类型重建即可。
        InstKind::ZeroInit => program.new_value().zero_init(new_matrix_ty.clone()),
        InstKind::Aggregate(aggregate) => {
            let old = aggregate.flatten(program);
            let mut transposed = vec![old[0]; old.len()];
            for row in 0..candidate.rows {
                for column in 0..candidate.columns {
                    transposed[column * candidate.rows + row] =
                        old[row * candidate.columns + column];
                }
            }
            // 核心重排：元素 (row, column) 在新布局的展平下标是
            // column*rows + row（新内层长度是 rows，即转置的索引公式）。
            let mut rows = Vec::with_capacity(candidate.columns);
            for values in transposed.chunks(candidate.rows) {
                rows.push(program.new_value().aggregate(values.to_vec()));
            }
            // 按新内层维度 rows 切成 columns 个子数组，构成 [columns][rows]。
            program
                .new_value()
                .raw(Aggregate::new_data(new_matrix_ty.clone(), rows))
        }
        _ => unreachable!(),
    }
}

fn restore_name(data: &mut ArenaContextMut<'_>, inst: Inst, name: Option<String>) {
    // replace_inst_with 重建指令时会清掉名字，这里把旧名贴回去，
    // 保持 IR dump 中指令可读（调试与差分比对都依赖名字）。
    if let Some(name) = name {
        data.inst_data_mut(inst).set_name(name);
    }
}

fn integer_constant<A: Arena + ?Sized>(data: &A, value: Inst) -> Option<i32> {
    let InstKind::Integer(integer) = data.inst_data(value).kind() else {
        return None;
    };
    Some(integer.value())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct LoopFixture {
        function: Function,
        alloc: Inst,
        gep: Inst,
        row: Inst,
        column: Inst,
        load: Inst,
    }

    fn build_loop(row_varies: bool, bound_value: i32) -> (Program, LoopFixture) {
        let matrix_ty = Type::get_array(Type::get_array(Type::get_i32(), 64), 32);
        let mut program = Program::new();
        let function = program.new_function(Type::get_unit(), "column_major".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }

        let alloc = data.new_local_inst().alloc(matrix_ty);
        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        for inst in [alloc, entry_jump] {
            data.layout_mut().insert_inst(entry, inst);
        }

        let iv = data.bb_data(header).params()[0];
        let bound = data.new_local_inst().integer(bound_value);
        let condition = data.new_local_inst().binary(BinaryOp::Lt, iv, bound);
        let branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        for inst in [condition, branch] {
            data.layout_mut().insert_inst(header, inst);
        }

        let fixed = data.new_local_inst().integer(3);
        let (row, column) = if row_varies { (iv, fixed) } else { (fixed, iv) };
        let gep = data
            .new_local_inst()
            .get_elem_ptr(alloc, vec![zero, row, column]);
        let load = data.new_local_inst().load(gep);
        let one = data.new_local_inst().integer(1);
        let next = data.new_local_inst().binary(BinaryOp::Add, iv, one);
        let backedge = data.new_local_inst().jump(header, vec![next]);
        for inst in [gep, load, next, backedge] {
            data.layout_mut().insert_inst(body, inst);
        }

        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        (
            program,
            LoopFixture {
                function,
                alloc,
                gep,
                row,
                column,
                load,
            },
        )
    }

    fn build_row_varying_loop() -> (Program, LoopFixture) {
        build_loop(true, 32)
    }

    #[test]
    fn transposes_a_hot_row_varying_local_matrix_access() {
        let (mut program, fixture) = build_row_varying_loop();

        assert!(ColumnMajor.run(&mut program));

        let data = program.func_data(fixture.function);
        let expected = Type::get_pointer(Type::get_array(Type::get_array(Type::get_i32(), 32), 64));
        assert_eq!(data.inst_data(fixture.alloc).ty(), &expected);
        let InstKind::GetElemPtr(gep) = data.inst_data(fixture.gep).kind() else {
            panic!("matrix access must remain a GEP");
        };
        assert_eq!(gep.base(), fixture.alloc);
        assert_eq!(gep.offsets()[1], fixture.column);
        assert_eq!(gep.offsets()[2], fixture.row);
    }

    #[test]
    fn rejects_an_out_of_bounds_induction_range() {
        let (mut program, _) = build_loop(true, 33);

        assert!(!ColumnMajor.run(&mut program));
    }

    #[test]
    fn keeps_an_existing_unit_stride_layout() {
        let (mut program, fixture) = build_loop(false, 32);
        let old_ty = program
            .func_data(fixture.function)
            .inst_data(fixture.alloc)
            .ty()
            .clone();

        assert!(!ColumnMajor.run(&mut program));
        assert_eq!(
            program
                .func_data(fixture.function)
                .inst_data(fixture.alloc)
                .ty(),
            &old_ty
        );
    }

    #[test]
    fn preserves_multiple_memory_users_of_one_gep() {
        let (mut program, fixture) = build_row_varying_loop();
        let data = program.func_data_mut(fixture.function);
        let block = data.layout().parent_bb(fixture.load).unwrap();
        let second_load = data.new_local_inst().load(fixture.gep);
        data.layout_mut()
            .insert_inst_before(fixture.load, second_load);

        assert!(ColumnMajor.run(&mut program));
        let data = program.func_data(fixture.function);
        assert_eq!(data.inst_data(fixture.gep).used_by().len(), 2);
        assert!(
            data.inst_data(fixture.gep)
                .used_by()
                .contains(&fixture.load)
        );
        assert!(data.inst_data(fixture.gep).used_by().contains(&second_load));
        assert_eq!(data.layout().parent_bb(second_load), Some(block));
    }

    #[test]
    fn transposes_a_zero_initialized_global_through_the_full_pass() {
        let mut program = Program::new();
        let old_ty = Type::get_array(Type::get_array(Type::get_i32(), 64), 32);
        let init = program.new_value().zero_init(old_ty);
        let global = program.new_value().global_alloc(init);
        let function = program.new_function(Type::get_unit(), "main".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let header = data
            .new_basic_block()
            .basic_block("header".into(), vec![Type::get_i32()]);
        let body = data.new_basic_block().basic_block("body".into(), vec![]);
        let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
        for block in [header, body, exit] {
            data.layout_mut().push_bb_back(block);
        }
        let zero = data.new_local_inst().integer(0);
        let entry_jump = data.new_local_inst().jump(header, vec![zero]);
        data.layout_mut().insert_inst(entry, entry_jump);
        let row = data.bb_data(header).params()[0];
        let bound = data.new_local_inst().integer(32);
        let condition = data.new_local_inst().binary(BinaryOp::Lt, row, bound);
        let branch = data
            .new_local_inst()
            .branch(condition, body, vec![], exit, vec![]);
        for inst in [condition, branch] {
            data.layout_mut().insert_inst(header, inst);
        }
        let column = data.new_local_inst().integer(1);
        let _ = data;
        let mut context = ArenaContextMut {
            program: &mut program,
            curr_func: Some(function),
        };
        let gep = context
            .new_local_value()
            .get_elem_ptr(global, vec![zero, row, column]);
        let load = context.new_local_value().load(gep);
        let data = context.curr_func_data_mut();
        let one = data.new_local_inst().integer(1);
        let next = data.new_local_inst().binary(BinaryOp::Add, row, one);
        let backedge = data.new_local_inst().jump(header, vec![next]);
        for inst in [gep, load, next, backedge] {
            data.layout_mut().insert_inst(body, inst);
        }
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(exit, ret);

        assert!(ColumnMajor.run(&mut program));
        let expected = Type::get_pointer(Type::get_array(Type::get_array(Type::get_i32(), 32), 64));
        assert_eq!(program.inst_data(global).ty(), &expected);
        let InstKind::GlobalAlloc(global_alloc) = program.inst_data(global).kind() else {
            panic!("global must remain allocated");
        };
        assert!(matches!(
            program.inst_data(global_alloc.init()).kind(),
            InstKind::ZeroInit
        ));
    }

    #[test]
    fn transposes_rectangular_global_aggregate_leaves() {
        let mut program = Program::new();
        let leaves = (0..6)
            .map(|value| program.new_value().integer(value))
            .collect::<Vec<_>>();
        let first = program.new_value().aggregate(leaves[0..3].to_vec());
        let second = program.new_value().aggregate(leaves[3..6].to_vec());
        let init = program.new_value().aggregate(vec![first, second]);
        let global = program.new_value().global_alloc(init);
        let function = program.new_function(Type::get_unit(), "main".into(), vec![]);
        let data = program.func_data_mut(function);
        let entry = data.add_entry_block();
        let ret = data.new_local_inst().ret(None);
        data.layout_mut().insert_inst(entry, ret);

        let candidate = Candidate {
            root: global,
            kind: RootKind::Global { init },
            rows: 2,
            columns: 3,
            element_ty: Type::get_i32(),
            accesses: Vec::new(),
        };
        let new_ty = Type::get_array(Type::get_array(Type::get_i32(), 2), 3);
        let new_init = transpose_initializer(&mut program, init, &candidate, &new_ty);
        let InstKind::Aggregate(outer) = program.inst_data(new_init).kind() else {
            panic!("transposed initializer must be an aggregate");
        };
        let flattened = outer.flatten(&program);
        assert_eq!(
            flattened,
            vec![
                leaves[0], leaves[3], leaves[1], leaves[4], leaves[2], leaves[5]
            ]
        );
        assert_eq!(program.inst_data(new_init).ty(), &new_ty);
    }

    #[test]
    fn rejects_a_matrix_pointer_that_escapes_through_a_call() {
        let (mut program, fixture) = build_row_varying_loop();
        let callee = program.new_function(
            Type::get_unit(),
            "consume".into(),
            vec![
                program
                    .func_data(fixture.function)
                    .inst_data(fixture.alloc)
                    .ty()
                    .clone(),
            ],
        );
        let data = program.func_data_mut(fixture.function);
        let entry = data.layout().entry_bb().unwrap().bb();
        let call =
            data.new_local_inst()
                .call_with_type(callee, vec![fixture.alloc], Type::get_unit());
        let terminator = data.layout().basicblock(entry).terminator();
        data.layout_mut().insert_inst_before(terminator, call);

        assert!(!ColumnMajor.run(&mut program));
    }
}
