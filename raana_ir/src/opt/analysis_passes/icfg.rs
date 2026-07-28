//! ICFG implementation.
//! Better remove unused function before building the icfg.
use std::ops::Range;

use itertools::Itertools;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use crate::opt::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeType {
    Normal,
    Call,
    Return,
    CallToReturn,
}

pub type Indices = SmallVec<[usize; 2]>;

/// AoS version struct. Showing equivalent representation of a structure
#[derive(Debug, Clone, PartialEq, Eq, Copy, Hash)]
pub struct Edge {
    pub edge_type: EdgeType,
    pub src: Node,
    pub dst: Node,
}

#[derive(Debug, Default)]
pub struct EdgeTable {
    pub edge_types: Vec<EdgeType>,
    pub srcs: Vec<Inst>,
    pub dsts: Vec<Inst>,
}

impl EdgeTable {
    fn push(&mut self, edge_type: EdgeType, src: Inst, dst: Inst) {
        self.edge_types.push(edge_type);
        self.srcs.push(src);
        self.dsts.push(dst);
    }

    fn len(&self) -> usize {
        self.edge_types.len()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Node {
    pub func: Function,
    pub inst: Inst,
}

impl Node {
    pub fn new(func: Function, inst: Inst) -> Self {
        Self { func, inst }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallSite {
    pub call: Inst,
    pub caller: Function,
    pub callee: Function,
    /// The instruction right next to the call instruction.
    pub continuation: Inst,
}

#[derive(Debug, Default)]
pub struct CallSiteTable {
    calls: Vec<Inst>,
    callers: Vec<Function>,
    callees: Vec<Function>,
    continuations: Vec<Inst>,
}

impl CallSiteTable {
    fn push(&mut self, call: Inst, caller: Function, callee: Function, continuation: Inst) {
        self.calls.push(call);
        self.callers.push(caller);
        self.callees.push(callee);
        self.continuations.push(continuation);
    }

    fn len(&self) -> usize {
        self.calls.len()
    }
}

pub struct ICFG {
    /// Entry function for a program. Usually `main` function.
    entry_func: Function,

    /// SoA form of Vec<Edge>,
    edges: EdgeTable,

    /// SoA form of Vec<CallSite>,
    call_sites: CallSiteTable,

    /// Continuous storage of return instruction.
    return_sites: Vec<Inst>,

    /// Sparse index used to resolve functions on cross-function edges.
    cross_edge_callsite_map: FxHashMap<usize, usize>,

    /// Index call and continuation nodes to their callsites.
    callsite_by_call: FxHashMap<Node, usize>,
    callsite_by_continuation: FxHashMap<Node, usize>,

    /// Index on continuous part of call sites.
    call_sites_range: FxHashMap<Function, Range<usize>>,

    /// Index on continuous part of return sites.
    return_sites_range: FxHashMap<Function, Range<usize>>,

    /// Store indices based on (Function, Inst) unique ID.
    /// The indices is used to index on edges.
    /// The forward one store all the indices of edges that have src == Inst
    /// The backward one store all the indices of edges that have dst == Inst
    forward: FxHashMap<Node, Indices>,
    backward: FxHashMap<Node, Indices>,

    /// All function reachable from main at comptime static estimate, including lib function.
    reachable_functions: FxHashSet<Function>,
}

impl ICFG {
    /// Get all callsites in a given function.
    pub fn call_sites_of(&self, function: Function) -> impl Iterator<Item = CallSite> {
        self.call_sites_range[&function]
            .clone()
            .map(|index| CallSite {
                call: self.call_sites.calls[index],
                caller: self.call_sites.callers[index],
                callee: self.call_sites.callees[index],
                continuation: self.call_sites.continuations[index],
            })
    }

    /// Get all callees of a given function.
    pub fn callees_of(&self, function: Function) -> &[Function] {
        &self.call_sites.callees[self.call_sites_range[&function].clone()]
    }

    /// Get all return instructions in a given function.
    pub fn return_sites_of(&self, function: Function) -> &[Inst] {
        &self.return_sites[self.return_sites_range[&function].clone()]
    }

    /// Get all outgoing edges of a node.
    pub fn outgoing_edges_of(&self, node: Node) -> impl Iterator<Item = Edge> {
        self.forward
            .get(&node)
            .into_iter()
            .flatten()
            .map(move |&index| {
                let edge_type = self.edges.edge_types[index];
                let dst_func = match edge_type {
                    EdgeType::Normal | EdgeType::CallToReturn => node.func,
                    EdgeType::Call => {
                        self.call_site_by_index(self.cross_edge_callsite_map[&index])
                            .callee
                    }
                    EdgeType::Return => {
                        self.call_site_by_index(self.cross_edge_callsite_map[&index])
                            .caller
                    }
                };
                Edge {
                    edge_type,
                    src: node,
                    dst: Node::new(dst_func, self.edges.dsts[index]),
                }
            })
    }

    /// Get all incoming edges of a node.
    pub fn incoming_edges_of(&self, node: Node) -> impl Iterator<Item = Edge> {
        self.backward
            .get(&node)
            .into_iter()
            .flatten()
            .map(move |&index| {
                let edge_type = self.edges.edge_types[index];
                let src_func = match edge_type {
                    EdgeType::Normal | EdgeType::CallToReturn => node.func,
                    EdgeType::Call => {
                        self.call_site_by_index(self.cross_edge_callsite_map[&index])
                            .caller
                    }
                    EdgeType::Return => {
                        self.call_site_by_index(self.cross_edge_callsite_map[&index])
                            .callee
                    }
                };
                Edge {
                    edge_type,
                    src: Node::new(src_func, self.edges.srcs[index]),
                    dst: node,
                }
            })
    }

    /// Get the callsite represented by a call node.
    pub fn call_site_at(&self, call: Node) -> Option<CallSite> {
        self.callsite_by_call
            .get(&call)
            .map(|&index| self.call_site_by_index(index))
    }

    /// Get the callsite immediately before a continuation node.
    pub fn call_site_before(&self, continuation: Node) -> Option<CallSite> {
        self.callsite_by_continuation
            .get(&continuation)
            .map(|&index| self.call_site_by_index(index))
    }

    fn call_site_by_index(&self, index: usize) -> CallSite {
        CallSite {
            call: self.call_sites.calls[index],
            caller: self.call_sites.callers[index],
            callee: self.call_sites.callees[index],
            continuation: self.call_sites.continuations[index],
        }
    }

    pub fn entry_func(&self) -> Function {
        self.entry_func
    }

    pub fn reachable_functions(&self) -> &FxHashSet<Function> {
        &self.reachable_functions
    }

    pub fn new(program: &Program) -> ICFG {
        let mut call_sites = CallSiteTable::default();
        let mut return_sites = vec![];
        let mut call_sites_range = FxHashMap::default();
        let mut return_sites_range = FxHashMap::default();
        let mut edges = EdgeTable::default();
        let mut forward: FxHashMap<Node, Indices> = FxHashMap::default();
        let mut backward: FxHashMap<Node, Indices> = FxHashMap::default();
        let mut entries = FxHashMap::default();
        let mut cross_edge_callsite_map = FxHashMap::default();
        let mut callsite_by_call = FxHashMap::default();
        let mut callsite_by_continuation = FxHashMap::default();

        fn insert_edge(
            ty: EdgeType,
            src: Node,
            dst: Node,
            edges: &mut EdgeTable,
            forward: &mut HashMap<Node, SmallVec<[usize; 2]>, rustc_hash::FxBuildHasher>,
            backward: &mut HashMap<Node, SmallVec<[usize; 2]>, rustc_hash::FxBuildHasher>,
        ) -> usize {
            let edge_len = edges.len();
            edges.push(ty, src.inst, dst.inst);

            forward.entry(src).or_default().push(edge_len);
            backward.entry(dst).or_default().push(edge_len);
            edge_len
        }

        for &func in program.function_layout() {
            let call_sites_start = call_sites.len();
            let return_sites_start = return_sites.len();
            let func_data = program.func_data(func);
            if func_data.layout().entry_bb().is_none() {
                continue;
            }
            for bb_layout in func_data.layout().basicblocks() {
                let first_inst = *bb_layout.insts().get_first().unwrap();
                entries.entry(func).or_insert(first_inst);
                for (&inst, &next) in bb_layout.insts().iter().tuple_windows() {
                    let inst_data = func_data.inst_data(inst);
                    let edge_type = match inst_data.kind() {
                        InstKind::Call(call) => {
                            let callsite_index = call_sites.len();
                            call_sites.push(inst, func, call.callee(), next);
                            callsite_by_call.insert(Node::new(func, inst), callsite_index);
                            callsite_by_continuation.insert(Node::new(func, next), callsite_index);
                            EdgeType::CallToReturn
                        }
                        _ => EdgeType::Normal,
                    };
                    insert_edge(
                        edge_type,
                        Node::new(func, inst),
                        Node::new(func, next),
                        &mut edges,
                        &mut forward,
                        &mut backward,
                    );
                }
                let last_inst = *bb_layout.insts().get_last().unwrap();
                let inst_data = func_data.inst_data(last_inst);

                let mut add_edge = |target| {
                    let jump_to = *func_data
                        .layout()
                        .basicblock(target)
                        .insts()
                        .get_first()
                        .unwrap();
                    insert_edge(
                        EdgeType::Normal,
                        Node::new(func, last_inst),
                        Node::new(func, jump_to),
                        &mut edges,
                        &mut forward,
                        &mut backward,
                    );
                };
                match inst_data.kind() {
                    InstKind::Return(..) => {
                        return_sites.push(last_inst);
                    }
                    InstKind::TailCall(tc) => {
                        // A tail call is simultaneously a return site (the caller's
                        // frame is gone) and a call site (control transfers to the
                        // callee). Record it as a call site with a sentinel
                        // `continuation == call` so the cross-function pass can
                        // distinguish it from a regular call (which has a real
                        // successor instruction as continuation).
                        return_sites.push(last_inst);
                        let callsite_index = call_sites.len();
                        call_sites.push(last_inst, func, tc.callee(), last_inst);
                        callsite_by_call.insert(Node::new(func, last_inst), callsite_index);
                        // Deliberately NOT inserted into `callsite_by_continuation`:
                        // a regular `Call` immediately preceding this terminator
                        // would already map `last_inst` as its continuation, and
                        // overwriting that entry would break the regular call's
                        // return-edge resolution.
                    }
                    InstKind::Jump(jump) => {
                        add_edge(jump.target());
                    }
                    InstKind::Branch(branch) => {
                        for target in [branch.t_target(), branch.f_target()] {
                            add_edge(target);
                        }
                    }
                    _ => unreachable!(),
                }
            }
            call_sites_range.insert(func, call_sites_start..call_sites.len());
            return_sites_range.insert(func, return_sites_start..return_sites.len());
        }

        let main_function = program.get_main_function();

        let mut function_queue = VecDeque::new();
        let mut function_visited = FxHashSet::default();

        function_queue.push_back(main_function);
        function_visited.insert(main_function);

        while !function_queue.is_empty() {
            let func = function_queue.pop_front().unwrap();
            let call_indices = &call_sites_range[&func];
            for call_index in call_indices.clone() {
                let call = call_sites.calls[call_index];
                let callee = call_sites.callees[call_index];
                // newly added entry.
                if function_visited.insert(callee) && !program.func_data(callee).layout().is_decl()
                {
                    function_queue.push_back(callee);
                }
                let callee_data = program.func_data(callee);
                if callee_data.layout().is_decl() {
                    continue;
                }
                let return_sites_indices = &return_sites_range[&callee];
                let returnsites = &return_sites[return_sites_indices.clone()];
                let entry = entries[&callee];
                let call_edge = insert_edge(
                    EdgeType::Call,
                    Node::new(func, call),
                    Node::new(callee, entry),
                    &mut edges,
                    &mut forward,
                    &mut backward,
                );
                cross_edge_callsite_map.insert(call_edge, call_index);
                // For a regular call, the continuation is the destination of its
                // CallToReturn edge (the instruction right after the call). For a
                // tail call there is no such edge — the sentinel
                // `continuation == call` marks it, and the callee's return value
                // lands at the tail-call instruction itself (the relay node).
                let return_to = if call_sites.continuations[call_index] == call {
                    call
                } else {
                    let call_to_return = *forward[&Node::new(func, call)].first().unwrap();
                    edges.dsts[call_to_return]
                };
                for &return_site in returnsites {
                    let edge_len = edges.len();
                    insert_edge(
                        EdgeType::Return,
                        Node::new(callee, return_site),
                        Node::new(func, return_to),
                        &mut edges,
                        &mut forward,
                        &mut backward,
                    );
                    cross_edge_callsite_map.insert(edge_len, call_index);
                }
            }
        }

        ICFG {
            entry_func: main_function,
            edges,
            call_sites,
            return_sites,
            cross_edge_callsite_map,
            callsite_by_call,
            callsite_by_continuation,
            call_sites_range,
            return_sites_range,
            forward,
            backward,
            reachable_functions: function_visited,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        Type,
        builder::{BasicBlockBuilder, LocalInstBuilder},
    };

    fn append_returning_block(program: &mut Program, function: Function, value: i32) -> Inst {
        let data = program.func_data_mut(function);
        let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
        data.layout_mut().push_bb_back(entry);
        let value = data.new_local_inst().integer(value);
        let ret = data.new_local_inst().ret(Some(value));
        data.layout_mut().insert_inst(entry, ret);
        ret
    }

    #[test]
    fn resolves_internal_call_and_return_edges() {
        let mut program = Program::new();
        let callee = program.new_function(Type::get_i32(), "callee".into(), vec![]);
        let callee_return = append_returning_block(&mut program, callee, 7);
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);

        let (call, continuation) = {
            let data = program.func_data_mut(main);
            let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
            data.layout_mut().push_bb_back(entry);
            let call = data
                .new_local_inst()
                .call_with_type(callee, vec![], Type::get_i32());
            let continuation = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(entry, call);
            data.layout_mut().insert_inst(entry, continuation);
            (call, continuation)
        };

        let icfg = ICFG::new(&program);
        let call_node = Node::new(main, call);
        let continuation_node = Node::new(main, continuation);
        let callsite = icfg.call_site_at(call_node).unwrap();
        assert_eq!(callsite.call, call);
        assert_eq!(callsite.caller, main);
        assert_eq!(callsite.callee, callee);
        assert_eq!(callsite.continuation, continuation);
        assert_eq!(icfg.call_site_before(continuation_node), Some(callsite));

        let outgoing = icfg.outgoing_edges_of(call_node).collect::<Vec<_>>();
        assert_eq!(outgoing.len(), 2);
        let call_edge = outgoing
            .iter()
            .copied()
            .find(|edge| edge.edge_type == EdgeType::Call)
            .unwrap();
        assert_eq!(call_edge.src, call_node);
        assert_eq!(call_edge.dst, Node::new(callee, callee_return));
        assert!(
            icfg.incoming_edges_of(Node::new(callee, callee_return))
                .any(|edge| edge == call_edge)
        );

        let return_edge = icfg
            .outgoing_edges_of(Node::new(callee, callee_return))
            .next()
            .unwrap();
        assert_eq!(return_edge.edge_type, EdgeType::Return);
        assert_eq!(return_edge.src, Node::new(callee, callee_return));
        assert_eq!(return_edge.dst, continuation_node);
        assert_eq!(icfg.call_site_before(return_edge.dst), Some(callsite));

        let incoming = icfg
            .incoming_edges_of(continuation_node)
            .collect::<Vec<_>>();
        assert_eq!(incoming.len(), 2);
        assert!(incoming.contains(&return_edge));
    }

    #[test]
    fn external_call_is_reachable_without_cross_function_edges() {
        let mut program = Program::new();
        let external = program.new_function(Type::get_unit(), "putint".into(), vec![]);
        let main = program.new_function(Type::get_unit(), "main".into(), vec![]);

        let call = {
            let data = program.func_data_mut(main);
            let entry = data.new_basic_block().basic_block("entry".into(), vec![]);
            data.layout_mut().push_bb_back(entry);
            let call = data
                .new_local_inst()
                .call_with_type(external, vec![], Type::get_unit());
            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(entry, call);
            data.layout_mut().insert_inst(entry, ret);
            call
        };

        let icfg = ICFG::new(&program);
        assert!(icfg.reachable_functions().contains(&external));
        let edges = icfg
            .outgoing_edges_of(Node::new(main, call))
            .collect::<Vec<_>>();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].edge_type, EdgeType::CallToReturn);
        assert_eq!(edges[0].src, Node::new(main, call));
        assert_eq!(icfg.call_site_at(edges[0].src).unwrap().callee, external);
    }

    #[test]
    fn tail_call_creates_call_and_return_edges() {
        let mut program = Program::new();

        // g(x: i32) -> i32 — returns x
        let g = program.new_function(Type::get_i32(), "g".into(), vec![Type::get_i32()]);
        let (g_ret, g_entry) = {
            let data = program.func_data_mut(g);
            let entry = data.add_entry_block();
            let param = data.bb_data(entry).params()[0];
            let ret = data.new_local_inst().ret(Some(param));
            data.layout_mut().insert_inst(entry, ret);
            (
                ret,
                *data.layout().entry_bb().unwrap().insts().get_first().unwrap(),
            )
        };

        // f(x: i32) -> i32 — tail-calls g(x)
        let f = program.new_function(Type::get_i32(), "f".into(), vec![Type::get_i32()]);
        let tail_call = {
            let data = program.func_data_mut(f);
            let entry = data.add_entry_block();
            let param = data.bb_data(entry).params()[0];
            let tc = data.new_local_inst().tail_call(g, vec![param]);
            data.layout_mut().insert_inst(entry, tc);
            tc
        };

        // main() -> i32 — calls f(42)
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        let (call_f, main_cont) = {
            let data = program.func_data_mut(main);
            let entry = data.add_entry_block();
            let val = data.new_local_inst().integer(42);
            let call = data.new_local_inst().call_with_type(f, vec![val], Type::get_i32());
            let ret = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(entry, call);
            data.layout_mut().insert_inst(entry, ret);
            (call, ret)
        };

        let icfg = ICFG::new(&program);

        // The tail call is recorded as a call site.
        let cs = icfg.call_site_at(Node::new(f, tail_call)).unwrap();
        assert_eq!(cs.caller, f);
        assert_eq!(cs.callee, g);

        // Call edge: (f, tail_call) -> (g, g_entry)
        let f_out = icfg.outgoing_edges_of(Node::new(f, tail_call)).collect::<Vec<_>>();
        let call_edge = f_out
            .iter()
            .find(|e| e.edge_type == EdgeType::Call)
            .unwrap();
        assert_eq!(call_edge.dst, Node::new(g, g_entry));

        // Return edge (relay input): (g, g_ret) -> (f, tail_call)
        let g_out = icfg.outgoing_edges_of(Node::new(g, g_ret)).collect::<Vec<_>>();
        let return_edge = g_out
            .iter()
            .find(|e| e.edge_type == EdgeType::Return)
            .unwrap();
        assert_eq!(return_edge.dst, Node::new(f, tail_call));

        // The tail-call node is deliberately absent from
        // callsite_by_continuation (collision avoidance with a preceding
        // regular Call whose continuation would be this terminator).
        assert_eq!(icfg.call_site_before(Node::new(f, tail_call)), None);

        // The tail-call node is also a return site of f, so f's caller sees a
        // Return edge from it (relay output).
        let relay = f_out
            .iter()
            .find(|e| e.edge_type == EdgeType::Return && e.dst.func == main)
            .unwrap();
        assert_eq!(relay.dst, Node::new(main, main_cont));
    }
}
