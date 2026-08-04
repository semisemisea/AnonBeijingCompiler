use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LoopUnrollRejectReason {
    HeaderIsEntry,
    UnsupportedLoopShape,
    NestedLoop,
    BodyHasParameters,
    UnsupportedHeaderEdges,
    UnsupportedBackedge,
    NonCanonicalEntry,
    EdgeArgumentMismatch,
    NoBasicInductionVariable,
    NoSupportedStrictExit,
    NonConstantTripCount,
    TripCountTooLarge,
    ProjectedSizeTooLarge,
    ContainsAlloc,
    BodyValueEscapesLoop,
}

impl LoopUnrollRejectReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HeaderIsEntry => "header_is_entry",
            Self::UnsupportedLoopShape => "unsupported_loop_shape",
            Self::NestedLoop => "nested_loop",
            Self::BodyHasParameters => "body_has_parameters",
            Self::UnsupportedHeaderEdges => "unsupported_header_edges",
            Self::UnsupportedBackedge => "unsupported_backedge",
            Self::NonCanonicalEntry => "non_canonical_entry",
            Self::EdgeArgumentMismatch => "edge_argument_mismatch",
            Self::NoBasicInductionVariable => "no_basic_induction_variable",
            Self::NoSupportedStrictExit => "no_supported_strict_exit",
            Self::NonConstantTripCount => "non_constant_trip_count",
            Self::TripCountTooLarge => "trip_count_too_large",
            Self::ProjectedSizeTooLarge => "projected_size_too_large",
            Self::ContainsAlloc => "contains_alloc",
            Self::BodyValueEscapesLoop => "body_value_escapes_loop",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopUnrollOutcome {
    Applied,
    WouldApply,
    Rejected(LoopUnrollRejectReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopUnrollEvent {
    pub function: String,
    pub header: String,
    pub outcome: LoopUnrollOutcome,
    pub trip_count: Option<usize>,
    pub header_size: usize,
    pub body_size: usize,
    pub projected_size: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoopUnrollStats {
    pub pass_invocations: u64,
    pub loop_observations: u64,
    pub unique_loops_seen: u64,
    pub shape_candidates: u64,
    pub exact_trip_candidates: u64,
    pub applied: u64,
    pub would_apply: u64,
    pub reject_reasons: BTreeMap<LoopUnrollRejectReason, u64>,
    pub trip_count_histogram: BTreeMap<usize, u64>,
    pub accepted_trip_count_histogram: BTreeMap<usize, u64>,
    pub body_size_histogram: BTreeMap<usize, u64>,
    pub projected_size_histogram: BTreeMap<usize, u64>,
    pub events: Vec<LoopUnrollEvent>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PassesRunStats {
    pub fixed_point_iterations: usize,
    pub loop_unroll: LoopUnrollStats,
}
