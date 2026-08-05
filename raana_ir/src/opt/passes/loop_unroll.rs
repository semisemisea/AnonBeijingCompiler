use std::sync::{Arc, Mutex};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    ir::remap::EntityMapper,
    opt::{
        analysis_passes::{
            induction_variable::{
                BasicInductionVariableAnalysis, constant_trip_count, normalize_strict_exit,
            },
            loop_analysis::{Loop, LoopAnalysis},
        },
        config::LoopUnrollMode,
        prelude::*,
        stats::{LoopUnrollEvent, LoopUnrollOutcome, LoopUnrollRejectReason, PassesRunStats},
        utils::{
            cfg::CFG,
            logical_edge::{LogicalEdge, incoming_edges, outgoing_edges},
        },
    },
};

const MAX_FULL_UNROLL_TRIPS: usize = 8;
// Small exact loops often expose an outer exact loop after the first unroll.
// Keep enough room for two-dimensional stencil kernels (for example 5 x 5),
// while the trip-count cap prevents this budget from applying to long loops.
const MAX_UNROLLED_NON_TERMINATORS: usize = 1_536;

pub struct LoopUnroll {
    mode: LoopUnrollMode,
    stats: Arc<Mutex<PassesRunStats>>,
    collect_stats: bool,
    observed: FxHashMap<(Function, BasicBlock), ObservedLoop>,
}

impl LoopUnroll {
    pub fn new(
        mode: LoopUnrollMode,
        stats: Arc<Mutex<PassesRunStats>>,
        collect_stats: bool,
    ) -> Self {
        Self {
            mode,
            stats,
            collect_stats,
            observed: FxHashMap::default(),
        }
    }
}

#[derive(Debug, Clone)]
struct UnrollCandidate {
    header: BasicBlock,
    blocks: Vec<BasicBlock>,
    latch: BasicBlock,
    exit: BasicBlock,
    continue_edge: LogicalEdge,
    backedge: LogicalEdge,
    exit_edge: LogicalEdge,
    trip_count: usize,
    loop_values: FxHashSet<Inst>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloneError {
    MissingLoopValue(Inst),
}

#[derive(Debug, Clone)]
struct CandidateRejection {
    reason: LoopUnrollRejectReason,
    trip_count: Option<usize>,
    header_size: usize,
    body_size: usize,
    projected_size: Option<usize>,
    shape_candidate: bool,
    exact_trip_candidate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ObservedLoop {
    outcome: LoopUnrollOutcome,
    trip_count: Option<usize>,
    body_size: usize,
    projected_size: Option<usize>,
    shape_candidate: bool,
    exact_trip_candidate: bool,
}

impl CandidateRejection {
    fn new(reason: LoopUnrollRejectReason) -> Self {
        Self {
            reason,
            trip_count: None,
            header_size: 0,
            body_size: 0,
            projected_size: None,
            shape_candidate: false,
            exact_trip_candidate: false,
        }
    }
}

impl Pass for LoopUnroll {
    fn run_on(&mut self, data: &mut ArenaContextMut<'_>) -> bool {
        if data.layout().is_decl() {
            return false;
        }
        if self.collect_stats {
            self.stats.lock().unwrap().loop_unroll.pass_invocations += 1;
        }
        let mut changed = false;
        loop {
            let Some(cfg) = CFG::new(data) else {
                return changed;
            };
            if cfg.is_acyclic() {
                return changed;
            }
            let (cfg, _dom_tree, loops) = LoopAnalysis::from_cfg(cfg);
            let ivs = BasicInductionVariableAnalysis::new(data, &cfg, &loops);
            let mut candidate = None;
            for looop in loops.loops() {
                match analyze_candidate(data, &cfg, &loops, &ivs, looop) {
                    Ok(found) => {
                        let outcome = match self.mode {
                            LoopUnrollMode::Enabled => LoopUnrollOutcome::Applied,
                            LoopUnrollMode::DryRun => LoopUnrollOutcome::WouldApply,
                            LoopUnrollMode::Disabled => unreachable!(),
                        };
                        self.record(
                            data,
                            looop,
                            outcome,
                            Some(found.trip_count),
                            non_terminators(data, found.header).len(),
                            body_size(data, &found),
                            Some(projected_size(data, &found).unwrap()),
                            true,
                            true,
                        );
                        if self.mode == LoopUnrollMode::Enabled {
                            candidate = Some(found);
                            break;
                        }
                    }
                    Err(rejection) => self.record(
                        data,
                        looop,
                        LoopUnrollOutcome::Rejected(rejection.reason),
                        rejection.trip_count,
                        rejection.header_size,
                        rejection.body_size,
                        rejection.projected_size,
                        rejection.shape_candidate,
                        rejection.exact_trip_candidate,
                    ),
                }
            }
            if self.mode == LoopUnrollMode::DryRun {
                return changed;
            }
            let Some(candidate) = candidate else {
                return changed;
            };
            apply_candidate(data, &candidate);
            changed = true;
        }
    }
}

impl LoopUnroll {
    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        data: &ArenaContextMut<'_>,
        looop: &Loop,
        outcome: LoopUnrollOutcome,
        trip_count: Option<usize>,
        header_size: usize,
        body_size: usize,
        projected_size: Option<usize>,
        shape_candidate: bool,
        exact_trip_candidate: bool,
    ) {
        if !self.collect_stats {
            return;
        }
        let key = (data.curr_func.unwrap(), looop.header());
        let current = ObservedLoop {
            outcome,
            trip_count,
            body_size,
            projected_size,
            shape_candidate,
            exact_trip_candidate,
        };
        let previous = self.observed.insert(key, current);
        let mut run_stats = self.stats.lock().unwrap();
        let stats = &mut run_stats.loop_unroll;
        stats.loop_observations += 1;
        if previous.is_none() {
            stats.unique_loops_seen += 1;
        } else if previous == Some(current) {
            return;
        }
        if let Some(previous) = previous {
            remove_observation(stats, previous);
        }
        add_observation(stats, current);
        match outcome {
            LoopUnrollOutcome::Applied
            | LoopUnrollOutcome::WouldApply
            | LoopUnrollOutcome::Rejected(_) => {}
        }
        stats.events.push(LoopUnrollEvent {
            function: data.name().to_owned(),
            header: data.bb_data(looop.header()).name().to_owned(),
            outcome,
            trip_count,
            header_size,
            body_size,
            projected_size,
        });
    }
}

fn add_observation(stats: &mut crate::opt::stats::LoopUnrollStats, observation: ObservedLoop) {
    if observation.shape_candidate {
        stats.shape_candidates += 1;
    }
    if observation.exact_trip_candidate {
        stats.exact_trip_candidates += 1;
    }
    if let Some(trip_count) = observation.trip_count {
        *stats.trip_count_histogram.entry(trip_count).or_default() += 1;
    }
    *stats
        .body_size_histogram
        .entry(observation.body_size)
        .or_default() += 1;
    if let Some(projected_size) = observation.projected_size {
        *stats
            .projected_size_histogram
            .entry(projected_size)
            .or_default() += 1;
    }
    match observation.outcome {
        LoopUnrollOutcome::Applied => {
            stats.applied += 1;
            if let Some(trip_count) = observation.trip_count {
                *stats
                    .accepted_trip_count_histogram
                    .entry(trip_count)
                    .or_default() += 1;
            }
        }
        LoopUnrollOutcome::WouldApply => {
            stats.would_apply += 1;
            if let Some(trip_count) = observation.trip_count {
                *stats
                    .accepted_trip_count_histogram
                    .entry(trip_count)
                    .or_default() += 1;
            }
        }
        LoopUnrollOutcome::Rejected(reason) => {
            *stats.reject_reasons.entry(reason).or_default() += 1;
        }
    }
}

fn remove_observation(stats: &mut crate::opt::stats::LoopUnrollStats, observation: ObservedLoop) {
    if observation.shape_candidate {
        stats.shape_candidates -= 1;
    }
    if observation.exact_trip_candidate {
        stats.exact_trip_candidates -= 1;
    }
    if let Some(trip_count) = observation.trip_count {
        decrement_histogram(&mut stats.trip_count_histogram, trip_count);
    }
    decrement_histogram(&mut stats.body_size_histogram, observation.body_size);
    if let Some(projected_size) = observation.projected_size {
        decrement_histogram(&mut stats.projected_size_histogram, projected_size);
    }
    match observation.outcome {
        LoopUnrollOutcome::Applied => {
            stats.applied -= 1;
            if let Some(trip_count) = observation.trip_count {
                decrement_histogram(&mut stats.accepted_trip_count_histogram, trip_count);
            }
        }
        LoopUnrollOutcome::WouldApply => {
            stats.would_apply -= 1;
            if let Some(trip_count) = observation.trip_count {
                decrement_histogram(&mut stats.accepted_trip_count_histogram, trip_count);
            }
        }
        LoopUnrollOutcome::Rejected(reason) => {
            let count = stats.reject_reasons.get_mut(&reason).unwrap();
            *count -= 1;
            if *count == 0 {
                stats.reject_reasons.remove(&reason);
            }
        }
    }
}

fn decrement_histogram(histogram: &mut std::collections::BTreeMap<usize, u64>, key: usize) {
    let count = histogram.get_mut(&key).unwrap();
    *count -= 1;
    if *count == 0 {
        histogram.remove(&key);
    }
}

fn analyze_candidate(
    data: &ArenaContextMut<'_>,
    cfg: &CFG,
    loops: &LoopAnalysis,
    ivs: &BasicInductionVariableAnalysis,
    looop: &Loop,
) -> Result<UnrollCandidate, CandidateRejection> {
    let header = looop.header();
    if Some(header) == data.layout().entry_bb().map(|block| block.bb()) {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::HeaderIsEntry,
        ));
    }
    if looop.body().len() < 2 || looop.latches().len() != 1 {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::UnsupportedLoopShape,
        ));
    }
    let latch = looop.latches()[0];
    if latch == header || !looop.contains(latch) {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::UnsupportedLoopShape,
        ));
    }
    if loops
        .loops()
        .iter()
        .any(|nested| nested.header() != header && looop.contains(nested.header()))
    {
        return Err(CandidateRejection::new(LoopUnrollRejectReason::NestedLoop));
    }

    let header_edges = outgoing_edges(data, header);
    if header_edges.len() != 2 {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::UnsupportedHeaderEdges,
        ));
    }
    let continue_edges = header_edges
        .iter()
        .copied()
        .filter(|edge| looop.contains(edge.target(data)))
        .collect::<Vec<_>>();
    let exit_edges = header_edges
        .iter()
        .copied()
        .filter(|edge| !looop.contains(edge.target(data)))
        .collect::<Vec<_>>();
    let [continue_edge] = continue_edges.as_slice() else {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::UnsupportedHeaderEdges,
        ));
    };
    let [exit_edge] = exit_edges.as_slice() else {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::UnsupportedHeaderEdges,
        ));
    };
    let exit = exit_edge.target(data);

    let latch_edges = outgoing_edges(data, latch);
    let [backedge] = latch_edges.as_slice() else {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::UnsupportedBackedge,
        ));
    };
    if backedge.target(data) != header {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::UnsupportedBackedge,
        ));
    }

    let incoming = incoming_edges(data, cfg, header);
    if incoming.len() != 2 {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::NonCanonicalEntry,
        ));
    }
    let entries = incoming
        .iter()
        .copied()
        .filter(|edge| !looop.contains(edge.source()))
        .collect::<Vec<_>>();
    let backedges = incoming
        .iter()
        .copied()
        .filter(|edge| looop.contains(edge.source()))
        .collect::<Vec<_>>();
    let [entry_edge] = entries.as_slice() else {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::NonCanonicalEntry,
        ));
    };
    let [incoming_backedge] = backedges.as_slice() else {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::NonCanonicalEntry,
        ));
    };
    if incoming_backedge != backedge || looop.get_preheader(cfg) != Some(entry_edge.source()) {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::NonCanonicalEntry,
        ));
    }
    if entry_edge.args(data).len() != data.bb_data(header).params().len()
        || backedge.args(data).len() != data.bb_data(header).params().len()
    {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::EdgeArgumentMismatch,
        ));
    }

    let variables = ivs.for_loop(looop);
    if variables.is_empty() {
        return Err(CandidateRejection {
            shape_candidate: true,
            ..CandidateRejection::new(LoopUnrollRejectReason::NoBasicInductionVariable)
        });
    }
    let exits = variables
        .iter()
        .filter_map(|iv| normalize_strict_exit(data, looop, iv).map(|exit| (iv, exit)))
        .collect::<Vec<_>>();
    if exits.is_empty() {
        return Err(CandidateRejection {
            shape_candidate: true,
            ..CandidateRejection::new(LoopUnrollRejectReason::NoSupportedStrictExit)
        });
    }
    let trip_count = exits
        .into_iter()
        .find_map(|(iv, exit)| constant_trip_count(data, iv, exit).map(|trip| trip.iterations()))
        .ok_or_else(|| CandidateRejection {
            shape_candidate: true,
            ..CandidateRejection::new(LoopUnrollRejectReason::NonConstantTripCount)
        })?;
    let blocks = data
        .layout()
        .basicblocks()
        .iter()
        .map(|block| block.bb())
        .filter(|&block| block != header && looop.contains(block))
        .collect::<Vec<_>>();
    if blocks.is_empty() || !blocks.contains(&continue_edge.target(data)) {
        return Err(CandidateRejection::new(
            LoopUnrollRejectReason::UnsupportedLoopShape,
        ));
    }
    // Full unrolling can clone arbitrary internal control flow, but only when the
    // header owns the sole loop exit and the latch owns the sole backedge.
    for &block in &blocks {
        for edge in outgoing_edges(data, block) {
            if block == latch && edge == *backedge {
                continue;
            }
            if !looop.contains(edge.target(data)) {
                return Err(CandidateRejection::new(
                    LoopUnrollRejectReason::UnsupportedLoopShape,
                ));
            }
        }
    }
    let header_insts = non_terminators(data, header);
    let body_insts = blocks
        .iter()
        .flat_map(|&block| non_terminators(data, block))
        .collect::<Vec<_>>();
    let total = header_insts
        .len()
        .checked_mul(trip_count.saturating_add(1))
        .and_then(|header| {
            body_insts
                .len()
                .checked_mul(trip_count)
                .and_then(|body| header.checked_add(body))
        })
        .ok_or_else(|| CandidateRejection {
            trip_count: Some(trip_count),
            header_size: header_insts.len(),
            body_size: body_insts.len(),
            shape_candidate: true,
            exact_trip_candidate: true,
            ..CandidateRejection::new(LoopUnrollRejectReason::ProjectedSizeTooLarge)
        })?;
    if trip_count > MAX_FULL_UNROLL_TRIPS {
        return Err(CandidateRejection {
            trip_count: Some(trip_count),
            header_size: header_insts.len(),
            body_size: body_insts.len(),
            projected_size: Some(total),
            shape_candidate: true,
            exact_trip_candidate: true,
            ..CandidateRejection::new(LoopUnrollRejectReason::TripCountTooLarge)
        });
    }
    if total > MAX_UNROLLED_NON_TERMINATORS {
        return Err(CandidateRejection {
            trip_count: Some(trip_count),
            header_size: header_insts.len(),
            body_size: body_insts.len(),
            projected_size: Some(total),
            shape_candidate: true,
            exact_trip_candidate: true,
            ..CandidateRejection::new(LoopUnrollRejectReason::ProjectedSizeTooLarge)
        });
    }
    if header_insts
        .iter()
        .chain(&body_insts)
        .any(|&inst| matches!(data.inst_data(inst).kind(), InstKind::Alloc))
    {
        return Err(CandidateRejection {
            trip_count: Some(trip_count),
            header_size: header_insts.len(),
            body_size: body_insts.len(),
            projected_size: Some(total),
            shape_candidate: true,
            exact_trip_candidate: true,
            ..CandidateRejection::new(LoopUnrollRejectReason::ContainsAlloc)
        });
    }

    let header_values = data
        .bb_data(header)
        .params()
        .iter()
        .copied()
        .chain(data.layout().basicblock(header).insts().iter().copied())
        .collect::<FxHashSet<_>>();
    let body_values = blocks
        .iter()
        .flat_map(|&block| {
            data.bb_data(block)
                .params()
                .iter()
                .copied()
                .chain(data.layout().basicblock(block).insts().iter().copied())
        })
        .collect::<FxHashSet<_>>();
    if body_values.iter().any(|&value| {
        data.inst_data(value).used_by().iter().any(|&user| {
            data.layout()
                .parent_bb(user)
                .is_none_or(|block| !looop.contains(block))
        })
    }) {
        return Err(CandidateRejection {
            trip_count: Some(trip_count),
            header_size: header_insts.len(),
            body_size: body_insts.len(),
            projected_size: Some(total),
            shape_candidate: true,
            exact_trip_candidate: true,
            ..CandidateRejection::new(LoopUnrollRejectReason::BodyValueEscapesLoop)
        });
    }
    let loop_values = header_values.union(&body_values).copied().collect();

    Ok(UnrollCandidate {
        header,
        blocks,
        latch,
        exit,
        continue_edge: *continue_edge,
        backedge: *backedge,
        exit_edge: *exit_edge,
        trip_count,
        loop_values,
    })
}

fn projected_size(data: &FunctionData, candidate: &UnrollCandidate) -> Option<usize> {
    non_terminators(data, candidate.header)
        .len()
        .checked_mul(candidate.trip_count.checked_add(1)?)?
        .checked_add(body_size(data, candidate).checked_mul(candidate.trip_count)?)
}

fn body_size(data: &FunctionData, candidate: &UnrollCandidate) -> usize {
    candidate
        .blocks
        .iter()
        .map(|&block| non_terminators(data, block).len())
        .sum()
}

fn apply_candidate(data: &mut ArenaContextMut<'_>, candidate: &UnrollCandidate) {
    let header_source = non_terminators(data, candidate.header);
    let header_params = data.bb_data(candidate.header).params().to_vec();
    let backedge_args = candidate.backedge.args(data).to_vec();
    let exit_args = candidate.exit_edge.args(data).to_vec();
    let continue_target = candidate.continue_edge.target(data);
    let continue_args = candidate.continue_edge.args(data).to_vec();

    if candidate.trip_count == 0 {
        let values = header_params
            .iter()
            .copied()
            .map(|param| (param, param))
            .collect();
        let final_args = remap_values(&exit_args, &values, &candidate.loop_values)
            .expect("zero-trip exit arguments must be available in the original header");
        data.replace_inst_with(data.layout().basicblock(candidate.header).terminator())
            .jump(candidate.exit, final_args);
        return;
    }

    let mut current_values = FxHashMap::default();
    for &param in &header_params {
        current_values.insert(param, param);
    }
    for &inst in &header_source {
        current_values.insert(inst, inst);
    }
    for &block in &candidate.blocks {
        for &param in data.bb_data(block).params() {
            current_values.insert(param, param);
        }
        for &inst in data.layout().basicblock(block).insts() {
            current_values.insert(inst, inst);
        }
    }
    let enter_args = remap_values(&continue_args, &current_values, &candidate.loop_values)
        .expect("validated continue arguments must be remappable");
    data.replace_inst_with(data.layout().basicblock(candidate.header).terminator())
        .jump(continue_target, enter_args);

    let mut anchor = *candidate.blocks.last().unwrap();
    let mut current_latch = candidate.latch;
    for iteration in 0..candidate.trip_count {
        let next_header = new_header(data, candidate.header, iteration + 1, &mut anchor);
        let next_params = data.bb_data(next_header).params().to_vec();
        let next_args = remap_values(&backedge_args, &current_values, &candidate.loop_values)
            .expect("validated backedge arguments must be remappable");
        data.replace_inst_with(data.layout().basicblock(current_latch).terminator())
            .jump(next_header, next_args);

        let mut next_values = header_params
            .iter()
            .copied()
            .zip(next_params.iter().copied())
            .collect::<FxHashMap<_, _>>();
        clone_non_terminators(
            data,
            &header_source,
            next_header,
            &mut next_values,
            &candidate.loop_values,
        )
        .expect("validated header instructions must be remappable");

        if iteration + 1 == candidate.trip_count {
            let final_args = remap_values(&exit_args, &next_values, &candidate.loop_values)
                .expect("validated exit arguments must be remappable");
            remap_external_header_uses(
                data,
                candidate,
                &header_params,
                &header_source,
                &next_values,
            );
            let jump = data.new_local_value().jump(candidate.exit, final_args);
            data.layout_mut().insert_inst(next_header, jump);
            break;
        }

        let block_map = clone_region(
            data,
            &candidate.blocks,
            iteration + 1,
            &mut anchor,
            &mut next_values,
            &candidate.loop_values,
        )
        .expect("validated loop region must be remappable");
        let enter_args = remap_values(&continue_args, &next_values, &candidate.loop_values)
            .expect("validated continue arguments must be remappable");
        let enter_body = data
            .new_local_value()
            .jump(block_map[&continue_target], enter_args);
        data.layout_mut().insert_inst(next_header, enter_body);
        current_latch = block_map[&candidate.latch];
        current_values = next_values;
    }
}

fn remap_external_header_uses(
    data: &mut ArenaContextMut<'_>,
    candidate: &UnrollCandidate,
    header_params: &[Inst],
    header_source: &[Inst],
    final_values: &FxHashMap<Inst, Inst>,
) {
    let mut users = FxHashSet::default();
    for &value in header_params.iter().chain(header_source) {
        users.extend(
            data.inst_data(value)
                .used_by()
                .iter()
                .copied()
                .filter(|&user| {
                    data.layout().parent_bb(user).is_some_and(|block| {
                        block != candidate.header && !candidate.blocks.contains(&block)
                    })
                }),
        );
    }
    let mut mapper = IterationMapper {
        values: final_values,
        loop_values: &candidate.loop_values,
        blocks: None,
    };
    let rewrites = users
        .into_iter()
        .map(|user| {
            let mapped = data
                .inst_data(user)
                .remap_refs(&mut mapper)
                .expect("validated external header use must be remappable");
            (user, mapped)
        })
        .collect::<Vec<_>>();
    for (user, mapped) in rewrites {
        data.replace_inst_with(user).raw(mapped);
    }
}

fn new_header(
    data: &mut ArenaContextMut<'_>,
    source: BasicBlock,
    iteration: usize,
    anchor: &mut BasicBlock,
) -> BasicBlock {
    let name = format!("{}_unroll_{iteration}", data.bb_data(source).name());
    let param_types = data
        .bb_data(source)
        .params()
        .iter()
        .map(|&param| data.inst_data(param).ty().clone())
        .collect();
    let block = data.new_basic_block().basic_block(name, param_types);
    data.layout_mut().insert_bb_after(*anchor, block);
    *anchor = block;
    block
}

fn clone_region(
    data: &mut ArenaContextMut<'_>,
    sources: &[BasicBlock],
    iteration: usize,
    anchor: &mut BasicBlock,
    values: &mut FxHashMap<Inst, Inst>,
    loop_values: &FxHashSet<Inst>,
) -> Result<FxHashMap<BasicBlock, BasicBlock>, CloneError> {
    let mut blocks = FxHashMap::default();
    for &source in sources {
        let name = format!("{}_unroll_{iteration}", data.bb_data(source).name());
        let param_types = data
            .bb_data(source)
            .params()
            .iter()
            .map(|&param| data.inst_data(param).ty().clone())
            .collect();
        let block = data.new_basic_block().basic_block(name, param_types);
        data.layout_mut().insert_bb_after(*anchor, block);
        *anchor = block;
        blocks.insert(source, block);
        for (&source_param, &cloned_param) in data
            .bb_data(source)
            .params()
            .iter()
            .zip(data.bb_data(block).params())
        {
            values.insert(source_param, cloned_param);
        }
    }
    for &source in sources {
        let insts = data
            .layout()
            .basicblock(source)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for inst in insts {
            let ty = data.inst_data(inst).ty().clone();
            let shell = data.new_local_value().undef(ty);
            values.insert(inst, shell);
        }
    }
    let mut mapper = IterationMapper {
        values,
        loop_values,
        blocks: Some(&blocks),
    };
    for &source in sources {
        let destination = blocks[&source];
        let insts = data
            .layout()
            .basicblock(source)
            .insts()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for inst in insts {
            let mapped = data.inst_data(inst).remap_refs(&mut mapper)?;
            let cloned = mapper.values[&inst];
            data.replace_inst_with(cloned).raw(mapped);
            data.layout_mut().insert_inst(destination, cloned);
        }
    }
    Ok(blocks)
}

fn clone_non_terminators(
    data: &mut ArenaContextMut<'_>,
    source: &[Inst],
    destination: BasicBlock,
    values: &mut FxHashMap<Inst, Inst>,
    loop_values: &FxHashSet<Inst>,
) -> Result<(), CloneError> {
    for &inst in source {
        let ty = data.inst_data(inst).ty().clone();
        let shell = data.new_local_value().undef(ty);
        values.insert(inst, shell);
    }
    let mut mapper = IterationMapper {
        values,
        loop_values,
        blocks: None,
    };
    for &inst in source {
        let mapped = data.inst_data(inst).remap_refs(&mut mapper)?;
        let cloned = mapper.values[&inst];
        data.replace_inst_with(cloned).raw(mapped);
        data.layout_mut().insert_inst(destination, cloned);
    }
    Ok(())
}

fn remap_values(
    source: &[Inst],
    values: &FxHashMap<Inst, Inst>,
    loop_values: &FxHashSet<Inst>,
) -> Result<Vec<Inst>, CloneError> {
    source
        .iter()
        .map(|&value| map_value(value, values, loop_values))
        .collect()
}

fn map_value(
    value: Inst,
    values: &FxHashMap<Inst, Inst>,
    loop_values: &FxHashSet<Inst>,
) -> Result<Inst, CloneError> {
    if value.is_global() {
        return Ok(value);
    }
    if let Some(&mapped) = values.get(&value) {
        return Ok(mapped);
    }
    if loop_values.contains(&value) {
        return Err(CloneError::MissingLoopValue(value));
    }
    Ok(value)
}

struct IterationMapper<'a> {
    values: &'a FxHashMap<Inst, Inst>,
    loop_values: &'a FxHashSet<Inst>,
    blocks: Option<&'a FxHashMap<BasicBlock, BasicBlock>>,
}

impl EntityMapper for IterationMapper<'_> {
    type Error = CloneError;

    fn map_inst(&mut self, inst: Inst) -> Result<Inst, Self::Error> {
        map_value(inst, self.values, self.loop_values)
    }

    fn map_block(&mut self, block: BasicBlock) -> Result<BasicBlock, Self::Error> {
        Ok(self
            .blocks
            .and_then(|blocks| blocks.get(&block).copied())
            .unwrap_or(block))
    }
}

fn non_terminators(data: &FunctionData, block: BasicBlock) -> Vec<Inst> {
    let insts = data.layout().basicblock(block).insts();
    insts
        .iter()
        .copied()
        .take(insts.len().saturating_sub(1))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opt::utils::logical_edge::outgoing_edges;

    struct Fixture {
        program: Program,
        function: Function,
        header: BasicBlock,
        body: BasicBlock,
        exit: BasicBlock,
    }

    fn fixture(initial: i32, bound: i32, step: i32) -> Fixture {
        let mut program = Program::new();
        let function = program.new_function(Type::get_i32(), "unroll".into(), vec![]);
        let (header, body, exit) = {
            let data = program.func_data_mut(function);
            let entry = data.add_entry_block();
            let header = data
                .new_basic_block()
                .basic_block("header".into(), vec![Type::get_i32(), Type::get_i32()]);
            let body = data.new_basic_block().basic_block("body".into(), vec![]);
            let exit = data
                .new_basic_block()
                .basic_block("exit".into(), vec![Type::get_i32()]);
            for block in [header, body, exit] {
                data.layout_mut().push_bb_back(block);
            }

            let initial = data.new_local_value().integer(initial);
            let zero = data.new_local_value().integer(0);
            let entry_jump = data.new_local_value().jump(header, vec![initial, zero]);
            data.layout_mut().insert_inst(entry, entry_jump);

            let params = data.bb_data(header).params().to_vec();
            let iv = params[0];
            let acc = params[1];
            let bound = data.new_local_value().integer(bound);
            let compare = if step > 0 {
                data.new_local_value().binary(BinaryOp::Lt, iv, bound)
            } else {
                data.new_local_value().binary(BinaryOp::Gt, iv, bound)
            };
            data.layout_mut().insert_inst(header, compare);
            let branch = data
                .new_local_value()
                .branch(compare, body, vec![], exit, vec![acc]);
            data.layout_mut().insert_inst(header, branch);

            let next_acc = data.new_local_value().binary(BinaryOp::Add, acc, iv);
            let step_value = data.new_local_value().integer(step.unsigned_abs() as i32);
            let next_iv = if step > 0 {
                data.new_local_value().binary(BinaryOp::Add, iv, step_value)
            } else {
                data.new_local_value().binary(BinaryOp::Sub, iv, step_value)
            };
            for inst in [next_acc, next_iv] {
                data.layout_mut().insert_inst(body, inst);
            }
            let backedge = data.new_local_value().jump(header, vec![next_iv, next_acc]);
            data.layout_mut().insert_inst(body, backedge);

            let result = data.bb_data(exit).params()[0];
            let ret = data.new_local_value().ret(Some(result));
            data.layout_mut().insert_inst(exit, ret);
            (header, body, exit)
        };
        Fixture {
            program,
            function,
            header,
            body,
            exit,
        }
    }

    fn run(fixture: &mut Fixture) -> bool {
        let mut data = ArenaContextMut {
            program: &mut fixture.program,
            curr_func: Some(fixture.function),
        };
        LoopUnroll::new(
            LoopUnrollMode::Enabled,
            Arc::new(Mutex::new(PassesRunStats::default())),
            false,
        )
        .run_on(&mut data)
    }

    fn assert_edge_arguments_well_typed(data: &FunctionData) {
        let cfg = CFG::new(data).unwrap();
        for &source in cfg.blocks() {
            for edge in outgoing_edges(data, source) {
                let params = data.bb_data(edge.target(data)).params();
                let args = edge.args(data);
                assert_eq!(args.len(), params.len());
                for (&arg, &param) in args.iter().zip(params) {
                    assert_eq!(data.inst_data(arg).ty(), data.inst_data(param).ty());
                }
            }
        }
    }

    #[test]
    fn fully_unrolls_a_small_forward_loop_and_is_idempotent() {
        let mut fixture = fixture(0, 4, 1);
        assert!(run(&mut fixture));
        assert!(!run(&mut fixture));

        let data = fixture.program.func_data(fixture.function);
        let (_cfg, _dom, loops) = LoopAnalysis::new(data);
        assert!(loops.loops().is_empty());
        assert_edge_arguments_well_typed(data);
        let header_blocks = data
            .layout()
            .basicblocks()
            .iter()
            .filter(|block| data.bb_data(block.bb()).name().contains("header"))
            .count();
        let body_blocks = data
            .layout()
            .basicblocks()
            .iter()
            .filter(|block| data.bb_data(block.bb()).name().contains("body"))
            .count();
        assert_eq!(header_blocks, 5);
        assert_eq!(body_blocks, 4);

        let final_header = data
            .layout()
            .basicblocks()
            .iter()
            .find(|block| {
                let terminator = block.terminator();
                matches!(data.inst_data(terminator).kind(), InstKind::Jump(jump)
                    if jump.target() == fixture.exit)
            })
            .unwrap()
            .bb();
        let final_params = data.bb_data(final_header).params();
        let InstKind::Jump(exit_jump) = data
            .inst_data(data.layout().basicblock(final_header).terminator())
            .kind()
        else {
            unreachable!();
        };
        assert_eq!(exit_jump.args(), [final_params[1]]);
    }

    #[test]
    fn preserves_the_final_failed_header_visit_for_zero_trip_loops() {
        let mut fixture = fixture(0, 0, 1);
        assert!(run(&mut fixture));
        let data = fixture.program.func_data(fixture.function);
        let InstKind::Jump(jump) = data
            .inst_data(data.layout().basicblock(fixture.header).terminator())
            .kind()
        else {
            panic!("zero-trip header must jump directly to exit");
        };
        assert_eq!(jump.target(), fixture.exit);
        assert_eq!(jump.args().len(), 1);
        let (_cfg, _dom, loops) = LoopAnalysis::new(data);
        assert!(loops.loops().is_empty());
        assert_edge_arguments_well_typed(data);
    }

    #[test]
    fn unrolls_backward_and_non_unit_loops() {
        for (initial, bound, step, expected_headers) in [(7, 0, -2, 5), (0, 7, 2, 5)] {
            let mut fixture = fixture(initial, bound, step);
            assert!(run(&mut fixture));
            let data = fixture.program.func_data(fixture.function);
            let headers = data
                .layout()
                .basicblocks()
                .iter()
                .filter(|block| data.bb_data(block.bb()).name().contains("header"))
                .count();
            assert_eq!(headers, expected_headers);
            assert_edge_arguments_well_typed(data);
        }
    }

    #[test]
    fn rewrites_external_header_parameter_uses_to_the_final_iteration() {
        let mut fixture = fixture(0, 4, 1);
        let original_acc = fixture
            .program
            .func_data(fixture.function)
            .bb_data(fixture.header)
            .params()[1];
        let ret = fixture
            .program
            .func_data(fixture.function)
            .layout()
            .basicblock(fixture.exit)
            .terminator();
        fixture
            .program
            .func_data_mut(fixture.function)
            .replace_inst_with(ret)
            .ret(Some(original_acc));

        assert!(run(&mut fixture));
        let data = fixture.program.func_data(fixture.function);
        let InstKind::Return(ret) = data.inst_data(ret).kind() else {
            unreachable!();
        };
        let final_value = ret.value().unwrap();
        assert_ne!(final_value, original_acc);
        let final_header = data.layout().parent_bb(final_value).or_else(|| {
            data.layout().basicblocks().iter().find_map(|block| {
                data.bb_data(block.bb())
                    .params()
                    .contains(&final_value)
                    .then_some(block.bb())
            })
        });
        assert!(final_header.is_some());
        assert!(
            data.bb_data(final_header.unwrap())
                .name()
                .contains("unroll_4")
        );
    }

    #[test]
    fn rejects_large_trip_counts() {
        let mut large = fixture(0, 9, 1);
        assert!(!run(&mut large));
    }

    #[test]
    fn clones_body_block_parameters() {
        let mut parameterized = fixture(0, 4, 1);
        let function = parameterized.function;
        let header = parameterized.header;
        let body = parameterized.body;
        {
            let data = parameterized.program.func_data_mut(function);
            let _parameter = data.new_basic_block().add_param(body, Type::get_i32());
            let zero = data.new_local_value().integer(0);
            let terminator = data.layout().basicblock(header).terminator();
            let InstKind::Branch(branch) = data.inst_data(terminator).kind() else {
                unreachable!();
            };
            let cond = branch.cond();
            let false_target = branch.f_target();
            let false_args = branch.f_args().to_vec();
            data.replace_inst_with(terminator).branch(
                cond,
                body,
                vec![zero],
                false_target,
                false_args,
            );
        }
        assert!(run(&mut parameterized));
        let data = parameterized.program.func_data(function);
        assert_edge_arguments_well_typed(data);
        let (_cfg, _dom, loops) = LoopAnalysis::new(data);
        assert!(loops.loops().is_empty());
    }
}
