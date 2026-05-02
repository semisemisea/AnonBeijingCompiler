use std::collections::{HashMap, HashSet, VecDeque};

use crate::ir::arena::Arena;
use crate::ir::{Program, builder_trait::*};
use crate::opt::pass::ArenaContext;
use crate::opt::utils::{self, BId, CFGGraph, DomTree, IDAllocator, IDomMap, VId};

const FUNC_ARG_OPT_ENABLE: bool = false;

use crate::{
    ir::{BasicBlock, Function, FunctionData, Inst, InstKind},
    opt::pass::Pass,
};

pub struct SSATransform;

// you can replace hashset with more efficient bitset;
type Set = HashSet<BId>;

type GPath = Vec<BId>;

// not all basic block has it's frontier so we can use HashMap instead of Vec
type Frontier = HashMap<BId, HashSet<BId>>;

type ValUsage = Vec<Vec<VId>>;

// variable(vid) is insert as basic block(bbid) at index(usize)
type Index = usize;
type InsertTable = Vec<Vec<(VId, Index)>>;

// Recording each variable version while doing SSA elimination.
type ValStack = Vec<Vec<Inst>>;

impl Pass for SSATransform {
    fn run_on(&self, data: &mut ArenaContext<'_>) {
        // function declaration. skip.
        if data.layout().entry_bb().is_none() {
            return;
        }

        eprintln!("----------------------------------");
        eprintln!("function: {:?}", data.curr_func.unwrap());
        eprintln!("name: {}", data.name());

        // Discretization. Assign each unique basic block with natural number 0..n
        let mut bb_id = IDAllocator::new(1);

        eprintln!("showing");
        eprintln!("finished");
        // get graph and reverse graph
        let (graph, prece) = utils::build_cfg_both(data, &mut bb_id);
        eprintln!("graph: {graph:?}");
        eprintln!("prece: {prece:?}");

        // entry_bb must get 0 for id
        assert!(bb_id.get_id(&data.layout().entry_bb().unwrap().bb()) == 0);

        let rpo_path = utils::rpo_path(&graph);
        // start from entry_bb so first element of RPO is zero
        assert!(rpo_path[0] == 0);
        eprintln!("rpo_path: {rpo_path:?}");

        // get immediate dominator of each block
        // dominance is a partial order.
        // immediate dominance means partial order coverage
        let idom_map = utils::idom(&prece, &rpo_path);
        eprintln!("idom_map: {idom_map:?}");

        // for dominance, its hasse diagram is a tree
        let donimnace_tree = utils::build_dominance_tree(&idom_map, rpo_path.len());
        eprintln!("dominance_tree: {donimnace_tree:?}");

        // then we can do frontier analysis
        let dom_frontier = dominance_analysis(&bb_id, &prece, &idom_map);
        eprintln!("dominance_frontier: {dom_frontier:?}");

        // find out where are varaibles defined.
        let mut val_id = IDAllocator::new(1);
        let val_usage = variable_analysis(&mut val_id, &mut bb_id, data);
        eprintln!("val_usage: {val_usage:?}");

        // variable(vid) is insert as basic block(bbid) at index(usize)
        let mut insert_table = vec![vec![]; bb_id.cnt()];

        let mut worked = vec![HashSet::new(); bb_id.cnt()];

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

        remove_list.into_iter().rev().for_each(|(inst, bb)| {
            let vd = data.inst_data(inst);
            let used_by = vd.used_by().iter().copied().collect::<Vec<_>>();
            for val in used_by.into_iter().rev() {
                data.remove_inst(val);
            }
            data.layout_mut().remove_inst(bb, inst);
            data.remove_inst(inst);
        });

        eprintln!();
        eprintln!("----------------------------------");
        eprintln!();
    }
}

#[allow(clippy::too_many_arguments)]
fn dfs(
    node: BId,
    tree: &DomTree,
    st: &mut ValStack,
    val_id: &IDAllocator<Inst, VId>,
    bb_id: &IDAllocator<BasicBlock, BId>,
    data: &mut ArenaContext<'_>,
    insert_table: &InsertTable,
    remove_list: &mut Vec<(Inst, BasicBlock)>,
) {
    let mut history = Vec::new();
    // Step 1:   Update `st` if block arguments update the value.
    let bb = bb_id.search_id(node);
    let bb_data = data.bb_data(bb);
    for &(vid, idx) in insert_table[node].iter() {
        st[vid].push(bb_data.params()[idx]);
        history.push(vid);
    }

    // Step 2:   Traverse the instruction list and find `alloc`, `store` and `load`.
    let bb_data = data.layout().basicblock(bb_id.search_id(node));
    let values = bb_data.insts().iter().copied().collect::<Vec<_>>();
    for val in values {
        let val_data = data.inst_data(val);
        // Step 2.3: Delete `load` and replace every use of `load` with value of variable.
        // Step 2.4: For `jump` and `branch`, update its arguments.
        match val_data.kind() {
            // Step 2.1: Straight delete `alloc`.
            // `alloc` can only be deleted when all `load` and `store` is deleted.
            InstKind::Alloc => {
                if val_id.get_id_safe(&val).is_some() {
                    remove_list.push((val, bb));
                }
            }
            // Step 2.2: Update the value in stack with corresponding variable if we met `store`.
            InstKind::Store(store) => {
                if let Some(&dest_id) = val_id.get_id_safe(&store.dest()) {
                    st[dest_id].push(store.src());
                    history.push(dest_id);

                    remove_list.push((val, bb));
                }
            }
            InstKind::Load(load) => {
                if let Some(&load_id) = val_id.get_id_safe(&load.src()) {
                    let rep_with = *st[load_id].last().unwrap();
                    utils::visit_and_replace(data, val, rep_with);
                    remove_list.push((val, bb));
                }
            }
            InstKind::Jump(jump) => {
                let target = jump.target();
                let target_id = bb_id.get_id(&target);
                let mut args = jump.args().to_vec();
                for (i, &(vid, _)) in (args.len()..).zip(insert_table[target_id].iter()) {
                    let item = match st[vid].last() {
                        Some(&val) => val,
                        None => {
                            let v = data.bb_data(target).params()[i];
                            let ty = data.inst_data(v).ty().clone();
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
                for (i, &(vid, _)) in (f_args.len()..).zip(insert_table[f_target_id].iter()) {
                    let item = match st[vid].last() {
                        Some(&val) => val,
                        None => {
                            let v = data.bb_data(f_target).params()[i];
                            let ty = data.inst_data(v).ty().clone();
                            data.new_local_inst().undef(ty)
                        }
                    };
                    f_args.push(item);
                }
                for (i, &(vid, _)) in (t_args.len()..).zip(insert_table[t_target_id].iter()) {
                    let item = match st[vid].last() {
                        Some(&val) => val,
                        None => {
                            let v = data.bb_data(t_target).params()[i];
                            let ty = data.inst_data(v).ty().clone();
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

    // Step 3: Recursively call the function.
    tree[node].iter().for_each(|&child| {
        dfs(
            child,
            tree,
            st,
            val_id,
            bb_id,
            data,
            insert_table,
            remove_list,
        )
    });

    for id in history {
        st[id].pop();
    }
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
                    if ty.is_i32() {
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
    let mut dominance_frontier = Frontier::new();

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
