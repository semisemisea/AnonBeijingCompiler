use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet},
    fmt::Debug,
    ops::RangeInclusive,
};

use itertools::Itertools;
use log::debug;
use raana_ir::ir::{BasicBlock, arena::Arena};
use raana_ir::ir::{FunctionData, Inst, InstKind};
use raana_ir::opt::prelude::*;

use crate::backend::armv8::register::{
    Bit, FReg, FloatRegister, IReg, IntRegister, Register, RegisterType,
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
                if virtual_reg.0.is_callee_saved() {
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
    fn new(max_size: usize, ty: RegisterType) -> VirtualRegister {
        let int_scratch = [IntRegister::x16, IntRegister::x17];
        let float_regs = [
            FloatRegister::v0,
            FloatRegister::v1,
            FloatRegister::v2,
            FloatRegister::v3,
            FloatRegister::v4,
            FloatRegister::v5,
            FloatRegister::v6,
            FloatRegister::v7,
            FloatRegister::v8,
            FloatRegister::v9,
            FloatRegister::v10,
            FloatRegister::v11,
            FloatRegister::v12,
            FloatRegister::v13,
            FloatRegister::v14,
            FloatRegister::v15,
            FloatRegister::v18,
            FloatRegister::v19,
            FloatRegister::v20,
            FloatRegister::v21,
            FloatRegister::v22,
            FloatRegister::v23,
            FloatRegister::v24,
            FloatRegister::v25,
        ];
        let container = match ty {
            RegisterType::Int => BinaryHeap::from_iter((0..max_size as u8).filter_map(|x| {
                let reg = IntRegister::try_from(x).unwrap();
                (!int_scratch.contains(&reg)).then_some(Reverse(Register::I(IReg(Bit::b64, reg))))
            })),
            RegisterType::Float => BinaryHeap::from_iter(
                float_regs
                    .into_iter()
                    .take(max_size)
                    .map(|reg| Reverse(Register::F(FReg(Bit::b32, reg)))),
            ),
        };
        Self {
            container,
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
    let (graph, prece) = cfg::build_cfg_both(data, &mut bb_alloc);
    let rpo_path = cfg::rpo_path(&graph);
    let mut int_vregs = VirtualRegister::new(REGISTER_COUNT, RegisterType::Int);
    let mut float_vregs = VirtualRegister::new(REGISTER_COUNT, RegisterType::Float);

    let mut call_ra = false;
    let mut extra_args = 0usize;

    for &fparam in data.params() {
        val_alloc.check_or_alloc_id_same(fparam);
    }
    for &bb_id in rpo_path.iter() {
        let bb: BasicBlock = bb_alloc.search_id(bb_id);
        let insts = data.layout().basicblock(bb).insts();
        let iter = data.bb_data(bb).params().iter().chain(insts);

        for &inst in iter {
            let id = val_alloc.check_or_alloc_id_same(inst);

            if let InstKind::Call(call) = data.inst_data(inst).kind() {
                call_ra = true;
                extra_args = extra_args.max(8.max(call.args().len()) - 8);
                for index in 0..8 {
                    int_vregs.add_rule(Reverse(Register::arguments(index)), id..=id);
                    float_vregs.add_rule(Reverse(Register::float_arguments(index)), id..=id);
                }
                int_vregs.add_rule(
                    Reverse(Register::I(IReg(Bit::b64, IntRegister::x8))),
                    id..=id,
                );
                int_vregs.add_rule(
                    Reverse(Register::I(IReg(Bit::b64, IntRegister::x18))),
                    id..=id,
                );
                for index in 0..7 {
                    // FIXME: we should also consider the size of temporary register.
                    int_vregs.add_rule(Reverse(Register::temporary(index, Bit::b64)), id..=id);
                }
            }
        }
    }

    for &bb_id in rpo_path.iter() {
        let bb: BasicBlock = bb_alloc.search_id(bb_id);
        let terminator_inst = utils::get_terminator_inst(data, bb);

        macro_rules! add_loop {
            ($backedge_goes_to: expr) => {
                if let Some(id) = bb_alloc.get_id_safe(&$backedge_goes_to) {
                    let head_bb = bb_alloc.search_id(*id);
                    let head_bb_first_inst = *data.bb_data(head_bb).params().first().unwrap_or(
                        data.layout()
                            .basicblock(head_bb)
                            .insts()
                            .get_first()
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

                                let curr_insts = data.layout().basicblock(curr_bb).insts();
                                if let Some(last_inst) = curr_insts.get_last() {
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

                            int_vregs.add_loop(*header_id..=max_loop_id);
                            float_vregs.add_loop(*header_id..=max_loop_id);
                        }
                    }
                }
            };
        }

        match data.inst_data(terminator_inst).kind() {
            InstKind::Jump(jump) => {
                add_loop!(jump.target());
            }
            InstKind::Branch(branch) => {
                add_loop!(branch.t_target());
                add_loop!(branch.f_target());
            }
            InstKind::Return(..) => {}
            _ => unreachable!(),
        }
    }
    let mut liveness_ranges = HashMap::new();

    macro_rules! insert_range {
        ($inst:expr, $min_id: expr) => {
            let max_id = data
                .inst_data($inst)
                .used_by()
                .iter()
                .filter_map(|&val| val_alloc.get_id_safe(&val))
                .max()
                .copied()
                .unwrap_or($min_id);
            let register_type = register_type(data, $inst);
            let range = match register_type {
                RegisterType::Int => int_vregs.extent_by_loop($min_id..=max_id),
                RegisterType::Float => float_vregs.extent_by_loop($min_id..=max_id),
            };
            liveness_ranges.insert($inst, range);
        };
    }
    // val_alloc.get_id($inst)

    for &fparam in data.params().iter().take(8) {
        insert_range!(fparam, val_alloc.get_id(&fparam));
    }
    for bb_id in rpo_path {
        let bb: BasicBlock = bb_alloc.search_id(bb_id);
        let insts = data.layout().basicblock(bb).insts();
        debug!("params:{:?}", data.bb_data(bb).params());

        if let Some(min_id) = data
            .bb_data(bb)
            .used_by()
            .iter()
            .map(|&val| val_alloc.get_id(&val))
            .min()
        {
            for &block_param in data.bb_data(bb).params() {
                insert_range!(block_param, min_id);
            }
        }
        for &inst in insts.iter().filter(|&&val| can_produce_value(val, data)) {
            insert_range!(inst, val_alloc.get_id(&inst));
        }
    }

    let unhandled = {
        let mut unsorted = liveness_ranges.keys().cloned().collect::<Vec<_>>();
        unsorted.sort_unstable_by_key(|x| (*liveness_ranges[x].start(), *liveness_ranges[x].end()));
        unsorted.into_iter()
    };

    let mut register_allocation = HashMap::new();
    // FIXME: we should also consider the size of parameter and temporary register.
    // if call_ra, we need to reserve 16 bits for x29 and x30.
    // start from beyond LR.
    let mut acc_inst_offset = extra_args * 8 + if call_ra { 16 } else { 0 };

    let mut int_active: Vec<(std::ops::RangeInclusive<usize>, VRegister, Inst)> =
        Vec::with_capacity(REGISTER_COUNT);
    let mut float_active: Vec<(std::ops::RangeInclusive<usize>, VRegister, Inst)> =
        Vec::with_capacity(REGISTER_COUNT);
    for val in unhandled {
        let new_range = liveness_ranges.get(&val).unwrap();
        let (vregs, active) = match register_type(data, val) {
            RegisterType::Int => (&mut int_vregs, &mut int_active),
            RegisterType::Float => (&mut float_vregs, &mut float_active),
        };
        let remove_partition =
            active.partition_point(|(range, _reg, _val)| range.end() < new_range.start());
        for (_, reg, _) in active.drain(0..remove_partition) {
            vregs.free(reg);
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
                register_allocation.insert($val, AllocationState::Stack(acc_inst_offset));
                acc_inst_offset += crate::backend::armv8::codegen::inst_size(data, $val);
            };
        }

        if let InstKind::Alloc = data.inst_data(val).kind() {
            alloc_stack!(val);
            continue;
        }

        if let Some(alloc) = vregs.alloc(new_range) {
            active_insert!(new_range, alloc, val);
            register_allocation.insert(
                val,
                AllocationState::Register(sized_register(data, val, alloc.0)),
            );
        } else {
            debug!("Spill!");
            if let Some((index, (occupied_range, occupied_reg, occupied_val))) = active
                .iter()
                .rev()
                .find_position(|&&(_, reg, _)| vregs.check(reg, new_range))
            {
                if occupied_range.end() > new_range.end() {
                    // spill the occupied one
                    alloc_stack!(*occupied_val);
                    register_allocation.insert(
                        val,
                        AllocationState::Register(sized_register(data, val, occupied_reg.0)),
                    );
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

    acc_inst_offset -= extra_args * 8;

    let offset = {
        let unaligned = acc_inst_offset
            + if call_ra { 16 } else { 0 }
            + extra_args * 8
            + int_vregs.callee_used.len() * 8
            + float_vregs.callee_used.len() * 8;
        if unaligned & 0x0F != 0 {
            (unaligned | 0x0F) + 1
        } else {
            unaligned
        }
    };

    for (index, &fparam) in data.params().iter().skip(8).enumerate() {
        // FIXME: we should also consider the size of parameter.
        register_allocation.insert(fparam, AllocationState::Stack(offset + 8 * index));
    }
    debug!("function name:{:?}", data.name());
    debug!("liveness ranges:{:?}", liveness_ranges);
    debug!("register allocation:{:?}", register_allocation);

    RegisterAllocationResult {
        allocation: register_allocation,
        offset,
        call_ra,
        extra_args,
        callee_usage: int_vregs
            .callee_used
            .into_iter()
            .chain(float_vregs.callee_used)
            .collect(),
    }
}

fn register_type(data: &FunctionData, val: Inst) -> RegisterType {
    if data.inst_data(val).ty().is_f32() {
        RegisterType::Float
    } else {
        RegisterType::Int
    }
}

fn can_produce_value(val: Inst, data: &FunctionData) -> bool {
    if data.inst_data(val).ty().size() == 0 {
        return false;
    }

    matches!(
        data.inst_data(val).kind(),
        InstKind::FuncArgRef(..)
            | InstKind::BlockArgRef(..)
            | InstKind::Alloc
            | InstKind::Load(..)
            | InstKind::GetPtr(..)
            | InstKind::GetElemPtr(..)
            | InstKind::Binary(..)
            | InstKind::Cast(..)
            | InstKind::Call(..)
    )
}

fn sized_register(data: &FunctionData, val: Inst, register: Register) -> Register {
    register.with_size(Bit::try_from(crate::backend::armv8::codegen::inst_size(data, val)).unwrap())
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
