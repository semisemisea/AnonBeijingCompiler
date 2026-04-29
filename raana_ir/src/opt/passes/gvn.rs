use std::collections::HashMap;

use crate::ir::{BinaryOp, Function, FunctionData, Inst, InstKind};

use crate::opt::pass::Pass;

use crate::opt::utils::{self, BIDAlloc, BId, DomTree, IDAllocator, VIDAlloc};

pub struct GlobalInstNumbering;

type InstNumber = usize;

#[derive(Debug, Hash, Clone, Copy, PartialEq, Eq)]
enum InstType {
    Const(i32),
    // in the SSA-form, everything is constant.
    // Here variable means: We don't know the value at comptime.
    Var(InstNumber),
    Binary {
        op: BinaryOp,
        lhs: InstNumber,
        rhs: InstNumber,
    },
    GetPtr {
        source: InstNumber,
        index: InstNumber,
    },
    GetElemPtr {
        source: InstNumber,
        index: InstNumber,
    },
    // Call can also be seen as the same in certain condition.
    // This can be identified through:
    // - If the callee is pure function
    // - If the arguments are all the same.
    // But in simplified GVN pass, we will ingnore this

    // Call {
    //     callee: koopa::ir::Function,
    //     args: Vec<InstNumber>,
    // },
}

fn is_op_commutative(op: &BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Add
            | BinaryOp::Mul
            | BinaryOp::NotEq
            | BinaryOp::And
            | BinaryOp::Or
            | BinaryOp::Xor
            | BinaryOp::Eq
    )
}

impl InstType {
    fn build_from_value(
        data: &FunctionData,
        value: Inst,
        val_id: &mut VIDAlloc,
    ) -> Option<InstType> {
        match data.dfg().value(value).kind() {
            InstKind::Integer(integer) => Some(Self::Const(integer.value())),
            InstKind::Float(float) => todo!(),
            InstKind::Load(..)
            | InstKind::Call(..)
            | InstKind::Alloc
            | InstKind::BlockArgRef(..)
            | InstKind::FuncArgRef(..)
            | InstKind::Aggregate(..)
            | InstKind::Undef
            | InstKind::ZeroInit => Some(Self::Var(val_id.check_or_alloc_id_same(value))),
            InstKind::GlobalAlloc(_global_alloc) => unreachable!(),
            InstKind::Store(_store) => None,
            InstKind::GetPtr(get_ptr) => Some(Self::GetPtr {
                source: val_id.check_or_alloc_id_same(get_ptr.src()),
                index: val_id.check_or_alloc_id_same(get_ptr.index()),
            }),
            InstKind::GetElemPtr(get_elem_ptr) => Some(Self::GetElemPtr {
                source: val_id.check_or_alloc_id_same(get_elem_ptr.src()),
                index: val_id.check_or_alloc_id_same(get_elem_ptr.index()),
            }),
            InstKind::Binary(binary) => {
                let lhs = val_id.check_or_alloc_id_same(binary.lhs());
                let rhs = val_id.check_or_alloc_id_same(binary.rhs());
                if is_op_commutative(binary.op()) {
                    Some(Self::Binary {
                        op: binary.op().clone(),
                        lhs: lhs.min(rhs),
                        rhs: rhs.max(lhs),
                    })
                } else {
                    Some(Self::Binary {
                        op: binary.op().clone(),
                        lhs,
                        rhs,
                    })
                }
            }
            InstKind::Return(..) | InstKind::Jump(..) | InstKind::Branch(..) => None,
        }
    }
}

type Map = HashMap<InstType, Inst>;

struct LayeredMap(Vec<Map>);

impl LayeredMap {
    #[inline]
    fn new() -> LayeredMap {
        LayeredMap(Default::default())
    }

    #[inline]
    fn new_scope(&mut self) {
        self.0.push(HashMap::default())
    }

    #[inline]
    fn pop_scope(&mut self) -> Option<Map> {
        self.0.pop()
    }

    fn get(&self, key: &InstType) -> Option<Inst> {
        self.0
            .iter()
            .rev()
            .find_map(|scope| scope.get(key).copied())
    }

    fn insert(&mut self, key: InstType, value: Inst) -> Option<Inst> {
        self.0.last_mut().unwrap().insert(key, value)
    }
}

impl Pass for GlobalInstNumbering {
    fn run_on(&mut self, _func: Function, data: &mut FunctionData) {
        // function declaration. we just have to skip it.
        if data.layout().entry_bb().is_none() {
            return;
        }
        eprintln!("----------------------------------------------------");
        eprintln!("gvn start: {:?}", data.name());

        let mut bb_alloc = IDAllocator::new(1);
        let mut val_alloc = IDAllocator::new(1);
        let mut layered_type_map = LayeredMap::new();
        let (graph, prece) = utils::build_cfg_both(data, &mut bb_alloc);

        // entry bb must be the first to be allocated.
        assert!(bb_alloc.get_id(&data.layout().entry_bb().unwrap().bb()) == 0);

        let rpo_path = utils::rpo_path(&graph);
        let idom_map = utils::idom(&prece, &rpo_path);
        let donimnace_tree = utils::build_dominance_tree(&idom_map, rpo_path.len());

        fn dfs(
            bb_id: BId,
            dom_tree: &DomTree,
            layered_type_map: &mut LayeredMap,
            val_alloc: &mut VIDAlloc,
            bb_alloc: &mut BIDAlloc,
            data: &mut FunctionData,
        ) {
            layered_type_map.new_scope();
            let bb = bb_alloc.search_id(bb_id);

            let mut to_replace = Vec::new();
            let iter = data
                .dfg()
                .bb(bb)
                .params()
                .iter()
                .chain(data.layout().bbs().node(&bb).unwrap().insts().keys())
                .copied();

            for val in iter {
                if let Some(expr) = InstType::build_from_value(data, val, val_alloc) {
                    eprintln!("epxr: {:?}", expr);
                    if let Some(rep_with) = layered_type_map.get(&expr) {
                        to_replace.push((val, rep_with))
                    } else {
                        layered_type_map.insert(expr, val);
                    }
                }
            }

            for (rep, rep_with) in to_replace {
                utils::visit_and_replace(data, rep, rep_with);
            }

            dom_tree[bb_id].iter().for_each(|&child| {
                dfs(child, dom_tree, layered_type_map, val_alloc, bb_alloc, data)
            });

            layered_type_map.pop_scope();
        }
        dfs(
            0,
            &donimnace_tree,
            &mut layered_type_map,
            &mut val_alloc,
            &mut bb_alloc,
            data,
        );

        eprintln!("----------------------------------------------------");
    }
}
