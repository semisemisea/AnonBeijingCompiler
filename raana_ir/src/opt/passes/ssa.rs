use std::collections::{HashMap, HashSet, VecDeque};

use crate::opt::utils::{self, BId, CFGGraph, DomTree, IDAllocator, IDomMap, VId};

const FUNC_ARG_OPT_ENABLE: bool = false;

use crate::{
    ir::{BasicBlock, BlockArgRef, Function, FunctionData, Inst, InstKind},
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
    fn run_on(&mut self, func: Function, data: &mut FunctionData) {
        // function declaration. skip.
        if data.layout().entry_bb().is_none() {
            return;
        }

        eprintln!("----------------------------------");
        eprintln!("function: {func:?}");
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
        assert!(bb_id.get_id(&data.layout().entry_bb().unwrap()) == 0);

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
        let dom_frontier = dominance_analysis(&bb_id.bb(), &prece, &idom_map);
        eprintln!("dominance_frontier: {dom_frontier:?}");

        // find out where are varaibles defined.
        let mut val_id = IDAllocator::new(1);
        let val_usage = variable_analysis(&mut val_id, &mut bb_id.bb(), data);
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

            // eprintln!("default work_queue {work_queue:?}");

            while !work_queue.is_empty() {
                let front = work_queue.pop_front().unwrap();
                if worked[front].contains(&vid) {
                    continue;
                }
                worked[front].insert(vid);
                // eprintln!("visited {front}");
                let bb = bb_id.search_id(front);
                let index = data.dfg().bb(bb).params().len();

                let var_ty = utils::alloc_ty(val_id.search_id(vid as _), data).clone();

                // TODO: Do it in another way.
                let arg = BlockArgRef::new_data(index, var_ty);
                insert_table[front].push((vid, index));
                let new_block_arg_ref = data.dfg_mut().new_value().insert_value(arg);
                data.dfg_mut()
                    .set_value_name(new_block_arg_ref, Some(format!("%vid_{}", vid)));

                data.dfg_mut()
                    .bb_mut(bb)
                    .params_mut()
                    .push(new_block_arg_ref);

                if let Some(sub_frontiers) = dom_frontier.get(&front) {
                    for &sub_front in sub_frontiers.iter() {
                        if !worked[sub_front].contains(&vid) {
                            // if !worked.contains(sub_front) {
                            //     worked.insert(*sub_front);
                            work_queue.push_back(sub_front);
                        } else {
                            eprintln!("detected loop and correctly stop.");
                            // eprintln!("sub front {sub_front}");
                            // eprintln!("worked {worked:?}");
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
            &bb_id.bb(),
            data,
            &insert_table,
            &mut remove_list,
        );

        // remove_list.into_iter().for_each(|(val, bb)| {
        //     dfs_remove(val, data, bb);
        // });
        remove_list.into_iter().rev().for_each(|(val, bb)| {
            let vd = data.dfg().value(val);
            // eprintln!("delete check {:?} {:?}, {:?}", val, vd.kind(), vd.used_by());
            let used_by = vd.used_by().iter().copied().collect::<Vec<_>>();
            for val in used_by.into_iter().rev() {
                data.dfg_mut().remove_value(val);
            }
            data.layout_mut().bb_mut(bb).insts_mut().remove(&val);
            data.dfg_mut().remove_value(val);
        });

        eprintln!();
        eprintln!("----------------------------------");
        eprintln!();
    }
}

// fn dfs_remove(val: Inst, data: &mut FunctionData, bb: BasicBlock) {
//     let mut remove_list = Vec::new();
//     _dfs_remove(val, data, &mut remove_list);
//     for val in remove_list.into_iter().rev() {
//         data.layout_mut().bb_mut(bb).insts_mut().remove(&val);
//         data.dfg_mut().remove_value(val);
//     }
// }
//
// fn _dfs_remove(val: Inst, data: &FunctionData, remove_list: &mut Vec<Inst>) {
//     let vd = data.dfg().value(val);
//     remove_list.push(val);
//     for &child in vd.used_by().iter() {
//         _dfs_remove(child, data, remove_list);
//     }
// }

#[allow(clippy::too_many_arguments)]
fn dfs(
    node: BId,
    tree: &DomTree,
    st: &mut ValStack,
    val_id: &IDAllocator<Inst, VId>,
    bb_id: &IDAllocator<BasicBlock, BId>,
    data: &mut FunctionData,
    insert_table: &InsertTable,
    remove_list: &mut Vec<(Inst, BasicBlock)>,
) {
    let mut history = Vec::new();
    // Step 1:   Update `st` if block arguments update the value.
    let bb = bb_id.search_id(node);
    let bb_data = data.dfg().bb(bb);
    for &(vid, idx) in insert_table[node].iter() {
        st[vid].push(bb_data.params()[idx]);
        history.push(vid);
    }

    // Step 2:   Traverse the instruction list and find `alloc`, `store` and `load`.
    let bb_data = data.layout().bbs().node(&bb_id.search_id(node)).unwrap();
    let values = bb_data.insts().iter().map(|(&x, _)| x).collect::<Vec<_>>();
    for val in values {
        let val_data = data.dfg().value(val);
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
                    st[dest_id].push(store.value());
                    history.push(dest_id);

                    remove_list.push((val, bb));
                }
            }
            InstKind::Load(load) => {
                if let Some(&load_id) = val_id.get_id_safe(&load.src()) {
                    let rep_with = *st[load_id].last().unwrap();
                    utils::visit_and_replace(data, val, rep_with);
                    // let list = val_data.used_by().iter().copied().collect::<Vec<_>>();
                    // for used_by in list {
                    //     utils::visit_and_replace(data, used_by, val, rep_with);
                    // }
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
                            let v = data.dfg().bb(target).params()[i];
                            let ty = data.dfg().value(v).ty().clone();
                            data.dfg_mut().new_value().undef(ty)
                        }
                    };
                    args.push(item);
                }
                data.dfg_mut()
                    .replace_value_with(val)
                    .jump_with_args(target, args);
            }
            InstKind::Branch(branch) => {
                let cond = branch.cond();
                let true_bb = branch.true_bb();
                let true_bb_id = bb_id.get_id(&true_bb);
                let false_bb = branch.false_bb();
                let false_bb_id = bb_id.get_id(&false_bb);
                let mut false_args = branch.false_args().to_vec();
                let mut true_args = branch.true_args().to_vec();
                for (i, &(vid, _)) in (false_args.len()..).zip(insert_table[false_bb_id].iter()) {
                    let item = match st[vid].last() {
                        Some(&val) => val,
                        None => {
                            let v = data.dfg().bb(false_bb).params()[i];
                            let ty = data.dfg().value(v).ty().clone();
                            data.dfg_mut().new_value().undef(ty)
                        }
                    };
                    false_args.push(item);
                }
                for (i, &(vid, _)) in (true_args.len()..).zip(insert_table[true_bb_id].iter()) {
                    let item = match st[vid].last() {
                        Some(&val) => val,
                        None => {
                            let v = data.dfg().bb(true_bb).params()[i];
                            let ty = data.dfg().value(v).ty().clone();
                            data.dfg_mut().new_value().undef(ty)
                        }
                    };
                    true_args.push(item);
                }
                data.dfg_mut()
                    .replace_value_with(val)
                    .branch_with_args(cond, true_bb, false_bb, true_args, false_args);
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
    eprintln!("skip: {skip_func_para}");
    let mut val_usage = ValUsage::new();

    // use iterator to get rid of nested for-loop
    // you don't have to care what does the iterator chain do.
    // only to know it return these things in tuple:
    //
    //  value handle     kind        which basic block it belongs to.
    for (val, val_kind, bb) in data
        .layout()
        .bbs()
        .iter()
        .flat_map(|(&bb, node)| node.insts().iter().zip(std::iter::repeat(bb)))
        .map(|((&val, _), bb)| (val, data.dfg().value(val).kind(), bb))
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
