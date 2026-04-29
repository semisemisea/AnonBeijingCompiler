use std::collections::{HashSet, VecDeque};

use crate::opt::pass::Pass;

use crate::ir::{
    BasicBlock, BinaryOp, Function, FunctionData, Inst, InstKind,
    builder_trait::{LocalInstBuilder, ScalarInstBuilder},
};

use crate::opt::utils::{BId, IDAllocator, VId, visit_and_replace};

pub struct SparseConditionConstantPropagation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VariableStatus {
    Top,
    Constant(i32),
    Bottom,
}

impl VariableStatus {
    fn new_with_const(value: i32) -> VariableStatus {
        VariableStatus::Constant(value)
    }

    fn new_variable() -> VariableStatus {
        VariableStatus::Bottom
    }

    #[must_use]
    fn update(&mut self, status: VariableStatus) -> bool {
        match &self {
            old_status @ VariableStatus::Constant(..) if **old_status != status => {
                *self = VariableStatus::Bottom;
                true
            }
            _ => false,
        }
    }

    fn as_const(&self) -> Option<i32> {
        match self {
            VariableStatus::Constant(constant) => Some(*constant),
            _ => None,
        }
    }
}

struct InstStatusMap {
    status: Vec<VariableStatus>,
    var_allocator: IDAllocator<Inst, VId>,
}

impl InstStatusMap {
    fn new() -> InstStatusMap {
        Self {
            status: Vec::new(),
            var_allocator: IDAllocator::new(1),
        }
    }

    fn insert(&mut self, val: Inst, status: VariableStatus) {
        let id = self.var_allocator.check_or_alloc_id_same(val);
        // must be a new value that did not appear before
        assert!(id == self.status.len());
        self.status.push(status);
    }

    #[must_use]
    fn merge(&mut self, val: Inst, status: VariableStatus) -> bool {
        let id = self.var_allocator.get_id(&val);
        self.status[id].update(status)
    }

    #[must_use]
    fn insert_or_merge(&mut self, val: Inst, status: VariableStatus) -> bool {
        let id = self.var_allocator.check_or_alloc_id_same(val);
        if id < self.status.len() {
            self.merge(val, status)
        } else {
            self.insert(val, status);
            false
        }
    }

    fn get(&self, val: Inst) -> &VariableStatus {
        self.var_allocator
            .get_id_safe(&val)
            .map(|&x| &self.status[x])
            // .unwrap()
            .unwrap_or(&VariableStatus::Top)
    }

    fn get_safe(&self, val: Inst) -> Option<&VariableStatus> {
        self.var_allocator
            .get_id_safe(&val)
            .and_then(|&x| self.status.get(x))
    }

    // fn debug(&self, data: &FunctionData) {
    //     for (i, st) in self.status.iter().enumerate() {
    //         eprintln!(
    //             "{:?} {:?} {:?}",
    //             self.var_allocator.search_id(i),
    //             data.dfg().value(self.var_allocator.search_id(i)).kind(),
    //             st
    //         )
    //     }
    //     eprintln!("{:?}", self.status);
    // }
}

// type InstStatus = Vec<VariableStatus>;
type EdgeSet = HashSet<(BId, BId)>;
type FlowWorklist = VecDeque<(BId, BId)>;
type SSAWorklist = VecDeque<Inst>;

const REMOVE_FLAG: bool = true;

impl Pass for SparseConditionConstantPropagation {
    fn run_on(&mut self, _func: Function, data: &mut FunctionData) {
        let Some(entry_bb) = data.layout().entry_bb() else {
            return;
        };
        let mut bb_allocator: IDAllocator<BasicBlock, BId> = IDAllocator::new(1);
        bb_allocator.check_or_alloc_id_same(entry_bb.bb());

        let mut edge_visited = EdgeSet::new();
        let mut vertex_visited = HashSet::new();

        let mut flow_worklist = FlowWorklist::new();
        let mut ssa_worklist = SSAWorklist::new();
        let mut value_status_map = InstStatusMap::new();

        for (&val, val_data) in data.dfg().values() {
            if let InstKind::Integer(int) = val_data.kind() {
                value_status_map.insert(val, VariableStatus::new_with_const(int.value()));
            }
        }

        for &param in data.params() {
            value_status_map.insert(param, VariableStatus::new_variable());
        }

        // trigger
        // This edge is not exist but we can manually trigger the loop.
        flow_worklist.push_back((0, 0));

        while !flow_worklist.is_empty() || !ssa_worklist.is_empty() {
            if let Some(edge) = flow_worklist.pop_front() {
                if edge_visited.contains(&edge) {
                    continue;
                }
                edge_visited.insert(edge);
                let current_bb = bb_allocator.search_id(edge.1);
                // for &param in data.dfg().bb(current_bb).params() {
                //     belongs_to.insert(param, current_bb);
                // }

                if !vertex_visited.contains(&edge.1) {
                    vertex_visited.insert(edge.1);
                    for (&inst, _) in data.layout().bbs().node(&current_bb).unwrap().insts() {
                        ssa_worklist.push_back(inst);
                    }
                }
            }
            if let Some(inst) = ssa_worklist.pop_front() {
                if let Some(ext) = process_instruction(
                    data,
                    &mut value_status_map,
                    inst,
                    &mut flow_worklist,
                    &mut bb_allocator,
                ) {
                    ssa_worklist.extend(ext.into_iter());
                }
            }
        }

        // eprintln!("showing");
        // value_status_map.debug(data);
        // eprintln!("showing");

        if REMOVE_FLAG {
            let replace_list = data
                .layout()
                .bbs()
                .iter()
                .flat_map(|(_, node)| node.insts().iter().map(|(&inst, _)| inst))
                .filter(|&inst| value_status_map.get_safe(inst).is_some())
                .collect::<Vec<_>>();

            for inst in replace_list.into_iter().rev() {
                let Some(constant) = value_status_map.get(inst).as_const() else {
                    continue;
                };
                data.dfg_mut().replace_value_with(inst).integer(constant);
                let parent_bb = data.layout().parent_bb(inst).unwrap();
                data.layout_mut()
                    .bbs_mut()
                    .node_mut(&parent_bb)
                    .unwrap()
                    .insts_mut()
                    .remove(&inst);
            }

            let mut useless_unconditional_list = Vec::new();
            for (_, node) in data.layout().bbs() {
                let &terminator_inst = node.insts().back_key().unwrap();
                if let InstKind::Branch(branch) = data.dfg().value(terminator_inst).kind() {
                    if let InstKind::Integer(..) = data.dfg().value(branch.cond()).kind() {
                        useless_unconditional_list.push(terminator_inst);
                    }
                }
            }

            for t_inst in useless_unconditional_list {
                let InstKind::Branch(branch) = data.dfg().value(t_inst).kind() else {
                    unreachable!()
                };
                let InstKind::Integer(int) = data.dfg().value(branch.cond()).kind() else {
                    unreachable!()
                };
                let (target, args) = if int.value() == 0 {
                    (branch.false_bb(), branch.false_args().to_vec())
                } else {
                    (branch.true_bb(), branch.true_args().to_vec())
                };
                data.dfg_mut()
                    .replace_value_with(t_inst)
                    .jump_with_args(target, args);
            }

            let remove_list = data
                .layout()
                .bbs()
                .iter()
                .map(|(&x, _)| x)
                .filter(|&bb| {
                    bb != data.layout().entry_bb().unwrap()
                        && data.dfg().bb(bb).used_by().is_empty()
                })
                .collect::<Vec<_>>();
            for bb in remove_list {
                data.layout_mut().bbs_mut().remove(bb);
                // data.dfg_mut().remove_bb(bb);
            }

            let mut useless_phi_list = Vec::new();
            for (&bb, _) in data.layout().bbs() {
                let bb_data = data.dfg().bb(bb);
                if !bb_data.params().is_empty() {
                    let jump_insts = bb_data
                        .used_by()
                        .iter()
                        .filter(|&&x| {
                            data.layout()
                                .parent_bb(x)
                                .is_some_and(|x| data.layout().bbs().contains_key(&x))
                        })
                        .copied()
                        .collect::<Vec<_>>();
                    if jump_insts.len() == 1 {
                        useless_phi_list.push((bb, jump_insts[0]));
                    }
                }
            }

            for (bb, jump_inst) in useless_phi_list {
                let params = data.dfg().bb(bb).params().to_vec();
                let args = match data.dfg().value(jump_inst).kind() {
                    InstKind::Jump(jump) => jump.args(),
                    InstKind::Branch(branch) => {
                        if branch.true_bb() == bb {
                            branch.true_args()
                        } else {
                            branch.false_args()
                        }
                    }
                    _ => unreachable!(),
                }
                .to_vec();
                for (arg, param) in args.into_iter().zip(params) {
                    // let used_by = data
                    //     .dfg()
                    //     .value(param)
                    //     .used_by()
                    //     .iter()
                    //     .copied()
                    //     .collect::<Vec<_>>();
                    // for param_used_by in used_by {
                    visit_and_replace(data, param, arg);
                    // }
                    data.dfg_mut()
                        .bb_mut(bb)
                        .params_mut()
                        .retain(|&x| x != param);
                }
                match data.dfg().value(jump_inst).kind() {
                    InstKind::Jump(jump) => {
                        let target_bb = jump.target();
                        data.dfg_mut().replace_value_with(jump_inst).jump(target_bb);
                    }
                    InstKind::Branch(branch) => {
                        if branch.true_bb() == bb {
                            let false_args = branch.false_args().to_vec();
                            let false_bb = branch.false_bb();
                            let cond = branch.cond();
                            data.dfg_mut()
                                .replace_value_with(jump_inst)
                                .branch_with_args(cond, bb, false_bb, vec![], false_args);
                        } else {
                            let true_args = branch.true_args().to_vec();
                            let true_bb = branch.true_bb();
                            let cond = branch.cond();
                            data.dfg_mut()
                                .replace_value_with(jump_inst)
                                .branch_with_args(cond, true_bb, bb, true_args, vec![]);
                        }
                    }
                    _ => unreachable!(),
                }
            }
        }
    }
}

fn process_instruction(
    data: &FunctionData,
    value_status_map: &mut InstStatusMap,
    inst: Inst,
    flow_worklist: &mut FlowWorklist,
    bb_allocator: &mut IDAllocator<BasicBlock, BId>,
) -> Option<Vec<Inst>> {
    macro_rules! ret_with {
        ($inst: expr) => {
            data.dfg()
                .value($inst)
                .used_by()
                .iter()
                .filter(|&&val| {
                    let Some(parent_bb) = data.layout().parent_bb(val) else {
                        return true;
                    };
                    bb_allocator.get_id_safe(&parent_bb).is_some()
                })
                .copied()
                .collect::<Vec<_>>()
        };
    }
    match data.dfg().value(inst).kind() {
        InstKind::ZeroInit
        | InstKind::Undef
        | InstKind::Aggregate(..)
        | InstKind::Integer(..)
        | InstKind::BlockArgRef(..)
        | InstKind::FuncArgRef(..)
        | InstKind::GlobalAlloc(..) => unreachable!(),
        InstKind::Store(..) => None,
        left => match left {
            InstKind::GetPtr(..)
            | InstKind::GetElemPtr(..)
            | InstKind::Load(..)
            | InstKind::Alloc => value_status_map
                .insert_or_merge(inst, VariableStatus::new_variable())
                .then_some(ret_with!(inst)),
            InstKind::Binary(binary) => {
                macro_rules! get_constant_or_continue {
                    ($e:expr) => {
                        if let InstKind::Integer(int) = data.dfg().value($e).kind() {
                            int.value()
                        } else {
                            match value_status_map.get($e) {
                                VariableStatus::Top => unreachable!(),
                                VariableStatus::Constant(constant) => *constant,
                                VariableStatus::Bottom => {
                                    return value_status_map
                                        .insert_or_merge(inst, VariableStatus::new_variable())
                                        .then_some(ret_with!(inst));
                                }
                            }
                        }
                    };
                }
                let lhs = get_constant_or_continue!(binary.lhs());
                let rhs = get_constant_or_continue!(binary.rhs());
                let outcome = mathematic_operation(binary.op(), lhs, rhs);
                value_status_map
                    .insert_or_merge(inst, VariableStatus::new_with_const(outcome))
                    .then_some(ret_with!(inst))
            }
            InstKind::Branch(branch) => {
                let cond = branch.cond();
                let condition_value_status = value_status_map.get(cond);
                let worklist = match condition_value_status {
                    VariableStatus::Top => unreachable!(),
                    VariableStatus::Constant(constant) => [
                        Some(if *constant != 0 {
                            (branch.true_bb(), branch.true_args())
                        } else {
                            (branch.false_bb(), branch.false_args())
                        }),
                        None,
                    ],
                    VariableStatus::Bottom => [
                        Some((branch.true_bb(), branch.true_args())),
                        Some((branch.false_bb(), branch.false_args())),
                    ],
                };
                let mut influenced = Vec::new();
                for (target_bb, args) in worklist.into_iter().flatten() {
                    let params = data.dfg().bb(target_bb).params();
                    for (&arg, &param) in args.iter().zip(params.iter()) {
                        let arg_status = match data.dfg().value(arg).kind() {
                            InstKind::Integer(integer) => {
                                &VariableStatus::Constant(integer.value())
                            }
                            InstKind::Undef => &VariableStatus::Top,
                            _ => value_status_map.get(arg),
                        };
                        if value_status_map.insert_or_merge(param, *arg_status) {
                            influenced.extend(ret_with!(param));
                        }
                    }
                    flow_worklist.push_back((
                        bb_allocator.get_id(&data.layout().parent_bb(inst).unwrap()),
                        bb_allocator.check_or_alloc_id_same(target_bb),
                    ));
                }
                Some(influenced)
            }
            InstKind::Jump(jump) => {
                let mut influenced = Vec::new();
                let params = data.dfg().bb(jump.target()).params();
                for (&arg, &param) in jump.args().iter().zip(params.iter()) {
                    let arg_status = value_status_map.get(arg);
                    if value_status_map.insert_or_merge(param, *arg_status) {
                        influenced.extend(ret_with!(param));
                    }
                }
                flow_worklist.push_back((
                    bb_allocator.get_id(&data.layout().parent_bb(inst).unwrap()),
                    bb_allocator.check_or_alloc_id_same(jump.target()),
                ));
                Some(influenced)
            }
            InstKind::Call(..) => value_status_map
                .insert_or_merge(inst, VariableStatus::new_variable())
                .then_some(ret_with!(inst)),
            InstKind::Return(..) => None,
            _ => unreachable!(),
        },
    }
}

fn mathematic_operation(op: &BinaryOp, lhs: i32, rhs: i32) -> i32 {
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
        BinaryOp::Mod => {
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
