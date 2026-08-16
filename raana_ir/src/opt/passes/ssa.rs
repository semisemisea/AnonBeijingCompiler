use crate::opt::prelude::*;

pub struct SSATransform;

const FUNC_ARG_OPT_ENABLE: bool = false;

// not all basic block has it's frontier so we can use HashMap instead of Vec
type Frontier = HashMap<BId, HashSet<BId>>;

type ValUsage = Vec<Vec<VId>>;

// variable(vid) is insert as basic block(bbid) at index(usize)
type Index = usize;
type InsertTable = Vec<Vec<(VId, Index)>>;

// Recording each variable version while doing SSA elimination.
type ValStack = Vec<Vec<Inst>>;

impl Pass for SSATransform {
    fn run(&mut self, program: &mut crate::ir::Program) -> bool {
        let func_layout = program.function_layout().to_vec();
        let mut arena_context = ArenaContextMut {
            program,
            curr_func: None,
        };
        let mut changed = false;
        for func in func_layout {
            arena_context.curr_func = Some(func);
            changed |= self.run_on(&mut arena_context);
        }
        let mut dce = super::dce::DeadCodeElimination;
        changed |= dce.run(program);
        changed
    }

    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        // function declaration. skip.
        if data.layout().entry_bb().is_none() {
            return false;
        }

        let mut ubb = super::dce::UnreachableBasicBlock;
        let mut changed = ubb.run_on(data);

        debug!("----------------------------------");
        debug!("function: {:?}", data.curr_func.unwrap());
        debug!("name: {}", data.name());

        // Discretization. Assign each unique basic block with natural number 0..n
        let mut bb_id = IDAllocator::new(1);

        debug!("showing");
        // get graph and reverse graph
        let (graph, prece) = cfg::build_cfg_both(data, &mut bb_id);
        debug!("graph: {graph:?}");
        debug!("prece: {prece:?}");

        // entry_bb must get 0 for id
        assert!(bb_id.get_id(&data.layout().entry_bb().unwrap().bb()) == 0);

        let rpo_path = cfg::rpo_path(&graph);
        // start from entry_bb so first element of RPO is zero
        assert!(rpo_path[0] == 0);
        debug!("rpo_path: {rpo_path:?}");

        // get immediate dominator of each block
        // dominance is a partial order.
        // immediate dominance means partial order coverage
        let idom_map = dom_tree::idom(&prece, &rpo_path);
        debug!("idom_map: {idom_map:?}");

        // for dominance, its hasse diagram is a tree
        let donimnace_tree = dom_tree::build_dominance_tree(&idom_map, rpo_path.len());
        debug!("dominance_tree: {donimnace_tree:?}");

        // then we can do frontier analysis
        let dom_frontier = dominance_analysis(&bb_id, &prece, &idom_map);
        debug!("dominance_frontier: {dom_frontier:?}");

        // find out where are varaibles defined.
        let mut val_id = IDAllocator::new(1);
        let val_usage = variable_analysis(&mut val_id, &mut bb_id, data);
        debug!("val_usage: {val_usage:?}");

        // variable(vid) is insert as basic block(bbid) at index(usize)
        let mut insert_table = vec![vec![]; bb_id.cnt()];

        let mut worked = vec![HashSet::default(); bb_id.cnt()];

        for (vid, frontiers) in val_usage.iter().enumerate().flat_map(|(vid, def_bbs)| {
            def_bbs
                .iter()
                .filter_map(|def_bb| dom_frontier.get(def_bb))
                .map(move |frontier| (vid, frontier))
        }) {
            // let mut worked = frontiers.clone();
            let mut work_queue = VecDeque::with_capacity(frontiers.len());
            for &front in frontiers.iter() {
                work_queue.push_back(front);
            }

            while !work_queue.is_empty() {
                let front = work_queue.pop_front().unwrap();
                if worked[front].contains(&vid) {
                    continue;
                }
                worked[front].insert(vid);
                let bb = bb_id.search_id(front);
                let index = data.bb_data(bb).params().len();

                let var_ty = utils::alloc_ty(val_id.search_id(vid as _), data).clone();

                let p = data.new_basic_block().add_param(bb, var_ty);
                changed = true;
                insert_table[front].push((vid, index));
                data.inst_data_mut(p).set_name(format!("vid_{}", vid));

                if let Some(sub_frontiers) = dom_frontier.get(&front) {
                    for &sub_front in sub_frontiers.iter() {
                        if !worked[sub_front].contains(&vid) {
                            work_queue.push_back(sub_front);
                        }
                    }
                }
            }
        }

        let mut val_stack = vec![vec![]; val_id.cnt()];
        let mut remove_list = Vec::new();

        dfs(
            0,
            &donimnace_tree,
            &mut val_stack,
            &val_id,
            &bb_id,
            data,
            &insert_table,
            &mut remove_list,
        );

        changed |= !remove_list.is_empty();
        remove_list.into_iter().rev().for_each(|(inst, bb)| {
            data.remove_layout_inst(bb, inst);
        });

        debug!("");
        debug!("----------------------------------");
        debug!("");
        changed
    }
}

#[allow(clippy::too_many_arguments)]
fn dfs(
    entry: BId,
    tree: &DomTree,
    st: &mut ValStack,
    val_id: &IDAllocator<Inst, VId>,
    bb_id: &IDAllocator<BasicBlock, BId>,
    data: &mut ArenaContextMut<'_>,
    insert_table: &InsertTable,
    remove_list: &mut Vec<(Inst, BasicBlock)>,
) {
    enum Visit {
        Enter(BId),
        Exit(Vec<VId>),
    }

    let mut visits = vec![Visit::Enter(entry)];
    while let Some(visit) = visits.pop() {
        let node = match visit {
            Visit::Enter(node) => node,
            Visit::Exit(history) => {
                for id in history {
                    st[id].pop();
                }
                continue;
            }
        };

        let mut history = Vec::new();
        // Step 1: Update `st` if block arguments update the value.
        let bb = bb_id.search_id(node);
        let bb_data = data.bb_data(bb);
        for &(vid, idx) in &insert_table[node] {
            st[vid].push(bb_data.params()[idx]);
            history.push(vid);
        }

        // Step 2: Traverse the instruction list and find `alloc`, `store` and `load`.
        let values = data
            .layout()
            .basicblock(bb)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for val in values {
            let val_data = data.inst_data(val);
            let ty = val_data.ty().clone();
            match val_data.kind() {
                InstKind::Alloc => {
                    if val_id.get_id_safe(&val).is_some() {
                        remove_list.push((val, bb));
                    }
                }
                InstKind::Store(store) => {
                    if let Some(&dest_id) = val_id.get_id_safe(&store.dest()) {
                        st[dest_id].push(store.src());
                        history.push(dest_id);
                        remove_list.push((val, bb));
                    }
                }
                InstKind::Load(load) => {
                    if let Some(&load_id) = val_id.get_id_safe(&load.src()) {
                        let rep_with = st[load_id]
                            .last()
                            .copied()
                            .unwrap_or_else(|| data.new_local_inst().undef(ty));
                        utils::visit_and_replace(data, val, rep_with);
                        remove_list.push((val, bb));
                    }
                }
                InstKind::Jump(jump) => {
                    let target = jump.target();
                    let target_id = bb_id.get_id(&target);
                    let mut args = jump.args().to_vec();
                    for (i, &(vid, _)) in (args.len()..).zip(&insert_table[target_id]) {
                        let item = match st[vid].last() {
                            Some(&val) => val,
                            None => {
                                let value = data.bb_data(target).params()[i];
                                let ty = data.inst_data(value).ty().clone();
                                data.new_local_inst().undef(ty)
                            }
                        };
                        args.push(item);
                    }
                    data.replace_inst_with(val).jump(target, args);
                }
                InstKind::Branch(branch) => {
                    let cond = branch.cond();
                    let t_target = branch.t_target();
                    let t_target_id = bb_id.get_id(&t_target);
                    let f_target = branch.f_target();
                    let f_target_id = bb_id.get_id(&f_target);
                    let mut f_args = branch.f_args().to_vec();
                    let mut t_args = branch.t_args().to_vec();
                    for (i, &(vid, _)) in (f_args.len()..).zip(&insert_table[f_target_id]) {
                        let item = match st[vid].last() {
                            Some(&val) => val,
                            None => {
                                let value = data.bb_data(f_target).params()[i];
                                let ty = data.inst_data(value).ty().clone();
                                data.new_local_inst().undef(ty)
                            }
                        };
                        f_args.push(item);
                    }
                    for (i, &(vid, _)) in (t_args.len()..).zip(&insert_table[t_target_id]) {
                        let item = match st[vid].last() {
                            Some(&val) => val,
                            None => {
                                let value = data.bb_data(t_target).params()[i];
                                let ty = data.inst_data(value).ty().clone();
                                data.new_local_inst().undef(ty)
                            }
                        };
                        t_args.push(item);
                    }
                    data.replace_inst_with(val)
                        .branch(cond, t_target, t_args, f_target, f_args);
                }
                _ => {}
            }
        }

        visits.push(Visit::Exit(history));
        for &child in tree[node].iter().rev() {
            visits.push(Visit::Enter(child));
        }
    }
}

/// An alloca is promotable only if it holds a single-machine-word value (scalar
/// or pointer) and its address never escapes: every use is either a `Store` that
/// writes *to* the slot or a `Load` that reads *from* it. Using the address in
/// any other way (passing it to a call, storing it into memory, address
/// arithmetic, returning it) forces the slot to stay in memory.
fn alloca_does_not_escape(val: Inst, data: &FunctionData) -> bool {
    data.inst_data(val)
        .used_by()
        .iter()
        .copied()
        .all(|user| match data.inst_data(user).kind() {
            InstKind::Store(store) => store.dest() == val,
            InstKind::Load(_) => true,
            _ => false,
        })
}

pub fn variable_analysis(
    val_id: &mut IDAllocator<Inst, VId>,
    bb_id: &mut IDAllocator<BasicBlock, BId>,
    data: &FunctionData,
) -> ValUsage {
    let mut skip_func_para = if FUNC_ARG_OPT_ENABLE {
        data.params().len()
    } else {
        0
    };
    let mut val_usage = ValUsage::new();

    // use iterator to get rid of nested for-loop
    // you don't have to care what does the iterator chain do.
    // only to know it return these things in tuple:
    //
    //  value handle     kind        which basic block it belongs to.
    for (val, val_kind, bb) in data
        .layout()
        .basicblocks()
        .iter()
        .flat_map(|layout| layout.insts().iter().zip(std::iter::repeat(layout.bb())))
        .map(|(&val, bb)| (val, data.inst_data(val).kind(), bb))
    {
        match val_kind {
            InstKind::Alloc => {
                if skip_func_para > 0 {
                    skip_func_para -= 1;
                } else {
                    let ty = utils::alloc_ty(val, data);
                    // Single-machine-word slot types (scalar or pointer) are
                    // promotable when the address does not escape.
                    if (ty.is_scalar() || ty.is_pointer()) && alloca_does_not_escape(val, data) {
                        val_id.check_or_alloc_id_same(val);
                        val_usage.push(Vec::new());
                    }
                }
            }
            InstKind::Store(store) => {
                if let Some(&vid) = val_id.get_id_safe(&store.dest()) {
                    let bbid = bb_id.get_id(&bb);
                    val_usage[vid].push(bbid);
                }
            }
            _ => {}
        }
    }

    val_usage
}

pub fn dominance_analysis(
    id_alloca: &IDAllocator<BasicBlock, BId>,
    prece: &CFGGraph,
    idom_map: &IDomMap,
) -> Frontier {
    let mut dominance_frontier = Frontier::default();

    // algorithm I looked up from wikipedia.
    for bb in 0..id_alloca.cnt() {
        if prece[&bb].len() >= 2 {
            for &pre in prece[&bb].iter() {
                let mut runner = pre;
                while runner != idom_map[bb] {
                    dominance_frontier.entry(runner).or_default().insert(bb);
                    runner = idom_map[runner];
                }
            }
        }
    }

    dominance_frontier
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(data: &mut ArenaContextMut<'_>) -> bool {
        SSATransform.run_on(data)
    }

    fn count_kind(data: &FunctionData, pred: impl Fn(&InstKind) -> bool) -> usize {
        data.layout()
            .basicblocks()
            .iter()
            .flat_map(|layout| layout.insts())
            .filter(|&&inst| pred(data.inst_data(inst).kind()))
            .count()
    }

    /// A pointer-typed alloca whose address is only used by `Store`/`Load` must
    /// be promoted: after the pass the slot is gone and the loaded pointer is
    /// replaced by the stored value directly.
    #[test]
    fn promotes_pointer_slot_alloca() {
        let mut program = Program::new();
        let ptr_ty = Type::get_pointer(Type::get_array(Type::get_i32(), 1024));
        let func =
            program.new_function(Type::get_unit(), "promote".to_owned(), vec![ptr_ty.clone()]);

        {
            let data = program.func_data_mut(func);
            let entry = data
                .new_basic_block()
                .basic_block("entry".to_owned(), vec![ptr_ty.clone()]);
            let mid = data.new_basic_block().basic_block("mid".to_owned(), vec![]);
            data.layout_mut().push_bb_back(entry);
            data.layout_mut().push_bb_back(mid);

            let slot = data.new_local_inst().alloc(ptr_ty.clone());
            let param = data.bb_data(entry).params()[0];
            data.set_params(vec![param]);
            let store1 = data.new_local_inst().store(param, slot);
            let jump = data.new_local_inst().jump(mid, vec![]);
            data.layout_mut().insert_inst(entry, slot);
            data.layout_mut().insert_inst(entry, store1);
            data.layout_mut().insert_inst(entry, jump);

            let loaded = data.new_local_inst().load(slot);
            let zero = data.new_local_inst().integer(0);
            let elem = data.new_local_inst().get_elem_ptr(loaded, vec![zero]);
            let store2 = data.new_local_inst().store(zero, elem);
            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(mid, loaded);
            data.layout_mut().insert_inst(mid, zero);
            data.layout_mut().insert_inst(mid, elem);
            data.layout_mut().insert_inst(mid, store2);
            data.layout_mut().insert_inst(mid, ret);
        }

        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(func),
        };
        assert!(run(&mut data));

        let data = data.curr_func_data();
        assert_eq!(
            count_kind(data, |k| matches!(k, InstKind::Alloc)),
            0,
            "slot should be promoted"
        );
        assert_eq!(
            count_kind(data, |k| matches!(k, InstKind::Store(..))),
            1,
            "only the final element store remains"
        );
        assert_eq!(
            count_kind(data, |k| matches!(k, InstKind::Load(..))),
            0,
            "slot load should be replaced"
        );

        // The GEP base must now be the function parameter, not a load of the slot.
        let gep = data
            .layout()
            .basicblocks()
            .iter()
            .flat_map(|layout| layout.insts())
            .find(|&&inst| matches!(data.inst_data(inst).kind(), InstKind::GetElemPtr(..)))
            .copied()
            .unwrap();
        let InstKind::GetElemPtr(gep_data) = data.inst_data(gep).kind() else {
            unreachable!()
        };
        let param = data.params()[0];
        assert_eq!(
            gep_data.base(),
            param,
            "GEP base should be the promoted param"
        );
    }
}

// TEMP DEBUG
