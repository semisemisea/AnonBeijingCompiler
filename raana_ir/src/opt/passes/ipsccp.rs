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
                    |node: Node, status: Lattice, lattice_map: &mut LatticeMap| {
                        if lattice_map.insert_or_update(node, status) {
                            extend_affected_node_used_by(node);
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
                    | InstKind::BlockArgRef(..)
                    | InstKind::FuncArgRef(..) => unreachable!(
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
                                let call_site = icfg.call_site_before(dst).unwrap();
                                merge_and_extend(
                                    Node::new(dst.func, call_site.call),
                                    ret_val_status,
                                    &mut lattice_map,
                                )
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
                    _ => {
                        icfg.outgoing_edges_of(Node::new(func, inst))
                            .for_each(push_edge);
                    }
                }
            }
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
