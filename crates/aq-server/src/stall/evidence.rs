//! Evidence the ladder decides from. Only agent-originated work is progress
//! (S5): heartbeats, repaints, transcript mtime, AQ-injected text (user-role
//! text) and token growth (usage) never are.
use aq_accounting::wire::{ObservationKind, TextRole};
use serde::{Deserialize, Serialize};

/// Distinct non-stall explanations surfaced at rung 1. Each holds the ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Explanation {
    ProviderBackoff,
    /// An H4 registered wait. Not sourced yet; the port reports none.
    RegisteredWait,
    KnownLongCheck,
    HumanControl,
    /// Observations are missing or gapped: raise a monitoring incident, never
    /// recover destructively.
    ObservationsUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputState {
    Ready,
    /// A terminal input is queued, injecting or of unknown delivery.
    Unknown,
    HumanControlled,
}

/// Rechecked before every action admission.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Admission {
    pub cancelled: bool,
    pub held: bool,
    pub hibernated: bool,
    /// The attempt no longer owns its task, or is terminal.
    pub ownership_lost: bool,
}
impl Admission {
    pub fn blocker(&self) -> Option<&'static str> {
        [
            (self.cancelled, "cancelled"),
            (self.held, "held"),
            (self.hibernated, "hibernated"),
            (self.ownership_lost, "ownership_lost"),
        ]
        .into_iter()
        .find_map(|(set, why)| set.then_some(why))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Latest agent-originated progress observation.
    pub last_progress_ms: Option<u64>,
    /// Freshness: the latest observation of any kind.
    pub observed_through_ms: Option<u64>,
    pub explanation: Option<Explanation>,
    pub input: InputState,
    pub admission: Admission,
    /// The B3 stop is confirmed (stopped or never started).
    pub stop_confirmed: bool,
}
impl Evidence {
    pub fn quiet(started_ms: u64) -> Self {
        Self {
            last_progress_ms: Some(started_ms),
            observed_through_ms: Some(started_ms),
            explanation: None,
            input: InputState::Ready,
            admission: Admission::default(),
            stop_confirmed: false,
        }
    }
}

pub fn is_progress(kind: &ObservationKind) -> bool {
    match kind {
        ObservationKind::TurnBoundary { .. }
        | ObservationKind::ToolStarted { .. }
        | ObservationKind::ToolResult { .. }
        | ObservationKind::Question { .. } => true,
        ObservationKind::TextReference { role, .. } => *role == TextRole::Assistant,
        ObservationKind::Usage { .. }
        | ObservationKind::ProviderError { .. }
        | ObservationKind::Gap { .. } => false,
    }
}

/// SQL predicate over `observation_events.record` matching [`is_progress`].
pub const PROGRESS_SQL: &str = "(record->'body'->'kind'->>'kind' IN ('turn_boundary','tool_started','tool_result','question') OR (record->'body'->'kind'->>'kind'='text_reference' AND record->'body'->'kind'->>'role'='assistant'))";
