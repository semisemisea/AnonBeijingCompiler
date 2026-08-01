# Block CFG And Loop Canonicalization

## CFG Identity And Scope

- New control-flow analyses use real `BasicBlock` handles through the
  data-driven CFG in `raana_ir/src/opt/utils/cfg.rs`.
- The data-driven CFG contains only blocks reachable from the function entry.
  Unreachable layout blocks do not participate in dominance or loop analysis.
- Legacy dense block IDs remain implementation details of legacy clients such
  as SSA. New APIs and transformations must not expose or mix those IDs with
  the data-driven CFG.
- A reachable block ends in exactly one terminator, has no earlier terminator,
  and targets only blocks present in the same function layout.

## Structural And Logical Edges

- A structural CFG edge is a unique `(source, target)` pair used for graph
  traversal, dominance, and natural-loop discovery.
- A logical edge is one `Jump` edge or one arm of a `Branch`, including its own
  positional argument vector.
- If both branch arms target the same block, the data-driven CFG contains one
  structural edge but the terminator still contains two logical edges. Phi
  analysis and CFG rewrites must inspect and preserve both arms independently.
- Block reverse-use sets contain user instructions, not logical-edge
  occurrences. One branch that targets a block twice therefore appears once in
  the target block's `used_by` set.
- Shared logical-edge code lives in `opt::utils::logical_edge`. Edge enumeration
  returns separate `Jump`, `True`, and `False` arms, while
  `LogicalEdgeRewriter` groups edits by terminator and applies each terminator
  replacement once through the instruction builders.

## Block Parameters And Phi Values

- Phi values are represented by target block parameters and positional
  arguments on incoming logical edges.
- Argument `i` on a logical edge supplies target parameter `i`. Rewrites must
  preserve argument count, type, and position on every affected logical edge.
- Parameter positions are determined from the target block's `params()` slice.
  `BlockArgRef::index()` is not a supported source of truth.
- Function arguments are the entry block parameters recorded by
  `FunctionData::params()`. Ordinary loop canonicalization must not replace the
  entry block or desynchronize those values.
- Adding or removing a block parameter is incomplete until every incoming
  logical edge has been rewritten consistently.

## CFG Mutation

- Arena instruction data, instruction/block reverse-use links, and layout
  ownership are coupled state. CFG rewrites must update all three through
  builders and layout helpers rather than mutating one layer directly.
- When both arms of one branch need changes, collect both edits and replace the
  branch once. Replacing one arm and then consulting stale branch data is not a
  valid rewrite strategy.
- Layout order is not execution order. Semantic reasoning follows CFG edges;
  layout placement is a separate code-layout decision.
- `Layout::insert_bb_before` inserts an empty block and updates the block layout
  reverse index. It does not create edges or move instructions. Inserting before
  the current entry changes the function entry and therefore requires explicit
  function-parameter migration by the caller.
- CFG, dominance, natural-loop, and induction-variable analyses are snapshots.
  Any change to blocks, terminators, targets, logical-edge arguments, or
  reachability invalidates all dependent snapshots.

## Natural Loops

- Natural loops are discovered from dominance backedges. A loop may have one
  or multiple latches; a latch is a loop block with a backedge to the header.
- Self-loops are valid natural loops. Reducibility and laminar nesting are
  prerequisites of the natural-loop model.
- A preheader is a block outside the loop that is the header's only structural
  predecessor from outside the loop and has the header as its only structural
  successor.
- Recognition of an existing preheader and creation of a preheader are separate
  operations.

## Canonicalization Policy

- Canonicalize only the form required by a concrete consumer. Do not create
  preheaders, dedicated latches, or dedicated exits globally without a user.
- Preheader creation redirects every outside logical edge to the new preheader.
  Backedges remain directed to the original loop header.
- A general preheader has parameters matching the header parameters in type and
  order. Each outside edge passes its original header arguments to the
  preheader, and the preheader forwards its own parameters to the header. This
  preserves distinct initial phi values from different outside edges.
- Same-target branch arms are redirected independently, even when both arms
  enter the same header and carry different arguments.
- A created preheader contains a terminating jump before a consumer inserts
  computations with `insert_before_terminator`.
- Entry-header loops are not rewritten by ordinary preheader creation because
  replacing the function entry also requires an explicit function-parameter
  migration.
- Canonicalization must be idempotent. Reapplying it to an already suitable
  loop reports no change.

## Simplification Interaction

- `SimplifyCFG` may remove a parameterless block whose only instruction is an
  argumentless jump. Such a block is not preserved merely because another pass
  intended it to be a preheader.
- Canonical blocks carry no permanent name-based or marker-based protection.
  Consumers create them on demand and give them an immediate semantic use.
- A pass must not create an unused trivial preheader on every fixed-point
  iteration, because simplification can remove it and cause non-convergence.
- Simplification that redirects or removes a block must preserve every logical
  branch arm and must not leave a terminator targeting a removed block.

## Verification

- Focused tests must cover critical entry edges, multiple outside predecessors,
  distinct incoming phi values, same-target branch arms, multiple latches,
  self-loops, nested loops, entry-header rejection, and idempotence.
- Run CFG verification and rebuild dominance/loop analyses after each tested CFG
  rewrite.
- Optimizer end-to-end tests must run with `-O 1` and inspect emitted RaanaIR or
  assembly to prove that canonicalization and its consumer both executed.
