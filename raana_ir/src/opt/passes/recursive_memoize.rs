//! Memoize pure self-recursive functions (M68).
//!
//! A pure function `f(K, C)` where `C` is an additive accumulator is rewritten
//! into a memoized form `f_memo(K, C, cache, size)`. The cache stores, per
//! `K`, the residual `h(K) = f(K, 0)` together with a one-bit classification:
//! either `f(K, C) = C + h(K)` (leaf residual) or `f(K, C) = v(K)` (fixed,
//! independent of `C` — the "barrier" of h-1's `return 7`). A cache hit
//! returns the stored value directly; a miss runs the original body with its
//! self-recursion replaced by `f_memo` calls and then backfills the entry.
//!
//! The h-1 hotspot is `fun(n, dep)`: a single-path chain where `n` only ever
//! increases (guarded) or halves, so every `n` in `[1, lim]` appears as a
//! distinct node. Naive recursion visits ~3.16G nodes; memoizing the `n`-keyed
//! residual collapses the work to one call per `n` (50M) plus one chain-step
//! per miss (~33M) — a 63x reduction (measured: QEMU 22.79s -> ~3.7s).
//!
//! # Trigger (structural, input-independent)
//!
//! 1. `f` is pure self-recursive: no stores, no calls other than `f` itself,
//!    loads only from globals, exactly two `i32` parameters.
//! 2. One parameter `C` is an additive accumulator: every use of `C` is
//!    `C + k` (with `k` not `C`-derived), a direct self-call argument at the
//!    accumulator position, or a returned `C`. Non-accumulator arguments of
//!    every self-call are `C`-independent. Every self-call result is used
//!    only as a return value.
//! 3. The single external callsite lives in a natural loop whose header
//!    parameter (the callsite's key argument) is a forward induction variable
//!    compared against a loop-invariant bound. The cache size is `bound + 1`,
//!    allocated at runtime via the compiler-provided `soyo_calloc` builtin.
//!
//! Everything outside this shape is left untouched (宁漏勿错). The cache is a
//! packed `i32` array: `entry = (val << 2) | tag`, `tag ∈ {1, 2}` (1 = leaf
//! residual, 2 = fixed), `0` = empty. `val` must fit in the low 30 bits; a
//! runtime guard skips caching a value that does not. Out-of-bounds `K` and
//! out-of-range residuals degrade to the uncached computation (correctness is
//! preserved for every input).

use crate::{
    ir::{
        BasicBlock, BinaryOp, Function, Inst, InstKind, Program, Type,
        builder_trait::*,
    },
    opt::{
        analysis_passes::{
            induction_variable::{BasicInductionVariableAnalysis, InductionStep},
            loop_analysis::LoopAnalysis,
        },
        pass::{ArenaContextMut, Pass},
        prelude::*,
        utils::body_clone::BodyClonePlan,
    },
};

/// Compiler-provided runtime cache allocator. The AArch64 backend lowers every
/// call to this declared function to an embedded assembly wrapper that
/// zero-extends its two 32-bit arguments and tail-calls glibc `calloc`
/// (zero-initialized, so tag `0` = empty for free). AArch64-only: the memoize
/// pass is gated behind `TargetPolicy::enable_chain_to_switch`.
pub const CALLOO_NAME: &str = "soyo_calloc";

/// The packed cache: `entry = (val << 2) | tag`.
const TAG_LEAF: i32 = 1;
const TAG_FIXED: i32 = 2;

/// Mask of the top two bits: a value fits the packed 30-bit field iff
/// `value & FIT_MASK == 0`.
const FIT_MASK: i32 = -0x40000000;

pub struct RecursiveMemoize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReturnKind {
    /// `ret v` with `v = C + k`: stores residual `k`.
    Leaf,
    /// `ret v` with `v` independent of `C`: fixed value.
    Fixed,
    /// `ret f(child, C')`: the parent's residual derives from the child's.
    Rec,
}

struct MemoInfo {
    func: Function,
    key_pos: usize,
    acc_pos: usize,
    /// Return classification in layout order (the clone preserves the order).
    kinds: Vec<ReturnKind>,
}

enum BoundKind {
    Const(i32),
    GlobalLoad(Inst),
    /// A caller entry-block parameter (available everywhere).
    Param(Inst),
}

struct CallsiteInfo {
    caller: Function,
    call: Inst,
    preheader: BasicBlock,
    bound: BoundKind,
}

impl Pass for RecursiveMemoize {
    fn run(&mut self, program: &mut Program) -> bool {
        // A source function that owns the builtin name must never be shadowed
        // by the compiler-provided allocator.
        if program.function_layout().iter().any(|&f| {
            program.func_data(f).name() == CALLOO_NAME
                && program.func_data(f).layout().entry_bb().is_some()
        }) {
            return false;
        }

        let candidates = program
            .function_layout()
            .iter()
            .copied()
            .filter_map(|f| {
                let memo = detect(program, f)?;
                let callsite = detect_callsite(program, f, memo.key_pos, memo.acc_pos)?;
                Some((memo, callsite))
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return false;
        }

        let calloc = find_or_declare_calloc(program);
        let mut changed = false;
        for (memo, callsite) in candidates {
            let Some(f_memo) = build_memoized(program, &memo) else {
                continue;
            };
            rewrite_callsite(program, &callsite, f_memo, memo.key_pos, memo.acc_pos, calloc);
            changed = true;
        }
        changed
    }
}

fn find_or_declare_calloc(program: &mut Program) -> Function {
    for &f in program.function_layout() {
        if program.func_data(f).name() == CALLOO_NAME {
            return f;
        }
    }
    program.new_function(
        Type::get_pointer(Type::get_i32()),
        CALLOO_NAME.into(),
        vec![Type::get_i32(), Type::get_i32()],
    )
}

fn const_i32(data: &FunctionData, inst: Inst) -> Option<i32> {
    match data.inst_data(inst).kind() {
        InstKind::Integer(integer) => Some(integer.value()),
        _ => None,
    }
}

/// `C`-derived values: the fixpoint of `{C} ∪ {v : an operand of v is derived}`.
/// Only exact when no non-entry block has block parameters, which `detect`
/// enforces (after SSA, values then only flow through layout instructions).
fn derived_set(data: &FunctionData, acc: Inst) -> HashSet<Inst> {
    let mut derived: HashSet<Inst> = HashSet::from_iter([acc]);
    loop {
        let mut changed = false;
        for layout in data.layout().basicblocks() {
            for &inst in layout.insts() {
                if derived.contains(&inst) {
                    continue;
                }
                if data
                    .inst_data(inst)
                    .inst_usage()
                    .any(|operand| derived.contains(&operand))
                {
                    derived.insert(inst);
                    changed = true;
                }
            }
        }
        if !changed {
            return derived;
        }
    }
}

fn detect(program: &Program, f: Function) -> Option<MemoInfo> {
    let data = program.func_data(f);
    if data.layout().entry_bb().is_none() || !data.ret_ty().is_i32() {
        return None;
    }
    let params = data.params().to_vec();
    if params.len() != 2 || params.iter().any(|p| !data.inst_data(*p).ty().is_i32()) {
        return None;
    }

    let mut self_calls = Vec::new();
    let mut returns = Vec::new();
    let mut has_foreign_call = false;
    let mut has_side_effect = false;
    for layout in data.layout().basicblocks() {
        for &inst in layout.insts() {
            match data.inst_data(inst).kind() {
                InstKind::Call(call) => {
                    if call.callee() == f {
                        self_calls.push(inst);
                    } else {
                        has_foreign_call = true;
                    }
                }
                InstKind::TailCall(..) => has_foreign_call = true,
                InstKind::Return(ret) => returns.push((inst, ret.value())),
                InstKind::Store(_) | InstKind::MemZero(_) | InstKind::Alloc => {
                    has_side_effect = true
                }
                InstKind::Load(load) => {
                    if !load.src().is_global() {
                        return None;
                    }
                }
                _ => {}
            }
        }
    }
    if self_calls.is_empty() || has_foreign_call || has_side_effect {
        return None;
    }

    // Every self-call must have exactly two arguments and its result must be
    // used only as a return value.
    for &call in &self_calls {
        let call_data = data.inst_data(call);
        let InstKind::Call(call_kind) = call_data.kind() else {
            unreachable!()
        };
        if call_kind.args().len() != 2 || call_data.used_by().is_empty() {
            return None;
        }
        if call_data.used_by().iter().any(|&user| {
            !matches!(data.inst_data(user).kind(), InstKind::Return(_))
        }) {
            return None;
        }
    }

    // Non-entry blocks must be parameterless so `derived_set` is exact.
    let entry = data.layout().entry_bb().unwrap().bb();
    if data
        .layout()
        .basicblocks()
        .iter()
        .any(|layout| layout.bb() != entry && !data.bb_data(layout.bb()).params().is_empty())
    {
        return None;
    }

    for acc_pos in 0..2 {
        let key_pos = 1 - acc_pos;
        if let Some(kinds) = classify(program, f, &params, key_pos, acc_pos, &self_calls, &returns)
        {
            return Some(MemoInfo {
                func: f,
                key_pos,
                acc_pos,
                kinds,
            });
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn classify(
    program: &Program,
    f: Function,
    params: &[Inst],
    key_pos: usize,
    acc_pos: usize,
    self_calls: &[Inst],
    returns: &[(Inst, Option<Inst>)],
) -> Option<Vec<ReturnKind>> {
    let data = program.func_data(f);
    let acc = params[acc_pos];
    let derived = derived_set(data, acc);

    // Self-call arguments: the accumulator slot is `C` or `C + k` with `k`
    // not `C`-derived; the key slot is `C`-independent.
    for &call in self_calls {
        let InstKind::Call(call_kind) = data.inst_data(call).kind() else {
            unreachable!()
        };
        let args = call_kind.args();
        let acc_arg = args[acc_pos];
        let acc_ok = acc_arg == acc
            || matches!(
                data.inst_data(acc_arg).kind(),
                InstKind::Binary(bin)
                    if bin.op() == BinaryOp::Add
                        && ((bin.lhs() == acc && !derived.contains(&bin.rhs()))
                            || (bin.rhs() == acc && !derived.contains(&bin.lhs())))
            );
        if !acc_ok || derived.contains(&args[key_pos]) {
            return None;
        }
    }

    // Every use of `C` must be a self-call accumulator argument equal to `C`,
    // a returned `C`, or an `Add(C, k)` whose result is itself only used as a
    // self-call accumulator argument or a return value.
    for &user in data.inst_data(acc).used_by() {
        let ok = match data.inst_data(user).kind() {
            InstKind::Call(call) => call.callee() == f && call.args()[acc_pos] == acc,
            InstKind::Return(ret) => ret.value() == Some(acc),
            InstKind::Binary(bin) => {
                let is_add = bin.op() == BinaryOp::Add
                    && ((bin.lhs() == acc && !derived.contains(&bin.rhs()))
                        || (bin.rhs() == acc && !derived.contains(&bin.lhs())));
                is_add
                    && data.inst_data(user).used_by().iter().all(|&u| {
                        match data.inst_data(u).kind() {
                            InstKind::Call(call) => {
                                call.callee() == f && call.args()[acc_pos] == user
                            }
                            InstKind::Return(ret) => ret.value() == Some(user),
                            _ => false,
                        }
                    })
            }
            _ => false,
        };
        if !ok {
            return None;
        }
    }

    // Classify returns.
    let mut kinds = Vec::with_capacity(returns.len());
    let mut has_rec = false;
    let mut uses_acc = false;
    for (_, value) in returns {
        let Some(value) = *value else {
            return None;
        };
        if matches!(
            data.inst_data(value).kind(),
            InstKind::Call(call) if call.callee() == f
        ) {
            kinds.push(ReturnKind::Rec);
            has_rec = true;
        } else if derived.contains(&value) {
            kinds.push(ReturnKind::Leaf);
            uses_acc = true;
        } else {
            kinds.push(ReturnKind::Fixed);
        }
    }
    // The accumulator must genuinely drive the recursion or a leaf.
    if !has_rec || !uses_acc {
        return None;
    }
    Some(kinds)
}

fn detect_callsite(
    program: &Program,
    f: Function,
    key_pos: usize,
    acc_pos: usize,
) -> Option<CallsiteInfo> {
    let mut callsite: Option<(Function, Inst, BasicBlock)> = None;
    for &caller in program.function_layout() {
        if caller == f {
            continue;
        }
        let data = program.func_data(caller);
        for layout in data.layout().basicblocks() {
            for &inst in layout.insts() {
                if let InstKind::Call(call) = data.inst_data(inst).kind() {
                    if call.callee() == f {
                        if callsite.is_some() {
                            return None;
                        }
                        callsite = Some((caller, inst, layout.bb()));
                    }
                }
            }
        }
    }
    let (caller, call, call_bb) = callsite?;
    let data = program.func_data(caller);
    let InstKind::Call(call_kind) = data.inst_data(call).kind() else {
        return None;
    };
    if call_kind.args().len() != 2 {
        return None;
    }
    let key_arg = call_kind.args()[key_pos];
    let acc_arg = call_kind.args()[acc_pos];
    if !data.inst_data(key_arg).ty().is_i32() || !data.inst_data(acc_arg).ty().is_i32() {
        return None;
    }

    let (cfg, _dom, loops) = LoopAnalysis::new(data);
    let loop_index = loops.min_loop_contain_index(call_bb)?;
    let looop = &loops.loops()[loop_index];
    let ivs = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
    let iv = ivs.find(looop, key_arg)?;
    let step = match iv.step() {
        InductionStep::Add(step) => const_i32(data, step)?,
        InductionStep::Sub(_) => return None,
    };
    if step <= 0 {
        return None;
    }

    let header = looop.header();
    let terminator = data.layout().basicblock(header).terminator();
    let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
        return None;
    };
    let true_inside = looop.contains(branch.t_target());
    let false_inside = looop.contains(branch.f_target());
    if true_inside == false_inside {
        return None;
    }
    let InstKind::Binary(compare) = data.inst_data(branch.cond()).kind() else {
        return None;
    };
    if !compare.op().is_compare() {
        return None;
    }
    let (op, bound) = if compare.lhs() == key_arg {
        (compare.op(), compare.rhs())
    } else if compare.rhs() == key_arg {
        (compare.op().swap_compare_args()?, compare.lhs())
    } else {
        return None;
    };
    let mut op = op;
    if !true_inside {
        op = op.complement_integer_compare()?;
    }
    // Forward bounded induction: `key_arg <= bound` (or `< bound`).
    if op != BinaryOp::Le && op != BinaryOp::Lt {
        return None;
    }
    let bound = classify_bound(data, bound)?;

    // The loop body must be free of stores and of other calls, otherwise the
    // globals `f` reads (and the memoized results) could change across
    // iterations.
    for block in looop.body() {
        for &inst in data.layout().basicblock(*block).insts() {
            if inst == call {
                continue;
            }
            match data.inst_data(inst).kind() {
                InstKind::Store(_)
                | InstKind::MemZero(_)
                | InstKind::Call(_)
                | InstKind::TailCall(_) => return None,
                _ => {}
            }
        }
    }

    let preheader = looop.get_preheader(&cfg)?;
    Some(CallsiteInfo {
        caller,
        call,
        preheader,
        bound,
    })
}

fn classify_bound(data: &FunctionData, bound: Inst) -> Option<BoundKind> {
    match data.inst_data(bound).kind() {
        InstKind::Integer(integer) => Some(BoundKind::Const(integer.value())),
        InstKind::Load(load) if load.src().is_global() => {
            Some(BoundKind::GlobalLoad(load.src()))
        }
        InstKind::BlockArgRef(_) if data.params().contains(&bound) => {
            Some(BoundKind::Param(bound))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Rewrite
// ---------------------------------------------------------------------------

fn build_memoized(program: &mut Program, memo: &MemoInfo) -> Option<Function> {
    let plan = BodyClonePlan::capture(program, memo.func).ok()?;
    let key_ty = Type::get_i32();
    let acc_ty = Type::get_i32();
    let cache_ty = Type::get_pointer(Type::get_i32());
    let name = program.func_data(memo.func).name().to_owned();
    let f_memo = program.new_function(
        Type::get_i32(),
        format!("{name}_memo"),
        vec![key_ty.clone(), acc_ty.clone(), cache_ty.clone(), Type::get_i32()],
    );

    // Scope A: build the prologue (entry / in_bounds / hit / miss).
    let miss_block = {
        let mut builder = ArenaContextMut {
            program,
            curr_func: Some(f_memo),
        };
        let param_tys = vec![key_ty.clone(), acc_ty.clone(), cache_ty.clone(), Type::get_i32()];
        let entry = builder
            .new_basic_block()
            .basic_block("entry".into(), param_tys.clone());
        let in_bounds_block = builder
            .new_basic_block()
            .basic_block("memo_in_bounds".into(), param_tys.clone());
        let hit_block = builder.new_basic_block().basic_block(
            "memo_hit".into(),
            vec![key_ty, acc_ty, cache_ty.clone(), Type::get_i32(), Type::get_i32(), Type::get_i32()],
        );
        let miss_block = builder
            .new_basic_block()
            .basic_block("memo_miss".into(), param_tys);
        for block in [entry, in_bounds_block, hit_block, miss_block] {
            builder.layout_mut().push_bb_back(block);
        }
        let (key, acc, cache, size) = (
            builder.bb_data(entry).params()[0],
            builder.bb_data(entry).params()[1],
            builder.bb_data(entry).params()[2],
            builder.bb_data(entry).params()[3],
        );
        builder.set_params(vec![key, acc, cache, size]);

        // entry: in_bounds = (key >= 1) && (key < size); probe the cache if
        // in bounds, otherwise fall straight to the miss path.
        let one = builder.new_local_inst().integer(1);
        let ge = builder.new_local_inst().binary(BinaryOp::Ge, key, one);
        let lt = builder.new_local_inst().binary(BinaryOp::Lt, key, size);
        let in_bounds = builder.new_local_inst().binary(BinaryOp::And, ge, lt);
        let entry_branch = builder.new_local_inst().branch(
            in_bounds,
            in_bounds_block,
            vec![key, acc, cache, size],
            miss_block,
            vec![key, acc, cache, size],
        );
        for inst in [ge, lt, in_bounds, entry_branch] {
            builder.layout_mut().insert_inst(entry, inst);
        }

        // in_bounds_block: load the packed entry, compute the tag, branch.
        let (ib_key, ib_acc, ib_cache, ib_size) = (
            builder.bb_data(in_bounds_block).params()[0],
            builder.bb_data(in_bounds_block).params()[1],
            builder.bb_data(in_bounds_block).params()[2],
            builder.bb_data(in_bounds_block).params()[3],
        );
        let three = builder.new_local_inst().integer(3);
        let k_ptr = builder.new_local_inst().get_elem_ptr(ib_cache, vec![ib_key]);
        let entry_val = builder.new_local_inst().load(k_ptr);
        let tag = builder.new_local_inst().binary(BinaryOp::And, entry_val, three);
        let zero = builder.new_local_inst().integer(0);
        let hit = builder.new_local_inst().binary(BinaryOp::NotEq, tag, zero);
        let ib_branch = builder.new_local_inst().branch(
            hit,
            hit_block,
            vec![ib_key, ib_acc, ib_cache, ib_size, entry_val, tag],
            miss_block,
            vec![ib_key, ib_acc, ib_cache, ib_size],
        );
        for inst in [k_ptr, entry_val, tag, hit, ib_branch] {
            builder.layout_mut().insert_inst(in_bounds_block, inst);
        }

        // hit_block: tag is 1 (leaf) -> C + val, or 2 (fixed) -> val.
        let (_, h_acc, _, _, h_val, h_tag) = (
            builder.bb_data(hit_block).params()[0],
            builder.bb_data(hit_block).params()[1],
            builder.bb_data(hit_block).params()[2],
            builder.bb_data(hit_block).params()[3],
            builder.bb_data(hit_block).params()[4],
            builder.bb_data(hit_block).params()[5],
        );
        let two = builder.new_local_inst().integer(2);
        let val = builder.new_local_inst().binary(BinaryOp::Shr, h_val, two);
        let is_leaf = builder.new_local_inst().binary(BinaryOp::Eq, h_tag, one);
        let c_plus = builder.new_local_inst().binary(BinaryOp::Add, h_acc, val);
        let res = builder.new_local_inst().select(is_leaf, c_plus, val);
        let h_ret = builder.new_local_inst().ret(Some(res));
        for inst in [val, is_leaf, c_plus, res, h_ret] {
            builder.layout_mut().insert_inst(hit_block, inst);
        }
        miss_block
    };

    let cloned = plan.clone_into(program, f_memo, miss_block).ok()?;

    // Scope B: thread (K, cache, size) through every cloned block, redirect
    // self-calls, and turn each return into a guarded store + ret. The cloned
    // entry's own `C` parameter dominates the whole body, so the leaf residual
    // and recursion increment are derived structurally from it (no `C`
    // threading).
    let mut builder = ArenaContextMut {
        program,
        curr_func: Some(f_memo),
    };

    let mut threaded: HashMap<BasicBlock, (Inst, Inst, Inst)> = HashMap::default();
    for &block in &cloned.blocks {
        let k_p = builder.new_basic_block().add_param(block, Type::get_i32());
        let cache_p = builder
            .new_basic_block()
            .add_param(block, cache_ty.clone());
        let size_p = builder.new_basic_block().add_param(block, Type::get_i32());
        threaded.insert(block, (k_p, cache_p, size_p));
    }

    // The miss block jumps into the cloned entry, forwarding the invocation's
    // (K, C) plus the threaded (K, cache, size).
    let (m_key, m_acc, m_cache, m_size) = (
        builder.bb_data(miss_block).params()[0],
        builder.bb_data(miss_block).params()[1],
        builder.bb_data(miss_block).params()[2],
        builder.bb_data(miss_block).params()[3],
    );
    let miss_jump = builder.new_local_inst().jump(
        cloned.entry,
        vec![m_key, m_acc, m_key, m_cache, m_size],
    );
    builder.layout_mut().insert_inst(miss_block, miss_jump);

    // Append (K, cache, size) to every outgoing logical edge.
    enum TerminatorData {
        Jump(BasicBlock, Vec<Inst>),
        Branch(Inst, BasicBlock, Vec<Inst>, BasicBlock, Vec<Inst>),
    }
    for &block in &cloned.blocks {
        let (k_p, cache_p, size_p) = threaded[&block];
        let terminator = builder.layout().basicblock(block).terminator();
        let data = match builder.inst_data(terminator).kind() {
            InstKind::Jump(jump) => TerminatorData::Jump(jump.target(), jump.args().to_vec()),
            InstKind::Branch(branch) => TerminatorData::Branch(
                branch.cond(),
                branch.t_target(),
                branch.t_args().to_vec(),
                branch.f_target(),
                branch.f_args().to_vec(),
            ),
            _ => continue,
        };
        match data {
            TerminatorData::Jump(target, mut args) => {
                args.extend([k_p, cache_p, size_p]);
                builder.replace_inst_with(terminator).jump(target, args);
            }
            TerminatorData::Branch(cond, t_target, mut t_args, f_target, mut f_args) => {
                t_args.extend([k_p, cache_p, size_p]);
                f_args.extend([k_p, cache_p, size_p]);
                builder
                    .replace_inst_with(terminator)
                    .branch(cond, t_target, t_args, f_target, f_args);
            }
        }
    }

    // Redirect every cloned self-call to f_memo with the threaded cache args.
    for &block in &cloned.blocks {
        let (_, cache_p, size_p) = threaded[&block];
        let insts: Vec<Inst> = builder
            .layout()
            .basicblock(block)
            .insts()
            .iter()
            .copied()
            .collect();
        for inst in insts {
            let InstKind::Call(call) = builder.inst_data(inst).kind() else {
                continue;
            };
            if call.callee() != memo.func {
                continue;
            }
            let mut args = call.args().to_vec();
            args.push(cache_p);
            args.push(size_p);
            builder
                .replace_inst_with(inst)
                .call_with_type(f_memo, args, Type::get_i32());
        }
    }

    // The cloned entry's accumulator parameter, from which the leaf residual
    // and recursion increment are derived. It dominates every block.
    let acc_param = builder.bb_data(cloned.entry).params()[memo.acc_pos];

    // Transform every cloned return into a guarded store followed by the ret.
    for (i, &ret) in cloned.returns.iter().enumerate() {
        let kind = memo.kinds[i];
        let block = builder.layout().parent_bb(ret).expect("return is laid out");
        let (k_p, cache_p, size_p) = threaded[&block];
        let value = match builder.inst_data(ret).kind() {
            InstKind::Return(return_kind) => return_kind.value(),
            _ => unreachable!(),
        };

        let (cache_val, tag, extra_ok) = match kind {
            ReturnKind::Leaf => {
                let value = value.unwrap();
                // Residual: `C + k` with `k` not `C`-derived.
                let res = residual_of(&mut builder, acc_param, value);
                insert_fresh(&mut builder, block, res);
                (res, builder.new_local_inst().integer(TAG_LEAF), None)
            }
            ReturnKind::Fixed => {
                let value = value.unwrap();
                (value, builder.new_local_inst().integer(TAG_FIXED), None)
            }
            ReturnKind::Rec => {
                let value = value.unwrap();
                let InstKind::Call(call) = builder.inst_data(value).kind() else {
                    unreachable!("recursion return value must be the call result");
                };
                let child = call.args()[0];
                let c_prime = call.args()[1];
                let zero = builder.new_local_inst().integer(0);
                let one_i = builder.new_local_inst().integer(1);
                let two = builder.new_local_inst().integer(2);
                let three = builder.new_local_inst().integer(3);
                let c_ge = builder.new_local_inst().binary(BinaryOp::Ge, child, one_i);
                let c_lt = builder.new_local_inst().binary(BinaryOp::Lt, child, size_p);
                let child_ib = builder.new_local_inst().binary(BinaryOp::And, c_ge, c_lt);
                let child_ptr = builder.new_local_inst().get_elem_ptr(cache_p, vec![child]);
                let child_entry = builder.new_local_inst().load(child_ptr);
                let child_ne0 =
                    builder.new_local_inst().binary(BinaryOp::NotEq, child_entry, zero);
                let child_tag = builder.new_local_inst().binary(BinaryOp::And, child_entry, three);
                let child_val = builder.new_local_inst().binary(BinaryOp::Shr, child_entry, two);
                // Increment: the non-`C` part of the recursion's accumulator
                // argument `C' = C + inc`.
                let inc = residual_of(&mut builder, acc_param, c_prime);
                let is_leaf = builder.new_local_inst().binary(BinaryOp::Eq, child_tag, one_i);
                let bumped = builder.new_local_inst().binary(BinaryOp::Add, child_val, inc);
                let newval = builder.new_local_inst().select(is_leaf, bumped, child_val);
                let ok = builder.new_local_inst().binary(BinaryOp::And, child_ib, child_ne0);
                // `inc` may be a constant or an already-laid-out operand.
                insert_fresh(&mut builder, block, inc);
                for inst in [
                    c_ge, c_lt, child_ib, child_ptr, child_entry, child_ne0, child_tag,
                    child_val, is_leaf, bumped, newval, ok,
                ] {
                    builder.layout_mut().insert_before_terminator(block, inst);
                }
                (newval, child_tag, Some(ok))
            }
        };

        emit_store(
            &mut builder,
            block,
            k_p,
            cache_p,
            size_p,
            cache_val,
            tag,
            extra_ok,
        );
    }

    Some(f_memo)
}

/// The residual `k` of an accumulator expression `C + k` (or `k + C`): the
/// operand that does not depend on `C`. Returns `0` for a bare `C`. The
/// defensive fallback recomputes `value - C`, which is exact because `C` is
/// the cloned entry's accumulator parameter dominating every block.
fn residual_of(builder: &mut ArenaContextMut<'_>, acc: Inst, value: Inst) -> Inst {
    if value == acc {
        return builder.new_local_inst().integer(0);
    }
    match builder.inst_data(value).kind() {
        InstKind::Binary(bin) if bin.op() == BinaryOp::Add => {
            if bin.lhs() == acc {
                return bin.rhs();
            }
            if bin.rhs() == acc {
                return bin.lhs();
            }
        }
        _ => {}
    }
    builder.new_local_inst().binary(BinaryOp::Sub, value, acc)
}

/// Insert `inst` into `block` only when it is a freshly created, non-constant
/// value. Constants (integers) and values already owned by a layout block must
/// stay out of the block layout.
fn insert_fresh(builder: &mut ArenaContextMut<'_>, block: BasicBlock, inst: Inst) {
    if builder.inst_data(inst).kind().is_const() {
        return;
    }
    if builder.layout().parent_bb(inst).is_some() {
        return;
    }
    builder.layout_mut().insert_before_terminator(block, inst);
}

fn emit_store(
    builder: &mut ArenaContextMut<'_>,
    block: BasicBlock,
    k: Inst,
    cache: Inst,
    size: Inst,
    val: Inst,
    tag: Inst,
    extra_ok: Option<Inst>,
) {
    let zero = builder.new_local_inst().integer(0);
    let one = builder.new_local_inst().integer(1);
    let two = builder.new_local_inst().integer(2);
    let mask = builder.new_local_inst().integer(FIT_MASK);
    let ge = builder.new_local_inst().binary(BinaryOp::Ge, k, one);
    let lt = builder.new_local_inst().binary(BinaryOp::Lt, k, size);
    let k_ib = builder.new_local_inst().binary(BinaryOp::And, ge, lt);
    let shl = builder.new_local_inst().binary(BinaryOp::Shl, val, two);
    let packed = builder.new_local_inst().binary(BinaryOp::Or, shl, tag);
    let andv = builder.new_local_inst().binary(BinaryOp::And, val, mask);
    let fits = builder.new_local_inst().binary(BinaryOp::Eq, andv, zero);
    let mut ok = builder.new_local_inst().binary(BinaryOp::And, k_ib, fits);
    let mut to_insert = vec![ge, lt, k_ib, shl, packed, andv, fits, ok];
    if let Some(extra) = extra_ok {
        let combined = builder.new_local_inst().binary(BinaryOp::And, ok, extra);
        to_insert.push(combined);
        ok = combined;
    }
    let sel = builder.new_local_inst().select(ok, packed, zero);
    let ptr = builder.new_local_inst().get_elem_ptr(cache, vec![k]);
    let store = builder.new_local_inst().store(sel, ptr);
    to_insert.extend([sel, ptr, store]);
    for inst in to_insert {
        builder.layout_mut().insert_before_terminator(block, inst);
    }
}

fn rewrite_callsite(
    program: &mut Program,
    callsite: &CallsiteInfo,
    f_memo: Function,
    key_pos: usize,
    acc_pos: usize,
    calloc: Function,
) {
    let mut builder = ArenaContextMut {
        program,
        curr_func: Some(callsite.caller),
    };

    // Re-emit the loop bound in the preheader and derive the cache size.
    let bound = match &callsite.bound {
        BoundKind::Const(value) => builder.new_local_inst().integer(*value),
        BoundKind::GlobalLoad(global) => {
            // FunctionData's Arena cannot address global instructions; build
            // the re-emitted load through a LocalBuilder rooted at the
            // ArenaContextMut (which can).
            let pointee = builder.inst_data(*global).ty().derefernce();
            crate::ir::builder::LocalBuilder {
                arena: &mut builder,
            }
            .raw(crate::ir::inst_kind::Load::new_data(*global, pointee))
        }
        BoundKind::Param(param) => *param,
    };
    let zero = builder.new_local_inst().integer(0);
    let one = builder.new_local_inst().integer(1);
    let four = builder.new_local_inst().integer(4);
    let size_raw = builder.new_local_inst().binary(BinaryOp::Add, bound, one);
    let ge0 = builder.new_local_inst().binary(BinaryOp::Ge, size_raw, zero);
    let size = builder.new_local_inst().select(ge0, size_raw, zero);
    let calloc_call = builder
        .new_local_inst()
        .call_with_type(calloc, vec![size, four], Type::get_pointer(Type::get_i32()));
    // `bound` is a re-emitted global load (laid out) or an inline operand
    // (constant / block parameter); only lay out the former, and always before
    // the size arithmetic that reads it.
    if matches!(callsite.bound, BoundKind::GlobalLoad(_)) {
        builder
            .layout_mut()
            .insert_before_terminator(callsite.preheader, bound);
    }
    for inst in [size_raw, ge0, size, calloc_call] {
        builder.layout_mut().insert_before_terminator(callsite.preheader, inst);
    }

    // Rewrite the callsite to pass the cache pointer and size.
    let InstKind::Call(call_kind) = builder.inst_data(callsite.call).kind() else {
        return;
    };
    let key_arg = call_kind.args()[key_pos];
    let acc_arg = call_kind.args()[acc_pos];
    builder
        .replace_inst_with(callsite.call)
        .call_with_type(f_memo, vec![key_arg, acc_arg, calloc_call, size], Type::get_i32());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Program, Type, builder_trait::*};
    use crate::opt::pass::Pass as _;

    /// Build the h-1-shaped recursion `fun(n, dep)`:
    /// `n == 1 -> dep`, otherwise `fun(n / 2, dep + 1)`.
    fn build_fun(program: &mut Program) -> Function {
        let f = program.new_function(
            Type::get_i32(),
            "fun".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        {
            let data = program.func_data_mut(f);
            let entry = data.add_entry_block();
            let leaf = data.new_basic_block().basic_block("leaf".into(), vec![]);
            let rec = data.new_basic_block().basic_block("rec".into(), vec![]);
            data.layout_mut().push_bb_back(leaf);
            data.layout_mut().push_bb_back(rec);
            let (n, dep) = (data.params()[0], data.params()[1]);
            let one = data.new_local_inst().integer(1);
            let eq = data.new_local_inst().binary(BinaryOp::Eq, n, one);
            let br = data
                .new_local_inst()
                .branch(eq, leaf, vec![], rec, vec![]);
            data.layout_mut().insert_inst(entry, eq);
            data.layout_mut().insert_inst(entry, br);

            let ret_dep = data.new_local_inst().ret(Some(dep));
            data.layout_mut().insert_inst(leaf, ret_dep);

            let two = data.new_local_inst().integer(2);
            let half = data.new_local_inst().binary(BinaryOp::Div, n, two);
            let c1 = data.new_local_inst().binary(BinaryOp::Add, dep, one);
            let call = data
                .new_local_inst()
                .call_with_type(f, vec![half, c1], Type::get_i32());
            let ret_call = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(rec, half);
            data.layout_mut().insert_inst(rec, c1);
            data.layout_mut().insert_inst(rec, call);
            data.layout_mut().insert_inst(rec, ret_call);
        }
        f
    }

    /// Build `main(bound)`: a forward `i <= bound` loop calling `fun(i, 0)`.
    fn build_main(program: &mut Program, f: Function) {
        let main = program.new_function(
            Type::get_i32(),
            "main".into(),
            vec![Type::get_i32()],
        );
        {
            let data = program.func_data_mut(main);
            let entry = data.add_entry_block();
            let header = data.new_basic_block().basic_block("header".into(), vec![Type::get_i32()]);
            let body = data.new_basic_block().basic_block("body".into(), vec![]);
            let exit = data.new_basic_block().basic_block("exit".into(), vec![]);
            for block in [header, body, exit] {
                data.layout_mut().push_bb_back(block);
            }
            let bound = data.params()[0];
            let zero = data.new_local_inst().integer(0);
            let entry_jump = data.new_local_inst().jump(header, vec![zero]);
            data.layout_mut().insert_inst(entry, entry_jump);

            let iv = data.bb_data(header).params()[0];
            let le = data.new_local_inst().binary(BinaryOp::Le, iv, bound);
            let header_br = data
                .new_local_inst()
                .branch(le, body, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, le);
            data.layout_mut().insert_inst(header, header_br);

            let zero_c = data.new_local_inst().integer(0);
            let call = data
                .new_local_inst()
                .call_with_type(f, vec![iv, zero_c], Type::get_i32());
            let one = data.new_local_inst().integer(1);
            let next = data.new_local_inst().binary(BinaryOp::Add, iv, one);
            let body_jump = data.new_local_inst().jump(header, vec![next]);
            data.layout_mut().insert_inst(body, call);
            data.layout_mut().insert_inst(body, one);
            data.layout_mut().insert_inst(body, next);
            data.layout_mut().insert_inst(body, body_jump);

            let ret = data.new_local_inst().ret(Some(zero));
            data.layout_mut().insert_inst(exit, ret);
        }
    }

    #[test]
    fn detects_the_accumulator_shape() {
        let mut program = Program::new();
        let f = build_fun(&mut program);
        let info = detect(&program, f).expect("fun(n, dep) must be recognized");
        assert_eq!(info.key_pos, 0);
        assert_eq!(info.acc_pos, 1);
    }

    #[test]
    fn detects_the_loop_callsite() {
        let mut program = Program::new();
        let f = build_fun(&mut program);
        build_main(&mut program, f);
        let info = detect(&program, f).unwrap();
        let callsite =
            detect_callsite(&program, f, info.key_pos, info.acc_pos).expect("loop callsite");
        assert_eq!(program.func_data(callsite.caller).name(), "main");
        assert!(matches!(callsite.bound, BoundKind::Param(_)));
    }

    #[test]
    fn rejects_an_accumulator_used_in_a_comparison() {
        let mut program = Program::new();
        let f = program.new_function(
            Type::get_i32(),
            "bad".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        {
            let data = program.func_data_mut(f);
            let entry = data.add_entry_block();
            let leaf = data.new_basic_block().basic_block("leaf".into(), vec![]);
            let rec = data.new_basic_block().basic_block("rec".into(), vec![]);
            data.layout_mut().push_bb_back(leaf);
            data.layout_mut().push_bb_back(rec);
            let (n, dep) = (data.params()[0], data.params()[1]);
            let one = data.new_local_inst().integer(1);
            // `dep < 0` compares the accumulator: not a pure accumulator.
            let zero = data.new_local_inst().integer(0);
            let cmp = data.new_local_inst().binary(BinaryOp::Lt, dep, zero);
            let br = data
                .new_local_inst()
                .branch(cmp, leaf, vec![], rec, vec![]);
            data.layout_mut().insert_inst(entry, cmp);
            data.layout_mut().insert_inst(entry, br);
            let ret_dep = data.new_local_inst().ret(Some(dep));
            data.layout_mut().insert_inst(leaf, ret_dep);
            let two = data.new_local_inst().integer(2);
            let half = data.new_local_inst().binary(BinaryOp::Div, n, two);
            let c1 = data.new_local_inst().binary(BinaryOp::Add, dep, one);
            let call = data
                .new_local_inst()
                .call_with_type(f, vec![half, c1], Type::get_i32());
            let ret_call = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(rec, half);
            data.layout_mut().insert_inst(rec, c1);
            data.layout_mut().insert_inst(rec, call);
            data.layout_mut().insert_inst(rec, ret_call);
        }
        assert!(detect(&program, f).is_none());
    }

    #[test]
    fn rejects_when_the_key_argument_depends_on_the_accumulator() {
        let mut program = Program::new();
        let f = program.new_function(
            Type::get_i32(),
            "bad_key".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        {
            let data = program.func_data_mut(f);
            let entry = data.add_entry_block();
            let leaf = data.new_basic_block().basic_block("leaf".into(), vec![]);
            let rec = data.new_basic_block().basic_block("rec".into(), vec![]);
            data.layout_mut().push_bb_back(leaf);
            data.layout_mut().push_bb_back(rec);
            let (n, dep) = (data.params()[0], data.params()[1]);
            let one = data.new_local_inst().integer(1);
            let eq = data.new_local_inst().binary(BinaryOp::Eq, n, one);
            let br = data
                .new_local_inst()
                .branch(eq, leaf, vec![], rec, vec![]);
            data.layout_mut().insert_inst(entry, eq);
            data.layout_mut().insert_inst(entry, br);
            let ret_dep = data.new_local_inst().ret(Some(dep));
            data.layout_mut().insert_inst(leaf, ret_dep);
            // Recursion key depends on `dep`: `fun(n + dep, dep + 1)`.
            let key = data.new_local_inst().binary(BinaryOp::Add, n, dep);
            let c1 = data.new_local_inst().binary(BinaryOp::Add, dep, one);
            let call = data
                .new_local_inst()
                .call_with_type(f, vec![key, c1], Type::get_i32());
            let ret_call = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(rec, key);
            data.layout_mut().insert_inst(rec, c1);
            data.layout_mut().insert_inst(rec, call);
            data.layout_mut().insert_inst(rec, ret_call);
        }
        assert!(detect(&program, f).is_none());
    }

    #[test]
    fn rewrites_the_recursion_and_the_callsite() {
        let mut program = Program::new();
        let f = build_fun(&mut program);
        build_main(&mut program, f);

        assert!(RecursiveMemoize.run(&mut program));

        // A memoized clone exists and the original callsite now targets it.
        let memo_func = program
            .function_layout()
            .iter()
            .copied()
            .find(|&g| program.func_data(g).name() == "fun_memo")
            .expect("fun_memo must be created");
        let mut calls_fun_memo = 0;
        let mut calls_original = 0;
        for &g in program.function_layout() {
            let data = program.func_data(g);
            for layout in data.layout().basicblocks() {
                for &inst in layout.insts() {
                    if let InstKind::Call(call) = data.inst_data(inst).kind() {
                        if call.callee() == memo_func {
                            calls_fun_memo += 1;
                        }
                        // Only the dead original may still recurse to itself.
                        if call.callee() == f && g != f {
                            calls_original += 1;
                        }
                    }
                }
            }
        }
        assert!(calls_fun_memo >= 2, "self-recursion plus the callsite");
        // No live caller references the original function.
        assert_eq!(calls_original, 0);
        // The allocator is declared.
        assert!(
            program
                .function_layout()
                .iter()
                .any(|&g| program.func_data(g).name() == CALLOO_NAME)
        );
    }

    #[test]
    fn the_pass_is_idempotent() {
        let mut program = Program::new();
        let f = build_fun(&mut program);
        build_main(&mut program, f);
        assert!(RecursiveMemoize.run(&mut program));
        // The original `fun` is no longer a candidate (no external callsite).
        let again = RecursiveMemoize.run(&mut program);
        assert!(!again);
    }
}
