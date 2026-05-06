use crate::opt::prelude::*;

pub fn idom(prece: &CFGGraph, rpo: &[BId]) -> IDomMap {
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
    debug!("rpo before panic: {:?}", rpo);
    let mut rpo_idx = vec![0; rpo.len()];
    for (i, &id) in rpo.iter().enumerate() {
        rpo_idx[id] = i;
    }

    map[0] = 0;

    let mut converged = false;
    while !converged {
        converged = true;
        for node in &rpo[1..] {
            let mut it = prece[node].iter();
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

#[must_use]
pub fn build_dominance_tree(idom_map: &IDomMap, rpo_len: usize) -> DomTree {
    let mut ret = vec![vec![]; rpo_len];
    // INFO: remember that idom_map we make `idom_map[0] = 0`
    // that is not allowed in a tree (no loop or ring)
    for (vid, &pa) in idom_map.iter().enumerate().skip(1) {
        ret[pa].push(vid);
    }
    ret
}
