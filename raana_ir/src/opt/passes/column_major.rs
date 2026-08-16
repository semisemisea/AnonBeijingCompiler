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

#[derive(Debug, Clone, Copy)]
enum RootKind {
    Local(Function),
    Global { init: Inst },
}

#[derive(Debug, Clone)]
struct Candidate {
    root: Inst,
    kind: RootKind,
    rows: usize,
    columns: usize,
    element_ty: Type,
    accesses: Vec<Access>,
}

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
    if !element.is_scalar() || matches!(element.kind(), TypeKind::Array(..)) {
        return None;
    }
    rows.checked_mul(*columns)?;
    rows.checked_mul(*columns)?.checked_mul(element.size())?;
    Some((element.clone(), *rows, *columns))
}

fn collect_candidates(program: &Program) -> Vec<Candidate> {
    let mut roots = Vec::new();
    for &function in program.function_layout() {
        let data = program.func_data(function);
        for (&inst, inst_data) in data.local_arena().inst_arena().datas() {
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

fn collect_uses(program: &Program, candidate: &mut Candidate) -> bool {
    if candidate.root.is_global()
        && program
            .inst_data(candidate.root)
            .used_by()
            .iter()
            .any(|user| user.is_global())
    {
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
                    recognized_users.insert(inst);
                }
                _ => return false,
            }
        }
    }
    let all_local_uses_recognized = match candidate.kind {
        RootKind::Local(function) => {
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
                return false;
            }
            let Some((row_delta, column_delta)) =
                classify_access_delta(&context, looop, &induction, access.row, access.column)
            else {
                return false;
            };
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
            if current_stride == 0 && transposed_stride == 0 {
                continue;
            }
            let Some(executions) = loop_nest_executions(&context, &loops, &induction, block) else {
                return false;
            };
            let weight = executions.saturating_mul(access.memory_user_count as u128);
            costs.push(AccessCost {
                current_stride,
                transposed_stride,
                weight,
            });
        }
    }

    is_profitable(&costs, candidate.element_ty.size())
}

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
                return None;
            }
            let step = i64::from(exit.signed_step());
            result = Some((
                row_coefficient.checked_mul(step)?,
                column_coefficient.checked_mul(step)?,
            ));
        }
    }
    result.or_else(|| {
        (is_loop_invariant(data, looop, row) && is_loop_invariant(data, looop, column))
            .then_some((0, 0))
    })
}

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

fn rewrite_candidate(program: &mut Program, candidate: &Candidate) {
    let new_matrix_ty = Type::get_array(
        Type::get_array(candidate.element_ty.clone(), candidate.rows),
        candidate.columns,
    );
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
    match program.inst_data(init).kind() {
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
            let mut rows = Vec::with_capacity(candidate.columns);
            for values in transposed.chunks(candidate.rows) {
                rows.push(program.new_value().aggregate(values.to_vec()));
            }
            program
                .new_value()
                .raw(Aggregate::new_data(new_matrix_ty.clone(), rows))
        }
        _ => unreachable!(),
    }
}

fn restore_name(data: &mut ArenaContextMut<'_>, inst: Inst, name: Option<String>) {
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
