use rustc_hash::{FxBuildHasher, FxHashMap};

use crate::opt::prelude::*;

pub struct CallGraph {
    call_edges: CallEdgeTable,
    call_site_indices: FxHashMap<Function, Vec<usize>>,
    callee_indices: FxHashMap<Function, Vec<usize>>,
}

pub struct CallEdge {
    callsite: Node,
    callee: Function,
}

#[derive(Default)]
struct CallEdgeTable {
    callsites: Vec<Node>,
    callees: Vec<Function>,
}

impl CallEdgeTable {
    fn len(&self) -> usize {
        self.callsites.len()
    }

    fn push(&mut self, call_site: Node, callee: Function) {
        self.callsites.push(call_site);
        self.callees.push(callee);
    }

    fn get(&self, i: usize) -> CallEdge {
        CallEdge {
            callsite: self.callsites[i],
            callee: self.callees[i],
        }
    }
}

impl CallGraph {
    /// Return all the callees of a function
    /// a.k.a all the functions that would be called inside the given function
    pub fn callees_in(&self, func: Function) -> impl Iterator<Item = Function> + '_ {
        self.call_site_indices
            .get(&func)
            .into_iter()
            .flatten()
            .map(|&idx| self.call_edges.get(idx).callee)
    }

    /// Return all the callsites of a function
    /// a.k.a all the instruction of `InstKind::Call` inside the given function
    pub fn callsites_in(&self, func: Function) -> impl Iterator<Item = Node> + '_ {
        self.call_site_indices
            .get(&func)
            .into_iter()
            .flatten()
            .map(|&idx| self.call_edges.get(idx).callsite)
    }

    /// Return all the callsites that call the given function.
    /// a.k.a all the call instruction which callee is given function.
    pub fn incoming_callsites_of(&self, func: Function) -> impl Iterator<Item = Node> + '_ {
        self.callee_indices
            .get(&func)
            .into_iter()
            .flatten()
            .map(|&idx| self.call_edges.get(idx).callsite)
    }

    pub fn in_degree_of(&self, func: Function) -> usize {
        self.callee_indices.get(&func).map_or(0, Vec::len)
    }

    pub fn out_degree_of(&self, func: Function) -> usize {
        self.call_site_indices.get(&func).map_or(0, Vec::len)
    }

    pub fn out_degrees_of_all(&self) -> impl Iterator<Item = (Function, usize)> {
        self.call_site_indices
            .iter()
            .map(|(&func, indices)| (func, indices.len()))
    }

    pub fn callees(&self) -> impl Iterator<Item = Function> {
        self.callee_indices.keys().copied()
    }

    pub fn in_degrees_of_all(&self) -> impl Iterator<Item = (Function, usize)> {
        self.callee_indices
            .iter()
            .map(|(&func, indices)| (func, indices.len()))
    }

    pub fn reaches(&self, from: Function, target: Function) -> bool {
        let mut queue = VecDeque::from([from]);
        let mut visited = HashSet::from_iter([from]);
        while let Some(function) = queue.pop_front() {
            for callee in self.callees_in(function) {
                if callee == target {
                    return true;
                }
                if visited.insert(callee) {
                    queue.push_back(callee);
                }
            }
        }
        false
    }

    pub fn new(p: &Program) -> CallGraph {
        let main_function = p.get_main_function();
        let mut func_queue = VecDeque::with_capacity(16);
        let mut func_visited =
        // idk why FxHashSet do not provide FxHashSet::with_capacity() so:
            HashSet::with_capacity_and_hasher(p.function_layout().len(), FxBuildHasher);
        func_queue.push_back(main_function);
        func_visited.insert(main_function);
        let mut call_edges = CallEdgeTable::default();
        let mut call_site_indices: FxHashMap<Function, Vec<usize>> = FxHashMap::default();
        let mut callee_indices: FxHashMap<Function, Vec<usize>> = FxHashMap::default();

        let mut push_edge = |call_site: Node, callee| {
            call_site_indices
                .entry(call_site.func)
                .or_default()
                .push(call_edges.len());
            callee_indices
                .entry(callee)
                .or_default()
                .push(call_edges.len());
            call_edges.push(call_site, callee);
        };

        while !func_queue.is_empty() {
            let func = func_queue.pop_front().unwrap();
            let data = p.func_data(func);
            for bb_layout in data.layout().basicblocks() {
                for &inst in bb_layout.insts() {
                    let callee = match data.inst_data(inst).kind() {
                        InstKind::Call(call) => call.callee(),
                        InstKind::TailCall(tailcall) => tailcall.callee(),
                        _ => {
                            continue;
                        }
                    };
                    push_edge(Node::new(func, inst), callee);
                    if func_visited.insert(callee) {
                        func_queue.push_back(callee);
                    }
                }
            }
        }

        CallGraph {
            call_edges,
            call_site_indices,
            callee_indices,
        }
    }
}
