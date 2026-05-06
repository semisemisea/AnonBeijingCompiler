use std::collections::HashSet;

use log::debug;

use crate::{
    ir::{BasicBlock, FunctionData, InstKind, arena::Arena},
    opt::utils::{IDAllocator, get_terminator_inst, type_alias::*},
};

pub fn rpo_path(g: &CFGGraph) -> GPath {
    let mut path = Vec::new();
    let mut visited = Set::new();
    fn dfs(node: usize, g: &CFGGraph, ans: &mut GPath, visited: &mut Set) {
        visited.insert(node);
        for &succ in g[&node].iter() {
            if !visited.contains(&succ) {
                dfs(succ, g, ans, visited);
            }
        }
        ans.push(node);
    }
    dfs(0, g, &mut path, &mut visited);
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
    fn dfs(
        node: BasicBlock,
        data: &FunctionData,
        bb_alloc: &mut BIDAlloc,
        graph: &mut CFGGraph,
        prece: &mut CFGGraph,
        visited: &mut HashSet<BasicBlock>,
    ) {
        if visited.contains(&node) {
            return;
        }
        visited.insert(node);
        let id = bb_alloc.check_or_alloc_id_same(node);
        let val = get_terminator_inst(data, node);
        match data.inst_data(val).kind() {
            InstKind::Jump(jump) => {
                let target_id = bb_alloc.check_or_alloc_id_same(jump.target());

                graph.entry(id).or_default().push(target_id);
                prece.entry(target_id).or_default().push(id);

                dfs(jump.target(), data, bb_alloc, graph, prece, visited);
            }
            InstKind::Branch(branch) => {
                let true_id = bb_alloc.check_or_alloc_id_same(branch.t_target());

                graph.entry(id).or_default().push(true_id);
                prece.entry(true_id).or_default().push(id);

                dfs(branch.t_target(), data, bb_alloc, graph, prece, visited);

                let false_id = bb_alloc.check_or_alloc_id_same(branch.f_target());

                graph.entry(id).or_default().push(false_id);
                prece.entry(false_id).or_default().push(id);

                dfs(branch.f_target(), data, bb_alloc, graph, prece, visited);
            }
            InstKind::Return(..) => {
                graph.entry(id).or_default();
            }
            _ => unreachable!(),
        }
    }
    // <a,b> in set E when a can directly jump to b
    let mut graph = CFGGraph::new();
    // reverse graph
    let mut prece = CFGGraph::new();
    prece.entry(0).or_default();
    let mut visited = HashSet::new();
    dfs(
        data.layout().entry_bb().unwrap().bb(),
        data,
        bb_alloc,
        &mut graph,
        &mut prece,
        &mut visited,
    );
    (graph, prece)
}
