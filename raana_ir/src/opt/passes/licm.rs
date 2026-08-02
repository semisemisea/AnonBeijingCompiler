//! Loop-invariant code motion (LICM).
//!
//! Hoists pure invariant instructions (arithmetic, logical, comparisons,
//! casts) out of natural loops into the preheader. Deliberately conservative
//! (`宁漏勿错`): only single-preheader loops whose header dominates the back
//! edge are considered, and only instructions with provably invariant
//! operands move. Block parameters are treated as invariant only when every
//! incoming edge passes the same instruction.
//!
//! After GSP the loop-carried global loads are already SSA values, so the
//! remaining hoist candidates are pure expressions over loop-invariant
//! operands (clang's `elaborate_licm_hoist` equivalent).

use std::collections::{HashMap, HashSet, VecDeque};

use crate::ir::inst_kind;
use crate::opt::prelude::*;

pub struct Licm;

impl Pass for Licm {
    fn run_on(&self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().entry_bb().is_none() {
            return false;
        }
        let mut changed = false;
        let mut bb_id = utils::IDAllocator::new(1);
        let (graph, prece) = cfg::build_cfg_both(data, &mut bb_id);
        let rpo_path = cfg::rpo_path(&graph);
        let idom_map = dom_tree::idom(&prece, &rpo_path);
        let dominators = dom_tree::build_dominance_tree(&idom_map, rpo_path.len());

        // Natural loop discovery: back edges (M -> H with H dominating M).
        // `prece[h]` lists H's predecessors, so the edge M->H is a back
        // edge exactly when H dominates M.
        let mut back_edges: Vec<(usize, usize)> = Vec::new();
        for h in 0..bb_id.cnt() {
            if let Some(preds) = prece.get(&h) {
                for &m in preds {
                    if m != h && dominates(h, m, &dominators) {
                        back_edges.push((m, h));
                    }
                }
            }
        }

        for (m, h) in back_edges {
            let loop_blocks = loop_body(&graph, &prece, m, h, &dominators);
            let Some(preheader) = preheader(data, &bb_id, &graph, &prece, h, &loop_blocks) else {
                continue;
            };
            if hoist_loop(data, &bb_id, &graph, &prece, h, &loop_blocks, preheader, &dominators) {
                changed = true;
            }
        }
        changed
    }
}

/// Whether `h` dominates `m`, by walking `m`'s idom chain (idom stored as
/// the parent in the dominator tree).
fn dominates(h: usize, m: usize, tree: &DomTree) -> bool {
    let mut runner = m;
    loop {
        if runner == h {
            return true;
        }
        // tree[i] lists children; the parent must be recovered by search.
        let parent = tree
            .iter()
            .enumerate()
            .find(|(_, children)| children.contains(&runner))
            .map(|(parent, _)| parent);
        match parent {
            Some(p) if p != runner => runner = p,
            _ => return runner == h,
        }
    }
}

/// The natural loop of back edge (M -> H): every block dominated by H that
/// can reach M.
fn loop_body(
    graph: &CFGGraph,
    prece: &CFGGraph,
    m: usize,
    h: usize,
    tree: &DomTree,
) -> HashSet<usize> {
    let mut body = HashSet::new();
    body.insert(h);
    body.insert(m);
    // Backward worklist from M: blocks that can reach M, restricted to
    // blocks dominated by H.
    let mut work_queue = VecDeque::from([m]);
    while let Some(bb) = work_queue.pop_front() {
        if let Some(preds) = prece.get(&bb) {
            for &pred in preds {
                if dominates(h, pred, tree) && body.insert(pred) {
                    work_queue.push_back(pred);
                }
            }
        }
    }
    let _ = graph;
    body
}

/// A unique non-loop predecessor of the header.
fn preheader(
    data: &ArenaContextMut<'_>,
    bb_id: &utils::IDAllocator<BasicBlock, usize>,
    graph: &CFGGraph,
    prece: &CFGGraph,
    h: usize,
    loop_blocks: &HashSet<usize>,
) -> Option<BasicBlock> {
    let _ = (graph, bb_id);
    let non_loop_preds: Vec<usize> = prece
        .get(&h)
        .into_iter()
        .flatten()
        .copied()
        .filter(|p| !loop_blocks.contains(p))
        .collect();
    if non_loop_preds.len() != 1 {
        return None;
    }
    Some(bb_id.search_id(non_loop_preds[0]))
}

/// Hoist invariant pure instructions from the loop into the preheader.
fn hoist_loop(
    data: &mut ArenaContextMut<'_>,
    bb_id: &utils::IDAllocator<BasicBlock, usize>,
    graph: &CFGGraph,
    prece: &CFGGraph,
    h: usize,
    loop_blocks: &HashSet<usize>,
    preheader: BasicBlock,
    tree: &DomTree,
) -> bool {
    let _ = (bb_id, graph, prece, tree);
    let h_bb = bb_id.search_id(h);
    // Define outside the loop: the function entry always dominates the loop,
    // so any definition not inside a loop block is available.
    // `invariant` maps an instruction (or block parameter) to the value that
    // must be substituted for it when hoisted: parameters resolve to their
    // invariant incoming argument, everything else to itself.
    let mut invariant: HashMap<Inst, Inst> = HashMap::new();
    let mut moved: Vec<(Inst, BasicBlock)> = Vec::new();

    // Collect all loop instructions in block order, with the block's params
    // treated specially (a param is invariant only when every incoming edge
    // passes the same invariant instruction).
    let mut insts_in_loop: Vec<(BasicBlock, Inst)> = Vec::new();
    for layout in data.layout().basicblocks() {
        let Some(bid) = bb_id.get_id_safe(&layout.bb()) else {
            continue;
        };
        if !loop_blocks.contains(&bid) {
            continue;
        }
        for &inst in layout.insts() {
            insts_in_loop.push((layout.bb(), inst));
        }
    }

    loop {
        let mut progressed = false;
        for (bb, inst) in &insts_in_loop {
            if invariant.contains_key(inst) {
                continue;
            }
            if !pure_kind(data, *inst) {
                continue;
            }
            if operands_invariant(data, *inst, bb, h_bb, loop_blocks, &mut invariant, bb_id) {
                invariant.insert(*inst, *inst);
                moved.push((*inst, *bb));
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }

    if moved.is_empty() {
        return false;
    }

    let preheader_terminator = data.layout().basicblock(preheader).terminator();
    for (inst, bb) in moved {
        // Rewrite parameters to their invariant incoming values so the
        // hoisted instruction no longer references values defined only on
        // the loop's edges.
        let replacement = rewrite_operands(data, inst, &invariant);
        utils::visit_and_replace(data, inst, replacement);
        data.layout_mut().remove_inst(bb, inst);
        data.layout_mut().insert_inst_before(preheader_terminator, replacement);
    }
    true
}

/// Rewrite an instruction's operands through the invariant map (parameters
/// become their incoming argument) and return the rewritten instruction.
fn rewrite_operands(
    data: &mut ArenaContextMut<'_>,
    inst: Inst,
    invariant: &HashMap<Inst, Inst>,
) -> Inst {
    let ty = data.inst_data(inst).ty().clone();
    match data.inst_data(inst).kind() {
        InstKind::Binary(binary) => {
            let op = binary.op();
            let lhs = *invariant.get(&binary.lhs()).unwrap_or(&binary.lhs());
            let rhs = *invariant.get(&binary.rhs()).unwrap_or(&binary.rhs());
            builder(data).insert_inst(inst_kind::Binary::new_data(lhs, rhs, op, ty))
        }
        InstKind::Cast(cast) => {
            let src = *invariant.get(&cast.src()).unwrap_or(&cast.src());
            builder(data).insert_inst(inst_kind::Cast::new_data(src, ty))
        }
        _ => inst,
    }
}

/// A local-instruction builder whose arena can also address global
/// instructions (needed only for the rewritten insts).
fn builder<'a>(data: &'a mut ArenaContextMut<'_>) -> crate::ir::builder::LocalBuilder<'a> {
    crate::ir::builder::LocalBuilder { arena: data }
}

fn pure_kind(data: &ArenaContextMut<'_>, inst: Inst) -> bool {
    match data.inst_data(inst).kind() {
        InstKind::Binary(..) | InstKind::Cast(..) => true,
        _ => false,
    }
}

fn operands_invariant(
    data: &ArenaContextMut<'_>,
    inst: Inst,
    bb: &BasicBlock,
    h_bb: BasicBlock,
    loop_blocks: &HashSet<usize>,
    invariant: &mut HashMap<Inst, Inst>,
    bb_id: &utils::IDAllocator<BasicBlock, usize>,
) -> bool {
    let operands: Vec<Inst> = match data.inst_data(inst).kind() {
        InstKind::Binary(binary) => vec![binary.lhs(), binary.rhs()],
        InstKind::Cast(cast) => vec![cast.src()],
        _ => return false,
    };
    for operand in operands {
        if data.inst_data(operand).kind().is_const() {
            continue;
        }
        if invariant.contains_key(&operand) {
            continue;
        }
        // A block parameter: invariant only when every incoming edge passes
        // the same instruction and that instruction is invariant or defined
        // outside the loop. The substitution is recorded so hoisted
        // instructions reference the incoming value instead of the param.
        if let InstKind::BlockArgRef(..) = data.inst_data(operand).kind() {
            let Some(param_block) = block_of_param(data, operand) else {
                return false;
            };
            let _ = (bb, h_bb);
            if param_block == *bb {
                let mut common: Option<Inst> = None;
                let mut all_same = true;
                for &pred_inst in data.bb_data(param_block).used_by() {
                    let args = match data.inst_data(pred_inst).kind() {
                        InstKind::Jump(jump) => jump.args().to_vec(),
                        InstKind::Branch(branch) => {
                            let mut a = branch.t_args().to_vec();
                            a.extend(branch.f_args().to_vec());
                            a
                        }
                        _ => return false,
                    };
                    // The param's index within its block.
                    let idx = data.bb_data(param_block).params().iter().position(|&p| p == operand);
                    let Some(idx) = idx else { return false };
                    let Some(&arg) = args.get(idx) else { return false };
                    match common {
                        None => common = Some(arg),
                        Some(c) if c == arg => {}
                        _ => all_same = false,
                    }
                }
                let Some(arg) = common else { return false };
                if all_same
                    && (invariant.contains_key(&arg)
                        || data.inst_data(arg).kind().is_const()
                        || defined_outside_loop(data, arg, loop_blocks, bb_id))
                {
                    invariant.insert(operand, arg);
                    continue;
                }
                return false;
            }
        }
        if defined_outside_loop(data, operand, loop_blocks, bb_id) {
            continue;
        }
        return false;
    }
    true
}

fn defined_outside_loop(
    data: &ArenaContextMut<'_>,
    inst: Inst,
    loop_blocks: &HashSet<usize>,
    bb_id: &utils::IDAllocator<BasicBlock, usize>,
) -> bool {
    if inst.is_global() {
        return true;
    }
    for layout in data.layout().basicblocks() {
        let Some(bid) = bb_id.get_id_safe(&layout.bb()) else {
            continue;
        };
        if loop_blocks.contains(&bid) {
            continue;
        }
        if layout.insts().iter().any(|i| *i == inst) {
            return true;
        }
    }
    false
}

fn block_of_param(data: &ArenaContextMut<'_>, param: Inst) -> Option<BasicBlock> {
    data.layout()
        .basicblocks()
        .iter()
        .find(|layout| data.bb_data(layout.bb()).params().contains(&param))
        .map(|layout| layout.bb())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// entry: jump header(32); header(n): br (n != 0) body/exit; body:
    /// v = 2 * 3; n' = n - 1; jump header(n') — the multiply of constants is
    /// invariant and must move to the preheader.
    #[test]
    fn hoists_constant_multiply_out_of_loop() {
        let mut program = Program::new();
        let func = program.new_function(Type::get_i32(), "licm".to_owned(), vec![]);
        let (entry, header, body, exit) = {
            let data = program.func_data_mut(func);
            let entry = data
                .new_basic_block()
                .basic_block("entry".to_owned(), vec![]);
            let header = data
                .new_basic_block()
                .basic_block("header".to_owned(), vec![Type::get_i32()]);
            let body = data
                .new_basic_block()
                .basic_block("body".to_owned(), vec![]);
            let exit = data
                .new_basic_block()
                .basic_block("exit".to_owned(), vec![]);
            data.layout_mut().push_bb_back(entry);
            data.layout_mut().push_bb_back(header);
            data.layout_mut().push_bb_back(body);
            data.layout_mut().push_bb_back(exit);

            let init = data.new_local_inst().integer(32);
            let jump = data.new_local_inst().jump(header, vec![init]);
            data.layout_mut().insert_inst(entry, jump);

            let param = data.bb_data(header).params()[0];
            let branch = data.new_local_inst().branch(param, body, vec![], exit, vec![]);
            data.layout_mut().insert_inst(header, branch);

            let two = data.new_local_inst().integer(2);
            let three = data.new_local_inst().integer(3);
            let mul = data.new_local_inst().binary(BinaryOp::Mul, two, three);
            let one = data.new_local_inst().integer(1);
            let dec = data.new_local_inst().binary(BinaryOp::Sub, param, one);
            let back = data.new_local_inst().jump(header, vec![dec]);
            data.layout_mut().insert_inst(body, mul);
            data.layout_mut().insert_inst(body, back);

            let ret = data.new_local_inst().ret(None);
            data.layout_mut().insert_inst(exit, ret);

            (entry, header, body, exit)
        };
        let mut data = ArenaContextMut {
            program: &mut program,
            curr_func: Some(func),
        };
        assert!(Licm.run_on(&mut data));
        // The multiply moved into the preheader (entry), before the jump.
        let entry_insts: Vec<Inst> = data
            .layout()
            .basicblock(entry)
            .insts()
            .iter()
            .copied()
            .collect();
        assert!(
            entry_insts
                .iter()
                .any(|&i| matches!(data.inst_data(i).kind(), InstKind::Binary(b) if b.op() == BinaryOp::Mul)),
            "multiply must be hoisted to the preheader"
        );
        let body_insts: Vec<Inst> = data
            .layout()
            .basicblock(body)
            .insts()
            .iter()
            .copied()
            .collect();
        assert!(
            !body_insts
                .iter()
                .any(|&i| matches!(data.inst_data(i).kind(), InstKind::Binary(b) if b.op() == BinaryOp::Mul)),
            "multiply must leave the loop body"
        );
    }
}
