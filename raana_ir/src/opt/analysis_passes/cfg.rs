use rustc_hash::FxHashSet as HashSet;

use log::debug;

use crate::{
    ir::{BasicBlock, FunctionData, InstKind, arena::Arena},
    opt::utils::{IDAllocator, get_terminator_inst, type_alias::*},
};

pub fn rpo_path(g: &CFGGraph) -> GPath {
    #[derive(Clone, Copy)]
    enum Visit {
        Enter(BId),
        Exit(BId),
    }

    let mut path = Vec::new();
    let mut visited = Set::default();
    let mut stack = vec![Visit::Enter(0)];
    while let Some(visit) = stack.pop() {
        match visit {
            Visit::Enter(node) => {
                if !visited.insert(node) {
                    continue;
                }
                stack.push(Visit::Exit(node));
                for &successor in g[&node].iter().rev() {
                    if !visited.contains(&successor) {
                        stack.push(Visit::Enter(successor));
                    }
                }
            }
            Visit::Exit(node) => path.push(node),
        }
    }
    path.reverse();
    debug!("Graph/Path: {:?} {:?}", g, path);
    path
}

// TODO: We can do cache there.
// TODO: Do forward/backward seperation to allow further more optimization.
pub fn build_cfg_both(
    data: &FunctionData,
    bb_alloc: &mut IDAllocator<BasicBlock, BId>,
) -> (CFGGraph, CFGGraph) {
    #[derive(Clone, Copy)]
    enum Visit {
        Enter(BasicBlock),
        FalseArm(BId, BasicBlock),
    }

    // <a,b> in set E when a can directly jump to b
    let mut graph = CFGGraph::default();
    // reverse graph
    let mut prece = CFGGraph::default();
    prece.entry(0).or_default();
    let mut visited = HashSet::default();
    let mut stack = vec![Visit::Enter(data.layout().entry_bb().unwrap().bb())];
    while let Some(visit) = stack.pop() {
        match visit {
            Visit::Enter(node) => {
                if !visited.insert(node) {
                    continue;
                }
                let id = bb_alloc.check_or_alloc_id_same(node);
                let terminator = get_terminator_inst(data, node);
                match data.inst_data(terminator).kind() {
                    InstKind::Jump(jump) => {
                        let target = jump.target();
                        let target_id = bb_alloc.check_or_alloc_id_same(target);
                        graph.entry(id).or_default().push(target_id);
                        prece.entry(target_id).or_default().push(id);
                        stack.push(Visit::Enter(target));
                    }
                    InstKind::Branch(branch) => {
                        let true_target = branch.t_target();
                        let true_id = bb_alloc.check_or_alloc_id_same(true_target);
                        graph.entry(id).or_default().push(true_id);
                        prece.entry(true_id).or_default().push(id);
                        stack.push(Visit::FalseArm(id, branch.f_target()));
                        stack.push(Visit::Enter(true_target));
                    }
                    InstKind::Return(..) | InstKind::TailCall(..) => {
                        graph.entry(id).or_default();
                    }
                    _ => unreachable!(),
                }
            }
            Visit::FalseArm(source_id, target) => {
                let target_id = bb_alloc.check_or_alloc_id_same(target);
                graph.entry(source_id).or_default().push(target_id);
                prece.entry(target_id).or_default().push(source_id);
                stack.push(Visit::Enter(target));
            }
        }
    }
    (graph, prece)
}
