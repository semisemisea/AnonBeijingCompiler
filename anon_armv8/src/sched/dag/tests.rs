use taki_mir::{abi::ArgPair, register::Writable};

use super::*;
use crate::{
    instructions::{Cond, Imm12, MemoryType},
    regs::{OperandSize, RegOrZr, int_reg},
};

fn writable(index: u8) -> Writable<Reg> {
    Writable::from_reg(int_reg(index))
}

fn args_deps() -> InstDeps {
    inst_deps(&MInst::Args {
        args: vec![
            ArgPair {
                vreg: writable(0),
                preg: int_reg(0),
            },
            ArgPair {
                vreg: writable(1),
                preg: int_reg(1),
            },
        ],
    })
}

#[test]
fn args_pseudo_is_a_free_nop_with_register_defs() {
    let deps = args_deps();
    assert_eq!(deps.class, SchedClass::Nop);
    assert!(
        deps.defs
            .iter()
            .all(|p| matches!(*p, p if p.hw_enc() == 0 || p.hw_enc() == 1))
    );
    assert!(deps.uses.is_empty());
    assert!(!deps.flags_def && !deps.flags_use);
    assert!(deps.mem.is_none());
    assert!(!deps.is_barrier);
}

#[test]
fn args_defs_keep_argument_readers_after_the_pseudo() {
    let insts = vec![
        MInst::Args {
            args: vec![ArgPair {
                vreg: writable(0),
                preg: int_reg(0),
            }],
        },
        MInst::AluRRR {
            op: crate::instructions::AluOp::Add,
            size: OperandSize::Size64,
            dst: writable(2),
            lhs: RegOrZr::Reg(int_reg(0)),
            rhs: RegOrZr::Reg(int_reg(3)),
        },
    ];
    let graph = DepGraph::build(&insts);
    assert!(has_edge(&graph, 0, 1));
    assert!(!has_edge(&graph, 1, 0));
}

#[test]
fn retval_is_a_free_nop_that_reads_the_return_value() {
    let deps = inst_deps(&MInst::RetVal {
        pair: taki_mir::abi::RetPair {
            vreg: int_reg(0),
            preg: int_reg(0),
        },
    });
    assert_eq!(deps.class, SchedClass::Nop);
    assert_eq!(deps.uses, vec![int_preg(0)]);
    assert!(deps.defs.is_empty());
    assert!(!deps.is_barrier);
}

fn has_edge(graph: &DepGraph, from: usize, to: usize) -> bool {
    graph.succs[from].iter().any(|edge| edge.node == to)
}

#[test]
fn records_edge_kind_statistics() {
    use crate::instructions::{AluOp, MemoryType};

    let writable = |index| Writable::from_reg(int_reg(index));
    let insts = vec![
        MInst::Load {
            ty: MemoryType::I64,
            dst: writable(1),
            addr: crate::instructions::AMode::Reg {
                base: crate::regs::stack_reg(),
            },
        },
        MInst::AluRRR {
            op: AluOp::Add,
            size: OperandSize::Size64,
            dst: writable(2),
            lhs: RegOrZr::Reg(int_reg(1)),
            rhs: RegOrZr::Reg(int_reg(3)),
        },
        MInst::Mov {
            size: OperandSize::Size64,
            dst: writable(2),
            src: int_reg(4),
        },
    ];
    let graph = DepGraph::build(&insts);
    let stats = &graph.stats;

    assert_eq!(stats.nodes, 3);
    assert!(stats.edges >= 2);
    assert!(stats.edge_kind_counts[EdgeKind::RegisterRaw as usize] >= 1);
    assert!(stats.edge_kind_counts[EdgeKind::RegisterWaw as usize] >= 1);
    assert_eq!(stats.memory_accesses, 1);
    assert_eq!(stats.known_root_accesses, 1);
    assert_eq!(stats.unknown_root_accesses, 0);
}

#[test]
fn keeps_all_readers_before_a_later_write() {
    let insts = vec![
        MInst::Mov {
            size: OperandSize::Size64,
            dst: writable(1),
            src: int_reg(0),
        },
        MInst::Mov {
            size: OperandSize::Size64,
            dst: writable(2),
            src: int_reg(0),
        },
        MInst::Mov {
            size: OperandSize::Size64,
            dst: writable(0),
            src: int_reg(3),
        },
    ];
    let graph = DepGraph::build(&insts);

    assert!(has_edge(&graph, 0, 2));
    assert!(has_edge(&graph, 1, 2));
}

#[test]
fn keeps_all_loads_before_a_later_store() {
    let addr = |base| crate::instructions::AMode::Reg { base };
    let insts = vec![
        MInst::Load {
            ty: crate::instructions::MemoryType::I64,
            dst: writable(1),
            addr: addr(int_reg(4)),
        },
        MInst::Load {
            ty: crate::instructions::MemoryType::I64,
            dst: writable(2),
            addr: addr(int_reg(5)),
        },
        MInst::Store {
            ty: crate::instructions::MemoryType::I64,
            src: int_reg(3),
            addr: addr(int_reg(6)),
        },
    ];
    let graph = DepGraph::build(&insts);

    assert!(has_edge(&graph, 0, 2));
    assert!(has_edge(&graph, 1, 2));
}

#[test]
fn separates_non_overlapping_stack_ranges() {
    let insts = vec![
        MInst::Store {
            ty: MemoryType::I64,
            src: int_reg(1),
            addr: AMode::SignedOffset {
                base: crate::regs::stack_reg(),
                offset: crate::instructions::SImm9::new(0).unwrap(),
            },
        },
        MInst::Load {
            ty: MemoryType::I64,
            dst: writable(2),
            addr: AMode::SignedOffset {
                base: crate::regs::stack_reg(),
                offset: crate::instructions::SImm9::new(8).unwrap(),
            },
        },
    ];
    let graph = DepGraph::build(&insts);

    assert!(!has_edge(&graph, 0, 1));
}

#[test]
fn preserves_overlapping_stack_ranges() {
    let insts = vec![
        MInst::Store {
            ty: MemoryType::I64,
            src: int_reg(1),
            addr: AMode::SignedOffset {
                base: crate::regs::stack_reg(),
                offset: crate::instructions::SImm9::new(0).unwrap(),
            },
        },
        MInst::Load {
            ty: MemoryType::I32,
            dst: writable(2),
            addr: AMode::SignedOffset {
                base: crate::regs::stack_reg(),
                offset: crate::instructions::SImm9::new(4).unwrap(),
            },
        },
    ];
    let graph = DepGraph::build(&insts);

    assert!(has_edge(&graph, 0, 1));
}

#[test]
fn a_disjoint_store_does_not_hide_an_older_alias() {
    let stack_addr = |offset| AMode::SignedOffset {
        base: crate::regs::stack_reg(),
        offset: crate::instructions::SImm9::new(offset).unwrap(),
    };
    let insts = vec![
        MInst::Store {
            ty: MemoryType::I64,
            src: int_reg(1),
            addr: stack_addr(0),
        },
        MInst::Store {
            ty: MemoryType::I64,
            src: int_reg(2),
            addr: stack_addr(16),
        },
        MInst::Load {
            ty: MemoryType::I64,
            dst: writable(3),
            addr: stack_addr(0),
        },
    ];
    let graph = DepGraph::build(&insts);

    assert!(has_edge(&graph, 0, 2));
    assert!(!has_edge(&graph, 1, 2));
}

#[test]
fn tracks_stack_addresses_through_moves_and_adds() {
    let insts = vec![
        MInst::MovPhys {
            size: OperandSize::Size64,
            dst: writable(4),
            src: crate::regs::stack_reg(),
        },
        MInst::AluRRImm12 {
            op: AluOp::Add,
            size: OperandSize::Size64,
            dst: writable(5),
            src: int_reg(4),
            imm: Imm12::new(16, false).unwrap(),
        },
        MInst::Store {
            ty: MemoryType::I64,
            src: int_reg(1),
            addr: AMode::Reg { base: int_reg(5) },
        },
        MInst::Load {
            ty: MemoryType::I64,
            dst: writable(2),
            addr: AMode::Reg {
                base: crate::regs::stack_reg(),
            },
        },
    ];
    let graph = DepGraph::build(&insts);

    assert!(!has_edge(&graph, 2, 3));
}

#[test]
fn keeps_unknown_and_sp_vs_fp_accesses_ordered() {
    let insts = vec![
        MInst::Store {
            ty: MemoryType::I64,
            src: int_reg(1),
            addr: AMode::Reg { base: int_reg(10) },
        },
        MInst::Load {
            ty: MemoryType::I64,
            dst: writable(2),
            addr: AMode::Reg {
                base: crate::regs::stack_reg(),
            },
        },
        MInst::Store {
            ty: MemoryType::I64,
            src: int_reg(3),
            addr: AMode::Reg {
                base: crate::regs::int_reg(crate::regs::FP),
            },
        },
    ];
    let graph = DepGraph::build(&insts);

    assert!(has_edge(&graph, 0, 1));
    assert!(has_edge(&graph, 1, 2));
}

#[test]
fn pair_ranges_and_writeback_remain_safe() {
    let insts = vec![
        MInst::StorePair {
            ty: MemoryType::I64,
            src1: int_reg(1),
            src2: int_reg(2),
            addr: PairAMode::SignedOffset {
                base: crate::regs::stack_reg(),
                offset: crate::instructions::SImm7Scaled::new(0, 8).unwrap(),
            },
        },
        MInst::Load {
            ty: MemoryType::I64,
            dst: writable(3),
            addr: AMode::SignedOffset {
                base: crate::regs::stack_reg(),
                offset: crate::instructions::SImm9::new(8).unwrap(),
            },
        },
        MInst::Load {
            ty: MemoryType::I64,
            dst: writable(4),
            addr: AMode::SignedOffset {
                base: crate::regs::stack_reg(),
                offset: crate::instructions::SImm9::new(16).unwrap(),
            },
        },
        MInst::StorePair {
            ty: MemoryType::I64,
            src1: int_reg(5),
            src2: int_reg(6),
            addr: PairAMode::PostIndex {
                base: crate::regs::stack_reg(),
                offset: crate::instructions::SImm7Scaled::new(16, 8).unwrap(),
            },
        },
    ];
    let graph = DepGraph::build(&insts);

    assert!(has_edge(&graph, 0, 1));
    assert!(!has_edge(&graph, 0, 2));
    assert!(has_edge(&graph, 1, 3));
    assert!(has_edge(&graph, 2, 3));
}

#[test]
fn register_redefinition_kills_address_provenance() {
    let insts = vec![
        MInst::MovPhys {
            size: OperandSize::Size64,
            dst: writable(4),
            src: crate::regs::stack_reg(),
        },
        MInst::LoadImm {
            size: OperandSize::Size64,
            dst: writable(4),
            value: 0,
        },
        MInst::Store {
            ty: MemoryType::I64,
            src: int_reg(1),
            addr: AMode::Reg { base: int_reg(4) },
        },
        MInst::Load {
            ty: MemoryType::I64,
            dst: writable(2),
            addr: AMode::SignedOffset {
                base: crate::regs::stack_reg(),
                offset: crate::instructions::SImm9::new(32).unwrap(),
            },
        },
    ];
    let graph = DepGraph::build(&insts);

    assert!(has_edge(&graph, 2, 3));
}

#[test]
fn load_destination_is_only_a_definition() {
    let deps = inst_deps(&MInst::Load {
        ty: crate::instructions::MemoryType::I64,
        dst: writable(1),
        addr: crate::instructions::AMode::Reg { base: int_reg(2) },
    });

    assert_eq!(deps.defs, vec![crate::regs::int_preg(1)]);
    assert_eq!(deps.uses, vec![crate::regs::int_preg(2)]);
}

#[test]
fn pair_writeback_defines_its_base_register() {
    let deps = inst_deps(&MInst::StorePair {
        ty: crate::instructions::MemoryType::I64,
        src1: int_reg(1),
        src2: int_reg(2),
        addr: crate::instructions::PairAMode::PostIndex {
            base: int_reg(3),
            offset: crate::instructions::SImm7Scaled::new(16, 8).unwrap(),
        },
    });

    assert_eq!(deps.defs, vec![crate::regs::int_preg(3)]);
    assert!(deps.uses.contains(&crate::regs::int_preg(3)));
}

#[test]
fn models_nzcv_producers_and_consumers() {
    let insts = vec![
        MInst::CmpImm {
            size: OperandSize::Size64,
            lhs: int_reg(0),
            imm: Imm12::new(0, false).unwrap(),
        },
        MInst::CSet {
            cond: Cond::Eq,
            dst: writable(1),
        },
        MInst::CmpRR {
            size: OperandSize::Size64,
            lhs: int_reg(2),
            rhs: RegOrZr::Reg(int_reg(3)),
        },
    ];
    let graph = DepGraph::build(&insts);

    assert!(has_edge(&graph, 0, 1));
    assert!(has_edge(&graph, 1, 2));
}

#[test]
fn barrier_is_ordered_after_the_entire_prefix() {
    let insts = vec![
        MInst::Mov {
            size: OperandSize::Size64,
            dst: writable(1),
            src: int_reg(0),
        },
        MInst::Mov {
            size: OperandSize::Size64,
            dst: writable(2),
            src: int_reg(3),
        },
        MInst::Jump {
            label: crate::labels::Label::from_block(taki_mir::block_order::MirBlockIndex::new(0)),
        },
    ];
    let graph = DepGraph::build(&insts);

    assert!(has_edge(&graph, 0, 2));
    assert!(has_edge(&graph, 1, 2));
}
