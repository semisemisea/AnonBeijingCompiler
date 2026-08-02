//! Scalar global promotion (GSP).
//!
//! A scalar global that is only ever loaded/stored through its own address
//! (never address-taken, never passed to a call) behaves like a local
//! variable as long as no called function can observe it. Promotion threads
//! the global's value through the function in SSA form (load once at entry,
//! stores become defs, one write-back before each return), so the backend
//! keeps it in registers instead of reloading/re-storing on every access —
//! clang's shape for the `bits`/`pos`/`size` globals in huffman-01.
//!
//! Eligibility is deliberately conservative (`宁漏勿错`):
//!
//! - The global must have scalar type and every use across the whole program
//!   must be a direct `load`/`store` of the global's address (no
//!   `getelemptr`, no escaping into call arguments or returns).
//! - A function may only be rewritten when none of its callees (transitively)
//!   touches the global. Runtime/declared functions never touch user globals
//!   and therefore never block promotion.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::ir::inst_kind;
use crate::opt::prelude::*;

pub struct ScalarGlobalPromotion;

impl Pass for ScalarGlobalPromotion {
    fn run(&self, program: &mut Program) -> bool {
        let may_touch = call_analysis(program);
        let eligible = eligible_globals(program);
        let mut changed = false;
        for func in program.function_layout().to_vec() {
            let mut arena_context = ArenaContextMut {
                program,
                curr_func: Some(func),
            };
            changed |= promote_function(&mut arena_context, &may_touch, &eligible);
        }
        changed
    }
}

/// Program-wide eligibility: the global must be a scalar pointer whose every
/// use in every function is a direct load or store of the global address.
fn eligible_globals(program: &Program) -> HashSet<Inst> {
    let mut eligible: HashMap<Inst, bool> = program
        .global_arena()
        .inst_arena()
        .datas()
        .map(|(inst, data)| {
            let ty = data.ty();
            (*inst, ty.is_pointer() && ty.derefernce().is_scalar())
        })
        .collect();
    for func in program.function_layout() {
        let func_data = program.func_data(*func);
        for layout in func_data.layout().basicblocks() {
            for &inst in layout.insts() {
                let kind = func_data.inst_data(inst).kind();
                // A load/store is fine only when it accesses the global
                // through the global's own address; every other use
                // (getelemptr, call, select, branch condition, ...)
                // disqualifies the global.
                for used in func_data.inst_data(inst).inst_usage() {
                    if let Some(ok) = eligible.get_mut(&used) {
                        let direct = matches!(kind, InstKind::Load(load) if load.src() == used)
                            || matches!(kind, InstKind::Store(store) if store.dest() == used);
                        if !direct {
                            *ok = false;
                        }
                    }
                }
            }
        }
    }
    eligible.into_iter().filter(|(_, ok)| *ok).map(|(g, _)| g).collect()
}

/// Function -> set of globals it may load or store, transitively through
/// direct calls.
fn call_analysis(program: &Program) -> HashMap<Function, HashSet<Inst>> {
    let mut direct: HashMap<Function, HashSet<Inst>> = HashMap::new();
    for func in program.function_layout() {
        let func_data = program.func_data(*func);
        let mut touched = HashSet::new();
        for layout in func_data.layout().basicblocks() {
            for &inst in layout.insts() {
                match func_data.inst_data(inst).kind() {
                    InstKind::Load(load) => {
                        touched.insert(load.src());
                    }
                    InstKind::Store(store) => {
                        touched.insert(store.dest());
                    }
                    _ => {}
                }
            }
        }
        direct.insert(*func, touched);
    }

    // Transitive closure over direct calls.
    let mut may_touch = direct.clone();
    let mut changed = true;
    while changed {
        changed = false;
        for func in program.function_layout() {
            let func_data = program.func_data(*func);
            let callees: Vec<Function> = func_data
                .layout()
                .basicblocks()
                .iter()
                .flat_map(|layout| layout.insts().iter())
                .filter_map(|&inst| match func_data.inst_data(inst).kind() {
                    InstKind::Call(call) => Some(call.callee()),
                    _ => None,
                })
                .collect();
            let own: HashSet<Inst> = may_touch[func].clone();
            let mut extra = Vec::new();
            for callee in &callees {
                if let Some(touched) = may_touch.get(callee) {
                    extra.extend(touched.iter().copied().filter(|x| !own.contains(x)));
                }
            }
            if !extra.is_empty() {
                may_touch.get_mut(func).unwrap().extend(extra);
                changed = true;
            }
        }
    }
    may_touch
}

/// Promote every eligible global inside the current function.
fn promote_function(
    data: &mut ArenaContextMut<'_>,
    may_touch: &HashMap<Function, HashSet<Inst>>,
    eligible: &HashSet<Inst>,
) -> bool {
    if data.layout().entry_bb().is_none() {
        return false;
    }
    let mut changed = false;
    let globals: Vec<Inst> = data.global().inst_arena().datas().map(|(i, _)| *i).collect();
    for global in globals {
        if !eligible.contains(&global) {
            continue;
        }
        if !function_uses_global(data, global) {
            continue;
        }
        if !promotable_in(data, global, may_touch) {
            continue;
        }
        changed |= promote(data, global);
    }
    changed
}

fn function_uses_global(data: &ArenaContextMut<'_>, global: Inst) -> bool {
    data.layout().basicblocks().iter().any(|layout| {
        layout.insts().iter().any(|&inst| {
            matches!(data.inst_data(inst).kind(), InstKind::Load(load) if load.src() == global)
                || matches!(data.inst_data(inst).kind(), InstKind::Store(store) if store.dest() == global)
        })
    })
}

/// The current function may be rewritten only when none of its
/// transitively-reachable callees touches the global.
fn promotable_in(
    data: &ArenaContextMut<'_>,
    global: Inst,
    may_touch: &HashMap<Function, HashSet<Inst>>,
) -> bool {
    for layout in data.layout().basicblocks() {
        for &inst in layout.insts() {
            if let InstKind::Call(call) = data.inst_data(inst).kind() {
                if may_touch.get(&call.callee()).is_some_and(|g| g.contains(&global)) {
                    return false;
                }
            }
        }
    }
    true
}

/// Thread `global` through the CFG in SSA form inside the current function.
fn promote(data: &mut ArenaContextMut<'_>, global: Inst) -> bool {
    let entry_bb = data.layout().entry_bb().unwrap().bb();
    let pointee = data.inst_data(global).ty().derefernce().clone();

    let mut bb_id = utils::IDAllocator::new(1);
    let (graph, prece) = cfg::build_cfg_both(data, &mut bb_id);
    let rpo_path = cfg::rpo_path(&graph);
    let idom_map = dom_tree::idom(&prece, &rpo_path);
    let dom_frontier = dominance_frontier(&bb_id, &prece, &idom_map);

    // Blocks that store to the global define a new value.
    let def_blocks: HashSet<usize> = data
        .layout()
        .basicblocks()
        .iter()
        .filter(|layout| {
            layout.insts().iter().any(|&inst| {
                matches!(data.inst_data(inst).kind(), InstKind::Store(store) if store.dest() == global)
            })
        })
        .map(|layout| bb_id.get_id(&layout.bb()))
        .collect();

    // Insert a block parameter for the global value at every dominance
    // frontier of a defining block.
    let mut param_of_block: HashMap<usize, Inst> = HashMap::new();
    let mut worked: HashSet<usize> = HashSet::new();
    for &def_id in &def_blocks {
        let mut work_queue: VecDeque<usize> = dom_frontier
            .get(&def_id)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        while let Some(front) = work_queue.pop_front() {
            if !worked.insert(front) {
                continue;
            }
            let bb = bb_id.search_id(front);
            let param = data.new_basic_block().add_param(bb, pointee.clone());
            param_of_block.insert(front, param);
            if let Some(sub) = dom_frontier.get(&front) {
                for &s in sub {
                    work_queue.push_back(s);
                }
            }
        }
    }

    // Entry load: the initial value of the global. Built through a builder
    // whose arena can address global instructions (FunctionData's Arena
    // cannot).
    let entry_load = builder(data).insert_inst(inst_kind::Load::new_data(global, pointee.clone()));
    let entry_first = *data.layout().basicblock(entry_bb).insts().get_first().unwrap();
    data.layout_mut().insert_inst_before(entry_first, entry_load);

    // Thread the value through the CFG in dominator-tree pre-order.
    let dom_tree = dom_tree::build_dominance_tree(&idom_map, rpo_path.len());
    let mut stack: Vec<Inst> = vec![entry_load];
    let mut remove_list: Vec<(Inst, BasicBlock)> = Vec::new();
    let mut write_backs: Vec<(Inst, BasicBlock)> = Vec::new();
    thread(
        0,
        &dom_tree,
        &bb_id,
        data,
        global,
        entry_load,
        &param_of_block,
        &mut stack,
        &mut remove_list,
        &mut write_backs,
    );

    let mut changed = false;
    for (inst, bb) in remove_list {
        data.remove_layout_inst(bb, inst);
        changed = true;
    }
    for (value, bb) in write_backs {
        let store = builder(data).insert_inst(inst_kind::Store::new_data(value, global));
        let terminator = data.layout().basicblock(bb).terminator();
        data.layout_mut().insert_inst_before(terminator, store);
        changed = true;
    }
    changed
}

/// A local-instruction builder whose arena can also address global
/// instructions.
fn builder<'a>(data: &'a mut ArenaContextMut<'_>) -> crate::ir::builder::LocalBuilder<'a> {
    crate::ir::builder::LocalBuilder { arena: data }
}

/// Dom-frontier computation (Wikipedia algorithm, as in the SSA pass).
fn dominance_frontier(
    id_alloca: &utils::IDAllocator<BasicBlock, usize>,
    prece: &CFGGraph,
    idom_map: &IDomMap,
) -> HashMap<usize, HashSet<usize>> {
    let mut frontier: HashMap<usize, HashSet<usize>> = HashMap::new();
    for bb in 0..id_alloca.cnt() {
        if let Some(preds) = prece.get(&bb) {
            if preds.len() >= 2 {
                for &pre in preds {
                    let mut runner = pre;
                    while runner != idom_map[bb] {
                        frontier.entry(runner).or_default().insert(bb);
                        runner = idom_map[runner];
                    }
                }
            }
        }
    }
    frontier
}

#[allow(clippy::too_many_arguments)]
fn thread(
    node: usize,
    tree: &DomTree,
    bb_id: &utils::IDAllocator<BasicBlock, usize>,
    data: &mut ArenaContextMut<'_>,
    global: Inst,
    entry_load: Inst,
    param_of_block: &HashMap<usize, Inst>,
    stack: &mut Vec<Inst>,
    remove_list: &mut Vec<(Inst, BasicBlock)>,
    write_backs: &mut Vec<(Inst, BasicBlock)>,
) {
    let bb = bb_id.search_id(node);
    let mut pushes = 0;
    if let Some(&param) = param_of_block.get(&node) {
        stack.push(param);
        pushes += 1;
    }

    let insts: Vec<Inst> = data.layout().basicblock(bb).insts().iter().copied().collect();
    for inst in insts {
        if inst == entry_load {
            continue;
        }
        match data.inst_data(inst).kind() {
            InstKind::Load(load) if load.src() == global => {
                let rep = stack.last().copied().unwrap_or(stack[0]);
                utils::visit_and_replace(data, inst, rep);
                remove_list.push((inst, bb));
            }
            InstKind::Store(store) if store.dest() == global => {
                stack.push(store.src());
                pushes += 1;
                remove_list.push((inst, bb));
            }
            InstKind::Return(..) => {
                if let Some(&last) = stack.last() {
                    write_backs.push((last, bb));
                }
            }
            InstKind::Jump(jump) => {
                let target = jump.target();
                let mut args = jump.args().to_vec();
                let has_param = param_of_block.contains_key(&bb_id.get_id(&target));
                if has_param {
                    args.push(stack.last().copied().unwrap_or(stack[0]));
                    data.replace_inst_with(inst).jump(target, args);
                }
            }
            InstKind::Branch(branch) => {
                let cond = branch.cond();
                let t_target = branch.t_target();
                let f_target = branch.f_target();
                let mut t_args = branch.t_args().to_vec();
                let mut f_args = branch.f_args().to_vec();
                let t_has = param_of_block.contains_key(&bb_id.get_id(&t_target));
                let f_has = param_of_block.contains_key(&bb_id.get_id(&f_target));
                if t_has {
                    t_args.push(stack.last().copied().unwrap_or(stack[0]));
                }
                if f_has {
                    f_args.push(stack.last().copied().unwrap_or(stack[0]));
                }
                if t_has || f_has {
                    data.replace_inst_with(inst)
                        .branch(cond, t_target, t_args, f_target, f_args);
                }
            }
            _ => {}
        }
    }

    if let Some(children) = tree.get(node) {
        for &child in children {
            thread(
                child,
                tree,
                bb_id,
                data,
                global,
                entry_load,
                param_of_block,
                stack,
                remove_list,
                write_backs,
            );
        }
    }

    for _ in 0..pushes {
        stack.pop();
    }
}

