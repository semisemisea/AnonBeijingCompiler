use std::collections::{HashMap, HashSet, hash_map::Entry};

use crate::{
    ir::{
        BasicBlock, FunctionData, Inst, InstKind, Type, TypeKind, builder_trait::LocalInstBuilder,
    },
    // opt::{FunctionPass, ModulePass},
};

use itertools::Itertools;

#[derive(Debug)]
pub struct IDAllocator<PKey, Id, NKey = PKey> {
    id_pos: HashMap<PKey, Id>,
    id_neg: HashMap<Id, NKey>,
    cnt: Id,
    increase_by: Id,
}

pub type VIDAlloc = IDAllocator<Inst, VId>;
pub type BIDAlloc = IDAllocator<BasicBlock, BId>;
pub type IDomMap = Vec<BId>;
pub type DomTree = Vec<Vec<BId>>;

impl<PK, I, NK> Default for IDAllocator<PK, I, NK>
where
    I: num_traits::One + num_traits::Zero,
{
    fn default() -> Self {
        IDAllocator::new(I::one())
    }
}

impl<PK, I, NK> IDAllocator<PK, I, NK>
where
    I: num_traits::Zero,
{
    pub fn new(increase_by: I) -> IDAllocator<PK, I, NK> {
        Self {
            id_pos: HashMap::new(),
            id_neg: HashMap::new(),
            cnt: I::zero(),
            increase_by,
        }
    }
}

impl<PK, I, NK> IDAllocator<PK, I, NK>
where
    PK: Eq + std::hash::Hash + Copy,
    I: std::ops::AddAssign<I> + Default + Copy + Eq + std::hash::Hash,
    NK: Eq + std::hash::Hash + Copy,
{
    #[inline]
    pub fn check_or_alloc_id(&mut self, pkey: PK, nkey: NK) -> I {
        match self.id_pos.entry(pkey) {
            Entry::Occupied(e) => *e.get(),
            Entry::Vacant(..) => {
                self.id_pos.insert(pkey, self.cnt);
                self.id_neg.insert(self.cnt, nkey);
                let ret = self.cnt;
                self.cnt += self.increase_by;
                ret
            }
        }
    }

    pub fn get_id_safe(&self, key: &PK) -> Option<&I> {
        self.id_pos.get(key)
    }

    pub fn get_id(&self, key: &PK) -> I {
        *self.id_pos.get(key).unwrap()
    }

    pub fn search_id(&self, id: I) -> NK {
        *self.id_neg.get(&id).unwrap()
    }
}

impl<K, I> IDAllocator<K, I>
where
    K: Eq + std::hash::Hash + Copy,
    I: std::ops::AddAssign<I> + Default + Copy + Eq + std::hash::Hash,
{
    #[inline]
    pub fn check_or_alloc_id_same(&mut self, key: K) -> I {
        self.check_or_alloc_id(key, key)
    }

    pub fn cnt(&self) -> I {
        self.cnt
    }

    // pub fn ids(&self) -> impl Iterator<Item = &I> {
    //     self.id_neg.keys()
    // }

    // pub fn keys(&self) -> impl Iterator<Item = &K> {
    //     self.id_pos.keys()
    // }
}

// Inst ID
pub type VId = usize;

// Basic Block ID
pub type BId = usize;

// Control Flow Graph
// Each Vertex is a basic block
pub type CFGGraph = HashMap<BId, Vec<BId>>;

// TODO: We can do cache there.
// TODO: Do forward/backward seperation to allow further more optimization.
#[inline]
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
        match data.dfg().value(val).kind() {
            InstKind::Jump(jump) => {
                let target_id = bb_alloc.check_or_alloc_id_same(jump.target());

                graph.entry(id).or_default().push(target_id);
                prece.entry(target_id).or_default().push(id);

                dfs(jump.target(), data, bb_alloc, graph, prece, visited);
            }
            InstKind::Branch(branch) => {
                let true_id = bb_alloc.check_or_alloc_id_same(branch.true_bb());

                graph.entry(id).or_default().push(true_id);
                prece.entry(true_id).or_default().push(id);

                dfs(branch.true_bb(), data, bb_alloc, graph, prece, visited);

                let false_id = bb_alloc.check_or_alloc_id_same(branch.false_bb());

                graph.entry(id).or_default().push(false_id);
                prece.entry(false_id).or_default().push(id);

                dfs(branch.false_bb(), data, bb_alloc, graph, prece, visited);
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
        data.layout().entry_bb().unwrap(),
        data,
        bb_alloc,
        &mut graph,
        &mut prece,
        &mut visited,
    );
    (graph, prece)
}

type GPath = Vec<BId>;
type Set = HashSet<BId>;

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
    eprintln!("Graph/Path: {:?} {:?}", g, path);
    path
}

#[inline]
pub fn get_terminator_inst(data: &FunctionData, bb: BasicBlock) -> Inst {
    *data
        .layout()
        .bbs()
        .node(&bb)
        .unwrap()
        .insts()
        .back_key()
        .unwrap()
}

pub fn alloc_ty(val: Inst, data: &FunctionData) -> &Type {
    use TypeKind;
    let val_data = data.dfg().value(val);
    assert!(matches!(val_data.kind(), InstKind::Alloc));
    // alloc should generate a pointer to its target type.
    let TypeKind::Pointer(pointee) = val_data.ty().kind() else {
        unreachable!()
    };
    &pointee
}

pub fn visit_and_replace(data: &mut FunctionData, rep: Inst, rep_with: Inst) {
    let list = data
        .dfg()
        .value(rep)
        .used_by()
        .iter()
        .copied()
        .collect_vec();
    for used_by in list {
        visit_and_replace_single(data, used_by, rep, rep_with);
    }
}

fn visit_and_replace_single(data: &mut FunctionData, used_by: Inst, rep: Inst, rep_with: Inst) {
    let rep_val_data = data.dfg().value(used_by);
    #[allow(unused_variables)]
    match rep_val_data.kind() {
        InstKind::Integer(..)
        | InstKind::ZeroInit
        | InstKind::Undef
        | InstKind::Aggregate(..)
        | InstKind::FuncArgRef(..)
        | InstKind::BlockArgRef(..)
        | InstKind::Alloc
        | InstKind::GlobalAlloc(..) => unreachable!("Encountered kind: {:?}", rep_val_data.kind()),
        InstKind::Float(..) => todo!(),
        InstKind::Load(load) => {
            data.dfg_mut().replace_value_with(used_by).load(rep_with);
        }
        InstKind::Store(store) => {
            if store.value() == rep {
                let dest = store.dest();
                data.dfg_mut()
                    .replace_value_with(used_by)
                    .store(rep_with, dest);
            }
        }
        InstKind::GetPtr(get_ptr) => {
            if get_ptr.index() == rep {
                let src = get_ptr.src();
                data.dfg_mut()
                    .replace_value_with(used_by)
                    .get_ptr(src, rep_with);
            }
        }
        InstKind::GetElemPtr(get_elem_ptr) => {
            if get_elem_ptr.index() == rep {
                let src = get_elem_ptr.src();
                data.dfg_mut()
                    .replace_value_with(used_by)
                    .get_elem_ptr(src, rep_with);
            }
        }
        InstKind::Binary(binary) => {
            let lhs = if binary.lhs() == rep {
                rep_with
            } else {
                binary.lhs()
            };
            eprintln!("lhs: {:?} {:?}", lhs, binary.lhs());
            let rhs = if binary.rhs() == rep {
                rep_with
            } else {
                binary.rhs()
            };
            eprintln!("rhs: {:?} {:?}", rhs, binary.rhs());
            let op = binary.op();
            data.dfg_mut()
                .replace_value_with(used_by)
                .binary(op, lhs, rhs);
        }
        InstKind::Branch(branch) => {
            let cond = if branch.cond() == rep {
                rep_with
            } else {
                branch.cond()
            };
            if let InstKind::Integer(int) = data.dfg().value(cond).kind() {
                let (target, args) = if int.value() == 0 {
                    (branch.false_bb(), branch.false_args().to_vec())
                } else {
                    (branch.true_bb(), branch.true_args().to_vec())
                };
                data.dfg_mut()
                    .replace_value_with(used_by)
                    .jump_with_args(target, args);
            } else {
                let true_args = branch
                    .true_args()
                    .iter()
                    .map(|&val| if val == rep { rep_with } else { val })
                    .collect();
                let false_args = branch
                    .false_args()
                    .iter()
                    .map(|&val| if val == rep { rep_with } else { val })
                    .collect();
                let (true_bb, false_bb) = (branch.true_bb(), branch.false_bb());
                data.dfg_mut()
                    .replace_value_with(used_by)
                    .branch_with_args(cond, true_bb, false_bb, true_args, false_args);
            }
        }
        InstKind::Jump(jump) => {
            let args = jump
                .args()
                .iter()
                .map(|&val| if val == rep { rep_with } else { val })
                .collect();
            let target = jump.target();
            data.dfg_mut()
                .replace_value_with(used_by)
                .jump_with_args(target, args);
        }
        InstKind::Call(call) => {
            let args = call
                .args()
                .iter()
                .map(|&val| if val == rep { rep_with } else { val })
                .collect();
            let callee = call.callee();
            data.dfg_mut()
                .replace_value_with(used_by)
                .call(callee, args);
        }
        InstKind::Return(ret) => {
            if ret.value() == Some(rep) {
                data.dfg_mut()
                    .replace_value_with(used_by)
                    .ret(Some(rep_with));
            }
        }
    }
}

#[must_use]
pub fn build_dominance_tree(idom_map: &IDomMap, rpo_len: usize) -> DomTree {
    let mut ret = vec![vec![]; rpo_len];
    // remember that idom_map we make `idom_map[0] = 0`
    // that is not allowed in a tree (no loop or ring)
    for (vid, &pa) in idom_map.iter().enumerate().skip(1) {
        ret[pa].push(vid);
    }
    ret
}

// #[must_use]
// fn direct_build_dominance_tree(
//     data: &koopa::ir::FunctionData,
//     id_alloc: &mut IDAllocator<BasicBlock, BId>,
// ) -> DomTree {
//     let (graph, prede) = build_cfg_both(data, id_alloc);
//     let rpo = rpo_path(&graph);
//     let idom_map = idom(&prede, &rpo);
//
//     build_dominance_tree(&idom_map, rpo.len())
// }

pub fn idom(prede: &CFGGraph, rpo: &[BId]) -> IDomMap {
    fn lca(n1: BId, n2: BId, map: &IDomMap, rpo_idx: &[BId]) -> BId {
        let mut p1 = n1;
        let mut p2 = n2;
        while p1 != p2 {
            while rpo_idx[p1] > rpo_idx[p2] {
                p1 = map[p1];
            }
            while rpo_idx[p1] < rpo_idx[p2] {
                p2 = map[p2];
            }
        }
        p1
    }

    let mut map = IDomMap::new();
    map.resize(rpo.len(), usize::MAX);
    eprintln!("rpo before panic: {:?}", rpo);
    let mut rpo_idx = vec![0; rpo.len()];
    for (i, &id) in rpo.iter().enumerate() {
        rpo_idx[id] = i;
    }

    map[0] = 0;

    let mut converged = false;
    while !converged {
        converged = true;
        for node in &rpo[1..] {
            let mut it = prede[node].iter();
            let mut new_idom = *it.find(|&&x| map[x] != usize::MAX).unwrap();
            for &other_node in it.filter(|&&x| map[x] != usize::MAX) {
                new_idom = lca(new_idom, other_node, &map, rpo);
            }
            if map[*node] != new_idom {
                map[*node] = new_idom;
                converged = false;
            }
        }
    }
    map
}
