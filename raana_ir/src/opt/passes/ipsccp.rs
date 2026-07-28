//! Implementation of *Interprocedural Sparse Condition Constant Propagation*
use rustc_hash::{FxHashMap, FxHashSet};

use crate::opt::{
    analysis_passes::icfg::{Edge, EdgeType, Node},
    prelude::*,
    utils::visit_and_replace,
};

pub struct IPSCCP;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Lattice {
    #[default]
    Top,
    Constant(i32),
    Bottom,
}

impl Lattice {
    fn merge(self, new: Lattice) -> Lattice {
        match (self, new) {
            (Lattice::Top, v) => v,
            (Lattice::Constant(lhs), rhs) => match rhs {
                Lattice::Top => self,
                Lattice::Constant(rhs) if lhs == rhs => self,
                _ => Lattice::Bottom,
            },
            (Lattice::Bottom, _) => Lattice::Bottom,
        }
    }

    fn update(&mut self, new: Lattice) -> bool {
        match (*self, new) {
            (Lattice::Top, _) if new != Lattice::Top => {
                *self = new;
                true
            }
            (Lattice::Constant(old), Lattice::Constant(new)) if old != new => {
                *self = Lattice::Bottom;
                true
            }
            (Lattice::Constant(..), Lattice::Bottom) => {
                *self = Lattice::Bottom;
                true
            }
            _ => false,
        }
    }
}

#[derive(Debug, Default)]
struct LatticeMap(FxHashMap<Node, Lattice>);

impl LatticeMap {
    fn new_var(&mut self, node: Node) {
        self.0.insert(node, Lattice::Bottom);
    }

    fn new_const(&mut self, node: Node, val: i32) {
        self.0.insert(node, Lattice::Constant(val));
    }

    fn get(&self, node: Node) -> Lattice {
        *self.0.get(&node).unwrap_or(&Lattice::Top)
    }

    // Return whether the original status is changed.
    fn insert_or_update(&mut self, node: Node, status: Lattice) -> bool {
        match self.0.entry(node) {
            std::collections::hash_map::Entry::Occupied(mut e) => e.get_mut().update(status),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(status);
                status != Lattice::Top
            }
        }
    }
}
type EdgeSet = FxHashSet<Edge>;
type NodeSet = FxHashSet<Node>;

impl Pass for IPSCCP {
    fn run(&self, program: &mut Program) -> bool {
        // Stage 0: Variables initialization.
        let mut edge_visited = EdgeSet::default();
        let mut node_visited = NodeSet::default();
        let mut edge_worklist: VecDeque<Edge> = VecDeque::default();
        let mut node_worklist: VecDeque<Node> = VecDeque::default();
        let mut lattice_map = LatticeMap::default();

        // Stage 0.1
        // Default: Set every integer to const, float to bottom, else remain top.
        for &func in program.function_layout() {
            let arena = ArenaContext {
                program,
                curr_func: Some(func),
            };
            for (&inst, data) in arena.inst_datas() {
                let node = Node::new(func, inst);
                match data.kind() {
                    InstKind::Integer(int) => lattice_map.new_const(node, int.value()),
                    InstKind::Float(..) => lattice_map.new_var(node),
                    _ => {}
                }
            }
        }

        // TODO: What about global value?

        // Stage 0.2
        // Build ICFG. Ready to start the worklist algorithm.
        let icfg = icfg::ICFG::new(program);
        let main_func = program.get_main_function();
        let entry_bb_layout = program.func_data(main_func).layout().entry_bb().unwrap();
        let first_inst = *entry_bb_layout.insts().get_first().unwrap();
        let start_node = Node::new(main_func, first_inst);

        // A virtual edge to start the loop
        let start_edge = Edge {
            edge_type: EdgeType::Normal,
            src: start_node,
            dst: start_node,
        };
        edge_worklist.push_back(start_edge);
        edge_visited.insert(start_edge);
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        struct Block {
            func: Function,
            block: BasicBlock,
        }
        impl Block {
            fn new(func: Function, block: BasicBlock) -> Block {
                Block { func, block }
            }
        }
        let mut block_visited = FxHashSet::default();

        // Stage 1: Worklist algorithm.
        while !edge_worklist.is_empty() || !node_worklist.is_empty() {
            if let Some(edge) = edge_worklist.pop_front() {
                let Edge { dst, .. } = edge;
                let parent_bb = program
                    .func_data(dst.func)
                    .layout()
                    .parent_bb(dst.inst)
                    .unwrap();
                block_visited.insert(Block::new(dst.func, parent_bb));
                // first time visit the node
                if node_visited.insert(dst) {
                    node_worklist.push_back(dst);
                }
            }

            // Tail-call relay nodes whose lattice was updated by the Return arm
            // need re-scheduling so their TailCall arm can forward the callee's
            // return value upward. They cannot be pushed to `node_worklist`
            // directly inside the node-processing block because the closures
            // (`merge_and_extend` et al.) hold a mutable borrow of it; collect
            // them here and drain after those closures are dropped.
            let mut relay_targets: Vec<Node> = Vec::new();

            if let Some(node) = node_worklist.pop_front() {
                let push_edge = |edge: Edge| {
                    edge_visited
                        .insert(edge)
                        .then(|| edge_worklist.push_back(edge));
                };
                let Node { func, inst } = node;
                let data = program.func_data(func);
                let mut extend_affected_node_used_by = |node: Node| {
                    let data = program.func_data(node.func);
                    node_worklist.extend(
                        data.inst_data(node.inst)
                            .used_by()
                            .iter()
                            .filter(|&&inst| {
                                data.layout().parent_bb(inst).is_some_and(|b| {
                                    block_visited.contains(&Block::new(node.func, b))
                                })
                            })
                            .map(|&inst| Node::new(node.func, inst)),
                    );
                };
                let mut merge_and_extend =
                    |node: Node, status: Lattice, lattice_map: &mut LatticeMap| -> bool {
                        if lattice_map.insert_or_update(node, status) {
                            extend_affected_node_used_by(node);
                            true
                        } else {
                            false
                        }
                    };
                match data.inst_data(inst).kind() {
                    // These instruction define a scalar and !never! appear in the layout
                    InstKind::Aggregate(..)
                    | InstKind::GlobalAlloc(..)
                    | InstKind::Undef
                    | InstKind::ZeroInit
                    | InstKind::Integer(..)
                    | InstKind::Float(..)
                    | InstKind::BlockArgRef(..) => unreachable!(
                        "instruction {:?} with data {:?} should never appear in the layout",
                        inst,
                        data.inst_data(inst)
                    ),
                    // For now, we lack simulation of main memory. So all memory related stuff is
                    // considerd as variable.
                    InstKind::GetElemPtr(..) | InstKind::Alloc | InstKind::Load(..) => {
                        merge_and_extend(node, Lattice::Bottom, &mut lattice_map);
                    }
                    InstKind::Binary(binary) => {
                        let status = match (
                            lattice_map.get(Node::new(func, binary.lhs())),
                            lattice_map.get(Node::new(func, binary.rhs())),
                        ) {
                            (Lattice::Bottom, _) | (_, Lattice::Bottom) => Lattice::Bottom,
                            (Lattice::Constant(lhs), Lattice::Constant(rhs)) => {
                                Lattice::Constant(mathematic_operation(binary.op(), lhs, rhs))
                            }
                            _ => Lattice::Top,
                        };
                        merge_and_extend(node, status, &mut lattice_map);
                    }
                    InstKind::Select(select) => {
                        let cond = select.cond();
                        let status_of = |node: Node| lattice_map.get(node);
                        let status = match status_of(Node::new(func, cond)) {
                            Lattice::Top => Lattice::Top,
                            Lattice::Constant(constant) => status_of(Node::new(
                                func,
                                if constant != 0 {
                                    select.if_true()
                                } else {
                                    select.if_false()
                                },
                            )),
                            Lattice::Bottom => status_of(Node::new(func, select.if_true()))
                                .merge(status_of(Node::new(func, select.if_false()))),
                        };
                        merge_and_extend(node, status, &mut lattice_map);
                    }
                    InstKind::Jump(jump) => {
                        let params = data.bb_data(jump.target()).params();
                        for (&arg, &param) in jump.args().iter().zip(params) {
                            let arg_status = lattice_map.get(Node::new(func, arg));
                            merge_and_extend(Node::new(func, param), arg_status, &mut lattice_map);
                        }
                    }
                    InstKind::Branch(branch) => {
                        let cond = branch.cond();
                        let cond_status = lattice_map.get(Node::new(func, cond));
                        let worklist = match cond_status {
                            Lattice::Top => [None, None],
                            Lattice::Bottom => [
                                Some((branch.t_target(), branch.t_args())),
                                Some((branch.f_target(), branch.f_args())),
                            ],
                            Lattice::Constant(c) => {
                                if c != 0 {
                                    [Some((branch.t_target(), branch.t_args())), None]
                                } else {
                                    [Some((branch.f_target(), branch.f_args())), None]
                                }
                            }
                        };
                        for (target, args) in worklist.into_iter().flatten() {
                            let params = data.bb_data(target).params();
                            for (&arg, &param) in args.iter().zip(params) {
                                let arg_status = lattice_map.get(Node::new(func, arg));
                                merge_and_extend(
                                    Node::new(func, param),
                                    arg_status,
                                    &mut lattice_map,
                                );
                            }
                        }
                    }
                    InstKind::Cast(cast) => {
                        if data.inst_data(inst).ty().is_i32() {
                            if let InstKind::Float(float) = data.inst_data(cast.src()).kind() {
                                merge_and_extend(
                                    node,
                                    Lattice::Constant(float.value() as i32),
                                    &mut lattice_map,
                                );
                            }
                            merge_and_extend(node, Lattice::Bottom, &mut lattice_map);
                        }
                        merge_and_extend(node, Lattice::Bottom, &mut lattice_map);
                    }
                    InstKind::Store(..) | InstKind::MemZero(..) => {}
                    InstKind::TailCall(tail_call) => {
                        // A tail call transfers control to the callee just like a
                        // regular call, so its actual arguments must flow into the
                        // callee's formal parameters. Without this, the only
                        // argument source IPSCCP would see is the (non-tail) call
                        // site, causing parameters that vary across recursive tail
                        // calls to be mis-propagated as constants.
                        let callee = tail_call.callee();
                        let callee_data = program.func_data(callee);
                        if !callee_data.layout().is_decl() {
                            for (&arg, &param) in
                                tail_call.args().iter().zip(callee_data.params())
                            {
                                let arg_status = lattice_map.get(Node::new(func, arg));
                                merge_and_extend(
                                    Node::new(callee, param),
                                    arg_status,
                                    &mut lattice_map,
                                );
                            }
                        }
                        // Relay: a tail call forwards the callee's return value
                        // directly to the current function's caller (the frame is
                        // reused). The callee's Return arm deposits its return value
                        // into *this* node's lattice (see the Return arm below);
                        // propagate it further along this node's outgoing Return
                        // edges, which connect to the caller's call site.
                        let node_status = lattice_map.get(node);
                        for Edge { dst, edge_type, .. } in
                            icfg.outgoing_edges_of(node)
                        {
                            if edge_type != EdgeType::Return {
                                continue;
                            }
                            if let Some(cs) = icfg.call_site_before(dst) {
                                merge_and_extend(
                                    Node::new(dst.func, cs.call),
                                    node_status,
                                    &mut lattice_map,
                                );
                            }
                        }
                    }
                    InstKind::Call(call) => {
                        let callee = call.callee();
                        let callee_data = program.func_data(callee);
                        if callee_data.layout().is_decl() {
                            merge_and_extend(node, Lattice::Bottom, &mut lattice_map);
                        } else {
                            for (&arg, &param) in call.args().iter().zip(callee_data.params()) {
                                let arg_status = lattice_map.get(Node::new(func, arg));
                                merge_and_extend(
                                    Node::new(callee, param),
                                    arg_status,
                                    &mut lattice_map,
                                );
                            }
                        }
                    }
                    InstKind::Return(ret) => {
                        if let Some(ret_val) = ret.value() {
                            let ret_val_status = lattice_map.get(Node::new(func, ret_val));
                            let outgoing_edges = icfg.outgoing_edges_of(node);
                            for Edge { dst, .. } in outgoing_edges {
                                // For a regular call, the Return edge lands at the
                                // call's continuation and `call_site_before` resolves
                                // the call site whose `.call` node receives the value.
                                // For a tail call, the edge lands at the tail-call
                                // instruction itself (the relay node), which is
                                // deliberately absent from `callsite_by_continuation`
                                // — deposit the value directly into that node.
                                let target = match icfg.call_site_before(dst) {
                                    Some(cs) => Node::new(dst.func, cs.call),
                                    None => dst,
                                };
                                if merge_and_extend(
                                    target,
                                    ret_val_status,
                                    &mut lattice_map,
                                ) {
                                    // The relay node's lattice was set externally
                                    // (by us, not by its own evaluation), so it will
                                    // not be revisited through `used_by`. Re-schedule
                                    // it so its TailCall arm can forward the value
                                    // upward along the tail-call chain.
                                    if matches!(
                                        program
                                            .func_data(target.func)
                                            .inst_data(target.inst)
                                            .kind(),
                                        InstKind::TailCall(..)
                                    ) {
                                        relay_targets.push(target);
                                    }
                                }
                            }
                        }
                    }
                }
                // update edges.
                match data.inst_data(inst).kind() {
                    InstKind::Branch(branch) => {
                        let cond = branch.cond();
                        let cond_status = lattice_map.get(Node::new(func, cond));
                        let construct_edge = |block| Edge {
                            edge_type: EdgeType::Normal,
                            src: Node::new(func, inst),
                            dst: Node::new(
                                func,
                                *data.layout().basicblock(block).insts().get_first().unwrap(),
                            ),
                        };
                        let edges = match cond_status {
                            Lattice::Top => [None, None],
                            Lattice::Bottom => [Some(branch.t_target()), Some(branch.f_target())],
                            Lattice::Constant(constant) => {
                                if constant != 0 {
                                    [Some(branch.t_target()), None]
                                } else {
                                    [Some(branch.f_target()), None]
                                }
                            }
                        };
                        edges
                            .into_iter()
                            .flatten()
                            .map(construct_edge)
                            .for_each(push_edge);
                    }
                    InstKind::Return(..) => {}
                    InstKind::TailCall(..) => {
                        // Push the Call edge so the callee's entry becomes
                        // reachable. Return edges are consumed by the relay logic
                        // in the TailCall lattice arm, not pushed here.
                        icfg.outgoing_edges_of(Node::new(func, inst))
                            .filter(|e| e.edge_type == EdgeType::Call)
                            .for_each(push_edge);
                    }
                    _ => {
                        icfg.outgoing_edges_of(Node::new(func, inst))
                            .for_each(push_edge);
                    }
                }
            }
            // Closures borrowing `node_worklist` are now dropped; safe to extend.
            node_worklist.extend(relay_targets.drain(..));
        }

        let mut changed = false;

        let const_replace_list = lattice_map
            .0
            .iter()
            .filter_map(|(&node, &lattice)| {
                if let Lattice::Constant(c) = lattice {
                    Some((node, c))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        for (node, val) in const_replace_list {
            // TODO: Waiting for call analysis
            let mut arena = ArenaContextMut {
                program,
                curr_func: Some(node.func),
            };
            if let InstKind::Call(..) = arena.inst_data(node.inst).kind() {
                let integer = arena.new_local_inst().integer(val);
                visit_and_replace(&mut arena, node.inst, integer);
            } else {
                arena.replace_inst_with(node.inst).integer(val);
                let data = program.func_data_mut(node.func);
                let Some(parent_bb) = data.layout().parent_bb(node.inst) else {
                    continue;
                };
                data.detach_layout_inst(parent_bb, node.inst);
                changed = true;
            }
        }

        let mut useless_conditional_branch = vec![];
        for &func in program.function_layout() {
            let data = program.func_data(func);
            for bb_layout in program.func_data(func).layout().basicblocks() {
                let terminator = *bb_layout.insts().get_last().unwrap();
                if let InstKind::Branch(branch) = data.inst_data(terminator).kind() {
                    if let InstKind::Integer(..) = data.inst_data(branch.cond()).kind() {
                        useless_conditional_branch.push(Node::new(func, terminator));
                    }
                }
            }
        }

        for node in useless_conditional_branch {
            let Node { func, inst } = node;
            let data = program.func_data_mut(func);
            let InstKind::Branch(branch) = data.inst_data(inst).kind() else {
                unreachable!()
            };
            let InstKind::Integer(int) = data.inst_data(branch.cond()).kind() else {
                unreachable!()
            };
            let (target, args) = if int.value() == 0 {
                (branch.f_target(), branch.f_args().to_vec())
            } else {
                (branch.t_target(), branch.t_args().to_vec())
            };
            data.replace_inst_with(inst).jump(target, args);
            changed = true;
        }

        let mut remove_list = vec![];
        for &func in program.function_layout() {
            let data = program.func_data(func);
            remove_list.extend(
                data.layout()
                    .basicblocks()
                    .iter()
                    .map(|l| l.bb())
                    .filter(|&bb| {
                        bb != data.layout().entry_bb().unwrap().bb()
                            && data.bb_data(bb).used_by().is_empty()
                    })
                    .map(|bb| Block::new(func, bb))
                    .collect::<Vec<_>>(),
            )
        }
        for bb in remove_list {
            let data = program.func_data_mut(bb.func);
            data.remove_layout_basicblock(bb.block);
            changed = true;
        }

        changed
    }
}

fn mathematic_operation(op: BinaryOp, lhs: i32, rhs: i32) -> i32 {
    match op {
        BinaryOp::NotEq => (lhs != rhs) as i32,
        BinaryOp::Eq => (lhs == rhs) as i32,
        BinaryOp::Gt => (lhs > rhs) as i32,
        BinaryOp::Lt => (lhs < rhs) as i32,
        BinaryOp::Ge => (lhs >= rhs) as i32,
        BinaryOp::Le => (lhs <= rhs) as i32,
        BinaryOp::Add => lhs.wrapping_add(rhs),
        BinaryOp::Sub => lhs.wrapping_sub(rhs),
        BinaryOp::Mul => lhs.wrapping_mul(rhs),
        BinaryOp::Div => {
            assert_ne!(rhs, 0);
            lhs.wrapping_div(rhs)
        }
        BinaryOp::Rem => {
            assert_ne!(rhs, 0);
            lhs.wrapping_rem(rhs)
        }
        BinaryOp::And => lhs & rhs,
        BinaryOp::Or => lhs | rhs,
        BinaryOp::Xor => lhs ^ rhs,
        BinaryOp::Shl => lhs.wrapping_shl(rhs as u32),
        BinaryOp::Shr => (lhs as u32).wrapping_shr(rhs as u32) as i32,
        BinaryOp::Sar => lhs.wrapping_shr(rhs as u32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        BinaryOp,
        builder::{BasicBlockBuilder, LocalInstBuilder, ScalarInstBuilder},
    };

    /// Build a self-recursive tail-call function and verify that IPSCCP does
    /// not mis-propagate a parameter that varies across recursive tail calls.
    ///
    /// ```text
    /// fun(n: i32, dep: i32) -> i32:
    ///     if n == 0: return dep      // base case — dep must stay variable
    ///     else: tail_call fun(n - 1, dep + 1)
    /// main(): return fun(2, 0)
    /// ```
    ///
    /// With correct tail-call argument propagation, `dep` receives both
    /// `Constant(0)` (from main) and `Constant(1)` (from the recursive
    /// `dep+1`), converging to `Bottom`. Without the fix, IPSCCP would only
    /// see the non-tail call `fun(2, 0)` and constant-propagate `dep` to `0`,
    /// replacing the `ret dep` with `ret 0`.
    #[test]
    fn tail_call_args_prevent_constant_mispropagation() {
        let mut program = Program::new();

        // fun(n: i32, dep: i32) -> i32
        let fun = program.new_function(
            Type::get_i32(),
            "fun".into(),
            vec![Type::get_i32(), Type::get_i32()],
        );
        let dep_param = {
            let data = program.func_data_mut(fun);
            let entry = data.add_entry_block();
            let params = data.bb_data(entry).params();
            let n = params[0];
            let dep = params[1];

            let base = data.new_basic_block().basic_block("base".into(), vec![]);
            let rec = data.new_basic_block().basic_block("rec".into(), vec![]);
            data.layout_mut().push_bb_back(base);
            data.layout_mut().push_bb_back(rec);

            // entry: br (n == 0), base, rec
            // (Integer constants are not placed in the layout — IPSCCP
            // initialises their lattice in Stage 0.1.)
            let zero = data.new_local_inst().integer(0);
            let cond = data.new_local_inst().binary(BinaryOp::Eq, n, zero);
            let br = data
                .new_local_inst()
                .branch(cond, base, vec![], rec, vec![]);
            data.layout_mut().insert_inst(entry, cond);
            data.layout_mut().insert_inst(entry, br);

            // base: ret dep
            let ret_dep = data.new_local_inst().ret(Some(dep));
            data.layout_mut().insert_inst(base, ret_dep);

            // rec: tail_call fun(n - 1, dep + 1)
            let one = data.new_local_inst().integer(1);
            let nm1 = data.new_local_inst().binary(BinaryOp::Sub, n, one);
            let one2 = data.new_local_inst().integer(1);
            let depp1 = data.new_local_inst().binary(BinaryOp::Add, dep, one2);
            let tc = data.new_local_inst().tail_call(fun, vec![nm1, depp1]);
            data.layout_mut().insert_inst(rec, nm1);
            data.layout_mut().insert_inst(rec, depp1);
            data.layout_mut().insert_inst(rec, tc);

            dep
        };

        // main() -> i32: return fun(2, 0)
        let main = program.new_function(Type::get_i32(), "main".into(), vec![]);
        {
            let data = program.func_data_mut(main);
            let entry = data.add_entry_block();
            let two = data.new_local_inst().integer(2);
            let zero = data.new_local_inst().integer(0);
            let call = data
                .new_local_inst()
                .call_with_type(fun, vec![two, zero], Type::get_i32());
            let ret = data.new_local_inst().ret(Some(call));
            data.layout_mut().insert_inst(entry, call);
            data.layout_mut().insert_inst(entry, ret);
        }

        IPSCCP.run(&mut program);

        // After IPSCCP, `dep` must still be a BlockArgRef (not folded to an
        // Integer constant). If the tail-call arm were missing, dep's lattice
        // would be Constant(0) and `replace_inst_with` would have mutated its
        // data to Integer(0).
        let data = program.func_data(fun);
        assert!(
            matches!(
                data.inst_data(dep_param).kind(),
                InstKind::BlockArgRef(..)
            ),
            "dep parameter was constant-propagated — tail-call arg propagation is broken"
        );
    }
}
