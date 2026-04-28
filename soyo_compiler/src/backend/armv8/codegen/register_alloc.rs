use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet},
    fmt::Debug,
    ops::RangeInclusive,
};

use itertools::Itertools;
use raana_ir::ir::{FunctionData, Inst, InstKind};

use crate::{
    backend::armv8::register::Register,
    opt::utils::{self, IDAllocator, VIDAlloc, get_terminator_inst},
};

type VRegister = Reverse<Register>;

#[derive(Debug)]
struct VirtualRegister {
    container: BinaryHeap<VRegister>,
    rules: HashMap<VRegister, Vec<RangeInclusive<usize>>>,
    loops: Vec<RangeInclusive<usize>>,
    callee_used: HashSet<Register>,
}

fn is_ranges_intersect(lhs: &RangeInclusive<usize>, rhs: &RangeInclusive<usize>) -> bool {
    !(lhs.end() < rhs.start() || lhs.start() > rhs.end())
}

impl VirtualRegister {
    fn alloc(&mut self, for_range: &RangeInclusive<usize>) -> Option<VRegister> {
        let mut not_usable = Vec::new();
        while let Some(virtual_reg) = self.container.pop() {
            if self.check(virtual_reg, for_range) {
                if virtual_reg.0.is_saved() {
                    self.callee_used.insert(virtual_reg.0);
                }
                return Some(virtual_reg);
            }
            not_usable.push(virtual_reg);
        }
        self.container.extend(not_usable);
        None
    }

    #[inline]
    fn free(&mut self, reg: VRegister) {
        self.container.push(reg);
    }

    #[inline]
    fn check(&mut self, virtual_reg: VRegister, for_range: &RangeInclusive<usize>) -> bool {
        match self.rules.get(&virtual_reg) {
            Some(ranges) => ranges
                .iter()
                .all(|range| !is_ranges_intersect(range, for_range)),
            None => true,
        }
    }

    #[inline]
    fn add_rule(&mut self, virtual_reg: VRegister, ranges: RangeInclusive<usize>) {
        self.rules.entry(virtual_reg).or_default().push(ranges);
    }

    #[inline]
    fn new(max_size: usize) -> VirtualRegister {
        Self {
            container: BinaryHeap::from_iter(
                (0..max_size as u8).map(|x| Reverse(x.try_into().unwrap())),
            ),
            rules: HashMap::new(),
            loops: Vec::new(),
            callee_used: HashSet::new(),
        }
    }

    fn add_loop(&mut self, r#loop: RangeInclusive<usize>) {
        self.loops.push(r#loop);
    }

    fn extent_by_loop(&self, range: RangeInclusive<usize>) -> RangeInclusive<usize> {
        let max_end = self.loops.iter().fold(*range.end(), |end, loop_range| {
            if range.contains(loop_range.start()) {
                end.max(*loop_range.end())
            } else {
                end
            }
        });
        *range.start()..=max_end
    }
}

#[derive(Debug)]
pub enum AllocationState {
    Register(Register),
    Stack(usize),
}

pub struct RegisterAllocationResult {
    pub allocation: RegisterAllocation,
    pub offset: usize,
    pub call_ra: bool,
    pub extra_args: usize,
    pub callee_usage: HashSet<Register>,
}

pub type RegisterAllocation = HashMap<Inst, AllocationState>;

const REGISTER_COUNT: usize = 24;

pub fn liveness_analysis(data: &FunctionData) -> RegisterAllocationResult {
    let mut bb_alloc = IDAllocator::new(1);
    let mut val_alloc: VIDAlloc = IDAllocator::new(4);
    // let graph = utils::build_cfg_forward(data, &mut bb_alloc);
    let (graph, prece) = utils::build_cfg_both(data, &mut bb_alloc);
    let rpo_path = utils::rpo_path(&graph);
    let mut register_manager = VirtualRegister::new(REGISTER_COUNT);

    let mut call_ra = false;
    let mut extra_args = 0usize;

    for &fparam in data.params() {
        val_alloc.check_or_alloc_id_same(fparam);
    }
    for &bb_id in rpo_path.iter() {
        let bb = bb_alloc.search_id(bb_id);
        let node = data.layout().bbs().node(&bb).unwrap();
        let iter = data.dfg().bb(bb).params().iter().chain(node.insts().keys());

        for &inst in iter {
            let id = val_alloc.check_or_alloc_id_same(inst);

            if let InstKind::Call(call) = data.dfg().value(inst).kind() {
                call_ra = true;
                extra_args = extra_args.max(8.max(call.args().len()) - 8);
                for index in 0..8 {
                    register_manager.add_rule(Reverse(Register::arguments(index)), id..=id);
                }
                for index in 0..7 {
                    register_manager.add_rule(Reverse(Register::temporary(index)), id..=id);
                }
            }
        }
    }

    for &bb_id in rpo_path.iter() {
        let bb = bb_alloc.search_id(bb_id);
        let terminator_inst = get_terminator_inst(data, bb);

        macro_rules! add_loop {
            ($backedge_goes_to: expr) => {
                if let Some(id) = bb_alloc.get_id_safe(&$backedge_goes_to) {
                    let head_bb = bb_alloc.search_id(*id);
                    let head_bb_first_inst = *data.dfg().bb(head_bb).params().first().unwrap_or(
                        data.layout()
                            .bbs()
                            .node(&head_bb)
                            .unwrap()
                            .insts()
                            .front_key()
                            .unwrap(),
                    );

                    if let (Some(header_id), Some(latch_term_id)) = (
                        val_alloc.get_id_safe(&head_bb_first_inst),
                        val_alloc.get_id_safe(&terminator_inst),
                    ) {
                        if header_id < latch_term_id {
                            let mut max_loop_id = *latch_term_id;

                            let mut worklist = vec![bb_id];
                            let mut visited = HashSet::new();
                            visited.insert(bb_id);

                            while let Some(curr_id) = worklist.pop() {
                                let curr_bb = bb_alloc.search_id(curr_id);

                                let curr_node = data.layout().bbs().node(&curr_bb).unwrap();
                                if let Some(last_inst) = curr_node.insts().back_key() {
                                    if let Some(inst_id) = val_alloc.get_id_safe(last_inst) {
                                        max_loop_id = max_loop_id.max(*inst_id);
                                    }
                                }

                                if curr_bb == head_bb {
                                    continue;
                                }

                                let preds = &prece[&curr_id];
                                for &pred_id in preds.iter() {
                                    if visited.insert(pred_id) {
                                        worklist.push(pred_id);
                                    }
                                }
                            }

                            register_manager.add_loop(*header_id..=max_loop_id);
                        }
                    }
                }
            };
        }

        match data.dfg().value(terminator_inst).kind() {
            InstKind::Jump(jump) => {
                add_loop!(jump.target());
            }
            InstKind::Branch(branch) => {
                add_loop!(branch.true_bb());
                add_loop!(branch.false_bb());
            }
            InstKind::Return(..) => {}
            _ => unreachable!(),
        }
    }
    // for &bb_id in rpo_path.iter() {
    //     let bb = bb_alloc.search_id(bb_id);
    //     let node = data.layout().bbs().node(&bb).unwrap();
    //     let iter = data
    //         .dfg()
    //         .bb(bb)
    //         .params()
    //         .iter()
    //         .chain(node.insts().iter().map(|(val, _)| val));
    //     for &inst in iter {
    //         let id = val_alloc.check_or_alloc(inst);
    //
    //         if let InstKind::Call(call) = data.dfg().value(inst).kind() {
    //             call_ra = true;
    //             extra_args = extra_args.max(8.max(call.args().len()) - 8);
    //             for index in 0..8 {
    //                 register_manager.add_rule(Reverse(Register::arguments(index)), id..=id);
    //             }
    //             for index in 0..7 {
    //                 register_manager.add_rule(Reverse(Register::temporary(index)), id..=id);
    //             }
    //         }
    //     }
    //
    //     let terminator_inst = get_terminator_inst(data, bb);
    //
    //     // TODO:
    //     macro_rules! add_loop {
    //         ($backedge_goes_to: expr) => {
    //             if let Some(id) = bb_alloc.get_id_safe($backedge_goes_to) {
    //                 let head_bb = bb_alloc.search_id(*id);
    //                 let head_bb_first_inst = *data.dfg().bb(head_bb).params().first().unwrap_or(
    //                     data.layout()
    //                         .bbs()
    //                         .node(&head_bb)
    //                         .unwrap()
    //                         .insts()
    //                         .front_key()
    //                         .unwrap(),
    //                 );
    //                 if let (Some(header_id), Some(latch_id)) = (
    //                     val_alloc.get_id_safe(head_bb_first_inst),
    //                     val_alloc.get_id_safe(terminator_inst),
    //                 ) && header_id < latch_id
    //                 {
    //                     register_manager.add_loop(*header_id..=*latch_id);
    //                 }
    //             }
    //         };
    //     }
    //     match data.dfg().value(terminator_inst).kind() {
    //         InstKind::Jump(jump) => {
    //             add_loop!(jump.target());
    //         }
    //         InstKind::Branch(branch) => {
    //             add_loop!(branch.true_bb());
    //             add_loop!(branch.false_bb());
    //         }
    //         InstKind::Return(..) => {}
    //         _ => unreachable!(),
    //     }
    // }

    let mut liveness_ranges = HashMap::new();

    macro_rules! insert_range {
        ($inst:expr, $min_id: expr) => {
            let max_id = data
                .dfg()
                .value($inst)
                .used_by()
                .iter()
                .filter_map(|&val| val_alloc.get_id_safe(&val))
                .max()
                .copied()
                .unwrap_or($min_id);
            let range = register_manager.extent_by_loop($min_id..=max_id);
            eprintln!("insert range:{:?} {:?}", $inst, range);
            liveness_ranges.insert($inst, range);
        };
    }
    // val_alloc.get_id($inst)

    for &fparam in data.params().iter().take(8) {
        insert_range!(fparam, val_alloc.get_id(&fparam));
    }
    for bb_id in rpo_path {
        let bb = bb_alloc.search_id(bb_id);
        let node = data.layout().bbs().node(&bb).unwrap();
        eprintln!("params:{:?}", data.dfg().bb(bb).params());

        if let Some(min_id) = data
            .dfg()
            .bb(bb)
            .used_by()
            .iter()
            .map(|&val| val_alloc.get_id(&val))
            .min()
        {
            for &block_param in data.dfg().bb(bb).params() {
                insert_range!(block_param, min_id);
            }
        }
        for &inst in node
            .insts()
            .keys()
            .filter(|&&val| can_produce_value(val, data))
        {
            insert_range!(inst, val_alloc.get_id(&inst));
        }
    }

    let unhandled = {
        let mut unsorted = liveness_ranges.keys().cloned().collect::<Vec<_>>();
        unsorted.sort_unstable_by_key(|x| (*liveness_ranges[x].start(), *liveness_ranges[x].end()));
        unsorted.into_iter()
    };

    let mut register_allocation = HashMap::new();
    let mut curr_inst_offset = extra_args * 4;

    let mut active: Vec<(std::ops::RangeInclusive<usize>, VRegister, Inst)> =
        Vec::with_capacity(REGISTER_COUNT);
    for val in unhandled {
        let new_range = liveness_ranges.get(&val).unwrap();
        let remove_partition =
            active.partition_point(|(range, _reg, _val)| range.end() < new_range.start());
        for (_, reg, _) in active.drain(0..remove_partition) {
            register_manager.free(reg);
        }

        macro_rules! active_insert {
            ($new_range:expr, $register:expr, $value: expr) => {
                let insert_idx =
                    active.partition_point(|(range, _reg, _val)| range.end() <= $new_range.end());
                active.insert(insert_idx, ($new_range.clone(), $register, $value));
            };
        }

        macro_rules! alloc_stack {
            ($val: expr) => {
                register_allocation.insert($val, AllocationState::Stack(curr_inst_offset));
                curr_inst_offset += crate::ir2riscv::inst_size(data, $val);
            };
        }

        if let InstKind::Alloc = data.dfg().value(val).kind() {
            alloc_stack!(val);
            continue;
        }

        if let Some(alloc) = register_manager.alloc(new_range) {
            active_insert!(new_range, alloc, val);
            register_allocation.insert(val, AllocationState::Register(alloc.0));
        } else {
            eprintln!("Spill!");
            if let Some((index, (occupied_range, occupied_reg, occupied_val))) = active
                .iter()
                .rev()
                .find_position(|&&(_, reg, _)| register_manager.check(reg, new_range))
            {
                if occupied_range.end() > new_range.end() {
                    // spill the occupied one
                    alloc_stack!(*occupied_val);
                    register_allocation.insert(val, AllocationState::Register(occupied_reg.0));
                    let alloc_reg = *occupied_reg;
                    let actual_index = active.len() - index - 1;
                    active.remove(actual_index);
                    active_insert!(new_range, alloc_reg, val);
                } else {
                    // spill the current one
                    alloc_stack!(val);
                }
            } else {
                // spill the current one
                alloc_stack!(val);
            }
        }
    }

    curr_inst_offset -= extra_args * 4;

    let offset = {
        let unaligned = curr_inst_offset
            + if call_ra { 4 } else { 0 }
            + extra_args * 4
            + register_manager.callee_used.len() * 4;
        if unaligned & 0x0F != 0 {
            (unaligned | 0x0F) + 1
        } else {
            unaligned
        }
    };

    for (index, &fparam) in data.params().iter().skip(8).enumerate() {
        register_allocation.insert(fparam, AllocationState::Stack(offset + 4 * index));
    }
    eprintln!("function name:{:?}", data.name());

    eprintln!("--------------------------------");

    for (k, v) in liveness_ranges.iter() {
        eprintln!("{:?} {:?}", k, v);
    }

    for (k, v) in register_allocation.iter() {
        eprintln!("{:?} {:?}", k, v);
    }

    eprintln!("--------------------------------");

    RegisterAllocationResult {
        allocation: register_allocation,
        offset,
        call_ra,
        extra_args,
        callee_usage: register_manager.callee_used,
    }
}

fn can_produce_value(val: Inst, data: &FunctionData) -> bool {
    matches!(
        data.dfg().value(val).kind(),
        InstKind::FuncArgRef(..)
            | InstKind::BlockArgRef(..)
            | InstKind::Alloc
            | InstKind::Load(..)
            | InstKind::GetPtr(..)
            | InstKind::GetElemPtr(..)
            | InstKind::Binary(..)
            | InstKind::Call(..)
    )
}

// TODO:
// last use at 100 and start at 100 is ok.
// but what about other situation like call.
// 2026-02-17 23:18:
// answer is not.
// we need to introduce Input/Output to tell the difference.
// we can ingore this case and consider it for collision for now.

#[cfg(test)]
mod test {
    use crate::backend::armv8::codegen::register_alloc::is_ranges_intersect;

    #[test]
    fn intersect() {
        assert!(is_ranges_intersect(&(0..=5), &(4..=10)));
        assert!(!is_ranges_intersect(&(0..=5), &(9..=10)));
        assert!(!is_ranges_intersect(&(10..=20), &(4..=9)));
        assert!(is_ranges_intersect(&(0..=5), &(2..=3)));
        assert!(is_ranges_intersect(&(2..=4), &(1..=5)));
        assert!(is_ranges_intersect(&(4..=10), &(0..=5)));
        assert!(is_ranges_intersect(&(4..=104), &(100..=100)));
        assert!(is_ranges_intersect(&(4..=8), &(8..=100)));
    }
}
